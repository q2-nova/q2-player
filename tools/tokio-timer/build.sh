#!/bin/sh
# Build timer-wake-upstream and timer-wake-patched.
#   tools/tokio-timer/build.sh host OUT_DIR   for this machine
#   tools/tokio-timer/build.sh q2 OUT_DIR     for the Q2 target, as
#       tools/build-target.py builds q2nd: pinned nightly, build-std, the
#       target JSON and the GNU SDK. Set Q2_SDK, Q2_TOOLCHAIN_REPO and
#       Q2_OS_REPO, for example to the sdk/mipsel-buildroot-linux-gnu_sdk-buildroot,
#       q2-toolchain and q2-os directories of tools/fetch-build-tools.py --out.
# OUT_DIR receives the two binaries (stripped for the Q2).
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
KIND=${1:?host or q2}
OUT=${2:?output directory required}
mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)
export CARGO_BUILD_JOBS=2
case "$KIND" in
host)
    export CARGO_TARGET_DIR=$ROOT/build/tokio-timer/target
    (cd "$HERE" && cargo build --release --offline --locked)
    (cd "$HERE/patched" && cargo build --release --offline --locked)
    cp "$CARGO_TARGET_DIR/release/timer-wake-upstream" "$CARGO_TARGET_DIR/release/timer-wake-patched" "$OUT/"
    ;;
q2)
    SDK=${Q2_SDK:?set Q2_SDK to the GNU SDK directory}
    TOOLCHAIN_REPO=${Q2_TOOLCHAIN_REPO:?set Q2_TOOLCHAIN_REPO to a q2-toolchain checkout}
    OS_REPO=${Q2_OS_REPO:?set Q2_OS_REPO to a q2-os checkout}
    TOOLCHAIN=$(python3 -c "import json;print(json.load(open('$OS_REPO/os/sources.json'))['rust_toolchain'])")
    TARGET=$OUT/mipsel-q2-os-linux-gnu.json
    python3 -c "import json;print(json.dumps(json.load(open('$TOOLCHAIN_REPO/q2/toolchain/mipsel-q2-linux-gnu.json')),indent=2))" > "$TARGET"
    export Q2_OS_CC=$SDK/bin/mipsel-buildroot-linux-gnu-gcc
    export CC_mipsel_q2_os_linux_gnu=$Q2_OS_CC
    export AR_mipsel_q2_os_linux_gnu=$SDK/bin/mipsel-buildroot-linux-gnu-ar
    export CARGO_TARGET_MIPSEL_Q2_OS_LINUX_GNU_LINKER=$OS_REPO/os/rust-link.py
    export CARGO_TARGET_DIR=$ROOT/build/tokio-timer/target-q2
    for dir in "$HERE" "$HERE/patched"; do
        (cd "$dir" && cargo +"$TOOLCHAIN" build --release --offline --locked -Zbuild-std=std,panic_abort -Zjson-target-spec --target "$TARGET")
    done
    for name in upstream patched; do
        "$SDK/bin/mipsel-buildroot-linux-gnu-strip" -o "$OUT/timer-wake-$name" "$CARGO_TARGET_DIR/mipsel-q2-os-linux-gnu/release/timer-wake-$name"
    done
    ;;
*) echo "usage: $0 host|q2 OUT_DIR" >&2; exit 2 ;;
esac
ls -l "$OUT"/timer-wake-*
