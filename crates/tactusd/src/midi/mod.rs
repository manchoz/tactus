//! ALSA MIDI backend abstraction, port matching, FD lifecycle helpers, and hotplug.

pub mod hotplug;
#[cfg(target_os = "linux")]
pub mod rawmidi;
pub mod rawmidi_uapi;

use std::io;
use std::os::fd::RawFd;
use std::sync::Arc;

use tactus_protocol::MidiPortInfo;

/// Strictly `Copy` set of opened non-blocking MIDI file descriptors handed to the RT thread.
///
/// When input and output target the same full-duplex subdevice on ALSA RawMIDI, `in_fd == out_fd`
/// (opened once with `O_RDWR | O_NONBLOCK | O_CLOEXEC`). Always release via [`close_fd_set`] so
/// shared descriptors are never double-closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MidiFdSet {
    pub in_fd: RawFd,
    pub out_fd: RawFd,
    pub generation: u64,
}

impl MidiFdSet {
    /// Empty descriptor set (`in_fd = -1`, `out_fd = -1`).
    pub const EMPTY: Self = Self {
        in_fd: -1,
        out_fd: -1,
        generation: 0,
    };

    /// Return `true` when neither input nor output file descriptor is active.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.in_fd < 0 && self.out_fd < 0
    }
}

/// Safely close any active file descriptors in `fds` outside the RT thread,
/// deduplicating when `in_fd == out_fd`.
pub fn close_fd_set(fds: MidiFdSet) {
    if fds.in_fd >= 0 {
        unsafe {
            libc::close(fds.in_fd);
        }
    }
    if fds.out_fd >= 0 && fds.out_fd != fds.in_fd {
        unsafe {
            libc::close(fds.out_fd);
        }
    }
}

/// Parsed `"hw:<card>,<device>,<subdevice>"` RawMIDI address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct HwAddr {
    pub card: u32,
    pub device: u32,
    pub subdevice: u32,
}

/// Format a RawMIDI port identifier `"hw:<card>,<device>,<subdevice>"`.
#[cfg(any(target_os = "linux", test))]
#[must_use]
pub fn format_hw_id(card: u32, device: u32, subdevice: u32) -> String {
    format!("hw:{card},{device},{subdevice}")
}

/// Parse `"hw:<card>,<device>,<subdevice>"` into [`HwAddr`].
#[must_use]
pub fn parse_hw_id(id: &str) -> Option<HwAddr> {
    let rest = id.strip_prefix("hw:")?;
    let mut parts = rest.split(',');
    let card = parts.next()?.parse::<u32>().ok()?;
    let device = parts.next()?.parse::<u32>().ok()?;
    let subdevice = parts.next()?.parse::<u32>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(HwAddr {
        card,
        device,
        subdevice,
    })
}

/// Match an available MIDI port by exact `id` or case-insensitive substring on `id`,
/// `card_name`, `port_name`, or `"{card_name} {port_name}"`.
#[must_use]
pub fn match_port<'a>(
    ports: &'a [MidiPortInfo],
    pattern: &str,
    require_read: bool,
    require_write: bool,
) -> Option<&'a MidiPortInfo> {
    let needle = pattern.trim();
    if needle.is_empty() {
        return None;
    }

    let eligible = |port: &&MidiPortInfo| -> bool {
        (!require_read || port.can_read) && (!require_write || port.can_write)
    };

    if let Some(exact) = ports.iter().filter(eligible).find(|port| port.id == needle) {
        return Some(exact);
    }

    let needle_lower = needle.to_ascii_lowercase();
    ports
        .iter()
        .filter(eligible)
        .filter(|port| {
            let combined = format!("{} {}", port.card_name, port.port_name);
            port.id.to_ascii_lowercase().contains(&needle_lower)
                || port.card_name.to_ascii_lowercase().contains(&needle_lower)
                || port.port_name.to_ascii_lowercase().contains(&needle_lower)
                || combined.to_ascii_lowercase().contains(&needle_lower)
        })
        .min_by(|a, b| compare_port_ids(&a.id, &b.id))
}

/// Compare port IDs numerically by [`HwAddr`] when possible, falling back to string order.
pub fn compare_port_ids(a: &str, b: &str) -> std::cmp::Ordering {
    match (parse_hw_id(a), parse_hw_id(b)) {
        (Some(ha), Some(hb)) => ha.cmp(&hb),
        _ => a.cmp(b),
    }
}

/// Backend boundary separating ALSA port discovery/opening (async control plane)
/// from zero-alloc RT descriptor I/O.
pub trait MidiBackend: Send + Sync {
    /// Discover currently available MIDI ports.
    fn enumerate_ports(&self) -> io::Result<Vec<MidiPortInfo>>;

    /// Open non-blocking file descriptors for the selected input/output ports.
    fn open_ports(
        &self,
        input: Option<&MidiPortInfo>,
        output: Option<&MidiPortInfo>,
        generation: u64,
    ) -> io::Result<MidiFdSet>;
}

/// Construct the platform MIDI backend (`LinuxRawMidiBackend` on Linux, stub elsewhere).
#[must_use]
pub fn default_midi_backend() -> Arc<dyn MidiBackend> {
    #[cfg(target_os = "linux")]
    {
        Arc::new(rawmidi::LinuxRawMidiBackend)
    }
    #[cfg(not(target_os = "linux"))]
    {
        Arc::new(NullMidiBackend)
    }
}

#[cfg(not(target_os = "linux"))]
struct NullMidiBackend;

#[cfg(not(target_os = "linux"))]
impl MidiBackend for NullMidiBackend {
    fn enumerate_ports(&self) -> io::Result<Vec<MidiPortInfo>> {
        Ok(Vec::new())
    }

    fn open_ports(
        &self,
        _input: Option<&MidiPortInfo>,
        _output: Option<&MidiPortInfo>,
        generation: u64,
    ) -> io::Result<MidiFdSet> {
        Ok(MidiFdSet {
            generation,
            ..MidiFdSet::EMPTY
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_ports() -> Vec<MidiPortInfo> {
        vec![
            MidiPortInfo {
                id: "hw:10,0,0".to_string(),
                card_name: "WIDI Uhost".to_string(),
                port_name: "WIDI Uhost MIDI 2".to_string(),
                can_read: true,
                can_write: true,
            },
            MidiPortInfo {
                id: "hw:2,0,0".to_string(),
                card_name: "WIDI Uhost".to_string(),
                port_name: "WIDI Uhost MIDI 1".to_string(),
                can_read: true,
                can_write: false,
            },
            MidiPortInfo {
                id: "hw:1,0,0".to_string(),
                card_name: "Prophet Rev2".to_string(),
                port_name: "Rev2 MIDI Out".to_string(),
                can_read: false,
                can_write: true,
            },
        ]
    }

    #[test]
    fn parse_and_format_hw_id_roundtrips() {
        let parsed = parse_hw_id("hw:12,3,1").expect("valid hw id");
        assert_eq!(
            parsed,
            HwAddr {
                card: 12,
                device: 3,
                subdevice: 1,
            }
        );
        assert_eq!(format_hw_id(12, 3, 1), "hw:12,3,1");

        assert_eq!(parse_hw_id("hw:1,0"), None);
        assert_eq!(parse_hw_id("hw:1,0,0,1"), None);
        assert_eq!(parse_hw_id("20:0"), None);
        assert_eq!(parse_hw_id("hw:-1,0,0"), None);
    }

    #[test]
    fn match_port_respects_capabilities_exact_id_and_numeric_order() {
        let ports = sample_ports();

        assert_eq!(match_port(&ports, "", true, false), None);
        assert_eq!(match_port(&ports, "   ", false, true), None);

        // Exact ID skips substring matches on lower card IDs.
        let exact = match_port(&ports, "hw:10,0,0", true, true).expect("exact match");
        assert_eq!(exact.id, "hw:10,0,0");

        // Substring on WIDI: read selects hw:2,0,0 (numerically < hw:10,0,0), write skips read-only hw:2,0,0.
        let widi_in = match_port(&ports, "widi", true, false).expect("widi input");
        assert_eq!(widi_in.id, "hw:2,0,0");

        let widi_out = match_port(&ports, "widi", false, true).expect("widi output");
        assert_eq!(widi_out.id, "hw:10,0,0");

        // Write-only port matches only when read is not required.
        assert_eq!(match_port(&ports, "Prophet", true, false), None);
        let rev2_out = match_port(&ports, "Prophet", false, true).expect("rev2 out");
        assert_eq!(rev2_out.id, "hw:1,0,0");
    }

    #[test]
    fn close_fd_set_handles_shared_duplex_pipe_without_double_close() {
        let mut fds = [-1 as RawFd; 2];
        let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert_eq!(rc, 0);

        // Pretend read end is shared between in_fd and out_fd, and close write end normally.
        close_fd_set(MidiFdSet {
            in_fd: fds[0],
            out_fd: fds[0],
            generation: 1,
        });
        close_fd_set(MidiFdSet {
            in_fd: -1,
            out_fd: fds[1],
            generation: 1,
        });

        // Verify fds[0] is now closed (EBADF on fcntl).
        let check = unsafe { libc::fcntl(fds[0], libc::F_GETFD) };
        assert_eq!(check, -1);
    }
}
