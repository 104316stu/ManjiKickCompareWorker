//! Spectrogram, constellation peaks and hashes (audio_match.py's
//! _spectrogram, _local_max, _peaks, _hashes, Fingerprint).

use realfft::{RealFftPlanner, RealToComplex};
use std::cell::RefCell;
use std::sync::{Arc, OnceLock};

use crate::params::*;
use crate::pyops::hanning_f32;

pub const BINS: usize = N_FFT / 2 + 1;
/// Hashes are (f1 << 14) | (f2 << 6) | dt with 8-bit frequencies: 22 bits.
pub const HASH_SPACE: usize = 1 << 22;

fn window() -> &'static [f32] {
    static W: OnceLock<Vec<f32>> = OnceLock::new();
    W.get_or_init(|| hanning_f32(N_FFT))
}

thread_local! {
    static FFT: RefCell<Option<Arc<dyn RealToComplex<f32>>>> = const { RefCell::new(None) };
}

fn fft_plan() -> Arc<dyn RealToComplex<f32>> {
    FFT.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(RealFftPlanner::<f32>::new().plan_fft_forward(N_FFT));
        }
        slot.as_ref().unwrap().clone()
    })
}

/// Log-magnitude STFT, frames x BINS row-major, in dB below the loudest bin.
pub struct Spectrogram {
    pub data: Vec<f32>,
    pub frames: usize,
}

pub fn spectrogram(x: &[f32]) -> Spectrogram {
    let padded;
    let x = if x.len() < N_FFT {
        let mut p = x.to_vec();
        p.resize(N_FFT, 0.0);
        padded = p;
        &padded[..]
    } else {
        x
    };
    let n_frames = 1 + (x.len() - N_FFT) / HOP;
    let w = window();
    let fft = fft_plan();
    let mut input = fft.make_input_vec();
    let mut output = fft.make_output_vec();
    let mut scratch = fft.make_scratch_vec();
    let mut data = vec![0f32; n_frames * BINS];
    let mut max = f32::NEG_INFINITY;
    for fr in 0..n_frames {
        let s = fr * HOP;
        let src = &x[s..s + N_FFT];
        for i in 0..N_FFT {
            input[i] = src[i] * w[i];
        }
        fft.process_with_scratch(&mut input, &mut output, &mut scratch)
            .expect("fft sizes");
        let row = &mut data[fr * BINS..(fr + 1) * BINS];
        for (k, c) in output.iter().enumerate() {
            // |z| as numpy's complex64 abs (hypot), correctly rounded via f64.
            let mag = ((c.re as f64) * (c.re as f64) + (c.im as f64) * (c.im as f64)).sqrt() as f32;
            let v = mag + 1e-6f32;
            let db = 20.0f32 * ((v as f64).log10() as f32);
            row[k] = db;
            if db > max {
                max = db;
            }
        }
    }
    for v in data.iter_mut() {
        *v -= max;
    }
    Spectrogram { data, frames: n_frames }
}

/// Frequency-direction running max over +-df (edges clipped), as the
/// first loop of _local_max computes D.
fn row_max_freq(src: &[f32], dst: &mut [f32], df: usize) {
    dst.copy_from_slice(src);
    let n = src.len();
    for k in 1..=df.min(n.saturating_sub(1)) {
        max_into(&mut dst[k..], &src[..n - k]);
        max_into(&mut dst[..n - k], &src[k..]);
    }
}

/// dst = max(dst, src) elementwise (vectorises to maxps).
#[inline]
fn max_into(dst: &mut [f32], src: &[f32]) {
    for (d, &s) in dst.iter_mut().zip(src) {
        *d = if s > *d { s } else { *d };
    }
}

/// (times, freqs) of the constellation, sorted by time then frequency.
pub fn peaks(s: &Spectrogram) -> (Vec<i64>, Vec<i64>) {
    let frames = s.frames;
    let mut t_out = Vec::new();
    let mut f_out = Vec::new();
    if frames == 0 {
        return (t_out, f_out);
    }
    let ring = 2 * PEAK_DT + 1;
    let mut d_rows = vec![0f32; ring * BINS];
    let mut computed = 0usize; // rows of D computed so far
    let mut e = vec![0f32; BINS];
    let fps_int = FPS as usize; // int(FPS) = 31
    // Candidates of the current one-second bucket, in row-major order.
    let mut bucket: Vec<(i64, i64, f32)> = Vec::new();
    let mut cur_bucket = 0usize;
    let flush = |bucket: &mut Vec<(i64, i64, f32)>, t_out: &mut Vec<i64>, f_out: &mut Vec<i64>| {
        if bucket.is_empty() {
            return;
        }
        // lexsort((-amp, bucket)): amplitude descending, stable.
        bucket.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        bucket.truncate(PEAKS_PER_SECOND);
        bucket.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
        for &(t, f, _) in bucket.iter() {
            t_out.push(t);
            f_out.push(f);
        }
        bucket.clear();
    };
    let peak_min = PEAK_MIN_DB as f32;
    for t in 0..frames {
        let need = (t + PEAK_DT).min(frames - 1);
        while computed <= need {
            let r = computed % ring;
            row_max_freq(
                &s.data[computed * BINS..(computed + 1) * BINS],
                &mut d_rows[r * BINS..(r + 1) * BINS],
                PEAK_DF,
            );
            computed += 1;
        }
        let lo = t.saturating_sub(PEAK_DT);
        let hi = need;
        e.copy_from_slice(&d_rows[(lo % ring) * BINS..(lo % ring + 1) * BINS]);
        for r in lo + 1..=hi {
            max_into(&mut e, &d_rows[(r % ring) * BINS..(r % ring + 1) * BINS]);
        }
        let srow = &s.data[t * BINS..(t + 1) * BINS];
        let b = t / fps_int;
        if b != cur_bucket {
            flush(&mut bucket, &mut t_out, &mut f_out);
            cur_bucket = b;
        }
        for f in 1..BINS - 1 {
            let v = srow[f];
            if v >= e[f] && v > peak_min {
                bucket.push((t as i64, f as i64, v));
            }
        }
    }
    flush(&mut bucket, &mut t_out, &mut f_out);
    (t_out, f_out)
}

/// _hashes: (f1, f2, dt) packed for each anchor and its next FANOUT peaks,
/// in numpy's concatenation order (by k, then anchor). `t` sorted.
pub fn hashes(t: &[i64], f: &[i64]) -> (Vec<u32>, Vec<i32>) {
    let n = t.len();
    let mut hs = Vec::with_capacity(n * FANOUT);
    let mut ts = Vec::with_capacity(n * FANOUT);
    for k in 1..=FANOUT {
        if k >= n {
            break;
        }
        let before = hs.len();
        for i in 0..n - k {
            let dt = t[i + k] - t[i];
            if dt <= MAX_DT {
                let f1 = (f[i] >> FREQ_SHIFT) as u32;
                let f2 = (f[i + k] >> FREQ_SHIFT) as u32;
                hs.push((f1 << 14) | (f2 << 6) | (dt as u32));
                ts.push(t[i] as i32);
            }
        }
        if hs.len() == before {
            break;
        }
    }
    (hs, ts)
}

/// A clip-side fingerprint: hashes and anchor times in lookup order.
pub struct ClipPrint {
    pub hashes: Vec<u32>,
    pub times: Vec<i32>,
    pub frames: i64,
}

impl ClipPrint {
    /// Fingerprint(x): hashes stably sorted by value.
    pub fn from_pcm(x: &[f32]) -> ClipPrint {
        let s = spectrogram(x);
        let (t, f) = peaks(&s);
        let (h, at) = hashes(&t, &f);
        let mut idx: Vec<u32> = (0..h.len() as u32).collect();
        idx.sort_by_key(|&i| h[i as usize]);
        ClipPrint {
            hashes: idx.iter().map(|&i| h[i as usize]).collect(),
            times: idx.iter().map(|&i| at[i as usize]).collect(),
            frames: s.frames as i64,
        }
    }
}

/// The video's fingerprint, indexed by hash value: the anchor times of
/// hash h are times[start[h]..start[h+1]], in stable-sort order.
pub struct VideoPrint {
    pub start: Vec<u32>,
    pub times: Vec<i32>,
    pub frames: i64,
    pub n_hashes: usize,
}

impl VideoPrint {
    pub fn from_pcm(x: &[f32]) -> VideoPrint {
        let s = spectrogram(x);
        let (t, f) = peaks(&s);
        let frames = s.frames as i64;
        drop(s);
        let (h, at) = hashes(&t, &f);
        let mut start = vec![0u32; HASH_SPACE + 1];
        for &v in &h {
            start[v as usize + 1] += 1;
        }
        for i in 0..HASH_SPACE {
            start[i + 1] += start[i];
        }
        let mut fill = start.clone();
        let mut times = vec![0i32; h.len()];
        for (i, &v) in h.iter().enumerate() {
            let slot = &mut fill[v as usize];
            times[*slot as usize] = at[i];
            *slot += 1;
        }
        VideoPrint { start, times, frames, n_hashes: h.len() }
    }

    #[inline]
    pub fn lookup(&self, h: u32) -> &[i32] {
        let h = h as usize;
        &self.times[self.start[h] as usize..self.start[h + 1] as usize]
    }
}
