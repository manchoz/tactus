#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Thin cross-platform (macOS / Linux) host launcher for the Tactus Arduino UNO Q release bundle.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

TRANSPORT=""
ADB_SERIAL=""
SSH_TARGET=""
SSH_IDENTITY=""
PUBKEY_PATH=""
TARGET_HOSTNAME="tactus"
AP_PSK="${TACTUS_AP_PSK:-}"
WIFI_COUNTRY="${TACTUS_WIFI_COUNTRY:-}"
NOPASSWD_SUDO=0
NO_TRIM=0
STAGE_ONLY=0
ROLLBACK_AP=0
SKIP_MCU=0
SKIP_KERNEL=0
REBOOT_OPT=""
NON_INTERACTIVE=0
ADB_NEED_SUDO_PW=0
ADB_ARDUINO_PW=""

usage() {
  cat <<'EOF'
Usage:
  ./install.sh [OPTIONS]

Guided (or automated with --yes) installer for an Arduino UNO Q from a Tactus release bundle.
Connect your UNO Q via USB-C (ADB) or Ethernet/WiFi (SSH) before running.

Options:
  --adb                   Use USB-C ADB transport
  --serial ID             Optional adb device serial (when multiple boards are attached)
  --ssh USER@HOST         Use IPv4 SSH transport (e.g. arduino@tactus.local)
  -i, --identity FILE     SSH private key path (for --ssh mode)
  --pubkey FILE           SSH public key to install into /home/arduino/.ssh/authorized_keys
  --hostname NAME         Target hostname (default: tactus)
  --psk PASSPHRASE        WPA2 passphrase for 'Tactus' WiFi AP (8..63 chars)
  --country CC            2-letter uppercase ISO WiFi country code (e.g. IT, US, DE)
  --nopasswd-sudo         Configure passwordless sudo for user 'arduino'
  --skip-mcu              Skip flashing the STM32U585 MCU LED matrix firmware
  --skip-kernel           Skip installing the custom 5 GHz WiFi kernel .deb
  --no-trim               Keep GUI/PipeWire/extra Arduino services untouched
  --stage-only            Stage configs without switching wlan0 to AP mode
  --rollback-ap           Restore wlan0 to NetworkManager and stop tactus-ap.service
  --reboot                Reboot automatically when kernel update requires it
  --no-reboot             Do not prompt or reboot after installation
  --yes                   Non-interactive mode using defaults and provided flags
  -h, --help              Show this help message

Environment variables:
  TACTUS_AP_PSK           WPA2 passphrase for Tactus AP
  TACTUS_WIFI_COUNTRY     2-letter ISO WiFi country code
  TACTUS_ARDUINO_PASSWORD Board 'arduino' user/sudo password (skips interactive prompt)
EOF
}

log() {
  printf '[tactus] %s\n' "$*"
}

die() {
  printf '[tactus] ERROR: %s\n' "$*" >&2
  exit 1
}

print_adb_install_hint() {
  cat >&2 <<'EOF'
'adb' (Android Platform Tools) is required for USB-C installation:
  macOS:         brew install android-platform-tools
  Debian/Ubuntu: sudo apt-get install -y adb
  Fedora:        sudo dnf install -y android-tools
  Arch:          sudo pacman -S android-tools
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
    --country)
      WIFI_COUNTRY="${2:?--country requires a 2-letter code}"
      shift 2
      ;;
    --nopasswd-sudo)
      NOPASSWD_SUDO=1
      shift
      ;;
    --skip-mcu)
      SKIP_MCU=1
      shift
      ;;
    --skip-kernel)
      SKIP_KERNEL=1
      shift
      ;;
    --no-trim)
      NO_TRIM=1
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
    --reboot)
      REBOOT_OPT="yes"
      shift
      ;;
    --no-reboot)
      REBOOT_OPT="no"
      shift
      ;;
    --yes)
      NON_INTERACTIVE=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown option: $1 (run ./install.sh --help)"
      ;;
  esac
done

[[ -f "${SCRIPT_DIR}/system/scripts/install-unoq-board.sh" ]] || \
  die "run install.sh from inside the extracted Tactus release bundle directory"

if [[ -z "$PUBKEY_PATH" ]]; then
  if [[ -f "${HOME}/.ssh/id_ed25519.pub" ]]; then
    PUBKEY_PATH="${HOME}/.ssh/id_ed25519.pub"
  elif [[ -f "${HOME}/.ssh/id_rsa.pub" ]]; then
    PUBKEY_PATH="${HOME}/.ssh/id_rsa.pub"
  fi
fi

# Guided interactive setup if transport wasn't explicitly specified.
if [[ -z "$TRANSPORT" ]]; then
  adb_detected=0
  if command -v adb >/dev/null 2>&1; then
    adb_check=(adb)
    [[ -n "$ADB_SERIAL" ]] && adb_check+=(-s "$ADB_SERIAL")
    if [[ "$("${adb_check[@]}" get-state 2>/dev/null || true)" == "device" ]]; then
      adb_detected=1
    fi
  fi

  if [[ "$adb_detected" -eq 1 ]]; then
    log "Detected Arduino UNO Q over USB-C ADB."
    TRANSPORT="adb"
  elif [[ "$NON_INTERACTIVE" -eq 1 ]]; then
    if ! command -v adb >/dev/null 2>&1; then
      print_adb_install_hint
    fi
    die "no UNO Q detected via USB-C ADB; pass --ssh <user@host> or connect via USB-C"
  else
    printf 'No UNO Q detected over USB-C ADB.\n'
    printf 'Choose connection method: [1] USB-C ADB (factory/offline)  [2] SSH (Ethernet/WiFi): '
    read -r choice
    if [[ "$choice" == "2" ]]; then
      TRANSPORT="ssh"
      printf 'SSH target [arduino@tactus.local]: '
      read -r SSH_TARGET
      SSH_TARGET="${SSH_TARGET:-arduino@tactus.local}"
    else
      if ! command -v adb >/dev/null 2>&1; then
        print_adb_install_hint
        exit 1
      fi
      TRANSPORT="adb"
    fi
  fi
fi

if [[ "$NON_INTERACTIVE" -eq 0 && "$ROLLBACK_AP" -eq 0 ]]; then
  if [[ -z "$AP_PSK" ]]; then
    printf "WiFi AP passphrase for 'Tactus' [Enter = keep existing / default 'tactus-link']: "
    read -r -s input_psk
    printf '\n'
    if [[ -n "$input_psk" ]]; then
      AP_PSK="$input_psk"
    fi
  fi
  if [[ -z "$WIFI_COUNTRY" ]]; then
    printf "WiFi 2-letter country code [Enter = keep existing / default 'IT']: "
    read -r input_cc
    if [[ -n "$input_cc" ]]; then
      WIFI_COUNTRY="$(printf '%s' "$input_cc" | tr '[:lower:]' '[:upper:]')"
    fi
  fi
  if [[ "$SKIP_MCU" -eq 0 ]]; then
    printf "Flash STM32U585 MCU LED matrix firmware? [Y/n]: "
    read -r ans_mcu
    if [[ "$ans_mcu" =~ ^[Nn]$ ]]; then
      SKIP_MCU=1
    fi
  fi
  if [[ "$SKIP_KERNEL" -eq 0 ]]; then
    printf "Install custom 5 GHz WiFi kernel (with automatic +2 boot fallback)? [Y/n]: "
    read -r ans_kern
    if [[ "$ans_kern" =~ ^[Nn]$ ]]; then
      SKIP_KERNEL=1
    fi
  fi
fi

if [[ -n "$AP_PSK" ]]; then
  psk_len=${#AP_PSK}
  (( psk_len >= 8 && psk_len <= 63 )) || die "AP passphrase must be 8..63 characters"
fi
if [[ -n "$WIFI_COUNTRY" ]]; then
  WIFI_COUNTRY="$(printf '%s' "$WIFI_COUNTRY" | tr '[:lower:]' '[:upper:]')"
  [[ "$WIFI_COUNTRY" =~ ^[A-Z]{2}$ ]] || die "country code must be 2 uppercase letters (e.g. IT, US, DE)"
fi

PROMPTED_ARDUINO_PASSWORD=""
prompt_new_arduino_password() {
  local pw1="" pw2=""
  read -r -s -p "Set new password for board user 'arduino': " pw1
  printf '\n' >&2
  [[ -n "$pw1" ]] || die "password cannot be empty"
  read -r -s -p "Confirm password for 'arduino': " pw2
  printf '\n' >&2
  [[ "$pw1" == "$pw2" ]] || die "passwords do not match"
  PROMPTED_ARDUINO_PASSWORD="$pw1"
}

build_board_args() {
  if [[ "$ROLLBACK_AP" -eq 1 ]]; then
    printf '%q ' --bundle-dir /tmp/tactus-install --rollback-ap
    return 0
  fi
  local args=(--bundle-dir /tmp/tactus-install --hostname "$TARGET_HOSTNAME")
  [[ -n "$AP_PSK" ]] && args+=(--psk "$AP_PSK")
  [[ -n "$WIFI_COUNTRY" ]] && args+=(--country "$WIFI_COUNTRY")
  [[ "$NOPASSWD_SUDO" -eq 1 ]] && args+=(--nopasswd-sudo)
  [[ "$NO_TRIM" -eq 1 ]] && args+=(--no-trim)
  [[ "$STAGE_ONLY" -eq 1 ]] && args+=(--stage-only)
  [[ "$SKIP_MCU" -eq 1 ]] && args+=(--skip-mcu)
  [[ "$SKIP_KERNEL" -eq 1 ]] && args+=(--skip-kernel)
  if [[ "$TRANSPORT" == "adb" && "$STAGE_ONLY" -eq 0 ]]; then
    args+=(--force-ap)
  fi
  printf '%q ' "${args[@]}"
}

handle_reboot_prompt() {
  local reboot_needed="$1"
  local mode="$2"
  [[ "$reboot_needed" -eq 1 ]] || return 0

  local do_reboot="no"
  if [[ "$REBOOT_OPT" == "yes" ]]; then
    do_reboot="yes"
  elif [[ "$REBOOT_OPT" == "no" || "$NON_INTERACTIVE" -eq 1 ]]; then
    do_reboot="no"
  else
    printf 'Reboot the UNO Q now to activate the 5 GHz kernel? [Y/n]: '
    read -r ans_reboot
    if [[ ! "$ans_reboot" =~ ^[Nn]$ ]]; then
      do_reboot="yes"
    fi
  fi

  if [[ "$do_reboot" == "yes" ]]; then
    log "Rebooting UNO Q..."
    local reboot_rc=0
    if [[ "$mode" == "adb" ]]; then
      if "${ADB_CMD[@]}" shell -T 'sudo -n true 2>/dev/null'; then
        "${ADB_CMD[@]}" shell -T 'sudo -n systemctl reboot' || reboot_rc=$?
      else
        printf '%s\n' "$ADB_ARDUINO_PW" | "${ADB_CMD[@]}" shell -T "sudo -S -p '' systemctl reboot" || reboot_rc=$?
      fi
    else
      if "${SSH_BASE[@]}" "$SSH_TARGET" 'sudo -n true' >/dev/null 2>&1; then
        "${SSH_BASE[@]}" "$SSH_TARGET" 'sudo -n systemctl reboot' || reboot_rc=$?
      else
        "${SSH_BASE[@]}" -tt "$SSH_TARGET" 'sudo systemctl reboot' || reboot_rc=$?
      fi
    fi
    # ssh/adb may return 255/1 when the remote drops the connection during reboot;
    # only treat explicit sudo/auth failure or non-zero when connection stayed up as an error.
    if [[ "$reboot_rc" -eq 0 || "$reboot_rc" -eq 255 ]]; then
      log "Board is rebooting. Connect to WiFi SSID 'Tactus' and open http://${TARGET_HOSTNAME}.local or http://10.42.0.1"
    else
      die "reboot command failed (exit $reboot_rc); run 'sudo reboot' on the board manually"
    fi
  else
    log "Skipping reboot. Run 'sudo reboot' on the board later to activate the 5 GHz kernel."
  fi
}

run_adb() {
  if ! command -v adb >/dev/null 2>&1; then
    print_adb_install_hint
    exit 1
  fi

  ADB_CMD=(adb)
  [[ -n "$ADB_SERIAL" ]] && ADB_CMD+=(-s "$ADB_SERIAL")

  if [[ "$("${ADB_CMD[@]}" get-state 2>/dev/null || true)" != "device" ]]; then
    die "no UNO Q detected via USB-C ADB (connect USB-C and check 'adb devices')"
  fi

  ADB_NEED_SUDO_PW=0
  ADB_ARDUINO_PW="${TACTUS_ARDUINO_PASSWORD:-}"
  if ! "${ADB_CMD[@]}" shell -T 'sudo -n true 2>/dev/null'; then
    ADB_NEED_SUDO_PW=1
    if "${ADB_CMD[@]}" shell -T 'sudo -n -l /usr/local/bin/arduino-passwd >/dev/null 2>&1'; then
      if [[ -z "$ADB_ARDUINO_PW" ]]; then
        log "Factory UNO Q detected (no password set for 'arduino')."
        prompt_new_arduino_password
        ADB_ARDUINO_PW="$PROMPTED_ARDUINO_PASSWORD"
      fi
      log "Setting 'arduino' password via factory /usr/local/bin/arduino-passwd..."
      printf '%s\n' "$ADB_ARDUINO_PW" | "${ADB_CMD[@]}" shell -T 'sudo -n /usr/local/bin/arduino-passwd' \
        || die "factory /usr/local/bin/arduino-passwd failed"
      printf '%s\n' "$ADB_ARDUINO_PW" | "${ADB_CMD[@]}" shell -T 'sudo -S -p "" true'
    else
      if [[ -z "$ADB_ARDUINO_PW" ]]; then
        read -r -s -p "Enter existing sudo password for board user 'arduino': " ADB_ARDUINO_PW
        printf '\n'
        [[ -n "$ADB_ARDUINO_PW" ]] || die "password cannot be empty"
      fi
      printf '%s\n' "$ADB_ARDUINO_PW" | "${ADB_CMD[@]}" shell -T 'sudo -S -p "" true 2>/dev/null' \
        || die "sudo password rejected for user 'arduino'"
    fi
  fi

  adb_cleanup() {
    "${ADB_CMD[@]}" shell -T 'rm -rf /tmp/tactus-install' >/dev/null 2>&1 || true
  }
  trap adb_cleanup EXIT

  if [[ -n "$PUBKEY_PATH" && "$ROLLBACK_AP" -eq 0 ]]; then
    local pubkey_b64
    pubkey_b64="$(base64 < "$PUBKEY_PATH" | tr -d '\r\n')"
    log "Installing SSH public key (${PUBKEY_PATH})..."
    "${ADB_CMD[@]}" shell -T "mkdir -p ~/.ssh && chmod 700 ~/.ssh && touch ~/.ssh/authorized_keys && chmod 600 ~/.ssh/authorized_keys && key=\$(printf '%s' '${pubkey_b64}' | base64 -d) && { grep -Fxq \"\$key\" ~/.ssh/authorized_keys || printf '%s\n' \"\$key\" >> ~/.ssh/authorized_keys; }"
  fi

  log "Uploading release bundle to UNO Q over USB-C ADB..."
  "${ADB_CMD[@]}" shell -T 'rm -rf /tmp/tactus-install'
  "${ADB_CMD[@]}" push "$SCRIPT_DIR" /tmp/tactus-install >/dev/null
  "${ADB_CMD[@]}" shell -T 'chmod +x /tmp/tactus-install/system/scripts/*.sh'

  local board_args host_epoch remote_cmd reboot_needed=0
  board_args="$(build_board_args)"
  host_epoch="$(date -u +%s)"
  remote_cmd="if [ \"\$(timedatectl show -p NTPSynchronized --value 2>/dev/null || echo no)\" != 'yes' ]; then date -u -s '@${host_epoch}' >/dev/null 2>&1 || true; fi; /tmp/tactus-install/system/scripts/install-unoq-board.sh ${board_args}"

  if [[ "$ADB_NEED_SUDO_PW" -eq 0 ]]; then
    "${ADB_CMD[@]}" shell -T "sudo -n bash -c $(printf '%q' "$remote_cmd")"
  else
    printf '%s\n' "$ADB_ARDUINO_PW" | "${ADB_CMD[@]}" shell -T "sudo -S -p '' bash -c $(printf '%q' "$remote_cmd")"
  fi

  if "${ADB_CMD[@]}" shell -T 'grep -qx 1 /tmp/tactus-install/.reboot-required 2>/dev/null'; then
    reboot_needed=1
  fi

  adb_cleanup
  trap - EXIT

  handle_reboot_prompt "$reboot_needed" "adb"
}

run_ssh() {
  command -v ssh >/dev/null 2>&1 || die "'ssh' not found on PATH"

  SSH_BASE=(ssh -4 -o StrictHostKeyChecking=accept-new)
  [[ -n "$SSH_IDENTITY" ]] && SSH_BASE+=(-i "$SSH_IDENTITY" -o IdentitiesOnly=yes)

  if [[ -n "$PUBKEY_PATH" ]] && ! "${SSH_BASE[@]}" -o BatchMode=yes "$SSH_TARGET" true >/dev/null 2>&1; then
    if command -v ssh-copy-id >/dev/null 2>&1; then
      log "Installing SSH public key via ssh-copy-id..."
      ssh-copy-id -4 -f -i "$PUBKEY_PATH" "$SSH_TARGET"
    fi
  fi

  local tar_args=()
  if tar --help 2>&1 | grep -q -- '--no-xattrs'; then
    tar_args+=(--no-xattrs)
  fi

  ssh_cleanup() {
    "${SSH_BASE[@]}" "$SSH_TARGET" 'rm -rf /tmp/tactus-install' >/dev/null 2>&1 || true
  }
  trap ssh_cleanup EXIT

  log "Uploading release bundle to ${SSH_TARGET}..."
  COPYFILE_DISABLE=1 tar "${tar_args[@]}" -C "$SCRIPT_DIR" -cf - . | \
    "${SSH_BASE[@]}" "$SSH_TARGET" 'rm -rf /tmp/tactus-install && mkdir -p /tmp/tactus-install && tar -C /tmp/tactus-install -xf - && chmod +x /tmp/tactus-install/system/scripts/*.sh'

  local board_args host_epoch remote_cmd reboot_needed=0
  board_args="$(build_board_args)"
  host_epoch="$(date -u +%s)"
  remote_cmd="if [ \"\$(timedatectl show -p NTPSynchronized --value 2>/dev/null || echo no)\" != 'yes' ]; then date -u -s '@${host_epoch}' >/dev/null 2>&1 || true; fi; /tmp/tactus-install/system/scripts/install-unoq-board.sh ${board_args}"

  if "${SSH_BASE[@]}" "$SSH_TARGET" 'sudo -n true' >/dev/null 2>&1; then
    "${SSH_BASE[@]}" "$SSH_TARGET" "sudo -n bash -c $(printf '%q' "$remote_cmd")"
  else
    "${SSH_BASE[@]}" -tt "$SSH_TARGET" "sudo bash -c $(printf '%q' "$remote_cmd")"
  fi

  if "${SSH_BASE[@]}" "$SSH_TARGET" 'grep -qx 1 /tmp/tactus-install/.reboot-required 2>/dev/null'; then
    reboot_needed=1
  fi

  ssh_cleanup
  trap - EXIT

  handle_reboot_prompt "$reboot_needed" "ssh"
}

case "$TRANSPORT" in
  adb) run_adb ;;
  ssh) run_ssh ;;
  *) die "invalid transport '$TRANSPORT'" ;;
esac
