#!/usr/bin/env bash
# Build the UNO Q 5 GHz kernel .deb. Shared by .github/workflows/build-unoq-kernel.yml and
# system/scripts/build-unoq-kernel-local.sh; run inside a Debian trixie environment that has the
# kernel build dependencies installed (see system/kernel/Containerfile).
#
# Env (all optional): KERNEL_REPO, KERNEL_REF, DEB_IMAGES_REPO, DEB_IMAGES_REF,
#   FRAGMENT (default: tactus-5ghz.config next to this script), WORK (default: $PWD),
#   OUT (if set, copy linux-image .deb, .changes and kernel.config there).
set -euo pipefail

KERNEL_REPO="${KERNEL_REPO:-https://github.com/arduino/linux-qcom}"
KERNEL_REF="${KERNEL_REF:-qcom-v7.2.11-arduino}"
DEB_IMAGES_REPO="${DEB_IMAGES_REPO:-https://github.com/arduino/arduino-deb-images}"
DEB_IMAGES_REF="${DEB_IMAGES_REF:-arduino}"
FRAGMENT="${FRAGMENT:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/tactus-5ghz.config}"
WORK="${WORK:-$PWD}"
OUT="${OUT:-}"

cd "$WORK"
# drop packages from previous runs so only the fresh build is exported
rm -f ./*.deb ./*.changes ./*.buildinfo
rm -rf arduino-deb-images
git clone --depth=1 --branch "$DEB_IMAGES_REF" "$DEB_IMAGES_REPO" arduino-deb-images

# build-linux-deb.py clones into ./linux; reuse an existing tree (incremental rebuild) via --local-dir.
if [ -d linux/.git ]; then
    git -C linux fetch --depth=1 origin "$KERNEL_REF"
    git -C linux checkout -f FETCH_HEAD
    src_args=(--local-dir linux)
else
    src_args=(--repo "$KERNEL_REPO" --ref "$KERNEL_REF")
fi

base_configs=(arduino-deb-images/kernel-configs/*.config)
python3 arduino-deb-images/scripts/build-linux-deb.py "${src_args[@]}" "${base_configs[@]}" "$FRAGMENT"

# olddefconfig silently drops options whose dependencies are off (e.g. CFG80211_CERTIFICATION_ONUS
# needs EXPERT), so check that every line of the fragment survived into the merged .config.
fail=0
while IFS= read -r line; do
    case "$line" in
        CONFIG_*=n)
            if grep -q "^${line%=n}=" linux/.config; then
                echo "kernel config: ${line%=n} is set but the fragment requires =n" >&2
                fail=1
            fi
            ;;
        CONFIG_*=*)
            if ! grep -qxF -- "$line" linux/.config; then
                echo "kernel config: missing from merged .config: $line" >&2
                fail=1
            fi
            ;;
        *) ;;
    esac
done <"$FRAGMENT"
[ "$fail" -eq 0 ] || exit 1

if [ -n "$OUT" ]; then
    rm -f "$OUT"/*.deb "$OUT"/*.changes
    for f in linux-image-*.deb; do
        case "$f" in *-dbg_*) continue ;; esac
        cp -v "$f" "$OUT"/
    done
    cp -v ./*.changes "$OUT"/
    cp -v linux/.config "$OUT"/kernel.config
fi
