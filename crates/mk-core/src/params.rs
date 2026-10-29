//! Analysis parameters, identical to audio_match.py's.

pub const FS_USIZE: usize = 8000;
pub const N_FFT: usize = 1024;
pub const HOP: usize = 256;
pub const FPS: f64 = 8000.0 / 256.0; // 31.25 frames/s
pub const MAX_CLIP_SECONDS: f64 = 240.0;
pub const MAX_VIDEO_SECONDS: f64 = 1800.0;
pub const MAX_CLIP_TOTAL_SECONDS: f64 = 900.0;

pub const PEAK_DF: usize = 10;
pub const PEAK_DT: usize = 5;
pub const PEAK_MIN_DB: f64 = -60.0;
pub const PEAKS_PER_SECOND: usize = 28;

pub const FANOUT: usize = 10;
pub const MAX_DT: i64 = 63;
pub const FREQ_SHIFT: i64 = 1;

pub const MAX_HASH_REPEATS: usize = 40;
pub const SEGMENT_SECONDS: f64 = 5.0;
pub const MIN_ALIGNED: i64 = 40;
pub const MIN_DENSITY: f64 = 4.0;
pub const SEGMENT_MIN_DENSITY: f64 = 2.5;
pub const SAME_COVERAGE: f64 = 0.80;
pub const SAME_MIN_DENSITY: f64 = 25.0;
pub const SAME_SPEED_TOLERANCE: f64 = 0.015;

pub const SPEED_MIN: f64 = 0.78;
pub const SPEED_MAX: f64 = 1.46;
pub const SPEED_STEP: f64 = 1.01;
pub const SPEED_REFINE_STEP: f64 = 0.0025;
pub const SPEED_REFINE_SPAN: f64 = 0.01;
pub const SWEEP_SECONDS: f64 = 60.0;
pub const SWEEP_ENOUGH_DENSITY: f64 = 30.0;

pub const EXCERPT_FRAME_SECONDS: f64 = 1.0;
pub const EXCERPT_LOUD_PERCENTILE: f64 = 90.0;
pub const EXCERPT_LOUD_DROP_DB: f64 = 10.0;
pub const EXCERPT_GAP_SECONDS: f64 = 3.0;

pub const DRIFT_WINDOW: i64 = 14;

pub const SHIFT_SEMITONES: f64 = 4.0;
pub const SHIFT_STEP: f64 = 0.125;
pub const SHIFT_REFINE_STEP: f64 = 1.0 / 32.0;
pub const SHIFT_TEMPO_MAX: f64 = 1.6;
pub const SHIFT_BUDGET_SECONDS_DEFAULT: f64 = 40.0;
