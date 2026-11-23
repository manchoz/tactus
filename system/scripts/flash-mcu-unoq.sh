#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Compile (or reuse exported) STM32U585 Zephyr sketch on the host and flash it
# onto an Arduino UNO Q over SSH using portable OpenOCD configs (zero board-side
# arduino-cli or .arduino15 dependency).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SYSTEM_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
REPO_ROOT="$(cd "${SYSTEM_DIR}/.." && pwd)"

OPENOCD_ADAPTER_CFG="${SYSTEM_DIR}/openocd/unoq-gpiod.cfg"
OPENOCD_FLASH_CFG="${SYSTEM_DIR}/openocd/unoq-flash-sketch.cfg"

SSH_TARGET="tactus"
SSH_IDENTITY=""
SKETCH_DIR="${REPO_ROOT}/mcu/tactus-zephyr"
BINARY_PATH=""
LOADER_PATH=""
SKIP_COMPILE=0
SKETCH_ONLY=0
ENABLE_BUTTONS=0
ENABLE_BRIDGE=0

usage() {
  cat <<'EOF'
Usage:
  flash-mcu-unoq.sh [OPTIONS]

Compile an Arduino UNO Q STM32U585 Zephyr sketch locally with `arduino-cli compile -e`,
push portable `system/openocd/*.cfg` configs and firmware artifacts over SSH, and flash
via OpenOCD (`linuxgpiod`) without requiring `arduino-cli`, `.arduino15`, ADB,
`_arduino._tcp`, or SSH passwords on the board.

Options:
  --ssh TARGET            SSH destination (default: tactus, e.g. arduino@tactus.local)
  -i, --identity FILE     SSH private key path
  --sketch DIR            Sketch directory (default: mcu/tactus-zephyr)
  --binary FILE           Prebuilt `.elf-zsk.bin` to flash directly (implies --skip-compile)
  --loader FILE           Zephyr loader ELF (`zephyr-arduino_uno_q_stm32u585xx.elf`)
                          (auto-discovered from host Arduino15 data dir if omitted)
  --sketch-only           Flash only the sketch at 0x08100000 (skip loader verify/flash)
  --skip-compile          Reuse existing `<sketch>/build/arduino.zephyr.unoq/*.elf-zsk.bin`
  --enable-buttons        Compile with `-DTACTUS_ENABLE_BUTTONS` (D2 resync, D3 direction)
  --enable-bridge         Set `[mcu_bridge] enabled = true` in `/var/lib/tactus/config.toml`
                          and restart `tactusd.service` after flashing
  -h, --help              Show this help message
EOF
}

log() {
  printf '[tactus-flash-mcu] %s\n' "$*"
}

die() {
  printf '[tactus-flash-mcu] ERROR: %s\n' "$*" >&2
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

while [[ $# -gt 0 ]]; do
  case "$1" in
    --ssh)
      SSH_TARGET="${2:?--ssh requires an SSH destination}"
      shift 2
      ;;
    -i|--identity)
      SSH_IDENTITY="${2:?--identity requires a file path}"
      shift 2
      ;;
    --sketch)
      SKETCH_DIR="${2:?--sketch requires a directory path}"
      shift 2
      ;;
    --binary)
      BINARY_PATH="${2:?--binary requires a file path}"
      SKIP_COMPILE=1
      shift 2
      ;;
    --loader)
      LOADER_PATH="${2:?--loader requires a file path}"
      shift 2
      ;;
    --sketch-only)
      SKETCH_ONLY=1
      shift
      ;;
    --skip-compile)
      SKIP_COMPILE=1
      shift
      ;;
    --enable-buttons)
      ENABLE_BUTTONS=1
      shift
      ;;
    --enable-bridge)
      ENABLE_BRIDGE=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown argument: $1 (use --help)"
      ;;
  esac
done

SKETCH_DIR="${SKETCH_DIR%/}"
SKETCH_NAME="$(basename "$SKETCH_DIR")"

[[ -f "$OPENOCD_ADAPTER_CFG" ]] || die "OpenOCD adapter config missing: $OPENOCD_ADAPTER_CFG"
[[ -f "$OPENOCD_FLASH_CFG" ]] || die "OpenOCD flash config missing: $OPENOCD_FLASH_CFG"

if [[ -n "$SSH_IDENTITY" && ! -f "$SSH_IDENTITY" ]]; then
  die "SSH identity file not found: $SSH_IDENTITY"
fi

if [[ -n "$LOADER_PATH" && ! -f "$LOADER_PATH" ]]; then
  die "Zephyr loader ELF not found: $LOADER_PATH"
fi

if [[ "$SKIP_COMPILE" -eq 0 ]]; then
  command -v arduino-cli >/dev/null 2>&1 || die "arduino-cli not found in PATH"
  [[ -d "$SKETCH_DIR" ]] || die "sketch directory not found: $SKETCH_DIR"

  compile_args=(compile -e --fqbn arduino:zephyr:unoq)
  if [[ "$ENABLE_BUTTONS" -eq 1 ]]; then
    compile_args+=(--build-property "compiler.cpp.extra_flags=-DTACTUS_ENABLE_BUTTONS")
    log "Compiling $SKETCH_DIR (TACTUS_ENABLE_BUTTONS enabled)..."
  else
    log "Compiling $SKETCH_DIR for arduino:zephyr:unoq..."
  fi
  arduino-cli "${compile_args[@]}" "$SKETCH_DIR"
fi

if [[ -z "$BINARY_PATH" ]]; then
  BINARY_PATH="${SKETCH_DIR}/build/arduino.zephyr.unoq/${SKETCH_NAME}.ino.elf-zsk.bin"
fi

[[ -f "$BINARY_PATH" ]] || die "exported sketch binary not found: $BINARY_PATH"

if [[ "$SKETCH_ONLY" -eq 0 && -z "$LOADER_PATH" ]]; then
  LOADER_PATH="$(find_host_loader_elf || true)"
fi

SSH_OPTS=(-o ConnectTimeout=10)
if [[ -n "$SSH_IDENTITY" ]]; then
  SSH_OPTS+=(-i "$SSH_IDENTITY")
fi

REMOTE_DIR="/tmp/tactus-mcu-flash-$$"
REMOTE_CREATED=0

cleanup() {
  local rc=$?
  if [[ "$REMOTE_CREATED" -eq 1 ]]; then
    ssh "${SSH_OPTS[@]}" "$SSH_TARGET" rm -rf -- "$REMOTE_DIR" >/dev/null 2>&1 || true
  fi
  return "$rc"
}
trap cleanup EXIT

ssh "${SSH_OPTS[@]}" "$SSH_TARGET" mkdir -p -- "$REMOTE_DIR"
REMOTE_CREATED=1

STAGED_FILES=("$OPENOCD_ADAPTER_CFG" "$OPENOCD_FLASH_CFG")
log "Copying OpenOCD configs and $(basename "$BINARY_PATH") to ${SSH_TARGET}:${REMOTE_DIR}/..."
scp "${SSH_OPTS[@]}" "${STAGED_FILES[@]}" "${SSH_TARGET}:${REMOTE_DIR}/"
scp "${SSH_OPTS[@]}" "$BINARY_PATH" "${SSH_TARGET}:${REMOTE_DIR}/sketch.elf-zsk.bin"

HAS_HOST_LOADER=0
if [[ "$SKETCH_ONLY" -eq 0 && -n "$LOADER_PATH" ]]; then
  log "Copying Zephyr loader $(basename "$LOADER_PATH") from host..."
  scp "${SSH_OPTS[@]}" "$LOADER_PATH" "${SSH_TARGET}:${REMOTE_DIR}/loader.elf"
  HAS_HOST_LOADER=1
fi

log "Flashing STM32U585 via linuxgpiod OpenOCD..."
ssh "${SSH_OPTS[@]}" "$SSH_TARGET" bash -s -- "$REMOTE_DIR" "$SKETCH_ONLY" "$HAS_HOST_LOADER" "$ENABLE_BRIDGE" <<'REMOTE_EOF'
set -euo pipefail

REMOTE_DIR="$1"
SKETCH_ONLY="$2"
HAS_HOST_LOADER="$3"
ENABLE_BRIDGE="$4"

OPENOCD_BIN=""
if [[ -x /opt/openocd/bin/openocd ]]; then
  OPENOCD_BIN="/opt/openocd/bin/openocd"
elif command -v openocd >/dev/null 2>&1; then
  OPENOCD_BIN="$(command -v openocd)"
else
  echo "ERROR: neither /opt/openocd/bin/openocd nor openocd in PATH found on board" >&2
  exit 1
fi

OPENOCD_ARGS=()
for script_dir in /opt/openocd/share/openocd/scripts /usr/share/openocd/scripts; do
  if [[ -d "$script_dir" ]]; then
    OPENOCD_ARGS+=(-s "$script_dir")
  fi
done

LOADER_ELF=""
if [[ "$SKETCH_ONLY" -eq 0 ]]; then
  if [[ "$HAS_HOST_LOADER" -eq 1 ]]; then
    LOADER_ELF="${REMOTE_DIR}/loader.elf"
  else
    for candidate in "${HOME}/.arduino15/packages/arduino/hardware/zephyr" /home/arduino/.arduino15/packages/arduino/hardware/zephyr; do
      if [[ -d "$candidate" ]]; then
        found="$(find "$candidate" -maxdepth 3 -type f -name 'zephyr-arduino_uno_q_stm32u585xx.elf' 2>/dev/null | sort -V | tail -n 1 || true)"
        if [[ -n "$found" && -f "$found" ]]; then
          LOADER_ELF="$found"
          break
        fi
      fi
    done
    if [[ -z "$LOADER_ELF" ]]; then
      echo "[tactus-flash-mcu] WARNING: Zephyr loader ELF not found on host or board; flashing sketch only at 0x08100000" >&2
    fi
  fi
fi

OPENOCD_ARGS+=(-f "${REMOTE_DIR}/unoq-gpiod.cfg")
if [[ -n "$LOADER_ELF" ]]; then
  OPENOCD_ARGS+=(-c "set filename0 ${LOADER_ELF}")
fi
OPENOCD_ARGS+=(
  -c "set filename1 ${REMOTE_DIR}/sketch.elf-zsk.bin"
  -f "${REMOTE_DIR}/unoq-flash-sketch.cfg"
)

"$OPENOCD_BIN" "${OPENOCD_ARGS[@]}"

if [[ "$ENABLE_BRIDGE" -eq 1 ]]; then
  CONFIG_PATH="/var/lib/tactus/config.toml"
  sudo test -f "$CONFIG_PATH" || {
    echo "ERROR: $CONFIG_PATH not found on board" >&2
    exit 1
  }
  sudo python3 - "$CONFIG_PATH" <<'PYEOF'
import pathlib
import re
import sys

path = pathlib.Path(sys.argv[1])
text = path.read_text(encoding="utf-8")

if re.search(r"(?m)^\[mcu_bridge\]\s*$", text):
    pattern = re.compile(r"(?ms)(^\[mcu_bridge\]\s*\n(?:(?!^\[).*\n)*?)")
    def replace_section(match: re.Match[str]) -> str:
        block = match.group(1)
        if re.search(r"(?m)^enabled\s*=", block):
            return re.sub(r"(?m)^enabled\s*=.*$", "enabled = true", block, count=1)
        return block + "enabled = true\n"
    updated = pattern.sub(replace_section, text, count=1)
else:
    suffix = "" if text.endswith("\n") else "\n"
    updated = (
        text
        + suffix
        + "\n[mcu_bridge]\nenabled = true\nsocket_path = \"/var/run/arduino-router.sock\"\n"
    )

if updated != text:
    tmp = path.with_suffix(".toml.tmp")
    tmp.write_text(updated, encoding="utf-8")
    tmp.replace(path)
PYEOF
  if id -u tactus >/dev/null 2>&1; then
    sudo chown tactus:tactus "$CONFIG_PATH"
  fi
  if systemctl is-active --quiet tactusd.service || systemctl is-enabled --quiet tactusd.service; then
    sudo systemctl restart tactusd.service
  fi
fi
REMOTE_EOF

log "MCU firmware flashed cleanly."
