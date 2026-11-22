# SPEC.md — Tactus Technical Specification

## 1. Scope & Roadmap Phases
- **v1 (PoC — Current Target):** Debian stock on UNO Q, CME WIDI Uhost on USB-C Host hub, 5 GHz WiFi AP, Rust `tactusd` bidirectional sync engine, mobile-first Web UI ("Add to Home Screen" on iOS/Android), optional 8×13 LED matrix heartbeat via `arduino-router`.
- **v2 (Field Hardware):** Qwiic OLED + rotary encoder + physical Direction/Resync switches on STM32U585 (Zephyr), optional hardware UART MIDI TRS/DIN, `overlayfs` rootfs protection.
- **v3 (Production Appliance):** Custom Yocto QLI image (`meta-tactus`), `read-only-rootfs`, dedicated `/data` partition, fast boot.

## 2. Sync Engine Specification (`tactus-sync-core`)

### 2.1 Operating Modes & Transitions (User-Selectable)
- `LinkToMidi`: Ableton Link timeline drives MIDI Clock output.
- `MidiToLink`: Incoming MIDI Clock drives Ableton Link tempo and phase.
- **Direction Switch:** Switching direction MUST immediately reset estimator state (`Unlocked`). When leaving `LinkToMidi` while playing and `start_stop_sync = true`, emit MIDI `Stop (0xFC)` cleanly so downstream sequencers do not hang.
- **Clock Domain:** Use `abl_link_clock_micros()` for all Ableton Link queries (`beat_at_time`, `time_at_beat`) and `CLOCK_MONOTONIC` for RT scheduling (`clock_nanosleep` with `TIMER_ABSTIME`). On Linux both map to the monotonic clock; maintain a single monotonic microsecond/nanosecond reference inside `tactus-sync-core`.

### 2.2 Mode A: `LinkToMidi`
- Query Ableton Link session state (`beat`, `phase`, `tempo`, `is_playing`) at each tick deadline.
- Generate 24 PPQN (`0xF8`) ticks scheduled with absolute timestamps (`TIMER_ABSTIME`) so isolated wake-up jitter never accumulates drift.
- **Transport (when `start_stop_sync = true`):**
  - Emit MIDI `Start (0xFA)` quantized to the next Link quantum boundary (default `quantum = 4.0` beats) when Link transitions to playing or on manual `ResyncDownbeat`.
  - Emit MIDI `Stop (0xFC)` immediately when Link stops.
  - When `start_stop_sync = false`, emit continuous `0xF8` clock ticks without `0xFA`/`0xFC`.
- **Latency Offset:** Apply configurable signed offset (`-50.0 ms` to `+50.0 ms`, step `0.5 ms`) to shift the emitted MIDI clock grid relative to Link peers.

### 2.3 Mode B: `MidiToLink` (Dual-Window Estimator + Phase Lock)
Designed for setups where MIDI tempo changes rarely and rock-solid Link BPM stability is prioritized over instant tracking.

1. **Timestamp Capture:**
   - Record each incoming `0xF8` with a monotonic timestamp (`CLOCK_MONOTONIC` captured immediately on `snd_seq_event_input` in the `SCHED_FIFO` thread, or via an ALSA timestamping queue) and a monotonically increasing `tick_index`.
2. **Acquisition Phase (`Unlocked` → `Acquiring` → `Locked`):**
   - On first `0xF8`, enter `Acquiring` without modifying Ableton Link yet.
   - Once at least `fast_window_secs` (default `1.5 s`, minimum `24` ticks) of valid ticks are collected, commit the initial OLS BPM to Link and transition to `Locked`, then continue growing the long window up to `ols_window_secs` (`10.0 s`).
3. **Long Window (Frequency Lock — 10.0 s):**
   - Maintain a fixed-capacity ring buffer (`2048` samples, zero heap allocations; covers up to 512 BPM over 10 s) storing `(tick_index, timestamp_ns)` pairs within the last **10.0 seconds**.
   - Compute tempo via **Ordinary Least Squares (OLS) linear regression** of `timestamp_ns` vs `tick_index`. Slope `m` (ns/tick) yields `BPM = 60_000_000_000.0 / (24.0 * m)`.
   - Apply a deadband (default `±0.05 BPM`): commit the new tempo to Ableton Link only when the OLS estimate differs from the current committed Link BPM by more than the deadband.
4. **Fast Window (Step-Change Detector — 1.5 s):**
   - Maintain a secondary short OLS window (default `1.5 s`, ~72 ticks at 120 BPM).
   - If `|fast_bpm - committed_bpm| > step_change_bpm` (default `1.5 BPM`) consistently for `step_change_beats` consecutive beats (default `3` beats), declare an intentional tempo change: flush the 10 s buffer, immediately commit `fast_bpm` to Link, and resume long-window integration.
5. **Phase Lock & Transport:**
   - On MIDI `Start (0xFA)`: reset `tick_index = 0`, align Link beat 1 via `request_beat_at_time()`, and start Link transport if `start_stop_sync = true`.
   - On MIDI `Continue (0xFB)`: resume without resetting bar phase; start Link transport if `start_stop_sync = true`.
   - On MIDI `Stop (0xFC)`: stop Link transport if `start_stop_sync = true`; keep tempo tracking if `0xF8` ticks continue to arrive.
   - Every 24 ticks (quarter note), measure phase error between the MIDI beat timestamp and Link timeline phase.
   - If `|phase_error| > phase_deadband_ms` (default `4.0 ms`), apply a gentle bounded nudge (max `1.0 ms` correction per beat) rather than abrupt jumps, preventing audible pitch artifacts on Link loopers/DAWs.
   - On MIDI clock timeout (no `0xF8` for `> 500 ms`), transition to `Unlocked`, clear estimator buffers, and stop pushing updates to Link.

## 3. MIDI I/O Specification
- Direct ALSA Sequencer (`snd_seq`) client named `Tactus` (no PipeWire).
- Subscribe/publish to selected USB-MIDI ports (including CME WIDI Uhost) matched by persistent client/port name substring in `config.toml`.
- Automatically handle USB hotplug (`SND_SEQ_EVENT_PORT_START`, `SND_SEQ_EVENT_PORT_EXIT`) without restarting `tactusd`.

## 4. Network & Mobile Web UI Specification
- **WiFi AP:** 5 GHz only, non-DFS channel (default `36`, selectable `36/40/44/48`), `country_code=IT`, `multicast_to_unicast=1`, `dtim_period=1`.
- **DHCP/mDNS:** Subnet `10.42.0.1/24`. `dnsmasq` MUST omit default router (`dhcp-option=3`) and DNS (`dhcp-option=6`) to prevent iOS/Android Captive Portal prompts and keep cellular data usable. Avahi announces `tactus.local`.
- **Web UI:** Single embedded vanilla HTML/CSS/JS bundle served over HTTP on port `80`.
  - Meta tags for iOS fullscreen Home Screen app (`apple-mobile-web-app-capable`, `apple-mobile-web-app-status-bar-style`, SVG/PNG touch icon).
  - WebSocket `/ws` pushing telemetry snapshots at `10–15 Hz` (plus immediate push on state/downbeat change).
  - Automatic silent reconnect on `document.visibilitychange` and network resume, immediately hydrating UI from a full state snapshot.
  - Core controls: Direction toggle (`Link → MIDI` / `MIDI → Link`), Start/Stop Sync toggle, BPM & Lock status readout, Link peer count, Downbeat indicator, Latency offset slider (`±50 ms`), MIDI input/output port selectors, Resync button.

## 5. Configuration Schema (`/var/lib/tactus/config.toml`)
```toml
[sync]
direction = "link_to_midi"        # "link_to_midi" | "midi_to_link"
start_stop_sync = true
quantum = 4.0
midi_offset_ms = 0.0
ols_window_secs = 10.0
fast_window_secs = 1.5
bpm_deadband = 0.05
step_change_bpm = 1.5
step_change_beats = 3
phase_deadband_ms = 4.0
max_phase_nudge_ms = 1.0

[midi]
input_port_match = "WIDI"         # Substring match on ALSA port name
output_port_match = "WIDI"

[web]
listen_addr = "0.0.0.0:80"

[mcu_bridge]
enabled = false                   # true when Zephyr sketch is active
socket_path = "/var/run/arduino-router.sock"
```
