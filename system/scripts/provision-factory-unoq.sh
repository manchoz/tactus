#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SYSTEM_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

TRANSPORT=""
ADB_SERIAL=""
SSH_TARGET=""
SSH_IDENTITY=""
PUBKEY_PATH=""
TARGET_HOSTNAME="tactus"
AP_PSK="${TACTUS_AP_PSK:-}"
WIFI_SSID=""
WIFI_PSK="${TACTUS_BOOTSTRAP_WIFI_PSK:-}"
DEB_DIR=""
BINARY_PATH=""
NOPASSWD_SUDO=0
STAGE_ONLY=0
ROLLBACK_AP=0
FORCE_AP=0
NO_TRIM=0

usage() {
  cat <<'EOF'
Usage:
  provision-factory-unoq.sh --adb [OPTIONS]
  provision-factory-unoq.sh --ssh <user@host> [OPTIONS]

Host-side launcher to provision a factory (USB-C ADB) or networked (SSH) Arduino UNO Q for Tactus.

Transport (choose one):
  --adb                   Connect to a factory UNO Q over USB-C using adb
  --serial ID             Optional adb device serial (when multiple boards are attached)
  --ssh USER@HOST         Connect to an existing UNO Q over IPv4 SSH
  -i, --identity FILE     SSH private key path (for --ssh mode)

Options:
  --pubkey FILE           SSH public key to install into /home/arduino/.ssh/authorized_keys
                          (in --adb mode, defaults to ~/.ssh/id_ed25519.pub or ~/.ssh/id_rsa.pub if present)
  --hostname NAME         Target hostname (default: tactus)
  --psk PASSPHRASE        WPA2 passphrase for 'Tactus' WiFi AP (8..63 chars; or TACTUS_AP_PSK env var)
  --nopasswd-sudo         Configure passwordless sudo for user 'arduino'
  --wifi-ssid SSID        (--adb only) Temporary WiFi network to download hostapd/dnsmasq/iw if offline
  --wifi-psk PSK          (--adb only) Passphrase for --wifi-ssid (or TACTUS_BOOTSTRAP_WIFI_PSK env var)
  --deb-dir DIR           Local directory of arm64 .deb files for completely offline install
  --binary FILE           Optional cross-compiled tactusd binary to deploy and enable
  --stage-only            Stage files without switching wlan0 to AP mode
  --rollback-ap           Restore wlan0 to NetworkManager and stop tactus-ap.service
  --force-ap              Bypass SSH-over-wlan0 lockout check
  --no-trim               Keep GUI/PipeWire/extra Arduino services untouched
  -h, --help              Show this help message

Environment variables:
  TACTUS_AP_PSK              WPA2 passphrase for Tactus AP
  TACTUS_BOOTSTRAP_WIFI_PSK  Passphrase for --wifi-ssid
  TACTUS_ARDUINO_PASSWORD    Optional arduino sudo password (skips interactive password prompt)
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --adb)
      TRANSPORT="adb"
      shift
      ;;
    --serial)
      ADB_SERIAL="${2:?--serial requires a device id}"
      shift 2
      ;;
    --ssh)
      TRANSPORT="ssh"
      SSH_TARGET="${2:?--ssh requires user@host}"
      shift 2
      ;;
    -i|--identity)
      SSH_IDENTITY="${2:?--identity requires a path}"
      shift 2
      ;;
    --pubkey)
      PUBKEY_PATH="${2:?--pubkey requires a path}"
      shift 2
      ;;
    --hostname)
      TARGET_HOSTNAME="${2:?--hostname requires a value}"
      shift 2
      ;;
    --psk)
      AP_PSK="${2:?--psk requires a passphrase}"
      shift 2
      ;;
    --wifi-ssid)
      WIFI_SSID="${2:?--wifi-ssid requires an SSID}"
      shift 2
      ;;
    --wifi-psk)
      WIFI_PSK="${2:?--wifi-psk requires a passphrase}"
      shift 2
      ;;
    --deb-dir)
      DEB_DIR="${2:?--deb-dir requires a directory}"
      shift 2
      ;;
    --binary)
      BINARY_PATH="${2:?--binary requires a file}"
      shift 2
      ;;
    --nopasswd-sudo)
      NOPASSWD_SUDO=1
      shift
      ;;
    --stage-only)
      STAGE_ONLY=1
      shift
      ;;
    --rollback-ap)
      ROLLBACK_AP=1
      shift
      ;;
    --force-ap)
      FORCE_AP=1
      shift
      ;;
    --no-trim)
      NO_TRIM=1
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

if [[ -z "$TRANSPORT" ]]; then
  echo "error: specify either --adb or --ssh <user@host>" >&2
  usage >&2
  exit 1
fi

if [[ -n "$AP_PSK" ]] && (( ${#AP_PSK} < 8 || ${#AP_PSK} > 63 )); then
  echo "error: AP passphrase must be 8..63 characters" >&2
  exit 1
fi

if [[ -n "$DEB_DIR" && ! -d "$DEB_DIR" ]]; then
  echo "error: --deb-dir '$DEB_DIR' does not exist" >&2
  exit 1
fi

if [[ -n "$BINARY_PATH" && ! -f "$BINARY_PATH" ]]; then
  echo "error: --binary '$BINARY_PATH' does not exist" >&2
  exit 1
fi

if [[ -n "$PUBKEY_PATH" && ! -f "$PUBKEY_PATH" ]]; then
  echo "error: --pubkey '$PUBKEY_PATH' does not exist" >&2
  exit 1
fi

log() {
  echo "[tactus-factory] $*"
}

build_provision_args() {
  local extra_args=()
  if [[ "$ROLLBACK_AP" -eq 1 ]]; then
    extra_args+=(--rollback-ap)
  else
    extra_args+=(--factory --hostname "$TARGET_HOSTNAME")
    if [[ "$STAGE_ONLY" -eq 1 ]]; then
      extra_args+=(--stage-only)
    fi
    if [[ "$FORCE_AP" -eq 1 ]]; then
      extra_args+=(--force-ap)
    fi
    if [[ "$NO_TRIM" -eq 1 ]]; then
      extra_args+=(--no-trim)
    fi
    if [[ "$NOPASSWD_SUDO" -eq 1 ]]; then
      extra_args+=(--nopasswd-sudo)
    fi
    if [[ -n "$DEB_DIR" ]]; then
      extra_args+=(--deb-dir /tmp/tactus-debs)
    fi
    if [[ -n "$BINARY_PATH" ]]; then
      extra_args+=(--binary /tmp/tactus-bin/tactusd)
    fi
  fi
  printf '%q ' "${extra_args[@]}"
}

run_adb() {
  if ! command -v adb >/dev/null 2>&1; then
    echo "error: 'adb' not found on PATH (install android-platform-tools)" >&2
    exit 1
  fi

  local adb_cmd=(adb)
  if [[ -n "$ADB_SERIAL" ]]; then
    adb_cmd+=(-s "$ADB_SERIAL")
  fi

  if [[ "$("${adb_cmd[@]}" get-state 2>/dev/null || true)" != "device" ]]; then
    echo "error: no UNO Q device connected via adb (connect USB-C and check 'adb devices')" >&2
    exit 1
  fi

  # Auto-select an SSH public key if none was specified explicitly.
  if [[ -z "$PUBKEY_PATH" ]]; then
    if [[ -f "${HOME}/.ssh/id_ed25519.pub" ]]; then
      PUBKEY_PATH="${HOME}/.ssh/id_ed25519.pub"
    elif [[ -f "${HOME}/.ssh/id_rsa.pub" ]]; then
      PUBKEY_PATH="${HOME}/.ssh/id_rsa.pub"
    fi
  fi

  local need_sudo_pw=0
  local arduino_pw="${TACTUS_ARDUINO_PASSWORD:-}"
  if ! "${adb_cmd[@]}" shell -T 'sudo -n true 2>/dev/null'; then
    need_sudo_pw=1
    if [[ -z "$arduino_pw" ]]; then
      read -r -s -p "Enter (or set new) password for board user 'arduino': " arduino_pw
      echo
      if [[ -z "$arduino_pw" ]]; then
        echo "error: non-empty password required when passwordless sudo is not configured" >&2
        exit 1
      fi
    fi

    if ! printf '%s\n' "$arduino_pw" | "${adb_cmd[@]}" shell -T 'sudo -S -p "" true 2>/dev/null'; then
      log "Setting 'arduino' password via factory /usr/local/bin/arduino-passwd..."
      if ! printf '%s\n' "$arduino_pw" | "${adb_cmd[@]}" shell -T 'sudo -n /usr/local/bin/arduino-passwd'; then
        echo "error: sudo password rejected and factory arduino-passwd unavailable" >&2
        exit 1
      fi
      printf '%s\n' "$arduino_pw" | "${adb_cmd[@]}" shell -T 'sudo -S -p "" true'
    fi
  fi

  local cleanup_bootstrap=0
  adb_cleanup() {
    if [[ "$cleanup_bootstrap" -eq 1 ]]; then
      "${adb_cmd[@]}" shell -T 'nmcli connection delete tactus-bootstrap-wifi >/dev/null 2>&1 || true' || true
    fi
    "${adb_cmd[@]}" shell -T 'rm -rf /tmp/tactus-system /tmp/tactus-debs /tmp/tactus-bin /tmp/.tactus-env' >/dev/null 2>&1 || true
  }
  trap adb_cleanup EXIT

  if [[ -n "$PUBKEY_PATH" ]]; then
    local pubkey_b64
    pubkey_b64="$(base64 < "$PUBKEY_PATH" | tr -d '\r\n')"
    log "Installing SSH public key (${PUBKEY_PATH})..."
    "${adb_cmd[@]}" shell -T "mkdir -p ~/.ssh && chmod 700 ~/.ssh && touch ~/.ssh/authorized_keys && chmod 600 ~/.ssh/authorized_keys && key=\$(printf '%s' '${pubkey_b64}' | base64 -d) && { grep -Fxq \"\$key\" ~/.ssh/authorized_keys || printf '%s\n' \"\$key\" >> ~/.ssh/authorized_keys; }"
  fi

  log "Pushing system/ files to UNO Q over ADB..."
  "${adb_cmd[@]}" shell -T 'rm -rf /tmp/tactus-system /tmp/tactus-debs /tmp/tactus-bin'
  "${adb_cmd[@]}" push "${SYSTEM_DIR}" /tmp/tactus-system >/dev/null
  "${adb_cmd[@]}" shell -T 'chmod +x /tmp/tactus-system/scripts/*.sh'

  if [[ -n "$DEB_DIR" ]]; then
    log "Pushing offline .deb packages..."
    "${adb_cmd[@]}" push "${DEB_DIR}" /tmp/tactus-debs >/dev/null
  fi

  if [[ -n "$BINARY_PATH" ]]; then
    log "Pushing tactusd binary..."
    "${adb_cmd[@]}" shell -T 'mkdir -p /tmp/tactus-bin'
    "${adb_cmd[@]}" push "$BINARY_PATH" /tmp/tactus-bin/tactusd >/dev/null
    "${adb_cmd[@]}" shell -T 'chmod 0755 /tmp/tactus-bin/tactusd'
  fi

  if [[ -n "$AP_PSK" ]]; then
    printf 'TACTUS_AP_PSK=%q\n' "$AP_PSK" | "${adb_cmd[@]}" shell -T 'umask 077 && cat > /tmp/.tactus-env'
  fi

  # Over USB-C ADB, check if hostapd/dnsmasq/iw need internet to install.
  if [[ "$ROLLBACK_AP" -eq 0 && "$STAGE_ONLY" -eq 0 && -z "$DEB_DIR" ]]; then
    local missing_pkgs
    missing_pkgs="$("${adb_cmd[@]}" shell -T 'for p in hostapd dnsmasq iw; do dpkg -s "$p" >/dev/null 2>&1 || echo "$p"; done' | tr -d '\r')"
    local has_route
    has_route="$("${adb_cmd[@]}" shell -T 'ip -4 route show default | grep -q "^default" && echo yes || echo no' | tr -d '\r')"

    if [[ -n "$missing_pkgs" && "$has_route" != "yes" ]]; then
      if [[ -n "$WIFI_SSID" ]]; then
        log "Board is offline and needs packages ($missing_pkgs); connecting wlan0 temporarily to '${WIFI_SSID}'..."
        cleanup_bootstrap=1
        "${adb_cmd[@]}" shell -T 'nmcli connection delete tactus-bootstrap-wifi >/dev/null 2>&1 || true'
        "${adb_cmd[@]}" shell -T 'nmcli radio wifi on >/dev/null 2>&1 || true; nmcli dev wifi rescan ifname wlan0 >/dev/null 2>&1 || true'
        local wifi_connect_cmd
        if [[ -n "$WIFI_PSK" ]]; then
          wifi_connect_cmd="nmcli dev wifi connect $(printf '%q' "$WIFI_SSID") password $(printf '%q' "$WIFI_PSK") ifname wlan0 name tactus-bootstrap-wifi"
        else
          wifi_connect_cmd="nmcli dev wifi connect $(printf '%q' "$WIFI_SSID") ifname wlan0 name tactus-bootstrap-wifi"
        fi
        local wifi_connected=0 attempt
        for attempt in 1 2 3; do
          if "${adb_cmd[@]}" shell -T "$wifi_connect_cmd"; then
            wifi_connected=1
            break
          fi
          if [[ "$attempt" -lt 3 ]]; then
            log "WiFi scan/connect attempt ${attempt}/3 did not complete; rescanning..."
            "${adb_cmd[@]}" shell -T 'nmcli dev wifi rescan ifname wlan0 >/dev/null 2>&1 || true'
            sleep 4
          fi
        done
        if [[ "$wifi_connected" -ne 1 ]]; then
          echo "error: failed to connect wlan0 to '${WIFI_SSID}' after 3 attempts" >&2
          exit 1
        fi
        local i
        for i in $(seq 1 15); do
          if [[ "$("${adb_cmd[@]}" shell -T 'ip -4 route show default | grep -q "^default" && echo yes || echo no' | tr -d '\r')" == "yes" ]]; then
            break
          fi
          sleep 1
        done
      else
        log "WARNING: Board has no default IPv4 route and needs ($missing_pkgs). Staging files and enabling SSH without AP cutover."
        log "         Re-run with --wifi-ssid <ssid> --wifi-psk <psk>, --deb-dir <dir>, or plug Ethernet and use --ssh."
        STAGE_ONLY=1
      fi
    fi
  fi

  # Force AP cutover over ADB unless --stage-only was requested, because ADB runs over USB-C.
  if [[ "$STAGE_ONLY" -eq 0 && "$ROLLBACK_AP" -eq 0 ]]; then
    FORCE_AP=1
  fi

  local prov_args host_epoch
  prov_args="$(build_provision_args)"
  host_epoch="$(date -u +%s)"
  local remote_cmd="if [ \"\$(timedatectl show -p NTPSynchronized --value 2>/dev/null || echo no)\" != 'yes' ]; then date -u -s '@${host_epoch}' >/dev/null 2>&1 || true; fi; if [ -f /tmp/.tactus-env ]; then set -a; . /tmp/.tactus-env; set +a; fi; /tmp/tactus-system/scripts/provision-debian.sh ${prov_args}"

  log "Executing on-board provisioning script via ADB..."
  if [[ "$need_sudo_pw" -eq 0 ]]; then
    "${adb_cmd[@]}" shell -T "sudo -n bash -c $(printf '%q' "$remote_cmd")"
  else
    printf '%s\n' "$arduino_pw" | "${adb_cmd[@]}" shell -T "sudo -S -p '' bash -c $(printf '%q' "$remote_cmd")"
  fi

  log "Done. Board reachable over SSH via 'ssh -4 arduino@${TARGET_HOSTNAME}.local' (Ethernet) or 'ssh -4 arduino@10.42.0.1' (Tactus AP)."
}

run_ssh() {
  local ssh_base=(ssh -4 -o StrictHostKeyChecking=accept-new)
  if [[ -n "$SSH_IDENTITY" ]]; then
    ssh_base+=(-i "$SSH_IDENTITY" -o IdentitiesOnly=yes)
  fi

  if [[ -n "$PUBKEY_PATH" ]] && ! "${ssh_base[@]}" -o BatchMode=yes "$SSH_TARGET" true >/dev/null 2>&1; then
    log "Installing SSH public key via ssh-copy-id..."
    local copy_args=(-4 -f -i "$PUBKEY_PATH")
    ssh-copy-id "${copy_args[@]}" "$SSH_TARGET"
  fi

  local tar_args=()
  if tar --help 2>&1 | grep -q -- '--no-xattrs'; then
    tar_args+=(--no-xattrs)
  fi

  log "Streaming system/ files to ${SSH_TARGET}..."
  COPYFILE_DISABLE=1 tar "${tar_args[@]}" -C "$SYSTEM_DIR" -cf - . | \
    "${ssh_base[@]}" "$SSH_TARGET" 'rm -rf /tmp/tactus-system /tmp/tactus-debs /tmp/tactus-bin /tmp/.tactus-env && mkdir -p /tmp/tactus-system && tar -C /tmp/tactus-system -xf - && chmod +x /tmp/tactus-system/scripts/*.sh'

  if [[ -n "$DEB_DIR" ]]; then
    log "Streaming offline .deb packages..."
    COPYFILE_DISABLE=1 tar "${tar_args[@]}" -C "$DEB_DIR" -cf - . | \
      "${ssh_base[@]}" "$SSH_TARGET" 'mkdir -p /tmp/tactus-debs && tar -C /tmp/tactus-debs -xf -'
  fi

  if [[ -n "$BINARY_PATH" ]]; then
    log "Streaming tactusd binary..."
    "${ssh_base[@]}" "$SSH_TARGET" 'mkdir -p /tmp/tactus-bin && cat > /tmp/tactus-bin/tactusd && chmod 0755 /tmp/tactus-bin/tactusd' < "$BINARY_PATH"
  fi

  if [[ -n "$AP_PSK" ]]; then
    printf 'TACTUS_AP_PSK=%q\n' "$AP_PSK" | "${ssh_base[@]}" "$SSH_TARGET" 'umask 077 && cat > /tmp/.tactus-env'
  fi

  local prov_args
  prov_args="$(build_provision_args)"
  local remote_cmd="if [ -f /tmp/.tactus-env ]; then set -a; . /tmp/.tactus-env; set +a; fi; /tmp/tactus-system/scripts/provision-debian.sh ${prov_args}"

  log "Running on-board provisioning script via SSH..."
  local cleanup_cmd='rm -rf /tmp/tactus-system /tmp/tactus-debs /tmp/tactus-bin /tmp/.tactus-env'
  if "${ssh_base[@]}" "$SSH_TARGET" 'sudo -n true' >/dev/null 2>&1; then
    "${ssh_base[@]}" "$SSH_TARGET" "sudo -n bash -c $(printf '%q' "$remote_cmd"); rc=\$?; ${cleanup_cmd}; exit \$rc"
  else
    "${ssh_base[@]}" -tt "$SSH_TARGET" "sudo bash -c $(printf '%q' "$remote_cmd"); rc=\$?; ${cleanup_cmd}; exit \$rc"
  fi

  log "Done. Board provisioned cleanly."
}

case "$TRANSPORT" in
  adb)
    run_adb
    ;;
  ssh)
    run_ssh
    ;;
esac
