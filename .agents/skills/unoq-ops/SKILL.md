---
name: unoq-ops
description: >-
  Operational runbook for connecting to, factory-provisioning (USB-C ADB or Ethernet SSH),
  verifying Bonjour/Zeroconf discovery, trimming background services, deploying tactusd,
  and flashing the STM32U585 MCU sketch/bootloader to the Arduino UNO Q hardware.
  Use when interacting with the physical UNO Q board, troubleshooting SSH/ADB/WiFi AP,
  trimming Debian services, cross-compiling/deploying binaries, or flashing mcu/tactus-zephyr.
---

# Arduino UNO Q Operations Runbook

## 1. SSH & Serial Console Access from macOS
- **SSH shortcut (`~/.ssh/config`):**
  - `ssh tactus` (Ethernet LAN `tactus.local`; provisioned boards configure `use-ipv6=no` in `/etc/avahi/avahi-daemon.conf` so `tactus.local` resolves cleanly over IPv4 without `-4`, though keeping `AddressFamily inet` / `-4` remains helpful on unprovisioned boards advertising an unroutable IPv6 ULA).
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
- **Fast binary-only deploy** (when `system/` files haven't changed):
  ```bash
  ./system/scripts/deploy-tactusd.sh --ssh tactus
  ```
  (Builds `aarch64-unknown-linux-gnu.2.36` release, installs `/usr/local/bin/tactusd`, restarts `tactusd.service`, and verifies SHA256 + `rt-sync` Core `3` isolation; pass `--skip-build` or `--binary <path>` to reuse an existing binary.)
- **Full `system/` + binary deploy** (when `systemd`, `hostapd`, `dnsmasq`, or `avahi` files changed):
  ```bash
  cargo zigbuild --workspace --release --target aarch64-unknown-linux-gnu.2.36
  ./system/scripts/provision-factory-unoq.sh --ssh arduino@tactus.local \
    -i ~/.ssh/id_ed25519 \
    --binary target/aarch64-unknown-linux-gnu/release/tactusd
  ```
- **USB-C ADB fallback deploy** (when neither `tactus.local` nor `10.42.0.1` is reachable over SSH):
  - Stage `tactusd` and updated `system/systemd/*.service` files into `/tmp/tactus-deploy/` with `adb push`, install via `adb shell -T` (`sudo install -m 0755` for `/usr/local/bin/tactusd`, `0644` for `/etc/systemd/system/*.service`), run `sudo systemctl daemon-reload`, `sudo systemctl reenable tactus-ap.service` (required whenever `[Install]` device symlinks such as `sys-subsystem-net-devices-wlan0.device.wants` change), `sudo systemd-analyze verify /etc/systemd/system/tactus*.service`, remove `/tmp/tactus-deploy`, and restart `tactus-ap.service tactus-dhcp.service tactusd.service`.
- **Post-deploy live verification (from macOS):**
  ```bash
  shasum -a 256 target/aarch64-unknown-linux-gnu/release/tactusd
  ssh tactus 'sha256sum /usr/local/bin/tactusd && ps -eLo pid,tid,psr,cls,rtprio,comm | grep -E "tactus|rt-sync|asio"; journalctl -u tactusd.service -n 20 --no-pager'
  ssh tactus 'systemctl is-active tactus-ap.service tactus-dhcp.service && sudo pkill -9 dnsmasq && sleep 3 && systemctl is-active tactus-dhcp.service'
  curl -sS http://tactus.local/healthz && curl -sSI http://tactus.local/ && curl -sS http://tactus.local/manifest.webmanifest
  ```
  Confirm matching binary SHA256, `rt-sync` **alone** on Core `3` (`PSR=3`, `FF 75`, `mlockall=locked`) with `tactus-async` and `asio` confined to `PSR=0..2`, `tactus-dhcp.service` auto-restarting after `SIGKILL`, IPv4 resolution without `-4`, `ui_hash` + `no-store` headers on `/` + versioned icon URLs in `/manifest.webmanifest`, and zero `Device or resource busy` (`EBUSY`) lines in `journalctl`.
  - **Per-service fault LED test:** Because `OnFailure=tactus-led-fault@%n.service` expands `%n` to `<unit>.service`, the instantiated unit name has a doubled `.service` suffix (check via `systemctl show tactus-ap.service -p OnFailure`). Test with `sudo systemctl start tactus-led-fault@tactus-ap.service.service` (not `@tactus-ap.service`, which sets `%i=tactus-ap` and creates a mismatched sentinel): verify `/run/tactus/fault.tactus-ap.service` and `unoq:user-red1/trigger` `[timer]` survive `sudo systemctl restart tactusd.service`, then clear cleanly back to `unoq:user-blue1` after `sudo systemctl restart tactus-ap.service`.

## 5. Compile & Flash MCU Sketch (`tactus-zephyr`)
Provisioned boards keep `_arduino._tcp` disabled, so `arduino-cli upload -l network` will not auto-discover `tactus.local`. Compile locally on macOS (`arduino-cli compile -e`) and flash over key-based SSH via the board's `linuxgpiod` OpenOCD path:
```bash
./system/scripts/flash-mcu-unoq.sh --ssh tactus --enable-bridge
```
- Uses portable `system/openocd/{unoq-gpiod.cfg,unoq-flash-sketch.cfg}` with zero board-side `arduino-cli` or `.arduino15` dependency (Yocto-aligned), and pushes the host `zephyr-arduino_uno_q_stm32u585xx.elf` (`filename0`) so Zephyr bootloader updates apply automatically on core upgrades.
- Pass `--skip-compile` (or `--binary <path>.elf-zsk.bin`) to re-flash an exported build, `--loader <ELF>` / `--sketch-only` to override or skip `filename0`, `--enable-buttons` for v2 `D2`/`D3` switches, and `-i ~/.ssh/id_ed25519` if targeting `arduino@tactus.local` directly.
- **USB-C ADB fallback flash:** Compile locally (`arduino-cli compile -e --fqbn arduino:zephyr:unoq mcu/tactus-zephyr`), `adb push` `system/openocd/{unoq-gpiod.cfg,unoq-flash-sketch.cfg}`, `tactus-zephyr.ino.elf-zsk.bin`, and host `~/Library/Arduino15/packages/arduino/hardware/zephyr/*/firmwares/zephyr-arduino_uno_q_stm32u585xx.elf` (`loader.elf`) to `/tmp/tactus-mcu-flash/`, run `/opt/openocd/bin/openocd -s /opt/openocd/share/openocd/scripts -f /tmp/tactus-mcu-flash/unoq-gpiod.cfg -c "set filename0 /tmp/tactus-mcu-flash/loader.elf" -c "set filename1 /tmp/tactus-mcu-flash/tactus-zephyr.ino.elf-zsk.bin" -f /tmp/tactus-mcu-flash/unoq-flash-sketch.cfg` via `adb shell -T`, then remove `/tmp/tactus-mcu-flash` and restart `tactusd.service`.
- **Avoid:** on-board `arduino-cli upload` (fails with `remoteocd` or non-`gpiod` OpenOCD) and legacy `/usr/local/bin/arduino-flash` (writes `0x80F0000` instead of UNO Q's `0x08100000` `user_sketch` partition). See `mcu/tactus-zephyr/README.md` if temporarily re-enabling `_arduino._tcp` for App Lab.

## 5b. Custom 5 GHz Kernel `.deb` (GitHub Actions or local container)
Stock WCN3990 reports `NO-IR` on all 5 GHz channels. `system/kernel/tactus-5ghz.config` (`CONFIG_CFG80211_CERTIFICATION_ONUS=y`, `CONFIG_ATH_REG_DYNAMIC_USER_REG_HINTS=y`, `LOCALVERSION=-arduino1-tactus`) is merged on top of `arduino/arduino-deb-images` `kernel-configs/*.config` by `build-linux-deb.py` against `arduino/linux-qcom` `qcom-v7.2.11-arduino`; the distinct version installs side-by-side with stock `7.2.11-arduino1+` in systemd-boot.
- **GitHub Actions:** `gh workflow run build-unoq-kernel.yml` → artifact `unoq-kernel-5ghz-deb`.
- **Local (Apple `container`, native arm64):** `./system/scripts/build-unoq-kernel-local.sh` (defaults `--cpus 8 --memory 16G`; the Apple VM default of 1 GB is far too small). Output in `target/unoq-kernel/`; the kernel tree lives in the `unoq-kernel-src` volume for incremental rebuilds (`--clean` resets it). Run via an unsandboxed shell (container needs `launchctl`).

## 5c. Test-Booting a Kernel Safely (UNO Q, systemd-boot over U-Boot)
- **No one-shot:** EFI variables are read-only, so `bootctl set-oneshot` fails. Use boot counting instead:
  `sudo bash -ec 'cd /boot/efi/loader; cp -a loader.conf loader.conf.bak; sed -i "s|^default .*|default <machine-id>-*|" loader.conf; mv entries/<id>-<ver>.conf entries/<id>-<ver>+2.conf'` (a `+N` suffix gives N tries; after failures systemd-boot falls back to the next entry; a successful boot is blessed and the suffix removed). Confirm with `sudo bootctl list`.
- **Permissions:** `/boot/efi` is root-only even for `cd`/`cat`; wrap in `sudo bash -c`. Under `set -e` a single denied command aborts the script.
- **PATH:** `iw`, `ip`, `hostapd`, `rfkill` live in `/usr/sbin` and are not on the ssh user's `PATH`; use full paths or `export PATH=/usr/sbin:/sbin:$PATH`.
- **Serial console:** on the Mac adapter the console reads cleanly at `230400` baud (`stty -f /dev/cu.usbserial-* 230400 raw -echo`); U-Boot/sd-boot text is unreadable, and spamming keys can stop sd-boot at its menu. Verify the booted kernel with `uname -r` over SSH instead.
- **Kernel config:** `olddefconfig` silently drops options whose dependencies are off (`CFG80211_CERTIFICATION_ONUS` needs `CONFIG_EXPERT=y`). Always grep the merged `.config` for every required option before packaging (both build paths already do).
- **Timed AP experiments:** stop `tactus-ap.service` only inside a script with a `trap` restore, or use `systemd-run --unit=… <script>` with a `sleep N` then restore, so the AP returns even if ssh dies. Do not start `tactus-dhcp.service` alone (`BindsTo=tactus-ap.service`).

## 6. Service Trimming Guardrails (`provision-debian.sh`)
- **Masked / disabled:** desktop/USB-probing daemons (`fwupd.service`, `udisks2.service`, `upower.service`, `blueman-mechanism.service`), x86-only `thermald.service` (no ARM64 zones; kernel `step_wise` thermal zones handle QRB2210/WCN3990 throttling), `urm.service` (THP disabled directly via `/etc/tmpfiles.d/99-tactus-thp.conf`), unused boot/legacy units (`remoteproc_audio_fixup.service`, `debos-grow-rootfs.service`, `networking.service`, `NetworkManager-wait-online.service`, `wpa_supplicant.service`, `wtmpdb-update-boot.service`, `cron.service`, `key_events.service`, `e2scrub_reap.service`), and background timers (`fwupd-refresh.timer`, `apt-daily*.timer`, `man-db.timer`, `dpkg-db-backup.timer`, `e2scrub_all.timer`).
- **Keep intact:** `arduino-burn-bootloader-unoq.service` (flag-guarded no-op after first boot; preserves manual `arduino-cli burn-bootloader`), `arduino-router.service`, `/opt/openocd`, `NetworkManager.service`, and WiFi modem daemons (`rmtfs.service`, `tqftpserv.service` — started early via drop-ins for WCN3990 firmware/NVRAM).

## 7. UNO Q Hardware Quick Reference
- **OS / Kernel:** Debian 13 (`trixie`), `7.2.11-arduino1+`, `aarch64` (4 cores, QRB2210), glibc `2.41`.
- **MIDI:** `# CONFIG_SND_SEQUENCER is not set` (no `/dev/snd/seq`); `snd-rawmidi` + `snd-usb-audio` available (`/dev/snd/midiC*D*`).
- **WiFi:** Qualcomm WCN3990 (`ath10k_snoc`, EEPROM `0x406c`) enforces 5 GHz `NO-IR` on the stock kernel. The custom `7.2.11-arduino1-tactus+` kernel (see §5b) clears it, so `tactus-ap.service` runs `tactus-ap.conf` (5 GHz ch `36`, `20 MHz` VHT); on the stock kernel it falls back to `tactus-ap-2g.conf` (2.4 GHz ch `11`). The chosen file is copied to `/run/tactus/hostapd.conf` (see `journalctl -u tactus-ap` for `tactus-ap: using …`).
- **Onboard LEDs:**
  - **Linux `LED1`:** `/sys/class/leds/unoq:user-{red,green,blue}1` (`trigger`/`brightness`); early-boot blue blink via `/etc/tmpfiles.d/99-tactus-led1-boot.conf`, runtime status via `tactusd` (`ReadWritePaths=-/sys/class/leds/unoq:user-*1`), fast-blink red alarms via `tactus-led-fault@.service`.
  - **MCU `LED3`:** `LED3_R/G/B` on STM32U585 `TIM5` PWM (`PWM_POLARITY_INVERTED` in UNO Q devicetree, so `analogWrite(LED3_*, 0..255)` is active-high: `0` = off, `255` = full bright — do not manually invert).
