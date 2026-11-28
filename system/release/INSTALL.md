# Tactus — Arduino UNO Q Turnkey Installer

This release package contains everything needed to provision a stock Debian **Arduino UNO Q** from scratch—**100% offline**—into a standalone USB-MIDI / BLE-MIDI ↔ Ableton Link bridge:

- `bin/tactusd` — `aarch64` Linux release daemon (RT `SCHED_FIFO` sync engine + embedded mobile Web UI)
- `mcu/` — STM32U585 Zephyr LED matrix + RGB `LED3` firmware & bootloader (`tactus-zephyr.ino.elf-zsk.bin`, `zephyr-arduino_uno_q_stm32u585xx.elf`)
- `debs/arm64/` — Offline Debian Trixie `arm64` packages (`hostapd`, `dnsmasq`, `iw` and runtime dependencies)
- `kernel/` — Custom `7.2.11-arduino1-tactus+` kernel `.deb` enabling 5 GHz WiFi AP mode (channel `36`), installed side-by-side with automatic `systemd-boot` `+2` rollback
- `system/` — Portable `systemd`, `hostapd`, `dnsmasq`, `avahi`, `NetworkManager`, and OpenOCD configurations

---

## Quick Start

### 1. Prerequisites on Your Computer
- **USB-C ADB (recommended for fresh factory boards):**
  - **macOS:** `brew install android-platform-tools`
  - **Debian/Ubuntu:** `sudo apt-get install -y adb`
  - **Windows:** `winget install Google.PlatformTools`
- **Or Ethernet / WiFi SSH (for already-networked boards):** standard `ssh` client.

### 2. Run the Installer
Extract the archive, `cd` into the extracted directory, connect your UNO Q via USB-C (or Ethernet), and run:

- **macOS / Linux:**
  ```bash
  ./install.sh
  ```
- **Windows (PowerShell):**
  ```powershell
  .\install.ps1
  ```

The guided installer will:
1. Auto-detect a USB-C ADB board (or prompt for SSH `user@host`).
2. Prompt you to set an initial password for board user `arduino` (on fresh factory boards) and install your SSH public key if found (`~/.ssh/id_ed25519.pub` or `id_rsa.pub`).
3. Ask for an optional custom WiFi AP passphrase (default `tactus-link`) and 2-letter WiFi country code (default `IT`; existing settings are preserved on upgrades).
4. Verify `SHA256SUMS`, provision Debian & `tactusd`, flash the STM32U585 MCU firmware, and stage the 5 GHz kernel with `+2` boot-counting fallback.
5. Prompt to reboot so the 5 GHz kernel takes effect.

### 3. Connect & Use
1. Join the **`Tactus`** WiFi network (default passphrase: `tactus-link`, or your custom passphrase).
2. Open **`http://tactus.local`** or **`http://10.42.0.1`** on your phone/tablet (or tap **Share → Add to Home Screen** on iOS).
3. Plug in a class-compliant USB-MIDI / BLE-MIDI interface (e.g. CME WIDI Uhost) to the UNO Q USB Host port.

---

## Non-Interactive / Advanced Flags

Run `./install.sh --help` (or `.\install.ps1 -Help`) for all flags. Common examples:

```bash
# Unattended USB-C factory install with custom AP passphrase & US regulatory domain:
TACTUS_ARDUINO_PASSWORD="<board-pw>" ./install.sh --adb --psk "<ap-passphrase>" --country US --nopasswd-sudo --yes --reboot

# Upgrade an existing Ethernet-connected UNO Q over SSH (preserves config.toml and AP settings):
./install.sh --ssh arduino@tactus.local --yes

# Skip custom kernel (stay on stock kernel / 2.4 GHz channel 11 AP fallback):
./install.sh --skip-kernel
```

---

## Hardware Status Indicators

- **Linux `LED1` (MPU status):** Blue = booting / ready; solid/pulsing status = runtime state; fast red blink = systemd unit fault (`journalctl -u tactusd -u tactus-ap`).
- **MCU 8×13 Matrix & `LED3`:** Shows sync direction arrow (`->` Link to MIDI, `<-` MIDI to Link), lock state, transport play/stop, and beat/downbeat pulses.

## Troubleshooting

- **AP stays on 2.4 GHz after install:** Reboot the board so the `7.2.11-arduino1-tactus+` kernel boots (`uname -r`).
- **Kernel boot fallback:** If the custom kernel fails to boot twice, `systemd-boot` automatically reverts to the stock `7.2.11-arduino1+` kernel.
- **Re-enable WiFi client mode (`wlan0`):** Run `./install.sh --rollback-ap` (macOS/Linux) or `.\install.ps1 -RollbackAp` (Windows) from this release folder.
