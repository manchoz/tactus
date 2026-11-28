# MILESTONES.md — Tactus PoC Execution Plan

## M0 — Workspace & Offline-Safe `abl_link` Build
- **Deliverables:**
  - Cargo workspace skeleton (`tactus-protocol`, `tactus-link-sys`, `tactus-sync-core`, `tactusd`).
  - `tactus-link-sys` wrapping vendored `vendor/ableton-link/extensions/abl_link` with dual `build.rs` resolution (`ABL_LINK_LIB_DIR` / `pkg-config` override for Yocto BitBake, vendored C++ compilation fallback with zero network access).
  - Minimal integration test instantiating `abl_link`, reading monotonic clock micros, beat, and tempo.
- **Verification:** `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` pass on dev host; cross-compilation succeeds for `aarch64-unknown-linux-gnu`.

## M1 — `tactus-sync-core` Deterministic Simulation
- **Deliverables:** Pure-Rust, OS-independent, zero-allocation sync engine implementing `SPEC.md §2`:
  - `Unlocked` → `Acquiring` (min `1.5 s` / `24` ticks) → `Locked` state machine + `> 500 ms` timeout recovery.
  - Fixed-capacity (`4096` samples) OLS linear regression BPM estimator (default `10 s`, configurable up to `30 s`) + deadband (`±0.05 BPM`).
  - 1.5 s fast step-change detector with beat hysteresis and long-window flush.
  - Phase-lock drift compensator with deadband (`±4.0 ms`) and bounded nudge (`≤ 1.0 ms/beat`).
  - 24 PPQN scheduler with `TIMER_ABSTIME` grid, signed latency offset (`±50 ms`), and `start_stop_sync` handling (`0xFA`/`0xFB`/`0xFC`).
- **Verification:** Deterministic unit/simulation tests covering: ±5 ms BLE jitter, 40 ppm quartz drift over 10+ minutes, 110→128 BPM step change, clock dropout/recovery, and direction switching while playing.

## M2 — Network Stack (Dedicated WiFi AP) & Debian SBC Provisioning
- **Deliverables:**
  - Portable `system/` files (`99-tactus-unmanaged.conf`, `hostapd/tactus-ap.conf`, `dnsmasq/tactus-ap.conf`, `avahi/tactus.service`, `systemd/tactus-ap.service`, `systemd/tactusd.service`).
  - Idempotent provisioning script for Debian PoC.
- **Verification:**
  - `NetworkManager` ignores `wlan0` while keeping USB-Ethernet operational for SSH/deploy.
  - iPhone/iPad joins the `Tactus` SSID with no Captive Portal popup, resolves `tactus.local`, and holds a stable Ableton Link session between two mobile peers.

## M3 — Headless `tactusd` RT Engine, RawMIDI Hotplug & Atomic Config
- **Deliverables:**
  - `tactusd` RT thread (`std::thread`, `SCHED_FIFO` prio 75, `mlockall`, pinned to Core 3) running a unified zero-spin `poll([midi_in_fd, timerfd(CLOCK_MONOTONIC, TFD_TIMER_ABSTIME), cmd_eventfd])` loop, bridged with the `tokio` control plane via lock-free `rtrb` rings (`Copy` `RtCommand`, `TelemetrySnapshot`, and `RawFd` handoff/return rings so `open()`/`close()` never touch the RT thread).
  - Explicit clock-domain bridge between `abl_link`'s Linux `CLOCK_MONOTONIC_RAW` domain and `CLOCK_MONOTONIC` (`timerfd` `TFD_TIMER_ABSTIME`).
  - Direct Kernel ALSA RawMIDI backend via `libc` UAPI ioctls (replacing `/dev/snd/seq` and requiring zero `libasound2-dev` sysroot for macOS `cargo zigbuild`, since stock Linux SBC kernels such as UNO Q `7.2.11-arduino1+` may omit `CONFIG_SND_SEQUENCER` and kernel headers while providing `CONFIG_SND_RAWMIDI=m` and `CONFIG_SND_USB_AUDIO=m`):
    - Port enumeration and selection via `/dev/snd/controlC*` (`SNDRV_CTL_IOCTL_CARD_INFO`, `SNDRV_CTL_IOCTL_RAWMIDI_NEXT_DEVICE`, `SNDRV_CTL_IOCTL_RAWMIDI_INFO`, `SNDRV_CTL_IOCTL_RAWMIDI_PREFER_SUBDEVICE`) on the `tokio` control plane, behind a clean backend boundary so `snd_seq` could be added later on Yocto `meta-tactus` if desired.
    - RawMIDI `/dev/snd/midiC*D*` edge-case handling: single `O_RDWR | O_NONBLOCK | O_CLOEXEC` FD when IN and OUT target the same subdevice (separate `O_RDONLY` / `O_WRONLY` FDs otherwise), separate INPUT/OUTPUT substream capability probing, `O(1)` MIDI 1.0 System Real-Time byte filtering (`MidiEvent::from_byte`), and graceful `POLLERR`/`POLLHUP`/`ENODEV` detach back to async on unplug.
    - USB hotplug on the `tokio` async plane via `NETLINK_KOBJECT_UEVENT` (`SUBSYSTEM=sound`, pure `libc` socket without `libudev`) plus a low-rate fallback rescan; generalize `MidiPortInfo` (`id` e.g. `"hw:1,0,0"`, `card_name`, `port_name`, `can_read`, `can_write`) while preserving `config.toml` substring matching (`input_port_match` / `output_port_matches`, up to 8 outputs).
  - Atomic `config.toml` persistence (`/var/lib/tactus/config.toml` via temp file + `fsync` + `rename`) with `--config` / `TACTUS_CONFIG` override.
- **Verification:**
  - Host unit/mock tests covering RawMIDI port matching, `MidiPortInfo` serialization, `CLOCK_MONOTONIC` ↔ `CLOCK_MONOTONIC_RAW` bridge math, atomic `config.toml` write/recovery, and deterministic pipe/mock RT loop tests (`cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`).
  - macOS cross-compile via `cargo zigbuild --workspace --release --target aarch64-unknown-linux-gnu.2.36` with no external `libasound` sysroot.
  - Live hardware test on Linux SBC (reference UNO Q, deployed via `./system/scripts/provision-factory-unoq.sh --ssh ... --binary ...`) with CME WIDI Uhost + Ableton Link peers in both `Link → MIDI` and `MIDI → Link` modes, verifying `SCHED_FIFO`/core pinning and unplug/replug recovery without restarting `tactusd`.

## M4 — Embedded Mobile Web UI ("Add to Home Screen")
- **Deliverables:**
  - `axum` HTTP/WebSocket server on `tokio` runtime serving embedded vanilla HTML/CSS/JS UI (`tactusd/web/`).
  - iOS/Android Home Screen metadata (`apple-mobile-web-app-capable`, icon) and silent WebSocket reconnect on `visibilitychange` with immediate full-state hydration.
  - Controls for Direction, Start/Stop Sync, Latency Offset (`±50 ms`), MIDI port selection, Resync, plus live BPM/Lock/Peers/Downbeat readout.
- **Verification:** Saved to iOS/Android Home Screen over `http://tactus.local`, controls update `tactusd` and persist across reboot, UI recovers cleanly after phone screen lock/app switch with zero measurable jitter impact on MIDI Clock.

## M5 — Zephyr Heartbeat (`Arduino_RouterBridge`) — Complete
- **Deliverables:**
  - Minimal `mcu/tactus-zephyr/tactus-zephyr.ino` sketch (`Arduino_RouterBridge` + 3-bit grayscale 8×13 `Arduino_LED_Matrix` + onboard PWM RGB `LED3`) rendering downbeat pulse (`180 ms` smooth decay), beat pulse (`120 ms`), sync direction arrow (`Link → MIDI` / `MIDI → Link`), lock status, and play/stop state, plus debounced optional v2 buttons (`#ifdef TACTUS_ENABLE_BUTTONS`) and build/flash instructions in `mcu/tactus-zephyr/README.md`.
  - Optional `tokio` MessagePack-RPC Unix-socket client (`crates/tactusd/src/mcu.rs`, using the standalone `arduino-router` crate, enabled via `[mcu_bridge] enabled = true`) connecting to `/var/run/arduino-router.sock` with exponential backoff (`250 ms` → `5 s`), registering incoming `tactus/cmd/*` methods (`$/register`) with `50 ms` debounce, and emitting `Bridge.notify()` (`tactus/state`, `tactus/downbeat`, & `tactus/beat`) strictly on downbeat wraps, integer beat crossings, and discrete state/`>= 0.1 BPM` transitions (zero 24 PPQN traffic).
- **Verification:**
  - Host `UnixStream::pair()` unit/integration tests covering MessagePack-RPC framing, `$/register` handshake, MCU rate-boundary filtering (24 sub-beat ticks emit zero frames; downbeat wrap, integer beats, and discrete transitions emit once), command translation/debounce, and backoff/EOF shutdown (`cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, and `cargo zigbuild --workspace --release --target aarch64-unknown-linux-gnu.2.36` pass with zero warnings).
  - Live UNO Q hardware verification: compiled and flashed `tactus-zephyr.ino` onto the STM32U585 via on-board `arduino-cli` + `linuxgpiod` OpenOCD (`flash_sketch.cfg` at `0x08100000`), deployed `tactusd` (`SCHED_FIFO` 75 isolated on Core `3`) with `[mcu_bridge] enabled = true`, and verified live `arduino-router` socket connection (`tactus/state` RPC round-trip returning `[1, msgid, nil, nil]`).
