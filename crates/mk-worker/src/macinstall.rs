//! macOS: the program installs itself like a normal app, all inside the
//! user's home, so no admin password is ever needed.
//!
//!   ~/Applications/ManjiKick Worker.app                  the app (Spotlight finds it)
//!   ~/Library/LaunchAgents/com.manjikick.worker.plist    starts it at login, restarts it after a crash
//!   ~/Library/Application Support/ManjiKickWorker/       data (config, token, bin/)
//!
//! launchd runs it: a crash brings it back, Quit (a clean exit) does not.
//! The .app is built here from the one program, so a self-update only has to
//! replace that program.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const APP_NAME: &str = "ManjiKick Worker";
pub const LABEL: &str = "com.manjikick.worker";
/// Set by the launch agent: this process is the one launchd looks after.
pub const AGENT_ENV: &str = "MKW_AGENT";

const ICON: &[u8] = include_bytes!("../../../assets/icon.icns");

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

pub fn app_dir() -> PathBuf {
    home().join("Applications").join(format!("{APP_NAME}.app"))
}

pub fn installed_exe() -> PathBuf {
    app_dir().join("Contents/MacOS/mkworker")
}

pub fn agent_plist() -> PathBuf {
    home().join("Library/LaunchAgents").join(format!("{LABEL}.plist"))
}

fn domain() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}

fn service() -> String {
    format!("{}/{LABEL}", domain())
}

fn launchctl(args: &[&str]) -> Result<(), String> {
    let out = Command::new("/bin/launchctl").args(args).stdin(Stdio::null()).output().map_err(|e| e.to_string())?;
    if out.status.success() {
        return Ok(());
    }
    let text = String::from_utf8_lossy(&out.stderr);
    Err(format!("launchctl {}: {}", args[0], text.trim()))
}

/// Started from inside an .app (Finder, Spotlight, the launch agent).
pub fn in_bundle() -> bool {
    std::env::current_exe().map(|e| e.to_string_lossy().contains(".app/Contents/MacOS/")).unwrap_or(false)
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn agent_text(exe: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array><string>{}</string><string>app</string></array>
  <key>EnvironmentVariables</key>
  <dict><key>{AGENT_ENV}</key><string>1</string></dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key>
  <dict><key>SuccessfulExit</key><false/></dict>
  <key>ThrottleInterval</key><integer>60</integer>
  <key>ExitTimeOut</key><integer>30</integer>
  <key>ProcessType</key><string>Interactive</string>
  <key>AbandonProcessGroup</key><true/>
  <key>LimitLoadToSessionType</key><string>Aqua</string>
</dict>
</plist>
"#,
        xml(&exe.to_string_lossy())
    )
}

fn info_text() -> String {
    let version = env!("CARGO_PKG_VERSION");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>{APP_NAME}</string>
  <key>CFBundleDisplayName</key><string>{APP_NAME}</string>
  <key>CFBundleIdentifier</key><string>{LABEL}</string>
  <key>CFBundleExecutable</key><string>mkworker</string>
  <key>CFBundleIconFile</key><string>icon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>{version}</string>
  <key>CFBundleVersion</key><string>{version}</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
"#
    )
}

/// Written beside the target, then renamed in: macOS remembers a program's
/// signature per file, and changing a signed file in place gets it killed.
fn put(path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let dir = path.parent().ok_or("no parent folder")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes).map_err(|e| format!("can't write {}: {e}", tmp.display()))?;
    let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode));
    std::fs::rename(&tmp, path).map_err(|e| format!("can't write {}: {e}", path.display()))
}

fn put_if_changed(path: &Path, text: &str) -> Result<bool, String> {
    if std::fs::read_to_string(path).is_ok_and(|t| t == text) {
        return Ok(false);
    }
    put(path, text.as_bytes(), 0o644).map(|_| true)
}

fn write_bundle() -> Result<(), String> {
    let contents = app_dir().join("Contents");
    put_if_changed(&contents.join("Info.plist"), &info_text())?;
    if !contents.join("Resources/icon.icns").exists() {
        put(&contents.join("Resources/icon.icns"), ICON, 0o644)?;
    }
    put_if_changed(&contents.join("PkgInfo"), "APPL????")?;
    Ok(())
}

pub fn start_at_login() -> bool {
    agent_plist().exists()
}

/// Off removes the agent file: nothing starts it at the next login, and the
/// running app is left alone.
pub fn set_start_at_login(on: bool) {
    if !on {
        let _ = std::fs::remove_file(agent_plist());
        return;
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| installed_exe());
    if let Err(e) = put_if_changed(&agent_plist(), &agent_text(&exe)) {
        crate::log!("start at login not set: {e}");
    }
    // Fails while it is loaded already, which is fine.
    let _ = load();
}

pub fn loaded() -> bool {
    launchctl(&["print", &service()]).is_ok()
}

fn load() -> Result<(), String> {
    let plist = agent_plist();
    let _ = launchctl(&["enable", &service()]);
    launchctl(&["bootstrap", &domain(), &plist.to_string_lossy()])
}

/// Started by hand (Finder, Spotlight) with the agent installed: let launchd
/// start it instead, so that it is looked after. False: run here after all.
pub fn hand_to_launchd() -> bool {
    if !start_at_login() {
        return false;
    }
    if !loaded() {
        if let Err(e) = load() {
            crate::log!("launch agent not loaded: {e}");
            return false;
        }
    }
    match launchctl(&["kickstart", &service()]) {
        Ok(()) => true,
        Err(e) => {
            crate::log!("launch agent not started: {e}");
            false
        }
    }
}

/// On every start of the app: after a self-update the bundle's version is out
/// of date, and an agent left by a moved app points at the wrong place.
pub fn refresh() {
    if let Err(e) = write_bundle() {
        crate::log!("app bundle not updated: {e}");
    }
    if start_at_login() {
        if let Ok(exe) = std::env::current_exe() {
            if let Err(e) = put_if_changed(&agent_plist(), &agent_text(&exe)) {
                crate::log!("launch agent not updated: {e}");
            }
        }
    }
}

fn wait_until_stopped(paths: &crate::paths::Paths) {
    let pid: Option<i32> = std::fs::read_to_string(paths.pid_file()).ok().and_then(|p| p.trim().parse().ok());
    let Some(pid) = pid else { return };
    if !crate::platform::process_alive(pid) || !crate::platform::is_our_program(pid) {
        return;
    }
    crate::platform::terminate(pid);
    for _ in 0..60 {
        if !crate::platform::process_alive(pid) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

/// Installs this program as the app (or updates it) and starts it.
pub fn install() -> Result<(), String> {
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let target = installed_exe();
    if loaded() {
        let _ = launchctl(&["bootout", &service()]);
    }
    let paths = crate::paths::Paths::get();
    paths.create_all();
    wait_until_stopped(&paths);
    write_bundle()?;
    if std::fs::canonicalize(&me).ok() != std::fs::canonicalize(&target).ok() {
        let bytes = std::fs::read(&me).map_err(|e| e.to_string())?;
        put(&target, &bytes, 0o755)?;
    }
    // A copy that came through a browser carries the quarantine mark.
    let _ = Command::new("/usr/bin/xattr").args(["-dr", "com.apple.quarantine"]).arg(app_dir())
        .stderr(Stdio::null()).status();
    let _ = Command::new("/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister")
        .arg("-f").arg(app_dir()).stdout(Stdio::null()).stderr(Stdio::null()).status();
    put_if_changed(&agent_plist(), &agent_text(&target))?;
    match load().and_then(|_| launchctl(&["kickstart", &service()])) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Login items switched off in System Settings, or no desktop
            // session (over SSH): the app still runs when opened.
            println!("not started by launchd ({e}); opening it directly");
            Command::new("/usr/bin/open").arg(app_dir()).status().map_err(|e| e.to_string()).map(|_| ())
        }
    }
}

/// Opens the app again just after this process has exited.
pub fn open_later() {
    use std::os::unix::process::CommandExt;
    let _ = Command::new("/bin/sh").args(["-c", "sleep 2; exec /usr/bin/open \"$0\""]).arg(app_dir())
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0).spawn();
}

/// Removes the agent file now; after this process has exited, a helper
/// unloads the agent and deletes the app (and all data when `everything`).
pub fn uninstall(everything: bool) {
    use std::os::unix::process::CommandExt;
    let _ = std::fs::remove_file(agent_plist());
    let quote = |p: &Path| format!("'{}'", p.to_string_lossy().replace('\'', "'\\''"));
    let paths = crate::paths::Paths::get();
    let mut script = format!("sleep 2; /bin/launchctl bootout {} 2>/dev/null; rm -rf {}", service(), quote(&app_dir()));
    if everything {
        for dir in [Some(paths.config_dir.clone()), paths.log_file.parent().map(PathBuf::from),
                    paths.tmp_dir.parent().map(PathBuf::from)].into_iter().flatten() {
            script.push_str(&format!(" {}", quote(&dir)));
        }
    }
    let _ = Command::new("/bin/sh").args(["-c", &script]).stdin(Stdio::null()).stdout(Stdio::null())
        .stderr(Stdio::null()).process_group(0).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plists_escape_the_path() {
        let text = agent_text(Path::new("/Users/a&b/x"));
        assert!(text.contains("/Users/a&amp;b/x"));
        assert!(text.contains(LABEL));
        assert!(info_text().contains(env!("CARGO_PKG_VERSION")));
    }
}
