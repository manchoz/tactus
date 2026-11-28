# Third-Party Notices & Source Availability

This tac•tus release bundle combines the following components:

1. **tac•tus (`tactusd`, `system/`, installers)**
   - **License:** Mozilla Public License 2.0 (`MPL-2.0`) — see `LICENSE`.
   - **Source:** https://github.com/manchoz/tactus

2. **Ableton Link (`vendor/ableton-link`, statically linked into `tactusd`)**
   - **License:** GNU General Public License v2.0 or later (`GPL-2.0-or-later`) — see `LICENSE-GPL-2.0`.
   - **Source:** https://github.com/Ableton/link (tag `Link-4.1.1`).
   - Under `MPL-2.0 §3.3` (*Larger Work*), the compiled `tactusd` binary is distributed under `GPL-2.0-or-later`.

3. **Arduino UNO Q Linux Kernel (`kernel/linux-image-*.deb`)**
   - **License:** GNU General Public License v2.0 (`GPL-2.0`) — see `LICENSE-GPL-2.0`.
   - **Upstream Kernel Source:** https://github.com/arduino/linux-qcom (`qcom-v7.2.11-arduino`)
   - **Debian Packaging & Configs:** https://github.com/arduino/arduino-deb-images (`arduino`) + `system/kernel/tactus-5ghz.config` in https://github.com/manchoz/tactus

4. **Arduino Zephyr Core & `Arduino_RouterBridge` (`mcu/`)**
   - **License:** Apache License 2.0 (`Apache-2.0`).
   - **Source:** https://github.com/arduino/ArduinoCore-zephyr and https://github.com/arduino-libraries/Arduino_RouterBridge

5. **Offline Debian Trixie `arm64` Packages (`debs/arm64/`)**
   - Unmodified Debian 13 (`trixie`) `arm64` packages (`hostapd`, `dnsmasq`, `iw`, and runtime dependencies) downloaded from `deb.debian.org` under their respective upstream open-source licenses. Source packages are available via `apt-get source` on Debian Trixie (`https://deb.debian.org/debian`).

## Trademarks & Non-Affiliation Disclaimer

**tac•tus** is an independent open-source project and is not affiliated, associated, authorized, endorsed by, or in any way officially connected with **Ableton AG** (<https://www.ableton.com>), **Arduino**, or any of their subsidiaries or affiliates.

**Ableton®** and **Link™** (*Ableton Link*) are trademarks of Ableton AG. All other product names, logos, and brands—including **Arduino®**, **MIDI®**, **Apple®**, **iPad®**, **iPhone®**, **iOS**, **macOS®**, **Safari®**, **Bonjour®**, **Android™**, and **CME WIDI**—are the property of their respective owners and are used solely to identify technical compatibility.
