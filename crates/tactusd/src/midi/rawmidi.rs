//! Direct Linux kernel ALSA RawMIDI (`/dev/snd/controlC*` + `/dev/snd/midiC*D*`) backend.
//!
//! Targets stock UNO Q kernels (`CONFIG_SND_RAWMIDI=m`, `# CONFIG_SND_SEQUENCER is not set`)
//! without linking `libasound` or requiring ALSA userspace headers.

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};

use tactus_protocol::MidiPortInfo;

use crate::log::log_warn;
use crate::midi::rawmidi_uapi::{
    cstr_bytes_to_string, SndCtlCardInfo, SndRawmidiInfo, SNDRV_CTL_IOCTL_CARD_INFO,
    SNDRV_CTL_IOCTL_RAWMIDI_INFO, SNDRV_CTL_IOCTL_RAWMIDI_NEXT_DEVICE,
    SNDRV_CTL_IOCTL_RAWMIDI_PREFER_SUBDEVICE, SNDRV_RAWMIDI_INFO_INPUT, SNDRV_RAWMIDI_INFO_OUTPUT,
    SNDRV_RAWMIDI_INFO_STREAM_INACTIVE, SNDRV_RAWMIDI_STREAM_INPUT, SNDRV_RAWMIDI_STREAM_OUTPUT,
};
use crate::midi::{
    compare_port_ids, format_hw_id, parse_hw_id, HwAddr, MidiBackend, MidiFdSet, OpenedMidiPorts,
    MAX_MIDI_OUTPUTS,
};

/// Kernel ALSA RawMIDI backend.
pub struct LinuxRawMidiBackend;

impl MidiBackend for LinuxRawMidiBackend {
    fn enumerate_ports(&self) -> io::Result<Vec<MidiPortInfo>> {
        enumerate_rawmidi_ports()
    }

    fn open_ports(
        &self,
        input: Option<&MidiPortInfo>,
        outputs: &[MidiPortInfo],
        generation: u64,
    ) -> io::Result<OpenedMidiPorts> {
        let in_addr = input
            .map(|p| {
                parse_hw_id(&p.id).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("invalid RawMIDI input id `{}`", p.id),
                    )
                })
            })
            .transpose()?;

        let mut out_entries: Vec<(&MidiPortInfo, HwAddr)> =
            Vec::with_capacity(outputs.len().min(MAX_MIDI_OUTPUTS));
        for port in outputs.iter().take(MAX_MIDI_OUTPUTS) {
            let addr = parse_hw_id(&port.id).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid RawMIDI output id `{}`", port.id),
                )
            })?;
            if !out_entries.iter().any(|(_, existing)| *existing == addr) {
                out_entries.push((port, addr));
            }
        }

        let mut fds = MidiFdSet {
            generation,
            ..MidiFdSet::EMPTY
        };
        let mut owned_fds: Vec<OwnedFd> = Vec::with_capacity(out_entries.len() + 1);
        let mut attached_input_id: Option<String> = None;
        let mut attached_output_ids: Vec<String> = Vec::with_capacity(out_entries.len());

        let input_shared_with_output = in_addr
            .is_some_and(|addr_in| out_entries.iter().any(|(_, addr_out)| *addr_out == addr_in));
        let mut shared_raw_fd: Option<(HwAddr, RawFd)> = None;

        if let (Some(in_port), Some(addr_in)) = (input, in_addr) {
            if input_shared_with_output {
                match open_rawmidi_subdevice(addr_in, libc::O_RDWR) {
                    Ok(fd) => {
                        let raw = fd.as_raw_fd();
                        fds.in_fd = raw;
                        attached_input_id = Some(in_port.id.clone());
                        shared_raw_fd = Some((addr_in, raw));
                        owned_fds.push(fd);
                    }
                    Err(err) => {
                        log_warn!(
                            "failed to open duplex MIDI port {} ({}): {err}",
                            in_port.id,
                            in_port.port_name
                        );
                    }
                }
            } else {
                match open_rawmidi_subdevice(addr_in, libc::O_RDONLY) {
                    Ok(fd) => {
                        fds.in_fd = fd.as_raw_fd();
                        attached_input_id = Some(in_port.id.clone());
                        owned_fds.push(fd);
                    }
                    Err(err) => {
                        log_warn!(
                            "failed to open MIDI input port {} ({}): {err}",
                            in_port.id,
                            in_port.port_name
                        );
                    }
                }
            }
        }

        for (out_port, addr_out) in out_entries {
            if let Some((shared_addr, shared_fd)) = shared_raw_fd {
                if shared_addr == addr_out {
                    if fds.push_output(shared_fd) {
                        attached_output_ids.push(out_port.id.clone());
                    }
                    continue;
                }
            }
            if in_addr == Some(addr_out) {
                // Shared O_RDWR open above failed; do not open O_WRONLY separately.
                continue;
            }
            match open_rawmidi_subdevice(addr_out, libc::O_WRONLY) {
                Ok(fd) => {
                    let raw = fd.as_raw_fd();
                    if fds.push_output(raw) {
                        attached_output_ids.push(out_port.id.clone());
                        owned_fds.push(fd);
                    }
                }
                Err(err) => {
                    log_warn!(
                        "failed to open MIDI output port {} ({}): {err}",
                        out_port.id,
                        out_port.port_name
                    );
                }
            }
        }

        for fd in owned_fds {
            let _ = fd.into_raw_fd();
        }

        Ok(OpenedMidiPorts {
            fds,
            attached_input_id,
            attached_output_ids,
        })
    }
}

fn enumerate_rawmidi_ports() -> io::Result<Vec<MidiPortInfo>> {
    let entries = match fs::read_dir("/dev/snd") {
        Ok(rd) => rd,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };

    let mut cards: Vec<u32> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            name.to_str()?.strip_prefix("controlC")?.parse::<u32>().ok()
        })
        .collect();
    cards.sort_unstable();
    cards.dedup();

    let mut ports = Vec::new();
    for card in cards {
        match enumerate_card_ports(card, &mut ports) {
            Ok(()) => {}
            Err(err) => {
                log_warn!("skipping /dev/snd/controlC{card} during RawMIDI scan: {err}");
            }
        }
    }

    ports.sort_by(|a, b| compare_port_ids(&a.id, &b.id));
    Ok(ports)
}

fn enumerate_card_ports(card: u32, out: &mut Vec<MidiPortInfo>) -> io::Result<()> {
    let ctl_path = format!("/dev/snd/controlC{card}");
    let ctl_fd = open_c_path(&ctl_path, libc::O_RDONLY | libc::O_CLOEXEC)?;

    let mut card_info = SndCtlCardInfo::default();
    let rc = unsafe {
        libc::ioctl(
            ctl_fd.as_raw_fd(),
            SNDRV_CTL_IOCTL_CARD_INFO as libc::Ioctl,
            &raw mut card_info,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }

    let mut card_name = cstr_bytes_to_string(&card_info.name);
    if card_name.is_empty() {
        card_name = cstr_bytes_to_string(&card_info.id);
    }
    if card_name.is_empty() {
        card_name = format!("Card {card}");
    }

    let mut dev: i32 = -1;
    loop {
        let rc = unsafe {
            libc::ioctl(
                ctl_fd.as_raw_fd(),
                SNDRV_CTL_IOCTL_RAWMIDI_NEXT_DEVICE as libc::Ioctl,
                &raw mut dev,
            )
        };
        if rc < 0 {
            let err = io::Error::last_os_error();
            // ENOTTY/EINVAL indicates no rawmidi support on this card control node.
            if matches!(err.raw_os_error(), Some(libc::ENOTTY | libc::EINVAL)) {
                break;
            }
            return Err(err);
        }
        if dev < 0 {
            break;
        }
        let device = dev as u32;

        let out_base =
            probe_rawmidi_info(ctl_fd.as_raw_fd(), device, 0, SNDRV_RAWMIDI_STREAM_OUTPUT)?;
        let in_base =
            probe_rawmidi_info(ctl_fd.as_raw_fd(), device, 0, SNDRV_RAWMIDI_STREAM_INPUT)?;

        let out_count = subdevice_count(out_base.as_ref(), SNDRV_RAWMIDI_INFO_OUTPUT);
        let in_count = subdevice_count(in_base.as_ref(), SNDRV_RAWMIDI_INFO_INPUT);
        let max_subdevs = out_count.max(in_count);

        for sub in 0..max_subdevs {
            let in_info = if sub < in_count {
                probe_rawmidi_info(ctl_fd.as_raw_fd(), device, sub, SNDRV_RAWMIDI_STREAM_INPUT)?
            } else {
                None
            };
            let out_info = if sub < out_count {
                probe_rawmidi_info(ctl_fd.as_raw_fd(), device, sub, SNDRV_RAWMIDI_STREAM_OUTPUT)?
            } else {
                None
            };

            let can_read = in_info.as_ref().is_some_and(stream_is_active);
            let can_write = out_info.as_ref().is_some_and(stream_is_active);
            if !can_read && !can_write {
                continue;
            }

            let port_name = choose_port_name(
                in_info.as_ref().or(out_info.as_ref()),
                &card_name,
                card,
                device,
                sub,
                max_subdevs,
            );

            out.push(MidiPortInfo {
                id: format_hw_id(card, device, sub),
                card_name: card_name.clone(),
                port_name,
                can_read,
                can_write,
            });
        }
    }

    Ok(())
}

fn probe_rawmidi_info(
    ctl_fd: RawFd,
    device: u32,
    subdevice: u32,
    stream: i32,
) -> io::Result<Option<SndRawmidiInfo>> {
    let mut info = SndRawmidiInfo {
        device,
        subdevice,
        stream,
        ..SndRawmidiInfo::default()
    };
    let rc = unsafe {
        libc::ioctl(
            ctl_fd,
            SNDRV_CTL_IOCTL_RAWMIDI_INFO as libc::Ioctl,
            &raw mut info,
        )
    };
    if rc == 0 {
        return Ok(Some(info));
    }
    let err = io::Error::last_os_error();
    if matches!(
        err.raw_os_error(),
        Some(libc::ENOENT | libc::ENXIO | libc::ENODEV | libc::EINVAL)
    ) {
        return Ok(None);
    }
    Err(err)
}

fn subdevice_count(info: Option<&SndRawmidiInfo>, dir_flag: u32) -> u32 {
    let Some(info) = info else {
        return 0;
    };
    if info.subdevices_count > 0 {
        info.subdevices_count
    } else if (info.flags & dir_flag) != 0 {
        1
    } else {
        0
    }
}

fn stream_is_active(info: &SndRawmidiInfo) -> bool {
    (info.flags & SNDRV_RAWMIDI_INFO_STREAM_INACTIVE) == 0
}

fn choose_port_name(
    info: Option<&SndRawmidiInfo>,
    card_name: &str,
    card: u32,
    device: u32,
    subdevice: u32,
    max_subdevs: u32,
) -> String {
    if let Some(info) = info {
        let subname = cstr_bytes_to_string(&info.subname);
        if !subname.is_empty() {
            return subname;
        }
        let dev_name = cstr_bytes_to_string(&info.name);
        if !dev_name.is_empty() {
            return if max_subdevs > 1 {
                format!("{dev_name} #{subdevice}")
            } else {
                dev_name
            };
        }
        let dev_id = cstr_bytes_to_string(&info.id);
        if !dev_id.is_empty() {
            return dev_id;
        }
    }
    if max_subdevs > 1 {
        format!("{card_name} MIDI {device} #{subdevice}")
    } else {
        format!("{card_name} ({})", format_hw_id(card, device, subdevice))
    }
}

/// Pin preferred subdevice on the control FD, then open `/dev/snd/midiC{c}D{d}` while `ctl_fd`
/// remains open so the kernel routes the open to the requested subdevice.
fn open_rawmidi_subdevice(addr: HwAddr, access_flags: libc::c_int) -> io::Result<OwnedFd> {
    let ctl_path = format!("/dev/snd/controlC{}", addr.card);
    let ctl_fd = open_c_path(&ctl_path, libc::O_RDONLY | libc::O_CLOEXEC)?;

    let preferred = addr.subdevice as libc::c_int;
    let rc = unsafe {
        libc::ioctl(
            ctl_fd.as_raw_fd(),
            SNDRV_CTL_IOCTL_RAWMIDI_PREFER_SUBDEVICE as libc::Ioctl,
            &raw const preferred,
        )
    };
    if rc < 0 {
        let err = io::Error::last_os_error();
        if !(addr.subdevice == 0 && err.raw_os_error() == Some(libc::ENOTTY)) {
            return Err(io::Error::new(
                err.kind(),
                format!(
                    "SNDRV_CTL_IOCTL_RAWMIDI_PREFER_SUBDEVICE({}) failed on {ctl_path}: {err}",
                    addr.subdevice
                ),
            ));
        }
    }

    let midi_path = format!("/dev/snd/midiC{}D{}", addr.card, addr.device);
    let midi_fd = open_c_path(
        &midi_path,
        access_flags | libc::O_NONBLOCK | libc::O_CLOEXEC,
    )?;

    drop(ctl_fd);
    Ok(midi_fd)
}

fn open_c_path(path: &str, flags: libc::c_int) -> io::Result<OwnedFd> {
    let c_path = CString::new(path)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "interior NUL in device path"))?;
    let fd = unsafe { libc::open(c_path.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
