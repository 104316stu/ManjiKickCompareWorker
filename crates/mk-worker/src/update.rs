//! Self-update from the GitHub releases of this repository. A newer release's
//! asset is downloaded next to the running program, checked against the
//! release's SHA256SUMS, tried (`version`), and only then swapped in.

use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

pub const LATEST_URL: &str = "https://api.github.com/repos/104316stu/ManjiKickCompareWorker/releases/latest";
/// How often a running worker looks for a new release.
pub const CHECK_EVERY: f64 = 24.0 * 3600.0;

static ASSET: OnceLock<&'static str> = OnceLock::new();

/// Which release asset this program is (set by each binary's main).
pub fn set_asset(name: &'static str) {
    let _ = ASSET.set(name);
}

pub fn asset() -> Option<&'static str> {
    ASSET.get().copied()
}

/// "0.2.0" from "v0.2.0" / "0.2.0"; None if it isn't x.y.z.
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().trim_start_matches('v');
    let core = s.split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
    let v = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(v)
}

pub fn current() -> (u64, u64, u64) {
    parse_version(env!("CARGO_PKG_VERSION")).unwrap_or((0, 0, 0))
}

#[derive(Clone, Debug)]
pub struct Release {
    pub tag: String,
    pub asset_url: String,
    pub sums_url: String,
}

/// The latest release if it is newer than this program and has our asset.
pub fn check() -> Result<Option<Release>, String> {
    let name = asset().ok_or("no release asset for this program")?;
    let agent = crate::api::agent(Duration::from_secs(30));
    let resp = agent
        .get(LATEST_URL)
        .set("Accept", "application/vnd.github+json")
        .call();
    let resp = match resp {
        Ok(r) => r,
        // No release published yet: nothing to update to.
        Err(ureq::Error::Status(404, _)) => return Ok(None),
        Err(e) => return Err(format!("update check failed: {e}")),
    };
    let v: serde_json::Value = serde_json::from_str(&resp.into_string().map_err(|e| e.to_string())?)
        .map_err(|e| format!("update check: bad reply: {e}"))?;
    let tag = v["tag_name"].as_str().unwrap_or("").to_string();
    let Some(latest) = parse_version(&tag) else { return Ok(None) };
    // Older or the same (e.g. a local build ahead of the releases): stay.
    if latest <= current() {
        return Ok(None);
    }
    let url_of = |n: &str| {
        v["assets"].as_array()?.iter().find(|a| a["name"].as_str() == Some(n))?["browser_download_url"]
            .as_str()
            .map(str::to_string)
    };
    match (url_of(name), url_of("SHA256SUMS")) {
        (Some(asset_url), Some(sums_url)) => Ok(Some(Release { tag, asset_url, sums_url })),
        _ => Ok(None), // a release still uploading, or one without our build
    }
}

fn fetch(url: &str, limit: u64) -> Result<Vec<u8>, String> {
    let agent = crate::api::agent(Duration::from_secs(60));
    let resp = agent.get(url).call().map_err(|e| format!("download failed: {e}"))?;
    let mut bytes = Vec::new();
    resp.into_reader().take(limit).read_to_end(&mut bytes).map_err(|e| format!("download failed: {e}"))?;
    Ok(bytes)
}

fn new_path(exe: &Path) -> PathBuf {
    let mut s = exe.as_os_str().to_owned();
    s.push(".new");
    PathBuf::from(s)
}

pub fn old_path(exe: &Path) -> PathBuf {
    let mut s = exe.as_os_str().to_owned();
    s.push(".old");
    PathBuf::from(s)
}

/// Downloads and checks the release; returns the ready file next to `exe`.
pub fn download(release: &Release, exe: &Path) -> Result<PathBuf, String> {
    let name = asset().ok_or("no release asset for this program")?;
    let sums = String::from_utf8_lossy(&fetch(&release.sums_url, 64 * 1024)?).to_string();
    let expected = sums
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((it.next()?, it.next()?.trim_start_matches('*')))
        })
        .find(|(_, n)| *n == name)
        .map(|(h, _)| h.to_ascii_lowercase())
        .ok_or_else(|| format!("{name} is not in SHA256SUMS"))?;
    let bytes = fetch(&release.asset_url, 200 * 1024 * 1024)?;
    let got: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
    if got != expected {
        return Err(format!("{name}: checksum mismatch, not installing it"));
    }
    let path = new_path(exe);
    std::fs::write(&path, &bytes).map_err(|e| format!("can't write {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    }
    // It has to run here before it replaces anything.
    let want = parse_version(&release.tag);
    let mut cmd = std::process::Command::new(&path);
    cmd.arg("--version");
    let out = crate::proc::quiet(&mut cmd).output();
    let ok = matches!(&out, Ok(o) if o.status.success()
        && String::from_utf8_lossy(&o.stdout).split_whitespace()
            .any(|w| parse_version(w.trim_start_matches("rs-")) == want));
    if !ok {
        let _ = std::fs::remove_file(&path);
        return Err(format!("the {} build does not run here; staying on this version", release.tag));
    }
    Ok(path)
}

/// Puts `new` in place of `exe`, keeping the old one as `.old` until the
/// new program has started. Returns false (and changes nothing) on failure.
pub fn swap(exe: &Path, new: &Path) -> Result<(), String> {
    let old = old_path(exe);
    let _ = std::fs::remove_file(&old);
    // Windows can rename a running program but not overwrite it.
    std::fs::rename(exe, &old).map_err(|e| format!("can't move the old program aside: {e}"))?;
    if let Err(e) = std::fs::rename(new, exe) {
        let _ = std::fs::rename(&old, exe);
        return Err(format!("can't put the new program in place: {e}"));
    }
    Ok(())
}

/// Undoes `swap` (the new program would not start).
pub fn roll_back(exe: &Path) {
    let old = old_path(exe);
    if old.exists() {
        let _ = std::fs::remove_file(exe);
        let _ = std::fs::rename(&old, exe);
    }
}

/// After an update: remove the previous program (it may still be exiting).
pub fn clean_old() {
    let Ok(exe) = std::env::current_exe() else { return };
    let old = old_path(&exe);
    if !old.exists() {
        return;
    }
    std::thread::spawn(move || {
        for _ in 0..30 {
            if std::fs::remove_file(&old).is_ok() || !old.exists() {
                return;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
}

/// Starts the swapped-in program with the same arguments. On Unix the
/// process becomes it (same pid); on Windows a new process starts and this
/// one must exit. Rolls back if it can't be started.
pub fn relaunch(exe: &Path) -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(exe).args(&args).env("MKW_UPDATED", "1").exec();
        roll_back(exe);
        Err(format!("the new program did not start ({err}); rolled back"))
    }
    #[cfg(windows)]
    {
        let mut cmd = std::process::Command::new(exe);
        cmd.args(&args).env("MKW_UPDATED", "1");
        match crate::proc::spawn_outside(&mut cmd, windows_sys::Win32::System::Threading::DETACHED_PROCESS) {
            Ok(_) => Ok(()),
            Err(e) => {
                roll_back(exe);
                Err(format!("the new program did not start ({e}); rolled back"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare() {
        assert_eq!(parse_version("v0.2.0"), Some((0, 2, 0)));
        assert_eq!(parse_version("1.10.3-rc1"), Some((1, 10, 3)));
        assert_eq!(parse_version("v1.2"), None);
        assert!(parse_version("v0.10.0") > parse_version("v0.9.9"));
    }
}
