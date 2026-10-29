//! The one place the worker touches mk-core, so a change in its API is a
//! change here. With `--features core-shim` a stand-in is used instead
//! (decodes nothing, compares nothing): only for compile checks and
//! --dry-run before mk-core exists.

use std::path::Path;

#[cfg(not(feature = "core-shim"))]
mod imp {
    use super::*;
    pub use mk_core::{video_too_short, Clip, MatchError, ALGO_VERSION, VIDEO_TOO_SHORT};

    pub fn load_clip(ffmpeg: &Path, bytes: &[u8]) -> Result<Clip, MatchError> {
        mk_core::load_clip(ffmpeg, bytes)
    }

    /// Seconds of the clip that get matched (its first MAX_CLIP_SECONDS),
    /// which the site's too-long-for-the-video check uses.
    pub fn clip_match_seconds(clip: &Clip) -> f64 {
        clip.pcm.len() as f64 / mk_core::FS as f64
    }

    pub fn decode_file(ffmpeg: &Path, path: &Path, max_seconds: f64) -> Result<Vec<f32>, MatchError> {
        mk_core::decode(ffmpeg, mk_core::Source::Path(path), max_seconds)
    }

    /// The comparison as JSON (serde), so the worker only picks fields by name.
    pub fn compare(clip: &Clip, x_video: &[f32]) -> serde_json::Value {
        serde_json::to_value(mk_core::compare(clip, x_video)).unwrap_or(serde_json::Value::Null)
    }
}

#[cfg(feature = "core-shim")]
mod imp {
    use super::*;
    pub const ALGO_VERSION: &str = "rs-shim";
    pub const VIDEO_TOO_SHORT: &str = "video audio shorter than two seconds";

    pub fn video_too_short(x: &[f32]) -> bool {
        x.len() < 16000
    }

    pub struct Clip {
        pub total_seconds: f64,
    }

    #[derive(Debug)]
    pub enum MatchError {
        ClipTooShort,
        Undecodable(String),
        Ffmpeg(String),
    }

    impl std::fmt::Display for MatchError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                MatchError::ClipTooShort => write!(f, "clip shorter than two seconds"),
                MatchError::Undecodable(s) => write!(f, "undecodable audio: {s}"),
                MatchError::Ffmpeg(s) => write!(f, "ffmpeg failed: {s}"),
            }
        }
    }

    pub fn load_clip(_ffmpeg: &Path, bytes: &[u8]) -> Result<Clip, MatchError> {
        if bytes.len() < 1000 {
            return Err(MatchError::ClipTooShort);
        }
        Ok(Clip { total_seconds: 30.0 })
    }

    pub fn clip_match_seconds(clip: &Clip) -> f64 {
        clip.total_seconds.min(240.0)
    }

    pub fn decode_file(_ffmpeg: &Path, _path: &Path, _max: f64) -> Result<Vec<f32>, MatchError> {
        Err(MatchError::Ffmpeg("core-shim build: no decoding".into()))
    }

    pub fn compare(_clip: &Clip, _x: &[f32]) -> serde_json::Value {
        serde_json::json!({"verdict": "unknown"})
    }
}

pub use imp::*;
