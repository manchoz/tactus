---
name: unoq-ops
description: >-
  Operational runbook for connecting to, factory-provisioning (USB-C ADB or Ethernet SSH),
  verifying Bonjour/Zeroconf discovery, and deploying tactusd to the Arduino UNO Q hardware.
  Use when interacting with the physical UNO Q board, troubleshooting SSH/ADB/WiFi AP,
  or cross-compiling and deploying binaries.
---

# Arduino UNO Q Operations Runbook

## 1. SSH & Serial Console Access from macOS
- **SSH shortcut (`~/.ssh/config`):**
  - `ssh tactus` (Ethernet LAN `tactus.local`, forces IPv4 `AddressFamily inet` because UNO Q advertises an unroutable IPv6 ULA on mDNS before IPv4).
  - `ssh tactus-ap` (`10.42.0.1` when joined to the `Tactus` WiFi AP).
  - Or explicitly: `ssh -4 -i ~/.ssh/id_ed25519 arduino@tactus.local` / `arduino@10.42.0.1`.
- **Serial Linux console (UART):** `/dev/cu.usbserial-*` at `115200 8N1`. Log in as **`arduino`** (`tactus` is a locked `nologin` daemon user).
- Provisioned boards have `NOPASSWD` sudo for `arduino` (`/etc/sudoers.d/99-arduino-nopasswd`).

## 2. Factory Provisioning (USB-C ADB or SSH)
Use `system/scripts/provision-factory-unoq.sh` (idempotent):
- **Fresh factory flash (USB-C ADB — no App Lab needed):**
  - Factory image boots with **no login password set** for `arduino`, `ssh.service`/`ssh.socket` disabled, `adbd` running as UID `1000` (`arduino`), and passwordless sudo for `/usr/local/bin/arduino-passwd` (reads password once on stdin).
  - Always use `adb shell -T` for non-interactive/piped commands (`sudo -S`, `arduino-passwd`, stdin redirection) to avoid PTY echo and `\r\n` mangling.
  - Factory boards have no RTC battery (`RTC=1970-01-01`, stale image date); the launcher syncs the board's UTC clock from the host before `apt-get update` and `ensure_arduino_password_healthy()` normalizes `/etc/shadow` `lastchg=0` so serial/SSH login never hits an expired-password loop.
  - Run (prompts twice with confirmation to set `arduino`'s password, or reads `TACTUS_ARDUINO_PASSWORD`):
    ```bash
    ./system/scripts/provision-factory-unoq.sh --adb \
      --pubkey ~/.ssh/id_ed25519.pub \
      --wifi-ssid "<BootstrapSSID>" --wifi-psk "<BootstrapPSK>" \
      --nopasswd-sudo
    ```
  - **Security rule:** Never write personal bootstrap WiFi credentials or passwords into repo files, commits, or artifacts. Verify `/etc/NetworkManager/system-connections/` has no `tactus-bootstrap-wifi` profile after provisioning.
- **Re-provisioning / resetting `arduino` password over SSH:**
  ```bash
  ./system/scripts/provision-factory-unoq.sh --ssh arduino@tactus.local -i ~/.ssh/id_ed25519 --nopasswd-sudo
  # To set/reset the 'arduino' serial console password (prompts with confirmation or uses TACTUS_ARDUINO_PASSWORD):
  ./system/scripts/provision-factory-unoq.sh --ssh arduino@tactus.local -i ~/.ssh/id_ed25519 --reset-arduino-password
  ```

## 3. Bonjour / Zeroconf Verification
- Ableton Link does **not** use Bonjour/mDNS (it uses UDP multicast `224.76.78.75:20808` / `ff12::8080:20808`).
- Tactus announces `_tactus._tcp` (with `serial_number` from `/sys/devices/soc0/serial_number` injected at provision time) and `_http._tcp` on port `80`, and disables stock `/etc/avahi/services/arduino.service` (`_arduino._tcp`).
- Verify from macOS (on Ethernet LAN or `Tactus` AP):
  ```bash
  dns-sd -B _tactus._tcp local.
  dns-sd -L tactus _tactus._tcp local.
  dns-sd -B _arduino._tcp local.   # should be empty on trimmed boards
  ```

## 4. Cross-Compile & Deploy `tactusd`
```bash
cargo zigbuild --workspace --release --target aarch64-unknown-linux-gnu.2.36
./system/scripts/provision-factory-unoq.sh --ssh arduino@tactus.local \
  -i ~/.ssh/id_ed25519 \
  --binary target/aarch64-unknown-linux-gnu/release/tactusd
```

## 5. UNO Q Hardware Quick Reference
- **OS / Kernel:** Debian 13 (`trixie`), `7.2.11-arduino1+`, `aarch64` (4 cores, QRB2210), glibc `2.41`.
- **MIDI:** `# CONFIG_SND_SEQUENCER is not set` (no `/dev/snd/seq`); `snd-rawmidi` + `snd-usb-audio` available (`/dev/snd/midiC*D*`).
- **WiFi:** Qualcomm WCN3990 (`ath10k_snoc`, EEPROM `0x406c`) enforces 5 GHz `NO-IR` without `CONFIG_ATH_USER_REGD` → Debian PoC uses 2.4 GHz ch `11` (`2462 MHz`, `20 MHz`).
