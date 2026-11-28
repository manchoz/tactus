#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Fast host-side tests for release packaging, [mcu_bridge] config updates,
# and systemd-boot +2 boot-counting logic.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

WORK_DIR="$(mktemp -d)"
cleanup() {
  rm -rf "$WORK_DIR"
}
trap cleanup EXIT

fail() {
  printf '[test-release-scripts] FAIL: %s\n' "$*" >&2
  exit 1
}

verify_sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum -c "$1"
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 -c "$1"
  else
    fail "neither sha256sum nor shasum found on PATH"
  fi
}

file_mode_oct() {
  python3 -c 'import os, sys; print(format(os.stat(sys.argv[1]).st_mode & 0o777, "04o"))' "$1"
}

# ---------------------------------------------------------------------------
# Test 1: package-release.sh with pre-release version & full bundle layout
# ---------------------------------------------------------------------------
pkg_in="${WORK_DIR}/pkg-in"
pkg_out="${WORK_DIR}/pkg-out"
mkdir -p "${pkg_in}/debs"
printf 'dummy-tactusd\n' > "${pkg_in}/tactusd"
printf 'dummy-mcu-bin\n' > "${pkg_in}/tactus-zephyr.ino.elf-zsk.bin"
printf 'dummy-loader\n' > "${pkg_in}/zephyr-arduino_uno_q_stm32u585xx.elf"
printf 'dummy-deb\n' > "${pkg_in}/debs/hostapd_2.10_arm64.deb"
printf 'dummy-kernel\n' > "${pkg_in}/linux-image-7.2.11-arduino1-tactus+_1_arm64.deb"

"${SCRIPT_DIR}/package-release.sh" \
  --version "0.2.0-rc.1" \
  --tactusd "${pkg_in}/tactusd" \
  --mcu-bin "${pkg_in}/tactus-zephyr.ino.elf-zsk.bin" \
  --mcu-loader "${pkg_in}/zephyr-arduino_uno_q_stm32u585xx.elf" \
  --deb-dir "${pkg_in}/debs" \
  --kernel-deb "${pkg_in}/linux-image-7.2.11-arduino1-tactus+_1_arm64.deb" \
  --out-dir "$pkg_out" >/dev/null

(cd "$pkg_out" && verify_sha256_file SHA256SUMS >/dev/null) || fail "top-level SHA256SUMS verification failed"

extract_dir="${WORK_DIR}/extracted"
mkdir -p "$extract_dir"
tar -xzf "${pkg_out}/tactus-0.2.0-rc.1-unoq-arm64.tar.gz" -C "$extract_dir"
bundle_root="${extract_dir}/tactus-0.2.0-rc.1-unoq-arm64"
[[ -d "$bundle_root" ]] || fail "missing extracted bundle root"
(cd "$bundle_root" && verify_sha256_file SHA256SUMS >/dev/null) || fail "internal SHA256SUMS verification failed"
[[ "$(cat "${bundle_root}/VERSION")" == "0.2.0-rc.1" ]] || fail "unexpected VERSION contents"
[[ -x "${bundle_root}/system/scripts/flash-mcu-board.sh" ]] || fail "flash-mcu-board.sh missing or not executable in bundle"

tarballs=("${pkg_out}"/tactus-*-unoq-arm64.tar.gz)
tarball="$(basename "${tarballs[0]}")"
resolved_ver="${tarball#tactus-}"
resolved_ver="${resolved_ver%-unoq-arm64.tar.gz}"
[[ "$resolved_ver" == "0.2.0-rc.1" ]] || fail "version extraction failed: got '${resolved_ver}'"
echo "PASS: Test 1 (package-release.sh bundle layout, checksums, and pre-release version)"

# ---------------------------------------------------------------------------
# Test 2: flash-mcu-board.sh --config-only (atomic update, TOML validity & repair)
# ---------------------------------------------------------------------------
assert_valid_mcu_toml() {
  python3 - "$1" <<'PYEOF'
import pathlib, sys
text = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
try:
    import tomllib
except ImportError:
    tomllib = None
if tomllib is not None:
    data = tomllib.loads(text)
    assert data["mcu_bridge"]["enabled"] is True, f"expected enabled=True, got {data}"
    assert data["mcu_bridge"]["socket_path"] == "/var/run/arduino-router.sock", f"unexpected socket_path: {data}"
in_sec = False
enabled_lines = []
for line in text.splitlines():
    s = line.strip()
    if s.startswith("[") and s.endswith("]"):
        in_sec = (s == "[mcu_bridge]")
    elif in_sec and s.startswith("enabled"):
        enabled_lines.append(s)
assert enabled_lines == ["enabled = true"], f"expected single 'enabled = true' in [mcu_bridge], got {enabled_lines}"
PYEOF
}

cfg1="${WORK_DIR}/config-existing.toml"
cat > "$cfg1" <<'EOF'
[link]
enabled = true

[mcu_bridge]
enabled = false
socket_path = "/var/run/arduino-router.sock"
EOF
chmod 0640 "$cfg1"

"${SCRIPT_DIR}/flash-mcu-board.sh" --config-only --no-restart --config "$cfg1"
"${SCRIPT_DIR}/flash-mcu-board.sh" --config-only --no-restart --config "$cfg1"
assert_valid_mcu_toml "$cfg1" || fail "cfg1 failed TOML/[mcu_bridge] validation"
[[ "$(file_mode_oct "$cfg1")" == "0640" ]] || fail "config mode changed from 0640 to $(file_mode_oct "$cfg1")"
[[ ! -f "${WORK_DIR}/config-existing.toml.tmp" ]] || fail "leftover .toml.tmp file"

cfg2="${WORK_DIR}/config-missing-section.toml"
printf '[link]\nenabled = true\n' > "$cfg2"
chmod 0640 "$cfg2"

"${SCRIPT_DIR}/flash-mcu-board.sh" --config-only --no-restart --config "$cfg2"
"${SCRIPT_DIR}/flash-mcu-board.sh" --config-only --no-restart --config "$cfg2"
[[ "$(grep -c '^\[mcu_bridge\]$' "$cfg2")" -eq 1 ]] || fail "[mcu_bridge] section duplicated on re-run"
assert_valid_mcu_toml "$cfg2" || fail "cfg2 failed TOML/[mcu_bridge] validation"
[[ "$(file_mode_oct "$cfg2")" == "0640" ]] || fail "config2 mode changed from 0640"

cfg3="${WORK_DIR}/config-missing-enabled-no-newline.toml"
printf '[mcu_bridge]\nsocket_path = "/var/run/arduino-router.sock"' > "$cfg3"
chmod 0640 "$cfg3"
"${SCRIPT_DIR}/flash-mcu-board.sh" --config-only --no-restart --config "$cfg3"
assert_valid_mcu_toml "$cfg3" || fail "cfg3 failed TOML/[mcu_bridge] validation"

cfg4="${WORK_DIR}/config-duplicate-enabled.toml"
cat > "$cfg4" <<'EOF'
[mcu_bridge]
enabled = true
enabled = false
socket_path = "/var/run/arduino-router.sock"
EOF
chmod 0640 "$cfg4"
"${SCRIPT_DIR}/flash-mcu-board.sh" --config-only --no-restart --config "$cfg4"
assert_valid_mcu_toml "$cfg4" || fail "cfg4 failed duplicate-key repair validation"
echo "PASS: Test 2 (flash-mcu-board.sh --config-only atomic update, TOML validation & repair)"

# ---------------------------------------------------------------------------
# Test 3: install-unoq-board.sh configure_systemd_boot_entry (+2 rollback)
# ---------------------------------------------------------------------------
# shellcheck source=system/scripts/install-unoq-board.sh
source "${SCRIPT_DIR}/install-unoq-board.sh"

loader_dir="${WORK_DIR}/efi/loader"
entries_dir="${loader_dir}/entries"
mkdir -p "$entries_dir"
printf 'timeout 3\ndefault old-entry.conf\n' > "${loader_dir}/loader.conf"
printf 'title Stock\n' > "${entries_dir}/abc123-7.2.11-arduino1+.conf"
printf 'title Custom\n' > "${entries_dir}/abc123-7.2.11-arduino1-tactus+.conf"

LC_ALL=C configure_systemd_boot_entry "$loader_dir" "7.2.11-arduino1-tactus+" "abc123" >/dev/null
[[ -f "${loader_dir}/loader.conf.bak" ]] || fail "loader.conf.bak not created"
[[ -f "${entries_dir}/abc123-7.2.11-arduino1+.conf" ]] || fail "stock boot entry was touched"
[[ -f "${entries_dir}/abc123-7.2.11-arduino1-tactus++2.conf" ]] || fail "custom +2.conf entry not created"
[[ ! -f "${entries_dir}/abc123-7.2.11-arduino1-tactus+.conf" ]] || fail "uncounted custom .conf still exists"
grep -qx 'default abc123-\*' "${loader_dir}/loader.conf" || fail "loader.conf default not updated"

# Re-run idempotency
LC_ALL=C configure_systemd_boot_entry "$loader_dir" "7.2.11-arduino1-tactus+" "abc123" >/dev/null
custom_count="$(find "$entries_dir" -maxdepth 1 -name 'abc123-7.2.11-arduino1-tactus+*.conf' | wc -l | tr -d ' ')"
[[ "$custom_count" -eq 1 ]] || fail "expected 1 custom boot entry on re-run, found ${custom_count}"

# Dirty duplicate state (both uncounted .conf and stale +2-1.conf exist)
printf 'title Fresh\n' > "${entries_dir}/abc123-7.2.11-arduino1-tactus+.conf"
mv "${entries_dir}/abc123-7.2.11-arduino1-tactus++2.conf" "${entries_dir}/abc123-7.2.11-arduino1-tactus++2-1.conf"
LC_ALL=C configure_systemd_boot_entry "$loader_dir" "7.2.11-arduino1-tactus+" "abc123" >/dev/null
[[ -f "${entries_dir}/abc123-7.2.11-arduino1-tactus++2.conf" ]] || fail "fresh +2.conf missing after dirty state cleanup"
[[ ! -f "${entries_dir}/abc123-7.2.11-arduino1-tactus++2-1.conf" ]] || fail "stale +2-1.conf not removed"
[[ ! -f "${entries_dir}/abc123-7.2.11-arduino1-tactus+.conf" ]] || fail "uncounted .conf survived dirty state cleanup"
echo "PASS: Test 3 (configure_systemd_boot_entry +2 rollback & duplicate cleanup)"

# ---------------------------------------------------------------------------
# Test 4: build-release-local.sh --help & --dry-run (CI-safe wiring checks)
# ---------------------------------------------------------------------------
"${SCRIPT_DIR}/build-release-local.sh" --help >/dev/null

dry_default="$("${SCRIPT_DIR}/build-release-local.sh" --dry-run)"
grep -q 'tactusd : BUILD' <<<"$dry_default" || fail "default dry-run did not plan tactusd BUILD"
grep -q 'mcu     : BUILD' <<<"$dry_default" || fail "default dry-run did not plan mcu BUILD"
grep -q 'debs    : BUILD' <<<"$dry_default" || fail "default dry-run did not plan debs BUILD"
grep -q 'kernel  : BUILD' <<<"$dry_default" || fail "default dry-run did not plan kernel BUILD"
grep -q 'package-release.sh' <<<"$dry_default" || fail "default dry-run did not plan package-release.sh"

cache_dir="${WORK_DIR}/local-cache"
mkdir -p "${cache_dir}/kernel" "${cache_dir}/debs"
printf 'dummy-tactusd\n' > "${cache_dir}/tactusd"
printf 'dummy-mcu-bin\n' > "${cache_dir}/tactus-zephyr.ino.elf-zsk.bin"
printf 'dummy-loader\n' > "${cache_dir}/zephyr-arduino_uno_q_stm32u585xx.elf"
printf 'dummy-deb\n' > "${cache_dir}/debs/hostapd_2.10_arm64.deb"
printf 'dummy-kernel\n' > "${cache_dir}/kernel/linux-image-7.2.11-arduino1-tactus+_1_arm64.deb"
printf 'dummy-dbg\n' > "${cache_dir}/kernel/linux-image-7.2.11-arduino1-tactus+-dbg_1_arm64.deb"

dry_reuse="$("${SCRIPT_DIR}/build-release-local.sh" \
  --dry-run \
  --reuse-cache \
  --reuse-tactusd \
  --reuse-mcu \
  --tactusd "${cache_dir}/tactusd" \
  --mcu-bin "${cache_dir}/tactus-zephyr.ino.elf-zsk.bin" \
  --mcu-loader "${cache_dir}/zephyr-arduino_uno_q_stm32u585xx.elf" \
  --deb-dir "${cache_dir}/debs" \
  --kernel-dir "${cache_dir}/kernel")"
grep -q 'tactusd : REUSE' <<<"$dry_reuse" || fail "reuse dry-run did not plan tactusd REUSE"
grep -q 'mcu     : REUSE' <<<"$dry_reuse" || fail "reuse dry-run did not plan mcu REUSE"
grep -q 'debs    : REUSE' <<<"$dry_reuse" || fail "reuse dry-run did not plan debs REUSE"
grep -q 'kernel  : REUSE (.*linux-image-7\.2\.11-arduino1-tactus+_1_arm64\.deb)' <<<"$dry_reuse" || fail "reuse dry-run did not pick non-dbg kernel .deb"

dry_partial="$("${SCRIPT_DIR}/build-release-local.sh" --dry-run --partial)"
grep -q 'mcu     : SKIP' <<<"$dry_partial" || fail "partial dry-run did not plan mcu SKIP"
grep -q 'debs    : SKIP' <<<"$dry_partial" || fail "partial dry-run did not plan debs SKIP"
grep -q 'kernel  : SKIP' <<<"$dry_partial" || fail "partial dry-run did not plan kernel SKIP"
grep -q -- '--allow-partial' <<<"$dry_partial" || fail "partial dry-run did not pass --allow-partial"
echo "PASS: Test 4 (build-release-local.sh --help, --dry-run, cache reuse, and --partial)"
