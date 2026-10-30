//! Windows: the tray app installs itself like a normal app.
//!
//!   %LOCALAPPDATA%\ManjiKickWorker\app\ManjiKick Worker.exe   the program
//!   Start menu\Programs\ManjiKick Worker.lnk                  its shortcut
//!   %LOCALAPPDATA%\ManjiKickWorker\                          data (config, token, bin\) as before
//!
//! A downloaded copy installs (or updates) the app and starts it; when the
//! installed one is the same or newer it just opens its panel.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use windows::core::{Interface, HSTRING};
use windows::Win32::System::Com::{CoCreateInstance, IPersistFile, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};

pub const APP_NAME: &str = "ManjiKick Worker";

fn local_base() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ManjiKickWorker")
}

pub fn app_dir() -> PathBuf {
    local_base().join("app")
}

pub fn installed_exe() -> PathBuf {
    app_dir().join(format!("{APP_NAME}.exe"))
}

pub fn shortcut() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Microsoft\\Windows\\Start Menu\\Programs")
        .join(format!("{APP_NAME}.lnk"))
}

fn same_file(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        std::fs::canonicalize(p)
            .unwrap_or_else(|_| p.to_path_buf())
            .to_string_lossy()
            .to_lowercase()
    };
    norm(a) == norm(b)
}

/// Whether this process is the installed app. Developer runs (MKW_NO_INSTALL,
/// or a separate MKW_HOME) count as installed so they never move themselves.
pub fn is_installed_copy() -> bool {
    if std::env::var_os("MKW_NO_INSTALL").is_some() || std::env::var_os("MKW_HOME").is_some() {
        return true;
    }
    std::env::current_exe().map(|e| same_file(&e, &installed_exe())).unwrap_or(true)
}

/// The version an exe reports with `--version`.
pub fn version_of(exe: &Path) -> Option<(u64, u64, u64)> {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--version");
    let out = crate::proc::quiet(&mut cmd).output().ok()?;
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .find_map(|w| crate::update::parse_version(w.trim_start_matches("rs-")))
}

/// Creates (or refreshes) the Start menu shortcut. COM must be initialised.
pub fn create_shortcut(exe: &Path) -> Result<(), String> {
    let lnk = shortcut();
    if let Some(dir) = lnk.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).map_err(|e| e.to_string())?;
        link.SetPath(&HSTRING::from(exe.as_os_str())).map_err(|e| e.to_string())?;
        if let Some(dir) = exe.parent() {
            link.SetWorkingDirectory(&HSTRING::from(dir.as_os_str())).map_err(|e| e.to_string())?;
        }
        link.SetIconLocation(&HSTRING::from(exe.as_os_str()), 0).map_err(|e| e.to_string())?;
        link.SetDescription(&HSTRING::from("Runs audio compares for audios.manjikick.com")).map_err(|e| e.to_string())?;
        let file: IPersistFile = link.cast().map_err(|e| e.to_string())?;
        file.Save(&HSTRING::from(lnk.as_os_str()), true).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Asks a running worker to stop and waits until it has (up to 30 s), then
/// ends any installed copy still left.
pub fn stop_running() {
    let paths = crate::paths::Paths::get();
    let _ = std::fs::write(paths.stop_file(), "stop\n");
    let until = Instant::now() + Duration::from_secs(30);
    while Instant::now() < until {
        if let Some(lock) = crate::platform::single_instance() {
            drop(lock);
            let _ = std::fs::remove_file(paths.stop_file());
            return;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    let mut cmd = std::process::Command::new("taskkill");
    cmd.args(["/F", "/IM", &format!("{APP_NAME}.exe")]);
    let _ = crate::proc::quiet(&mut cmd).output();
    let _ = std::fs::remove_file(paths.stop_file());
}

/// Copies this program into the app folder (and WebView2Loader.dll if it
/// sits next to it, which only local builds need), adds the shortcut, and
/// moves a "Start with Windows" entry to the installed path.
pub fn install_self() -> Result<PathBuf, String> {
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let target = installed_exe();
    std::fs::create_dir_all(app_dir()).map_err(|e| e.to_string())?;
    // A running installed copy can be renamed but not overwritten.
    if target.exists() {
        let old = crate::update::old_path(&target);
        let _ = std::fs::remove_file(&old);
        if std::fs::copy(&me, &target).is_err() {
            std::fs::rename(&target, &old).map_err(|e| format!("can't replace the installed app: {e}"))?;
            std::fs::copy(&me, &target).map_err(|e| format!("can't install: {e}"))?;
        }
    } else {
        std::fs::copy(&me, &target).map_err(|e| format!("can't install: {e}"))?;
    }
    if let Some(dir) = me.parent() {
        let dll = dir.join("WebView2Loader.dll");
        if dll.exists() {
            let _ = std::fs::copy(&dll, app_dir().join("WebView2Loader.dll"));
        }
    }
    if let Err(e) = create_shortcut(&target) {
        crate::log!("Start menu shortcut not created: {e}");
    }
    Ok(target)
}

pub fn launch(exe: &Path) -> bool {
    let mut cmd = std::process::Command::new(exe);
    cmd.env("MKW_UPDATED", "1");
    crate::proc::spawn_outside(&mut cmd, 0).is_ok()
}

/// Removes the shortcut now; after this process exits, a helper deletes the
/// app folder (and the whole data folder when `everything`).
pub fn uninstall(everything: bool) {
    let _ = std::fs::remove_file(shortcut());
    let target = if everything { local_base() } else { app_dir() };
    let script = format!(
        "ping -n 4 127.0.0.1 >nul & rmdir /s /q \"{}\"",
        target.display()
    );
    let mut cmd = std::process::Command::new("cmd.exe");
    cmd.args(["/c", &script]).stdin(std::process::Stdio::null());
    let _ = crate::proc::spawn_outside(&mut cmd, windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
}
