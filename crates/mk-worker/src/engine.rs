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
use std::sync::{Arc, Mutex};
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
    Running,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Today {
    pub date: String,
    pub done: u64,
    pub same: u64,
    pub errors: u64,
    pub secs: f64,
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
    pub last: Option<String>,
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
}

impl Shared {
    pub fn new(paths: Paths, cfg: Config, threads_override: Option<usize>, dry_run: bool, interactive: bool) -> Arc<Shared> {
        let cores = config::cores();
        let policy = config::policy(cfg.mode, cores, false);
        let mut today = load_today(&paths);
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
                last: None,
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
            Phase::NoToken => "No token: set one from the clipboard".into(),
            Phase::TokenRejected(s) => format!("Token rejected ({s})"),
            Phase::Offline(s) => format!("Can't reach the site: {s}"),
            Phase::Off => "Off".into(),
            Phase::Paused(r) => format!("Paused: {r}"),
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
            "last": st.last,
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
        st.today.done += 1;
        st.today.same += (verdict == "same") as u64;
        st.today.errors += (verdict == "unknown") as u64;
        st.today.secs += secs;
        st.last = Some(format!("{}: {}", job.video_id, verdict));
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

fn load_today(paths: &Paths) -> Today {
    std::fs::read_to_string(paths.status_file())
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| serde_json::from_value(v["today"].clone()).ok())
        .unwrap_or_default()
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
            if let Some((b0, t0, c0, i0)) = self.last {
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
                                st.server_name = Some(h.name);
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

            // The CLI stops on a rejected token (the tray waits for a new one).
            if !sh.interactive && matches!(sh.st.lock().unwrap().phase, Phase::TokenRejected(_)) {
                break;
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
                        Ok((jobs, poll)) => {
                            if jobs.is_empty() {
                                next_lease = now + poll.max(3.0);
                            } else {
                                crate::log!("leased {} job{}", jobs.len(), if jobs.len() == 1 { "" } else { "s" });
                                sh.st.lock().unwrap().queue.extend(jobs);
                                next_lease = now + 3.0;
                            }
                        }
                        Err(ApiError::Auth(c)) => {
                            crate::log!("token refused by the site (HTTP {c})");
                            sh.st.lock().unwrap().phase = Phase::TokenRejected(format!("HTTP {c}"));
                            api = None;
                            next_hello = now + 600.0;
                        }
                        Err(ApiError::Slow) => next_lease = now + 60.0,
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
        } else {
            helpers = crate::job::run(&sh, a, &job, tools.as_ref().unwrap(), max_video, max_bytes, lease_seconds);
        }
        let used = platform::thread_cpu_seconds() - cpu0 + helpers;
        let duty = sh.cfg.lock().unwrap().duty;
        let rest = if duty < 1.0 { used * (1.0 - duty) / duty } else { 0.0 };
        {
            let mut st = sh.st.lock().unwrap();
            st.busy -= 1;
            st.resting += (rest > 0.0) as usize;
        }
        if rest > 0.0 {
            let end = Instant::now() + Duration::from_secs_f64(rest.min(3600.0));
            while Instant::now() < end && !sh.quitting() {
                std::thread::sleep(Duration::from_millis(250));
            }
            sh.st.lock().unwrap().resting -= 1;
        }
    }
}
