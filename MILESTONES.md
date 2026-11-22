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
  - Fixed-capacity (`2048` samples) 10 s OLS linear regression BPM estimator + deadband (`±0.05 BPM`).
  - 1.5 s fast step-change detector with beat hysteresis and long-window flush.
  - Phase-lock drift compensator with deadband (`±4.0 ms`) and bounded nudge (`≤ 1.0 ms/beat`).
  - 24 PPQN scheduler with `TIMER_ABSTIME` grid, signed latency offset (`±50 ms`), and `start_stop_sync` handling (`0xFA`/`0xFB`/`0xFC`).
- **Verification:** Deterministic unit/simulation tests covering: ±5 ms BLE jitter, 40 ppm quartz drift over 10+ minutes, 110→128 BPM step change, clock dropout/recovery, and direction switching while playing.

## M2 — Network Stack (5 GHz AP) & System Provisioning on UNO Q
- **Deliverables:**
  - Portable `system/` files (`99-tactus-unmanaged.conf`, `hostapd/tactus-ap.conf`, `dnsmasq/tactus-ap.conf`, `avahi/tactus.service`, `systemd/tactus-ap.service`, `systemd/tactusd.service`).
  - Idempotent provisioning script for Debian PoC.
- **Verification:**
  - `NetworkManager` ignores `wlan0` while keeping USB-Ethernet operational for SSH/deploy.
  - iPhone/iPad joins `Tactus` 5 GHz SSID (non-DFS ch 36–48) with no Captive Portal popup, resolves `tactus.local`, and holds a stable Ableton Link session between two mobile peers.

## M3 — Headless `tactusd` RT Engine, ALSA Hotplug & Atomic Config
- **Deliverables:**
  - `tactusd` RT thread (`std::thread`, `SCHED_FIFO` prio 75, `mlockall`, core pinning) bridged via `rtrb` with `Copy` / zero-alloc `TelemetrySnapshot`.
  - Direct ALSA Sequencer (`alsa` crate, no PipeWire) for MIDI Clock I/O; port enumeration and USB hotplug (`SND_SEQ_EVENT_PORT_START` / `PORT_EXIT`) handled outside the RT hot path.
  - Atomic `config.toml` persistence (`/var/lib/tactus/config.toml` via temp file + `fsync` + `rename`) with CLI/env override.
- **Verification:** Live hardware test on UNO Q with CME WIDI Uhost + Ableton Link peers in both `Link → MIDI` and `MIDI → Link` modes; unplug/replug WIDI Uhost recovers automatically without restarting `tactusd`.

## M4 — Embedded Mobile Web UI ("Add to Home Screen")
- **Deliverables:**
  - `axum` HTTP/WebSocket server on `tokio` runtime serving embedded vanilla HTML/CSS/JS UI (`tactusd/web/`).
  - iOS/Android Home Screen metadata (`apple-mobile-web-app-capable`, icon) and silent WebSocket reconnect on `visibilitychange` with immediate full-state hydration.
  - Controls for Direction, Start/Stop Sync, Latency Offset (`±50 ms`), MIDI port selection, Resync, plus live BPM/Lock/Peers/Downbeat readout.
- **Verification:** Saved to iOS/Android Home Screen over `http://tactus.local`, controls update `tactusd` and persist across reboot, UI recovers cleanly after phone screen lock/app switch with zero measurable jitter impact on MIDI Clock.

## M5 — Zephyr Heartbeat (`Arduino_RouterBridge`) — Optional PoC Bonus
- **Deliverables:**
  - Minimal `mcu/tactus-zephyr` sketch driving the onboard 8×13 LED matrix (downbeat pulse + direction arrow).
  - `tokio` MessagePack-RPC client in `tactusd` connected to `/var/run/arduino-router.sock` using `Bridge.notify()` (low-rate events only, zero 24 PPQN traffic).
- **Verification:** LED matrix pulses in sync with Link/MIDI downbeat while RT MIDI timing remains unaffected.
