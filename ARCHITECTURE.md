# ARCHITECTURE.md — Tactus System & Software Design

## 1. Monorepo Layout
```text
tactus/
├── AGENTS.md
├── SPEC.md
├── ARCHITECTURE.md
├── MILESTONES.md
├── Cargo.toml                        # Workspace root
├── deny.toml                         # cargo-deny license & dependency policy
├── vendor/
│   └── ableton-link/                 # Vendored Ableton/link (submodule or tarball)
├── crates/
│   ├── tactus-link-sys/              # FFI bindings + safe wrapper around extensions/abl_link
│   ├── tactus-sync-core/             # Pure Rust sync state machine, OLS (up to 30 s), step detector, phase lock (no ALSA/OS deps)
│   ├── tactus-protocol/              # Shared Command / Telemetry / Config types (serde JSON & TOML)
│   └── tactusd/                      # Main daemon binary (RT thread + ALSA RawMIDI + tokio/axum + embedded web UI + MCU RPC + LED1)
│       └── web/                      # Vanilla HTML/CSS/JS/icons embedded via include_str!/include_bytes!
├── mcu/
│   └── tactus-zephyr/                # STM32U585 Zephyr sketch using Arduino_RouterBridge (v1 LED matrix + RGB LED3, v2 OLED/encoder)
└── system/                           # Portable 1:1 between Debian PoC and Yocto meta-tactus
    ├── NetworkManager/
    │   └── 99-tactus-unmanaged.conf
    ├── hostapd/
    │   ├── tactus-ap.conf            # 5 GHz ch 36 (default when kernel clears NO-IR)
    │   └── tactus-ap-2g.conf         # 2.4 GHz ch 11 automatic fallback
    ├── dnsmasq/
    │   └── tactus-ap.conf
    ├── avahi/
    │   └── tactus.service
    ├── config/
    │   └── config.toml               # Default appliance config template
    ├── kernel/                       # UNO Q 5 GHz Kconfig fragment + reproducible Debian kernel build
    ├── modules-load/
    │   └── 99-tactus-wifi.conf
    ├── openocd/
    │   ├── unoq-gpiod.cfg            # linuxgpiod SWD config for UNO Q STM32U585
    │   └── unoq-flash-sketch.cfg     # Idempotent OpenOCD verify/write at 0x08100000
    ├── scripts/                      # Provisioning, deploy, MCU flash, kernel build, and CI check-invariants.sh
    ├── tmpfiles/                     # Boot LED1 trigger & THP configuration
    └── systemd/
        ├── tactus-ap.service         # Sets IP on wlan0, selects 5 GHz/2.4 GHz hostapd, Upholds=tactus-dhcp.service
        ├── tactus-dhcp.service       # Supervises foreground dnsmasq DHCP server (BindsTo=tactus-ap)
        ├── tactus-led-fault@.service # Writes per-service fault sentinel & drives LED1 red blink on unit failure
        └── tactusd.service           # Runs tactusd with CAP_SYS_NICE, decoupled from tactus-ap (Wants= only)
```

## 2. Threading & Concurrency Model (`tactusd`)
1. **CPU Isolation & RT Thread (`rt-sync`, `std::thread`):**
   - `main()` calls `pin_non_rt_threads(RT_PREFERRED_CORE)` at startup before spawning `rt-sync` or `tokio`, confining `main`, `tokio` worker/blocking threads, and `abl_link`'s Asio thread to Cores `0–2`.
   - `rt-sync` then pins itself to dedicated Core `3` with `SCHED_FIFO` priority `75` + `mlockall(MCL_CURRENT | MCL_FUTURE)`.
   - Owns active ALSA RawMIDI file descriptors (handed off from async), `abl_link` session handle, `ClockBridge` (`CLOCK_MONOTONIC_RAW` <-> `CLOCK_MONOTONIC`), and `SyncEngine` from `tactus-sync-core`.
   - Polls incoming `Copy` `RtMessage` items (`RtCommand` + `AttachMidi`/`Shutdown`) from `rtrb::Consumer<RtMessage>` (non-blocking) and returns replaced/detached FDs over a `256`-slot `rtrb::Producer<FdReturn>` backed by a stack-resident `pending_fd_return` retry slot so `RawFd`s never drop.
   - Pushes `TelemetrySnapshot` items to `rtrb::Producer<TelemetrySnapshot>` with sticky `prev_snap` tracking (only advancing `prev_snap` when `push()` succeeds so dropped/full retries preserve discrete edge notifications) and signals `rt_notify_tx` (`eventfd` -> `AsyncFd` on Linux) on `FdReturn` pushes, discrete state transitions, every integer beat boundary (`beat_index`), and the `10 Hz` (`100 ms`) heartbeat (never on 24 PPQN ticks).
2. **Async Runtime (`tokio`, Cores 0–2, normal priority):**
   - **State Hub Task:** Wakes immediately on `rt_notify_rx` (`AsyncFd` on Linux, with a `250 ms` safety fallback poll; `50 ms` on non-Linux hosts), drains `FdReturn` / `TelemetrySnapshot`, updates `tokio::sync::watch::Sender<AppState>`, and dispatches `EngineCommand` to the RT thread.
   - **Web Server Task (`axum`):** Serves embedded UI + `/ws` WebSocket endpoint subscribed to `watch::Receiver<AppState>` with `TCP_NODELAY` enabled on accepted TCP sockets (`tap_io`).
   - **Persistence Task:** Debounces config changes and executes atomic write (`config.toml.tmp` → `fsync` → `rename`).
   - **MCU RPC Task (optional in v1, active in v2):** Connects to `/var/run/arduino-router.sock` via the standalone [`arduino-router`](https://github.com/manchoz/arduino-router-rs) crate (`ManagedClient`), sends low-rate `Bridge.notify()` telemetry (`tactus/state`, `tactus/downbeat`, `tactus/beat` — zero 24 PPQN traffic), and translates debounced incoming MCU commands into `EngineCommand`.
   - **Status LED Task (`status_led.rs`):** Drives Linux `LED1` (`/sys/class/leds/unoq:user-{red,green,blue}1`) for boot, connectivity, and status indication, yielding whenever any per-service `/run/tactus/fault.<unit>.service` sentinel written by `tactus-led-fault@.service` is present.

## 3. Unified Command & Telemetry Contract (`tactus-protocol`)
Both WebSocket (JSON) and `arduino-router` (MessagePack-RPC) share the exact same semantic model:

- **`EngineCommand`:**
  - `SetDirection(Direction)`
  - `SetStartStopSync(bool)`
  - `SetClockThru(bool)`
  - `SetOffsetMs(f32)`
  - `SetQuantum(f64)`
  - `SelectMidiPorts { input: String, outputs: Vec<String> }`
  - `ResyncDownbeat`
  - `SetLinkTempoManual(f64)` (when in `LinkToMidi` mode)
  - `SetLinkPlaying(bool)` (when in `LinkToMidi` mode)
  - `SetRoundBpm(bool)` (when in `MidiToLink` mode)
- **`TelemetrySnapshot` (strictly `Copy`, zero-alloc):**
  - `direction: Direction`
  - `lock_state: LockState` (`Unlocked` | `Acquiring` | `Locked`)
  - `start_stop_sync: bool`
  - `clock_thru: bool`
  - `bpm_link: f64`
  - `bpm_midi_long: Option<f64>`
  - `bpm_midi_fast: Option<f64>`
  - `phase_error_ms: f32`
  - `beat_phase: f64` (`0.0..quantum`)
  - `quantum: f64`
  - `link_peers: usize`
  - `is_playing: bool`
  - `midi_offset_ms: f32`
  - `round_bpm: bool`
- **`AppState` (async control plane `watch::Sender<AppState>`):**
  - Wraps `telemetry: TelemetrySnapshot` alongside `available_midi_ports: Vec<MidiPortInfo>`, `selected_midi_input: Option<String>`, `selected_midi_outputs: Vec<String>`, `midi_input_match: String`, and `midi_output_matches: Vec<String>`.

## 4. `tactus-link-sys` Dual Build Strategy (Debian + Yocto)
`build.rs` checks in order:
1. If `ABL_LINK_LIB_DIR` env var or `pkg-config` finds `abl-link`, link dynamically/statically against the prebuilt system library (Yocto BitBake path — zero C++ compilation inside Cargo).
2. Otherwise, compile vendored `vendor/ableton-link/extensions/abl_link` via `cc` with zero network access (Debian PoC / host dev path).

## 5. System & Network Boot Sequence
1. `NetworkManager` starts and ignores `wlan0` / `p2p-dev-wlan0` (`99-tactus-unmanaged.conf`), while managing USB-Ethernet for SSH/dev.
2. `tactus-ap.service` (`BindsTo=sys-subsystem-net-devices-wlan0.device`, `After=NetworkManager.service systemd-rfkill.service`) brings up `wlan0`, assigns `10.42.0.1/24` via `ip addr replace`, sets regdomain (`iw reg set IT`), selects 5 GHz `tactus-ap.conf` or falls back to 2.4 GHz `tactus-ap-2g.conf` if ch `36` reports `no IR`, supervises `hostapd`, and upholds `tactus-dhcp.service` (`Upholds=tactus-dhcp.service`). `tactus-dhcp.service` (`BindsTo=tactus-ap.service`) supervises foreground `dnsmasq` (DHCP no-GW/no-DNS).
3. `avahi-daemon` (`use-ipv6=no` in `/etc/avahi/avahi-daemon.conf`) publishes `tactus.local` over IPv4 alongside `_tactus._tcp` and `_http._tcp`.
4. `tactusd.service` starts immediately (`After=network.target arduino-router.service`, `Wants=arduino-router.service tactus-ap.service` — intentionally decoupled from `tactus-ap.service` with no `After=`) with `CapabilityBoundingSet=CAP_SYS_NICE CAP_IPC_LOCK CAP_NET_BIND_SERVICE` and `ReadWritePaths=/var/lib/tactus /run/tactus -/run/arduino-router.sock -/sys/class/leds/unoq:user-{red,green,blue}1`.
