//! OS-level real-time thread hardening, wake signaling (`eventfd`/`pipe`), and `timerfd`.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// Result of attempting to apply `SCHED_FIFO`, `mlockall`, and CPU core affinity on the RT thread.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RtHardeningReport {
    pub pinned_core: Option<usize>,
    pub sched_fifo_prio: Option<i32>,
    pub mlockall_ok: bool,
    pub affinity_errno: Option<i32>,
    pub sched_errno: Option<i32>,
    pub mlock_errno: Option<i32>,
}

/// Apply Linux real-time thread hardening (`SCHED_FIFO`, `mlockall`, CPU core affinity).
///
/// Failures (such as `EPERM` when running unprivileged outside `systemd`) are recorded in the
/// returned report rather than panicking so host/dev runs degrade gracefully.
#[cfg(target_os = "linux")]
#[must_use]
pub fn harden_current_thread(preferred_core: usize, fifo_prio: i32) -> RtHardeningReport {
    let mut report = RtHardeningReport::default();

    let online = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    let max_core = if online > 0 {
        (online as usize).saturating_sub(1)
    } else {
        0
    };
    let target_core = preferred_core.min(max_core);

    let mut cpuset: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::CPU_ZERO(&mut cpuset);
        libc::CPU_SET(target_core, &mut cpuset);
    }
    let aff_rc = unsafe {
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &raw const cpuset)
    };
    if aff_rc == 0 {
        report.pinned_core = Some(target_core);
    } else {
        report.affinity_errno = io::Error::last_os_error().raw_os_error();
    }

    let mlock_rc = unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) };
    if mlock_rc == 0 {
        report.mlockall_ok = true;
    } else {
        report.mlock_errno = io::Error::last_os_error().raw_os_error();
    }

    let mut param: libc::sched_param = unsafe { std::mem::zeroed() };
    param.sched_priority = fifo_prio;
    let sched_rc = unsafe {
        libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &raw const param)
    };
    if sched_rc == 0 {
        report.sched_fifo_prio = Some(fifo_prio);
    } else {
        report.sched_errno = Some(sched_rc);
    }

    report
}

/// No-op RT hardening stub on non-Linux development hosts (e.g. macOS).
#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn harden_current_thread(_preferred_core: usize, _fifo_prio: i32) -> RtHardeningReport {
    RtHardeningReport::default()
}

/// Async-side sender used to wake the RT `poll()` loop without blocking.
pub struct WakeSender {
    fd: OwnedFd,
}

/// RT-side receiver polled in `poll()` and drained without allocations.
pub struct WakeReceiver {
    fd: OwnedFd,
}

/// Create a non-blocking cross-thread wakeup pair (`eventfd` on Linux, `pipe` on macOS).
pub fn create_wake_pair() -> io::Result<(WakeSender, WakeReceiver)> {
    #[cfg(target_os = "linux")]
    {
        let efd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if efd < 0 {
            return Err(io::Error::last_os_error());
        }
        let rx_fd = unsafe { OwnedFd::from_raw_fd(efd) };
        let tx_raw = unsafe { libc::fcntl(rx_fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if tx_raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let tx_fd = unsafe { OwnedFd::from_raw_fd(tx_raw) };
        Ok((WakeSender { fd: tx_fd }, WakeReceiver { fd: rx_fd }))
    }

    #[cfg(not(target_os = "linux"))]
    {
        let mut fds = [-1 as RawFd; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let rx_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let tx_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        set_nonblocking_cloexec(rx_fd.as_raw_fd())?;
        set_nonblocking_cloexec(tx_fd.as_raw_fd())?;
        Ok((WakeSender { fd: tx_fd }, WakeReceiver { fd: rx_fd }))
    }
}

#[cfg(not(target_os = "linux"))]
fn set_nonblocking_cloexec(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if fd_flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, fd_flags | libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl WakeSender {
    /// Signal the RT thread to wake up immediately.
    pub fn wake(&self) {
        #[cfg(target_os = "linux")]
        let payload = 1u64.to_ne_bytes();
        #[cfg(not(target_os = "linux"))]
        let payload = [1u8; 1];

        let _ = unsafe { libc::write(self.fd.as_raw_fd(), payload.as_ptr().cast(), payload.len()) };
    }
}

impl WakeReceiver {
    /// Raw descriptor to include in `poll()`.
    #[must_use]
    pub fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Drain all pending wakeup notifications without blocking or allocating.
    pub fn drain(&self) {
        #[cfg(target_os = "linux")]
        let mut buf = [0u8; 8];
        #[cfg(not(target_os = "linux"))]
        let mut buf = [0u8; 64];

        loop {
            let n = unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break;
            }
        }
    }
}

/// Linux `CLOCK_MONOTONIC` non-blocking `timerfd` wrapper for `TFD_TIMER_ABSTIME` sleeps.
#[cfg(target_os = "linux")]
pub struct TimerFd {
    fd: OwnedFd,
}

#[cfg(target_os = "linux")]
impl TimerFd {
    /// Create a non-blocking `CLOCK_MONOTONIC` `timerfd`.
    pub fn new() -> io::Result<Self> {
        let fd = unsafe {
            libc::timerfd_create(
                libc::CLOCK_MONOTONIC,
                libc::TFD_NONBLOCK | libc::TFD_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
        })
    }

    /// Raw descriptor to include in `poll()`.
    #[must_use]
    pub fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Arm a one-shot absolute deadline (`CLOCK_MONOTONIC` nanoseconds).
    pub fn arm_abstime_ns(&self, abs_mono_ns: i64) -> io::Result<()> {
        let spec = crate::clock::abs_ns_to_itimerspec(abs_mono_ns);
        let rc = unsafe {
            libc::timerfd_settime(
                self.fd.as_raw_fd(),
                libc::TFD_TIMER_ABSTIME,
                &raw const spec,
                std::ptr::null_mut(),
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Drain timer expirations without blocking or allocating.
    pub fn drain(&self) {
        let mut buf = [0u8; 8];
        loop {
            let n = unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wake_pair_signals_and_drains_cleanly() {
        let (tx, rx) = create_wake_pair().expect("create wake pair");
        tx.wake();
        tx.wake();

        let mut pfd = libc::pollfd {
            fd: rx.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&raw mut pfd, 1, 10) };
        assert_eq!(ready, 1);
        assert_ne!(pfd.revents & libc::POLLIN, 0);

        rx.drain();
        pfd.revents = 0;
        let after = unsafe { libc::poll(&raw mut pfd, 1, 0) };
        assert_eq!(after, 0);
    }
}
