//! ALSA MIDI backend abstraction, port matching, FD lifecycle helpers, and hotplug.

pub mod hotplug;
#[cfg(target_os = "linux")]
pub mod rawmidi;
pub mod rawmidi_uapi;

use std::io;
use std::os::fd::RawFd;
use std::sync::Arc;

use tactus_protocol::MidiPortInfo;
pub use tactus_protocol::MAX_MIDI_OUTPUTS;

/// Strictly `Copy` set of opened non-blocking MIDI file descriptors handed to the RT thread.
///
/// When input and output target the same full-duplex subdevice on ALSA RawMIDI, `in_fd` equals one
/// of `out_fds[..out_count]` (opened once with `O_RDWR | O_NONBLOCK | O_CLOEXEC`). Always release
/// via [`close_fd_set`] so shared descriptors are never double-closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MidiFdSet {
    pub in_fd: RawFd,
    pub out_fds: [RawFd; MAX_MIDI_OUTPUTS],
    pub out_count: u8,
    pub generation: u64,
}

impl MidiFdSet {
    /// Empty descriptor set (`in_fd = -1`, `out_count = 0`).
    pub const EMPTY: Self = Self {
        in_fd: -1,
        out_fds: [-1; MAX_MIDI_OUTPUTS],
        out_count: 0,
        generation: 0,
    };

    /// Build a descriptor set from an input FD and slice of output FDs (capped at [`MAX_MIDI_OUTPUTS`]).
    #[cfg(test)]
    #[must_use]
    pub fn from_parts(in_fd: RawFd, outputs: &[RawFd], generation: u64) -> Self {
        let mut set = Self {
            in_fd,
            generation,
            ..Self::EMPTY
        };
        for &fd in outputs {
            if !set.push_output(fd) {
                break;
            }
        }
        set
    }

    /// Construct a descriptor set with a single input FD.
    #[must_use]
    pub const fn single_input(in_fd: RawFd, generation: u64) -> Self {
        Self {
            in_fd,
            out_fds: [-1; MAX_MIDI_OUTPUTS],
            out_count: 0,
            generation,
        }
    }

    /// Construct a descriptor set with a single output FD.
    #[must_use]
    pub fn single_output(out_fd: RawFd, generation: u64) -> Self {
        let mut set = Self {
            generation,
            ..Self::EMPTY
        };
        let _ = set.push_output(out_fd);
        set
    }

    /// Return `true` when neither input nor output file descriptors are active.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.in_fd < 0 && self.out_count == 0
    }

    /// Active output file descriptors slice.
    #[must_use]
    pub fn active_outputs(&self) -> &[RawFd] {
        let len = (self.out_count as usize).min(MAX_MIDI_OUTPUTS);
        &self.out_fds[..len]
    }

    /// Append an output FD if non-negative and capacity remains.
    pub fn push_output(&mut self, fd: RawFd) -> bool {
        let len = self.out_count as usize;
        if fd < 0 || len >= MAX_MIDI_OUTPUTS {
            return false;
        }
        self.out_fds[len] = fd;
        self.out_count = (len + 1) as u8;
        true
    }

    /// Remove and return the output FD at `idx`, compacting remaining active slots.
    pub fn remove_output_at(&mut self, idx: usize) -> Option<RawFd> {
        let len = (self.out_count as usize).min(MAX_MIDI_OUTPUTS);
        if idx >= len {
            return None;
        }
        let removed = self.out_fds[idx];
        for i in idx..(len - 1) {
            self.out_fds[i] = self.out_fds[i + 1];
        }
        self.out_fds[len - 1] = -1;
        self.out_count = (len - 1) as u8;
        Some(removed)
    }
}

/// Extract deduplicated non-negative descriptors from `fds` on the stack (`len <= MAX_MIDI_OUTPUTS + 1`).
fn collect_unique_fds(fds: MidiFdSet) -> ([RawFd; MAX_MIDI_OUTPUTS + 1], usize) {
    let mut unique = [-1 as RawFd; MAX_MIDI_OUTPUTS + 1];
    let mut len = 0usize;

    let mut push_unique = |fd: RawFd| {
        if fd < 0 || unique[..len].contains(&fd) {
            return;
        }
        unique[len] = fd;
        len += 1;
    };

    push_unique(fds.in_fd);
    for &fd in fds.active_outputs() {
        push_unique(fd);
    }
    (unique, len)
}

/// Safely close any active file descriptors in `fds` outside the RT thread,
/// deduplicating shared duplex descriptors so no FD is double-closed.
pub fn close_fd_set(fds: MidiFdSet) {
    let (unique, len) = collect_unique_fds(fds);
    for &fd in &unique[..len] {
        unsafe {
            libc::close(fd);
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

/// Match all unique writable MIDI output ports matching `patterns`, preserving rule order
/// and capping at [`MAX_MIDI_OUTPUTS`].
#[must_use]
pub fn match_output_ports(ports: &[MidiPortInfo], patterns: &[String]) -> Vec<MidiPortInfo> {
    let mut matched: Vec<MidiPortInfo> = Vec::new();
    for pattern in patterns {
        if let Some(port) = match_port(ports, pattern, false, true) {
            if !matched.iter().any(|existing| existing.id == port.id) {
                matched.push(port.clone());
                if matched.len() >= MAX_MIDI_OUTPUTS {
                    break;
                }
            }
        }
    }
    matched
}

/// Result returned by [`MidiBackend::open_ports`], pairing opened descriptors with the exact port IDs
/// that succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenedMidiPorts {
    pub fds: MidiFdSet,
    pub attached_input_id: Option<String>,
    pub attached_output_ids: Vec<String>,
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

    /// Open non-blocking file descriptors for the selected input and output ports.
    fn open_ports(
        &self,
        input: Option<&MidiPortInfo>,
        outputs: &[MidiPortInfo],
        generation: u64,
    ) -> io::Result<OpenedMidiPorts>;
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
        _outputs: &[MidiPortInfo],
        generation: u64,
    ) -> io::Result<OpenedMidiPorts> {
        Ok(OpenedMidiPorts {
            fds: MidiFdSet {
                generation,
                ..MidiFdSet::EMPTY
            },
            attached_input_id: None,
            attached_output_ids: Vec::new(),
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

        // Multi-output matcher preserves pattern order, skips read-only/missing, and deduplicates.
        let multi = match_output_ports(
            &ports,
            &[
                "Prophet".to_string(),
                "widi".to_string(),
                "hw:10,0,0".to_string(),
                "nonexistent".to_string(),
            ],
        );
        let ids: Vec<&str> = multi.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["hw:1,0,0", "hw:10,0,0"]);
    }

    #[test]
    fn close_fd_set_handles_shared_duplex_and_multi_output_without_double_close() {
        let mut p1 = [-1 as RawFd; 2];
        let mut p2 = [-1 as RawFd; 2];
        assert_eq!(unsafe { libc::pipe(p1.as_mut_ptr()) }, 0);
        assert_eq!(unsafe { libc::pipe(p2.as_mut_ptr()) }, 0);

        let mut fds = MidiFdSet::from_parts(p1[0], &[p1[0], p2[1]], 1);
        assert_eq!(fds.active_outputs(), &[p1[0], p2[1]]);
        assert_eq!(fds.remove_output_at(0), Some(p1[0]));
        assert_eq!(fds.active_outputs(), &[p2[1]]);
        assert!(fds.push_output(p1[0]));

        let (unique, len) = collect_unique_fds(fds);
        assert_eq!(&unique[..len], &[p1[0], p2[1]]);

        close_fd_set(fds);
        close_fd_set(MidiFdSet::from_parts(p1[1], &[p2[0]], 1));
    }
}
