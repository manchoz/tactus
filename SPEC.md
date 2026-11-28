# SPEC.md — tac•tus Technical Specification

## 1. Scope & Roadmap Phases
- **v1 (PoC — Current Target):** Stock Debian on a Linux SBC (reference board: Arduino UNO Q), class-compliant USB-MIDI / BLE-MIDI (e.g., CME WIDI Uhost) over USB Host, dedicated WiFi AP, Rust `tactusd` bidirectional sync engine, mobile-first Web UI ("Add to Home Screen" on iOS/Android), optional MCU LED matrix heartbeat via `arduino-router`.
- **v2 (Field Hardware):** Qwiic OLED + rotary encoder + physical Direction/Resync switches on STM32U585 (Zephyr), optional hardware UART MIDI TRS/DIN, `overlayfs` rootfs protection.
- **v3 (Production Appliance):** Custom Yocto QLI image (`meta-tactus`), `read-only-rootfs`, dedicated `/data` partition, fast boot.

## 2. Sync Engine Specification (`tactus-sync-core`)

### 2.1 Operating Modes & Transitions (User-Selectable)
- `LinkToMidi`: Ableton Link timeline drives MIDI Clock output.
- `MidiToLink`: Incoming MIDI Clock drives Ableton Link tempo and phase.
- **Direction Switch:** Switching direction MUST immediately reset estimator state (`Unlocked`). When leaving `LinkToMidi` while playing and `start_stop_sync = true`, emit MIDI `Stop (0xFC)` cleanly so downstream sequencers do not hang.
- **Clock Domain:** Official `abl_link` reads `CLOCK_MONOTONIC_RAW` on Linux (`abl_link_clock_micros()`), whereas `timerfd_settime(TFD_TIMER_ABSTIME)` uses NTP-slewed `CLOCK_MONOTONIC`. `tactusd` runs `SyncEngine` in `CLOCK_MONOTONIC_RAW` nanoseconds and explicitly bridges deadlines to `CLOCK_MONOTONIC` via `ClockBridge` (`100 ms` bracketed refresh) so slewed drift never accumulates.

### 2.2 Mode A: `LinkToMidi`
- Query Ableton Link session state (`beat`, `phase`, `tempo`, `is_playing`) at each tick deadline.
- Generate 24 PPQN (`0xF8`) ticks scheduled with absolute timestamps (`TIMER_ABSTIME`) so isolated wake-up jitter never accumulates drift.
- **Transport (when `start_stop_sync = true`):**
  - Emit MIDI `Start (0xFA)` quantized to the next Link quantum boundary (default `quantum = 4.0` beats) when Link transitions to playing (including via Web UI `SetLinkPlaying`), when `start_stop_sync` is enabled mid-playback while Link is playing, or on manual `ResyncDownbeat`.
  - Emit MIDI `Stop (0xFC)` immediately when Link stops or when `start_stop_sync` is disabled while playing.
  - When `start_stop_sync = false`, emit continuous `0xF8` clock ticks without `0xFA`/`0xFC`.
- **Latency Offset:** Apply configurable signed offset (`-50.0 ms` to `+50.0 ms`, step `0.5 ms`) to shift the emitted MIDI clock grid relative to Link peers.

### 2.3 Mode B: `MidiToLink` (Dual-Window Estimator + Phase Lock)
Designed for setups where MIDI tempo changes rarely and rock-solid Link BPM stability is prioritized over instant tracking.

1. **Timestamp Capture:**
   - Record each incoming `0xF8` with a monotonic timestamp (`CLOCK_MONOTONIC_RAW` captured immediately on ALSA RawMIDI `/dev/snd/midiC*D*` read in the `SCHED_FIFO` thread; `snd_seq` remains optional for future Yocto `meta-tactus`) and a monotonically increasing `tick_index`.
2. **Acquisition Phase (`Unlocked` → `Acquiring` → `Locked`):**
   - On first `0xF8`, enter `Acquiring` without modifying Ableton Link yet.
   - Once at least `fast_window_secs` (default `1.5 s`, minimum `24` ticks) of valid ticks are collected, commit the initial OLS BPM to Link and transition to `Locked`, then continue growing the long window up to `ols_window_secs` (default `10.0 s`, configurable up to `30.0 s`).
3. **Long Window (Frequency Lock — `ols_window_secs` up to `30.0 s`):**
   - Maintain a fixed-capacity ring buffer (`4096` samples, zero heap allocations; covers up to `340 BPM` over `30 s` or `999 BPM` over `10 s`) storing `(tick_index, timestamp_ns)` pairs within `ols_window_secs`.
   - Compute tempo via contiguous-slice **Ordinary Least Squares (OLS) linear regression** (`SampleRing::as_slices`) of `timestamp_ns` vs `tick_index`. Slope `m` (ns/tick) yields `BPM = 60_000_000_000.0 / (24.0 * m)`.
   - Once `Locked`, evaluate the long OLS window on every `6` ticks and all beat boundaries to minimize hot-path CPU overhead.
   - Apply a deadband (default `±0.05 BPM`): commit the new tempo to Ableton Link only when the OLS estimate differs from the current committed Link BPM by more than the deadband. Optionally snap committed BPM to the nearest integer when `round_bpm = true` (preserving unrounded diagnostics).
4. **Fast Window (Step-Change Detector — 1.5 s):**
   - Maintain a secondary short OLS window (default `1.5 s`, ~72 ticks at 120 BPM).
   - If `|fast_bpm - committed_bpm| > step_change_bpm` (default `1.5 BPM`) consistently for `step_change_beats` consecutive beats (default `3` beats), declare an intentional tempo change: flush the long buffer, immediately commit `fast_bpm` to Link, and resume long-window integration.
5. **Phase Lock & Transport:**
   - Apply symmetric `midi_offset_ms` (`±50.0 ms`) to incoming `Start (0xFA)` and beat-boundary denoised timestamps (negative offset compensates incoming MIDI/BLE-MIDI transport latency symmetrically with `LinkToMidi`).
   - On MIDI `Start (0xFA)`: reset `tick_index = 0`, align Link beat 0 at the offset-adjusted timestamp via `force_beat_at_time()`, and start Link transport via `set_is_playing()` (1 ms pre-downbeat margin so remote Link peers align immediately without a 1-bar preroll) if `start_stop_sync = true`.
   - On MIDI `Continue (0xFB)`: resume without resetting bar phase; start Link transport if `start_stop_sync = true`.
   - On MIDI `Stop (0xFC)`: stop Link transport if `start_stop_sync = true`; keep tempo tracking if `0xF8` ticks continue to arrive.
   - Every 24 ticks (quarter note), measure phase error between the MIDI beat timestamp and Link timeline phase.
   - If `|phase_error| > phase_deadband_ms` (default `4.0 ms`), apply a gentle bounded nudge (max `1.0 ms` correction per beat) rather than abrupt jumps, preventing audible pitch artifacts on Link loopers/DAWs.
   - On MIDI clock timeout (no `0xF8` for `> 500 ms`), transition to `Unlocked`, clear estimator buffers, and stop pushing updates to Link.
   - **RT Clock Thru (`clock_thru = true`):** Forward incoming `0xF8` (plus `0xFA`/`0xFB`/`0xFC` when `start_stop_sync = true`) directly to all other selected MIDI outputs, automatically skipping the active input port descriptor to prevent clock feedback loops. Disabling `clock_thru` while playing emits `Stop (0xFC)` cleanly to relayed outputs.

## 3. MIDI I/O Specification
- Direct kernel ALSA RawMIDI (`/dev/snd/midiC*D*`) via UAPI ioctls on stock Debian (optional `snd_seq` in future Yocto `meta-tactus`).
- Select 1 MIDI input (`input_port_match`) and up to 8 simultaneous MIDI outputs (`output_port_matches`) matched by persistent card/port name substring or `hw:C,D,S` in `config.toml`.
- Automatically handle USB hotplug (`NETLINK_KOBJECT_UEVENT` + 5 s fallback rescan) and per-output fault isolation without restarting `tactusd`.

## 4. Network & Mobile Web UI Specification
- **WiFi AP:** Dedicated local AP (`hostapd`) tuned for low-latency Ableton Link UDP multicast (`country_code=IT`, `multicast_to_unicast=1`, `dtim_period=1`, `ap_isolate=0`), defaulting to 5 GHz non-DFS channel `36` (`5180 MHz`, `20 MHz` VHT) on radios whose kernel permits 5 GHz AP mode, with automatic fallback to 2.4 GHz channel `11` (`2462 MHz`, `20 MHz`, clear of BLE advertising channels `2402`/`2426`/`2480 MHz`) when 5 GHz channel `36` is `NO-IR` (for example on the stock UNO Q kernel).
- **DHCP/mDNS & Zeroconf Discovery:** Subnet `10.42.0.1/24`. `dnsmasq` MUST omit default router (`dhcp-option=3`) and DNS (`dhcp-option=6`) to prevent iOS/Android Captive Portal prompts and keep cellular data usable. Avahi announces `tactus.local` along with `_tactus._tcp` (port `80`, TXT keys `app`, `version`, `board`, `serial_number`, `path`, `ws`, `ap_ssid`, `ssh`) and `_http._tcp` (port `80`, `path=/`) on all active IP interfaces (`wlan0` AP and Ethernet), while stock `_arduino._tcp` (`arduino.service`) is disabled when trimmed.
- **Web UI:** Single embedded vanilla HTML/CSS/JS bundle served over HTTP on port `80`.
  - Meta tags for iOS fullscreen Home Screen app (`apple-mobile-web-app-capable`, `apple-mobile-web-app-status-bar-style`, SVG/PNG touch icon).
  - WebSocket `/ws` pushing telemetry snapshots at `10–15 Hz` (plus immediate push on state/downbeat change).
  - Automatic silent reconnect on `document.visibilitychange` and network resume, immediately hydrating UI from a full state snapshot.
  - Core controls: Direction toggle (`Link → MIDI` / `MIDI → Link`), Start/Stop Sync toggle, Play/Stop transport control (`Link → MIDI`), Round BPM toggle (`MIDI → Link`), BPM & Lock status readout, Link peer count, Downbeat indicator, Latency offset slider (`±50 ms`), MIDI input/output port selectors (up to `8` simultaneous outputs), Resync button, and feature-detected Screen Wake Lock (`navigator.wakeLock` when available in secure contexts).

## 5. Configuration Schema (`/var/lib/tactus/config.toml`)
```toml
[sync]
direction = "link_to_midi"        # "link_to_midi" | "midi_to_link"
start_stop_sync = true
clock_thru = true                 # Relay incoming MIDI clock/transport to other selected outputs in midi_to_link
quantum = 4.0
initial_bpm = 120.0               # Standalone Link tempo restored on boot when no peers are present
round_bpm = false                 # Snap committed MidiToLink BPM to nearest integer BPM
midi_offset_ms = 0.0
ols_window_secs = 10.0            # Max 30.0 s
fast_window_secs = 1.5
bpm_deadband = 0.05
step_change_bpm = 1.5
step_change_beats = 3
phase_deadband_ms = 4.0
max_phase_nudge_ms = 1.0

[midi]
input_port_match = "WIDI"         # Substring match on ALSA RawMIDI port name
output_port_matches = ["WIDI"]    # Up to 8 output port match rules (legacy output_port_match supported)

[web]
listen_addr = "0.0.0.0:80"

[mcu_bridge]
enabled = false                   # true when Zephyr sketch is active
socket_path = "/var/run/arduino-router.sock"
```
