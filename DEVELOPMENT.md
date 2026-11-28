# Tactus — Development & Source Provisioning Guide

For end-user setup, Web UI instructions, and hardware status legends, see the [End-User Manual (README.md)](README.md).

---

## Architectural Highlights

- **Userspace Real-Time Determinism:** Targets stock Debian Linux (and future Yocto QLI `meta-tactus`) without `PREEMPT_RT` or PipeWire — uses a dedicated `SCHED_FIFO` thread, `mlockall`, `CLOCK_MONOTONIC` absolute timers (`TIMER_ABSTIME`), CPU core affinity, and direct ALSA RawMIDI (`/dev/snd/midiC*D*`) I/O (keeping `/dev/snd/seq` optional for Yocto `meta-tactus`).
- **Strict RT vs. Async Separation:** The RT sync thread performs zero heap allocations, zero mutexes, zero blocking I/O, and zero `tokio` calls; the `tokio` control plane (`axum` HTTP/WebSocket, config persistence, MCU RPC) communicates exclusively through lock-free SPSC ring buffers (`rtrb`) with strictly `Copy` `TelemetrySnapshot` and `RtCommand` messages.
- **Official Ableton Link C API:** Wraps `Ableton/link`'s `extensions/abl_link` C99 API (`tactus-link-sys`), avoiding custom Link protocol reimplementations.
- **Yocto & Read-Only Rootfs Ready:** Offline builds with zero network/`git` activity in `build.rs`, dual prebuilt/vendored `abl_link` resolution, atomic state writes restricted to `/var/lib/tactus/config.toml`, and runtime state on `tmpfs` (`/run/tactus/`).

## Documentation & Workspace Layout

- [SPEC.md](SPEC.md) — Functional, bidirectional sync algorithm, network, Web UI, and `config.toml` specifications
- [ARCHITECTURE.md](ARCHITECTURE.md) — Monorepo layout, threading model, protocol contract, and boot sequence
- [MILESTONES.md](MILESTONES.md) — Phased execution and verification plan
- [AGENTS.md](AGENTS.md) — Non-negotiable architectural invariants and verification gates
- `arduino-router` crate — [GitHub (`manchoz/arduino-router-rs`)](https://github.com/manchoz/arduino-router-rs) · [crates.io](https://crates.io/crates/arduino-router) · [docs.rs](https://docs.rs/arduino-router)

```text
tactus/
├── Cargo.toml                  # Workspace root (edition 2021, resolver 2)
├── crates/
│   ├── tactus-protocol/        # Shared EngineCommand, Copy RtCommand/TelemetrySnapshot, AppState, TactusConfig
│   ├── tactus-link-sys/        # Offline-safe build.rs + C99 FFI + safe RAII AblLink/SessionState wrappers
│   ├── tactus-sync-core/       # Pure-Rust, OS-independent, zero-alloc bidirectional sync engine
│   └── tactusd/                # Appliance daemon (RT sync engine, ALSA RawMIDI hotplug, atomic config, embedded Web UI, optional MCU RPC)
├── mcu/
│   └── tactus-zephyr/          # Optional UNO Q STM32U585 Zephyr sketch (Arduino_RouterBridge + 8x13 LED matrix)
├── system/                     # systemd, hostapd, dnsmasq, avahi, NetworkManager, kernel, scripts, release installers
└── vendor/
    └── ableton-link/           # Pinned Ableton/link submodule (Link-4.1.1 + asio-standalone)
```

---

## Development Environment Setup

### 1. Prerequisites

- **Rust toolchain** via [`rustup`](https://rustup.rs/) (`rustc`, `cargo`, `rustfmt`, `clippy`)
- **C++17 compiler** for compiling `abl_link.cpp` from `vendor/ableton-link`:
  - **macOS:** Xcode Command Line Tools (`xcode-select --install`, provides Apple Clang)
  - **Debian/Ubuntu:** `sudo apt-get install build-essential git`
  - *(Note: `cmake` is **not** required for host builds — `tactus-link-sys/build.rs` compiles `extensions/abl_link/src/abl_link.cpp` directly via the `cc` crate.)*
- **Git** with submodule support

### 2. Clone with Submodules (Offline Vendoring)

`vendor/ableton-link` (`Link-4.1.1`) uses a nested submodule (`modules/asio-standalone`, the cross-platform standalone Asio C++ networking headers required by Link). Initialize submodules recursively:

```bash
git clone --recurse-submodules https://github.com/manchoz/tactus.git
cd tactus
```

If you already cloned without `--recurse-submodules`:

```bash
git submodule update --init --recursive
```

### 3. Host Build, Lint, and Test

The entire workspace (`tactus-sync-core`, `tactus-protocol`, `tactus-link-sys`, and `tactusd`) builds and passes deterministic tests on the development host (macOS or Linux) without ALSA or Link hardware attached:

```bash
# Format check
cargo fmt --check

# Clippy lints (warnings are denied)
cargo clippy --workspace --all-targets -- -D warnings

# Run all workspace unit, protocol, FFI, and deterministic simulation tests
cargo test --workspace

# Architectural invariant, Web UI JS, shell script, and release installer checks (same as CI)
./system/scripts/check-invariants.sh
node --check crates/tactusd/web/app.js
bash -n system/scripts/*.sh system/release/install.sh
shellcheck system/scripts/*.sh system/kernel/*.sh system/release/install.sh
./system/scripts/test-release-scripts.sh
```

Run `tactusd` locally on the host (uses defaults if `/var/lib/tactus/config.toml` is missing, or pass `--config <PATH>`):

```bash
cargo run -p tactusd
```

### 4. Cross-Compiling for ARM64 Linux SBCs (`aarch64-unknown-linux-gnu`)

Install the Rust target once:

```bash
rustup target add aarch64-unknown-linux-gnu
```

#### Option A: macOS (or Linux) via `cargo-zigbuild`

Install Zig and `cargo-zigbuild`:

```bash
# macOS (Homebrew)
brew install zig
cargo install cargo-zigbuild
```

Build the workspace for `aarch64-unknown-linux-gnu` (pinning glibc baseline as needed for the target Debian image, e.g. `2.36`):

```bash
cargo zigbuild --workspace --target aarch64-unknown-linux-gnu.2.36
```

Verify the resulting ELF binary:

```bash
file target/aarch64-unknown-linux-gnu/debug/tactusd
```

#### Option B: Debian / Ubuntu Linux Native Cross-Toolchain

Install the GNU `aarch64` C/C++ cross-compiler:

```bash
sudo apt-get install gcc-aarch64-linux-gnu g++-aarch64-linux-gnu
```

Build with explicit cross linker and C++ compiler env vars:

```bash
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++ \
cargo build --workspace --target aarch64-unknown-linux-gnu
```

*(Note: `tactusd` talks directly to Linux kernel ALSA RawMIDI UAPI ioctls and `NETLINK_KOBJECT_UEVENT` via `libc`, so `cargo zigbuild` requires no target `libasound2-dev` or `libudev` sysroot.)*

### 5. Prebuilt `abl_link` Overrides (Yocto / BitBake Offline Builds)

`crates/tactus-link-sys/build.rs` never accesses the network or runs `git`. It resolves `abl_link` in the following priority order:

1. **`ABL_LINK_LIB_DIR` env var:** Links directly against a prebuilt library directory (skipping C++ compilation). Optional overrides:
   - `ABL_LINK_LIB_NAME` (defaults to `abl_link`)
   - `ABL_LINK_LINK_KIND` (`static` or `dylib`; auto-detects `lib<name>.a`)
2. **`pkg-config`:** Probes `abl-link`, then `abl_link`.
3. **Vendored fallback:** Compiles `vendor/ableton-link/extensions/abl_link/src/abl_link.cpp` offline (override directory via `ABL_LINK_VENDOR_DIR`).

---

## Provisioning from Source Repository (Arduino UNO Q)

A fresh UNO Q image boots with SSH masked and only USB-C `adb` active (Arduino App Lab is **not** required). From the host:

```bash
# Factory board over USB-C ADB (temporarily connects WiFi to fetch hostapd/dnsmasq/iw,
# then deletes the temporary WiFi profile, enables SSH + key auth, configures Tactus AP,
# trims GUI/PipeWire/extra Arduino services, sets volatile journald, and verifies):
./system/scripts/provision-factory-unoq.sh --adb \
  --pubkey ~/.ssh/id_ed25519.pub \
  --wifi-ssid "<YourWiFi>" --wifi-psk "<WiFiPassword>" \
  --nopasswd-sudo

# Existing / Ethernet-connected UNO Q over SSH (idempotent):
./system/scripts/provision-factory-unoq.sh --ssh arduino@tactus.local --nopasswd-sudo
```

Run `./system/scripts/provision-factory-unoq.sh --help` for offline `--deb-dir`, custom `--psk`, `--stage-only`, and `--rollback-ap` options.

### 5 GHz AP Kernel (Optional, UNO Q)

`tactus-ap.service` defaults to 5 GHz channel `36` (`20 MHz` VHT) and automatically falls back to 2.4 GHz channel `11` if the board's kernel marks channel `36` `NO-IR` (true on the stock UNO Q kernel). To build a custom Debian kernel `.deb` enabling 5 GHz AP mode ([`system/kernel/tactus-5ghz.config`](system/kernel/tactus-5ghz.config)), run `./system/scripts/build-unoq-kernel-local.sh` (or trigger the `Build UNO Q 5GHz Kernel` GitHub Actions workflow).

### Release Bundle Packaging

To assemble a local offline release archive (`.tar.gz` / `.zip` with `SHA256SUMS`), use `./system/scripts/package-release.sh`; official release artifacts are built by [`.github/workflows/release-unoq.yml`](.github/workflows/release-unoq.yml). See [system/release/INSTALL.md](system/release/INSTALL.md) for the turnkey bundle layout and installer flags.

### Linux `LED1` Sysfs & Fault Unit

`tactusd` drives the onboard RGB `LED1` (`/sys/class/leds/unoq:user-{red,green,blue}1`) for boot, AP/client connectivity, and sync status, yielding to a fast red blink if any systemd unit triggers `tactus-led-fault@.service`.

---

## Network Discovery & Board Diagnostics (Bonjour / SSH)

Provisioned boards announce `_tactus._tcp` (with `app`, `version`, `board`, `serial_number`, `path`, `ws`, `ap_ssid`, and `ssh` TXT records) and `_http._tcp` on port `80` across all active interfaces (`wlan0` AP and Ethernet LAN):

```bash
# macOS (when connected to the 'Tactus' WiFi AP or same Ethernet LAN)
dns-sd -B _tactus._tcp local.
dns-sd -L tactus _tactus._tcp local.

# Linux (Avahi)
avahi-browse -rt _tactus._tcp
```

To inspect daemon and AP logs on the board over SSH (`ssh arduino@tactus.local`):

```bash
journalctl -u tactusd -u tactus-ap -n 100 --no-pager
```

---

## Runtime & Configuration Reference

- **Daemon binary:** `tactusd`
- **mDNS hostname:** `tactus.local`
- **Config path:** `/var/lib/tactus/config.toml` (override via `--config` or `TACTUS_CONFIG`; schema defined in [SPEC.md §5](SPEC.md#L65-L93))
- **Runtime socket / temp path:** `/run/tactus/` (`tmpfs`)
