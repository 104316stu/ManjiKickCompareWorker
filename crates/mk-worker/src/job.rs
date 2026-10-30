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
const REFUSED: &[&str] = &["HTTP Error 429", "Too Many Requests", "Sign in to confirm you", "confirm you're not a bot", "confirm you\u{2019}re not a bot"];
/// The site's own format choice, so a verdict differs only if the machine does.
const FORMAT: &str = "bestaudio[ext=webm][protocol=https]/bestaudio[ext=m4a][protocol=https]/bestaudio";
/// Slowest speed the compare searches (the clip may be slowed down to this).
const SPEED_MIN: f64 = 0.78;
/// The site's own wording, so it stores these results instead of redoing them.
const LIVE: &str = "Live audio is not supported for comparison";

fn too_long(video_seconds: f64, max_video: u32) -> String {
    format!("video is {} min long, over the {} min limit for comparing",
        (video_seconds / 60.0).floor() as i64, max_video / 60)
}

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
            sh.back_off(if matches!(e, ApiError::Slow(_)) { 30.0 } else { 10.0 });
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

    // A video known to be over the limit isn't downloaded at all.
    if let Some(video_seconds) = job.video_seconds.filter(|v| v.is_finite() && *v > max_video as f64) {
        post(sh, api, job, "unknown", Map::new(), Some(too_long(video_seconds, max_video)), json!({}), started);
        return 0.0;
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
    let site_cookie = sh.site_cookie.lock().unwrap().clone();
    let Some(slot) = sh.youtube_slot() else {
        let _ = std::fs::remove_dir_all(&folder);
        hand_back(api, job, "stopped");
        return 0.0;
    };
    // The wait for a place counts against the lease too.
    let left = left.min((lease_left(job, lease_seconds) - 50.0).max(20.0));
    let (dl, cpu) = download(&|| sh.cancelled(), tools, &job.video_id, &folder, max_video, max_bytes,
        Duration::from_secs_f64(left), site_cookie.as_deref());
    drop(slot);
    let download_ms = t.elapsed().as_millis() as u64;
    match dl {
        Download::Ok(path) => {
            sh.youtube_ok();
            sh.add_download(std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0));
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
            sh.bot_check_without_cookie();
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

/// Searches in one job, entries kept per search, and the reply's size: the
/// site takes at most 16 KB.
const SEARCH_MAX: usize = 6;
const SEARCH_MAX_RESULTS: usize = 12;
const SEARCH_BODY_MAX: usize = 15_000;

enum Search {
    Ok(Vec<Value>),
    Refused(String),
    /// Anything else (network, yt-dlp): the site searches itself.
    Failed(String),
    Cancelled,
}

/// "ytsearch10:artist title": a count of 1-20 and a query on one line.
fn valid_search(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("ytsearch") else { return false };
    let Some((n, query)) = rest.split_once(':') else { return false };
    n.parse::<u32>().is_ok_and(|n| (1..=20).contains(&n))
        && !query.trim().is_empty()
        && query.chars().count() <= 200
        && !query.chars().any(|c| c.is_control())
}

/// A search job: a recognition's YouTube searches, run side by side. The
/// entries go back to the site, which ranks the videos and hands out their
/// compares.
pub fn run_search(sh: &Shared, api: &Api, job: &Job, tools: &Tools) -> f64 {
    let started = Instant::now();
    if job.searches.len() > SEARCH_MAX || !job.searches.iter().all(|s| valid_search(s)) {
        hand_back(api, job, "searches not understood");
        return 0.0;
    }
    if sh.dry_run {
        hand_back(api, job, &format!("dry run, {} searches", job.searches.len()));
        sh.back_off(15.0);
        return 0.0;
    }
    let site_cookie = sh.site_cookie.lock().unwrap().clone();
    let results: Vec<(Search, f64)> = std::thread::scope(|s| {
        let runs: Vec<_> = job
            .searches
            .iter()
            .enumerate()
            .map(|(i, search)| {
                let folder = sh.paths.tmp_dir.join(format!("search-{}-{i}-{}", job.job_id, std::process::id()));
                let cookie = site_cookie.as_deref();
                s.spawn(move || {
                    let out = youtube_search(&|| sh.cancelled(), tools, search, &folder, cookie);
                    let _ = std::fs::remove_dir_all(&folder);
                    out
                })
            })
            .collect();
        runs.into_iter().map(|h| h.join().unwrap_or((Search::Failed("search thread failed".into()), 0.0))).collect()
    });
    let cpu = results.iter().map(|(_, c)| c).sum();
    let mut entries = Map::new();
    for (search, (outcome, _)) in job.searches.iter().zip(results) {
        match outcome {
            Search::Ok(found) => {
                entries.insert(search.clone(), Value::Array(found));
            }
            Search::Refused(msg) => {
                api.release(std::slice::from_ref(job));
                sh.bot_check_without_cookie();
                let wait = sh.youtube_refused();
                crate::log!("YouTube refused this machine ({msg}); pausing {} s", wait as u64);
                return cpu;
            }
            Search::Failed(msg) => {
                hand_back(api, job, &format!("search failed ({msg})"));
                return cpu;
            }
            Search::Cancelled => {
                hand_back(api, job, "stopped");
                return cpu;
            }
        }
    }
    sh.youtube_ok();
    let total_ms = started.elapsed().as_millis() as u64;
    let mut body = json!({"job_id": job.job_id, "lease": job.lease, "entries": entries,
                          "timing": {"total_ms": total_ms}});
    // Over the size the site takes: the last results of the longest list go first.
    while body.to_string().len() > SEARCH_BODY_MAX {
        let lists = body["entries"].as_object_mut().unwrap();
        let Some(longest) = lists.values_mut().filter_map(Value::as_array_mut).max_by_key(|l| l.len()) else { break };
        if longest.pop().is_none() {
            break;
        }
    }
    let found: usize = body["entries"].as_object().map(|m| m.values().filter_map(Value::as_array).map(Vec::len).sum()).unwrap_or(0);
    let mut slow_tries = 0;
    let mut tries = 0;
    loop {
        match api.result(&body) {
            Ok(_) => {
                crate::log!("job {} search: {} videos from {} searches in {:.1} s", job.job_id, found,
                    job.searches.len(), total_ms as f64 / 1000.0);
                return cpu;
            }
            Err(ApiError::Stale) => {
                crate::log!("job {}: search came after the site stopped waiting", job.job_id);
                return cpu;
            }
            Err(ApiError::Auth(c)) => {
                sh.token_rejected(c);
                return cpu;
            }
            Err(ApiError::Slow(after)) if slow_tries < 3 => {
                let wait = after.unwrap_or(5.0).clamp(1.0, 30.0);
                slow_tries += 1;
                sh.back_off(wait);
                std::thread::sleep(Duration::from_secs_f64(wait));
            }
            Err(e) if tries < 2 => {
                tries += 1;
                crate::log!("job {}: posting the search failed ({e}); retrying", job.job_id);
                std::thread::sleep(Duration::from_secs(2));
            }
            Err(e) => {
                crate::log!("job {}: search not delivered ({e})", job.job_id);
                return cpu;
            }
        }
    }
}

/// Where a Shazam job may send its request, and the headers it may set.
const SHAZAM_URL_PREFIX: &str = "https://amp.shazam.com/discovery/v5/";
const SHAZAM_HEADERS: &[&str] = &["X-Shazam-Platform", "X-Shazam-AppVersion", "Accept", "Accept-Language", "User-Agent"];

/// A Shazam job: the site's recognition request (the clip's signature,
/// worked out on the site) sent from this address, since Shazam limits each
/// one. The track goes back cut to the fields the site reads.
pub fn run_shazam(sh: &Shared, api: &Api, job: &Job) -> f64 {
    let started = Instant::now();
    let req = &job.request;
    let url = req["url"].as_str().unwrap_or("");
    if !url.starts_with(SHAZAM_URL_PREFIX) || url.chars().any(|c| c.is_whitespace() || c.is_control())
        || !req["body"].is_object()
    {
        hand_back(api, job, "Shazam request not understood");
        return 0.0;
    }
    if sh.dry_run {
        hand_back(api, job, "dry run, Shazam request");
        return 0.0;
    }
    let agent = crate::api::agent(Duration::from_secs(30));
    let mut call = agent.post(url).set("Content-Type", "application/json");
    for name in SHAZAM_HEADERS {
        if let Some(v) = req["headers"][*name].as_str().filter(|v| v.len() < 300 && !v.chars().any(char::is_control)) {
            call = call.set(name, v);
        }
    }
    let (status, track) = match call.send_string(&req["body"].to_string()) {
        Ok(resp) => {
            let status = resp.status();
            let body: Value = resp.into_string().ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
            sh.shazam_ok();
            (status, shazam_track(&body["track"]))
        }
        Err(ureq::Error::Status(429, _)) => {
            let wait = sh.shazam_refused();
            crate::log!("Shazam turned this machine away (429); no Shazam jobs for {} s", wait as u64);
            (429, Value::Null)
        }
        Err(ureq::Error::Status(code, _)) => (code, Value::Null),
        Err(e) => {
            hand_back(api, job, &format!("Shazam not reached ({e})"));
            return 0.0;
        }
    };
    let total_ms = started.elapsed().as_millis() as u64;
    let body = json!({"job_id": job.job_id, "lease": job.lease, "shazam": {"status": status, "track": track},
                      "timing": {"total_ms": total_ms}});
    match api.result(&body) {
        Ok(_) => crate::log!("job {} Shazam: {} in {:.1} s", job.job_id,
            match (status, track["title"].as_str()) {
                (200, Some(t)) => format!("{} \u{2013} {}", track["subtitle"].as_str().unwrap_or(""), t),
                (200, None) => "no match".into(),
                (code, _) => format!("HTTP {code}"),
            }, total_ms as f64 / 1000.0),
        Err(ApiError::Auth(c)) => sh.token_rejected(c),
        Err(e) => crate::log!("job {}: Shazam answer not delivered ({e})", job.job_id),
    }
    0.0
}

/// The parts of a Shazam track the site reads: names, links, ISRC, the
/// Apple Music actions and the video section.
fn shazam_track(t: &Value) -> Value {
    if !t.is_object() {
        return Value::Null;
    }
    let cut = |v: &Value, n: usize| v.as_str().map(|s| Value::String(s.chars().take(n).collect())).unwrap_or(Value::Null);
    let id = |v: &Value| match v {
        Value::String(s) => Value::String(s.chars().take(40).collect()),
        Value::Number(n) => Value::String(n.to_string()),
        _ => Value::Null,
    };
    let actions: Vec<Value> = t["hub"]["actions"].as_array().map(Vec::as_slice).unwrap_or(&[]).iter().take(12)
        .map(|a| json!({"type": cut(&a["type"], 40), "id": id(&a["id"]), "uri": cut(&a["uri"], 500)}))
        .collect();
    let sections: Vec<Value> = t["sections"].as_array().map(Vec::as_slice).unwrap_or(&[]).iter().take(12)
        .map(|x| json!({"type": cut(&x["type"], 40), "youtubeurl": cut(&x["youtubeurl"], 500)}))
        .collect();
    json!({"title": cut(&t["title"], 300), "subtitle": cut(&t["subtitle"], 300), "url": cut(&t["url"], 500),
           "key": id(&t["key"]), "isrc": cut(&t["isrc"], 20), "hub": {"actions": actions}, "sections": sections})
}

/// One yt-dlp search, entries only (`--flat-playlist`: no video pages).
fn youtube_search(cancel: &dyn Fn() -> bool, tools: &Tools, search: &str, folder: &Path,
                  site_cookie: Option<&crate::cookie::SiteCookie>) -> (Search, f64) {
    let _ = std::fs::remove_dir_all(folder);
    if std::fs::create_dir_all(folder).is_err() {
        return (Search::Failed("temp folder".into()), 0.0);
    }
    let out = folder.join("out.json");
    let err = folder.join("err.txt");
    let (Ok(fo), Ok(fe)) = (std::fs::File::create(&out), std::fs::File::create(&err)) else {
        return (Search::Failed("could not create temp files".into()), 0.0);
    };
    let mut cmd = Command::new(&tools.ytdlp);
    cmd.args(["--ignore-config", "--no-cache-dir", "--flat-playlist", "--dump-single-json", "--no-warnings"])
        .args(["--socket-timeout", "12", "--retries", "1", "--extractor-retries", "1"])
        .env("DENO_NO_UPDATE_CHECK", "1")
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1")
        .stdout(fo)
        .stderr(fe);
    if let Some(deno) = &tools.deno {
        cmd.arg("--js-runtimes").arg(format!("deno:{}", deno.display()));
    }
    // Cookies as for a download (see download()): one file per run.
    let mut _cookie_file = None;
    if let Some(cookies) = std::env::var_os("MKW_COOKIES").filter(|p| Path::new(p).is_file()) {
        cmd.arg("--cookies").arg(cookies);
    } else if let Some(c) = site_cookie {
        if let Ok(f) = crate::cookie::write_temp(c, folder) {
            cmd.arg("--cookies").arg(f.path());
            _cookie_file = Some(f);
        }
    }
    cmd.arg("--").arg(search);
    let (status, cpu) = crate::proc::run(&mut cmd, Instant::now() + Duration::from_secs(45), cancel);
    drop(_cookie_file.take());
    let read = |p: &Path| String::from_utf8_lossy(&std::fs::read(p).unwrap_or_default()).into_owned();
    let stderr = read(&err);
    let outcome = match status {
        Err(crate::proc::RunError::Cancelled) => Search::Cancelled,
        Err(crate::proc::RunError::Timeout) => Search::Failed("search took over 45 s".into()),
        Err(crate::proc::RunError::Spawn(e)) => Search::Failed(format!("yt-dlp failed to start: {e}")),
        Ok(_) if REFUSED.iter().any(|m| stderr.contains(m)) => Search::Refused(first_line(&stderr)),
        Ok(_) => match serde_json::from_str::<Value>(&read(&out)) {
            Ok(v) => Search::Ok(search_entries(&v)),
            Err(_) => Search::Failed(first_line(&stderr)),
        },
    };
    (outcome, cpu)
}

/// What the site reads from each result: id, title, channel or uploader,
/// duration; titles cut short so a whole job fits in one reply.
fn search_entries(v: &Value) -> Vec<Value> {
    let cut = |s: &Value, n: usize| s.as_str().map(|s| Value::String(s.chars().take(n).collect()));
    v["entries"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .filter(|e| e["id"].as_str().is_some_and(|id| id.len() == 11))
        .take(SEARCH_MAX_RESULTS)
        .map(|e| {
            let mut m = Map::new();
            m.insert("id".into(), e["id"].clone());
            if let Some(t) = cut(&e["title"], 120) {
                m.insert("title".into(), t);
            }
            if let Some(c) = cut(&e["channel"], 80) {
                m.insert("channel".into(), c);
            } else if let Some(u) = cut(&e["uploader"], 80) {
                m.insert("uploader".into(), u);
            }
            if e["duration"].is_number() {
                m.insert("duration".into(), e["duration"].clone());
            }
            Value::Object(m)
        })
        .collect()
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
    let mut slow_tries = 0;
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
            Err(ApiError::Slow(after)) if slow_tries < 6 => {
                // Retry-After when given, else 5 s doubling; new leases wait too.
                let wait = after.unwrap_or(5.0 * 2f64.powi(slow_tries)).clamp(1.0, 120.0);
                slow_tries += 1;
                sh.back_off(wait);
                crate::log!("job {}: the site asked to slow down; posting again in {:.0} s", job.job_id, wait);
                std::thread::sleep(Duration::from_secs_f64(wait));
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
            limit: Duration, site_cookie: Option<&crate::cookie::SiteCookie>) -> (Download, f64) {
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
        // Length and live flag first (printed even when the filter skips it),
        // then the downloaded file.
        .args(["--print", "pre_process:%(duration)s|%(is_live)s"])
        .args(["--print", "after_move:filepath"])
        .env("DENO_NO_UPDATE_CHECK", "1")
        // yt-dlp (Python) writes UTF-8 then, not the Windows code page.
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1")
        .stdout(fo)
        .stderr(fe);
    if let Some(deno) = &tools.deno {
        cmd.arg("--js-runtimes").arg(format!("deno:{}", deno.display()));
    }
    // An optional YouTube cookie file (Netscape format) for machines whose
    // address YouTube asks to sign in; yt-dlp may rewrite it, so it's used
    // from its own path and never sent anywhere else.
    // The site's cookie (trusted workers) goes to a one-run file that is
    // deleted when yt-dlp is done (`_cookie_file` is dropped at the end of
    // this function, whatever happens). A local MKW_COOKIES wins.
    let mut _cookie_file = None;
    if let Some(cookies) = std::env::var_os("MKW_COOKIES").filter(|p| Path::new(p).is_file()) {
        cmd.arg("--cookies").arg(cookies);
    } else if let Some(c) = site_cookie {
        match crate::cookie::write_temp(c, folder) {
            Ok(f) => {
                cmd.arg("--cookies").arg(f.path());
                _cookie_file = Some(f);
            }
            Err(e) => crate::log!("site cookie not usable for this download: {e}"),
        }
    }
    // The id goes after "--" so it can never be read as an option.
    cmd.arg("--").arg(format!("https://www.youtube.com/watch?v={video_id}"));
    let (status, cpu) = crate::proc::run(&mut cmd, Instant::now() + limit, cancel);
    drop(_cookie_file.take());
    // Lossy: one byte that isn't UTF-8 (a curly quote in the console's code
    // page) must not blank the whole message.
    let read = |p: &Path| String::from_utf8_lossy(&std::fs::read(p).unwrap_or_default()).into_owned();
    let stdout = read(&out);
    let stderr = read(&err);
    let outcome = match status {
        Err(crate::proc::RunError::Cancelled) => Download::Cancelled,
        Err(crate::proc::RunError::Timeout) => Download::Local(format!("download took over {} s", limit.as_secs())),
        Err(crate::proc::RunError::Spawn(e)) => Download::Local(format!("yt-dlp failed to start: {e}")),
        Ok(_) if REFUSED.iter().any(|m| stderr.contains(m)) => Download::Refused(first_line(&stderr)),
        Ok(_) => {
            let lines: Vec<&str> = stdout.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
            let file = lines.iter().rev().map(PathBuf::from).find(|p| p.is_file());
            // "1234.0|False" from the pre_process print.
            let (duration, live) = lines
                .iter()
                .find_map(|l| l.split_once('|'))
                .map(|(d, l)| (d.trim().parse::<f64>().ok().filter(|d| d.is_finite()), l.trim() == "True"))
                .unwrap_or((None, false));
            match file {
                Some(p) => Download::Ok(p),
                None if live => Download::Failed(LIVE.into()),
                None if duration.is_some_and(|d| d > max_video as f64) => {
                    Download::Failed(too_long(duration.unwrap_or(0.0), max_video))
                }
                None if !stderr.trim().is_empty() => Download::Failed(first_line(&stderr)),
                None => Download::Failed("video skipped (live or too long)".into()),
            }
        }
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
        Duration::from_secs(150), None);
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
    fn shazam_track_keeps_what_the_site_reads() {
        let t = json!({"title": "Roses", "subtitle": "The Chainsmokers", "url": "https://www.shazam.com/track/1",
            "key": 123, "isrc": "USQX91500872", "images": {"big": "x"}, "share": {"text": "long"},
            "hub": {"actions": [{"type": "applemusicplay", "id": "1440", "name": "x"}, {"type": "uri", "uri": "https://itunes.apple.com/p"}]},
            "sections": [{"type": "SONG", "metadata": []}, {"type": "VIDEO", "youtubeurl": "https://cdn.shazam.com/video/v3/x"}]});
        let k = shazam_track(&t);
        assert_eq!(k["title"], "Roses");
        assert_eq!(k["key"], "123");
        assert!(k.get("images").is_none() && k.get("share").is_none());
        assert_eq!(k["hub"]["actions"][0]["id"], "1440");
        assert_eq!(k["sections"][1]["youtubeurl"], "https://cdn.shazam.com/video/v3/x");
        assert_eq!(shazam_track(&Value::Null), Value::Null);
    }

    #[test]
    fn only_plain_searches_are_run() {
        assert!(valid_search("ytsearch10:artist song"));
        assert!(!valid_search("ytsearch0:x") && !valid_search("ytsearch21:x") && !valid_search("ytsearch10:"));
        assert!(!valid_search("https://example.com/") && !valid_search("ytsearch5:a\nb"));
    }

    #[test]
    fn search_entries_keep_what_the_site_reads() {
        let v = json!({"entries": [
            {"id": "abcdefghijk", "title": "Song", "channel": "Artist - Topic", "duration": 201.0, "url": "x"},
            {"id": "short", "title": "dropped"},
            {"id": "bcdefghijkl", "title": "t".repeat(300), "uploader": "Someone"}]});
        let e = search_entries(&v);
        assert_eq!(e.len(), 2);
        assert_eq!(e[0], json!({"id": "abcdefghijk", "title": "Song", "channel": "Artist - Topic", "duration": 201.0}));
        assert_eq!(e[1]["title"].as_str().unwrap().len(), 120);
        assert_eq!(e[1]["uploader"], "Someone");
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

#[cfg(test)]
mod wording_tests {
    #[test]
    fn too_long_matches_the_site() {
        assert_eq!(super::too_long(3725.0, 1800), "video is 62 min long, over the 30 min limit for comparing");
    }
}
