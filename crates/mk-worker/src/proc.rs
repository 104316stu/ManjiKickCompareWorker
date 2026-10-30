//! Running a helper (yt-dlp) with a deadline and a cancel check, and killing
//! its whole tree when either trips: the yt-dlp builds are self-extracting
//! and run their real work in a child process.

use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub enum RunError {
    Spawn(String),
    Timeout,
    Cancelled,
}

/// No console window, below-normal priority (inherited), own process group.
pub fn quiet(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.stdin(std::process::Stdio::null())
}

#[cfg(windows)]
struct Tree(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Tree {
    fn new(child: &Child) -> Tree {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::*;
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if !job.is_null() {
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                AssignProcessToJobObject(job, child.as_raw_handle() as _);
            }
            Tree(job)
        }
    }
    /// CPU seconds of every process that ran in the tree.
    fn cpu_seconds(&self) -> f64 {
        crate::platform::job_cpu_seconds(self.0)
    }
    fn kill(&self, child: &mut Child) {
        if !self.0.is_null() {
            unsafe {
                windows_sys::Win32::System::JobObjects::TerminateJobObject(self.0, 1);
            }
        }
        let _ = child.kill();
    }
}

#[cfg(windows)]
impl Drop for Tree {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(self.0);
            }
        }
    }
}

#[cfg(unix)]
struct Tree(i32);

#[cfg(unix)]
impl Tree {
    fn new(child: &Child) -> Tree {
        Tree(child.id() as i32)
    }
    fn kill(&self, _child: &mut Child) {
        unsafe {
            libc::kill(-self.0, libc::SIGKILL);
        }
    }
}

/// Runs `cmd` to the end, the deadline or `cancel()`. Also returns the CPU
/// seconds the helper (and what it started) used, for --duty.
#[cfg(windows)]
pub fn run(cmd: &mut Command, deadline: Instant, cancel: &dyn Fn() -> bool) -> (Result<ExitStatus, RunError>, f64) {
    let mut child = match quiet(cmd).spawn() {
        Ok(c) => c,
        Err(e) => return (Err(RunError::Spawn(e.to_string())), 0.0),
    };
    let tree = Tree::new(&child);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return (Ok(status), tree.cpu_seconds()),
            Ok(None) => {}
            Err(e) => {
                tree.kill(&mut child);
                return (Err(RunError::Spawn(e.to_string())), tree.cpu_seconds());
            }
        }
        let timed_out = Instant::now() >= deadline;
        if timed_out || cancel() {
            tree.kill(&mut child);
            let _ = child.wait();
            let cpu = tree.cpu_seconds();
            return (Err(if timed_out { RunError::Timeout } else { RunError::Cancelled }), cpu);
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

#[cfg(unix)]
pub fn run(cmd: &mut Command, deadline: Instant, cancel: &dyn Fn() -> bool) -> (Result<ExitStatus, RunError>, f64) {
    use std::os::unix::process::ExitStatusExt;
    let mut child = match quiet(cmd).spawn() {
        Ok(c) => c,
        Err(e) => return (Err(RunError::Spawn(e.to_string())), 0.0),
    };
    let tree = Tree::new(&child);
    let pid = child.id() as libc::pid_t;
    // wait4 rather than Child::try_wait: it also reports the CPU the helper
    // and its waited-for children used.
    let reap = |flags: libc::c_int| -> Option<(ExitStatus, f64)> {
        let mut status = 0;
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        let r = unsafe { libc::wait4(pid, &mut status, flags, &mut ru) };
        if r == pid {
            let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
            Some((ExitStatus::from_raw(status), tv(ru.ru_utime) + tv(ru.ru_stime)))
        } else {
            None
        }
    };
    loop {
        if let Some((status, cpu)) = reap(libc::WNOHANG) {
            return (Ok(status), cpu);
        }
        let timed_out = Instant::now() >= deadline;
        if timed_out || cancel() {
            tree.kill(&mut child);
            let cpu = reap(0).map(|(_, c)| c).unwrap_or(0.0);
            return (Err(if timed_out { RunError::Timeout } else { RunError::Cancelled }), cpu);
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// Starts a program that must outlive this one (the relaunched app, the
/// browser, a cleanup helper): outside the job object that otherwise ends
/// everything we started when we exit. `extra` adds creation flags.
pub fn spawn_outside(cmd: &mut Command, extra: u32) -> std::io::Result<Child> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        cmd.creation_flags(extra | CREATE_BREAKAWAY_FROM_JOB);
        match cmd.spawn() {
            Ok(c) => Ok(c),
            // Inside someone else's job that forbids it: start normally.
            Err(_) => {
                cmd.creation_flags(extra);
                cmd.spawn()
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = extra;
        cmd.spawn()
    }
}
