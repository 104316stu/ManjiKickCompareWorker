//! mkworker-tray: the Windows tray app (no console window).
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
fn main() {
    mkworker::tray::main();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("mkworker-tray is the Windows tray app; on Linux use `mkworker run`.");
    std::process::exit(2);
}
