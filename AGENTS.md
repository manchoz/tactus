# AGENTS.md — Tactus

## Project Identity
**Tactus** is a standalone embedded appliance (running on a Linux SBC; PoC reference board: Arduino UNO Q) that bridges USB-MIDI / BLE-MIDI and Ableton Link over a dedicated WiFi Access Point.

- **Daemon:** `tactusd`
- **Hostname mDNS:** `tactus.local`
- **Config path:** `/var/lib/tactus/config.toml` (override via `--config` or `TACTUS_CONFIG`)
- **Runtime path:** `/run/tactus/` (`tmpfs`)
- **Future Yocto layer:** `meta-tactus`

## Non-Negotiable Architectural Invariants
1. **No `PREEMPT_RT`, no PipeWire:** Target stock Debian Linux (and future Yocto QLI). Achieve MIDI timing determinism strictly in userspace (`SCHED_FIFO`, `mlockall`, `CLOCK_MONOTONIC` absolute timers, core affinity).
   - **MIDI I/O:** Stock Linux SBC kernels (such as UNO Q `7.2.11-arduino1+`) may lack `CONFIG_SND_SEQUENCER` (no `/dev/snd/seq`); talk directly to **ALSA RawMIDI (`/dev/snd/midiC*D*`)** on stock Debian PoC (keeping `/dev/snd/seq` optional for Yocto `meta-tactus`). RawMIDI subdevice opens are exclusive (`EBUSY`), so before reopening/switching ports the async control plane MUST detach and close prior FDs (`AttachMidi(EMPTY)` → await `FdReturn::Replaced` → `close_fd_set`) before calling `open_ports()`, and publish `selected_midi_input`/`selected_midi_outputs` strictly from actually opened FDs. Multi-output fan-out (`MAX_MIDI_OUTPUTS = 8`) must stay strictly `Copy`/zero-alloc in RT with per-output fault isolation, and `MidiToLink` RT Clock Thru MUST skip `out_fd == in_fd` to prevent clock feedback loops.
   - **Clock domains:** Official `abl_link` on Linux uses `CLOCK_MONOTONIC_RAW`, whereas `clock_nanosleep(TIMER_ABSTIME)` uses `CLOCK_MONOTONIC`; bridge the two domains explicitly in `tactusd`.
2. **Strict RT vs. Async Separation:**
   - The Real-Time Sync Engine runs on a dedicated `std::thread` (`SCHED_FIFO`). **ZERO** heap allocations, **ZERO** mutexes, **ZERO** blocking I/O, and **ZERO** `tokio` calls inside the RT loop.
   - Control-plane (`axum` HTTP/WebSocket, config persistence, `arduino-router` RPC) runs on a `tokio` runtime on the remaining cores.
   - Communication between RT and Async MUST use lock-free SPSC ring buffers (`rtrb`) or atomics.
3. **Official Ableton Link C API (`abl_link`) Only:** Never implement a pure-Rust Ableton Link protocol. Wrap `Ableton/link`'s `extensions/abl_link` C99 API in `crates/tactus-link-sys`.
4. **Yocto-Ready & Read-Only Rootfs Ready:**
   - Never download code or run `git` inside `build.rs`. Support both vendored C++ sources (dev/PoC) and `pkg-config` / `ABL_LINK_LIB_DIR` (Yocto offline BitBake build).
   - Assume `/` is mounted read-only. Write state ONLY to `/var/lib/tactus/config.toml` via atomic temp-file + `fsync` + `rename`. Logs and sockets go exclusively to `tmpfs` (`/run/tactus/` or `journald` volatile).
   - Keep `Cargo.toml` dependencies lean to simplify BitBake `cargo vendor` packaging.
5. **NetworkManager Coexistence & Discovery:** Never remove or disable `NetworkManager`. Exclude `wlan0` and `p2p-dev-wlan0` via `/etc/NetworkManager/conf.d/99-tactus-unmanaged.conf` and manage the dedicated WiFi AP with `hostapd` (default 2.4 GHz ch `11`, `20 MHz`, clear of BLE advertising channels; optional 5 GHz non-DFS ch `36–48` on radios permitting 5 GHz AP) + `dnsmasq` + `avahi-daemon` (announcing `tactus.local`, `_tactus._tcp`, and `_http._tcp`; stock `arduino.service` disabled when trimmed).
6. **MCU Bridge Boundary (`Arduino_RouterBridge`):** Never route 24 PPQN MIDI Clock over `/var/run/arduino-router.sock`. Use `Bridge.notify()` (MessagePack-RPC) strictly for low-rate telemetry (downbeat pulse, BPM/state changes) and debounced physical input events.
7. **Embedded Web UI Cache-Busting (iOS Home Screen):** Standalone iOS Home Screen web apps keep an isolated WebKit cache and stay suspended across daemon upgrades. Always serve `/`, `/style.css`, `/app.js`, and `/manifest.webmanifest` with `Cache-Control: no-store, no-cache, must-revalidate`, `Pragma: no-cache`, and `Expires: 0`, version asset links with `?v=<ui_hash>`, and preserve the `/healthz` `ui_hash` auto-reload check (loop-guarded via `sessionStorage`) in `app.js`.

## Build & Verification Rules
- Pure sync logic (`tactus-sync-core`) MUST compile and pass deterministic unit/simulation tests on the host (`macOS` / `x86_64` / `aarch64`) without ALSA or Link hardware attached.
- Cross-compile target: `aarch64-unknown-linux-gnu` (reference Debian 13 `trixie` Linux SBC, glibc `2.41`; on macOS build via `cargo zigbuild --workspace --target aarch64-unknown-linux-gnu.2.36`).
- Before closing any milestone, run `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` (plus `node --check crates/tactusd/web/app.js` when editing Web UI JS and `node` is available).
