//! OS-level real-time thread hardening, wake signaling (`eventfd`/`pipe`), and `timerfd`.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

#[cfg(target_os = "linux")]
use tokio::io::unix::AsyncFd;
#[cfg(target_os = "linux")]
use tokio::io::Interest;

/// Restrict the calling thread (and child threads it subsequently spawns) to non-RT CPU cores,
/// excluding `excluded_core` when at least 2 online cores exist.
#[cfg(target_os = "linux")]
pub fn pin_non_rt_threads(excluded_core: usize) -> Result<usize, i32> {
    let online = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    if online <= 1 {
        return Ok(online.max(0) as usize);
    }
    let capped_online = (online as usize).min(libc::CPU_SETSIZE as usize);
    if capped_online <= 1 {
        return Ok(capped_online);
    }
    let excluded = excluded_core.min(capped_online - 1);

    let mut cpuset: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::CPU_ZERO(&mut cpuset);
        for core in 0..capped_online {
            if core != excluded {
                libc::CPU_SET(core, &mut cpuset);
            }
        }
    }
    let rc = unsafe {
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &raw const cpuset)
    };
    if rc == 0 {
        Ok(capped_online - 1)
    } else {
        Err(io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EPERM))
    }
}

/// Non-Linux stub for [`pin_non_rt_threads`].
#[cfg(not(target_os = "linux"))]
pub fn pin_non_rt_threads(_excluded_core: usize) -> Result<usize, i32> {
    Ok(0)
}

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

    /// Extract the owned descriptor for registration with `tokio::io::unix::AsyncFd`.
    #[cfg(target_os = "linux")]
    #[must_use]
    pub fn into_owned_fd(self) -> OwnedFd {
        self.fd
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

/// Async-side wrapper around [`WakeReceiver`] waking `tokio` immediately on RT `eventfd` signals.
#[cfg(target_os = "linux")]
pub struct AsyncWakeReceiver {
    fd: AsyncFd<OwnedFd>,
}

#[cfg(target_os = "linux")]
impl AsyncWakeReceiver {
    /// Register the RT notification descriptor with `tokio`'s reactor (must run inside `tokio`).
    pub fn new(rx: WakeReceiver) -> io::Result<Self> {
        // SAFETY: `rx.into_owned_fd()` transfers an open `OwnedFd` whose descriptor remains valid
        // and owned exclusively by `AsyncFd` for its entire lifetime.
        let fd =
            unsafe { AsyncFd::register_with_interest(rx.into_owned_fd(), Interest::READABLE)? };
        Ok(Self { fd })
    }

    /// Wait until at least one RT notification arrives, draining `eventfd` without spinning.
    pub async fn wait(&self) -> io::Result<()> {
        loop {
            let mut guard = self.fd.readable().await?;
            let res = guard.try_io(|inner| {
                let mut drained = false;
                let mut buf = [0u8; 8];
                loop {
                    let n = unsafe {
                        libc::read(
                            inner.get_ref().as_raw_fd(),
                            buf.as_mut_ptr().cast(),
                            buf.len(),
                        )
                    };
                    if n > 0 {
                        drained = true;
                        continue;
                    }
                    if n == 0 {
                        return Ok(drained);
                    }
                    let err = io::Error::last_os_error();
                    if err.kind() == io::ErrorKind::WouldBlock {
                        return if drained { Ok(true) } else { Err(err) };
                    }
                    if err.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(err);
                }
            });

            match res {
                Ok(Ok(true)) => {
                    guard.clear_ready();
                    return Ok(());
                }
                Ok(Ok(false)) => continue,
                Ok(Err(err)) => return Err(err),
                Err(_would_block) => continue,
            }
        }
    }

    /// No-op on Linux (`wait()` drains `eventfd` directly).
    pub fn drain_fallback(&self) {}
}

/// Non-Linux async wakeup fallback preserving the pipe read end while relying on fallback polling.
#[cfg(not(target_os = "linux"))]
pub struct AsyncWakeReceiver {
    rx: WakeReceiver,
}

#[cfg(not(target_os = "linux"))]
impl AsyncWakeReceiver {
    /// Wrap the wake receiver (keeps the pipe read end alive so RT writes never hit `SIGPIPE`).
    pub fn new(rx: WakeReceiver) -> io::Result<Self> {
        Ok(Self { rx })
    }

    /// Pending future on non-Linux hosts; `control.rs` drains notifications on fallback ticks.
    pub async fn wait(&self) -> io::Result<()> {
        std::future::pending().await
    }

    /// Drain pending bytes from the pipe on fallback ticks.
    pub fn drain_fallback(&self) {
        self.rx.drain();
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
