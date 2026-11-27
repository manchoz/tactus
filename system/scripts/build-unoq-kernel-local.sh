#!/usr/bin/env bash
# Build the UNO Q 5 GHz kernel .deb locally inside an Apple `container` VM.
# Same inputs as .github/workflows/build-unoq-kernel.yml:
#   arduino/linux-qcom + arduino-deb-images kernel-configs + system/kernel/tactus-5ghz.config
set -euo pipefail

KERNEL_REPO="https://github.com/arduino/linux-qcom"
KERNEL_REF="qcom-v7.2.11-arduino"
DEB_IMAGES_REPO="https://github.com/arduino/arduino-deb-images"
DEB_IMAGES_REF="arduino"
CPUS=8
MEMORY="16G"
OUT_DIR=""
CLEAN=0
IMAGE="tactus-unoq-kernel-build"
VOLUME="unoq-kernel-src"

usage() {
    cat <<EOF
Usage: $0 [options]
  --kernel-repo URL      Kernel git repo (default: $KERNEL_REPO)
  --kernel-ref REF       Kernel branch/tag (default: $KERNEL_REF)
  --deb-images-ref REF   arduino-deb-images ref (default: $DEB_IMAGES_REF)
  --cpus N               VM CPUs (default: $CPUS)
  --memory SIZE          VM memory, e.g. 16G (default: $MEMORY)
  --out DIR              Output dir for .deb files (default: <repo>/target/unoq-kernel)
  --clean                Delete the persistent kernel source volume first
  -h, --help             Show this help
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --kernel-repo) KERNEL_REPO="$2"; shift 2 ;;
        --kernel-ref) KERNEL_REF="$2"; shift 2 ;;
        --deb-images-ref) DEB_IMAGES_REF="$2"; shift 2 ;;
        --cpus) CPUS="$2"; shift 2 ;;
        --memory) MEMORY="$2"; shift 2 ;;
        --out) OUT_DIR="$2"; shift 2 ;;
        --clean) CLEAN=1; shift ;;
        -h | --help) usage; exit 0 ;;
        *) echo "Unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT_DIR="${OUT_DIR:-$REPO_ROOT/target/unoq-kernel}"
mkdir -p "$OUT_DIR"
OUT_DIR="$(cd "$OUT_DIR" && pwd)"

command -v container >/dev/null || { echo "Apple 'container' CLI not found (brew install container)" >&2; exit 1; }
if ! container system status >/dev/null 2>&1; then
    echo "[unoq-kernel] starting container system..."
    container system start
fi

if [ "$CLEAN" -eq 1 ]; then
    echo "[unoq-kernel] removing volume $VOLUME"
    container volume delete "$VOLUME" 2>/dev/null || true
fi
container volume create "$VOLUME" >/dev/null 2>&1 || true

echo "[unoq-kernel] building image $IMAGE"
container build --tag "$IMAGE" --file "$REPO_ROOT/system/kernel/Containerfile" "$REPO_ROOT/system/kernel"

echo "[unoq-kernel] building kernel (ref=$KERNEL_REF, cpus=$CPUS, memory=$MEMORY)"
# system/kernel (build.sh + tactus-5ghz.config) is shared with the GitHub workflow.
container run --rm \
    --cpus "$CPUS" --memory "$MEMORY" \
    --volume "$VOLUME:/work" \
    --volume "$REPO_ROOT/system/kernel:/kernel:ro" \
    --volume "$OUT_DIR:/out" \
    --env WORK=/work \
    --env OUT=/out \
    --env KERNEL_REPO="$KERNEL_REPO" \
    --env KERNEL_REF="$KERNEL_REF" \
    --env DEB_IMAGES_REPO="$DEB_IMAGES_REPO" \
    --env DEB_IMAGES_REF="$DEB_IMAGES_REF" \
    "$IMAGE" bash /kernel/build.sh

echo "[unoq-kernel] done. Outputs in $OUT_DIR:"
ls -lh "$OUT_DIR"
(cd "$OUT_DIR" && shasum -a 256 ./*.deb)
