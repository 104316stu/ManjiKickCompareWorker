//! One job: fetch the clip, check it, download the video's audio with
//! yt-dlp, decode, compare, post the result. Same steps and wording as the
//! site's own compare.

use crate::api::{Api, ApiError, Job};
use crate::engine::Shared;
use crate::sidecars::Tools;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// yt-dlp's stderr that means YouTube is refusing this machine, not the video.
const REFUSED: &[&str] = &["HTTP Error 429", "Too Many Requests", "Sign in to confirm you", "confirm you're not a bot"];
/// The site's own format choice, so a verdict differs only if the machine does.
const FORMAT: &str = "bestaudio[ext=webm][protocol=https]/bestaudio[ext=m4a][protocol=https]/bestaudio";
/// Slowest speed the compare searches (the clip may be slowed down to this).
const SPEED_MIN: f64 = 0.78;

enum Download {
    Ok(PathBuf),
    /// yt-dlp's own refusal of the video: the site would get the same, so it is posted.
    Failed(String),
    /// A problem on this machine (timeout, can't start yt-dlp): hand the job back.
    Local(String),
    Refused(String),
    Cancelled,
}

/// Seconds left on a job's lease, by this machine's clock: the lease was
/// granted when the lease reply arrived and lasts `lease_seconds`.
pub fn lease_left(job: &Job, lease_seconds: f64) -> f64 {
    match job.leased_at {
        Some(at) => lease_seconds - at.elapsed().as_secs_f64(),
        None => 0.0,
    }
}

fn hand_back(api: &Api, job: &Job, why: &str) {
    crate::log!("job {}: {why}; handed back", job.job_id);
    api.release(std::slice::from_ref(job));
}

/// Runs one job. Returns the CPU seconds its helper processes used (for --duty).
pub fn run(sh: &Shared, api: &Api, job: &Job, tools: &Tools, max_video: u32, max_bytes: u64, lease_seconds: f64) -> f64 {
    let started = Instant::now();
    let clip_bytes = match api.clip(job) {
        Ok(b) => b,
        Err(ApiError::Stale) => {
            crate::log!("job {}: lease went stale", job.job_id);
            return 0.0;
        }
        Err(e) => {
            hand_back(api, job, &format!("clip not available ({e})"));
            sh.back_off(if matches!(e, ApiError::Slow) { 30.0 } else { 10.0 });
            return 0.0;
        }
    };
    let clip = match crate::core::load_clip(&tools.ffmpeg, &clip_bytes) {
        Ok(c) => c,
        // The site reaches the same verdict on the same bytes.
        Err(e @ crate::core::MatchError::ClipTooShort) if !sh.dry_run => {
            post(sh, api, job, "unknown", Map::new(), Some(e.to_string()), json!({}), started);
            return 0.0;
        }
        // Decoding trouble may be this machine's: let another worker or the site try.
        Err(e) => {
            hand_back(api, job, &format!("clip not decoded here ({e})"));
            return 0.0;
        }
    };
    drop(clip_bytes);
    let clip_seconds = crate::core::clip_match_seconds(&clip);
    if sh.dry_run {
        hand_back(api, job, &format!("dry run, clip {clip_seconds:.1} s ok, video {}", job.video_id));
        // Keep a dry run from churning through the site's queue.
        sh.back_off(15.0);
        std::thread::sleep(Duration::from_secs(15));
        return 0.0;
    }

    // Don't download a video that can't hold the clip at any speed (the
    // site's own check, on the matched part of the clip).
    if let Some(video_seconds) = job.video_seconds.filter(|v| *v > 0.0) {
        if clip_seconds > 0.0 && clip_seconds > video_seconds / SPEED_MIN * 2.0 + 30.0 {
            let msg = format!("clip is {} s, too long to be the {} s video at any speed",
                clip_seconds as i64, video_seconds as i64);
            post(sh, api, job, "unknown", Map::new(), Some(msg), json!({}), started);
            return 0.0;
        }
    }

    let folder = sh.paths.tmp_dir.join(format!("job-{}-{}", job.job_id, std::process::id()));
    let _ = std::fs::remove_dir_all(&folder);
    if let Err(e) = std::fs::create_dir_all(&folder) {
        hand_back(api, job, &format!("temp folder: {e}"));
        return 0.0;
    }
    let t = Instant::now();
    // The whole job has to land inside its lease.
    let left = (lease_left(job, lease_seconds) - 50.0).clamp(20.0, 240.0);
    let (dl, cpu) = download(&|| sh.cancelled(), tools, &job.video_id, &folder, max_video, max_bytes,
        Duration::from_secs_f64(left));
    let download_ms = t.elapsed().as_millis() as u64;
    match dl {
        Download::Ok(path) => {
            sh.youtube_ok();
            let t = Instant::now();
            let decoded = crate::core::decode_file(&tools.ffmpeg, &path, max_video as f64);
            let _ = std::fs::remove_dir_all(&folder);
            let x = match decoded {
                Ok(x) => x,
                Err(e) => {
                    hand_back(api, job, &format!("video not decoded here ({e})"));
                    return cpu;
                }
            };
            let outcome = if crate::core::video_too_short(&x) {
                Err(crate::core::VIDEO_TOO_SHORT.to_string())
            } else {
                Ok(crate::core::compare(&clip, &x))
            };
            drop(x);
            let compare_ms = t.elapsed().as_millis() as u64;
            let timing = json!({"download_ms": download_ms, "compare_ms": compare_ms});
            match outcome {
                Ok(result) => {
                    let verdict = result["verdict"].as_str().unwrap_or("unknown").to_string();
                    post(sh, api, job, &verdict, fields(&result), None, timing, started)
                }
                Err(e) => post(sh, api, job, "unknown", Map::new(), Some(e), timing, started),
            }
        }
        Download::Failed(msg) => {
            let _ = std::fs::remove_dir_all(&folder);
            post(sh, api, job, "unknown", Map::new(), Some(msg), json!({"download_ms": download_ms}), started)
        }
        Download::Local(msg) => {
            let _ = std::fs::remove_dir_all(&folder);
            hand_back(api, job, &msg);
        }
        Download::Refused(msg) => {
            let _ = std::fs::remove_dir_all(&folder);
            api.release(std::slice::from_ref(job));
            let wait = sh.youtube_refused();
            crate::log!("YouTube refused this machine ({msg}); pausing {} s", wait as u64);
        }
        Download::Cancelled => {
            let _ = std::fs::remove_dir_all(&folder);
            hand_back(api, job, "stopped");
        }
    }
    cpu
}

/// The result's numeric fields plus pitch_searched (no nulls, no strings).
fn fields(result: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(obj) = result.as_object() {
        for (k, v) in obj {
            if v.is_number() || (k == "pitch_searched" && v.is_boolean()) {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn post(sh: &Shared, api: &Api, job: &Job, verdict: &str, result: Map<String, Value>, error: Option<String>,
        mut timing: Value, started: Instant) {
    let total_ms = started.elapsed().as_millis() as u64;
    timing["total_ms"] = json!(total_ms);
    let error = error.map(|e| e.chars().take(200).collect::<String>());
    let body = json!({"job_id": job.job_id, "lease": job.lease, "verdict": verdict, "result": result,
                      "error": error, "timing": timing});
    let mut tries = 0;
    loop {
        match api.result(&body) {
            Ok(reply) => {
                sh.record(job, verdict, total_ms as f64 / 1000.0);
                let server = reply["server_verdict"].as_str().map(|v| format!(" (site: {v})")).unwrap_or_default();
                crate::log!("job {} {}: {}{}{} in {:.1} s", job.job_id, job.video_id, verdict, server,
                    error.as_ref().map(|e| format!(" - {e}")).unwrap_or_default(), total_ms as f64 / 1000.0);
                return;
            }
            Err(ApiError::Stale) => {
                crate::log!("job {}: result came after the lease ran out", job.job_id);
                return;
            }
            Err(ApiError::Auth(c)) => {
                crate::log!("job {}: result refused, token rejected (HTTP {c})", job.job_id);
                sh.token_rejected(c);
                return;
            }
            Err(e) if tries < 2 => {
                tries += 1;
                crate::log!("job {}: posting the result failed ({e}); retrying", job.job_id);
                std::thread::sleep(Duration::from_secs(5));
            }
            Err(e) => {
                crate::log!("job {}: result not delivered ({e})", job.job_id);
                return;
            }
        }
    }
}

fn download(cancel: &dyn Fn() -> bool, tools: &Tools, video_id: &str, folder: &Path, max_video: u32, max_bytes: u64,
            limit: Duration) -> (Download, f64) {
    let out = folder.join("out.txt");
    let err = folder.join("err.txt");
    let (Ok(fo), Ok(fe)) = (std::fs::File::create(&out), std::fs::File::create(&err)) else {
        return (Download::Local("could not create temp files".into()), 0.0);
    };
    let mut cmd = Command::new(&tools.ytdlp);
    cmd.args(["--ignore-config", "--no-playlist", "--no-cache-dir", "--no-part", "--no-progress", "--no-simulate"])
        .args(["-f", FORMAT])
        .args(["--match-filter", &format!("!is_live & duration<={max_video}")])
        .args(["--max-filesize", &max_bytes.to_string()])
        .args(["--socket-timeout", "20", "--retries", "2"])
        .arg("-o")
        .arg(folder.join("%(id)s.%(ext)s"))
        .args(["--print", "after_move:filepath"])
        .env("DENO_NO_UPDATE_CHECK", "1")
        .stdout(fo)
        .stderr(fe);
    if let Some(deno) = &tools.deno {
        cmd.arg("--js-runtimes").arg(format!("deno:{}", deno.display()));
    }
    // The id goes after "--" so it can never be read as an option.
    cmd.arg("--").arg(format!("https://www.youtube.com/watch?v={video_id}"));
    let (status, cpu) = crate::proc::run(&mut cmd, Instant::now() + limit, cancel);
    let stdout = std::fs::read_to_string(&out).unwrap_or_default();
    let stderr = std::fs::read_to_string(&err).unwrap_or_default();
    let outcome = match status {
        Err(crate::proc::RunError::Cancelled) => Download::Cancelled,
        Err(crate::proc::RunError::Timeout) => Download::Local(format!("download took over {} s", limit.as_secs())),
        Err(crate::proc::RunError::Spawn(e)) => Download::Local(format!("yt-dlp failed to start: {e}")),
        Ok(_) if REFUSED.iter().any(|m| stderr.contains(m)) => Download::Refused(first_line(&stderr)),
        Ok(_) => match stdout.lines().map(str::trim).filter(|l| !l.is_empty()).last().map(PathBuf::from) {
            Some(p) if p.is_file() => Download::Ok(p),
            _ if !stderr.trim().is_empty() => Download::Failed(first_line(&stderr)),
            _ => Download::Failed("video skipped (live or too long)".into()),
        },
    };
    (outcome, cpu)
}

/// yt-dlp's first meaningful error line, URLs taken out.
fn first_line(stderr: &str) -> String {
    let lines: Vec<&str> = stderr.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let line = lines
        .iter()
        .find(|l| l.starts_with("ERROR:"))
        .or_else(|| lines.iter().find(|l| !l.starts_with("WARNING:")))
        .or_else(|| lines.first())
        .copied()
        .unwrap_or("yt-dlp failed");
    let clean: Vec<String> = line
        .split_whitespace()
        .map(|w| if w.contains("://") { "<url>".to_string() } else { w.to_string() })
        .collect();
    clean.join(" ").chars().take(200).collect()
}

/// `mkworker test-download <video id>`: download and decode one video with
/// this machine's tools, to check yt-dlp, deno and ffmpeg work here.
pub fn probe(paths: &crate::paths::Paths, tools: &Tools, video_id: &str) -> Result<String, String> {
    let folder = paths.tmp_dir.join(format!("probe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&folder);
    std::fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
    let t = Instant::now();
    let (dl, _) = download(&|| crate::platform::stop_requested(), tools, video_id, &folder, 1800, 80 * 1024 * 1024,
        Duration::from_secs(150));
    let download_s = t.elapsed().as_secs_f64();
    let out = match dl {
        Download::Ok(path) => {
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let t = Instant::now();
            crate::core::decode_file(&tools.ffmpeg, &path, 1800.0)
                .map(|x| format!("downloaded {} ({} KB) in {:.1} s, decoded {:.1} s of audio in {:.1} s",
                    path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                    size / 1024, download_s, x.len() as f64 / 8000.0, t.elapsed().as_secs_f64()))
                .map_err(|e| e.to_string())
        }
        Download::Failed(m) | Download::Local(m) => Err(format!("download failed: {m}")),
        Download::Refused(m) => Err(format!("YouTube refused this machine: {m}")),
        Download::Cancelled => Err("cancelled".into()),
    };
    let _ = std::fs::remove_dir_all(&folder);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_line_prefers_errors_and_hides_urls() {
        let err = "WARNING: something
ERROR: [youtube] abc: Video unavailable https://www.youtube.com/watch?v=abc
";
        assert_eq!(first_line(err), "ERROR: [youtube] abc: Video unavailable <url>");
        assert_eq!(first_line(""), "yt-dlp failed");
    }

    #[test]
    fn fields_keeps_numbers_and_pitch_searched() {
        let v = json!({"verdict": "same", "speed": 1.0, "aligned": 12, "pitch_searched": true,
                       "semitones": null, "offset_seconds": -1.5});
        let n = fields(&v);
        let mut keys: Vec<_> = n.keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["aligned", "offset_seconds", "pitch_searched", "speed"]);
    }
}
