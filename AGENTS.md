# AGENTS.md — Tactus

## Project Identity
**Tactus** is a standalone embedded appliance (running on Arduino UNO Q) that bridges USB-MIDI / BLE-MIDI (via CME WIDI Uhost in PoC) and Ableton Link over a dedicated 5 GHz WiFi Access Point.

- **Daemon:** `tactusd`
- **Hostname mDNS:** `tactus.local`
- **Config path:** `/var/lib/tactus/config.toml` (override via `--config` or `TACTUS_CONFIG`)
- **Runtime path:** `/run/tactus/` (`tmpfs`)
- **Future Yocto layer:** `meta-tactus`

## Non-Negotiable Architectural Invariants
1. **No `PREEMPT_RT`, no PipeWire:** Target stock Debian Linux (and future Yocto QLI). Achieve MIDI timing determinism strictly in userspace (`SCHED_FIFO`, `mlockall`, `CLOCK_MONOTONIC` absolute timers, core affinity). Talk directly to ALSA Sequencer (`/dev/snd/seq`).
2. **Strict RT vs. Async Separation:**
   - The Real-Time Sync Engine runs on a dedicated `std::thread` (`SCHED_FIFO`). **ZERO** heap allocations, **ZERO** mutexes, **ZERO** blocking I/O, and **ZERO** `tokio` calls inside the RT loop.
   - Control-plane (`axum` HTTP/WebSocket, config persistence, `arduino-router` RPC) runs on a `tokio` runtime on the remaining cores.
   - Communication between RT and Async MUST use lock-free SPSC ring buffers (`rtrb`) or atomics.
3. **Official Ableton Link C API (`abl_link`) Only:** Never implement a pure-Rust Ableton Link protocol. Wrap `Ableton/link`'s `extensions/abl_link` C99 API in `crates/tactus-link-sys`.
4. **Yocto-Ready & Read-Only Rootfs Ready:**
   - Never download code or run `git` inside `build.rs`. Support both vendored C++ sources (dev/PoC) and `pkg-config` / `ABL_LINK_LIB_DIR` (Yocto offline BitBake build).
   - Assume `/` is mounted read-only. Write state ONLY to `/var/lib/tactus/config.toml` via atomic temp-file + `fsync` + `rename`. Logs and sockets go exclusively to `tmpfs` (`/run/tactus/` or `journald` volatile).
   - Keep `Cargo.toml` dependencies lean to simplify BitBake `cargo vendor` packaging.
5. **NetworkManager Coexistence:** Never remove or disable `NetworkManager`. Exclude `wlan0` via drop-in (`/etc/NetworkManager/conf.d/99-tactus-unmanaged.conf`) and manage the 5 GHz AP with `hostapd` + `dnsmasq` + `avahi-daemon`.
6. **MCU Bridge Boundary (`Arduino_RouterBridge`):** Never route 24 PPQN MIDI Clock over `/var/run/arduino-router.sock`. Use `Bridge.notify()` (MessagePack-RPC) strictly for low-rate telemetry (downbeat pulse, BPM/state changes) and debounced physical input events.

## Build & Verification Rules
- Pure sync logic (`tactus-sync-core`) MUST compile and pass deterministic unit/simulation tests on the host (`macOS` / `x86_64` / `aarch64`) without ALSA or Link hardware attached.
- Cross-compile target: `aarch64-unknown-linux-gnu`.
- Before closing any milestone, run `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace`.
