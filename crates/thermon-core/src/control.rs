//! Signalling and renicing processes.
//!
//! A PID can be reused between showing a process and acting on it, so callers
//! pass the `start_ticks` they showed and nothing happens if it doesn't match.

use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;

use crate::processes::parse_pid_stat;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// SIGTERM: ask it to quit.
    Term,
    /// SIGKILL: stop it immediately.
    Kill,
}

impl Signal {
    fn raw(self) -> libc::c_int {
        match self {
            Signal::Term => libc::SIGTERM,
            Signal::Kill => libc::SIGKILL,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Signal::Term => "SIGTERM",
            Signal::Kill => "SIGKILL",
        }
    }
}

#[derive(Debug)]
pub enum ControlError {
    /// PID 0, 1, or out of range: never a sensible target.
    Refused(u32),
    /// Gone before we got to it.
    NoSuchProcess(u32),
    /// The PID now belongs to a different process than the one shown.
    Reused(u32),
    Io(io::Error),
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ControlError::Refused(pid) => write!(f, "refusing to touch pid {pid}"),
            ControlError::NoSuchProcess(pid) => write!(f, "process {pid} has already exited"),
            ControlError::Reused(pid) => {
                write!(
                    f,
                    "pid {pid} now belongs to a different process; nothing was done"
                )
            }
            ControlError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ControlError {}

fn check_pid(pid: u32) -> Result<libc::pid_t, ControlError> {
    match libc::pid_t::try_from(pid) {
        Ok(p) if p > 1 => Ok(p),
        _ => Err(ControlError::Refused(pid)),
    }
}

fn start_ticks_of(root: &Path, pid: u32) -> Result<u64, ControlError> {
    let bytes = fs::read(root.join(format!("proc/{pid}/stat")))
        .map_err(|_| ControlError::NoSuchProcess(pid))?;
    let stat = parse_pid_stat(&String::from_utf8_lossy(&bytes)).map_err(ControlError::Io)?;
    Ok(stat.start_ticks)
}

fn verify(pid: u32, expected: Option<u64>) -> Result<(), ControlError> {
    if let Some(expected) = expected
        && start_ticks_of(Path::new("/"), pid)? != expected
    {
        return Err(ControlError::Reused(pid));
    }
    Ok(())
}

fn os_error(pid: u32) -> ControlError {
    let e = io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::ESRCH) {
        ControlError::NoSuchProcess(pid)
    } else {
        ControlError::Io(e)
    }
}

/// Send `sig` to `pid`, but only if it is still the process that started at
/// `start_ticks` (when given).
pub fn signal(pid: u32, start_ticks: Option<u64>, sig: Signal) -> Result<(), ControlError> {
    let raw_pid = check_pid(pid)?;
    // SAFETY: plain syscall; returns a new fd or -1.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, raw_pid, 0) };
    if fd < 0 {
        return Err(os_error(pid));
    }
    // SAFETY: the syscall just returned this fd and nothing else owns it.
    let pidfd = unsafe { OwnedFd::from_raw_fd(fd as libc::c_int) };
    // The pidfd pins the process, so checking after opening it leaves no
    // window for reuse.
    verify(pid, start_ticks)?;
    // SAFETY: valid pidfd, no siginfo.
    let r = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            sig.raw(),
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if r < 0 { Err(os_error(pid)) } else { Ok(()) }
}

/// How a renice went across a process's threads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReniceReport {
    pub changed: usize,
    pub failed: usize,
    /// First error seen, for display.
    pub error: Option<String>,
}

/// Set the nice value of every thread of `pid`. Nice is per thread on Linux,
/// so renicing only the main thread would leave the workers untouched.
pub fn renice(pid: u32, start_ticks: Option<u64>, nice: i32) -> Result<ReniceReport, ControlError> {
    check_pid(pid)?;
    verify(pid, start_ticks)?;
    let tasks =
        fs::read_dir(format!("/proc/{pid}/task")).map_err(|_| ControlError::NoSuchProcess(pid))?;
    let mut report = ReniceReport {
        changed: 0,
        failed: 0,
        error: None,
    };
    for entry in tasks.flatten() {
        let Some(tid) = entry
            .file_name()
            .to_str()
            .and_then(|t| t.parse::<libc::id_t>().ok())
        else {
            continue;
        };
        // SAFETY: plain syscall with integer arguments.
        if unsafe { libc::setpriority(libc::PRIO_PROCESS, tid, nice) } == 0 {
            report.changed += 1;
        } else {
            let e = io::Error::last_os_error();
            // A thread that exited meanwhile isn't a failure.
            if e.raw_os_error() != Some(libc::ESRCH) {
                report.failed += 1;
                report.error.get_or_insert_with(|| e.to_string());
            }
        }
    }
    if report.changed == 0 && report.failed == 0 {
        return Err(ControlError::NoSuchProcess(pid));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Command;

    fn nice_of(tid: u32, pid: u32) -> i32 {
        let stat = fs::read_to_string(format!("/proc/{pid}/task/{tid}/stat")).unwrap();
        parse_pid_stat(&stat).unwrap().nice
    }

    #[test]
    fn refuses_init_and_zero() {
        assert!(matches!(
            signal(0, None, Signal::Term),
            Err(ControlError::Refused(0))
        ));
        assert!(matches!(
            signal(1, None, Signal::Term),
            Err(ControlError::Refused(1))
        ));
        assert!(matches!(
            signal(u32::MAX, None, Signal::Term),
            Err(ControlError::Refused(_))
        ));
    }

    #[test]
    fn wrong_start_time_is_not_signalled() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let start = start_ticks_of(Path::new("/"), pid).unwrap();
        assert!(matches!(
            signal(pid, Some(start + 1), Signal::Term),
            Err(ControlError::Reused(_))
        ));
        // Still alive; the right identity works.
        signal(pid, Some(start), Signal::Term).unwrap();
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGTERM));
        assert!(matches!(
            signal(pid, Some(start), Signal::Term),
            Err(ControlError::NoSuchProcess(_))
        ));
    }

    #[test]
    fn renice_reaches_every_thread() {
        // A process with worker threads.
        let mut child = Command::new("python3")
            .args([
                "-c",
                "import threading,time\n[threading.Thread(target=time.sleep,args=(30,),daemon=True).start() for _ in range(3)]\ntime.sleep(30)",
            ])
            .spawn()
            .unwrap();
        let pid = child.id();
        let mut threads = 0;
        for _ in 0..50 {
            threads = fs::read_dir(format!("/proc/{pid}/task")).unwrap().count();
            if threads >= 4 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(threads >= 4, "python threads didn't start");
        let report = renice(pid, None, 7).unwrap();
        assert!(report.changed >= 4 && report.failed == 0, "{report:?}");
        for t in fs::read_dir(format!("/proc/{pid}/task")).unwrap().flatten() {
            let tid: u32 = t.file_name().to_str().unwrap().parse().unwrap();
            assert_eq!(nice_of(tid, pid), 7, "thread {tid}");
        }
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
