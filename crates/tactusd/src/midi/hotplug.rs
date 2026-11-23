//! USB/ALSA sound hotplug monitoring via `NETLINK_KOBJECT_UEVENT` (no `libudev`).

/// Return `true` if a raw netlink uevent packet belongs to `SUBSYSTEM=sound`.
#[cfg(any(target_os = "linux", test))]
#[must_use]
pub fn is_sound_uevent(payload: &[u8]) -> bool {
    payload
        .split(|&byte| byte == 0)
        .any(|entry| entry == b"SUBSYSTEM=sound")
}

#[cfg(target_os = "linux")]
mod imp {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    use tokio::io::unix::AsyncFd;
    use tokio::io::Interest;

    use super::is_sound_uevent;

    /// Non-blocking Linux `NETLINK_KOBJECT_UEVENT` listener filtered for `SUBSYSTEM=sound`.
    pub struct UeventMonitor {
        fd: AsyncFd<OwnedFd>,
    }

    impl UeventMonitor {
        /// Bind a `NETLINK_KOBJECT_UEVENT` broadcast listener (requires no root privileges).
        pub fn new() -> io::Result<Self> {
            let sock = unsafe {
                libc::socket(
                    libc::AF_NETLINK,
                    libc::SOCK_DGRAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                    libc::NETLINK_KOBJECT_UEVENT,
                )
            };
            if sock < 0 {
                return Err(io::Error::last_os_error());
            }
            let owned = unsafe { OwnedFd::from_raw_fd(sock) };

            let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
            addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
            addr.nl_pid = 0;
            addr.nl_groups = 1; // kernel uevent multicast group

            let bind_rc = unsafe {
                libc::bind(
                    owned.as_raw_fd(),
                    (&raw const addr).cast::<libc::sockaddr>(),
                    std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
                )
            };
            if bind_rc < 0 {
                return Err(io::Error::last_os_error());
            }

            // SAFETY: `owned` is an open `OwnedFd` whose underlying descriptor remains open and
            // invariant for the full lifetime of `AsyncFd`.
            let fd = unsafe { AsyncFd::register_with_interest(owned, Interest::READABLE)? };
            Ok(Self { fd })
        }

        /// Wait until at least one `SUBSYSTEM=sound` uevent (or `ENOBUFS` overflow) is received.
        pub async fn wait_sound_event(&self) -> io::Result<()> {
            loop {
                let mut guard = self.fd.readable().await?;
                let res = guard.try_io(|inner| {
                    let mut saw_sound = false;
                    let mut buf = [0u8; 4096];
                    loop {
                        let n = unsafe {
                            libc::recv(
                                inner.get_ref().as_raw_fd(),
                                buf.as_mut_ptr().cast(),
                                buf.len(),
                                0,
                            )
                        };
                        if n > 0 {
                            if is_sound_uevent(&buf[..n as usize]) {
                                saw_sound = true;
                            }
                            continue;
                        }
                        if n == 0 {
                            return Ok(saw_sound);
                        }
                        let err = io::Error::last_os_error();
                        if err.kind() == io::ErrorKind::WouldBlock {
                            return if saw_sound { Ok(true) } else { Err(err) };
                        }
                        if err.kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        if err.raw_os_error() == Some(libc::ENOBUFS) {
                            return Ok(true);
                        }
                        return Err(err);
                    }
                });

                match res {
                    Ok(Ok(true)) => return Ok(()),
                    Ok(Ok(false)) => continue,
                    Ok(Err(err)) => return Err(err),
                    Err(_would_block) => continue,
                }
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use std::io;

    /// Non-Linux hotplug stub (relies on periodic rescan).
    pub struct UeventMonitor;

    impl UeventMonitor {
        pub fn new() -> io::Result<Self> {
            Ok(Self)
        }

        pub async fn wait_sound_event(&self) -> io::Result<()> {
            std::future::pending().await
        }
    }
}

pub use imp::UeventMonitor;

#[cfg(test)]
mod tests {
    use super::is_sound_uevent;

    #[test]
    fn parses_nul_delimited_sound_uevent_payloads() {
        let sound_add = b"add@/devices/platform/usb/sound/card1/midiC1D0\0ACTION=add\0SUBSYSTEM=sound\0DEVNAME=snd/midiC1D0\0";
        assert!(is_sound_uevent(sound_add));

        let sound_remove =
            b"remove@/devices/platform/usb/sound/card1\0ACTION=remove\0SUBSYSTEM=sound\0";
        assert!(is_sound_uevent(sound_remove));

        let usb_only = b"add@/devices/platform/usb\0ACTION=add\0SUBSYSTEM=usb\0";
        assert!(!is_sound_uevent(usb_only));

        let partial = b"SUBSYSTEM=sound_card\0";
        assert!(!is_sound_uevent(partial));

        assert!(!is_sound_uevent(b""));
    }
}
