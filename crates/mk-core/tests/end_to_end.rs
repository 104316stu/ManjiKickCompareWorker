//! compare() on generated audio: a cut of a signal is found in it, at its
//! offset, and unrelated noise is not.

use mk_core::{compare_with, Clip, CompareResult, Options, FS};

/// A deterministic "song": a sequence of note chords with a decaying
/// envelope, so the spectrogram has clear, moving peaks.
fn song(seconds: f64, seed: u32) -> Vec<f32> {
    let mut x = seed;
    let mut rnd = move || {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        x
    };
    let n = (seconds * FS as f64) as usize;
    let note = FS as usize / 4;
    let mut out = vec![0f32; n];
    for start in (0..n).step_by(note) {
        let freqs: Vec<f64> = (0..3).map(|_| 200.0 + (rnd() % 3000) as f64).collect();
        for i in start..(start + note).min(n) {
            let t = (i - start) as f64 / FS as f64;
            let env = (-t * 6.0).exp();
            let v: f64 = freqs.iter().map(|f| (2.0 * std::f64::consts::PI * f * t).sin()).sum();
            out[i] = (0.2 * env * v) as f32;
        }
    }
    out
}

fn noise(seconds: f64, seed: u32) -> Vec<f32> {
    let mut x = seed;
    (0..(seconds * FS as f64) as usize)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x as f32 / u32::MAX as f32 - 0.5) * 0.3
        })
        .collect()
}

fn run(clip: Vec<f32>, video: &[f32]) -> CompareResult {
    let clip = Clip::from_pcm(clip).unwrap();
    compare_with(&clip, video, &Options { max_tempos: Some(3), ..Options::exhaustive() })
}

#[test]
fn a_cut_of_the_video_is_the_same_recording() {
    let video = song(90.0, 7);
    let start = 20 * FS as usize;
    let r = run(video[start..start + 40 * FS as usize].to_vec(), &video);
    assert_eq!(r.verdict, "same", "{r:?}");
    assert!((r.offset_seconds.unwrap() - 20.0).abs() < 0.05, "{r:?}");
    assert_eq!(r.speed, 1.0);
    let json = serde_json::to_value(&r).unwrap();
    for key in ["verdict", "speed", "aligned", "density", "prominence", "coverage", "offset_seconds",
                "matched_segments", "clip_hashes", "clip_seconds", "video_seconds", "clip_total_seconds",
                "match_start_seconds", "match_end_seconds", "extra_seconds"] {
        assert!(json.get(key).is_some(), "missing {key}");
    }
    assert!(json.get("pitch_searched").is_none() && json.get("semitones").is_none());
}

#[test]
fn unrelated_audio_is_different() {
    let video = song(60.0, 7);
    let r = run(noise(30.0, 99), &video);
    assert_eq!(r.verdict, "different", "{r:?}");
    assert!(r.pitch_searched);
    assert_eq!(r.offset_seconds, None);
    let json = serde_json::to_value(&r).unwrap();
    assert!(json["offset_seconds"].is_null());
    assert!(json.get("match_start_seconds").is_none());
}

#[test]
fn short_clip_is_refused() {
    assert_eq!(Clip::from_pcm(vec![0.0; FS as usize]).unwrap_err().to_string(), "clip shorter than two seconds");
}
