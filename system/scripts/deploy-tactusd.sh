#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Cross-compile (or reuse existing) tactusd release binary, copy it over SSH,
# atomically install to /usr/local/bin/tactusd, and restart tactusd.service.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

SSH_TARGET="tactus"
SSH_IDENTITY=""
ZIGBUILD_TARGET="aarch64-unknown-linux-gnu.2.36"
BINARY_PATH=""
SKIP_BUILD=0
VERIFY=1

usage() {
  cat <<'EOF'
Usage:
  deploy-tactusd.sh [OPTIONS]

Cross-compile `tactusd` (`cargo zigbuild`) and deploy the binary over SSH without
running full board provisioning. Use `provision-factory-unoq.sh` instead whenever
`system/` unit, hostapd, dnsmasq, or Avahi files change.

Options:
  --ssh TARGET            SSH destination (default: tactus, e.g. arduino@tactus.local)
  -i, --identity FILE     SSH private key path
  --binary FILE           Prebuilt `tactusd` binary to deploy directly (implies --skip-build)
  --skip-build            Reuse existing `target/aarch64-unknown-linux-gnu/release/tactusd`
  --target TRIPLE         Cargo zigbuild target (default: aarch64-unknown-linux-gnu.2.36)
  --no-verify             Skip post-restart SHA256 and RT core-isolation checks
  -h, --help              Show this help message
EOF
}

log() {
  printf '[tactus-deploy] %s\n' "$*"
}

die() {
  printf '[tactus-deploy] ERROR: %s\n' "$*" >&2
  exit 1
}

compute_sha256() {
  local file="$1"
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | awk '{print $1}'
  elif command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{print $1}'
  else
    die "neither shasum nor sha256sum found in PATH"
  fi
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
    --binary)
      BINARY_PATH="${2:?--binary requires a file path}"
      SKIP_BUILD=1
      shift 2
      ;;
    --skip-build)
      SKIP_BUILD=1
      shift
      ;;
    --target)
      ZIGBUILD_TARGET="${2:?--target requires a target triple}"
      shift 2
      ;;
    --no-verify)
      VERIFY=0
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

if [[ -n "$SSH_IDENTITY" && ! -f "$SSH_IDENTITY" ]]; then
  die "SSH identity file not found: $SSH_IDENTITY"
fi

if [[ "$SKIP_BUILD" -eq 0 ]]; then
  command -v cargo >/dev/null 2>&1 || die "cargo not found in PATH"
  log "Building release tactusd (${ZIGBUILD_TARGET})..."
  (cd "$REPO_ROOT" && cargo zigbuild --workspace --release --target "$ZIGBUILD_TARGET")
fi

if [[ -z "$BINARY_PATH" ]]; then
  BASE_TRIPLE="${ZIGBUILD_TARGET%%.[0-9]*}"
  BINARY_PATH="${REPO_ROOT}/target/${BASE_TRIPLE}/release/tactusd"
fi

[[ -f "$BINARY_PATH" ]] || die "tactusd binary not found: $BINARY_PATH"

LOCAL_SHA="$(compute_sha256 "$BINARY_PATH")"

SSH_OPTS=(-o ConnectTimeout=10)
if [[ -n "$SSH_IDENTITY" ]]; then
  SSH_OPTS+=(-i "$SSH_IDENTITY")
fi

REMOTE_TMP="/tmp/tactusd-deploy-$$"
UPLOADED=0

cleanup() {
  local rc=$?
  if [[ "$UPLOADED" -eq 1 ]]; then
    ssh "${SSH_OPTS[@]}" "$SSH_TARGET" rm -f -- "$REMOTE_TMP" >/dev/null 2>&1 || true
  fi
  return "$rc"
}
trap cleanup EXIT

log "Copying $(basename "$BINARY_PATH") (${LOCAL_SHA:0:12}...) to ${SSH_TARGET}:${REMOTE_TMP}..."
scp "${SSH_OPTS[@]}" "$BINARY_PATH" "${SSH_TARGET}:${REMOTE_TMP}"
UPLOADED=1

log "Installing /usr/local/bin/tactusd and restarting tactusd.service..."
ssh "${SSH_OPTS[@]}" "$SSH_TARGET" bash -s -- "$REMOTE_TMP" "$LOCAL_SHA" "$VERIFY" <<'REMOTE_EOF'
set -euo pipefail

REMOTE_TMP="$1"
EXPECTED_SHA="$2"
VERIFY="$3"

sudo install -m 0755 -o root -g root "$REMOTE_TMP" /usr/local/bin/tactusd
rm -f "$REMOTE_TMP"
sudo systemctl restart tactusd.service

if [[ "$VERIFY" -eq 1 ]]; then
  sleep 1
  ACTIVE_STATE="$(systemctl is-active tactusd.service || true)"
  if [[ "$ACTIVE_STATE" != "active" ]]; then
    echo "ERROR: tactusd.service state is '$ACTIVE_STATE'" >&2
    journalctl -u tactusd.service -n 25 --no-pager >&2 || true
    exit 1
  fi

  ACTUAL_SHA="$(sha256sum /usr/local/bin/tactusd | awk '{print $1}')"
  if [[ "$ACTUAL_SHA" != "$EXPECTED_SHA" ]]; then
    echo "ERROR: SHA256 mismatch (expected $EXPECTED_SHA, got $ACTUAL_SHA)" >&2
    exit 1
  fi

  echo "[tactus-deploy] tactusd.service active (sha256=${ACTUAL_SHA})"
  ps -eLo pid,tid,psr,cls,rtprio,comm | grep -E "tactus|rt-sync|asio" || true
fi
REMOTE_EOF

log "Deploy complete."
