//! Linux Kernel ALSA Control & RawMIDI UAPI constants and `#[repr(C)]` structs.
//!
//! Verified against Linux `<sound/asound.h>` so `tactusd` requires zero `libasound2-dev`
//! headers or sysroot libraries when cross-compiling from macOS via `cargo zigbuild`.

#![allow(dead_code)]

use std::mem::size_of;

pub const SNDRV_RAWMIDI_STREAM_OUTPUT: i32 = 0;
pub const SNDRV_RAWMIDI_STREAM_INPUT: i32 = 1;

pub const SNDRV_RAWMIDI_INFO_OUTPUT: u32 = 0x0000_0001;
pub const SNDRV_RAWMIDI_INFO_INPUT: u32 = 0x0000_0002;
pub const SNDRV_RAWMIDI_INFO_DUPLEX: u32 = 0x0000_0004;
pub const SNDRV_RAWMIDI_INFO_UMP: u32 = 0x0000_0008;
pub const SNDRV_RAWMIDI_INFO_STREAM_INACTIVE: u32 = 0x0000_0010;

const IOC_NRBITS: u32 = 8;
const IOC_TYPEBITS: u32 = 8;
const IOC_SIZEBITS: u32 = 14;

const IOC_NRSHIFT: u32 = 0;
const IOC_TYPESHIFT: u32 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u32 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u32 = IOC_SIZESHIFT + IOC_SIZEBITS;

const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;

const fn ioc(dir: u32, ty: u8, nr: u8, size: usize) -> u32 {
    (dir << IOC_DIRSHIFT)
        | ((ty as u32) << IOC_TYPESHIFT)
        | ((nr as u32) << IOC_NRSHIFT)
        | ((size as u32) << IOC_SIZESHIFT)
}

const fn ior<T>(ty: u8, nr: u8) -> u32 {
    ioc(IOC_READ, ty, nr, size_of::<T>())
}

const fn iow<T>(ty: u8, nr: u8) -> u32 {
    ioc(IOC_WRITE, ty, nr, size_of::<T>())
}

const fn iowr<T>(ty: u8, nr: u8) -> u32 {
    ioc(IOC_READ | IOC_WRITE, ty, nr, size_of::<T>())
}

/// `_IOR('U', 0x01, struct snd_ctl_card_info)`
pub const SNDRV_CTL_IOCTL_CARD_INFO: u32 = ior::<SndCtlCardInfo>(b'U', 0x01);

/// `_IOWR('U', 0x40, int)`
pub const SNDRV_CTL_IOCTL_RAWMIDI_NEXT_DEVICE: u32 = iowr::<i32>(b'U', 0x40);

/// `_IOWR('U', 0x41, struct snd_rawmidi_info)`
pub const SNDRV_CTL_IOCTL_RAWMIDI_INFO: u32 = iowr::<SndRawmidiInfo>(b'U', 0x41);

/// `_IOW('U', 0x42, int)`
pub const SNDRV_CTL_IOCTL_RAWMIDI_PREFER_SUBDEVICE: u32 = iow::<i32>(b'U', 0x42);

/// Kernel `struct snd_ctl_card_info` from `<sound/asound.h>`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SndCtlCardInfo {
    pub card: i32,
    pub pad: i32,
    pub id: [u8; 16],
    pub driver: [u8; 16],
    pub name: [u8; 32],
    pub longname: [u8; 80],
    pub reserved_: [u8; 16],
    pub mixername: [u8; 80],
    pub components: [u8; 128],
}

impl Default for SndCtlCardInfo {
    fn default() -> Self {
        Self {
            card: 0,
            pad: 0,
            id: [0; 16],
            driver: [0; 16],
            name: [0; 32],
            longname: [0; 80],
            reserved_: [0; 16],
            mixername: [0; 80],
            components: [0; 128],
        }
    }
}

/// Kernel `struct snd_rawmidi_info` from `<sound/asound.h>`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SndRawmidiInfo {
    pub device: u32,
    pub subdevice: u32,
    pub stream: i32,
    pub card: i32,
    pub flags: u32,
    pub id: [u8; 64],
    pub name: [u8; 80],
    pub subname: [u8; 32],
    pub subdevices_count: u32,
    pub subdevices_avail: u32,
    pub tied_device: i32,
    pub reserved: [u8; 60],
}

impl Default for SndRawmidiInfo {
    fn default() -> Self {
        Self {
            device: 0,
            subdevice: 0,
            stream: 0,
            card: 0,
            flags: 0,
            id: [0; 64],
            name: [0; 80],
            subname: [0; 32],
            subdevices_count: 0,
            subdevices_avail: 0,
            tied_device: 0,
            reserved: [0; 60],
        }
    }
}

/// Decode a NUL-terminated kernel byte array into a trimmed `String`.
#[must_use]
pub fn cstr_bytes_to_string(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

#[cfg(test)]
mod tests {
    use std::mem::{align_of, offset_of, size_of};

    use super::*;

    #[test]
    fn uapi_structs_match_linux_arm64_and_x86_64_abi() {
        assert_eq!(size_of::<SndCtlCardInfo>(), 376);
        assert_eq!(align_of::<SndCtlCardInfo>(), 4);

        assert_eq!(size_of::<SndRawmidiInfo>(), 268);
        assert_eq!(align_of::<SndRawmidiInfo>(), 4);

        assert_eq!(offset_of!(SndRawmidiInfo, device), 0);
        assert_eq!(offset_of!(SndRawmidiInfo, subdevice), 4);
        assert_eq!(offset_of!(SndRawmidiInfo, stream), 8);
        assert_eq!(offset_of!(SndRawmidiInfo, card), 12);
        assert_eq!(offset_of!(SndRawmidiInfo, flags), 16);
        assert_eq!(offset_of!(SndRawmidiInfo, id), 20);
        assert_eq!(offset_of!(SndRawmidiInfo, name), 84);
        assert_eq!(offset_of!(SndRawmidiInfo, subname), 164);
        assert_eq!(offset_of!(SndRawmidiInfo, subdevices_count), 196);
        assert_eq!(offset_of!(SndRawmidiInfo, subdevices_avail), 200);
        assert_eq!(offset_of!(SndRawmidiInfo, tied_device), 204);
        assert_eq!(offset_of!(SndRawmidiInfo, reserved), 208);
    }

    #[test]
    fn uapi_ioctl_constants_match_linux_headers() {
        assert_eq!(SNDRV_CTL_IOCTL_CARD_INFO, 0x8178_5501);
        assert_eq!(SNDRV_CTL_IOCTL_RAWMIDI_NEXT_DEVICE, 0xc004_5540);
        assert_eq!(SNDRV_CTL_IOCTL_RAWMIDI_INFO, 0xc10c_5541);
        assert_eq!(SNDRV_CTL_IOCTL_RAWMIDI_PREFER_SUBDEVICE, 0x4004_5542);
    }

    #[test]
    fn cstr_decoder_truncates_at_nul_and_trims() {
        let mut buf = [0u8; 16];
        buf[..11].copy_from_slice(b" WIDI Uhost");
        buf[12..15].copy_from_slice(b"abc");
        assert_eq!(cstr_bytes_to_string(&buf), "WIDI Uhost");
        assert_eq!(cstr_bytes_to_string(b"MIDI 1"), "MIDI 1");
    }
}
