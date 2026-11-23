# Tactus

**Tactus** is a standalone embedded Rust appliance (targeting the Arduino UNO Q) that bidirectionally bridges USB-MIDI / BLE-MIDI (via CME WIDI Uhost in PoC) and **Ableton Link** over a dedicated 5 GHz WiFi Access Point.

## Architectural Highlights

- **Userspace Real-Time Determinism:** Targets stock Debian Linux (and future Yocto QLI `meta-tactus`) without `PREEMPT_RT` or PipeWire — uses a dedicated `SCHED_FIFO` thread, `mlockall`, `CLOCK_MONOTONIC` absolute timers (`TIMER_ABSTIME`), CPU core affinity, and direct ALSA RawMIDI (`/dev/snd/midiC*D*`) I/O (keeping `/dev/snd/seq` optional for Yocto `meta-tactus`).
- **Strict RT vs. Async Separation:** The RT sync thread performs zero heap allocations, zero mutexes, zero blocking I/O, and zero `tokio` calls; the `tokio` control plane (`axum` HTTP/WebSocket, config persistence, MCU RPC) communicates exclusively through lock-free SPSC ring buffers (`rtrb`) with strictly `Copy` `TelemetrySnapshot` and `RtCommand` messages.
- **Official Ableton Link C API:** Wraps `Ableton/link`'s `extensions/abl_link` C99 API (`tactus-link-sys`), avoiding custom Link protocol reimplementations.
- **Yocto & Read-Only Rootfs Ready:** Offline builds with zero network/`git` activity in `build.rs`, dual prebuilt/vendored `abl_link` resolution, atomic state writes restricted to `/var/lib/tactus/config.toml`, and runtime state on `tmpfs` (`/run/tactus/`).

## Project Status

| Milestone | Scope | Status |
| :--- | :--- | :--- |
| **M0** | Cargo workspace & offline-safe `abl_link` FFI build | Complete |
| **M1** | Pure-Rust `tactus-sync-core` & deterministic simulations | Complete |
| **M2** | WiFi AP (`hostapd`/`dnsmasq`/`avahi`), Zeroconf & UNO Q provisioning | Complete |
| **M3** | Headless `tactusd` RT engine, ALSA RawMIDI hotplug & atomic `config.toml` | Complete |
| **M4** | Embedded mobile Web UI (`axum` + WebSocket + Home Screen PWA) | Planned |
| **M5** | Optional STM32U585 Zephyr LED matrix bridge (`Arduino_RouterBridge`) | Planned |

## Documentation & Workspace Layout

- [SPEC.md](SPEC.md) — Functional, bidirectional sync algorithm, network, Web UI, and `config.toml` specifications
- [ARCHITECTURE.md](ARCHITECTURE.md) — Monorepo layout, threading model, protocol contract, and boot sequence
- [MILESTONES.md](MILESTONES.md) — Phased execution and verification plan
- [AGENTS.md](AGENTS.md) — Non-negotiable architectural invariants and verification gates

```text
tactus/
├── Cargo.toml                  # Workspace root (edition 2021, resolver 2)
├── crates/
│   ├── tactus-protocol/        # Shared EngineCommand, Copy RtCommand/TelemetrySnapshot, AppState, TactusConfig
│   ├── tactus-link-sys/        # Offline-safe build.rs + C99 FFI + safe RAII AblLink/SessionState wrappers
│   ├── tactus-sync-core/       # Pure-Rust, OS-independent, zero-alloc bidirectional sync engine
│   └── tactusd/                # Headless daemon (RT sync engine, ALSA RawMIDI hotplug, atomic config; Web/RPC in M4–M5)
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

`tactus-sync-core`, `tactus-protocol`, and `tactus-link-sys` build and pass deterministic tests on the development host (macOS or Linux) without ALSA or Link hardware attached:

```bash
# Format check
cargo fmt --check

# Clippy lints (warnings are denied)
cargo clippy --workspace --all-targets -- -D warnings

# Run all workspace unit, protocol, FFI, and deterministic simulation tests
cargo test --workspace
```

Run `tactusd` locally on the host (uses defaults if `/var/lib/tactus/config.toml` is missing, or pass `--config <PATH>`):

```bash
cargo run -p tactusd
```

### 4. Cross-Compiling for Arduino UNO Q (`aarch64-unknown-linux-gnu`)

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

## Provisioning a Factory Arduino UNO Q

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

### Network Discovery (Bonjour / Zeroconf)

Provisioned boards announce `_tactus._tcp` (with `app`, `version`, `board`, `serial_number`, `path`, `ws`, `ap_ssid`, and `ssh` TXT records) and `_http._tcp` on port `80` across all active interfaces (`wlan0` AP and Ethernet LAN):

```bash
# macOS (when connected to the 'Tactus' WiFi AP or same Ethernet LAN)
dns-sd -B _tactus._tcp local.
dns-sd -L tactus _tactus._tcp local.

# Linux (Avahi)
avahi-browse -rt _tactus._tcp
```

---

## Runtime & Configuration Reference

- **Daemon binary:** `tactusd`
- **mDNS hostname:** `tactus.local`
- **Config path:** `/var/lib/tactus/config.toml` (override via `--config` or `TACTUS_CONFIG`; schema defined in [SPEC.md §5](SPEC.md#L62-L87))
- **Runtime socket / temp path:** `/run/tactus/` (`tmpfs`)

---

## License

Tactus original source code is licensed under the **Mozilla Public License 2.0 (`MPL-2.0`)** without Exhibit B incompatibility — see [LICENSE](LICENSE) and [Cargo.toml](Cargo.toml).

Tactus links against official [`Ableton/link`](vendor/ableton-link/LICENSE.md), which is licensed under **`GPL-2.0-or-later`** (with a commercial licensing option from Ableton AG). Under MPL-2.0 §3.3 (*Larger Work*), compiled `tactusd` binaries and appliance images incorporating open-source `Ableton/link` are distributed under **`GPL-2.0-or-later`**, while Tactus's own source files remain available under `MPL-2.0`.
