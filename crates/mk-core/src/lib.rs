//! Port of the site's frontend/audio_match.py: is a Roblox clip the same
//! recording as a YouTube video's audio? Same constellation fingerprint,
//! same parameters, same verdicts; the numerics follow numpy's semantics
//! so a verdict differs only where floating point rounding does.
//!
//! Public contract (used by mk-worker):
//!
//!   pub const FS: u32 = 8000;
//!   pub const ALGO_VERSION: &str;                       // "rs-<version>"
//!   pub fn decode(ffmpeg: &Path, source: Source, max_seconds: f64) -> Result<Vec<f32>, MatchError>
//!       // the server's ffmpeg command: -v error -nostdin -i <src> -t <int> -vn -f f32le -ac 1 -ar 8000 pipe:1
//!   pub fn load_clip(ffmpeg: &Path, bytes: &[u8]) -> Result<Clip, MatchError>
//!   pub fn compare(clip: &Clip, x_video: &[f32]) -> CompareResult
//!   pub struct CompareResult { verdict, speed, aligned, density, prominence, coverage, offset_seconds,
//!       clip_seconds, video_seconds, semitones: Option<f64>, pitch_searched: bool,
//!       match_start_seconds, match_end_seconds, clip_total_seconds, extra_seconds: Option<f64>, .. }  // serde::Serialize
//!   pub enum MatchError { ClipTooShort, Undecodable(String), Ffmpeg(String) }  // Display = the server's wording
//!
//! The video is decoded by the caller with `decode(.., MAX_VIDEO_SECONDS)`.
//! The server refuses a video under two seconds ("video audio shorter than
//! two seconds"); check it with [`video_too_short`] before calling
//! [`compare`].

use std::fmt;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::ser::{Serialize, SerializeMap, Serializer};

mod fingerprint;
mod matcher;
mod params;
#[doc(hidden)]
pub mod pyops;

pub use matcher::Options;

pub const FS: u32 = 8000;
pub const ALGO_VERSION: &str = concat!("rs-", env!("CARGO_PKG_VERSION"));
pub const MAX_CLIP_SECONDS: f64 = params::MAX_CLIP_SECONDS;
pub const MAX_VIDEO_SECONDS: f64 = params::MAX_VIDEO_SECONDS;
pub const MAX_CLIP_TOTAL_SECONDS: f64 = params::MAX_CLIP_TOTAL_SECONDS;
/// The server's error text for a video under two seconds.
pub const VIDEO_TOO_SHORT: &str = "video audio shorter than two seconds";
const DECODE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq)]
pub enum MatchError {
    /// "clip shorter than two seconds"
    ClipTooShort,
    /// "undecodable audio" plus ": <ffmpeg's last stderr line>" when it said one.
    Undecodable(String),
    /// ffmpeg could not be run (missing binary, timeout): "ffmpeg failed: ..."
    Ffmpeg(String),
}

impl fmt::Display for MatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MatchError::ClipTooShort => f.write_str("clip shorter than two seconds"),
            MatchError::Undecodable(detail) if detail.is_empty() => f.write_str("undecodable audio"),
            MatchError::Undecodable(detail) => write!(f, "undecodable audio: {detail}"),
            MatchError::Ffmpeg(msg) => write!(f, "ffmpeg failed: {msg}"),
        }
    }
}

impl std::error::Error for MatchError {}

pub enum Source<'a> {
    Bytes(&'a [u8]),
    Path(&'a Path),
}

/// Mono f32 PCM at FS from `source`, only its first `max_seconds`.
pub fn decode(ffmpeg: &Path, source: Source, max_seconds: f64) -> Result<Vec<f32>, MatchError> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-v", "error", "-nostdin", "-i"]);
    let stdin_bytes = match source {
        Source::Bytes(b) => {
            cmd.arg("pipe:0");
            Some(b)
        }
        Source::Path(p) => {
            cmd.arg(p);
            None
        }
    };
    cmd.args(["-t", &(max_seconds.trunc() as i64).to_string()]);
    cmd.args(["-vn", "-f", "f32le", "-ac", "1", "-ar", &FS.to_string(), "pipe:1"]);
    cmd.stdin(if stdin_bytes.is_some() { Stdio::piped() } else { Stdio::null() });
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let child = cmd.spawn().map_err(|e| MatchError::Ffmpeg(e.to_string()))?;
    // Killed and reaped on every way out of here, unless it exited itself.
    let mut child = KillOnDrop(child);
    let out_pipe = child.0.stdout.take();
    let err_pipe = child.0.stderr.take();
    let (Some(mut out_pipe), Some(mut err_pipe)) = (out_pipe, err_pipe) else {
        return Err(MatchError::Ffmpeg("no output pipes".into()));
    };
    let out_t = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = out_pipe.read_to_end(&mut v);
        v
    });
    let err_t = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = err_pipe.read_to_end(&mut v);
        v
    });
    let in_t = match (stdin_bytes, child.0.stdin.take()) {
        (Some(b), Some(mut pipe)) => {
            let data = b.to_vec();
            Some(std::thread::spawn(move || {
                let _ = pipe.write_all(&data);
            }))
        }
        _ => None,
    };
    let deadline = Instant::now() + DECODE_TIMEOUT;
    let status = loop {
        match child.0.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() > deadline => {
                return Err(MatchError::Ffmpeg(format!(
                    "timed out after {} seconds",
                    DECODE_TIMEOUT.as_secs()
                )));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => return Err(MatchError::Ffmpeg(e.to_string())),
        }
    };
    if let Some(t) = in_t {
        let _ = t.join();
    }
    let stdout = out_t.join().unwrap_or_default();
    let stderr = err_t.join().unwrap_or_default();
    if !status.success() || stdout.is_empty() {
        return Err(MatchError::Undecodable(last_line(&stderr)));
    }
    Ok(stdout.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
}

/// A running ffmpeg that is killed and waited for when dropped, so no
/// error path leaves it behind (pipes close once it is gone, which ends the
/// reader threads).
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Ok(None) | Err(_) = self.0.try_wait() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

/// Python: stderr.decode("utf-8", "replace").strip().splitlines()[-1][:160]
fn last_line(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let is_break = |c: char| {
        matches!(c, '\n' | '\r' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}' | '\u{2029}')
    };
    let trimmed = text.trim();
    let last = trimmed.split(is_break).last().unwrap_or("");
    last.chars().take(160).collect()
}

/// A decoded clip: `pcm` is its first MAX_CLIP_SECONDS (what is matched),
/// `total_seconds` the whole file's length up to MAX_CLIP_TOTAL_SECONDS.
#[derive(Debug, Clone)]
pub struct Clip {
    pub pcm: Vec<f32>,
    pub total_seconds: f64,
}

impl Clip {
    /// load_clip's checks and cut, from PCM already decoded with
    /// MAX_CLIP_TOTAL_SECONDS.
    pub fn from_pcm(mut x: Vec<f32>) -> Result<Clip, MatchError> {
        if x.len() < FS as usize * 2 {
            return Err(MatchError::ClipTooShort);
        }
        let total_seconds = pyops::py_round(x.len() as f64 / FS as f64, 2);
        x.truncate((MAX_CLIP_SECONDS * FS as f64) as usize);
        Ok(Clip { pcm: x, total_seconds })
    }
}

pub fn load_clip(ffmpeg: &Path, bytes: &[u8]) -> Result<Clip, MatchError> {
    Clip::from_pcm(decode(ffmpeg, Source::Bytes(bytes), MAX_CLIP_TOTAL_SECONDS)?)
}

/// True when the server would refuse this video audio (VIDEO_TOO_SHORT).
pub fn video_too_short(x_video: &[f32]) -> bool {
    x_video.len() < FS as usize * 2
}

/// The site's compare_pcm(clip.pcm, x_video, clip.total_seconds), with
/// its 40 s shift-search budget (Options::default()).
pub fn compare(clip: &Clip, x_video: &[f32]) -> CompareResult {
    compare_with(clip, x_video, &Options::default())
}

/// compare() with explicit search limits (Options::exhaustive() searches
/// every tempo regardless of time, for reproducible runs).
pub fn compare_with(clip: &Clip, x_video: &[f32], opts: &Options) -> CompareResult {
    matcher::compare_pcm(&clip.pcm, x_video, Some(clip.total_seconds), opts)
}

/// The server's result dict. Serializes to exactly its keys: fields the
/// server leaves out are left out here too.
#[derive(Debug, Clone, PartialEq)]
pub struct CompareResult {
    /// "same" | "close" | "different"
    pub verdict: String,
    pub speed: f64,
    pub aligned: i64,
    pub density: f64,
    pub prominence: f64,
    pub coverage: f64,
    /// None (JSON null) on 'different'.
    pub offset_seconds: Option<f64>,
    /// Present on a match.
    pub matched_segments: Option<i64>,
    pub clip_hashes: Option<i64>,
    /// Only when the pitch was found moved apart from the tempo.
    pub semitones: Option<f64>,
    /// Serialized only when true.
    pub pitch_searched: bool,
    /// Not in the server's dict: when the shift search ran, how many of its
    /// coarse tempos (72 in the grid) were voted on. Serialized only then.
    pub shift_tempos: Option<usize>,
    /// Not in the server's dict: the shift search's time budget (or
    /// `max_tempos`) ended it before the tempo grid did, so a pitch-shifted
    /// match may have been missed. Serialized only when true.
    pub shift_cut: bool,
    pub clip_seconds: f64,
    pub video_seconds: f64,
    pub clip_total_seconds: f64,
    /// The song's span in the clip, when anything matched.
    pub match_start_seconds: Option<f64>,
    pub match_end_seconds: Option<f64>,
    /// Present (maybe null: unknown) whenever match_end_seconds is.
    pub extra_seconds: Option<f64>,
}

impl Serialize for CompareResult {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(None)?;
        m.serialize_entry("clip_seconds", &self.clip_seconds)?;
        m.serialize_entry("video_seconds", &self.video_seconds)?;
        m.serialize_entry("clip_total_seconds", &self.clip_total_seconds)?;
        if self.pitch_searched {
            m.serialize_entry("pitch_searched", &true)?;
        }
        if let Some(n) = self.shift_tempos {
            m.serialize_entry("shift_tempos", &n)?;
        }
        if self.shift_cut {
            m.serialize_entry("shift_cut", &true)?;
        }
        m.serialize_entry("verdict", &self.verdict)?;
        m.serialize_entry("speed", &self.speed)?;
        m.serialize_entry("aligned", &self.aligned)?;
        m.serialize_entry("density", &self.density)?;
        m.serialize_entry("prominence", &self.prominence)?;
        m.serialize_entry("coverage", &self.coverage)?;
        m.serialize_entry("offset_seconds", &self.offset_seconds)?;
        if let Some(v) = self.matched_segments {
            m.serialize_entry("matched_segments", &v)?;
        }
        if let Some(v) = self.clip_hashes {
            m.serialize_entry("clip_hashes", &v)?;
        }
        if let Some(v) = self.semitones {
            m.serialize_entry("semitones", &v)?;
        }
        if let (Some(a), Some(b)) = (self.match_start_seconds, self.match_end_seconds) {
            m.serialize_entry("match_start_seconds", &a)?;
            m.serialize_entry("match_end_seconds", &b)?;
            m.serialize_entry("extra_seconds", &self.extra_seconds)?;
        }
        m.end()
    }
}

/// Internals exposed for the parity harness and tests; not a stable API.
#[doc(hidden)]
pub mod internals {
    pub use crate::fingerprint::{hashes, peaks, spectrogram, ClipPrint, VideoPrint};
    pub use crate::matcher::{compare_pcm, excerpt, loud_run, speed_grid, tempo_grid};
}
