//! The process a daemon belongs to, when its starter names one.
//!
//! A daemon started by a build outlives that build on purpose, and with the
//! default idle timeout of zero it runs until someone stops it. A test suite
//! cannot rely on stopping it: a test killed by a timeout runs no `Drop`, and a
//! test whose runtime directory stays (a fixed scratch folder, a private
//! `HOME`) never removes the socket that [`super::BoundSocket`] watches. Such a
//! daemon ran for a day on a build machine, its store open, re-parented to
//! init.
//!
//! `KACHE_DAEMON_OWNER_PID=<pid>` names the process the daemon belongs to. The
//! variable is inherited down the chain (test, cargo, wrapper, the daemon the
//! wrapper auto-starts), so every daemon a process starts, however indirectly,
//! drains on the heartbeat after that process ends. Unset, nothing changes: the
//! service-managed and auto-started daemons of real builds keep running as
//! before.

/// The variable that names the owning process.
pub(crate) const OWNER_PID_ENV: &str = "KACHE_DAEMON_OWNER_PID";

/// A handle on the owning process, taken when the daemon starts, so a
/// recycled process ID never reads as the owner still running.
#[derive(Debug)]
pub(crate) struct DaemonOwner {
    pid: u32,
    handle: OwnerHandle,
}

#[derive(Debug)]
enum OwnerHandle {
    /// The owner had already ended when the daemon started.
    Gone,
    /// Linux: a pidfd, readable once the process has exited.
    #[cfg(target_os = "linux")]
    PidFd(std::os::fd::OwnedFd),
    /// Other Unix systems: probed by `kill(pid, 0)`.
    #[cfg(unix)]
    Probe,
    /// Windows: a process handle with SYNCHRONIZE access.
    #[cfg(windows)]
    Process(windows_sys::Win32::Foundation::HANDLE),
}

// SAFETY: a Win32 process handle may be waited on and closed from any thread.
#[cfg(windows)]
unsafe impl Send for OwnerHandle {}
#[cfg(windows)]
unsafe impl Sync for OwnerHandle {}

impl DaemonOwner {
    /// The owner the environment names, or `None` when it names none. A
    /// value that is not a process ID is ignored with a warning: a daemon
    /// must not stop because of a typo, nor run on unowned without a word.
    pub(crate) fn from_env() -> Option<Self> {
        let value = std::env::var(OWNER_PID_ENV).ok()?;
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        match value.parse::<u32>() {
            Ok(pid) if pid > 0 => Some(Self::watch(pid)),
            _ => {
                tracing::warn!("{OWNER_PID_ENV}={value:?} is not a process ID; ignoring it");
                None
            }
        }
    }

    /// Start watching `pid`.
    pub(crate) fn watch(pid: u32) -> Self {
        Self {
            pid,
            handle: OwnerHandle::open(pid),
        }
    }

    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    /// Whether the owner has ended. A probe that fails for any reason other
    /// than "no such process" keeps the daemon running.
    pub(crate) fn is_gone(&self) -> bool {
        self.handle.is_gone(self.pid)
    }
}

impl OwnerHandle {
    #[cfg(target_os = "linux")]
    fn open(pid: u32) -> Self {
        use std::os::fd::{FromRawFd as _, OwnedFd};
        // SAFETY: pidfd_open takes a pid and flags and returns a new fd or -1.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
        if fd >= 0 {
            // SAFETY: the kernel just returned this descriptor to us alone.
            return Self::PidFd(unsafe { OwnedFd::from_raw_fd(fd as i32) });
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => Self::Gone,
            // Kernels before 5.3, or a seccomp filter: fall back to probing.
            _ => Self::Probe,
        }
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    fn open(pid: u32) -> Self {
        if unix_process_is_gone(pid) {
            Self::Gone
        } else {
            Self::Probe
        }
    }

    #[cfg(windows)]
    fn open(pid: u32) -> Self {
        use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, GetLastError};
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE};
        // SAFETY: OpenProcess takes plain values and returns a handle or null.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if !handle.is_null() {
            return Self::Process(handle);
        }
        // SAFETY: reads this thread's last error.
        if unsafe { GetLastError() } == ERROR_INVALID_PARAMETER {
            // No process has this ID.
            Self::Gone
        } else {
            // Access denied or another failure proves nothing.
            Self::Process(std::ptr::null_mut())
        }
    }

    fn is_gone(&self, pid: u32) -> bool {
        let _ = pid;
        match self {
            Self::Gone => true,
            #[cfg(target_os = "linux")]
            Self::PidFd(fd) => {
                use std::os::fd::AsRawFd as _;
                let mut poll = libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: one valid pollfd, no wait.
                let ready = unsafe { libc::poll(&mut poll, 1, 0) };
                ready > 0 && poll.revents & libc::POLLIN != 0
            }
            #[cfg(unix)]
            Self::Probe => unix_process_is_gone(pid),
            #[cfg(windows)]
            Self::Process(handle) => {
                use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
                use windows_sys::Win32::System::Threading::WaitForSingleObject;
                if handle.is_null() {
                    return false;
                }
                // SAFETY: a handle we opened with SYNCHRONIZE; zero wait.
                unsafe { WaitForSingleObject(*handle, 0) == WAIT_OBJECT_0 }
            }
        }
    }
}

#[cfg(windows)]
impl Drop for OwnerHandle {
    fn drop(&mut self) {
        if let Self::Process(handle) = self
            && !handle.is_null()
        {
            // SAFETY: a handle we opened and close once.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(*handle) };
        }
    }
}

#[cfg(unix)]
fn unix_process_is_gone(pid: u32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    let alive = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
    !alive && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_owner_is_gone_only_once_its_process_has_ended() {
        assert!(!DaemonOwner::watch(std::process::id()).is_gone());

        let mut child = if cfg!(windows) {
            std::process::Command::new("cmd")
                .args(["/C", "ping -n 30 127.0.0.1 >NUL"])
                .spawn()
        } else {
            std::process::Command::new("sleep").arg("30").spawn()
        }
        .expect("spawning a stand-in owner");
        let owner = DaemonOwner::watch(child.id());
        assert!(!owner.is_gone(), "a running owner read as gone");
        child.kill().unwrap();
        child.wait().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !owner.is_gone() {
            assert!(
                std::time::Instant::now() < deadline,
                "an ended owner still read as running"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // An owner that ended before the daemon started.
        assert!(DaemonOwner::watch(child.id()).is_gone());
    }
}
