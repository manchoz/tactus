---
name: rt-midi-engine
description: >-
  Real-time audio/MIDI engine invariants for tactusd and tactus-sync-core:
  ALSA RawMIDI exclusive-open lifecycle, multi-output fan-out, clock domain bridging,
  CPU core pinning order, rtrb FdReturn safety, and Tokio AsyncFd registration.
  Use when editing crates/tactusd/src/{rt,midi,control,clock}.rs or crates/tactus-sync-core/.
---

# Real-Time MIDI & Link Engine Rules

## 1. ALSA RawMIDI I/O (`/dev/snd/midiC*D*`)
- Target ALSA RawMIDI directly on stock Debian kernels (which may lack `CONFIG_SND_SEQUENCER` / `/dev/snd/seq`; keep `/dev/snd/seq` optional for Yocto `meta-tactus`).
- RawMIDI subdevice opens are exclusive (`EBUSY`): before reopening/switching ports, the async control plane MUST detach and close prior FDs in strict order:
  1. Send `AttachMidi(EMPTY)` to RT.
  2. Await `FdReturn::Replaced`.
  3. Call `close_fd_set`.
  4. Call `open_ports()`.
- Publish `selected_midi_input` / `selected_midi_outputs` strictly from actually opened FDs.
- Multi-output fan-out (`MAX_MIDI_OUTPUTS = 8`) MUST stay strictly `Copy` / zero-alloc in RT with per-output fault isolation.
- `MidiToLink` RT Clock Thru MUST skip `out_fd == in_fd` to prevent clock feedback loops.

## 2. Clock Domains
- `abl_link` on Linux uses `CLOCK_MONOTONIC_RAW`; `clock_nanosleep(TIMER_ABSTIME)` uses `CLOCK_MONOTONIC`. Bridge both domains explicitly in `crates/tactusd/src/clock.rs`.

## 3. Thread Pinning, RT Hardening & Ring Buffers
- Call `pin_non_rt_threads()` to pin `main`, `tokio`, and `abl_link` Asio off `RT_PREFERRED_CORE` **before** spawning `rt-sync` or `tokio`.
- Harden `rt-sync` onto Core `3` (`SCHED_FIFO` 75, `mlockall`) **only after** `AblLinkDriver::new()`.
- **ZERO** heap allocations, mutexes, blocking I/O, or `tokio` calls inside the RT loop.
- Communicate RT ↔ Async strictly via `rtrb` SPSC ring buffers (`Copy` items) or atomics.
- Never drop `FdReturn`: keep a `256`-slot ring + stack `pending_fd_return` retry slot, and drain `fd_ret_rx` before `Shutdown` without spin-waiting.
- On Linux `tokio 1.54+`, register raw descriptors inside Tokio via `unsafe { AsyncFd::register_with_interest(...) }` (never deprecated `with_interest`) and call `guard.clear_ready()` after draining `eventfd` to `WouldBlock`.
