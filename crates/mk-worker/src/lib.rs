//! ManjiKick compare worker. Leases audio compares from the site's worker API
//! (/api/worker/v1), runs them on
//! this machine with mk-core and posts the results. Two faces share this
//! library: the `mkworker` CLI (Linux, and Windows for debugging) and the
//! `mkworker-tray` Windows tray app.

pub mod api;
pub mod config;
pub mod core;
pub mod engine;
pub mod job;
pub mod logging;
pub mod paths;
pub mod platform;
pub mod proc;
pub mod sidecars;
pub mod zipx;

#[cfg(windows)]
pub mod tray;

pub const VERSION: &str = concat!("rs-", env!("CARGO_PKG_VERSION"));
pub const DEFAULT_SERVER: &str = "https://audios.manjikick.com";

/// Seconds since the Unix epoch, as the server's `expires_at` is.
pub fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn os_name() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        std::env::consts::OS
    }
}

/// "ManjiKickWorker/rs-0.1.0 (windows; x86_64)". The site sits behind
/// Cloudflare, which turns away curl-like user agents.
pub fn user_agent() -> String {
    format!("ManjiKickWorker/{} ({}; {})", VERSION, os_name(), std::env::consts::ARCH)
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => { $crate::logging::line(&format!($($arg)*)) };
}
