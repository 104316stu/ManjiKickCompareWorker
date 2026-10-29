//! The helper programs: yt-dlp (downloads), ffmpeg (decodes) and deno (the
//! JavaScript runtime yt-dlp solves YouTube's player challenges with).
//! Pinned versions are downloaded once into the cache bin folder and
//! checked against a sha256 fixed here; config can point at system copies.
//!
//! Pins: yt-dlp 2026.08.19 (as the site uses); ffmpeg from imageio-ffmpeg
//! 0.6.0's wheels, the very build the site decodes with; deno 2.9.5.

use crate::config::Config;
use crate::paths::Paths;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Tools {
    pub ytdlp: PathBuf,
    pub ffmpeg: PathBuf,
    pub deno: Option<PathBuf>,
}

struct Pin {
    /// File name in the bin folder.
    file: &'static str,
    url: &'static str,
    sha256: &'static str,
    /// The archive member to take (prefix match), or None for a plain binary.
    member: Option<&'static str>,
}

const YTDLP_VERSION: &str = "2026.08.19";

#[cfg(all(windows, target_arch = "x86_64"))]
mod pins {
    use super::Pin;
    pub fn ytdlp() -> Option<Pin> {
        Some(Pin {
            file: "yt-dlp-2026.08.19.exe",
            url: "https://github.com/yt-dlp/yt-dlp/releases/download/2026.08.19/yt-dlp.exe",
            sha256: "66674953fe251b89f4d08c5f0e35e0728679bd67ab3d7d05c0562af101dd3e7a",
            member: None,
        })
    }
    pub fn ffmpeg() -> Option<Pin> {
        Some(Pin {
            file: "ffmpeg-7.1-imageio-0.6.0.exe",
            url: "https://files.pythonhosted.org/packages/2c/c6/fa760e12a2483469e2bf5058c5faff664acf66cadb4df2ad6205b016a73d/imageio_ffmpeg-0.6.0-py3-none-win_amd64.whl",
            sha256: "02fa47c83703c37df6bfe4896aab339013f62bf02c5ebf2dce6da56af04ffc0a",
            member: Some("imageio_ffmpeg/binaries/ffmpeg-"),
        })
    }
    pub fn deno() -> Option<Pin> {
        Some(Pin {
            file: "deno-2.9.5.exe",
            url: "https://github.com/denoland/deno/releases/download/v2.9.5/deno-x86_64-pc-windows-msvc.zip",
            sha256: "171efab55ac6b9881fd53ee4c20f8bf3bb1340ffc618483746909014db12216a",
            member: Some("deno.exe"),
        })
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod pins {
    use super::Pin;
    fn musl() -> bool {
        std::path::Path::new("/lib/ld-musl-x86_64.so.1").exists()
    }
    pub fn ytdlp() -> Option<Pin> {
        Some(if musl() {
            Pin {
                file: "yt-dlp-2026.08.19-musl",
                url: "https://github.com/yt-dlp/yt-dlp/releases/download/2026.08.19/yt-dlp_musllinux",
                sha256: "f3dec9cfeaf304cec98290fe41c6ad465d4b747d302473559643e7af24929722",
                member: None,
            }
        } else {
            Pin {
                file: "yt-dlp-2026.08.19",
                url: "https://github.com/yt-dlp/yt-dlp/releases/download/2026.08.19/yt-dlp_linux",
                sha256: "58162f9bfdc27458ea47bfcb311cf47028f17d8154a8bf7d689861d46399230a",
                member: None,
            }
        })
    }
    pub fn ffmpeg() -> Option<Pin> {
        // A static build: runs on glibc and musl alike.
        Some(Pin {
            file: "ffmpeg-7.0.2-imageio-0.6.0",
            url: "https://files.pythonhosted.org/packages/a0/2d/43c8522a2038e9d0e7dbdf3a61195ecc31ca576fb1527a528c877e87d973/imageio_ffmpeg-0.6.0-py3-none-manylinux2014_x86_64.whl",
            sha256: "c7e46fcec401dd990405049d2e2f475e2b397779df2519b544b8aab515195282",
            member: Some("imageio_ffmpeg/binaries/ffmpeg-"),
        })
    }
    pub fn deno() -> Option<Pin> {
        if musl() {
            return None; // deno ships glibc builds only
        }
        Some(Pin {
            file: "deno-2.9.5",
            url: "https://github.com/denoland/deno/releases/download/v2.9.5/deno-x86_64-unknown-linux-gnu.zip",
            sha256: "8b010a3b1a4a0188a67cdb8a7a27348b2a501af78aec7fc74f2ace167368d530",
            member: Some("deno"),
        })
    }
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
mod pins {
    use super::Pin;
    pub fn ytdlp() -> Option<Pin> {
        Some(Pin {
            file: "yt-dlp-2026.08.19",
            url: "https://github.com/yt-dlp/yt-dlp/releases/download/2026.08.19/yt-dlp_linux_aarch64",
            sha256: "b16e4dab368a816cd05d477d698a605a6ae87ccee1c8ffd38fa21d7254141fcc",
            member: None,
        })
    }
    pub fn ffmpeg() -> Option<Pin> {
        Some(Pin {
            file: "ffmpeg-imageio-0.6.0",
            url: "https://files.pythonhosted.org/packages/33/e7/1925bfbc563c39c1d2e82501d8372734a5c725e53ac3b31b4c2d081e895b/imageio_ffmpeg-0.6.0-py3-none-manylinux2014_aarch64.whl",
            sha256: "1d47bebd83d2c5fc770720d211855f208af8a596c82d17730aa51e815cdee6dc",
            member: Some("imageio_ffmpeg/binaries/ffmpeg-"),
        })
    }
    pub fn deno() -> Option<Pin> {
        Some(Pin {
            file: "deno-2.9.5",
            url: "https://github.com/denoland/deno/releases/download/v2.9.5/deno-aarch64-unknown-linux-gnu.zip",
            sha256: "6b7cae3a8fc4385a59dea3146fcb8bad7fea4230e0ad36a8c692afacbc254be0",
            member: Some("deno"),
        })
    }
}

#[cfg(not(any(
    all(windows, target_arch = "x86_64"),
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64"))
)))]
mod pins {
    use super::Pin;
    pub fn ytdlp() -> Option<Pin> {
        None
    }
    pub fn ffmpeg() -> Option<Pin> {
        None
    }
    pub fn deno() -> Option<Pin> {
        None
    }
}

fn exe_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

/// `base` on PATH.
pub fn on_path(base: &str) -> Option<PathBuf> {
    let name = exe_name(base);
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(&name))
        .find(|p| p.is_file())
}

fn resolve(
    setting: &str,
    base: &str,
    pin: Option<Pin>,
    paths: &Paths,
    progress: &dyn Fn(String),
) -> Result<Option<PathBuf>, String> {
    match setting.trim() {
        "none" | "off" => Ok(None),
        "system" => on_path(base).map(Some).ok_or_else(|| format!("{base} not found on PATH")),
        "" | "auto" => match pin {
            Some(pin) => install(&pin, base, paths, progress).map(Some),
            None => on_path(base).map(Some).ok_or_else(|| format!("no pinned {base} for this platform; install it")),
        },
        path => {
            let p = PathBuf::from(path);
            if p.is_file() {
                Ok(Some(p))
            } else {
                Err(format!("{base}: {path} does not exist"))
            }
        }
    }
}

pub fn ensure(cfg: &Config, paths: &Paths, progress: &dyn Fn(String)) -> Result<Tools, String> {
    let ytdlp = resolve(&cfg.ytdlp, "yt-dlp", pins::ytdlp(), paths, progress)?.ok_or("yt-dlp is required")?;
    let ffmpeg = resolve(&cfg.ffmpeg, "ffmpeg", pins::ffmpeg(), paths, progress)?.ok_or("ffmpeg is required")?;
    // Without deno yt-dlp still works, on a degraded path; don't fail for it.
    let deno = match resolve(&cfg.deno, "deno", pins::deno(), paths, progress) {
        Ok(d) => d,
        Err(e) => {
            crate::log!("deno unavailable ({e}); YouTube downloads may miss formats");
            None
        }
    };
    let mut keep = vec![ytdlp.clone(), ffmpeg.clone()];
    keep.extend(deno.clone());
    cleanup_old(paths, &keep);
    Ok(Tools { ytdlp, ffmpeg, deno })
}

/// Old pinned versions left in the bin folder after an upgrade.
fn cleanup_old(paths: &Paths, keep: &[PathBuf]) {
    let Ok(dir) = std::fs::read_dir(&paths.bin_dir) else { return };
    let kept: Vec<String> = keep
        .iter()
        .filter(|k| k.parent() == Some(paths.bin_dir.as_path()))
        .filter_map(|k| k.file_name()?.to_str().map(String::from))
        .collect();
    for e in dir.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let ours = ["yt-dlp-", "ffmpeg-", "deno-"].iter().any(|p| name.starts_with(p));
        let current = kept.iter().any(|k| name == *k || name == format!("{k}.sha256"));
        if ours && !current {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn marker(target: &Path) -> PathBuf {
    let mut s = target.as_os_str().to_owned();
    s.push(".sha256");
    PathBuf::from(s)
}

fn install(pin: &Pin, base: &str, paths: &Paths, progress: &dyn Fn(String)) -> Result<PathBuf, String> {
    let target = paths.bin_dir.join(pin.file);
    if target.is_file()
        && std::fs::read_to_string(marker(&target)).map(|m| m.trim() == pin.sha256).unwrap_or(false)
    {
        return Ok(target);
    }
    std::fs::create_dir_all(&paths.bin_dir).map_err(|e| e.to_string())?;
    crate::log!("downloading {base} from {}", pin.url);
    let part = paths.bin_dir.join(format!("{}.download", pin.file));
    let digest = download(pin.url, &part, &|pct| progress(format!("downloading {base} ({pct}%)")))?;
    if digest != pin.sha256 {
        let _ = std::fs::remove_file(&part);
        return Err(format!("{base}: checksum mismatch (got {digest}), refusing to use it"));
    }
    let tmp = paths.bin_dir.join(format!("{}.tmp", pin.file));
    match pin.member {
        None => std::fs::rename(&part, &tmp).map_err(|e| e.to_string())?,
        Some(member) => {
            progress(format!("unpacking {base}"));
            let bytes = std::fs::read(&part).map_err(|e| e.to_string())?;
            let (_, data) = crate::zipx::extract(&bytes, |n| {
                if member.ends_with('-') { n.starts_with(member) } else { n == member }
            })?;
            drop(bytes);
            std::fs::write(&tmp, data).map_err(|e| e.to_string())?;
            let _ = std::fs::remove_file(&part);
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755));
    }
    let _ = std::fs::remove_file(&target);
    std::fs::rename(&tmp, &target).map_err(|e| e.to_string())?;
    std::fs::write(marker(&target), pin.sha256).map_err(|e| e.to_string())?;
    crate::log!("{base} ready: {}", target.display());
    Ok(target)
}

/// Streams `url` to `dest`; returns its sha256 (hex).
fn download(url: &str, dest: &Path, progress: &dyn Fn(u64)) -> Result<String, String> {
    let agent = crate::api::agent(Duration::from_secs(60));
    let resp = agent.get(url).call().map_err(|e| format!("download failed: {e}"))?;
    let total: u64 = resp.header("Content-Length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(dest).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let (mut got, mut shown) = (0u64, 0u64);
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("download failed: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        got += n as u64;
        if total > 0 {
            let pct = got * 100 / total;
            if pct >= shown + 5 {
                shown = pct;
                progress(pct);
            }
        }
    }
    file.flush().map_err(|e| e.to_string())?;
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

pub fn ytdlp_version() -> &'static str {
    YTDLP_VERSION
}
