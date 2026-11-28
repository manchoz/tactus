---
name: ap-networking-systemd
description: >-
  WiFi Access Point, NetworkManager coexistence, systemd unit supervision, Avahi mDNS,
  and Linux LED1 status/fault handling for Tactus Debian SBCs.
  Use when editing system/{systemd,hostapd,dnsmasq,avahi,NetworkManager,kernel}/ or status_led.rs.
---

# WiFi AP, Systemd & Linux LED1 Rules

## 1. NetworkManager Coexistence
- Never remove or disable `NetworkManager`.
- Exclude `wlan0` and `p2p-dev-wlan0` via `/etc/NetworkManager/conf.d/99-tactus-unmanaged.conf`.

## 2. WiFi AP (`tactus-ap.service`) & DHCP (`tactus-dhcp.service`)
- Default AP: 5 GHz non-DFS ch `36` (`20 MHz` VHT; requires custom UNO Q kernel from `system/kernel/tactus-5ghz.config` so `NO-IR` is cleared).
- Automatic fallback: `tactus-ap.service` `ExecStartPre` selects `tactus-ap-2g.conf` (2.4 GHz ch `11`, `20 MHz`, clear of BLE advertising) whenever 5 GHz ch `36` reports `no IR`.
- `tactus-ap.service` MUST bind to `sys-subsystem-net-devices-wlan0.device`, order `After=systemd-rfkill.service`, and supervise DHCP via `Upholds=tactus-dhcp.service` — **never** `Wants=` alongside `BindsTo=` (causes a restart storm).
- `tactus-dhcp.service` runs foreground `dnsmasq` with `BindsTo=tactus-ap.service`, omitting DHCP options `3` (gateway) and `6` (DNS).
- `tactusd.service` MUST stay decoupled from `tactus-ap.service` (`Wants=` only, **no `After=`**) so MIDI/Link sync starts immediately at boot.

## 3. Zeroconf (`avahi-daemon`)
- Configure IPv4-only (`use-ipv6=no` in `/etc/avahi/avahi-daemon.conf`), announcing `tactus.local`, `_tactus._tcp`, and `_http._tcp`.
- Disable stock `arduino.service` when trimming boards.

## 4. Linux `LED1` Status (`status_led.rs`)
- Drive `/sys/class/leds/unoq:user-{red,green,blue}1` exclusively on the async `tokio` control plane.
- Yield whenever any per-service `/run/tactus/fault.<unit>.service` sentinel exists (written by `OnFailure=tactus-led-fault@%n.service`) and clear stale `[timer]` triggers after transient faults.
