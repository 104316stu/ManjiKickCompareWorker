//! The worker loop: one manager thread leases jobs and decides the mode's
//! policy; worker threads run the jobs. The tray and the CLI drive the same
//! engine through `Shared`.

use crate::api::{Api, ApiError, Job, MAX_BATCH};
use crate::config::{self, Config, Mode, Policy};
use crate::paths::Paths;
use crate::platform;
use crate::sidecars::Tools;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// A job is only started with this much of its lease left.
const MIN_LEASE_LEFT: f64 = 75.0;
/// Auto switches to Max after this long without keyboard/mouse input.
const AUTO_IDLE_SECONDS: f64 = 300.0;
/// Max halves its threads this long after a YouTube cooldown.
const HALF_AFTER_COOLDOWN: f64 = 1800.0;

#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    Starting,
    Setup(String),
    NoToken,
    TokenRejected(String),
    Offline(String),
    Off,
    Paused(String),
    Cooldown,
    /// Finishing up to restart into a new release.
    Updating(String),
    Running,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Today {
    pub date: String,
    pub done: u64,
    pub same: u64,
    pub errors: u64,
    pub secs: f64,
    /// Video audio downloaded.
    #[serde(default)]
    pub bytes: u64,
}

impl Today {
    fn add(&mut self, verdict: &str, secs: f64) {
        self.done += 1;
        self.same += (verdict == "same") as u64;
        self.errors += (verdict == "unknown") as u64;
        self.secs += secs;
    }
}


pub struct State {
    pub phase: Phase,
    pub policy: Policy,
    pub threads: usize,
    pub busy: usize,
    /// Threads sitting out their --duty rest (no job, but no new lease for them).
    pub resting: usize,
    pub queue: Vec<Job>,
    pub cooldown_until: f64,
    pub cooldown_streak: u32,
    pub half_until: f64,
    pub today: Today,
    /// Since this machine first ran the worker.
    pub all: Today,
    pub last: Option<String>,
    /// "Up to date (v0.2.0)", "Updating to v0.2.1", ...
    pub update_text: String,
    /// A checked new release waiting to be swapped in: (file, tag).
    pub update_ready: Option<(std::path::PathBuf, String)>,
    pub update_checking: bool,
    /// "Check for updates" was clicked.
    pub update_check_now: bool,
    /// Registering a new worker: not before this.
    pub register_after: f64,
    /// A token worked at least once this run (a later 401/403 is "rejected").
    pub ever_connected: bool,
    /// Set when the engine stopped to restart into the swapped-in program.
    pub restart: bool,
    pub server_name: Option<String>,
    pub trust: Option<String>,
    pub max_video_seconds: u32,
    pub video_max_bytes: u64,
    /// How long a lease lasts (from /hello), counted from when it arrived.
    pub lease_seconds: f64,
    /// No new lease before this (epoch seconds): set after jobs go back.
    pub lease_after: f64,
    pub token_changed: bool,
    /// A worker thread got 401/403: the manager drops its connection.
    pub auth_failed: bool,
    pub tools: Option<Tools>,
}

pub struct Shared {
    pub paths: Paths,
    pub cfg: Mutex<Config>,
    /// --threads (CLI) overrides every mode.
    pub threads_override: Option<usize>,
    pub dry_run: bool,
    /// The tray: battery/fullscreen/busy pauses and the Running toggle apply.
    pub interactive: bool,
    pub quit: AtomicBool,
    pub st: Mutex<State>,
    /// The site's YouTube cookie (trusted workers), in memory only.
    pub site_cookie: Mutex<Option<Arc<crate::cookie::SiteCookie>>>,
    /// A bot check hit while holding no cookie: ask /hello again (when).
    pub cookie_recheck: Mutex<Option<f64>>,
    /// yt-dlp runs going now (config youtube_at_once caps them).
    youtube_running: Mutex<usize>,
    youtube_free: Condvar,
}

/// A place among the yt-dlp runs allowed at once; freed when dropped.
pub struct YoutubeSlot<'a>(&'a Shared);

impl Drop for YoutubeSlot<'_> {
    fn drop(&mut self) {
        *self.0.youtube_running.lock().unwrap() -= 1;
        self.0.youtube_free.notify_one();
    }
}

impl Shared {
    pub fn new(paths: Paths, cfg: Config, threads_override: Option<usize>, dry_run: bool, interactive: bool) -> Arc<Shared> {
        let cores = config::cores();
        let policy = config::policy(cfg.mode, cores, false);
        let (mut today, all) = load_totals(&paths);
        if today.date != platform::local_date() {
            today = Today { date: platform::local_date(), ..Default::default() };
        }
        let max_video = cfg.max_video_seconds;
        Arc::new(Shared {
            paths,
            cfg: Mutex::new(cfg),
            threads_override,
            dry_run,
            interactive,
            quit: AtomicBool::new(false),
            site_cookie: Mutex::new(None),
            cookie_recheck: Mutex::new(None),
            youtube_running: Mutex::new(0),
            youtube_free: Condvar::new(),
            st: Mutex::new(State {
                phase: Phase::Starting,
                policy,
                threads: policy.threads,
                busy: 0,
                resting: 0,
                queue: Vec::new(),
                cooldown_until: 0.0,
                cooldown_streak: 0,
                half_until: 0.0,
                today,
                all,
                last: None,
                update_text: String::new(),
                update_ready: None,
                update_checking: false,
                update_check_now: false,
                register_after: 0.0,
                ever_connected: false,
                restart: false,
                server_name: None,
                trust: None,
                max_video_seconds: max_video,
                video_max_bytes: 80 * 1024 * 1024,
                lease_seconds: 180.0,
                lease_after: 0.0,
                token_changed: true,
                auth_failed: false,
                tools: None,
            }),
        })
    }

    pub fn quitting(&self) -> bool {
        self.quit.load(Ordering::Relaxed) || platform::stop_requested()
    }

    /// In-flight downloads stop when this turns true (Off, paused, quitting).
    pub fn cancelled(&self) -> bool {
        if self.quitting() {
            return true;
        }
        let st = self.st.lock().unwrap();
        matches!(st.phase, Phase::Off | Phase::Paused(_) | Phase::TokenRejected(_))
    }

    pub fn update_config(&self, f: impl FnOnce(&mut Config)) {
        let mut cfg = self.cfg.lock().unwrap();
        f(&mut cfg);
        if let Err(e) = cfg.save(&self.paths.config_file()) {
            crate::log!("could not save settings: {e}");
        }
    }

    pub fn token_changed(&self) {
        let mut st = self.st.lock().unwrap();
        st.token_changed = true;
        if matches!(st.phase, Phase::TokenRejected(_) | Phase::NoToken) {
            st.phase = Phase::Starting;
        }
    }

    pub fn token_rejected(&self, code: u16) {
        let mut st = self.st.lock().unwrap();
        st.phase = Phase::TokenRejected(format!("HTTP {code}"));
        st.auth_failed = true;
    }

    /// "Running · Max · 7 threads", "Paused: game running", ...
    pub fn status_text(&self) -> String {
        if self.quit.load(Ordering::Relaxed) {
            return "Stopped".into();
        }
        let st = self.st.lock().unwrap();
        match &st.phase {
            Phase::Starting => "Starting".into(),
            Phase::Setup(s) => format!("Setting up: {s}"),
            Phase::NoToken => "Connecting to the site".into(),
            Phase::TokenRejected(s) => format!("Token rejected ({s})"),
            Phase::Offline(s) => format!("Can't reach the site: {s}"),
            Phase::Off => "Off".into(),
            Phase::Paused(r) => format!("Paused: {r}"),
            Phase::Updating(tag) => format!("Updating to {tag}"),
            Phase::Cooldown => {
                let mins = ((st.cooldown_until - crate::now()) / 60.0).ceil().max(1.0);
                format!("YouTube cooldown {mins} min")
            }
            Phase::Running => format!(
                "Running · {} · {} thread{}{}",
                st.policy.label,
                st.threads,
                if st.threads == 1 { "" } else { "s" },
                if self.dry_run { " · dry run" } else { "" }
            ),
        }
    }

    pub fn today_text(&self) -> String {
        let st = self.st.lock().unwrap();
        let t = &st.today;
        let avg = if t.done > 0 { t.secs / t.done as f64 } else { 0.0 };
        format!("Today: {} compared, {} same, {:.1} s avg", t.done, t.same, avg)
    }

    pub fn now_text(&self) -> String {
        let st = self.st.lock().unwrap();
        format!("Now: {} job{}", st.busy, if st.busy == 1 { "" } else { "s" })
    }

    pub fn status_json(&self) -> serde_json::Value {
        let text = self.status_text();
        let st = self.st.lock().unwrap();
        let cfg = self.cfg.lock().unwrap();
        serde_json::json!({
            "version": crate::VERSION,
            "pid": std::process::id(),
            "at": crate::now(),
            "status": text,
            "mode": cfg.mode.key(),
            "policy": st.policy.label,
            "threads": st.threads,
            "busy": st.busy,
            "queued": st.queue.len(),
            "cooldown_until": st.cooldown_until,
            "today": st.today,
            "all": st.all,
            "last": st.last,
            "update": st.update_text,
            "server": cfg.server,
            "worker_name": st.server_name,
            "trust": st.trust,
            "dry_run": self.dry_run,
        })
    }

    pub fn save_status(&self) {
        let path = self.paths.status_file();
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, self.status_json().to_string()).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    /// One line for logs and the --foreground-status console.
    pub fn status_line(&self) -> String {
        format!("[mkworker] {} | {} | {}", self.status_text(), self.today_text(), self.now_text())
    }

    pub fn record(&self, job: &Job, verdict: &str, secs: f64) {
        let mut st = self.st.lock().unwrap();
        let date = platform::local_date();
        if st.today.date != date {
            st.today = Today { date, ..Default::default() };
        }
        st.today.add(verdict, secs);
        st.all.add(verdict, secs);
        st.last = Some(format!("{}: {}", job.video_id, verdict));
    }

    /// Where yt-dlp's cookie comes from now, for the panel.
    pub fn youtube_signin(&self) -> &'static str {
        if std::env::var_os("MKW_COOKIES").is_some_and(|p| std::path::Path::new(&p).is_file()) {
            "signed in (cookie file)"
        } else if self.site_cookie.lock().unwrap().is_some() {
            "signed in (site cookie)"
        } else {
            "not signed in"
        }
    }

    /// Acts on a cookie_version from /hello or /lease: fetch a new cookie
    /// when it changed, drop ours when there is none (or we aren't trusted).
    pub fn cookie_version_seen(&self, api: &Api, seen: Option<Option<String>>) {
        let Some(seen) = seen else { return };
        let held = self.site_cookie.lock().unwrap().as_ref().map(|c| c.version.clone());
        match seen {
            None => {
                if held.is_some() {
                    crate::log!("site cookie withdrawn");
                    *self.site_cookie.lock().unwrap() = None;
                }
            }
            Some(v) if held.as_deref() == Some(v.as_str()) => {}
            Some(_) => match crate::cookie::fetch(api) {
                Ok(crate::cookie::Fetched::Cookie(c)) => {
                    crate::log!("site cookie received (version {})", c.version);
                    *self.site_cookie.lock().unwrap() = Some(Arc::new(c));
                }
                Ok(crate::cookie::Fetched::None) => {
                    *self.site_cookie.lock().unwrap() = None;
                }
                Err(e) => crate::log!("site cookie not fetched: {e}"),
            },
        }
    }

    /// YouTube asked this machine to sign in while it had no cookie: ask
    /// the site again soon (at most every 10 minutes).
    pub fn bot_check_without_cookie(&self) {
        if self.site_cookie.lock().unwrap().is_some() || std::env::var_os("MKW_COOKIES").is_some() {
            return;
        }
        let mut next = self.cookie_recheck.lock().unwrap();
        if next.is_none() {
            *next = Some(crate::now());
        }
    }

    /// Waits for a free yt-dlp place (the other threads keep comparing
    /// meanwhile); None when the work was cancelled while waiting.
    pub fn youtube_slot(&self) -> Option<YoutubeSlot<'_>> {
        let mut running = self.youtube_running.lock().unwrap();
        loop {
            let limit = self.cfg.lock().unwrap().youtube_at_once;
            if limit == 0 || *running < limit {
                *running += 1;
                return Some(YoutubeSlot(self));
            }
            if self.cancelled() {
                return None;
            }
            running = self.youtube_free.wait_timeout(running, Duration::from_millis(500)).unwrap().0;
        }
    }

    pub fn add_download(&self, bytes: u64) {
        let mut st = self.st.lock().unwrap();
        st.today.bytes += bytes;
        st.all.bytes += bytes;
    }

    /// The popup's data: everything it shows, refreshed every second.
    pub fn popup_json(&self, startup: bool) -> serde_json::Value {
        let status = self.status_text();
        let st = self.st.lock().unwrap();
        let cfg = self.cfg.lock().unwrap();
        let trust = st.trust.as_deref().map(|t| {
            let mut c = t.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        });
        let running = match &trust {
            Some(t) => format!("Running \u{b7} {t}"),
            None => "Running".into(),
        };
        let pill = match &st.phase {
            Phase::Running => (running.as_str(), "running"),
            Phase::Paused(_) | Phase::Cooldown => ("Paused", "paused"),
            Phase::Updating(_) => ("Updating", "busy"),
            Phase::Off => ("Off", "off"),
            // Not connected yet: it registers by itself, nothing is wrong.
            Phase::NoToken => ("Connecting\u{2026}", "paused"),
            Phase::TokenRejected(_) => ("Token problem", "error"),
            Phase::Offline(_) => ("Offline", "error"),
            Phase::Starting | Phase::Setup(_) => ("Starting", "busy"),
        };
        let left = st.cooldown_until - crate::now();
        let cooldown = if left > 0.0 { format!("{} min", (left / 60.0).ceil()) } else { "none".into() };
        let tiles = |t: &Today| serde_json::json!({
            "compared": t.done,
            "same": t.same,
            "avg": if t.done > 0 { format!("{:.1} s", t.secs / t.done as f64) } else { "-".into() },
            "mb": format!("{:.0}", t.bytes as f64 / 1_048_576.0),
        });
        serde_json::json!({
            "pill": pill.0, "kind": pill.1, "status": status,
            "today": tiles(&st.today), "all": tiles(&st.all),
            "threads": st.threads, "busy": st.busy, "cooldown": cooldown,
            "mode": cfg.mode.key(), "running": cfg.running,
            "pause_fullscreen": cfg.pause_fullscreen, "pause_battery": cfg.pause_battery,
            "startup": startup,
            "connected": st.server_name.is_some(), "name": st.server_name, "trust": trust,
            "update": st.update_text, "version": env!("CARGO_PKG_VERSION"),
            "youtube": self.youtube_signin(),
            "server": cfg.server,
        })
    }

    /// YouTube refused this machine: back off 60 s, doubling, up to 1 h.
    pub fn youtube_refused(&self) -> f64 {
        let mut st = self.st.lock().unwrap();
        st.cooldown_streak += 1;
        let wait = (60.0 * 2f64.powi(st.cooldown_streak as i32 - 1)).min(3600.0);
        st.cooldown_until = crate::now() + wait;
        if st.policy.heavy {
            st.half_until = crate::now() + wait + HALF_AFTER_COOLDOWN;
        }
        wait
    }

    /// Wait at least `secs` before leasing again (a job was just handed back).
    pub fn back_off(&self, secs: f64) {
        let mut st = self.st.lock().unwrap();
        st.lease_after = st.lease_after.max(crate::now() + secs);
    }

    pub fn youtube_ok(&self) {
        self.st.lock().unwrap().cooldown_streak = 0;
    }
}

fn load_totals(paths: &Paths) -> (Today, Today) {
    let v: serde_json::Value = std::fs::read_to_string(paths.status_file())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let get = |k: &str| serde_json::from_value(v[k].clone()).unwrap_or_default();
    (get("today"), get("all"))
}

/// Machine load sampler for Light's "PC busy" pause and Linux's Auto.
struct Load {
    last: Option<(u64, u64, f64, Instant)>,
    others: f64,
}

impl Load {
    /// Share of all CPU time used by other programs since the last sample.
    fn sample(&mut self) -> f64 {
        let now = Instant::now();
        let cpu = platform::family_cpu_seconds();
        if let Some((busy, total)) = platform::system_ticks() {
            if let Some((b0, t0, c0, i0)) = self.last.filter(|l| busy >= l.0 && total >= l.1) {
                let dt = (total - t0) as f64;
                let wall = now.duration_since(i0).as_secs_f64() * config::cores() as f64;
                if dt > 0.0 && wall > 0.0 {
                    let all = (busy - b0) as f64 / dt;
                    let ours = ((cpu - c0) / wall).clamp(0.0, 1.0);
                    self.others = (all - ours).clamp(0.0, 1.0);
                }
            }
            self.last = Some((busy, total, cpu, now));
        }
        self.others
    }
}

pub struct Engine {
    shared: Arc<Shared>,
}

impl Engine {
    pub fn new(shared: Arc<Shared>) -> Engine {
        Engine { shared }
    }

    /// Runs until `shared.quit` (or a stop signal / stop file). Blocks.
    pub fn run(&self, status_every: Option<Duration>) {
        let sh = &self.shared;
        let cores = config::cores();
        // Leftovers from a run that was killed.
        let _ = std::fs::remove_dir_all(&sh.paths.tmp_dir);
        let _ = std::fs::create_dir_all(&sh.paths.tmp_dir);
        // Worker threads: as many as any mode can ask for; idle ones sleep.
        let pool = cores.max(sh.threads_override.unwrap_or(0)).max(1);
        let mut handles = Vec::new();
        for i in 0..pool {
            let s = sh.clone();
            handles.push(std::thread::spawn(move || worker_loop(i, s)));
        }

        let mut api: Option<Api> = None;
        let mut next_lease = 0.0f64;
        let mut next_hello = 0.0f64;
        let mut next_sense = 0.0f64;
        let mut next_save = 0.0f64;
        let mut next_line = crate::now() + status_every.map(|d| d.as_secs_f64()).unwrap_or(60.0);
        let mut load = Load { last: None, others: 0.0 };
        let mut sensed = (false, false, false, false); // fullscreen, battery, free, busy
        let mut priority_idle: Option<bool> = None;
        let mut last_phase = Phase::Starting;
        let mut pending_cookie: Option<Option<String>> = None;
        // The first check waits a minute so a fresh start settles first.
        let mut next_update = crate::now() + 60.0;
        crate::update::clean_old();
        sh.st.lock().unwrap().update_text = format!("Up to date (v{})", env!("CARGO_PKG_VERSION"));

        while !sh.quitting() {
            let now = crate::now();
            if sh.paths.stop_file().exists() {
                let _ = std::fs::remove_file(sh.paths.stop_file());
                crate::log!("stop requested");
                sh.quit.store(true, Ordering::Relaxed);
                break;
            }

            // Tools first (downloaded once).
            if sh.st.lock().unwrap().tools.is_none() {
                let cfg = sh.cfg.lock().unwrap().clone();
                let s2 = sh.clone();
                match crate::sidecars::ensure(&cfg, &sh.paths, &move |m| {
                    s2.st.lock().unwrap().phase = Phase::Setup(m);
                }) {
                    Ok(t) => {
                        crate::log!("tools: yt-dlp {} | ffmpeg {} | deno {}", t.ytdlp.display(), t.ffmpeg.display(),
                            t.deno.as_ref().map(|d| d.display().to_string()).unwrap_or_else(|| "none".into()));
                        let mut st = sh.st.lock().unwrap();
                        st.tools = Some(t);
                        st.phase = Phase::Starting;
                    }
                    Err(e) => {
                        crate::log!("setup failed: {e}");
                        sh.st.lock().unwrap().phase = Phase::Setup(format!("failed: {e} (retrying)"));
                        wait(sh, 60.0);
                        continue;
                    }
                }
            }

            // Connect (again) when the token changed.
            let changed = std::mem::take(&mut sh.st.lock().unwrap().token_changed);
            if changed {
                api = None;
                next_hello = 0.0;
            } else if std::mem::take(&mut sh.st.lock().unwrap().auth_failed) && api.is_some() {
                api = None;
                next_hello = now + 600.0;
            }
            if api.is_none() && now >= next_hello {
                match sh.paths.read_token() {
                    None => sh.st.lock().unwrap().phase = Phase::NoToken,
                    Some(token) => {
                        let server = sh.cfg.lock().unwrap().server.clone();
                        let a = Api::new(&server, &token);
                        let threads = sh.st.lock().unwrap().threads;
                        match a.hello(threads, crate::core::ALGO_VERSION) {
                            Ok(h) => {
                                crate::log!("connected to {server} as '{}' ({}){}", h.name, h.trust,
                                    if sh.dry_run { ", dry run: jobs are leased and handed back" } else { "" });
                                if !h.algo.is_empty() {
                                    crate::log!("site algorithm {}, this worker {}", h.algo, crate::core::ALGO_VERSION);
                                }
                                let mut st = sh.st.lock().unwrap();
                                let cfg_max = sh.cfg.lock().unwrap().max_video_seconds;
                                st.max_video_seconds = h.max_video_seconds.map(|m| m.min(cfg_max)).unwrap_or(cfg_max);
                                st.video_max_bytes = h.video_max_bytes.unwrap_or(80 * 1024 * 1024);
                                st.lease_seconds = h.lease_seconds.filter(|s| *s >= 30.0).unwrap_or(180.0);
                                pending_cookie = h.cookie_version.clone();
                                st.server_name = Some(h.name);
                                st.ever_connected = true;
                                st.trust = Some(h.trust);
                                st.phase = Phase::Starting;
                                api = Some(a);
                            }
                            Err(ApiError::Auth(c)) => {
                                crate::log!("token refused by the site (HTTP {c}); set a new one");
                                sh.st.lock().unwrap().phase = Phase::TokenRejected(format!("HTTP {c}"));
                                next_hello = now + 600.0;
                            }
                            Err(e) => {
                                crate::log!("hello failed: {e}");
                                sh.st.lock().unwrap().phase = Phase::Offline(e.to_string());
                                next_hello = now + 60.0;
                            }
                        }
                    }
                }
            }

            // Updates: never without a token.
            {
                let auto = sh.cfg.lock().unwrap().auto_update;
                let mut st = sh.st.lock().unwrap();
                let blocked = sh.paths.read_token().is_none() || sh.dry_run;
                let asked = std::mem::take(&mut st.update_check_now);
                if asked {
                    next_update = now;
                    if !st.update_checking && st.update_ready.is_none() {
                        st.update_text = "Checking for updates\u{2026}".into();
                    }
                }
                if !auto && !asked && st.update_ready.is_none() && !st.update_checking {
                    st.update_text = format!("Updates off (v{})", env!("CARGO_PKG_VERSION"));
                } else if (auto || asked) && now >= next_update && !blocked && !st.update_checking && st.update_ready.is_none()
                    && crate::update::asset().is_some()
                {
                    next_update = now + crate::update::CHECK_EVERY;
                    st.update_checking = true;
                    let s2 = sh.clone();
                    std::thread::spawn(move || check_for_update(&s2));
                }
            }
            let ready = {
                let st = sh.st.lock().unwrap();
                st.update_ready.clone()
            };
            if let Some((file, tag)) = ready {
                sh.st.lock().unwrap().phase = Phase::Updating(tag.clone());
                if let Some(a) = &api {
                    let back: Vec<Job> = std::mem::take(&mut sh.st.lock().unwrap().queue);
                    a.release(&back);
                }
                if sh.st.lock().unwrap().busy == 0 {
                    match std::env::current_exe().map_err(|e| e.to_string())
                        .and_then(|exe| crate::update::swap(&exe, &file))
                    {
                        Ok(()) => {
                            crate::log!("installed {tag}; restarting into it");
                            sh.st.lock().unwrap().restart = true;
                            break;
                        }
                        Err(e) => {
                            crate::log!("update to {tag} failed: {e}");
                            let _ = std::fs::remove_file(&file);
                            let mut st = sh.st.lock().unwrap();
                            st.update_ready = None;
                            st.update_text = format!("Update failed (v{})", env!("CARGO_PKG_VERSION"));
                            st.phase = Phase::Starting;
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }

            // No token, or one the site stopped taking: register a new worker.
            // 429 (too many from this address today) waits 30 min; a token
            // that had worked is replaced at most once an hour.
            if api.is_none() {
                let go = {
                    let st = sh.st.lock().unwrap();
                    let need = match st.phase {
                        Phase::NoToken => true,
                        Phase::TokenRejected(_) => true,
                        _ => false,
                    };
                    // A token from MKW_TOKEN can't be replaced by registering.
                    let from_env = std::env::var("MKW_TOKEN").is_ok_and(|t| t.trim().starts_with("mkw_"));
                    need && !from_env && now >= st.register_after
                };
                if go {
                    let rejected = matches!(sh.st.lock().unwrap().phase, Phase::TokenRejected(_));
                    let server = sh.cfg.lock().unwrap().server.clone();
                    crate::log!("registering this machine with {server}");
                    match crate::pair::register(&server, &crate::pair::hostname()) {
                        Ok(token) => match sh.paths.write_token(&token) {
                            Ok(()) => {
                                crate::log!("registered: token saved");
                                sh.st.lock().unwrap().register_after = now + if rejected { 3600.0 } else { 300.0 };
                                sh.token_changed();
                            }
                            Err(e) => {
                                crate::log!("registered, but the token could not be saved: {e}");
                                sh.st.lock().unwrap().register_after = now + 300.0;
                            }
                        },
                        Err(crate::pair::RegisterError::Limited) => {
                            crate::log!("the site limits new workers from this address today; retrying in 30 min");
                            sh.st.lock().unwrap().register_after = now + 1800.0;
                        }
                        Err(crate::pair::RegisterError::Other(e)) => {
                            crate::log!("registering failed: {e}; retrying in 5 min");
                            sh.st.lock().unwrap().register_after = now + 300.0;
                        }
                    }
                }
            }

            // Cookie news from /hello (fetched outside the state lock).
            if let (Some(seen), Some(a)) = (pending_cookie.take(), &api) {
                sh.cookie_version_seen(a, Some(seen));
            }
            let recheck = {
                let mut next = sh.cookie_recheck.lock().unwrap();
                match *next {
                    Some(t) if now >= t && api.is_some() => {
                        *next = None;
                        true
                    }
                    _ => false,
                }
            };
            if recheck {
                if let Some(a) = &api {
                    let threads = sh.st.lock().unwrap().threads;
                    if let Ok(h) = a.hello(threads, crate::core::ALGO_VERSION) {
                        sh.cookie_version_seen(a, h.cookie_version);
                    }
                }
            }

            // Sensors and policy.
            if now >= next_sense {
                next_sense = now + 5.0;
                let others = load.sample();
                let free = match platform::idle_seconds() {
                    Some(idle) => idle >= AUTO_IDLE_SECONDS,
                    None => match platform::load_average() {
                        // Linux: what everything else puts on the machine.
                        Some(l) => {
                            let ours = sh.st.lock().unwrap().busy as f64;
                            (l - ours).max(0.0) < (cores as f64 * 0.25).max(0.75)
                        }
                        None => others < 0.25,
                    },
                };
                // Hysteresis on "busy": on above 60 %, off below 35 %.
                let busy = if sensed.3 { others > 0.35 } else { others > 0.60 };
                sensed = (platform::fullscreen(), platform::on_battery(), free, busy);
            }
            let cfg = sh.cfg.lock().unwrap().clone();
            let mut policy = config::policy(cfg.mode, cores, sensed.2);
            let mut threads = sh.threads_override.unwrap_or(if cfg.threads > 0 { cfg.threads } else { policy.threads });
            {
                let st = sh.st.lock().unwrap();
                if policy.heavy && now < st.half_until {
                    threads = (threads / 2).max(1);
                }
            }
            threads = threads.clamp(1, pool);
            // On a shared host /proc/stat shows everyone's load: only the tray
            // (a desktop) pauses for a busy PC.
            if !sh.interactive || cfg.mode == Mode::Max || (cfg.mode == Mode::Auto && sensed.2) {
                policy.pause_busy = false;
            }
            if priority_idle != Some(policy.idle_priority) {
                platform::set_priority(policy.idle_priority);
                priority_idle = Some(policy.idle_priority);
            }

            // Phase.
            let pause = if cfg.pause_fullscreen && sensed.0 {
                Some("game or fullscreen app")
            } else if cfg.pause_battery && sensed.1 {
                Some("on battery")
            } else if policy.pause_busy && sensed.3 {
                Some("PC busy")
            } else {
                None
            };
            {
                let mut st = sh.st.lock().unwrap();
                st.policy = policy;
                st.threads = threads;
                // Without a connection the phase says why (no token, offline...).
                if api.is_some() {
                    st.phase = if sh.interactive && !cfg.running {
                        Phase::Off
                    } else if let Some(r) = pause {
                        Phase::Paused(r.into())
                    } else if now < st.cooldown_until {
                        Phase::Cooldown
                    } else {
                        Phase::Running
                    };
                }
                if st.phase != last_phase {
                    crate::log!("{}", match &st.phase {
                        Phase::Running => format!("running: {} ({} thread{})", policy.label, threads, if threads == 1 { "" } else { "s" }),
                        p => format!("{p:?}"),
                    });
                    last_phase = st.phase.clone();
                }
            }

            // Lease or hand back.
            if let Some(a) = &api {
                let (phase, want, lease_after) = {
                    let st = sh.st.lock().unwrap();
                    // Live jobs (any worker past shadow) wait on the site: no prefetch.
                    let prefetch = if st.trust.as_deref() == Some("shadow") { policy.prefetch } else { 0 };
                    let want = (threads + prefetch).saturating_sub(st.busy + st.resting + st.queue.len());
                    (st.phase.clone(), want.min(MAX_BATCH), st.lease_after)
                };
                if phase != Phase::Running {
                    let back: Vec<Job> = std::mem::take(&mut sh.st.lock().unwrap().queue);
                    if !back.is_empty() {
                        a.release(&back);
                        next_lease = next_lease.max(now + 30.0);
                    }
                } else if want > 0 && now >= next_lease && now >= lease_after {
                    match a.lease(want) {
                        Ok((jobs, poll, cookie_version)) => {
                            sh.cookie_version_seen(a, cookie_version);
                            if jobs.is_empty() {
                                next_lease = now + poll.max(1.0);
                            } else {
                                crate::log!("leased {} job{}", jobs.len(), if jobs.len() == 1 { "" } else { "s" });
                                sh.st.lock().unwrap().queue.extend(jobs);
                                next_lease = now + 1.0;
                            }
                        }
                        Err(ApiError::Auth(c)) => {
                            crate::log!("token refused by the site (HTTP {c})");
                            sh.st.lock().unwrap().phase = Phase::TokenRejected(format!("HTTP {c}"));
                            api = None;
                            next_hello = now + 600.0;
                        }
                        Err(ApiError::Slow(after)) => next_lease = now + after.unwrap_or(60.0).clamp(5.0, 600.0),
                        Err(e) => {
                            crate::log!("lease failed: {e}");
                            next_lease = now + 30.0;
                        }
                    }
                }
            }

            if now >= next_save {
                next_save = now + 10.0;
                sh.save_status();
            }
            if status_every.is_some() && now >= next_line {
                next_line = now + status_every.unwrap().as_secs_f64();
                crate::logging::status(&sh.status_line());
            }
            std::thread::sleep(Duration::from_millis(500));
        }

        // Shutting down: hand back what hasn't started; running jobs see
        // `cancelled()` and hand theirs back too.
        sh.quit.store(true, Ordering::Relaxed);
        crate::log!("stopping: handing unfinished jobs back");
        if let Some(a) = &api {
            let back: Vec<Job> = std::mem::take(&mut sh.st.lock().unwrap().queue);
            a.release(&back);
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        while sh.st.lock().unwrap().busy > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
        }
        sh.save_status();
        for h in handles {
            if h.is_finished() {
                let _ = h.join();
            }
        }
    }
}

fn check_for_update(sh: &Shared) {
    let result = crate::update::check().and_then(|found| match found {
        None => Ok(None),
        Some(rel) => {
            crate::log!("new release {}: downloading", rel.tag);
            sh.st.lock().unwrap().update_text = format!("Downloading {}", rel.tag);
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            crate::update::download(&rel, &exe).map(|file| Some((file, rel.tag)))
        }
    });
    let mut st = sh.st.lock().unwrap();
    st.update_checking = false;
    match result {
        Ok(Some((file, tag))) => {
            st.update_text = format!("Updating to {tag}");
            st.update_ready = Some((file, tag));
        }
        Ok(None) => st.update_text = format!("Up to date (v{})", env!("CARGO_PKG_VERSION")),
        Err(e) => {
            crate::log!("{e}");
            st.update_text = format!("Update check failed (v{})", env!("CARGO_PKG_VERSION"));
        }
    }
}

fn wait(sh: &Shared, secs: f64) {
    let end = Instant::now() + Duration::from_secs_f64(secs);
    while Instant::now() < end && !sh.quitting() && !sh.paths.stop_file().exists() {
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn worker_loop(index: usize, sh: Arc<Shared>) {
    let mut api: Option<(String, Api)> = None;
    while !sh.quitting() {
        let picked = {
            let mut st = sh.st.lock().unwrap();
            if st.phase == Phase::Running && index < st.threads && !st.queue.is_empty() {
                let job = st.queue.remove(0);
                st.busy += 1;
                Some((job, st.tools.clone(), st.max_video_seconds, st.video_max_bytes, st.lease_seconds))
            } else {
                None
            }
        };
        let Some((job, tools, max_video, max_bytes, lease_seconds)) = picked else {
            std::thread::sleep(Duration::from_millis(400));
            continue;
        };
        // An Api per thread, rebuilt when the token or server changes.
        let token = sh.paths.read_token().unwrap_or_default();
        let server = sh.cfg.lock().unwrap().server.clone();
        let key = format!("{server}\n{token}");
        if api.as_ref().map(|(k, _)| k != &key).unwrap_or(true) {
            api = Some((key.clone(), Api::new(&server, &token)));
        }
        let a = &api.as_ref().unwrap().1;
        // This thread's CPU plus its helpers': other threads' work is theirs.
        let cpu0 = platform::thread_cpu_seconds();
        let mut helpers = 0.0;
        if crate::job::lease_left(&job, lease_seconds) < MIN_LEASE_LEFT || tools.is_none() {
            a.release(std::slice::from_ref(&job));
            sh.back_off(10.0);
        } else if job.is_search() {
            helpers = crate::job::run_search(&sh, a, &job, tools.as_ref().unwrap());
        } else {
            helpers = crate::job::run(&sh, a, &job, tools.as_ref().unwrap(), max_video, max_bytes, lease_seconds);
        }
        let used = platform::thread_cpu_seconds() - cpu0 + helpers;
        let duty = sh.cfg.lock().unwrap().duty;
        let rest = if duty > 0.0 && duty < 1.0 { used.max(0.0) * (1.0 - duty) / duty } else { 0.0 };
        {
            let mut st = sh.st.lock().unwrap();
            st.busy = st.busy.saturating_sub(1);
            st.resting += (rest > 0.0) as usize;
        }
        if rest > 0.0 {
            let end = Instant::now() + Duration::from_secs_f64(if rest.is_finite() { rest.clamp(0.0, 3600.0) } else { 0.0 });
            while Instant::now() < end && !sh.quitting() {
                std::thread::sleep(Duration::from_millis(250));
            }
            let mut st = sh.st.lock().unwrap();
            st.resting = st.resting.saturating_sub(1);
        }
    }
}
