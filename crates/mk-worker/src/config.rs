//! The config file: plain `key = value` lines, `#` comments. The tray writes
//! it; on Linux edit it by hand or pass flags.
//!
//!   server = https://audios.manjikick.com
//!   mode = auto                  # light | balanced | max | auto
//!   threads = 0                  # 0 = the mode decides
//!   duty = 1.0                   # share of CPU time to use (CLI)
//!   max_video_seconds = 1800
//!   running = true               # tray's Running toggle
//!   pause_fullscreen = true      # tray checkbox (overrides the mode)
//!   pause_battery = true
//!   ytdlp = auto                 # auto (pinned download) | system | <path>
//!   ffmpeg = auto
//!   deno = auto                  # also: none
//!   auto_update = true           # install new releases by itself

use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Light,
    Balanced,
    Max,
    Auto,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "light" => Some(Mode::Light),
            "balanced" => Some(Mode::Balanced),
            "max" => Some(Mode::Max),
            "auto" => Some(Mode::Auto),
            _ => None,
        }
    }
    pub fn key(self) -> &'static str {
        match self {
            Mode::Light => "light",
            Mode::Balanced => "balanced",
            Mode::Max => "max",
            Mode::Auto => "auto",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Mode::Light => "Light",
            Mode::Balanced => "Balanced",
            Mode::Max => "Max",
            Mode::Auto => "Auto",
        }
    }
    /// The pause checkboxes a mode starts with; the user may flip them after.
    pub fn default_pauses(self) -> (bool, bool) {
        match self {
            Mode::Light | Mode::Balanced => (true, true),
            Mode::Max => (false, false),
            Mode::Auto => (true, false),
        }
    }
}

/// What a mode means right now on this machine.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Policy {
    pub threads: usize,
    pub prefetch: usize,
    /// IDLE_PRIORITY_CLASS rather than BELOW_NORMAL (Windows).
    pub idle_priority: bool,
    /// Pause when other programs keep the CPU busy.
    pub pause_busy: bool,
    /// Max-like: halves its threads for 30 min after a YouTube cooldown.
    pub heavy: bool,
    /// For the status line, e.g. "Auto (Max)".
    pub label: &'static str,
}

pub fn cores() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2)
}

pub fn light() -> Policy {
    Policy { threads: 1, prefetch: 0, idle_priority: true, pause_busy: true, heavy: false, label: "Light" }
}
pub fn balanced(cores: usize) -> Policy {
    Policy { threads: (cores / 2).max(1), prefetch: 1, idle_priority: true, pause_busy: false, heavy: false, label: "Balanced" }
}
pub fn max(cores: usize) -> Policy {
    Policy { threads: cores.saturating_sub(1).max(1), prefetch: 2, idle_priority: false, pause_busy: false, heavy: true, label: "Max" }
}

/// `machine_free`: Auto's switch. Windows: no input for 5 min; Linux: the
/// load average from everything else is low.
pub fn policy(mode: Mode, cores: usize, machine_free: bool) -> Policy {
    match mode {
        Mode::Light => light(),
        Mode::Balanced => balanced(cores),
        Mode::Max => max(cores),
        Mode::Auto if machine_free => Policy { label: "Auto (Max)", ..max(cores) },
        Mode::Auto => Policy { label: "Auto (Light)", ..light() },
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub server: String,
    pub mode: Mode,
    /// 0 = the mode decides.
    pub threads: usize,
    pub duty: f64,
    pub max_video_seconds: u32,
    pub running: bool,
    pub pause_fullscreen: bool,
    pub pause_battery: bool,
    pub ytdlp: String,
    pub ffmpeg: String,
    pub deno: String,
    pub auto_update: bool,
}

impl Default for Config {
    fn default() -> Self {
        let (pf, pb) = Mode::Auto.default_pauses();
        Config {
            server: crate::DEFAULT_SERVER.into(),
            mode: Mode::Auto,
            threads: 0,
            duty: 1.0,
            max_video_seconds: 1800,
            running: true,
            pause_fullscreen: pf,
            pause_battery: pb,
            ytdlp: "auto".into(),
            ffmpeg: "auto".into(),
            deno: "auto".into(),
            auto_update: true,
        }
    }
}

fn truthy(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

impl Config {
    pub fn load(path: &Path) -> Config {
        let mut c = Config::default();
        let Ok(text) = std::fs::read_to_string(path) else { return c };
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            match k {
                "server" if v.starts_with("https://") || v.starts_with("http://") => {
                    c.server = v.trim_end_matches('/').to_string()
                }
                "mode" => {
                    if let Some(m) = Mode::parse(v) {
                        c.mode = m
                    }
                }
                "threads" => c.threads = v.parse().unwrap_or(0),
                "duty" => c.duty = v.parse::<f64>().unwrap_or(1.0).clamp(0.05, 1.0),
                "max_video_seconds" => c.max_video_seconds = v.parse().unwrap_or(1800),
                "running" => c.running = truthy(v),
                "pause_fullscreen" => c.pause_fullscreen = truthy(v),
                "pause_battery" => c.pause_battery = truthy(v),
                "ytdlp" | "yt-dlp" => c.ytdlp = v.to_string(),
                "ffmpeg" => c.ffmpeg = v.to_string(),
                "deno" => c.deno = v.to_string(),
                "auto_update" => c.auto_update = truthy(v),
                _ => {}
            }
        }
        c
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let text = format!(
            "# ManjiKick compare worker settings (key = value)\n\
             server = {}\nmode = {}\nthreads = {}\nduty = {}\nmax_video_seconds = {}\nrunning = {}\n\
             pause_fullscreen = {}\npause_battery = {}\n\
             # auto = download the pinned build; system = use the one on PATH; or a full path\n\
             ytdlp = {}\nffmpeg = {}\ndeno = {}\nauto_update = {}\n",
            self.server, self.mode.key(), self.threads, self.duty, self.max_video_seconds, self.running,
            self.pause_fullscreen, self.pause_battery, self.ytdlp, self.ffmpeg, self.deno, self.auto_update
        );
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_then_load_round_trips() {
        let dir = std::env::temp_dir().join(format!("mkw-cfg-{}", std::process::id()));
        let path = dir.join("config");
        let c = Config { mode: Mode::Max, threads: 3, duty: 0.2, running: false, ffmpeg: "system".into(), ..Default::default() };
        c.save(&path).unwrap();
        let d = Config::load(&path);
        assert_eq!((d.mode, d.threads, d.duty, d.running, d.ffmpeg.as_str()), (Mode::Max, 3, 0.2, false, "system"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn modes_scale_with_cores() {
        assert_eq!(policy(Mode::Light, 8, true).threads, 1);
        assert_eq!(policy(Mode::Balanced, 8, false).threads, 4);
        assert_eq!(policy(Mode::Max, 8, false).threads, 7);
        assert_eq!(policy(Mode::Max, 1, false).threads, 1);
        assert_eq!(policy(Mode::Auto, 8, true).threads, 7);
        assert_eq!(policy(Mode::Auto, 8, false).threads, 1);
        assert!(policy(Mode::Max, 8, false).prefetch <= 2);
    }
}
