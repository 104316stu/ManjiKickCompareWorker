//! OS specifics: priority, CPU time, local time, stop signals, single
//! instance, and the sensors the modes use (fullscreen, battery, idle, load).

use std::sync::atomic::{AtomicBool, Ordering};

/// Set by Ctrl+C / SIGTERM.
pub static STOP_SIGNAL: AtomicBool = AtomicBool::new(false);

pub fn stop_requested() -> bool {
    STOP_SIGNAL.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------- Windows
#[cfg(windows)]
mod imp {
    use super::*;
    use windows_sys::Win32::Foundation::*;
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    use windows_sys::Win32::System::SystemInformation::{GetLocalTime, GetTickCount};
    use windows_sys::Win32::System::Threading::*;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    use windows_sys::Win32::UI::Shell::SHQueryUserNotificationState;

    pub fn set_priority(idle: bool) {
        let class = if idle { IDLE_PRIORITY_CLASS } else { BELOW_NORMAL_PRIORITY_CLASS };
        unsafe {
            SetPriorityClass(GetCurrentProcess(), class);
        }
    }

    fn ft(f: &FILETIME) -> u64 {
        ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64
    }

    /// This process's user + kernel CPU seconds.
    pub fn cpu_seconds() -> f64 {
        unsafe {
            let mut c = std::mem::zeroed();
            let mut e = std::mem::zeroed();
            let mut k = std::mem::zeroed();
            let mut u = std::mem::zeroed();
            if GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) == 0 {
                return 0.0;
            }
            (ft(&k) + ft(&u)) as f64 / 1e7
        }
    }

    /// The calling thread's user + kernel CPU seconds.
    pub fn thread_cpu_seconds() -> f64 {
        unsafe {
            let mut c = std::mem::zeroed();
            let mut e = std::mem::zeroed();
            let mut k = std::mem::zeroed();
            let mut u = std::mem::zeroed();
            if GetThreadTimes(GetCurrentThread(), &mut c, &mut e, &mut k, &mut u) == 0 {
                return 0.0;
            }
            (ft(&k) + ft(&u)) as f64 / 1e7
        }
    }

    /// CPU seconds of every process that ran in a job object.
    pub fn job_cpu_seconds(job: HANDLE) -> f64 {
        use windows_sys::Win32::System::JobObjects::*;
        if job.is_null() {
            return 0.0;
        }
        unsafe {
            let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = std::mem::zeroed();
            let ok = QueryInformationJobObject(
                job,
                JobObjectBasicAccountingInformation,
                &mut info as *mut _ as *mut _,
                std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            );
            if ok == 0 {
                return 0.0;
            }
            (info.TotalUserTime + info.TotalKernelTime) as f64 / 1e7
        }
    }

    static FAMILY: std::sync::atomic::AtomicPtr<core::ffi::c_void> =
        std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

    /// Puts this process in a job object of its own, so everything it starts
    /// (yt-dlp, deno, ffmpeg) is counted together and dies with it.
    pub fn init_family() {
        use windows_sys::Win32::System::JobObjects::*;
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(job, JobObjectExtendedLimitInformation, &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32);
            if AssignProcessToJobObject(job, GetCurrentProcess()) == 0 {
                CloseHandle(job);
                return;
            }
            FAMILY.store(job, Ordering::Relaxed);
        }
    }

    /// CPU seconds of this process and everything it started.
    pub fn family_cpu_seconds() -> f64 {
        let job = FAMILY.load(Ordering::Relaxed);
        if job.is_null() {
            cpu_seconds()
        } else {
            job_cpu_seconds(job)
        }
    }

    /// (busy, total) CPU ticks of the whole machine.
    pub fn system_ticks() -> Option<(u64, u64)> {
        use windows_sys::Win32::System::Threading::GetSystemTimes;
        unsafe {
            let mut i = std::mem::zeroed();
            let mut k = std::mem::zeroed();
            let mut u = std::mem::zeroed();
            if GetSystemTimes(&mut i, &mut k, &mut u) == 0 {
                return None;
            }
            // Kernel time includes idle time.
            let total = ft(&k) + ft(&u);
            Some((total - ft(&i), total))
        }
    }

    pub fn local_parts() -> (u16, u16, u16, u16, u16, u16) {
        unsafe {
            let mut t = std::mem::zeroed();
            GetLocalTime(&mut t);
            (t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
        }
    }

    unsafe extern "system" fn on_ctrl(_kind: u32) -> BOOL {
        STOP_SIGNAL.store(true, Ordering::Relaxed);
        1
    }

    pub fn install_stop_handler() {
        unsafe {
            SetConsoleCtrlHandler(Some(on_ctrl), 1);
        }
    }

    #[allow(dead_code)]
    pub struct Instance(HANDLE);
    unsafe impl Send for Instance {}

    /// One worker per user: the tray and the CLI share this mutex.
    pub fn single_instance() -> Option<Instance> {
        let name: Vec<u16> = "Local\\ManjiKickWorker.single".encode_utf16().chain(Some(0)).collect();
        unsafe {
            let h = CreateMutexW(std::ptr::null(), 1, name.as_ptr());
            if h.is_null() {
                return None;
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                CloseHandle(h);
                return None;
            }
            Some(Instance(h))
        }
    }

    /// A fullscreen app, game (D3D fullscreen) or presentation is running.
    pub fn fullscreen() -> bool {
        unsafe {
            let mut state = 0;
            if SHQueryUserNotificationState(&mut state) != 0 {
                return false;
            }
            // QUNS_BUSY = 2, QUNS_RUNNING_D3D_FULL_SCREEN = 3, QUNS_PRESENTATION_MODE = 4
            matches!(state, 2 | 3 | 4)
        }
    }

    pub fn on_battery() -> bool {
        unsafe {
            let mut s: SYSTEM_POWER_STATUS = std::mem::zeroed();
            GetSystemPowerStatus(&mut s) != 0 && s.ACLineStatus == 0
        }
    }

    pub fn idle_seconds() -> Option<f64> {
        unsafe {
            let mut info = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
            if GetLastInputInfo(&mut info) == 0 {
                return None;
            }
            Some(GetTickCount().wrapping_sub(info.dwTime) as f64 / 1000.0)
        }
    }

    pub fn load_average() -> Option<f64> {
        None
    }
}

// ---------------------------------------------------------------- Unix
#[cfg(unix)]
mod imp {
    use super::*;

    pub fn set_priority(_idle: bool) {
        unsafe {
            libc::setpriority(libc::PRIO_PROCESS, 0, 19);
        }
    }

    /// This process's CPU seconds plus its finished children's (ffmpeg,
    /// yt-dlp), so --duty also counts the work they did.
    pub fn cpu_seconds() -> f64 {
        let get = |who| unsafe {
            let mut r: libc::rusage = std::mem::zeroed();
            if libc::getrusage(who, &mut r) != 0 {
                return 0.0;
            }
            let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
            tv(r.ru_utime) + tv(r.ru_stime)
        };
        get(libc::RUSAGE_SELF) + get(libc::RUSAGE_CHILDREN)
    }

    /// The calling thread's CPU seconds.
    pub fn thread_cpu_seconds() -> f64 {
        unsafe {
            let mut r: libc::rusage = std::mem::zeroed();
            if libc::getrusage(libc::RUSAGE_THREAD, &mut r) != 0 {
                return 0.0;
            }
            let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
            tv(r.ru_utime) + tv(r.ru_stime)
        }
    }

    pub fn init_family() {}

    /// This process and its finished children.
    pub fn family_cpu_seconds() -> f64 {
        cpu_seconds()
    }

    pub fn system_ticks() -> Option<(u64, u64)> {
        let text = std::fs::read_to_string("/proc/stat").ok()?;
        let line = text.lines().next()?;
        let v: Vec<u64> = line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
        if v.len() < 4 {
            return None;
        }
        let total: u64 = v.iter().take(8).sum();
        let idle = v[3] + v.get(4).copied().unwrap_or(0);
        Some((total - idle, total))
    }

    pub fn local_parts() -> (u16, u16, u16, u16, u16, u16) {
        unsafe {
            let t = libc::time(std::ptr::null_mut());
            let mut tm: libc::tm = std::mem::zeroed();
            libc::localtime_r(&t, &mut tm);
            (
                (tm.tm_year + 1900) as u16,
                (tm.tm_mon + 1) as u16,
                tm.tm_mday as u16,
                tm.tm_hour as u16,
                tm.tm_min as u16,
                tm.tm_sec as u16,
            )
        }
    }

    extern "C" fn on_signal(_: libc::c_int) {
        STOP_SIGNAL.store(true, Ordering::Relaxed);
    }

    pub fn install_stop_handler() {
        unsafe {
            libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
            libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
        }
    }

    /// The pid file is the lock; the caller writes it.
    pub struct Instance;
    pub fn single_instance() -> Option<Instance> {
        Some(Instance)
    }

    pub fn fullscreen() -> bool {
        false
    }
    pub fn on_battery() -> bool {
        // AC adapters show up as power_supply entries of type Mains.
        let Ok(dir) = std::fs::read_dir("/sys/class/power_supply") else { return false };
        let mut mains = false;
        for e in dir.flatten() {
            let p = e.path();
            if std::fs::read_to_string(p.join("type")).map(|t| t.trim() == "Mains").unwrap_or(false) {
                mains = true;
                if std::fs::read_to_string(p.join("online")).map(|t| t.trim() == "1").unwrap_or(true) {
                    return false;
                }
            }
        }
        mains
    }
    pub fn idle_seconds() -> Option<f64> {
        None
    }
    pub fn load_average() -> Option<f64> {
        std::fs::read_to_string("/proc/loadavg").ok()?.split_whitespace().next()?.parse().ok()
    }

    /// Opens and locks the pid file; None while another worker holds it.
    /// The lock goes with the process, so a crash leaves nothing stuck.
    pub fn lock_pid_file(path: &std::path::Path) -> Option<std::fs::File> {
        use std::os::unix::io::AsRawFd;
        let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path).ok()?;
        let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        (r == 0).then_some(f)
    }

    /// Whether `pid` runs this same program (so a stale pid file can't
    /// make `stop` signal something else).
    pub fn is_our_program(pid: i32) -> bool {
        let Ok(theirs) = std::fs::read_link(format!("/proc/{pid}/exe")) else { return false };
        let theirs = theirs.to_string_lossy().trim_end_matches(" (deleted)").to_string();
        let Ok(ours) = std::env::current_exe() else { return false };
        let ours = std::fs::canonicalize(&ours).unwrap_or(ours);
        let name = |p: &str| std::path::Path::new(p).file_name().map(|n| n.to_owned());
        std::path::Path::new(&theirs) == ours || name(&theirs) == name(&ours.to_string_lossy())
    }

    pub fn process_alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }
    pub fn terminate(pid: i32) -> bool {
        unsafe { libc::kill(pid, libc::SIGTERM) == 0 }
    }
}

pub use imp::*;

pub fn local_date() -> String {
    let (y, m, d, ..) = local_parts();
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn local_stamp() -> String {
    let (y, mo, d, h, mi, s) = local_parts();
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}
