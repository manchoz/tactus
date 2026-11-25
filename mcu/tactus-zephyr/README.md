# `tactus-zephyr` — UNO Q STM32U585 LED Matrix Companion

Optional Milestone M5 microcontroller firmware for the Arduino UNO Q STM32U585 (Zephyr OS). It connects to `tactusd` on the Linux MPU via `Arduino_RouterBridge` (`/var/run/arduino-router.sock`) and drives both the onboard 8×13 LED matrix (`0..7`) and the onboard hardware-PWM RGB LED (`LED3_R/G/B`).

> **Rate Boundary (`AGENTS.md` Invariant #6):** Only discrete state changes, integer beat pulses (`tactus/beat`), and bar-downbeat pulses (`tactus/downbeat`) are sent over the router bridge—never 24 PPQN (`0xF8`) MIDI Clock ticks.

---

## 8×13 LED Matrix & RGB `LED3` Layout

The matrix is indexed row-major as `frame[row * 13 + col]` (`row = 0..7`, `col = 0..12`, 104 LEDs total) with `setGrayscaleBits(3)` (`0` = off, `7` = max).

```text
col:   0  1  2  3  4  5  6  7  8  9 10 11 12
r0:  [ B  B  B  B  B  B  B  B  B  B  B  B  B ]   Outer border (38 LEDs):
r1:  [ B  .  .  .  .  .  .  >  .  .  .  .  B ]     Downbeat flash (7 -> 0 over 180 ms)
r2:  [ B  .  .  .  .  .  .  .  >  .  .  .  B ]     + corner pulse on Beats 2..N (1 -> 5 -> 1 over 120 ms)
r3:  [ B  .  .  =  =  =  =  =  =  >  .  .  B ]   Center 5x7 arrow:
r4:  [ B  .  .  .  .  .  .  .  >  .  .  .  B ]     Link -> MIDI (right) | MIDI -> Link (left)
r5:  [ B  .  .  .  .  .  .  .  >  .  .  .  B ]
r6:  [ B  .  L  L  .  .  .  .  .  P  P  .  B ]   Status strip: Lock/Link (L) & Play/Stop (P)
r7:  [ B  B  B  B  B  B  B  B  B  B  B  B  B ]
```

### Visual Legend (8×13 Matrix)

| Element | Coordinates | Visual State |
| :--- | :--- | :--- |
| **Standby (waiting for `tactusd`)** | `(r3, c5..7)` | Gentle breathing 3-pixel center bar (`1..5`) until first `tactus/state` arrives |
| **Direction Arrow (`Link -> MIDI`)** | `r1..5, c3..9` | Right arrow (`->`): `3` stopped, `5` playing, `+1` (`6`) on Beats 2..N, punches to `7` on downbeat |
| **Direction Arrow (`MIDI -> Link`)** | `r1..5, c3..9` | Left arrow (`<-`): `3` stopped, `5` playing, `+1` (`6`) on Beats 2..N, punches to `7` on downbeat |
| **Lock Status (`MidiToLink`)** | `(r6, c2..3)` | `Unlocked`: dim 1-px dot `(r6, c2)=1` · `Acquiring`: pulsing `1..6` (`800 ms`) · `Locked`: solid `6` |
| **Clock Ready (`LinkToMidi`)** | `(r6, c2..3)` | Solid `4` when stopped, solid `6` when playing |
| **Transport (`Stopped` vs `Playing`)** | `(r6, c9..10)` + corners | `Stopped`: dim single pip `(r6, c10)=1`, corners `0` · `Playing`: bright `5` bar `(r6, c9..10)`, corners `1` |
| **Downbeat Pulse (Beat 1)** | Perimeter (`r0, r7, c0, c12`) + arrow | Flashes to `7` on `tactus/downbeat` and decays smoothly `7 -> 0` over `180 ms` |
| **Beat Pulse (Beats 2..N)** | 4 corners + arrow/play | Pulses corners `1 -> 5 -> 1` and nudges arrow/play `5 -> 6 -> 5` over `120 ms` on `tactus/beat` |

### Visual Legend (Onboard PWM RGB `LED3`)

| State | Color & Pulse Behavior |
| :--- | :--- |
| **Standby (waiting for `tactusd`)** | Soft breathing blue (`1600 ms` triangle) |
| **Stopped** | Subtle steady teal (`Link -> MIDI`), dim amber (`Unlocked`), breathing amber (`Acquiring`, `800 ms`), or calm emerald (`Locked`) |
| **Playing — Beat 1 (`tactus/downbeat`)** | Bright warm gold/white flash decaying quadratically over `180 ms` to a low direction-colored floor |
| **Playing — Beats 2..N (`tactus/beat`)** | Medium-bright cyan (`Link -> MIDI`), emerald (`MIDI -> Link` locked), or amber (`Acquiring`/`Unlocked`) pulse decaying over `120 ms` |

---

## MessagePack-RPC Wire Contract

Matches `crates/tactusd/src/mcu.rs` over `/var/run/arduino-router.sock` (`arduino-router`):

### MPU (`tactusd`) → MCU (`tactus-zephyr`)

Registered on the MCU via `Bridge.provide_safe(method, fn)`:

| Method | Signature / Payload | Meaning |
| :--- | :--- | :--- |
| `"tactus/state"` | `(int bpm_tenths, int direction, int lock_state, bool is_playing)` | Pushed on connect and on discrete state/lock/direction/play changes or `>= 0.1 BPM` changes (`bpm_tenths = round(bpm_link * 10)`) |
| `"tactus/downbeat"` | `(int bpm_tenths, int direction, int lock_state, bool is_playing)` | Pushed once per bar downbeat wrap (Beat 1) while `is_playing == true` |
| `"tactus/beat"` | `(int beat_index, int bpm_tenths, int direction, int lock_state, bool is_playing)` | Pushed on each non-downbeat integer beat crossing (`beat_index >= 1`) while `is_playing == true` |

Field encodings:
- `direction`: `0` = `LinkToMidi`, `1` = `MidiToLink`
- `lock_state`: `0` = `Unlocked`, `1` = `Acquiring`, `2` = `Locked`

### MCU (`tactus-zephyr`) → MPU (`tactusd`) *(Optional v2 Input Events)*

`tactusd` registers these methods with `arduino-router` (`$/register`) and applies a `50 ms` per-command debounce:

| Method | Args | Action in `tactusd` |
| :--- | :--- | :--- |
| `"tactus/cmd/toggle_direction"` | `()` | Toggles `LinkToMidi` ↔ `MidiToLink` |
| `"tactus/cmd/set_direction"` | `(0 \| 1 \| "link_to_midi" \| "midi_to_link")` | Sets sync direction |
| `"tactus/cmd/resync_downbeat"` | `()` | Arms quantized downbeat resync |
| `"tactus/cmd/toggle_play"` | `()` | Toggles Link playback state |
| `"tactus/cmd/set_playing"` | `(bool)` | Sets Link playback state |
| `"tactus/cmd/adjust_bpm"` | `(delta)` | Adjusts manual Link BPM (`20.0..999.0`) |
| `"tactus/cmd/set_bpm"` | `(bpm)` | Sets manual Link BPM (`20.0..999.0`) |

### Optional Physical Buttons (Disabled by Default)

`tactus-zephyr.ino` includes a debounced (`30 ms`) active-low button helper guarded by `#ifdef TACTUS_ENABLE_BUTTONS` so floating pins on stock UNO Q boards never trigger false commands:
- `D2` → GND: `Bridge.notify("tactus/cmd/resync_downbeat")`
- `D3` → GND: `Bridge.notify("tactus/cmd/toggle_direction")`

---

## Build & Flash (Arduino UNO Q)

### Option A: Automated Host Compile + SSH OpenOCD *(Recommended & Yocto-Aligned)*

Tactus disables stock `/etc/avahi/services/arduino.service` (`_arduino._tcp`) per `AGENTS.md` Invariant #5 and relies on SSH key auth rather than password auth. The [flash-mcu-unoq.sh](../../system/scripts/flash-mcu-unoq.sh) helper compiles locally on the host with `arduino-cli compile -e`, pushes Tactus's portable OpenOCD configs ([unoq-gpiod.cfg](../../system/openocd/unoq-gpiod.cfg) + [unoq-flash-sketch.cfg](../../system/openocd/unoq-flash-sketch.cfg)) along with the exported `.elf-zsk.bin` (and host Zephyr loader ELF when present) over SSH, and flashes via `openocd` (`linuxgpiod`) — requiring **zero** `arduino-cli` or `.arduino15` installation on the board and mapping 1:1 to future Yocto `meta-tactus` firmware sync:

```bash
# Compile locally and flash over SSH (uses ~/.ssh/config `tactus` host by default)
./system/scripts/flash-mcu-unoq.sh --ssh tactus

# Include optional v2 buttons (D2/D3) and enable [mcu_bridge] in config.toml:
./system/scripts/flash-mcu-unoq.sh --ssh arduino@tactus.local -i ~/.ssh/id_ed25519 \
  --enable-buttons --enable-bridge

# Re-flash an already-exported build (skip recompiling), optionally specifying --loader or --sketch-only:
./system/scripts/flash-mcu-unoq.sh --ssh tactus --skip-compile
```

*(Prerequisites on host: `arduino-cli` with `arduino:zephyr` core and `Arduino_RouterBridge` library installed; on the target board, only `/opt/openocd/bin/openocd` on Debian UNO Q or `openocd` in `PATH` on Yocto is needed.)*

### Option B: Manual `compile -e` + `scp` + `openocd`

1. Export the compiled `.elf-zsk.bin` locally on your host (`mcu/tactus-zephyr/build/` is gitignored):

   ```bash
   arduino-cli compile -e --fqbn arduino:zephyr:unoq mcu/tactus-zephyr
   ```

2. Copy the portable OpenOCD configs and sketch binary and flash the STM32U585 `user_sketch` partition (`0x08100000`) directly via `/opt/openocd` (or `openocd`):

   ```bash
   scp system/openocd/unoq-gpiod.cfg system/openocd/unoq-flash-sketch.cfg \
     mcu/tactus-zephyr/build/arduino.zephyr.unoq/tactus-zephyr.ino.elf-zsk.bin tactus:/tmp/
   ssh tactus '/opt/openocd/bin/openocd -s /opt/openocd/share/openocd/scripts \
     -f /tmp/unoq-gpiod.cfg \
     -c "set filename1 /tmp/tactus-zephyr.ino.elf-zsk.bin" \
     -f /tmp/unoq-flash-sketch.cfg \
     && rm -f /tmp/unoq-gpiod.cfg /tmp/unoq-flash-sketch.cfg /tmp/tactus-zephyr.ino.elf-zsk.bin'
   ```

   *(Note: [unoq-gpiod.cfg](../../system/openocd/unoq-gpiod.cfg) clears `stm32u5x.cpu configure -event reset-start {}` because `linuxgpiod` ignores `adapter speed 480`, and [unoq-flash-sketch.cfg](../../system/openocd/unoq-flash-sketch.cfg) targets UNO Q's `0x08100000` `user_sketch` partition rather than the legacy `/usr/local/bin/arduino-flash` script's `0x80F0000`. Similarly, `arduino-cli upload` on the board invokes `remoteocd` or `SiliconLabs` OpenOCD without `linuxgpiod`.)*

### Option C: Standard Host `arduino-cli upload` / Arduino App Lab (USB-C ADB or `_arduino._tcp`)

- **USB-C ADB:** `arduino-cli upload -p <SERIAL_FROM_BOARD_LIST> --fqbn arduino:zephyr:unoq mcu/tactus-zephyr`
- **Network (`remoteocd_network`):** `arduino-cli upload -l network` requires `mdns-discovery` to see `_arduino._tcp` and prompts for SSH password auth. Temporarily re-enable `/etc/avahi/services/arduino.service` while iterating from IDE / App Lab:

  ```bash
  ssh tactus 'sudo mv /etc/avahi/services/arduino.service.disabled /etc/avahi/services/arduino.service && sudo systemctl reload avahi-daemon'
  arduino-cli upload -l network -p <UNO_Q_IP> --upload-field password=<ARDUINO_PW> --fqbn arduino:zephyr:unoq mcu/tactus-zephyr
  # Optional: re-trim afterward
  ssh tactus 'sudo mv /etc/avahi/services/arduino.service /etc/avahi/services/arduino.service.disabled && sudo systemctl reload avahi-daemon'
  ```

---

## Enabling the Bridge in `tactusd`

1. Edit `/var/lib/tactus/config.toml` on the UNO Q and enable `[mcu_bridge]`:

   ```toml
   [mcu_bridge]
   enabled = true
   socket_path = "/var/run/arduino-router.sock"
   ```

2. Ensure `arduino-router.service` is active and restart `tactusd`:

   ```bash
   sudo systemctl restart tactusd.service
   ```

*(Note: `system/systemd/tactusd.service` already permits socket access via `ReadWritePaths=/var/lib/tactus /run/tactus -/run/arduino-router.sock`, and `system/scripts/provision-debian.sh` automatically adds the `tactus` service user to the `arduino-router` group when present.)*
