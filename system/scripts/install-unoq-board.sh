#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Unified board-side installer for Tactus Arduino UNO Q release bundles.
# Intended to be executed as root on the target UNO Q Debian board.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SYSTEM_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
BUNDLE_DIR="$(cd "${SYSTEM_DIR}/.." && pwd)"

TARGET_HOSTNAME="tactus"
AP_PSK="${TACTUS_AP_PSK:-}"
WIFI_COUNTRY="${TACTUS_WIFI_COUNTRY:-}"
NOPASSWD_SUDO=0
NO_TRIM=0
STAGE_ONLY=0
ROLLBACK_AP=0
FORCE_AP=0
SKIP_MCU=0
SKIP_KERNEL=0
SKIP_VERIFY=0
REBOOT_REQUIRED=0

usage() {
  cat <<'EOF'
Usage: install-unoq-board.sh [OPTIONS]

Board-side installer executed on the Arduino UNO Q from an extracted Tactus release bundle.

Options:
  --bundle-dir DIR            Bundle root directory (default: auto-detected)
  --hostname NAME             Target hostname (default: tactus)
  --psk PASSPHRASE            WPA2 passphrase for 'Tactus' WiFi AP (8..63 chars)
  --country CC                2-letter uppercase ISO WiFi country code (e.g. IT, US, DE)
  --nopasswd-sudo             Configure passwordless sudo for user 'arduino'
  --no-trim                   Keep GUI/PipeWire/extra Arduino services untouched
  --stage-only                Stage configs without switching wlan0 to AP mode
  --rollback-ap               Restore wlan0 to NetworkManager and stop tactus-ap.service
  --force-ap                  Bypass SSH-over-wlan0 lockout protection
  --skip-mcu                  Skip flashing STM32U585 MCU flashing and [mcu_bridge] enablement
  --skip-kernel               Skip custom 5 GHz kernel .deb installation
  --skip-verify               Skip SHA256SUMS verification before installing
  -h, --help                  Show this help message
EOF
}

log() {
  printf '[tactus-install] %s\n' "$*"
}

die() {
  printf '[tactus-install] ERROR: %s\n' "$*" >&2
  exit 1
}

write_reboot_marker() {
  printf '%s\n' "$REBOOT_REQUIRED" > "${BUNDLE_DIR}/.reboot-required"
  chmod 0644 "${BUNDLE_DIR}/.reboot-required"
}

flash_mcu_from_bundle() {
  local sketch_bin="${BUNDLE_DIR}/mcu/tactus-zephyr.ino.elf-zsk.bin"
  local loader_elf="${BUNDLE_DIR}/mcu/zephyr-arduino_uno_q_stm32u585xx.elf"
  if [[ ! -f "$sketch_bin" ]]; then
    log "MCU sketch binary not present in bundle; skipping MCU flash."
    return 0
  fi

  local flash_args=(
    --sketch "$sketch_bin"
    --openocd-dir "${BUNDLE_DIR}/system/openocd"
    --enable-bridge
  )
  if [[ -f "$loader_elf" ]]; then
    flash_args+=(--loader "$loader_elf")
  else
    log "WARNING: Zephyr loader ELF not bundled; flashing sketch only at 0x08100000."
    flash_args+=(--sketch-only)
  fi

  "${SCRIPT_DIR}/flash-mcu-board.sh" "${flash_args[@]}"
  log "MCU firmware flashed and [mcu_bridge] enabled."
}

configure_systemd_boot_entry() {
  local loader_dir="$1"
  local kver="$2"
  local machine_id_override="${3:-}"
  local loader_conf="${loader_dir}/loader.conf"
  local entries_dir="${loader_dir}/entries"

  if [[ ! -f "$loader_conf" || ! -d "$entries_dir" ]]; then
    log "WARNING: ${loader_conf} not found; skipping systemd-boot +2 rollback configuration."
    return 0
  fi

  if [[ ! -f "${loader_conf}.bak" ]]; then
    cp -a "$loader_conf" "${loader_conf}.bak"
  fi

  local uncounted=() counted=()
  shopt -s nullglob
  uncounted=("${entries_dir}"/*-"${kver}".conf)
  counted=("${entries_dir}"/*-"${kver}"+[0-9]*.conf)
  shopt -u nullglob

  local active_entry=""
  if [[ ${#uncounted[@]} -gt 0 ]]; then
    if [[ ${#counted[@]} -gt 0 ]]; then
      rm -f "${counted[@]}"
    fi
    local entry="${uncounted[0]}"
    local stem="${entry%.conf}"
    active_entry="${stem}+2.conf"
    mv "$entry" "$active_entry"
    log "Enabled systemd-boot +2 boot-counting rollback on $(basename "$active_entry")."
  elif [[ ${#counted[@]} -gt 0 ]]; then
    active_entry="${counted[0]}"
    if [[ ${#counted[@]} -gt 1 ]]; then
      rm -f "${counted[@]:1}"
    fi
  else
    log "WARNING: No systemd-boot entry matching *-${kver}.conf found under ${entries_dir}."
    return 0
  fi

  local entry_base machine_id="$machine_id_override"
  entry_base="$(basename "$active_entry")"
  if [[ -z "$machine_id" && -r /etc/machine-id ]]; then
    machine_id="$(tr -d '[:space:]' < /etc/machine-id)"
  fi
  if [[ -z "$machine_id" ]]; then
    machine_id="${entry_base%%-*}"
  fi

  if grep -q '^default ' "$loader_conf"; then
    sed -i.tmp "s|^default .*|default ${machine_id}-*|" "$loader_conf"
    rm -f "${loader_conf}.tmp"
  else
    printf 'default %s-*\n' "$machine_id" >>"$loader_conf"
  fi
}

install_kernel_from_bundle() {
  local kernel_debs=()
  shopt -s nullglob
  kernel_debs=("${BUNDLE_DIR}"/kernel/linux-image-*.deb)
  shopt -u nullglob

  if [[ ${#kernel_debs[@]} -eq 0 ]]; then
    log "No custom kernel .deb found in bundle; skipping kernel install."
    return 0
  fi
  local kernel_deb="${kernel_debs[0]}"
  local pkg_name pkg_ver kver running_kver installed_status installed_ver
  pkg_name="$(dpkg-deb -f "$kernel_deb" Package)"
  pkg_ver="$(dpkg-deb -f "$kernel_deb" Version)"
  kver="${pkg_name#linux-image-}"
  running_kver="$(uname -r)"
  installed_status="$(dpkg-query -W -f='${Status}' "$pkg_name" 2>/dev/null || true)"
  installed_ver="$(dpkg-query -W -f='${Version}' "$pkg_name" 2>/dev/null || true)"

  if [[ "$running_kver" == "$kver" && "$installed_status" == "install ok installed" && "$installed_ver" == "$pkg_ver" ]]; then
    log "Custom 5 GHz kernel ${kver} (${pkg_ver}) is already running; skipping reinstall."
    return 0
  fi

  local loader_dir="/boot/efi/loader"
  local entries_dir="${loader_dir}/entries"
  local existing_entries=()
  if [[ -d "$entries_dir" ]]; then
    shopt -s nullglob
    existing_entries=("${entries_dir}"/*-"${kver}".conf "${entries_dir}"/*-"${kver}"+[0-9]*.conf)
    shopt -u nullglob
  fi

  if [[ "$installed_status" == "install ok installed" && "$installed_ver" == "$pkg_ver" && ${#existing_entries[@]} -gt 0 ]]; then
    log "Custom 5 GHz kernel ${kver} (${pkg_ver}) is already staged; ensuring boot-counting rollback..."
    configure_systemd_boot_entry "$loader_dir" "$kver"
    REBOOT_REQUIRED=1
    return 0
  fi

  if [[ -d "$entries_dir" ]]; then
    rm -f "${entries_dir}"/*-"${kver}"+[0-9]*.conf
  fi

  log "Installing custom 5 GHz kernel package ${pkg_name} (${pkg_ver}) side-by-side..."
  dpkg -i "$kernel_deb"

  configure_systemd_boot_entry "$loader_dir" "$kver"
  bootctl list --no-pager 2>/dev/null | head -n 30 || true
  REBOOT_REQUIRED=1
}

main() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --bundle-dir)
        BUNDLE_DIR="${2:?--bundle-dir requires a directory}"
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
      --force-ap)
        FORCE_AP=1
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
      --skip-verify)
        SKIP_VERIFY=1
        shift
        ;;
      -h|--help)
        usage
        exit 0
        ;;
      *)
        die "unknown option: $1"
        ;;
    esac
  done

  [[ "$(id -u)" -eq 0 ]] || die "must run as root (invoke via sudo)"

  if [[ "$SKIP_VERIFY" -eq 0 && -f "${BUNDLE_DIR}/SHA256SUMS" ]]; then
    log "Verifying release bundle checksums..."
    (cd "$BUNDLE_DIR" && sha256sum -c SHA256SUMS)
  fi

  chmod +x "${BUNDLE_DIR}/system/scripts/"*.sh

  if [[ "$ROLLBACK_AP" -eq 1 ]]; then
    log "Rolling back AP configuration and restoring wlan0 to NetworkManager..."
    "${BUNDLE_DIR}/system/scripts/provision-debian.sh" --rollback-ap
    REBOOT_REQUIRED=0
    write_reboot_marker
    echo "TACTUS_REBOOT_REQUIRED=0"
    return 0
  fi

  if [[ -r "${BUNDLE_DIR}/VERSION" ]]; then
    TACTUS_VERSION="$(tr -d '[:space:]' < "${BUNDLE_DIR}/VERSION")"
    export TACTUS_VERSION
  fi
  if [[ -n "$AP_PSK" ]]; then
    export TACTUS_AP_PSK="$AP_PSK"
  fi
  if [[ -n "$WIFI_COUNTRY" ]]; then
    export TACTUS_WIFI_COUNTRY="$WIFI_COUNTRY"
  fi

  local prov_args=(--factory --hostname "$TARGET_HOSTNAME")
  if [[ -d "${BUNDLE_DIR}/debs/arm64" ]] && compgen -G "${BUNDLE_DIR}/debs/arm64/*.deb" >/dev/null 2>&1; then
    prov_args+=(--deb-dir "${BUNDLE_DIR}/debs/arm64")
  fi
  if [[ -f "${BUNDLE_DIR}/bin/tactusd" ]]; then
    chmod 0755 "${BUNDLE_DIR}/bin/tactusd"
    prov_args+=(--binary "${BUNDLE_DIR}/bin/tactusd")
  fi
  [[ "$NOPASSWD_SUDO" -eq 1 ]] && prov_args+=(--nopasswd-sudo)
  [[ "$NO_TRIM" -eq 1 ]] && prov_args+=(--no-trim)
  [[ "$STAGE_ONLY" -eq 1 ]] && prov_args+=(--stage-only)
  [[ "$FORCE_AP" -eq 1 ]] && prov_args+=(--force-ap)

  log "Provisioning Debian OS, networking, and tactusd..."
  "${BUNDLE_DIR}/system/scripts/provision-debian.sh" "${prov_args[@]}"

  if [[ "$SKIP_MCU" -eq 0 ]]; then
    flash_mcu_from_bundle
  else
    log "Skipping MCU flash (--skip-mcu)."
  fi

  if [[ "$SKIP_KERNEL" -eq 0 ]]; then
    install_kernel_from_bundle
  else
    log "Skipping custom 5 GHz kernel install (--skip-kernel)."
  fi

  write_reboot_marker

  log "------------------------------------------------------------"
  log "Tactus UNO Q installation complete!"
  log "  Web UI (Ethernet / mDNS): http://${TARGET_HOSTNAME}.local"
  log "  Web UI (Tactus WiFi AP):  http://10.42.0.1  (SSID: Tactus)"
  if [[ "$REBOOT_REQUIRED" -eq 1 ]]; then
    log "  5 GHz kernel staged with +2 boot-counting fallback."
    echo "TACTUS_REBOOT_REQUIRED=1"
  else
    echo "TACTUS_REBOOT_REQUIRED=0"
  fi
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
