//! One log file (rotated to .1 past 5 MB), optionally echoed to stdout.

use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

static FILE: Mutex<Option<std::fs::File>> = Mutex::new(None);
static ECHO: AtomicBool = AtomicBool::new(false);

pub fn init(path: &Path, echo: bool) {
    ECHO.store(echo, Ordering::Relaxed);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::metadata(path).map(|m| m.len() > 5 * 1024 * 1024).unwrap_or(false) {
        let mut old = path.as_os_str().to_owned();
        old.push(".1");
        let _ = std::fs::rename(path, old);
    }
    let file = std::fs::OpenOptions::new().create(true).append(true).open(path).ok();
    *FILE.lock().unwrap() = file;
}

pub fn line(message: &str) {
    let text = format!("{} {}\n", crate::platform::local_stamp(), message);
    if ECHO.load(Ordering::Relaxed) {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(text.as_bytes());
        let _ = out.flush();
    }
    if let Ok(mut guard) = FILE.lock() {
        if let Some(f) = guard.as_mut() {
            let _ = f.write_all(text.as_bytes());
        }
    }
}

/// Printed to stdout only (the --foreground-status line); still logged.
pub fn status(message: &str) {
    if !ECHO.load(Ordering::Relaxed) {
        println!("{} {}", crate::platform::local_stamp(), message);
    }
    line(message);
}
