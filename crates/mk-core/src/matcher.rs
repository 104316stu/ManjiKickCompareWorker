//! Matching: _align, _coverage, _score, the speed sweep, the tempo/pitch
//! shift search and compare_pcm.

use std::borrow::Cow;
use std::time::{Duration, Instant};

use crate::fingerprint::{hashes, peaks, spectrogram, ClipPrint, VideoPrint, BINS};
use crate::params::*;
use crate::pyops::{interp_stretch, np_round, pairwise_sum, percentile, py_round, py_round_int};
use crate::CompareResult;

const HALF_FFT: f64 = (N_FFT / 2) as f64; // 512

/// Knobs that are not part of the algorithm's answer, only of how long it
/// may search. `shift_budget_seconds: None` = no wall-clock limit.
///
/// `threads` > 1 runs the speed and tempo trials of one compare in
/// parallel; trials are still folded in grid order, so the answer is the
/// same for any thread count (only the wall-clock budget can end the
/// shift search at a different tempo).
#[derive(Debug, Clone)]
pub struct Options {
    pub shift_budget_seconds: Option<f64>,
    pub max_tempos: Option<usize>,
    pub threads: usize,
}

impl Default for Options {
    /// The site's behaviour: the shift search stops trying new tempos after
    /// 40 s. Fixed here (the environment is not read); set
    /// `shift_budget_seconds` to change it.
    fn default() -> Self {
        Options { shift_budget_seconds: Some(SHIFT_BUDGET_SECONDS_DEFAULT), max_tempos: None, threads: 1 }
    }
}

impl Options {
    /// Every tempo, no clock: the deterministic search parity runs use.
    pub fn exhaustive() -> Self {
        Options { shift_budget_seconds: None, max_tempos: None, threads: 1 }
    }
}

struct Align {
    aligned: i64,
    prominence: f64,
    offset: i64,
    matched_times: Vec<i32>,
    near_times: Vec<i32>,
    near_offsets: Vec<i32>,
}

/// _align. `clip` pairs are visited in the given order.
fn align(clip_h: &[u32], clip_t: &[i32], video: &VideoPrint) -> Option<Align> {
    if clip_h.is_empty() || video.n_hashes == 0 {
        return None;
    }
    let mut offsets: Vec<i32> = Vec::new();
    let mut q_times: Vec<i32> = Vec::new();
    for (i, &h) in clip_h.iter().enumerate() {
        let hits = video.lookup(h);
        if hits.is_empty() || hits.len() > MAX_HASH_REPEATS {
            continue;
        }
        let qt = clip_t[i];
        for &vt in hits {
            offsets.push(vt - qt);
            q_times.push(qt);
        }
    }
    if offsets.is_empty() {
        return Some(Align {
            aligned: 0,
            prominence: 0.0,
            offset: 0,
            matched_times: Vec::new(),
            near_times: Vec::new(),
            near_offsets: Vec::new(),
        });
    }
    let lo = *offsets.iter().min().unwrap() as i64;
    let hi = *offsets.iter().max().unwrap() as i64;
    let width = (hi - lo + 1) as usize;
    let mut hist = vec![0i64; width];
    for &o in &offsets {
        hist[(o as i64 - lo) as usize] += 1;
    }
    let mut smooth = hist.clone();
    for i in 1..width {
        smooth[i] += hist[i - 1];
    }
    for i in 0..width - 1 {
        smooth[i] += hist[i + 1];
    }
    let (best, aligned) = argmax_first(&smooth);
    let guard = FPS as usize; // int(FPS) = 31
    let ex_lo = best.saturating_sub(guard);
    let ex_hi = (best + guard + 1).min(width);
    let mut second = 0i64;
    for (i, &v) in smooth.iter().enumerate() {
        if (i < ex_lo || i >= ex_hi) && v > second {
            second = v;
        }
    }
    let prominence = aligned as f64 / second.max(3) as f64;
    let off = best as i64 + lo;
    let mut matched_times = Vec::new();
    let mut near_times = Vec::new();
    let mut near_offsets = Vec::new();
    for (k, &o) in offsets.iter().enumerate() {
        let d = (o as i64 - off).abs();
        if d <= 1 {
            matched_times.push(q_times[k]);
        }
        if d <= DRIFT_WINDOW {
            near_times.push(q_times[k]);
            near_offsets.push(o);
        }
    }
    Some(Align { aligned, prominence, offset: off, matched_times, near_times, near_offsets })
}

fn argmax_first(v: &[i64]) -> (usize, i64) {
    let mut bi = 0;
    let mut bv = v[0];
    for (i, &x) in v.iter().enumerate().skip(1) {
        if x > bv {
            bv = x;
            bi = i;
        }
    }
    (bi, bv)
}

const SEG: i64 = (SEGMENT_SECONDS * FPS) as i64; // int(156.25) = 156

/// _coverage: (coverage, matched, inside_seconds, span).
fn coverage(clip_frames: i64, video_frames: i64, a: &Align) -> (f64, i64, f64, Option<(i64, i64)>) {
    let n_seg = ((clip_frames + SEG - 1) / SEG).max(1) as usize;
    let mut votes = vec![0i64; n_seg];
    for &t in &a.matched_times {
        let i = (t as i64).div_euclid(SEG);
        if i >= 0 && (i as usize) < n_seg {
            votes[i as usize] += 1;
        }
    }
    let mut inside_n = 0i64;
    let mut matched = 0i64;
    let mut first: Option<usize> = None;
    let mut last = 0usize;
    for i in 0..n_seg {
        let s = i as i64 * SEG + a.offset;
        let inside = s + SEG > 0 && s < video_frames;
        if inside {
            inside_n += 1;
            if votes[i] as f64 >= SEGMENT_MIN_DENSITY * SEGMENT_SECONDS {
                matched += 1;
                if first.is_none() {
                    first = Some(i);
                }
                last = i;
            }
        }
    }
    if inside_n == 0 {
        return (0.0, 0, 0.0, None);
    }
    let lo = (-a.offset).max(0);
    let hi = clip_frames.min(video_frames - a.offset);
    let inside_seconds = ((hi - lo) as f64 / FPS).max(0.0);
    let span = first.map(|f| (f as i64 * SEG, clip_frames.min((last as i64 + 1) * SEG)));
    (matched as f64 / inside_n as f64, matched, inside_seconds, span)
}

/// One trial's measurement (_score's dict).
#[derive(Debug, Clone)]
pub(crate) struct Score {
    pub speed: f64,
    pub aligned: i64,
    pub density: f64,
    pub prominence: f64,
    pub offset_seconds: f64,
    pub coverage: f64,
    pub matched_segments: i64,
    pub clip_hashes: i64,
    pub semitones: Option<f64>,
    near_times: Vec<i32>,
    near_offsets: Vec<i32>,
    pub span: Option<(i64, i64)>,
}

fn score(clip_h: &[u32], clip_t: &[i32], clip_frames: i64, video: &VideoPrint, speed: f64) -> Option<Score> {
    let a = align(clip_h, clip_t, video)?;
    let (cov, matched, inside_seconds, span) = coverage(clip_frames, video.frames, &a);
    Some(Score {
        speed,
        aligned: a.aligned,
        density: py_round(a.aligned as f64 / inside_seconds.max(1.0), 2),
        prominence: py_round(a.prominence, 2),
        offset_seconds: py_round(a.offset as f64 / FPS, 2),
        coverage: py_round(cov, 3),
        matched_segments: matched,
        clip_hashes: clip_h.len() as i64,
        semitones: None,
        near_times: a.near_times,
        near_offsets: a.near_offsets,
        span,
    })
}

/// _stretch.
fn stretch(x: &[f32], speed: f64) -> Cow<'_, [f32]> {
    if (speed - 1.0).abs() < 1e-6 {
        return Cow::Borrowed(x);
    }
    let n_out = (x.len() as f64 * speed) as usize;
    Cow::Owned(interp_stretch(x, n_out, speed))
}

fn try_speed(x_clip: &[f32], video: &VideoPrint, speed: f64) -> Option<Score> {
    let fp = ClipPrint::from_pcm(&stretch(x_clip, speed));
    score(&fp.hashes, &fp.times, fp.frames, video, speed)
}

fn is_match(r: Option<&Score>) -> bool {
    matches!(r, Some(r) if r.aligned >= MIN_ALIGNED && r.density >= MIN_DENSITY)
}

/// _better: prefer more coverage, then more votes; ties go to `a`.
fn better(a: Option<Score>, b: Option<Score>) -> Option<Score> {
    match (a, b) {
        (a, None) => a,
        (None, b) => b,
        (Some(a), Some(b)) => {
            let ge = a.coverage > b.coverage || (a.coverage == b.coverage && a.aligned >= b.aligned);
            Some(if ge { a } else { b })
        }
    }
}

/// np.polyfit(times, offsets, 1)[0]: the least-squares slope.
fn slope(times: &[i32], offsets: &[i32]) -> f64 {
    let n = times.len() as f64;
    let mx = times.iter().map(|&v| v as f64).sum::<f64>() / n;
    let my = offsets.iter().map(|&v| v as f64).sum::<f64>() / n;
    let mut sxy = 0.0;
    let mut sxx = 0.0;
    for i in 0..times.len() {
        let dx = times[i] as f64 - mx;
        sxy += dx * (offsets[i] as f64 - my);
        sxx += dx * dx;
    }
    if sxx == 0.0 {
        0.0
    } else {
        sxy / sxx
    }
}

/// _drift_corrected. `retry(speed)` makes the new trial.
fn drift_corrected(r: Score, retry: &dyn Fn(f64) -> Option<Score>) -> Score {
    if r.near_times.len() < 50 {
        return r;
    }
    let b = slope(&r.near_times, &r.near_offsets);
    if !(b.abs() >= 2e-4 && b.abs() <= 0.02) {
        return r;
    }
    let speed = py_round(r.speed * (1.0 + b), 5);
    let fixed = retry(speed);
    if is_match(fixed.as_ref()) {
        better(fixed, Some(r)).unwrap()
    } else {
        r
    }
}

/// _speed_grid: nearest-to-1.0 first.
pub fn speed_grid() -> Vec<f64> {
    let mut grid = Vec::new();
    let mut s = SPEED_MIN;
    while s <= SPEED_MAX + 1e-9 {
        if (s - 1.0).abs() > SAME_SPEED_TOLERANCE {
            grid.push(py_round(s, 4));
        }
        s *= SPEED_STEP;
    }
    grid.sort_by(|a, b| a.ln().abs().total_cmp(&b.ln().abs()));
    grid
}

/// _tempo_grid: SPEED_STEP powers from SPEED_MIN to SHIFT_TEMPO_MAX.
pub fn tempo_grid() -> Vec<f64> {
    let k_min = (SPEED_MIN.ln() / SPEED_STEP.ln() - 1e-9).ceil() as i64;
    let k_max = (SHIFT_TEMPO_MAX.ln() / SPEED_STEP.ln() + 1e-9).floor() as i64;
    let mut ks: Vec<i64> = (k_min..=k_max).collect();
    ks.sort_by_key(|k| k.abs());
    ks.iter().map(|&k| py_round(SPEED_STEP.powf(k as f64), 4)).collect()
}

/// _loud_run: (start, end) samples of the longest loud stretch.
pub fn loud_run(x: &[f32]) -> Option<(usize, usize)> {
    let frame = (EXCERPT_FRAME_SECONDS * FS_USIZE as f64) as usize;
    let n = x.len() / frame;
    if n < 2 {
        return None;
    }
    let mut sq = vec![0f64; frame];
    let db: Vec<f64> = (0..n)
        .map(|i| {
            for (j, v) in x[i * frame..(i + 1) * frame].iter().enumerate() {
                let d = *v as f64;
                sq[j] = d * d;
            }
            let rms = (pairwise_sum(&sq) / frame as f64).sqrt();
            20.0 * (rms + 1e-9).log10()
        })
        .collect();
    let thr = percentile(&db, EXCERPT_LOUD_PERCENTILE) - EXCERPT_LOUD_DROP_DB;
    let loud: Vec<i64> = (0..n).filter(|&i| db[i] >= thr).map(|i| i as i64).collect();
    if loud.is_empty() {
        return None;
    }
    let gap = py_round_int(EXCERPT_GAP_SECONDS / EXCERPT_FRAME_SECONDS);
    let mut starts = vec![loud[0]];
    let mut ends = Vec::new();
    for w in 0..loud.len() - 1 {
        if loud[w + 1] - loud[w] - 1 > gap {
            ends.push(loud[w]);
            starts.push(loud[w + 1]);
        }
    }
    ends.push(*loud.last().unwrap());
    let mut bi = 0;
    for i in 1..starts.len() {
        if ends[i] - starts[i] > ends[bi] - starts[bi] {
            bi = i;
        }
    }
    Some((starts[bi] as usize * frame, (ends[bi] as usize + 1) * frame))
}

/// _excerpt.
pub fn excerpt(x: &[f32]) -> &[f32] {
    let n = (SWEEP_SECONDS * FS_USIZE as f64) as usize;
    if x.len() <= n {
        return x;
    }
    let start = match loud_run(x) {
        None => (x.len() - n) / 2,
        Some((a, b)) => {
            let c = (a as i64 + b as i64 - n as i64).div_euclid(2);
            c.min((x.len() - n) as i64).max(0) as usize
        }
    };
    &x[start..start + n]
}

fn refine_steps() -> i64 {
    py_round_int(SPEED_REFINE_SPAN / SPEED_REFINE_STEP)
}

/// _sweep_speeds.
fn sweep_speeds(ex: &[f32], video: &VideoPrint, threads: usize) -> Option<f64> {
    let mut sweep: Option<Score> = None;
    let grid = speed_grid();
    'grid: for batch in grid.chunks(threads.max(1)) {
        for r in par_map(batch, threads, |&speed| try_speed(ex, video, speed)) {
            if is_match(r.as_ref()) && sweep.as_ref().is_none_or(|s| r.as_ref().unwrap().aligned > s.aligned) {
                let r = r.unwrap();
                let enough = r.density >= SWEEP_ENOUGH_DENSITY;
                sweep = Some(r);
                if enough {
                    break 'grid;
                }
            }
        }
    }
    let mut sweep = sweep?;
    let centre = sweep.speed;
    let steps = refine_steps();
    let speeds: Vec<f64> = (-steps..=steps)
        .filter(|&i| i != 0)
        .map(|i| py_round(centre + i as f64 * SPEED_REFINE_STEP, 4))
        .collect();
    for r in par_map(&speeds, threads, |&speed| try_speed(ex, video, speed)) {
        if is_match(r.as_ref()) && r.as_ref().unwrap().aligned > sweep.aligned {
            sweep = r.unwrap();
        }
    }
    Some(sweep.speed)
}

/// `items.iter().map(f).collect()`, on up to `threads` threads, in order.
fn par_map<T: Sync, R: Send>(items: &[T], threads: usize, f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    if threads <= 1 || items.len() <= 1 {
        return items.iter().map(f).collect();
    }
    let per = items.len().div_ceil(threads.min(items.len()));
    std::thread::scope(|scope| {
        let f = &f;
        let handles: Vec<_> = items
            .chunks(per)
            .map(|chunk| scope.spawn(move || chunk.iter().map(f).collect::<Vec<R>>()))
            .collect();
        handles.into_iter().flat_map(|h| h.join().expect("compare thread")).collect()
    })
}

// ---- tempo and pitch apart ----------------------------------------------------

struct Peaks {
    t: Vec<f64>,
    f: Vec<f64>,
    frames: i64,
}

fn clip_peaks(x: &[f32]) -> Peaks {
    let s = spectrogram(x);
    let (t, f) = peaks(&s);
    Peaks {
        t: t.iter().map(|&v| v as f64).collect(),
        f: f.iter().map(|&v| v as f64).collect(),
        frames: s.frames as i64,
    }
}

/// 2.0 ** (-semitones / 12)
#[inline]
fn pitch_factor(semitones: f64) -> f64 {
    2.0f64.powf(-semitones / 12.0)
}

/// _Scaled + _try_shift.
fn try_shift(p: &Peaks, video: &VideoPrint, tempo: f64, semitones: f64) -> Option<Score> {
    let factor = pitch_factor(semitones);
    let mut ts = Vec::with_capacity(p.t.len());
    let mut fs = Vec::with_capacity(p.t.len());
    for i in 0..p.t.len() {
        let t = np_round(p.t[i] * tempo);
        let f = np_round(p.f[i] * factor);
        if f > 0.0 && f < HALF_FFT {
            ts.push(t as i64);
            fs.push(f as i64);
        }
    }
    let (h, at) = hashes(&ts, &fs);
    let frames = py_round_int(p.frames as f64 * tempo);
    let mut r = score(&h, &at, frames, video, tempo)?;
    r.semitones = Some(semitones);
    Some(r)
}

/// _pairs: (anchor, partner) indexes, by k then anchor.
fn pairs(n: usize) -> (Vec<u32>, Vec<u32>) {
    let mut a = Vec::new();
    let mut b = Vec::new();
    if n >= 2 {
        for k in 1..=FANOUT.min(n - 1) {
            for i in 0..n - k {
                a.push(i as u32);
                b.push((i + k) as u32);
            }
        }
    }
    (a, b)
}

/// _pitch_votes: (aligned, density) per shift at one tempo.
fn pitch_votes(
    p: &Peaks,
    prs: &(Vec<u32>, Vec<u32>),
    video: &VideoPrint,
    tempo: f64,
    semitones: &[f64],
) -> (Vec<i64>, Vec<f64>) {
    let shifts = semitones.len();
    let nothing = (vec![0i64; shifts], vec![0f64; shifts]);
    let ts: Vec<i64> = p.t.iter().map(|&t| np_round(t * tempo) as i64).collect();
    let mut kept: Vec<(u32, u32, u32)> = Vec::new();
    for k in 0..prs.0.len() {
        let (a, b) = (prs.0[k], prs.1[k]);
        let dt = ts[b as usize] - ts[a as usize];
        if dt <= MAX_DT {
            kept.push((a, b, dt as u32));
        }
    }
    if kept.is_empty() || video.n_hashes == 0 {
        return nothing;
    }
    let n = p.f.len();
    let mut fq = vec![0u32; n];
    let mut valid = vec![false; n];
    // Votes of every row, row-major, with row boundaries.
    let mut offs: Vec<i32> = Vec::new();
    let mut row_end: Vec<usize> = Vec::with_capacity(shifts);
    for &semi in semitones {
        let factor = pitch_factor(semi);
        for i in 0..n {
            let fs = np_round(p.f[i] * factor);
            valid[i] = fs > 0.0 && fs < HALF_FFT;
            let c = fs.clamp(0.0, HALF_FFT) as i64;
            fq[i] = (c >> FREQ_SHIFT) as u32;
        }
        for &(a, b, dt) in &kept {
            let (a, b) = (a as usize, b as usize);
            if !(valid[a] && valid[b]) {
                continue;
            }
            let h = (fq[a] << 14) | (fq[b] << 6) | dt;
            let hits = video.lookup(h);
            if hits.is_empty() || hits.len() > MAX_HASH_REPEATS {
                continue;
            }
            let anchor = ts[a] as i32;
            for &vt in hits {
                offs.push(vt - anchor);
            }
        }
        row_end.push(offs.len());
    }
    if offs.is_empty() {
        return nothing;
    }
    let glo = *offs.iter().min().unwrap() as i64;
    let ghi = *offs.iter().max().unwrap() as i64;
    let width = (ghi - glo + 1) as usize;
    let mut hist = vec![0i64; width];
    let mut aligned = vec![0i64; shifts];
    let mut density = vec![0f64; shifts];
    let clip_frames = py_round_int(p.frames as f64 * tempo);
    let mut start = 0usize;
    for r in 0..shifts {
        let row = &offs[start..row_end[r]];
        start = row_end[r];
        if row.is_empty() {
            continue; // aligned 0, density 0: never a hit
        }
        let rlo = *row.iter().min().unwrap() as i64 - glo;
        let rhi = *row.iter().max().unwrap() as i64 - glo;
        // The smoothed row is nonzero only within [rlo-1, rhi+1] (clipped
        // to the shared histogram): its first maximum lies there.
        let a = (rlo - 1).max(0) as usize;
        let b = (rhi + 1).min(width as i64 - 1) as usize;
        for v in &mut hist[a..=b] {
            *v = 0;
        }
        for &o in row {
            hist[(o as i64 - glo) as usize] += 1;
        }
        let mut best = a;
        let mut best_v = i64::MIN;
        for i in a..=b {
            let mut s = hist[i];
            if i > 0 {
                s += hist[i - 1];
            }
            if i + 1 < width {
                s += hist[i + 1];
            }
            if s > best_v {
                best_v = s;
                best = i;
            }
        }
        // hist outside [a, b] may hold another row's counts only at
        // positions we never read beyond a-1 / b+1, which are zeroed below.
        for v in &mut hist[a..=b] {
            *v = 0;
        }
        let offset = best as i64 + glo;
        let inside = (clip_frames.min(video.frames - offset) - (-offset).max(0)).max(0) as f64 / FPS;
        aligned[r] = best_v;
        density[r] = best_v as f64 / inside.max(1.0);
    }
    (aligned, density)
}

#[derive(Clone, Copy)]
struct ShiftBest {
    aligned: i64,
    density: f64,
    tempo: f64,
    semitones: f64,
}

/// _best_shift.
fn best_shift(
    votes: (Vec<i64>, Vec<f64>),
    tempo: f64,
    semitones: &[f64],
    best: Option<ShiftBest>,
) -> Option<ShiftBest> {
    let (aligned, density) = votes;
    let mut i_best: Option<usize> = None;
    let mut v_best = -1i64;
    for i in 0..aligned.len() {
        let hit = aligned[i] >= MIN_ALIGNED && density[i] >= MIN_DENSITY;
        let v = if hit { aligned[i] } else { -1 };
        if i_best.is_none() || v > v_best {
            v_best = v;
            i_best = Some(i);
        }
    }
    if v_best < 0 {
        return best; // no hit at all
    }
    let i = i_best.unwrap();
    if best.is_none_or(|b| aligned[i] > b.aligned) {
        return Some(ShiftBest { aligned: aligned[i], density: density[i], tempo, semitones: semitones[i] });
    }
    best
}

/// _shift_search.
/// How far the coarse tempo sweep of a shift search got.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ShiftProgress {
    /// Coarse tempos voted on.
    pub tried: usize,
    /// True when the time budget or `max_tempos` ended the sweep before
    /// the grid did (and no strong hit had ended it first).
    pub cut: bool,
}

fn deadline_after(budget: Option<f64>) -> Option<Instant> {
    let b = budget?;
    if b.is_nan() {
        return Some(Instant::now());
    }
    let d = Duration::try_from_secs_f64(b.max(0.0)).ok()?; // too large: no limit
    Instant::now().checked_add(d)
}

fn shift_search(x_clip: &[f32], video: &VideoPrint, opts: &Options, progress: &mut ShiftProgress) -> Option<Score> {
    let deadline = deadline_after(opts.shift_budget_seconds);
    let p = clip_peaks(excerpt(x_clip));
    let prs = pairs(p.t.len());
    let steps = py_round_int(SHIFT_SEMITONES / SHIFT_STEP);
    let coarse: Vec<f64> = (-steps..=steps).map(|i| i as f64 * SHIFT_STEP).collect();
    let mut best: Option<ShiftBest> = None;
    let full_grid = tempo_grid();
    let mut grid = full_grid.clone();
    if let Some(m) = opts.max_tempos {
        grid.truncate(m);
    }
    let threads = opts.threads.max(1);
    let mut strong = false;
    'grid: for batch in grid.chunks(threads) {
        if deadline.is_some_and(|d| Instant::now() > d) {
            break;
        }
        let votes = par_map(batch, threads, |&tempo| pitch_votes(&p, &prs, video, tempo, &coarse));
        for (&tempo, v) in batch.iter().zip(votes) {
            progress.tried += 1;
            best = best_shift(v, tempo, &coarse, best);
            if best.is_some_and(|b| b.density >= SWEEP_ENOUGH_DENSITY) {
                strong = true;
                break 'grid;
            }
        }
    }
    progress.cut = !strong && progress.tried < full_grid.len();
    let b0 = best?;
    let fine: Vec<f64> = (-4..=4).map(|i| b0.semitones + i as f64 * SHIFT_REFINE_STEP).collect();
    let centre = b0.tempo;
    let steps = refine_steps();
    let tempos: Vec<f64> = (-steps..=steps).map(|i| py_round(centre + i as f64 * SPEED_REFINE_STEP, 4)).collect();
    let votes = par_map(&tempos, threads, |&tempo| pitch_votes(&p, &prs, video, tempo, &fine));
    for (&tempo, v) in tempos.iter().zip(votes) {
        best = best_shift(v, tempo, &fine, best);
    }
    let best = best.unwrap();
    let full = clip_peaks(x_clip);
    let semitones = py_round(best.semitones, 4);
    let r = try_shift(&full, video, best.tempo, semitones);
    if !is_match(r.as_ref()) {
        return None;
    }
    Some(drift_corrected(r.unwrap(), &|speed| try_shift(&full, video, speed, semitones)))
}

/// compare_pcm + _report_span.
pub fn compare_pcm(x_clip: &[f32], x_video: &[f32], total_seconds: Option<f64>, opts: &Options) -> CompareResult {
    let fs = FS_USIZE as f64;
    let video = VideoPrint::from_pcm(x_video);
    let native = try_speed(x_clip, &video, 1.0);
    let mut best = if is_match(native.as_ref()) { native.clone() } else { None };
    let retry_speed = |speed: f64| try_speed(x_clip, &video, speed);
    if best.as_ref().is_none_or(|b| b.coverage < SAME_COVERAGE) {
        if let Some(speed) = sweep_speeds(excerpt(x_clip), &video, opts.threads) {
            let full = try_speed(x_clip, &video, speed);
            if is_match(full.as_ref()) {
                best = better(Some(drift_corrected(full.unwrap(), &retry_speed)), best);
            }
        }
    }
    if let Some(b) = best.take() {
        best = Some(if b.coverage < SAME_COVERAGE { drift_corrected(b, &retry_speed) } else { b });
    }

    let decoded = x_clip.len() as f64 / fs;
    let mut result = CompareResult {
        verdict: String::new(),
        speed: 1.0,
        aligned: 0,
        density: 0.0,
        prominence: 0.0,
        coverage: 0.0,
        offset_seconds: None,
        matched_segments: None,
        clip_hashes: None,
        semitones: None,
        pitch_searched: false,
        shift_tempos: None,
        shift_cut: false,
        clip_seconds: py_round(decoded, 2),
        video_seconds: py_round(x_video.len() as f64 / fs, 2),
        clip_total_seconds: py_round(decoded.max(total_seconds.unwrap_or(0.0)), 2),
        match_start_seconds: None,
        match_end_seconds: None,
        extra_seconds: None,
    };
    if best.is_none() {
        result.pitch_searched = true;
        let mut progress = ShiftProgress::default();
        best = shift_search(x_clip, &video, opts, &mut progress);
        result.shift_tempos = Some(progress.tried);
        result.shift_cut = progress.cut;
    }
    let Some(best) = best else {
        result.verdict = "different".into();
        if let Some(n) = &native {
            result.aligned = n.aligned;
            result.density = n.density;
            result.prominence = n.prominence;
        }
        return result;
    };
    result.speed = best.speed;
    result.aligned = best.aligned;
    result.density = best.density;
    result.prominence = best.prominence;
    result.offset_seconds = Some(best.offset_seconds);
    result.coverage = best.coverage;
    result.matched_segments = Some(best.matched_segments);
    result.clip_hashes = Some(best.clip_hashes);
    if let Some((s0, s1)) = best.span {
        let speed = if best.speed != 0.0 { best.speed } else { 1.0 };
        let start = py_round(decoded.min(s0 as f64 / FPS / speed), 2);
        let end = py_round(decoded.min(s1 as f64 / FPS / speed), 2);
        result.match_start_seconds = Some(start);
        result.match_end_seconds = Some(end);
        let total = result.clip_total_seconds;
        result.extra_seconds = if end >= decoded - SEGMENT_SECONDS && total > decoded + SEGMENT_SECONDS {
            None
        } else {
            Some(py_round((total - end).max(0.0), 2))
        };
    }
    if let Some(s) = best.semitones {
        result.semitones = Some(py_round(s, 2));
    }
    let sped = (best.speed - 1.0).abs() > SAME_SPEED_TOLERANCE;
    let weak = best.density < SAME_MIN_DENSITY;
    result.verdict = if !sped && !weak && best.coverage >= SAME_COVERAGE && best.semitones.is_none() {
        "same"
    } else {
        "close"
    }
    .into();
    if !sped {
        result.speed = 1.0;
    }
    let _ = BINS;
    result
}
