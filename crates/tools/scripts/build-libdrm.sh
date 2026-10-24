#!/bin/bash
# Build libdrm for DragOS.
# No cross-compilation needed (host == target x86_64); we only avoid host
# glibc by compiling with zig's bundled musl. Produces musl-linked libdrm.

set -e

LIBDRM_VERSION="${LIBDRM_VERSION:-2.4.120}"
PREFIX="$(pwd)/initrd/usr"
WORK="${WORK:-/tmp/libdrm-build}"
CCWRAP="${WORK}/zig-cc.sh"

mkdir -p "$PREFIX/lib" "$WORK"

# zig cc must default to musl target (host ld would link glibc otherwise)
cat > "$CCWRAP" <<'EOF'
#!/bin/bash
exec zig cc -target x86_64-linux-musl "$@"
EOF
chmod +x "$CCWRAP"

if [ ! -d "$WORK/libdrm-$LIBDRM_VERSION" ]; then
  curl -L "https://dri.freedesktop.org/libdrm/libdrm-$LIBDRM_VERSION.tar.xz" | tar -xJ -C "$WORK"
fi

build() { # $1 = extra meson args
  meson setup "$WORK/build" "$WORK/libdrm-$LIBDRM_VERSION" \
    --prefix="$PREFIX" \
    --libdir=lib \
    --cross-file - <<EOF >/dev/null 2>&1
[binaries]
c = '$CCWRAP'
cpp = '$CCWRAP'
ar = 'zig ar'
ranlib = 'zig ranlib'
strip = 'zig strip'
[host_machine]
system = 'linux'
cpu_family = 'x86_64'
cpu = 'x86_64'
endian = 'little'
EOF
  ninja -C "$WORK/build"
  # install with a plain file copy (DESTDIR weirdness avoided)
  find "$WORK/build" -name 'libdrm*.so*' -exec cp -av {} "$PREFIX/lib/" \;
  echo "libdrm built -> $PREFIX/lib/"
}

build "$@"