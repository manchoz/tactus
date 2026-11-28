#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Board-side STM32U585 OpenOCD flasher and [mcu_bridge] config updater.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

SKETCH_BIN=""
LOADER_ELF=""
SKETCH_ONLY=0
OPENOCD_DIR="${SCRIPT_DIR}/../openocd"
ENABLE_BRIDGE=0
CONFIG_PATH="/var/lib/tactus/config.toml"
CONFIG_ONLY=0
NO_RESTART=0

usage() {
  cat <<'EOF'
Usage: flash-mcu-board.sh [OPTIONS]

Flash STM32U585 firmware on the UNO Q via OpenOCD (linuxgpiod) and/or enable
[mcu_bridge] in config.toml.

Options:
  --sketch FILE        Path to exported .elf-zsk.bin (required unless --config-only)
  --loader FILE        Optional Zephyr loader ELF (zephyr-arduino_uno_q_stm32u585xx.elf)
  --sketch-only        Flash only the sketch at 0x08100000 (skip loader verify/flash)
  --openocd-dir DIR    Directory containing unoq-gpiod.cfg and unoq-flash-sketch.cfg
  --enable-bridge      Set [mcu_bridge] enabled = true in config and restart tactusd
  --config PATH        Config file path (default: /var/lib/tactus/config.toml)
  --config-only        Update config only without running OpenOCD (implies --enable-bridge)
  --no-restart         Skip systemctl restart tactusd.service
  -h, --help           Show this help message
EOF
}

log() {
  printf '[tactus-flash-mcu] %s\n' "$*"
}

die() {
  printf '[tactus-flash-mcu] ERROR: %s\n' "$*" >&2
  exit 1
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --sketch)
      SKETCH_BIN="${2:?--sketch requires a file path}"
      shift 2
      ;;
    --loader)
      LOADER_ELF="${2:?--loader requires a file path}"
      shift 2
      ;;
    --sketch-only)
      SKETCH_ONLY=1
      shift
      ;;
    --openocd-dir)
      OPENOCD_DIR="${2:?--openocd-dir requires a directory path}"
      shift 2
      ;;
    --enable-bridge)
      ENABLE_BRIDGE=1
      shift
      ;;
    --config)
      CONFIG_PATH="${2:?--config requires a file path}"
      shift 2
      ;;
    --config-only)
      CONFIG_ONLY=1
      ENABLE_BRIDGE=1
      shift
      ;;
    --no-restart)
      NO_RESTART=1
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

run_privileged() {
  if [[ "$EUID" -eq 0 ]] || [[ -w "$CONFIG_PATH" && -w "$(dirname "$CONFIG_PATH")" ]]; then
    "$@"
  else
    sudo "$@"
  fi
}

flash_openocd() {
  [[ -n "$SKETCH_BIN" ]] || die "--sketch is required unless --config-only is set"
  [[ -f "$SKETCH_BIN" ]] || die "sketch binary not found: $SKETCH_BIN"
  [[ -f "${OPENOCD_DIR}/unoq-gpiod.cfg" ]] || die "OpenOCD adapter config missing: ${OPENOCD_DIR}/unoq-gpiod.cfg"
  [[ -f "${OPENOCD_DIR}/unoq-flash-sketch.cfg" ]] || die "OpenOCD flash config missing: ${OPENOCD_DIR}/unoq-flash-sketch.cfg"

  local openocd_bin=""
  if [[ -x /opt/openocd/bin/openocd ]]; then
    openocd_bin="/opt/openocd/bin/openocd"
  elif command -v openocd >/dev/null 2>&1; then
    openocd_bin="$(command -v openocd)"
  else
    die "neither /opt/openocd/bin/openocd nor openocd in PATH found on board"
  fi

  local openocd_args=()
  local script_dir=""
  for script_dir in /opt/openocd/share/openocd/scripts /usr/share/openocd/scripts; do
    if [[ -d "$script_dir" ]]; then
      openocd_args+=(-s "$script_dir")
    fi
  done

  if [[ "$SKETCH_ONLY" -eq 0 && -z "$LOADER_ELF" ]]; then
    local candidate="" found=""
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
      log "WARNING: Zephyr loader ELF not found on host or board; flashing sketch only at 0x08100000."
    fi
  fi

  openocd_args+=(-f "${OPENOCD_DIR}/unoq-gpiod.cfg")
  if [[ "$SKETCH_ONLY" -eq 0 && -n "$LOADER_ELF" ]]; then
    [[ -f "$LOADER_ELF" ]] || die "Zephyr loader ELF not found: $LOADER_ELF"
    openocd_args+=(-c "set filename0 {${LOADER_ELF}}")
  fi
  openocd_args+=(
    -c "set filename1 {${SKETCH_BIN}}"
    -f "${OPENOCD_DIR}/unoq-flash-sketch.cfg"
  )

  log "Flashing STM32U585 via linuxgpiod OpenOCD..."
  "$openocd_bin" "${openocd_args[@]}"
}

enable_mcu_bridge_config() {
  run_privileged test -f "$CONFIG_PATH" || die "$CONFIG_PATH not found on board"

  run_privileged python3 - "$CONFIG_PATH" <<'PYEOF'
import os
import pathlib
import re
import stat
import sys

path = pathlib.Path(sys.argv[1])
text = path.read_text(encoding="utf-8")
orig_mode = stat.S_IMODE(path.stat().st_mode)
if str(path) == "/var/lib/tactus/config.toml" and (orig_mode & 0o007) != 0:
    mode = 0o640
else:
    mode = orig_mode

lines = text.splitlines(keepends=True)
out: list[str] = []
in_section = False
found_section = False
wrote_enabled = False
header_re = re.compile(r"^\s*\[([^\]]+)\]\s*(?:#.*)?$")
enabled_re = re.compile(r"^\s*enabled\s*=")

for line in lines:
    header_match = header_re.match(line)
    if header_match:
        if in_section and not wrote_enabled:
            if out and not out[-1].endswith("\n"):
                out[-1] += "\n"
            out.append("enabled = true\n")
            wrote_enabled = True
        in_section = header_match.group(1).strip() == "mcu_bridge"
        if in_section:
            found_section = True
        out.append(line)
        continue

    if in_section and enabled_re.match(line):
        if not wrote_enabled:
            out.append("enabled = true\n")
            wrote_enabled = True
        continue

    out.append(line)

if in_section and not wrote_enabled:
    if out and not out[-1].endswith("\n"):
        out[-1] += "\n"
    out.append("enabled = true\n")
elif not found_section:
    if out and not out[-1].endswith("\n"):
        out[-1] += "\n"
    if out and out[-1].strip() != "":
        out.append("\n")
    out.append('[mcu_bridge]\nenabled = true\nsocket_path = "/var/run/arduino-router.sock"\n')

updated = "".join(out)

try:
    import tomllib
except ImportError:
    tomllib = None
if tomllib is not None:
    parsed = tomllib.loads(updated)
    if parsed.get("mcu_bridge", {}).get("enabled") is not True:
        raise RuntimeError("validated TOML missing [mcu_bridge].enabled = true")

if updated != text or orig_mode != mode:
    tmp = path.with_suffix(".toml.tmp")
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, mode)
    with os.fdopen(fd, "wb") as f:
        f.write(updated.encode("utf-8"))
        f.flush()
        os.fsync(f.fileno())
    os.chmod(tmp, mode)
    os.replace(tmp, path)
    dir_fd = os.open(path.parent, os.O_RDONLY)
    try:
        os.fsync(dir_fd)
    finally:
        os.close(dir_fd)
PYEOF

  if id -u tactus >/dev/null 2>&1 && { [[ "$EUID" -eq 0 ]] || [[ "$CONFIG_PATH" == "/var/lib/tactus/config.toml" ]]; }; then
    run_privileged chown tactus:tactus "$CONFIG_PATH"
  fi
  if [[ "$NO_RESTART" -eq 0 ]]; then
    if systemctl is-active --quiet tactusd.service || systemctl is-enabled --quiet tactusd.service; then
      if [[ "$EUID" -eq 0 ]]; then
        systemctl restart tactusd.service
      else
        sudo systemctl restart tactusd.service
      fi
    fi
  fi
}

if [[ "$CONFIG_ONLY" -eq 0 ]]; then
  flash_openocd
fi

if [[ "$ENABLE_BRIDGE" -eq 1 ]]; then
  enable_mcu_bridge_config
fi
