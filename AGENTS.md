# AGENTS.md — Tactus

## Project Identity & Stack
- **Appliance:** `tactusd` (`tactus.local`) — standalone Linux SBC USB/BLE-MIDI ↔ Ableton Link bridge over a dedicated WiFi AP (PoC: Arduino UNO Q, stock Debian 13 `trixie`; future Yocto `meta-tactus`).
- **Stack:** Rust 2021 workspace + C++17 `abl_link` FFI (`vendor/ableton-link`), `tokio` async control plane (`axum`, `arduino-router`), lock-free `rtrb` SPSC rings to a userspace RT thread.
- **Paths:** Config `/var/lib/tactus/config.toml` (`--config` / `TACTUS_CONFIG`), volatile runtime `/run/tactus/` (`tmpfs`).

## Build, Test & Lint Commands
```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo zigbuild --workspace --release --target aarch64-unknown-linux-gnu.2.36
# When editing Web UI or system scripts:
node --check crates/tactusd/web/app.js
bash -n system/scripts/*.sh && shellcheck system/scripts/*.sh system/kernel/*.sh
```
*(Zero warnings required; `cargo zigbuild` is mandatory for Linux-gated changes because macOS `clippy` skips `#[cfg(target_os = "linux")]`.)*

## Directory Map
- `crates/tactus-sync-core/` — Pure-Rust, zero-alloc, OS-independent sync engine & deterministic simulations.
- `crates/tactus-protocol/` — Shared `EngineCommand`, `Copy` RT messages, `AppState`, `TactusConfig`.
- `crates/tactus-link-sys/` — Offline-safe `build.rs` + C99 FFI wrappers for `vendor/ableton-link`.
- `crates/tactusd/` — Daemon binary (`src/` RT/MIDI/control/web/MCU/LED, `web/` embedded mobile PWA).
- `mcu/tactus-zephyr/` — Optional STM32U585 Zephyr sketch (`Arduino_RouterBridge` LED matrix).
- `system/` — `systemd/`, `hostapd/`, `dnsmasq/`, `avahi/`, `NetworkManager/`, `kernel/`, `scripts/`.
- `vendor/ableton-link/` — Pinned `Ableton/link` submodule (`Link-4.1.1`).

## Non-Negotiable Invariants
- **Never** require `PREEMPT_RT` or PipeWire; achieve timing determinism strictly in userspace (`SCHED_FIFO`, `mlockall`, `CLOCK_MONOTONIC`, core affinity).
- **Never** allocate, lock a mutex, block on I/O, or call `tokio` inside the RT sync thread (`rt-sync`).
- **Never** implement a pure-Rust Ableton Link protocol; wrap official `abl_link` C99 API in `crates/tactus-link-sys`.
- **Never** download code or run `git` in `build.rs`; support both vendored C++ and `pkg-config`/`ABL_LINK_LIB_DIR` for Yocto offline builds, and keep `Cargo.toml` deps lean.
- **Always** assume `/` is read-only: write persistent state ONLY to `/var/lib/tactus/config.toml` (atomic temp + `fsync` + `rename`); put logs/sockets in `tmpfs`.
- **Never** remove/disable `NetworkManager`; decouple `tactusd.service` from `tactus-ap.service` (`Wants=` only, no `After=`).
- **Never** route 24 PPQN MIDI Clock over `/var/run/arduino-router.sock`; send only low-rate telemetry (`tactus/downbeat` guarded by `cur_beat == 0`, `tactus/beat`, discrete state) and debounced inputs.
- **Hardware safety (live board):** Never chain disruptive commands (`reboot`, boot/`loader.conf`, AP restarts) after fallible steps with `;` (use `&&` or separate commands); create a rollback and keep SSH/serial access before editing boot/AP config; ask before rebooting a live session.

## On-Demand Skills & Reference Index
- **Specs:** `SPEC.md` (algorithms/API/config), `ARCHITECTURE.md` (threading/boot), `MILESTONES.md` (status).
- Use `/rt-midi-engine` (`.agents/skills/rt-midi-engine/SKILL.md`) when editing RT sync, ALSA RawMIDI, clock bridging, `rtrb`, or Tokio `AsyncFd`.
- Use `/ap-networking-systemd` (`.agents/skills/ap-networking-systemd/SKILL.md`) when editing WiFi AP, `systemd`, `hostapd`, `dnsmasq`, `avahi`, `NetworkManager`, or `status_led.rs`.
- Use `/web-ui-pwa` (`.agents/skills/web-ui-pwa/SKILL.md`) when editing `crates/tactusd/web/` or `crates/tactusd/src/web.rs`.
- Use `/unoq-ops` (`.agents/skills/unoq-ops/SKILL.md`) for UNO Q SSH/ADB, factory provisioning, deploying `tactusd`, or flashing `mcu/tactus-zephyr`.
- Use `/crate-release` (`.agents/skills/crate-release/SKILL.md`) when releasing crates to crates.io.
