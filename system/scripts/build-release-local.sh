#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Build all UNO Q offline release artifacts locally on Apple Silicon (or Linux)
# and assemble tactus-<version>-unoq-arm64.{tar.gz,zip} + SHA256SUMS.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SYSTEM_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
REPO_ROOT="$(cd "${SYSTEM_DIR}/.." && pwd)"

VERSION=""
OUT_DIR="${REPO_ROOT}/target/release-dist"
KERNEL_DIR="${REPO_ROOT}/target/unoq-kernel"
DEB_DIR="${REPO_ROOT}/target/unoq-debs"
TACTUSD_BIN="${REPO_ROOT}/target/aarch64-unknown-linux-gnu/release/tactusd"
SKETCH_DIR="${REPO_ROOT}/mcu/tactus-zephyr"
MCU_BIN="${SKETCH_DIR}/build/arduino.zephyr.unoq/tactus-zephyr.ino.elf-zsk.bin"
MCU_LOADER=""
CPUS=8
MEMORY="16G"
KERNEL_REF=""
DEB_IMAGES_REF=""
CLEAN_KERNEL=0
REUSE_KERNEL=0
REUSE_DEBS=0
REUSE_MCU=0
REUSE_TACTUSD=0
PARTIAL=0
DRY_RUN=0

CONTAINER_READY=0
_CLEANUP_DIR=""

cleanup_staging() {
  if [[ -n "${_CLEANUP_DIR:-}" && -d "${_CLEANUP_DIR}" ]]; then
    rm -rf "${_CLEANUP_DIR}"
  fi
}
trap cleanup_staging EXIT

usage() {
  cat <<'EOF'
Usage: build-release-local.sh [OPTIONS]

Build all UNO Q offline release artifacts locally and assemble
target/release-dist/tactus-<version>-unoq-arm64.{tar.gz,zip} + SHA256SUMS.

Options:
  --version VER          Release version (default: workspace Cargo.toml version)
  --out-dir DIR          Release bundle output dir (default: target/release-dist)
  --kernel-dir DIR       Kernel .deb directory (default: target/unoq-kernel)
  --deb-dir DIR          Offline arm64 .deb directory (default: target/unoq-debs)
  --tactusd FILE         Path to release tactusd binary (default: target/aarch64-unknown-linux-gnu/release/tactusd)
  --mcu-bin FILE         Path to tactus-zephyr.ino.elf-zsk.bin
  --mcu-loader FILE      Path to zephyr-arduino_uno_q_stm32u585xx.elf (auto-discovered if omitted)
  --cpus N               VM CPUs for kernel build (default: 8)
  --memory SIZE          VM memory for kernel build, e.g. 16G (default: 16G)
  --kernel-ref REF       Kernel git ref passed to build-unoq-kernel-local.sh
  --deb-images-ref REF   arduino-deb-images ref passed to build-unoq-kernel-local.sh
  --clean-kernel         Pass --clean to build-unoq-kernel-local.sh
  --reuse-kernel         Reuse existing linux-image-*.deb in --kernel-dir if present
  --reuse-debs           Reuse existing *.deb files in --deb-dir if present
  --reuse-mcu            Reuse existing compiled MCU sketch binary if present
  --reuse-tactusd        Reuse existing release tactusd binary if present
  --reuse-cache          Shorthand for --reuse-kernel --reuse-debs
  --partial              Allow packaging without kernel/debs/MCU (passes --allow-partial)
  --dry-run              Print planned actions without executing builds or packaging
  -h, --help             Show this help message
EOF
}

log() {
  printf '[tactus-release-local] %s\n' "$*"
}

die() {
  printf '[tactus-release-local] ERROR: %s\n' "$*" >&2
  exit 1
}

find_host_loader_elf() {
  local candidates=()
  local cli_data_dir=""

  if [[ -n "${ARDUINO_DIRECTORIES_DATA:-}" ]]; then
    candidates+=("${ARDUINO_DIRECTORIES_DATA}")
  fi
  if command -v arduino-cli >/dev/null 2>&1; then
    cli_data_dir="$(arduino-cli config get directories.data 2>/dev/null || true)"
    if [[ -n "$cli_data_dir" ]]; then
      candidates+=("$cli_data_dir")
    fi
  fi
  candidates+=("${HOME}/Library/Arduino15" "${HOME}/.arduino15")

  local found=""
  local dir=""
  for dir in "${candidates[@]}"; do
    local zephyr_dir="${dir}/packages/arduino/hardware/zephyr"
    if [[ -d "$zephyr_dir" ]]; then
      found="$(find "$zephyr_dir" -maxdepth 3 -type f -name 'zephyr-arduino_uno_q_stm32u585xx.elf' 2>/dev/null | sort -V | tail -n 1 || true)"
      if [[ -n "$found" && -f "$found" ]]; then
        printf '%s\n' "$found"
        return 0
      fi
    fi
  done
  return 1
}

find_kernel_deb() {
  local dir="$1"
  [[ -d "$dir" ]] || return 1

  local was_nullglob=0
  shopt -q nullglob && was_nullglob=1
  shopt -s nullglob

  local newest=""
  local candidate=""
  for candidate in "${dir}"/linux-image-*.deb; do
    case "$(basename "$candidate")" in
      *-dbg_*.deb) continue ;;
    esac
    if [[ -z "$newest" || "$candidate" -nt "$newest" ]]; then
      newest="$candidate"
    fi
  done

  if [[ "$was_nullglob" -eq 0 ]]; then
    shopt -u nullglob
  fi

  [[ -n "$newest" ]] || return 1
  printf '%s\n' "$newest"
}

has_cached_debs() {
  local dir="$1"
  [[ -d "$dir" ]] && compgen -G "${dir}/*.deb" >/dev/null 2>&1
}

ensure_container_system() {
  if [[ "$CONTAINER_READY" -eq 1 ]]; then
    return 0
  fi
  command -v container >/dev/null 2>&1 || die "Apple 'container' CLI not found (install with: brew install container)"
  if ! container system status >/dev/null 2>&1; then
    log "Starting Apple container system..."
    container system start
  fi
  CONTAINER_READY=1
}

fetch_offline_debs() {
  local dest_dir="$1"
  ensure_container_system
  mkdir -p "$dest_dir"
  dest_dir="$(cd "$dest_dir" && pwd)"

  local staging="${dest_dir}/.staging-$$"
  rm -rf "$staging"
  mkdir -p "$staging"
  _CLEANUP_DIR="$staging"

  log "Fetching offline Debian Trixie arm64 .deb packages (hostapd, dnsmasq, iw)..."
  container run --rm -i --volume "${staging}:/out" debian:trixie bash -euo pipefail <<'EOF'
if [[ "$(dpkg --print-architecture)" != "arm64" ]]; then
  dpkg --add-architecture arm64
  printf 'APT::Architecture "arm64";\nAPT::Architectures { "arm64"; };\n' > /etc/apt/apt.conf.d/99-arm64.conf
fi

dpkg-query -W -f='${Package}\n' > /tmp/base-pkgs.txt
printf '%s\n' systemd systemd-sysv init-system-helpers adduser netbase iproute2 avahi-daemon >> /tmp/base-pkgs.txt

apt-get update
: > /tmp/empty-status
mapfile -t resolved < <(
  apt-get -s -o Dir::State::status=/tmp/empty-status install --no-install-recommends hostapd dnsmasq iw \
  | awk '$1 == "Inst" { print $2 }' \
  | grep -Fxv -f /tmp/base-pkgs.txt \
  | sort -u
)

echo "Resolved offline arm64/all packages: ${resolved[*]}"
(cd /out && apt-get download "${resolved[@]}")

ls /out/hostapd_*_arm64.deb /out/dnsmasq_*_all.deb /out/dnsmasq-base_*_arm64.deb /out/iw_*_arm64.deb >/dev/null
! compgen -G '/out/*_amd64.deb' >/dev/null
! compgen -G '/out/dnsmasq-base-lua_*.deb' >/dev/null
EOF

  rm -f "${dest_dir}"/*.deb
  mv "${staging}"/*.deb "${dest_dir}/"
  rm -rf "$staging"
  _CLEANUP_DIR=""
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version)
      VERSION="${2:?--version requires a value}"
      shift 2
      ;;
    --out-dir)
      OUT_DIR="${2:?--out-dir requires a directory}"
      shift 2
      ;;
    --kernel-dir)
      KERNEL_DIR="${2:?--kernel-dir requires a directory}"
      shift 2
      ;;
    --deb-dir)
      DEB_DIR="${2:?--deb-dir requires a directory}"
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
    --cpus)
      CPUS="${2:?--cpus requires a value}"
      shift 2
      ;;
    --memory)
      MEMORY="${2:?--memory requires a value}"
      shift 2
      ;;
    --kernel-ref)
      KERNEL_REF="${2:?--kernel-ref requires a value}"
      shift 2
      ;;
    --deb-images-ref)
      DEB_IMAGES_REF="${2:?--deb-images-ref requires a value}"
      shift 2
      ;;
    --clean-kernel)
      CLEAN_KERNEL=1
      shift
      ;;
    --reuse-kernel)
      REUSE_KERNEL=1
      shift
      ;;
    --reuse-debs)
      REUSE_DEBS=1
      shift
      ;;
    --reuse-mcu)
      REUSE_MCU=1
      shift
      ;;
    --reuse-tactusd)
      REUSE_TACTUSD=1
      shift
      ;;
    --reuse-cache)
      REUSE_KERNEL=1
      REUSE_DEBS=1
      shift
      ;;
    --partial)
      PARTIAL=1
      shift
      ;;
    --dry-run)
      DRY_RUN=1
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

# Determine planned action for each artifact
TACTUSD_ACTION="BUILD"
if [[ "$REUSE_TACTUSD" -eq 1 && -f "$TACTUSD_BIN" ]]; then
  TACTUSD_ACTION="REUSE"
fi

MCU_ACTION="BUILD"
if [[ "$REUSE_MCU" -eq 1 && -f "$MCU_BIN" ]]; then
  MCU_ACTION="REUSE"
elif [[ "$PARTIAL" -eq 1 ]]; then
  MCU_ACTION="SKIP"
fi

DEBS_ACTION="BUILD"
if [[ "$REUSE_DEBS" -eq 1 ]] && has_cached_debs "$DEB_DIR"; then
  DEBS_ACTION="REUSE"
elif [[ "$PARTIAL" -eq 1 ]]; then
  DEBS_ACTION="SKIP"
fi

KERNEL_ACTION="BUILD"
KERNEL_DEB=""
if [[ "$REUSE_KERNEL" -eq 1 ]]; then
  KERNEL_DEB="$(find_kernel_deb "$KERNEL_DIR" || true)"
fi
if [[ -n "$KERNEL_DEB" ]]; then
  KERNEL_ACTION="REUSE"
elif [[ "$PARTIAL" -eq 1 ]]; then
  KERNEL_ACTION="SKIP"
fi

if [[ "$MCU_ACTION" != "SKIP" && -z "$MCU_LOADER" ]]; then
  MCU_LOADER="$(find_host_loader_elf || true)"
fi

if [[ "$DRY_RUN" -eq 1 ]]; then
  log "Plan:"
  log "  tactusd : ${TACTUSD_ACTION} (${TACTUSD_BIN})"
  log "  mcu     : ${MCU_ACTION} (${MCU_BIN})"
  log "  debs    : ${DEBS_ACTION} (${DEB_DIR})"
  log "  kernel  : ${KERNEL_ACTION} (${KERNEL_DEB:-${KERNEL_DIR}/linux-image-*.deb})"
  log "  out_dir : ${OUT_DIR}"

  dry_pkg_args=(--tactusd "$TACTUSD_BIN" --out-dir "$OUT_DIR")
  [[ -n "$VERSION" ]] && dry_pkg_args+=(--version "$VERSION")
  if [[ "$MCU_ACTION" != "SKIP" ]]; then
    dry_pkg_args+=(--mcu-bin "$MCU_BIN" --mcu-loader "${MCU_LOADER:-<auto-discovered-loader>}")
  fi
  [[ "$DEBS_ACTION" != "SKIP" ]] && dry_pkg_args+=(--deb-dir "$DEB_DIR")
  [[ "$KERNEL_ACTION" != "SKIP" ]] && dry_pkg_args+=(--kernel-deb "${KERNEL_DEB:-${KERNEL_DIR}/linux-image-*.deb}")
  if [[ "$MCU_ACTION" == "SKIP" || "$DEBS_ACTION" == "SKIP" || "$KERNEL_ACTION" == "SKIP" ]]; then
    dry_pkg_args+=(--allow-partial)
  fi

  log "Would run: ${SCRIPT_DIR}/package-release.sh ${dry_pkg_args[*]}"
  exit 0
fi

# Preflight checks only for tools required by BUILD steps
if [[ "$TACTUSD_ACTION" == "BUILD" ]]; then
  command -v cargo >/dev/null 2>&1 || die "'cargo' not found on PATH"
  cargo zigbuild --help >/dev/null 2>&1 || die "'cargo-zigbuild' not found (install with: cargo install cargo-zigbuild)"
fi
if [[ "$MCU_ACTION" == "BUILD" ]]; then
  command -v arduino-cli >/dev/null 2>&1 || die "'arduino-cli' not found on PATH"
fi
if [[ "$DEBS_ACTION" == "BUILD" || "$KERNEL_ACTION" == "BUILD" ]]; then
  ensure_container_system
fi

# 1. Build or reuse tactusd (aarch64-unknown-linux-gnu.2.36)
if [[ "$TACTUSD_ACTION" == "REUSE" ]]; then
  log "Reusing existing tactusd: ${TACTUSD_BIN}"
else
  log "Building tactusd (aarch64-unknown-linux-gnu.2.36)..."
  (cd "$REPO_ROOT" && cargo zigbuild -p tactusd --release --target aarch64-unknown-linux-gnu.2.36)
  [[ -f "$TACTUSD_BIN" ]] || die "expected tactusd binary not found at ${TACTUSD_BIN}"
fi

# 2. Build or reuse MCU firmware & loader
if [[ "$MCU_ACTION" == "SKIP" ]]; then
  log "Skipping MCU firmware build (--partial)"
elif [[ "$MCU_ACTION" == "REUSE" ]]; then
  log "Reusing existing MCU binary: ${MCU_BIN}"
else
  log "Compiling STM32U585 Zephyr sketch (${SKETCH_DIR})..."
  arduino-cli compile -e --fqbn arduino:zephyr:unoq "$SKETCH_DIR"
  [[ -f "$MCU_BIN" ]] || die "expected MCU binary not found at ${MCU_BIN}"
fi

if [[ "$MCU_ACTION" != "SKIP" ]]; then
  if [[ -z "$MCU_LOADER" ]]; then
    MCU_LOADER="$(find_host_loader_elf || true)"
  fi
  [[ -n "$MCU_LOADER" && -f "$MCU_LOADER" ]] || die "Zephyr loader ELF not found (pass --mcu-loader or install arduino:zephyr via arduino-cli)"
fi

# 3. Fetch or reuse offline arm64 .debs
if [[ "$DEBS_ACTION" == "SKIP" ]]; then
  log "Skipping offline .deb fetch (--partial)"
elif [[ "$DEBS_ACTION" == "REUSE" ]]; then
  log "Reusing cached offline .debs in ${DEB_DIR}"
else
  fetch_offline_debs "$DEB_DIR"
fi

# 4. Build or reuse 5 GHz UNO Q kernel .deb
if [[ "$KERNEL_ACTION" == "SKIP" ]]; then
  log "Skipping 5 GHz kernel build (--partial)"
elif [[ "$KERNEL_ACTION" == "REUSE" ]]; then
  log "Reusing cached kernel .deb: ${KERNEL_DEB}"
else
  kernel_args=(--cpus "$CPUS" --memory "$MEMORY" --out "$KERNEL_DIR")
  [[ -n "$KERNEL_REF" ]] && kernel_args+=(--kernel-ref "$KERNEL_REF")
  [[ -n "$DEB_IMAGES_REF" ]] && kernel_args+=(--deb-images-ref "$DEB_IMAGES_REF")
  [[ "$CLEAN_KERNEL" -eq 1 ]] && kernel_args+=(--clean)

  log "Building 5 GHz UNO Q kernel locally..."
  "${SCRIPT_DIR}/build-unoq-kernel-local.sh" "${kernel_args[@]}"
  KERNEL_DEB="$(find_kernel_deb "$KERNEL_DIR" || true)"
  [[ -n "$KERNEL_DEB" && -f "$KERNEL_DEB" ]] || die "no linux-image-*.deb found in ${KERNEL_DIR} after kernel build"
fi

# 5. Assemble final release bundle
pkg_args=(--tactusd "$TACTUSD_BIN" --out-dir "$OUT_DIR")
[[ -n "$VERSION" ]] && pkg_args+=(--version "$VERSION")
if [[ "$MCU_ACTION" != "SKIP" ]]; then
  pkg_args+=(--mcu-bin "$MCU_BIN" --mcu-loader "$MCU_LOADER")
fi
[[ "$DEBS_ACTION" != "SKIP" ]] && pkg_args+=(--deb-dir "$DEB_DIR")
[[ "$KERNEL_ACTION" != "SKIP" ]] && pkg_args+=(--kernel-deb "$KERNEL_DEB")
if [[ "$MCU_ACTION" == "SKIP" || "$DEBS_ACTION" == "SKIP" || "$KERNEL_ACTION" == "SKIP" ]]; then
  pkg_args+=(--allow-partial)
fi

log "Assembling release bundle..."
"${SCRIPT_DIR}/package-release.sh" "${pkg_args[@]}"
