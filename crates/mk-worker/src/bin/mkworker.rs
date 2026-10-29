//! mkworker: the command-line worker.
//!
//!   mkworker run [--mode light|balanced|max|auto] [--threads N] [--duty F]
//!                [--max-video-seconds N] [--daemon] [--foreground-status] [--dry-run]
//!   mkworker status [--json]
//!   mkworker stop
//!   mkworker token set        (reads the token from stdin)

use mkworker::config::{Config, Mode};
use mkworker::engine::{Engine, Shared};
use mkworker::paths::Paths;
use mkworker::platform;
use std::io::Read;
use std::time::Duration;

const USAGE: &str = "\
ManjiKick compare worker

usage:
  mkworker run [options]     work until stopped
      --mode MODE            light | balanced | max | auto (default: from config, auto)
      --threads N            fixed number of compares at once (overrides the mode)
      --duty F               use at most this share of CPU time, e.g. 0.2
      --max-video-seconds N  skip longer videos (default 1800)
      --daemon               detach and run in the background (Linux)
      --foreground-status    print only one status line per minute
      --dry-run              lease jobs and hand them back, post nothing
  mkworker status [--json]   what the running worker is doing
  mkworker stop              stop the running worker
  mkworker token set         save the token read from stdin
  mkworker test-download ID  check yt-dlp + ffmpeg here with one YouTube video
  mkworker version";

/// The pid file this process wrote, removed again on any exit.
static OWN_PID_FILE: std::sync::Mutex<Option<std::path::PathBuf>> = std::sync::Mutex::new(None);

fn exit(code: i32) -> ! {
    if let Some(p) = OWN_PID_FILE.lock().ok().and_then(|mut g| g.take()) {
        let _ = std::fs::remove_file(p);
    }
    std::process::exit(code)
}

fn die(msg: &str) -> ! {
    eprintln!("mkworker: {msg}");
    exit(2)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let paths = Paths::get();
    match args.first().map(String::as_str) {
        Some("run") => run(&paths, &args[1..]),
        Some("status") => status(&paths, args.iter().any(|a| a == "--json")),
        Some("stop") => stop(&paths),
        Some("test-download") => test_download(&paths, args.get(1)),
        Some("token") if args.get(1).map(String::as_str) == Some("set") => token_set(&paths),
        Some("version") | Some("--version") | Some("-V") => {
            println!("mkworker {} ({})", mkworker::VERSION, mkworker::user_agent())
        }
        Some("help") | Some("--help") | Some("-h") | None => println!("{USAGE}"),
        Some(other) => die(&format!("unknown command '{other}'\n\n{USAGE}")),
    }
}

fn token_set(paths: &Paths) {
    let mut text = String::new();
    let _ = std::io::stdin().read_to_string(&mut text);
    let token = text.trim();
    if !token.starts_with("mkw_") || token.len() < 12 || token.contains(char::is_whitespace) {
        die("that doesn't look like a worker token (mkw_...)");
    }
    match paths.write_token(token) {
        Ok(()) => println!("token saved to {}", paths.token_file().display()),
        Err(e) => die(&format!("could not save the token: {e}")),
    }
}

fn test_download(paths: &Paths, id: Option<&String>) {
    let id = id.filter(|v| v.len() == 11 && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .unwrap_or_else(|| die("usage: mkworker test-download <11-character YouTube video id>"));
    paths.create_all();
    mkworker::logging::init(&paths.log_file, true);
    platform::install_stop_handler();
    let cfg = Config::load(&paths.config_file());
    let tools = mkworker::sidecars::ensure(&cfg, paths, &|m| println!("{m}"))
        .unwrap_or_else(|e| die(&format!("setup failed: {e}")));
    match mkworker::job::probe(paths, &tools, id) {
        Ok(msg) => println!("ok: {msg}"),
        Err(e) => die(&e),
    }
}

fn status(paths: &Paths, as_json: bool) {
    let Ok(text) = std::fs::read_to_string(paths.status_file()) else {
        println!("no status yet (has the worker run here?)");
        std::process::exit(1);
    };
    if as_json {
        println!("{}", text.trim());
        return;
    }
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
    let age = mkworker::now() - v["at"].as_f64().unwrap_or(0.0);
    let today = &v["today"];
    let done = today["done"].as_u64().unwrap_or(0);
    let avg = if done > 0 { today["secs"].as_f64().unwrap_or(0.0) / done as f64 } else { 0.0 };
    let alive = age < 60.0;
    println!("{}{}", v["status"].as_str().unwrap_or("?"),
        if alive { String::new() } else { format!("  (not updated for {} s: not running?)", age as u64) });
    println!("today: {} compared, {} same, {} failed, {:.1} s avg",
        done, today["same"].as_u64().unwrap_or(0), today["errors"].as_u64().unwrap_or(0), avg);
    println!("now: {} jobs, {} queued | worker '{}' ({}) | {}",
        v["busy"].as_u64().unwrap_or(0), v["queued"].as_u64().unwrap_or(0),
        v["worker_name"].as_str().unwrap_or("-"), v["trust"].as_str().unwrap_or("-"),
        v["version"].as_str().unwrap_or("?"));
    println!("last: {}", v["last"].as_str().unwrap_or("-"));
}

fn stop(paths: &Paths) {
    let pid: Option<u32> = std::fs::read_to_string(paths.pid_file()).ok().and_then(|p| p.trim().parse().ok());
    #[cfg(unix)]
    if let Some(p) = pid {
        if !platform::process_alive(p as i32) || !platform::is_our_program(p as i32) {
            println!("no worker running (stale pid file removed)");
            let _ = std::fs::remove_file(paths.pid_file());
            return;
        }
    }
    let _ = std::fs::write(paths.stop_file(), "stop\n");
    #[cfg(unix)]
    if let Some(pid) = pid {
        platform::terminate(pid as i32);
    }
    let Some(pid) = pid else {
        println!("asked the worker to stop (no pid file, so it may not be running)");
        return;
    };
    for _ in 0..60 {
        if !paths.pid_file().exists() {
            println!("stopped (pid {pid})");
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    println!("asked pid {pid} to stop; it is still finishing up");
}


fn run(paths: &Paths, args: &[String]) {
    let mut cfg = Config::load(&paths.config_file());
    let mut threads = None;
    let (mut daemon, mut fg_status, mut dry_run) = (false, false, false);
    let mut i = 0;
    let value = |i: usize, name: &str| -> String {
        args.get(i + 1).cloned().unwrap_or_else(|| die(&format!("{name} needs a value")))
    };
    while i < args.len() {
        let a = args[i].as_str();
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (a, None),
        };
        let mut take = || -> String {
            if let Some(v) = inline.clone() {
                v
            } else {
                i += 1;
                value(i - 1, flag)
            }
        };
        match flag {
            "--mode" => {
                let v = take();
                cfg.mode = Mode::parse(&v).unwrap_or_else(|| die(&format!("unknown mode '{v}'")));
            }
            "--threads" => {
                let v = take();
                threads = Some(v.parse::<usize>().ok().filter(|n| (1..=256).contains(n))
                    .unwrap_or_else(|| die("--threads needs a number from 1 to 256")));
            }
            "--duty" => {
                let v = take();
                cfg.duty = v.parse::<f64>().ok().filter(|d| *d > 0.0 && *d <= 1.0)
                    .unwrap_or_else(|| die("--duty needs a number above 0 and at most 1"))
                    .max(0.05);
            }
            "--max-video-seconds" => {
                let v = take();
                cfg.max_video_seconds = v.parse().unwrap_or_else(|_| die("--max-video-seconds needs a number"));
            }
            "--daemon" => daemon = true,
            "--foreground-status" => fg_status = true,
            "--dry-run" => dry_run = true,
            _ => die(&format!("unknown option '{a}'\n\n{USAGE}")),
        }
        i += 1;
    }
    paths.create_all();

    #[cfg(unix)]
    if daemon {
        let exe = std::env::current_exe().unwrap_or_else(|e| die(&e.to_string()));
        let rest: Vec<&String> = args.iter().filter(|a| a.as_str() != "--daemon").collect();
        use std::os::unix::process::CommandExt;
        let child = std::process::Command::new(exe)
            .arg("run")
            .args(rest)
            .env("MKW_DAEMON", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap_or_else(|e| die(&format!("could not start in the background: {e}")));
        println!("started in the background (pid {}); log: {}", child.id(), paths.log_file.display());
        return;
    }
    #[cfg(not(unix))]
    if daemon {
        die("--daemon is for Linux; on Windows use mkworker-tray");
    }

    let background = std::env::var_os("MKW_DAEMON").is_some();
    mkworker::logging::init(&paths.log_file, !background && !fg_status);

    // One worker per user: a mutex on Windows, a locked pid file on Unix.
    let _instance = platform::single_instance().unwrap_or_else(|| die("another worker is already running"));
    #[cfg(unix)]
    let pid_lock = platform::lock_pid_file(&paths.pid_file())
        .unwrap_or_else(|| die("another worker is already running; `mkworker stop` first"));

    if paths.read_token().is_none() {
        let msg = format!("no worker token: run `mkworker token set` (or put it in {})", paths.token_file().display());
        mkworker::log!("{msg}");
        die(&msg);
    }

    // All checks passed: this is the worker now.
    let _ = std::fs::remove_file(paths.stop_file());
    #[cfg(unix)]
    {
        use std::io::{Seek, Write};
        let mut f = &pid_lock;
        let _ = f.set_len(0);
        let _ = f.seek(std::io::SeekFrom::Start(0));
        let _ = writeln!(f, "{}", std::process::id());
    }
    #[cfg(not(unix))]
    let _ = std::fs::write(paths.pid_file(), format!("{}\n", std::process::id()));
    *OWN_PID_FILE.lock().unwrap() = Some(paths.pid_file());
    platform::init_family();
    platform::install_stop_handler();
    platform::set_priority(true);
    mkworker::log!("mkworker {} starting (mode {}{})", mkworker::VERSION, cfg.mode.label(),
        threads.map(|t| format!(", {t} threads")).unwrap_or_default());
    let shared = Shared::new(paths.clone(), cfg, threads, dry_run, false);
    Engine::new(shared.clone()).run(Some(Duration::from_secs(60)));
    mkworker::log!("stopped");
    // A rejected token ends the CLI with an error so a supervisor notices.
    let rejected = matches!(shared.st.lock().unwrap().phase, mkworker::engine::Phase::TokenRejected(_));
    exit(if rejected { 3 } else { 0 });
}
