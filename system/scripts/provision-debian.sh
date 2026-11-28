#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SYSTEM_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

MODE="stage"
EXPLICIT_MODE=0
FORCE_AP=0
TRIM_SERVICES=0
NO_TRIM=0
VOLATILE_JOURNAL=0
ENABLE_SSH=0
NOPASSWD_SUDO=0
RUN_VERIFY=0
TARGET_HOSTNAME=""
DEB_DIR=""
BINARY_PATH=""
ARDUINO_PASSWORD_FILE=""

usage() {
  cat <<'EOF'
Usage: provision-debian.sh [OPTIONS]

Idempotent Debian provisioning script for Tactus (Linux SBC / Arduino UNO Q).

Options:
  --factory           Full factory appliance preset:
                      (--activate-ap --trim-services --volatile-journal --enable-ssh --verify --hostname tactus)
  --stage-only        Install packages and stage configs without switching wlan0 to AP mode (default)
  --activate-ap       Mark wlan0 unmanaged by NetworkManager and enable/start tactus-ap.service
  --rollback-ap       Stop/disable tactus-ap.service and restore wlan0 to NetworkManager
  --trim-services     Switch to multi-user.target, mask PipeWire, and disable non-Tactus services
  --no-trim           Skip service trimming even when --factory is used
  --volatile-journal  Configure systemd-journald for volatile tmpfs storage
  --enable-ssh        Unmask, initialize host keys if missing, and enable/start ssh.service
  --nopasswd-sudo     Configure passwordless sudo for user 'arduino' (validated via visudo)
  --hostname NAME     Set system hostname (e.g. tactus) and restart avahi-daemon
  --deb-dir DIR       Directory of pre-downloaded arm64 .deb packages for offline install
  --binary PATH       Install tactusd binary to /usr/local/bin/tactusd and enable/start tactusd.service
  --arduino-password-file PATH
                      Read new password for user 'arduino' from PATH, apply via chpasswd, and delete PATH
  --verify            Run post-provision verification checks
  --force-ap          Bypass SSH-over-wlan0 lockout protection during --activate-ap
  -h, --help          Show this help message

Environment variables:
  TACTUS_AP_PSK       Optional WPA2 passphrase (8..63 chars) to override default in tactus-ap.conf
  TACTUS_WIFI_COUNTRY Optional 2-letter ISO WiFi country code (e.g. US, DE, JP; default: IT)
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --factory)
      if [[ "$EXPLICIT_MODE" -eq 0 ]]; then
        MODE="activate"
      fi
      TRIM_SERVICES=1
      VOLATILE_JOURNAL=1
      ENABLE_SSH=1
      RUN_VERIFY=1
      if [[ -z "$TARGET_HOSTNAME" ]]; then
        TARGET_HOSTNAME="tactus"
      fi
      shift
      ;;
    --stage-only)
      MODE="stage"
      EXPLICIT_MODE=1
      shift
      ;;
    --activate-ap)
      MODE="activate"
      EXPLICIT_MODE=1
      shift
      ;;
    --rollback-ap)
      MODE="rollback"
      EXPLICIT_MODE=1
      shift
      ;;
    --trim-services)
      TRIM_SERVICES=1
      shift
      ;;
    --no-trim)
      NO_TRIM=1
      shift
      ;;
    --volatile-journal)
      VOLATILE_JOURNAL=1
      shift
      ;;
    --enable-ssh)
      ENABLE_SSH=1
      shift
      ;;
    --nopasswd-sudo)
      NOPASSWD_SUDO=1
      shift
      ;;
    --hostname)
      if [[ $# -lt 2 || -z "$2" ]]; then
        echo "error: --hostname requires a non-empty value" >&2
        exit 1
      fi
      TARGET_HOSTNAME="$2"
      shift 2
      ;;
    --deb-dir)
      if [[ $# -lt 2 || -z "$2" ]]; then
        echo "error: --deb-dir requires a directory path" >&2
        exit 1
      fi
      DEB_DIR="$2"
      shift 2
      ;;
    --binary)
      if [[ $# -lt 2 || -z "$2" ]]; then
        echo "error: --binary requires a file path" >&2
        exit 1
      fi
      BINARY_PATH="$2"
      shift 2
      ;;
    --arduino-password-file)
      if [[ $# -lt 2 || -z "$2" ]]; then
        echo "error: --arduino-password-file requires a file path" >&2
        exit 1
      fi
      ARDUINO_PASSWORD_FILE="$2"
      shift 2
      ;;
    --verify)
      RUN_VERIFY=1
      shift
      ;;
    --force-ap)
      FORCE_AP=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "error: unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

if [[ "$NO_TRIM" -eq 1 ]]; then
  TRIM_SERVICES=0
fi

if [[ $EUID -ne 0 ]]; then
  echo "error: must run as root (use sudo)" >&2
  exit 1
fi

log() {
  echo "[tactus-provision] $*"
}

has_wired_uplink() {
  local iface
  for path in /sys/class/net/*; do
    iface="$(basename "$path")"
    case "$iface" in
      en*|eth*|usb*)
        if [[ "$(cat "${path}/operstate" 2>/dev/null || true)" == "up" ]] &&
           ip -4 -o addr show dev "$iface" 2>/dev/null | grep -q 'inet '; then
          return 0
        fi
        ;;
    esac
  done
  return 1
}

check_ssh_cutover_safety() {
  if [[ "$FORCE_AP" -eq 1 ]]; then
    return 0
  fi

  local ssh_server_ip=""
  if [[ -n "${SSH_CONNECTION:-}" ]]; then
    ssh_server_ip="$(awk '{print $3}' <<<"$SSH_CONNECTION")"
  fi

  local wlan_ips
  wlan_ips="$(ip -4 -o addr show dev wlan0 2>/dev/null | awk '{print $4}' | cut -d/ -f1 || true)"

  if [[ -n "$ssh_server_ip" ]] && grep -Fxq "$ssh_server_ip" <<<"$wlan_ips"; then
    if ! has_wired_uplink; then
      echo "error: active SSH session is on wlan0 (${ssh_server_ip}) and no wired interface is up." >&2
      echo "       Connect Ethernet first or re-run with --force-ap." >&2
      exit 1
    fi
  elif ! has_wired_uplink && [[ -n "$wlan_ips" ]]; then
    echo "error: wlan0 has an IP and no wired Ethernet interface is up." >&2
    echo "       Connect Ethernet first or re-run with --force-ap." >&2
    exit 1
  fi
}

rollback_ap() {
  log "Rolling back AP configuration and restoring wlan0 to NetworkManager..."
  systemctl disable --now tactus-dhcp.service tactus-ap.service 2>/dev/null || true
  rm -f /etc/NetworkManager/conf.d/99-tactus-unmanaged.conf
  systemctl enable --now wpa_supplicant.service 2>/dev/null || true
  if systemctl is-active --quiet NetworkManager.service; then
    nmcli general reload || systemctl reload NetworkManager.service || true
    nmcli device set wlan0 managed yes 2>/dev/null || true
  fi
  log "Rollback complete."
}

missing_packages() {
  local pkgs=(hostapd dnsmasq iw iproute2 avahi-daemon)
  local pkg
  for pkg in "${pkgs[@]}"; do
    if ! dpkg -s "$pkg" >/dev/null 2>&1; then
      echo "$pkg"
    fi
  done
}

install_packages() {
  local missing=()
  while IFS= read -r pkg; do
    [[ -n "$pkg" ]] && missing+=("$pkg")
  done < <(missing_packages)

  if [[ ${#missing[@]} -gt 0 && -n "$DEB_DIR" ]]; then
    if [[ ! -d "$DEB_DIR" ]]; then
      echo "error: --deb-dir '${DEB_DIR}' does not exist" >&2
      exit 1
    fi
    local debs=()
    shopt -s nullglob
    debs=("${DEB_DIR}"/*.deb)
    shopt -u nullglob
    if [[ ${#debs[@]} -gt 0 ]]; then
      log "Installing offline .deb packages from ${DEB_DIR}..."
      dpkg -i -E -G "${debs[@]}" || apt-get install -f -y --no-install-recommends
      missing=()
      while IFS= read -r pkg; do
        [[ -n "$pkg" ]] && missing+=("$pkg")
      done < <(missing_packages)
    fi
  fi

  if [[ ${#missing[@]} -gt 0 ]]; then
    if [[ "$(timedatectl show -p NTPSynchronized --value 2>/dev/null || echo no)" != "yes" ]]; then
      systemctl restart systemd-timesyncd.service 2>/dev/null || true
      for _ in 1 2 3 4 5; do
        if [[ "$(timedatectl show -p NTPSynchronized --value 2>/dev/null || echo no)" == "yes" ]]; then
          break
        fi
        sleep 1
      done
    fi
    log "Installing packages via apt: ${missing[*]}"
    apt-get update
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends "${missing[@]}"
  else
    log "Required packages already installed."
  fi

  # Stock hostapd/dnsmasq services must stay disabled so tactus-ap.service owns wlan0 exclusively.
  systemctl disable --now hostapd.service dnsmasq.service 2>/dev/null || true
  systemctl mask hostapd.service dnsmasq.service 2>/dev/null || true
}

ensure_tactus_user() {
  if ! id -u tactus >/dev/null 2>&1; then
    log "Creating system user 'tactus'..."
    useradd --system --home-dir /var/lib/tactus --create-home --shell /usr/sbin/nologin tactus
  fi
  usermod -aG audio tactus
  if getent group arduino-router >/dev/null 2>&1; then
    usermod -aG arduino-router tactus
  fi
  if getent group gpiod >/dev/null 2>&1; then
    usermod -aG gpiod tactus
  fi
  install -d -o tactus -g tactus -m 0750 /var/lib/tactus
}

resolve_tactus_version() {
  local ver="${TACTUS_VERSION:-}"
  if [[ -z "$ver" && -r "${SYSTEM_DIR}/../Cargo.toml" ]]; then
    ver="$(awk '
      /^\[workspace\.package\]/ { in_ws = 1; next }
      /^\[/ { in_ws = 0 }
      in_ws && /^[[:space:]]*version[[:space:]]*=/ {
        gsub(/.*=[[:space:]]*"|".*/, "", $0)
        print
        exit
      }
    ' "${SYSTEM_DIR}/../Cargo.toml")"
  fi
  if [[ -z "$ver" && -n "$BINARY_PATH" && -x "$BINARY_PATH" ]]; then
    ver="$("$BINARY_PATH" --help 2>&1 | awk 'NR==1 { print $2; exit }' || true)"
  elif [[ -z "$ver" && -x /usr/local/bin/tactusd ]]; then
    ver="$(/usr/local/bin/tactusd --help 2>&1 | awk 'NR==1 { print $2; exit }' || true)"
  fi
  ver="$(printf '%s' "$ver" | tr -cd 'A-Za-z0-9._+-')"
  printf '%s' "${ver:-0.1.0}"
}

configure_avahi_ipv4_only() {
  local avahi_conf="/etc/avahi/avahi-daemon.conf"
  if [[ ! -f "$avahi_conf" ]]; then
    log "Creating ${avahi_conf} with use-ipv6=no..."
    printf '[server]\nuse-ipv6=no\n' >"$avahi_conf"
    chmod 0644 "$avahi_conf"
    systemctl restart avahi-daemon.service
    return 0
  fi

  local tmp_conf
  tmp_conf="$(mktemp)"
  awk '
    BEGIN { in_server = 0; seen_server = 0; wrote_ipv6 = 0 }
    /^\[server\]/ {
      print
      in_server = 1
      seen_server = 1
      next
    }
    /^\[/ {
      if (in_server && !wrote_ipv6) {
        print "use-ipv6=no"
        wrote_ipv6 = 1
      }
      in_server = 0
      print
      next
    }
    in_server && /^[#;[:space:]]*use-ipv6[[:space:]]*=/ {
      if (!wrote_ipv6) {
        print "use-ipv6=no"
        wrote_ipv6 = 1
      }
      next
    }
    { print }
    END {
      if (!seen_server) {
        print ""
        print "[server]"
        print "use-ipv6=no"
      } else if (in_server && !wrote_ipv6) {
        print "use-ipv6=no"
      }
    }
  ' "$avahi_conf" >"$tmp_conf"

  if ! cmp -s "$tmp_conf" "$avahi_conf"; then
    log "Setting use-ipv6=no in ${avahi_conf}..."
    install -m 0644 "$tmp_conf" "$avahi_conf"
    rm -f "$tmp_conf"
    systemctl restart avahi-daemon.service
  else
    rm -f "$tmp_conf"
    systemctl reload-or-restart avahi-daemon.service
  fi
}

install_system_files() {
  log "Installing hostapd, dnsmasq, avahi, config, and systemd unit files..."
  install -d -m 0755 /etc/hostapd /etc/dnsmasq.d /etc/avahi/services /etc/systemd/system

  local ap_psk="${TACTUS_AP_PSK:-}"
  if [[ -z "$ap_psk" && -f /etc/hostapd/tactus-ap.conf ]]; then
    ap_psk="$(sed -n 's/^wpa_passphrase=//p' /etc/hostapd/tactus-ap.conf | head -n 1 || true)"
  fi
  if [[ -n "$ap_psk" ]]; then
    local psk_len=${#ap_psk}
    if (( psk_len < 8 || psk_len > 63 )); then
      echo "error: TACTUS_AP_PSK must be 8..63 characters" >&2
      exit 1
    fi
    if [[ "$ap_psk" =~ [[:cntrl:]] ]]; then
      echo "error: TACTUS_AP_PSK cannot contain control characters" >&2
      exit 1
    fi
  fi

  local ap_country="${TACTUS_WIFI_COUNTRY:-}"
  if [[ -z "$ap_country" && -f /etc/hostapd/tactus-ap.conf ]]; then
    ap_country="$(sed -n 's/^country_code=//p' /etc/hostapd/tactus-ap.conf | head -n 1 || true)"
  fi
  ap_country="${ap_country:-IT}"
  if [[ ! "$ap_country" =~ ^[A-Z]{2}$ ]]; then
    echo "error: TACTUS_WIFI_COUNTRY must be a 2-letter uppercase ISO country code (got '${ap_country}')" >&2
    exit 1
  fi

  local tmp_conf ap_conf
  for ap_conf in tactus-ap.conf tactus-ap-2g.conf; do
    tmp_conf="$(mktemp)"
    cp "${SYSTEM_DIR}/hostapd/${ap_conf}" "$tmp_conf"
    if [[ -n "$ap_psk" ]]; then
      grep -v '^wpa_passphrase=' "$tmp_conf" >"${tmp_conf}.next"
      printf 'wpa_passphrase=%s\n' "$ap_psk" >>"${tmp_conf}.next"
      mv "${tmp_conf}.next" "$tmp_conf"
    fi
    if [[ "$ap_country" != "IT" ]]; then
      sed -i "s/^country_code=.*/country_code=${ap_country}/" "$tmp_conf"
    fi
    install -m 0600 "$tmp_conf" "/etc/hostapd/${ap_conf}"
    rm -f "$tmp_conf"
  done

  install -m 0644 "${SYSTEM_DIR}/dnsmasq/tactus-ap.conf" /etc/dnsmasq.d/tactus-ap.conf

  local tmp_avahi board_serial="" tactus_version=""
  tmp_avahi="$(mktemp)"
  tactus_version="$(resolve_tactus_version)"
  if [[ -r /sys/devices/soc0/serial_number ]]; then
    board_serial="$(tr -cd 'A-Za-z0-9._-' </sys/devices/soc0/serial_number)"
  fi
  awk -v serial="$board_serial" -v ver="$tactus_version" '
    /<type>_tactus\._tcp<\/type>/ { in_tactus = 1 }
    in_tactus && /<txt-record>version=.*<\/txt-record>/ {
      sub(/<txt-record>version=.*<\/txt-record>/, "<txt-record>version=" ver "</txt-record>")
    }
    in_tactus && /<\/service>/ {
      if (serial != "") {
        print "    <txt-record>serial_number=" serial "</txt-record>"
      }
      in_tactus = 0
    }
    { print }
  ' "${SYSTEM_DIR}/avahi/tactus.service" >"$tmp_avahi"
  if command -v python3 >/dev/null 2>&1; then
    python3 -c 'import sys, xml.etree.ElementTree as ET; ET.parse(sys.argv[1])' "$tmp_avahi"
  fi
  install -m 0644 "$tmp_avahi" /etc/avahi/services/tactus.service
  rm -f "$tmp_avahi"

  local tactusd_unit_changed=0
  if [[ ! -f /etc/systemd/system/tactusd.service ]] ||
     ! cmp -s "${SYSTEM_DIR}/systemd/tactusd.service" /etc/systemd/system/tactusd.service; then
    tactusd_unit_changed=1
  fi

  if [[ "$ap_country" != "IT" ]]; then
    local tmp_ap_unit
    tmp_ap_unit="$(mktemp)"
    sed "s|iw reg set IT|iw reg set ${ap_country}|g" "${SYSTEM_DIR}/systemd/tactus-ap.service" >"$tmp_ap_unit"
    install -m 0644 "$tmp_ap_unit" /etc/systemd/system/tactus-ap.service
    rm -f "$tmp_ap_unit"
  else
    install -m 0644 "${SYSTEM_DIR}/systemd/tactus-ap.service" /etc/systemd/system/tactus-ap.service
  fi
  install -m 0644 "${SYSTEM_DIR}/systemd/tactus-dhcp.service" /etc/systemd/system/tactus-dhcp.service
  install -m 0644 "${SYSTEM_DIR}/systemd/tactusd.service" /etc/systemd/system/tactusd.service
  rm -f /etc/systemd/system/tactus-led-fault.service
  install -m 0644 "${SYSTEM_DIR}/systemd/tactus-led-fault@.service" /etc/systemd/system/tactus-led-fault@.service

  install -d -m 0755 /etc/systemd/system.conf.d /etc/tmpfiles.d
  install -m 0644 "${SYSTEM_DIR}/systemd/system-boot-tactus.conf" /etc/systemd/system.conf.d/99-tactus-boot.conf
  install -m 0644 "${SYSTEM_DIR}/tmpfiles/99-tactus-thp.conf" /etc/tmpfiles.d/99-tactus-thp.conf
  install -m 0644 "${SYSTEM_DIR}/tmpfiles/99-tactus-led1-boot.conf" /etc/tmpfiles.d/99-tactus-led1-boot.conf
  systemd-tmpfiles --create /etc/tmpfiles.d/99-tactus-thp.conf 2>/dev/null || true

  if command -v modinfo >/dev/null 2>&1 && modinfo -n ath10k_snoc >/dev/null 2>&1; then
    install -d -m 0755 /etc/modules-load.d
    install -m 0644 "${SYSTEM_DIR}/modules-load/99-tactus-wifi.conf" /etc/modules-load.d/99-tactus-wifi.conf
  fi
  if systemctl list-unit-files rmtfs.service --no-legend 2>/dev/null | grep -q rmtfs.service; then
    install -d -m 0755 /etc/systemd/system/rmtfs.service.d
    install -m 0644 "${SYSTEM_DIR}/systemd/rmtfs-early.conf" /etc/systemd/system/rmtfs.service.d/10-tactus-early.conf
  fi
  if systemctl list-unit-files tqftpserv.service --no-legend 2>/dev/null | grep -q tqftpserv.service; then
    install -d -m 0755 /etc/systemd/system/tqftpserv.service.d
    install -m 0644 "${SYSTEM_DIR}/systemd/tqftpserv-early.conf" /etc/systemd/system/tqftpserv.service.d/10-tactus-early.conf
  fi

  if [[ ! -f /var/lib/tactus/config.toml ]]; then
    log "Seeding default /var/lib/tactus/config.toml..."
    install -o tactus -g tactus -m 0640 "${SYSTEM_DIR}/config/config.toml" /var/lib/tactus/config.toml
  else
    log "Preserving existing /var/lib/tactus/config.toml."
  fi

  dnsmasq --test --conf-file=/etc/dnsmasq.d/tactus-ap.conf
  systemctl daemon-reload
  systemctl enable --now avahi-daemon.service
  configure_avahi_ipv4_only

  if [[ "$tactusd_unit_changed" -eq 1 && -z "$BINARY_PATH" ]] &&
     systemctl is-active --quiet tactusd.service; then
    log "Restarting tactusd.service after systemd unit update..."
    systemctl restart tactusd.service
  fi
}

enable_ssh() {
  log "Ensuring OpenSSH server is unmasked, initialized, and active..."
  systemctl unmask ssh.service ssh.socket sshd.service 2>/dev/null || true
  if [[ ! -f /etc/ssh/ssh_host_ed25519_key ]]; then
    DEBIAN_FRONTEND=noninteractive dpkg-reconfigure openssh-server
    ssh-keygen -A
  fi
  systemctl disable --now ssh.socket 2>/dev/null || true
  systemctl enable --now ssh.service
}

configure_nopasswd_sudo() {
  log "Configuring passwordless sudo for 'arduino'..."
  local tmp_sudoers
  tmp_sudoers="$(mktemp)"
  printf 'arduino ALL=(ALL:ALL) NOPASSWD: ALL\n' >"$tmp_sudoers"
  visudo -cf "$tmp_sudoers"
  install -m 0440 "$tmp_sudoers" /etc/sudoers.d/99-arduino-nopasswd
  rm -f "$tmp_sudoers"
}

ensure_arduino_password_healthy() {
  if [[ -n "$ARDUINO_PASSWORD_FILE" ]]; then
    if [[ ! -f "$ARDUINO_PASSWORD_FILE" ]]; then
      echo "error: --arduino-password-file '${ARDUINO_PASSWORD_FILE}' does not exist" >&2
      exit 1
    fi
    local new_pw=""
    IFS= read -r new_pw < "$ARDUINO_PASSWORD_FILE" || true
    rm -f "$ARDUINO_PASSWORD_FILE"
    if [[ -z "$new_pw" ]]; then
      echo "error: empty password in --arduino-password-file" >&2
      exit 1
    fi
    log "Updating login password for user 'arduino'..."
    printf 'arduino:%s\n' "$new_pw" | chpasswd
    passwd -u arduino >/dev/null 2>&1 || true
  fi

  if ! id -u arduino >/dev/null 2>&1; then
    return 0
  fi

  local shadow_hash shadow_lstchg
  shadow_hash="$(awk -F: '$1=="arduino"{print $2}' /etc/shadow)"
  shadow_lstchg="$(awk -F: '$1=="arduino"{print $3}' /etc/shadow)"

  if [[ -n "$shadow_hash" && "$shadow_hash" != !* && "$shadow_hash" != \** ]]; then
    if [[ "$shadow_lstchg" == "0" ]]; then
      local epoch_days
      epoch_days=$(( $(date -u +%s) / 86400 ))
      if (( epoch_days > 0 )); then
        log "Fixing /etc/shadow lastchg=0 (no-RTC clock trap) for user 'arduino' to day ${epoch_days}..."
        chage -d "$epoch_days" arduino
      fi
    fi
  else
    log "WARNING: user 'arduino' has no active login password in /etc/shadow (serial TTY login disabled)."
  fi
}

configure_volatile_journal() {
  log "Configuring systemd-journald for volatile tmpfs storage..."
  install -d -m 0755 /etc/systemd/journald.conf.d
  install -m 0644 "${SYSTEM_DIR}/systemd/journald-tactus.conf" /etc/systemd/journald.conf.d/99-tactus-volatile.conf
  systemctl restart systemd-journald.service
}

trim_services() {
  log "Switching default target to multi-user.target and trimming non-Tactus services..."
  systemctl set-default multi-user.target
  systemctl isolate multi-user.target 2>/dev/null || true

  local units=(
    lightdm.service
    arduino-app-cli.service
    arduino-cloud-connector.service
    arduino-avahi-serial.service
    docker.socket
    docker.service
    containerd.service
    ModemManager.service
    bluetooth.service
    accounts-daemon.service
    fwupd-refresh.timer
    apt-daily.timer
    apt-daily-upgrade.timer
    man-db.timer
    dpkg-db-backup.timer
    e2scrub_all.timer
    remoteproc_audio_fixup.service
    debos-grow-rootfs.service
    networking.service
    NetworkManager-wait-online.service
    wpa_supplicant.service
    wtmpdb-update-boot.service
    cron.service
    key_events.service
    e2scrub_reap.service
  )
  local unit
  for unit in "${units[@]}"; do
    if systemctl list-unit-files "$unit" --no-legend 2>/dev/null | grep -q "$unit"; then
      systemctl disable --now "$unit" 2>/dev/null || true
    fi
  done

  # Stop and mask D-Bus-activated desktop daemons and x86/unused vendor daemons
  # so USB-MIDI hotplug or D-Bus activation cannot respawn them.
  local masked_units=(
    fwupd.service
    udisks2.service
    upower.service
    blueman-mechanism.service
    thermald.service
    urm.service
  )
  for unit in "${masked_units[@]}"; do
    if systemctl list-unit-files "$unit" --no-legend 2>/dev/null | grep -q "$unit"; then
      systemctl disable --now "$unit" 2>/dev/null || true
      systemctl mask "$unit" 2>/dev/null || true
    fi
  done

  # Disable PipeWire globally (AGENTS.md Invariant #1: no PipeWire).
  systemctl --global mask \
    pipewire.socket pipewire.service \
    pipewire-pulse.socket pipewire-pulse.service \
    wireplumber.service filter-chain.service 2>/dev/null || true

  # Stop any currently running user PipeWire instances immediately.
  if id -u arduino >/dev/null 2>&1; then
    local arduino_uid
    arduino_uid="$(id -u arduino)"
    if [[ -d "/run/user/${arduino_uid}" ]]; then
      systemctl --machine=arduino@.host --user stop \
        pipewire.socket pipewire.service \
        pipewire-pulse.socket pipewire-pulse.service \
        wireplumber.service filter-chain.service 2>/dev/null || true
    fi
  fi
  pkill -x 'pipewire|pipewire-pulse|wireplumber|filter-chain' 2>/dev/null || true

  if [[ -f /etc/avahi/services/arduino.service ]]; then
    mv -f /etc/avahi/services/arduino.service /etc/avahi/services/arduino.service.disabled
    systemctl reload-or-restart avahi-daemon.service 2>/dev/null || true
  fi
}

configure_hostname() {
  local new_host="$1"
  log "Setting hostname to '${new_host}'..."
  hostnamectl set-hostname "$new_host"
  if grep -qE '^127\.0\.1\.1[[:space:]]' /etc/hosts; then
    awk -v host="$new_host" '
      /^127\.0\.1\.1[[:space:]]/ {
        if (!seen) { print "127.0.1.1\t" host; seen = 1 }
        next
      }
      { print }
    ' /etc/hosts >/etc/hosts.tactus.tmp && mv /etc/hosts.tactus.tmp /etc/hosts
  else
    printf '127.0.1.1\t%s\n' "$new_host" >>/etc/hosts
  fi
  systemctl restart avahi-daemon.service
}

activate_ap() {
  check_ssh_cutover_safety

  log "Cleaning temporary bootstrap WiFi profiles and excluding wlan0 from NetworkManager..."
  if systemctl is-active --quiet NetworkManager.service; then
    nmcli connection delete tactus-bootstrap-wifi 2>/dev/null || true
    nmcli device disconnect wlan0 2>/dev/null || true
    nmcli device set wlan0 managed no 2>/dev/null || true
  fi

  install -d -m 0755 /etc/NetworkManager/conf.d
  install -m 0644 "${SYSTEM_DIR}/NetworkManager/99-tactus-unmanaged.conf" /etc/NetworkManager/conf.d/99-tactus-unmanaged.conf

  if systemctl is-active --quiet NetworkManager.service; then
    nmcli general reload || systemctl reload NetworkManager.service || true
    sleep 1
  fi

  if command -v rfkill >/dev/null 2>&1; then
    rfkill unblock wifi || true
  fi

  log "Enabling and starting tactus-ap.service and tactus-dhcp.service..."
  systemctl reenable tactus-ap.service tactus-dhcp.service
  systemctl restart tactus-ap.service
  systemctl restart tactus-dhcp.service

  for _ in 1 2 3 4 5 6 7 8 9 10; do
    if /usr/sbin/iw dev wlan0 info 2>/dev/null | grep -q 'type AP' &&
       ip -4 -br addr show dev wlan0 2>/dev/null | grep -q '10\.42\.0\.1/24'; then
      break
    fi
    sleep 0.5
  done

  if systemctl is-active --quiet avahi-daemon.service; then
    systemctl restart avahi-daemon.service
  fi
}

install_binary() {
  if [[ ! -f "$BINARY_PATH" ]]; then
    echo "error: --binary '${BINARY_PATH}' does not exist" >&2
    exit 1
  fi
  log "Installing tactusd binary to /usr/local/bin/tactusd..."
  install -m 0755 "$BINARY_PATH" /usr/local/bin/tactusd
  systemctl enable --now tactusd.service
  systemctl restart tactusd.service
}

verify_board() {
  local failures=0
  log "Running post-provision verification checks..."

  if [[ -n "$TARGET_HOSTNAME" ]]; then
    local actual_host
    actual_host="$(hostname)"
    if [[ "$actual_host" == "$TARGET_HOSTNAME" ]]; then
      echo "  [PASS] hostname=$actual_host"
    else
      echo "  [FAIL] hostname='$actual_host' (expected '$TARGET_HOSTNAME')" >&2
      failures=$((failures + 1))
    fi
  fi

  local core_services=(avahi-daemon.service NetworkManager.service)
  if systemctl list-unit-files arduino-router.service --no-legend 2>/dev/null | grep -q arduino-router.service; then
    core_services+=(arduino-router.service)
  fi
  if [[ "$ENABLE_SSH" -eq 1 ]]; then
    core_services+=(ssh.service)
  fi
  if [[ "$MODE" == "activate" ]]; then
    core_services+=(tactus-ap.service tactus-dhcp.service)
  fi
  local svc
  for svc in "${core_services[@]}"; do
    if systemctl is-active --quiet "$svc"; then
      echo "  [PASS] $svc active"
    else
      echo "  [FAIL] $svc is not active" >&2
      failures=$((failures + 1))
    fi
  done

  if [[ "$MODE" == "activate" ]]; then
    if nmcli -t -f DEVICE,STATE dev | grep -q '^wlan0:unmanaged$'; then
      echo "  [PASS] wlan0 unmanaged by NetworkManager"
    else
      echo "  [FAIL] wlan0 is not unmanaged by NetworkManager" >&2
      failures=$((failures + 1))
    fi

    if /usr/sbin/iw dev wlan0 info | grep -q 'type AP' && /usr/sbin/iw dev wlan0 info | grep -q 'ssid Tactus'; then
      echo "  [PASS] wlan0 broadcasting SSID 'Tactus' in AP mode"
    else
      echo "  [FAIL] wlan0 is not in AP mode with SSID 'Tactus'" >&2
      failures=$((failures + 1))
    fi

    if ip -4 -br addr show dev wlan0 | grep -q '10\.42\.0\.1/24'; then
      echo "  [PASS] wlan0 has 10.42.0.1/24"
    else
      echo "  [FAIL] wlan0 missing 10.42.0.1/24" >&2
      failures=$((failures + 1))
    fi
  fi

  local avahi_svc="/etc/avahi/services/tactus.service"
  local avahi_xml_ok=1
  if [[ ! -f "$avahi_svc" ]]; then
    avahi_xml_ok=0
  elif command -v python3 >/dev/null 2>&1 &&
       ! python3 -c 'import sys, xml.etree.ElementTree as ET; ET.parse(sys.argv[1])' "$avahi_svc" >/dev/null 2>&1; then
    avahi_xml_ok=0
  elif ! grep -q '<type>_tactus\._tcp</type>' "$avahi_svc" ||
       ! grep -q '<type>_http\._tcp</type>' "$avahi_svc"; then
    avahi_xml_ok=0
  fi

  if [[ "$avahi_xml_ok" -eq 1 && -r /sys/devices/soc0/serial_number ]]; then
    local expected_serial
    expected_serial="$(tr -cd 'A-Za-z0-9._-' </sys/devices/soc0/serial_number)"
    if [[ -n "$expected_serial" ]] && ! grep -Fq "<txt-record>serial_number=${expected_serial}</txt-record>" "$avahi_svc"; then
      avahi_xml_ok=0
    fi
  fi

  if [[ "$avahi_xml_ok" -eq 1 ]]; then
    echo "  [PASS] /etc/avahi/services/tactus.service declares _tactus._tcp + _http._tcp with serial"
  else
    echo "  [FAIL] /etc/avahi/services/tactus.service missing, invalid XML, or missing _tactus._tcp/serial" >&2
    failures=$((failures + 1))
  fi

  if grep -Eq '^[[:space:]]*use-ipv6=no([[:space:]]|$)' /etc/avahi/avahi-daemon.conf 2>/dev/null; then
    echo "  [PASS] /etc/avahi/avahi-daemon.conf sets use-ipv6=no"
  else
    echo "  [FAIL] /etc/avahi/avahi-daemon.conf missing use-ipv6=no" >&2
    failures=$((failures + 1))
  fi

  local avahi_pid avahi_established=0
  avahi_pid="$(systemctl show -p MainPID --value avahi-daemon.service 2>/dev/null || true)"
  if [[ -n "$avahi_pid" && "$avahi_pid" != "0" ]]; then
    for _ in 1 2 3 4 5 6 7 8 9 10; do
      if journalctl -u avahi-daemon.service _PID="$avahi_pid" --no-pager 2>/dev/null |
         grep -Fq '(/services/tactus.service) successfully established'; then
        avahi_established=1
        break
      fi
      sleep 0.5
    done
  fi
  if [[ "$avahi_established" -eq 1 ]]; then
    echo "  [PASS] avahi-daemon established _tactus._tcp (/services/tactus.service)"
  else
    echo "  [FAIL] avahi-daemon did not report /services/tactus.service established" >&2
    failures=$((failures + 1))
  fi

  if id -u arduino >/dev/null 2>&1; then
    local shadow_hash shadow_lstchg
    shadow_hash="$(awk -F: '$1=="arduino"{print $2}' /etc/shadow)"
    shadow_lstchg="$(awk -F: '$1=="arduino"{print $3}' /etc/shadow)"
    if [[ -z "$shadow_hash" || "$shadow_hash" == !* || "$shadow_hash" == \** ]]; then
      echo "  [FAIL] user 'arduino' has no active login password (run with --reset-arduino-password or 'sudo passwd arduino')" >&2
      failures=$((failures + 1))
    elif [[ "$shadow_lstchg" == "0" ]]; then
      echo "  [FAIL] user 'arduino' password is expired (lastchg=0; must change on first login)" >&2
      failures=$((failures + 1))
    else
      echo "  [PASS] user 'arduino' has an active, non-expired login password"
    fi
  fi

  if [[ "$TRIM_SERVICES" -eq 1 ]]; then
    if [[ "$(systemctl get-default)" == "multi-user.target" ]]; then
      echo "  [PASS] default target is multi-user.target"
    else
      echo "  [FAIL] default target is not multi-user.target" >&2
      failures=$((failures + 1))
    fi

    if ! pgrep -x 'pipewire|pipewire-pulse|wireplumber|filter-chain' >/dev/null 2>&1; then
      echo "  [PASS] zero PipeWire processes running"
    else
      echo "  [FAIL] PipeWire processes still running" >&2
      failures=$((failures + 1))
    fi

    if [[ ! -f /etc/avahi/services/arduino.service ]]; then
      echo "  [PASS] stock arduino.service (_arduino._tcp) disabled"
    else
      echo "  [FAIL] stock /etc/avahi/services/arduino.service still active" >&2
      failures=$((failures + 1))
    fi
  fi

  if [[ "$VOLATILE_JOURNAL" -eq 1 ]]; then
    if [[ -f /etc/systemd/journald.conf.d/99-tactus-volatile.conf ]]; then
      echo "  [PASS] volatile journald drop-in installed"
    else
      echo "  [FAIL] volatile journald drop-in missing" >&2
      failures=$((failures + 1))
    fi
  fi

  if [[ -f /var/lib/tactus/config.toml ]]; then
    echo "  [PASS] /var/lib/tactus/config.toml present"
  else
    echo "  [FAIL] /var/lib/tactus/config.toml missing" >&2
    failures=$((failures + 1))
  fi

  if [[ "$failures" -gt 0 ]]; then
    echo "[tactus-provision] VERIFICATION FAILED ($failures error(s))" >&2
    exit 1
  fi
}

if [[ "$MODE" == "rollback" ]]; then
  rollback_ap
  exit 0
fi

install_packages
ensure_tactus_user
install_system_files

if [[ "$ENABLE_SSH" -eq 1 ]]; then
  enable_ssh
fi

if [[ "$NOPASSWD_SUDO" -eq 1 ]]; then
  configure_nopasswd_sudo
fi

ensure_arduino_password_healthy

if [[ "$VOLATILE_JOURNAL" -eq 1 ]]; then
  configure_volatile_journal
fi

if [[ "$TRIM_SERVICES" -eq 1 ]]; then
  trim_services
fi

if [[ -n "$TARGET_HOSTNAME" ]]; then
  configure_hostname "$TARGET_HOSTNAME"
fi

if [[ "$MODE" == "activate" ]]; then
  activate_ap
else
  log "Staged system files (AP activation skipped; run with --activate-ap when ready)."
fi

if [[ -n "$BINARY_PATH" ]]; then
  install_binary
fi

if [[ "$RUN_VERIFY" -eq 1 ]]; then
  verify_board
fi

log "Provisioning finished cleanly."
