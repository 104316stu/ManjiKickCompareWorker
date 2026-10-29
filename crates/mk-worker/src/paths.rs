//! Where everything lives. Windows: %LOCALAPPDATA%\ManjiKickWorker\.
//! Linux: XDG dirs (~/.config/mkworker, ~/.local/state/mkworker,
//! ~/.cache/mkworker). MKW_HOME puts all of it under one folder instead
//! (handy on panels where $HOME is odd).

use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub log_file: PathBuf,
    pub bin_dir: PathBuf,
    pub tmp_dir: PathBuf,
}

impl Paths {
    pub fn get() -> Paths {
        if let Some(home) = std::env::var_os("MKW_HOME").filter(|v| !v.is_empty()) {
            let base = PathBuf::from(home);
            return Paths {
                config_dir: base.clone(),
                state_dir: base.clone(),
                log_file: base.join("log"),
                bin_dir: base.join("bin"),
                tmp_dir: base.join("tmp"),
            };
        }
        Self::platform()
    }

    #[cfg(windows)]
    fn platform() -> Paths {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("ManjiKickWorker");
        Paths {
            config_dir: base.clone(),
            state_dir: base.clone(),
            log_file: base.join("log").join("mkworker.log"),
            bin_dir: base.join("bin"),
            tmp_dir: base.join("tmp"),
        }
    }

    #[cfg(not(windows))]
    fn platform() -> Paths {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        let xdg = |var: &str, fallback: &str| {
            std::env::var_os(var)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(fallback))
                .join("mkworker")
        };
        let state = xdg("XDG_STATE_HOME", ".local/state");
        let cache = xdg("XDG_CACHE_HOME", ".cache");
        Paths {
            config_dir: xdg("XDG_CONFIG_HOME", ".config"),
            log_file: state.join("log"),
            state_dir: state,
            bin_dir: cache.join("bin"),
            tmp_dir: cache.join("tmp"),
        }
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config")
    }
    pub fn token_file(&self) -> PathBuf {
        self.config_dir.join("token")
    }
    pub fn status_file(&self) -> PathBuf {
        self.state_dir.join("status.json")
    }
    pub fn pid_file(&self) -> PathBuf {
        self.state_dir.join("pid")
    }
    /// `mkworker stop` creates it; a running worker sees it, removes it and stops.
    pub fn stop_file(&self) -> PathBuf {
        self.state_dir.join("stop")
    }
    pub fn log_dir(&self) -> PathBuf {
        self.log_file.parent().map(PathBuf::from).unwrap_or_else(|| self.state_dir.clone())
    }

    pub fn create_all(&self) {
        for dir in [&self.config_dir, &self.state_dir, &self.bin_dir, &self.tmp_dir, &self.log_dir()] {
            let _ = std::fs::create_dir_all(dir);
        }
    }

    pub fn read_token(&self) -> Option<String> {
        if let Ok(v) = std::env::var("MKW_TOKEN") {
            let v = v.trim().to_string();
            if v.starts_with("mkw_") {
                return Some(v);
            }
        }
        let v = std::fs::read_to_string(self.token_file()).ok()?;
        let v = v.trim().to_string();
        v.starts_with("mkw_").then_some(v)
    }

    pub fn write_token(&self, token: &str) -> std::io::Result<()> {
        let _ = std::fs::create_dir_all(&self.config_dir);
        let path = self.token_file();
        std::fs::write(&path, format!("{}\n", token.trim()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }
}
