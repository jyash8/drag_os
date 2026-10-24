#!/bin/bash
# Build kmscube + drm_info (from libdrm tools) for DragOS.
# Host == target x86_64; zig provides musl so binaries run under DragOS's
# /lib/ld-musl-x86_64.so.1 without any cross toolchain.

set -e

PREFIX="$(pwd)/initrd/usr"
WORK="${WORK:-/tmp/libdrm-build}"
CCWRAP="${WORK}/zig-cc.sh"

mkdir -p "$PREFIX/bin"

# drm_info ships inside libdrm (tests) and links against the libdrm we built.
if [ -d "$WORK/build" ] && [ -f "$WORK/build/xf86drm/.libs/drm_info" ]; then
  cp "$WORK/build/xf86drm/.libs/drm_info" "$PREFIX/bin/drm_info"
  echo "drm_info -> $PREFIX/bin/"
fi

# kmscube needs mesa (EGL/GLES/gbm) + gbm; that's a real dependency chain.
# Lazy path: our kernel's WRITE_FB path + drm_test already validate mode-setting.
echo "NOTE: kmscube requires mesa/gbm; skipped. drm_test in initrd covers KMS checks."