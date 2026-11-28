#!/usr/bin/env bash
set -euo pipefail

# Deterministic architectural invariant checks enforced in CI (AGENTS.md).

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT_DIR"

fail() {
  echo "INVARIANT FAILED: $*" >&2
  exit 1
}

strip_cfg_test() {
  awk '/^#\[cfg\(test\)\]/{exit} {print}' "$1"
}

# 1. RT thread purity (crates/tactusd/src/rt.rs non-test code):
#    No tokio, mutexes, or heap allocations.
if strip_cfg_test crates/tactusd/src/rt.rs | grep -nE 'tokio::|Mutex|RwLock|Vec::|String::|Box::|vec!|format!'; then
  fail "crates/tactusd/src/rt.rs contains forbidden async/lock/allocation primitives outside #[cfg(test)]"
fi

# 2. Zero-alloc pure sync core (crates/tactus-sync-core/src/*.rs non-test code):
for f in crates/tactus-sync-core/src/*.rs; do
  if strip_cfg_test "$f" | grep -nE 'Mutex|RwLock|Vec::|String::|Box::|HashMap|BTreeMap|vec!|format!'; then
    fail "$f contains forbidden lock/allocation primitives outside #[cfg(test)]"
  fi
done

# 3. Offline-safe build.rs (no git or network downloads):
if grep -nE 'Command::new\("git"\)|Command::new\("curl"\)|Command::new\("wget"\)|https?://' crates/tactus-link-sys/build.rs; then
  fail "crates/tactus-link-sys/build.rs must remain strictly offline (no git/network calls)"
fi

# 4. Systemd & WiFi AP decoupling / supervision invariants:
if grep -nE '^After=.*tactus-ap\.service' system/systemd/tactusd.service; then
  fail "tactusd.service must NOT order After=tactus-ap.service (keep boot decoupled)"
fi
if ! grep -qE '^Wants=.*tactus-ap\.service' system/systemd/tactusd.service; then
  fail "tactusd.service must declare Wants=tactus-ap.service"
fi
if ! grep -qE '^Upholds=tactus-dhcp\.service' system/systemd/tactus-ap.service; then
  fail "tactus-ap.service must supervise DHCP via Upholds=tactus-dhcp.service"
fi
if grep -nE '^Wants=.*tactus-dhcp\.service' system/systemd/tactus-ap.service; then
  fail "tactus-ap.service must NOT use Wants=tactus-dhcp.service alongside BindsTo="
fi

# 5. Dedicated AP DHCP-only config (omit default router option 3 & DNS option 6):
if ! grep -qE '^dhcp-option=3$' system/dnsmasq/tactus-ap.conf || ! grep -qE '^dhcp-option=6$' system/dnsmasq/tactus-ap.conf; then
  fail "system/dnsmasq/tactus-ap.conf must omit DHCP options 3 and 6"
fi

echo "All Tactus architectural invariants passed."
