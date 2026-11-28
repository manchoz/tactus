#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Assemble the self-contained Tactus Arduino UNO Q release bundle (.tar.gz, .zip, SHA256SUMS).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SYSTEM_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
REPO_ROOT="$(cd "${SYSTEM_DIR}/.." && pwd)"

VERSION=""
TACTUSD_BIN=""
MCU_BIN=""
MCU_LOADER=""
DEB_DIR=""
KERNEL_DEB=""
OUT_DIR="${REPO_ROOT}/target/release-dist"
ALLOW_PARTIAL=0

usage() {
  cat <<'EOF'
Usage: package-release.sh [OPTIONS]

Assemble tactus-<version>-unoq-arm64.{tar.gz,zip} and SHA256SUMS.

Options:
  --version VER       Release version (default: workspace Cargo.toml version)
  --tactusd FILE      Path to aarch64 release tactusd binary (required)
  --mcu-bin FILE      Path to tactus-zephyr.ino.elf-zsk.bin
  --mcu-loader FILE   Path to zephyr-arduino_uno_q_stm32u585xx.elf
  --deb-dir DIR       Directory of offline arm64 .deb packages
  --kernel-deb FILE   Path to custom 5 GHz linux-image-*.deb
  --out-dir DIR       Output directory (default: target/release-dist)
  --allow-partial     Allow packaging without MCU/deb/kernel artifacts (for local testing)
  -h, --help          Show this help message
EOF
}

die() {
  printf '[tactus-package] ERROR: %s\n' "$*" >&2
  exit 1
}

log() {
  printf '[tactus-package] %s\n' "$*"
}

resolve_workspace_version() {
  awk '
    /^\[workspace\.package\]/ { in_ws = 1; next }
    /^\[/ { in_ws = 0 }
    in_ws && /^[[:space:]]*version[[:space:]]*=/ {
      gsub(/.*=[[:space:]]*"|".*/, "", $0)
      print
      exit
    }
  ' "${REPO_ROOT}/Cargo.toml"
}

compute_sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$@"
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$@"
  else
    die "neither sha256sum nor shasum found on PATH"
  fi
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version)
      VERSION="${2:?--version requires a value}"
      shift 2
      ;;
    --tactusd)
      TACTUSD_BIN="${2:?--tactusd requires a file}"
      shift 2
      ;;
    --mcu-bin)
      MCU_BIN="${2:?--mcu-bin requires a file}"
      shift 2
      ;;
    --mcu-loader)
      MCU_LOADER="${2:?--mcu-loader requires a file}"
      shift 2
      ;;
    --deb-dir)
      DEB_DIR="${2:?--deb-dir requires a directory}"
      shift 2
      ;;
    --kernel-deb)
      KERNEL_DEB="${2:?--kernel-deb requires a file}"
      shift 2
      ;;
    --out-dir)
      OUT_DIR="${2:?--out-dir requires a directory}"
      shift 2
      ;;
    --allow-partial)
      ALLOW_PARTIAL=1
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

[[ -n "$VERSION" ]] || VERSION="$(resolve_workspace_version)"
[[ -n "$VERSION" ]] || die "unable to determine version"
VERSION="${VERSION#v}"

[[ -n "$TACTUSD_BIN" && -f "$TACTUSD_BIN" ]] || die "--tactusd binary not found: ${TACTUSD_BIN:-<unset>}"

if [[ "$ALLOW_PARTIAL" -eq 0 ]]; then
  [[ -n "$MCU_BIN" && -f "$MCU_BIN" ]] || die "--mcu-bin required (or pass --allow-partial)"
  [[ -n "$MCU_LOADER" && -f "$MCU_LOADER" ]] || die "--mcu-loader required (or pass --allow-partial)"
  if [[ -z "$DEB_DIR" || ! -d "$DEB_DIR" ]] || ! compgen -G "${DEB_DIR}/*.deb" >/dev/null 2>&1; then
    die "--deb-dir with *.deb files required (or pass --allow-partial)"
  fi
  [[ -n "$KERNEL_DEB" && -f "$KERNEL_DEB" ]] || die "--kernel-deb required (or pass --allow-partial)"
fi

BUNDLE_NAME="tactus-${VERSION}-unoq-arm64"
STAGE_PARENT="$(mktemp -d)"
trap 'rm -rf "$STAGE_PARENT"' EXIT
BUNDLE_DIR="${STAGE_PARENT}/${BUNDLE_NAME}"

mkdir -p \
  "${BUNDLE_DIR}/bin" \
  "${BUNDLE_DIR}/mcu" \
  "${BUNDLE_DIR}/debs/arm64" \
  "${BUNDLE_DIR}/kernel" \
  "${BUNDLE_DIR}/system/scripts"

log "Staging release bundle ${BUNDLE_NAME}..."
printf '%s\n' "$VERSION" > "${BUNDLE_DIR}/VERSION"

cp "${REPO_ROOT}/LICENSE" "${BUNDLE_DIR}/LICENSE"
cp "${REPO_ROOT}/vendor/ableton-link/LICENSE.md" "${BUNDLE_DIR}/LICENSE-GPL-2.0"
cp "${SYSTEM_DIR}/release/install.sh" "${BUNDLE_DIR}/install.sh"
cp "${SYSTEM_DIR}/release/install.ps1" "${BUNDLE_DIR}/install.ps1"
cp "${SYSTEM_DIR}/release/INSTALL.md" "${BUNDLE_DIR}/INSTALL.md"
cp "${SYSTEM_DIR}/release/THIRD_PARTY_NOTICES.md" "${BUNDLE_DIR}/THIRD_PARTY_NOTICES.md"

cp "$TACTUSD_BIN" "${BUNDLE_DIR}/bin/tactusd"
if [[ -n "$MCU_BIN" && -f "$MCU_BIN" ]]; then
  cp "$MCU_BIN" "${BUNDLE_DIR}/mcu/tactus-zephyr.ino.elf-zsk.bin"
fi
if [[ -n "$MCU_LOADER" && -f "$MCU_LOADER" ]]; then
  cp "$MCU_LOADER" "${BUNDLE_DIR}/mcu/zephyr-arduino_uno_q_stm32u585xx.elf"
fi
if [[ -n "$DEB_DIR" && -d "$DEB_DIR" ]] && compgen -G "${DEB_DIR}/*.deb" >/dev/null 2>&1; then
  cp "${DEB_DIR}"/*.deb "${BUNDLE_DIR}/debs/arm64/"
fi
if [[ -n "$KERNEL_DEB" && -f "$KERNEL_DEB" ]]; then
  cp "$KERNEL_DEB" "${BUNDLE_DIR}/kernel/$(basename "$KERNEL_DEB")"
fi

for subdir in NetworkManager avahi config dnsmasq hostapd modules-load openocd systemd tmpfiles; do
  cp -R "${SYSTEM_DIR}/${subdir}" "${BUNDLE_DIR}/system/${subdir}"
done
cp "${SYSTEM_DIR}/scripts/provision-debian.sh" "${BUNDLE_DIR}/system/scripts/provision-debian.sh"
cp "${SYSTEM_DIR}/scripts/install-unoq-board.sh" "${BUNDLE_DIR}/system/scripts/install-unoq-board.sh"
cp "${SYSTEM_DIR}/scripts/flash-mcu-board.sh" "${BUNDLE_DIR}/system/scripts/flash-mcu-board.sh"

chmod 0755 \
  "${BUNDLE_DIR}/install.sh" \
  "${BUNDLE_DIR}/bin/tactusd" \
  "${BUNDLE_DIR}/system/scripts/provision-debian.sh" \
  "${BUNDLE_DIR}/system/scripts/install-unoq-board.sh" \
  "${BUNDLE_DIR}/system/scripts/flash-mcu-board.sh"

log "Generating internal SHA256SUMS..."
(
  cd "$BUNDLE_DIR"
  find . -type f ! -name 'SHA256SUMS' | sed 's|^\./||' | LC_ALL=C sort | while IFS= read -r rel; do
    compute_sha256_file "$rel"
  done > SHA256SUMS
)

mkdir -p "$OUT_DIR"
OUT_ABS="$(cd "$OUT_DIR" && pwd)"
TAR_PATH="${OUT_ABS}/${BUNDLE_NAME}.tar.gz"
ZIP_PATH="${OUT_ABS}/${BUNDLE_NAME}.zip"
rm -f "$TAR_PATH" "$ZIP_PATH" "${OUT_ABS}/SHA256SUMS"

TAR_ARGS=()
if tar --help 2>&1 | grep -q -- '--no-xattrs'; then
  TAR_ARGS+=(--no-xattrs)
fi

log "Creating ${TAR_PATH}..."
COPYFILE_DISABLE=1 tar "${TAR_ARGS[@]}" -C "$STAGE_PARENT" -czf "$TAR_PATH" "$BUNDLE_NAME"

log "Creating ${ZIP_PATH}..."
if command -v zip >/dev/null 2>&1; then
  (cd "$STAGE_PARENT" && zip -r -q "$ZIP_PATH" "$BUNDLE_NAME")
else
  python3 - "$STAGE_PARENT" "$BUNDLE_NAME" "$ZIP_PATH" <<'PYEOF'
import os
import pathlib
import sys
import zipfile

stage_parent = pathlib.Path(sys.argv[1])
bundle_name = sys.argv[2]
zip_path = pathlib.Path(sys.argv[3])
root = stage_parent / bundle_name

with zipfile.ZipFile(zip_path, "w", compression=zipfile.ZIP_DEFLATED) as zf:
    for path in sorted(root.rglob("*")):
        if path.is_file():
            rel = path.relative_to(stage_parent)
            info = zipfile.ZipInfo(str(rel))
            mode = path.stat().st_mode
            info.external_attr = (mode & 0xFFFF) << 16
            zf.writestr(info, path.read_bytes(), compress_type=zipfile.ZIP_DEFLATED)
PYEOF
fi

(
  cd "$OUT_ABS"
  compute_sha256_file "${BUNDLE_NAME}.tar.gz" "${BUNDLE_NAME}.zip" > SHA256SUMS
)

log "Release artifacts ready in ${OUT_ABS}:"
ls -lh "$TAR_PATH" "$ZIP_PATH" "${OUT_ABS}/SHA256SUMS"
