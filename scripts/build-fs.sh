#!/bin/bash
# Build minimal filesystem image for DragOS QEMU
# This script sets up a clean, structured FHS initrd with core execution environment
set -e

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

echo "Building minimal filesystem image..."

STAGING="/tmp/dragos-fs"
rm -rf "$STAGING"
mkdir -p "$STAGING"

# Standard FHS directory structure
mkdir -p "$STAGING"/bin
mkdir -p "$STAGING"/sbin
mkdir -p "$STAGING"/etc
mkdir -p "$STAGING"/lib/firmware/amdgpu
mkdir -p "$STAGING"/lib/firmware/i915
mkdir -p "$STAGING"/usr/bin
mkdir -p "$STAGING"/usr/lib
mkdir -p "$STAGING"/dev
mkdir -p "$STAGING"/dev/dri
mkdir -p "$STAGING"/dev/input
mkdir -p "$STAGING"/dev/snd
mkdir -p "$STAGING"/dev/shm
mkdir -p "$STAGING"/dev/pts
mkdir -p "$STAGING"/proc
mkdir -p "$STAGING"/sys
mkdir -p "$STAGING"/tmp
chmod 1777 "$STAGING"/tmp
mkdir -p "$STAGING"/var
mkdir -p "$STAGING"/root

# Standard configuration files in /etc
printf "/lib:/usr/lib" > "$STAGING"/etc/ld-musl-x86_64.path
cat << 'EOF' > "$STAGING"/etc/passwd
root:x:0:0:root:/root:/bin/sh
user:x:1000:1000:user:/root:/bin/sh
sshd:x:74:74:Privilege-separated SSH:/var/empty:/bin/sh
nobody:x:65534:65534:nobody:/nonexistent:/bin/false
EOF

cat << 'EOF' > "$STAGING"/etc/group
root:x:0:
user:x:1000:
sshd:x:74:
nobody:x:65534:
nogroup:x:65534:
EOF

cat << 'EOF' > "$STAGING"/etc/hosts
127.0.0.1   localhost dragos
::1         localhost ip6-localhost ip6-loopback
EOF

cat << 'EOF' > "$STAGING"/etc/resolv.conf
nameserver 10.0.2.3
nameserver 1.1.1.1
nameserver 8.8.8.8
EOF

cat << 'EOF' > "$STAGING"/etc/drirc
<driconf>
    <device>
        <application name="Default">
        </application>
    </device>
</driconf>
EOF
cp -p "$STAGING"/etc/drirc "$STAGING"/root/.drirc

# Musl dynamic linker and standard libraries
mkdir -p "$STAGING"/lib64

# Glibc dynamic linker and libraries (for glibc binaries like mywima)
for lib in ld-linux-x86-64.so.2 libc.so.6 libm.so.6 libgcc_s.so.1 libgbm.so.1 libseat.so.1 libudev.so.1 \
           libinput.so.10 libpixman-1.so.0 libxkbcommon.so.0 libdrm.so.2 libevdev.so.2 libexpat.so.1 \
           libffi.so.8 libglib-2.0.so.0 libgobject-2.0.so.0 libgudev-1.0.so.0 liblua5.4.so.5.4 \
           libmtdev.so.1 libpcre2-8.so.0 libsystemd.so.0 libwacom.so.9 libcap.so.2 libz.so.1 libzstd.so.1 \
           libgallium-*.so libLLVM.so.22.1 libSPIRV-Tools.so libsensors.so.5 \
           libxcb.so.1 libxcb-randr.so.0 libxcb-sync.so.1 libxcb-present.so.0 libxshmfence.so.1 \
           libxcb-xfixes.so.0 libxcb-dri3.so.0 libelf.so.1 libdrm_amdgpu.so.1 libdrm_intel.so.1 \
           libX11-xcb.so.1 libstdc++.so.6 libedit.so.0 libxml2.so.16 libXau.so.6 libXdmcp.so.6 \
           libpciaccess.so.0 libncursesw.so.6 libicuuc.so.78 libicudata.so.78 \
           libEGL.so.1 libEGL_mesa.so.0 libGLESv2.so.2 libGLdispatch.so.0 libGL.so.1 libGLX.so.0 libGLX_mesa.so.0 \
            libX11.so.6 libxcb-shm.so.0 libwayland-client.so.0 libwayland-server.so.0; do
    src=$(find /usr/lib /lib64 /lib /usr/lib/gcc -name "$lib" 2>/dev/null | head -n 1)
    if [ -n "$src" ] && [ -f "$src" ]; then
        cp -L "$src" "$STAGING"/lib/
        [ "$lib" = "ld-linux-x86-64.so.2" ] && cp -L "$src" "$STAGING"/lib64/
    fi
done
ldconfig -r "$STAGING" 2>/dev/null || echo "[build-fs] WARNING: ldconfig failed, glibc binaries need LD_LIBRARY_PATH"

# Stage binary and all of its runtime shared libraries via ldd
stage_bin_with_libs() {
    local bin="$1"
    [ -f "$bin" ] || return 0
    local bname=$(basename "$bin")
    cp -L "$bin" "$STAGING/usr/bin/$bname"
    cp -L "$bin" "$STAGING/bin/$bname"
    chmod +x "$STAGING/usr/bin/$bname" "$STAGING/bin/$bname"
    for lib in $(ldd "$bin" 2>/dev/null | grep -o '/[^ ]*' || true); do
        if [ -f "$lib" ]; then
            local lname=$(basename "$lib")
            cp -L "$lib" "$STAGING/usr/lib/$lname" 2>/dev/null || true
            cp -L "$lib" "$STAGING/lib/$lname" 2>/dev/null || true
            if echo "$lib" | grep -q "ld-linux"; then
                cp -L "$lib" "$STAGING/lib64/$lname" 2>/dev/null || true
            fi
        fi
    done
}

# curl/wget/ssh/sshd staging with full runtime library dependencies
for bin in curl wget ssh sshd ssh-keygen mpv; do
    if [ -f "/usr/bin/$bin" ]; then
        stage_bin_with_libs "/usr/bin/$bin"
    elif [ -f "/usr/sbin/$bin" ]; then
        stage_bin_with_libs "/usr/sbin/$bin"
    fi
done

# D-Bus daemon and tools, needed for the session bus that xdg-desktop-portal
# runs on. Staged with stage_bin_with_libs so libdbus-1, libexpat, libaudit,
# libcap-ng, libsystemd and the dynamic linker come along.
for bin in dbus-daemon dbus-send; do
    if [ -f "/usr/bin/$bin" ]; then
        stage_bin_with_libs "/usr/bin/$bin"
    elif [ -f "/usr/sbin/$bin" ]; then
        stage_bin_with_libs "/usr/sbin/$bin"
    fi
done

# D-Bus session bus configuration file — dbus-daemon refuses to start without it.
mkdir -p "$STAGING/usr/share/dbus-1"
cp -p /usr/share/dbus-1/session.conf "$STAGING/usr/share/dbus-1/session.conf" 2>/dev/null || true

# Standard GNU coreutils (the basic file, shell and text manipulation utilities
# expected on any Linux system). These are dynamically linked against glibc/musl
# and are staged with their runtime library dependencies.
for bin in \
    cat chmod chown cp -p cut date dd df dir dirname du echo env false \
    find fold head hostid id install join link ln logname ls mkdir \
    mkfifo mknod mv nice nl nohost nohup od paste pathchk pinky pr \
    printenv printf pwd readlink rm rmdir seq sleep sort split stat \
    stty sum sync tac tail tee test touch tr true uname unlink uptime \
    users wc who whoami yes; do
    if [ -f "/usr/bin/$bin" ]; then
        stage_bin_with_libs "/usr/bin/$bin"
    fi
done

# zellij: a terminal multiplexer, used here as the end-to-end check on the
# terminal layer. It is the strictest client available -- it immediately puts
# the tty into raw mode, queries TIOCGWINSZ to size itself, and draws with
# absolute cursor positioning and SGR colour. It therefore fails if any of
# those are wrong, where a shell would hide the same defect.
#
# Staged with the same ldd closure as the other glibc binaries above, so it
# runs against the guest's glibc rather than the host's.
if [ -f "/usr/bin/zellij" ]; then
    stage_bin_with_libs "/usr/bin/zellij"
fi

if [ -f "$STAGING/usr/bin/sshd" ] || [ -f "$STAGING/bin/sshd" ]; then
    mkdir -p "$STAGING/usr/sbin" "$STAGING/sbin"
    SSHD_SRC="$STAGING/usr/bin/sshd"
    [ -f "$SSHD_SRC" ] || SSHD_SRC="$STAGING/bin/sshd"
    cp -L "$SSHD_SRC" "$STAGING/usr/sbin/sshd"
    cp -L "$SSHD_SRC" "$STAGING/sbin/sshd"
    chmod +x "$STAGING/usr/sbin/sshd" "$STAGING/sbin/sshd"
fi

for helper in sshd-session sshd-auth; do
    if [ -f "/usr/lib/ssh/$helper" ]; then
        mkdir -p "$STAGING/usr/lib/ssh"
        cp -L "/usr/lib/ssh/$helper" "$STAGING/usr/lib/ssh/$helper"
        chmod +x "$STAGING/usr/lib/ssh/$helper"
        stage_bin_with_libs "/usr/lib/ssh/$helper"
    fi
done

# Handle versioned libcrypto/libssl wildcards and ca-certs
for pat in libcrypto.so.* libssl.so.* libldap*.so* liblber*.so* libsasl2.so*; do
    src=$(find /usr/lib /lib64 /lib -name "$pat" 2>/dev/null | head -n 1)
    [ -n "$src" ] && [ -f "$src" ] && cp -L "$src" "$STAGING"/lib/ 2>/dev/null || true
    [ -n "$src" ] && [ -f "$src" ] && cp -L "$src" "$STAGING"/usr/lib/ 2>/dev/null || true
done
for pat in libSDL3.so* libSDL2*.so*; do
    for src in $(find /usr/lib /lib64 /lib -name "$pat" 2>/dev/null || true); do
        [ -f "$src" ] || continue
        bname=$(basename "$src")
        cp -L "$src" "$STAGING/lib/$bname" 2>/dev/null || true
        cp -L "$src" "$STAGING/usr/lib/$bname" 2>/dev/null || true
    done
done
if [ -f "$ROOT/initrd/libvulkan_virtio.so" ]; then
    cp -p "$ROOT/initrd/libvulkan_virtio.so" "$STAGING/usr/lib/" 2>/dev/null || true
    cp -p "$ROOT/initrd/libvulkan_virtio.so" "$STAGING/lib/" 2>/dev/null || true
fi
if [ -f "$ROOT/initrd/virtio_icd.json" ]; then
    mkdir -p "$STAGING/usr/share/vulkan/icd.d"
    cp -p "$ROOT/initrd/virtio_icd.json" "$STAGING/usr/share/vulkan/icd.d/" 2>/dev/null || true
fi
if [ -f "$STAGING/usr/lib/libvulkan_virtio.so" ]; then
    for lib in $(ldd "$STAGING/usr/lib/libvulkan_virtio.so" 2>/dev/null | grep -o '/[^ ]*' || true); do
        [ -f "$lib" ] || continue
        lname=$(basename "$lib")
        [ -f "$STAGING/usr/lib/$lname" ] && [ -f "$STAGING/lib/$lname" ] && continue
        cp -L "$lib" "$STAGING/usr/lib/$lname" 2>/dev/null || true
        cp -L "$lib" "$STAGING/lib/$lname" 2>/dev/null || true
    done
fi
mkdir -p "$STAGING/etc/ssl/certs" && cp -p /etc/ssl/certs/ca-certificates.crt "$STAGING/etc/ssl/certs/" 2>/dev/null || true
mkdir -p "$STAGING/usr/lib/locale" && cp -r /usr/lib/locale/C.utf8 "$STAGING/usr/lib/locale/" 2>/dev/null || true
cp -p /usr/lib/locale/locale-archive "$STAGING/usr/lib/locale/" 2>/dev/null || true

# SSH Configuration, host keys, and privilege separation
mkdir -p "$STAGING/etc/ssh" "$STAGING/var/empty" "$STAGING/run/sshd" "$STAGING/usr/share/empty.sshd"
chmod 755 "$STAGING/var/empty" "$STAGING/run/sshd" "$STAGING/usr/share/empty.sshd" 2>/dev/null || true
if [ ! -f "$STAGING/etc/ssh/ssh_host_ed25519_key" ]; then
    ssh-keygen -t ed25519 -f "$STAGING/etc/ssh/ssh_host_ed25519_key" -N "" -q 2>/dev/null || true
    ssh-keygen -t rsa -b 2048 -f "$STAGING/etc/ssh/ssh_host_rsa_key" -N "" -q 2>/dev/null || true
    ssh-keygen -t ecdsa -f "$STAGING/etc/ssh/ssh_host_ecdsa_key" -N "" -q 2>/dev/null || true
fi
chmod 600 "$STAGING"/etc/ssh/ssh_host_*_key 2>/dev/null || true
chmod 644 "$STAGING"/etc/ssh/ssh_host_*_key.pub 2>/dev/null || true

# Developer dev keypair for host SSH client tooling (`cargo run -p tools -- ssh`)
DEV_KEY="$ROOT/.dragos_dev_key"
if [ ! -f "$DEV_KEY" ]; then
    ssh-keygen -t ed25519 -f "$DEV_KEY" -N "" -C "dragos-dev" -q 2>/dev/null || true
fi
mkdir -p "$STAGING/root/.ssh"
chmod 700 "$STAGING/root/.ssh" 2>/dev/null || true
if [ -f "$DEV_KEY.pub" ]; then
    cat "$DEV_KEY.pub" > "$STAGING/root/.ssh/authorized_keys"
    chmod 600 "$STAGING/root/.ssh/authorized_keys" 2>/dev/null || true
fi

cat << 'EOF' > "$STAGING/etc/ssh/sshd_config"
Port 22
ListenAddress 0.0.0.0
HostKey /etc/ssh/ssh_host_ed25519_key
HostKey /etc/ssh/ssh_host_rsa_key
HostKey /etc/ssh/ssh_host_ecdsa_key
PermitRootLogin yes
PasswordAuthentication yes
PermitEmptyPasswords yes
UsePAM no
StrictModes no
PidFile /run/sshd.pid
Subsystem sftp internal-sftp
LogLevel INFO
EOF

cat << 'EOF' > "$STAGING/etc/ssh/ssh_config"
Host *
    StrictHostKeyChecking no
    UserKnownHostsFile /dev/null
EOF

# Ensure sshd user exists in /etc/passwd and /etc/group
if [ -f "$STAGING/etc/passwd" ]; then
    grep -q "sshd:" "$STAGING/etc/passwd" || echo "sshd:x:74:74:Privilege-separated SSH:/var/empty:/bin/sh" >> "$STAGING/etc/passwd"
fi
if [ -f "$STAGING/etc/group" ]; then
    grep -q "sshd:" "$STAGING/etc/group" || echo "sshd:x:74:" >> "$STAGING/etc/group"
fi

# Terminal binaries and deps (foot/weston) + fallback minimal wayland client
for bin in foot weston-terminal footclient; do
  [ -f /usr/bin/$bin ] && cp -L /usr/bin/$bin "$STAGING"/usr/bin/
done
# Also stage kitty if available (optional fallback, not required for autostart)
[ -f /usr/bin/kitty ] && cp -L /usr/bin/kitty "$STAGING"/usr/bin/ 2>/dev/null || true
for lib in libwayland-client.so.0 libwayland-cursor.so.0 libfontconfig.so.1 libfreetype.so.6; do
  src=$(find /usr/lib -name "$lib" 2>/dev/null | head -n1); [ -n "$src" ] && cp -L "$src" "$STAGING"/usr/lib/ && cp -L "$src" "$STAGING"/lib/
done
mkdir -p "$STAGING"/usr/share/foot "$STAGING"/etc/fonts
cp -r /usr/share/foot "$STAGING"/usr/share/ 2>/dev/null || true
cp -r /etc/fonts "$STAGING"/etc/ 2>/dev/null || true
RUST_FOOT=/home/jyash8/Projects/mywima/target/release/foot
if [ -f "$RUST_FOOT" ]; then
  echo "[build-fs] Staging Rust foot client (with libs)..."
  stage_bin_with_libs "$RUST_FOOT"
  chmod +x "$STAGING/usr/bin/foot" "$STAGING/bin/foot"
fi
# If no foot/weston was staged, build minimal Wayland SHM client as foot
if [ ! -f "$STAGING/usr/bin/foot" ] && [ ! -f "$STAGING/usr/bin/weston-terminal" ]; then
  echo "[build-fs] No host terminal found, building minimal Wayland SHM client as foot..."
  TMPDIR=$(mktemp -d)
  cat > "$TMPDIR/minimal_foot.c" <<'MINFOOT_EOF'
#define _POSIX_C_SOURCE 200809L
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <time.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <fcntl.h>
#include <wayland-client.h>
#include "xdg-shell-client-protocol.h"
static struct wl_compositor *compositor = NULL;
static struct wl_shm *shm = NULL;
static struct xdg_wm_base *wm_base = NULL;
static struct wl_surface *surface = NULL;
static struct xdg_surface *xdg_surface = NULL;
static struct xdg_toplevel *toplevel = NULL;
static int configured = 0;
static int width = 800, height = 600;
static void randname(char *buf){struct timespec ts; clock_gettime(CLOCK_REALTIME,&ts); long r=ts.tv_nsec; for(int i=0;i<6;i++){buf[i]='A'+(r&15)+(r&16)*2; r>>=5;}}
static int create_shm_file(off_t size){
    long sc_memfd=319; int fd=syscall(sc_memfd,"wl_foot_shm",1);
    if(fd<0){char name[]="/tmp/wl_shm-XXXXXX"; fd=mkstemp(name); if(fd>=0) unlink(name);}
    if(fd>=0){if(ftruncate(fd,size)<0){close(fd);return -1;}}
    return fd;
}
static void draw_and_commit(){
    int stride=width*4; int size=stride*height;
    int fd=create_shm_file(size); if(fd<0){fprintf(stderr,"foot: shm alloc failed\n"); return;}
    void *data=mmap(NULL,size,PROT_READ|PROT_WRITE,MAP_SHARED,fd,0); if(data==MAP_FAILED){close(fd);return;}
    uint32_t *pixels=data;
    for(int y=0;y<height;y++) for(int x=0;x<width;x++){uint32_t col=0xFFFFFFFF; if(x<4||x>=width-4||y<4||y>=height-4) col=0xFF333333; else if(y<30) col=0xFF444444; pixels[y*width+x]=col;}
    for(int y=50;y<150;y++) for(int x=50;x<250;x++) pixels[y*width+x]=0xFF55FF55;
    for(int y=200;y<300;y++) for(int x=100;x<300;x++) pixels[y*width+x]=0xFF5555FF;
    struct wl_shm_pool *pool=wl_shm_create_pool(shm,fd,size);
    struct wl_buffer *buffer=wl_shm_pool_create_buffer(pool,0,width,height,stride,WL_SHM_FORMAT_XRGB8888);
    wl_surface_attach(surface,buffer,0,0); wl_surface_commit(surface);
    wl_shm_pool_destroy(pool); close(fd); munmap(data,size);
}
static void xdg_surface_handle_configure(void *d, struct xdg_surface *s, uint32_t serial){xdg_surface_ack_configure(s,serial); if(!configured){configured=1; draw_and_commit();}}
static const struct xdg_surface_listener xdg_surface_listener={xdg_surface_handle_configure};
static void xdg_toplevel_handle_configure(void *d, struct xdg_toplevel *t,int32_t w,int32_t h,struct wl_array *s){}
static void xdg_toplevel_handle_close(void *d, struct xdg_toplevel *t){exit(0);}
static const struct xdg_toplevel_listener toplevel_listener={xdg_toplevel_handle_configure,xdg_toplevel_handle_close};
static void wm_base_ping(void *d, struct xdg_wm_base *b,uint32_t serial){xdg_wm_base_pong(b,serial);}
static const struct xdg_wm_base_listener wm_base_listener={wm_base_ping};
static void registry_global(void *d, struct wl_registry *reg,uint32_t id,const char *iface,uint32_t ver){if(strcmp(iface,wl_compositor_interface.name)==0) compositor=wl_registry_bind(reg,id,&wl_compositor_interface,4); else if(strcmp(iface,wl_shm_interface.name)==0) shm=wl_registry_bind(reg,id,&wl_shm_interface,1); else if(strcmp(iface,xdg_wm_base_interface.name)==0) wm_base=wl_registry_bind(reg,id,&xdg_wm_base_interface,1);}
static void registry_remove(void *d, struct wl_registry *r,uint32_t id){}
static const struct wl_registry_listener registry_listener={registry_global,registry_remove};
int main(int argc,char **argv){
    struct wl_display *display=wl_display_connect(NULL); if(!display){fprintf(stderr,"no wayland display\n"); return 1;}
    struct wl_registry *registry=wl_display_get_registry(display); wl_registry_add_listener(registry,&registry_listener,NULL); wl_display_roundtrip(display);
    if(!compositor||!shm||!wm_base){fprintf(stderr,"missing globals\n"); return 1;}
    xdg_wm_base_add_listener(wm_base,&wm_base_listener,NULL);
    surface=wl_compositor_create_surface(compositor); xdg_surface=xdg_wm_base_get_xdg_surface(wm_base,surface);
    xdg_surface_add_listener(xdg_surface,&xdg_surface_listener,NULL); toplevel=xdg_surface_get_toplevel(xdg_surface);
    xdg_toplevel_add_listener(toplevel,&toplevel_listener,NULL); xdg_toplevel_set_title(toplevel,"foot"); xdg_toplevel_set_app_id(toplevel,"foot");
    wl_surface_commit(surface); while(wl_display_dispatch(display)!=-1){}
    return 0;
}
MINFOOT_EOF
  wayland-scanner client-header /usr/share/wayland-protocols/stable/xdg-shell/xdg-shell.xml "$TMPDIR/xdg-shell-client-protocol.h" 2>/dev/null
  wayland-scanner private-code /usr/share/wayland-protocols/stable/xdg-shell/xdg-shell.xml "$TMPDIR/xdg-shell-protocol.c" 2>/dev/null
  gcc -o "$TMPDIR/foot" "$TMPDIR/minimal_foot.c" "$TMPDIR/xdg-shell-protocol.c" $(pkg-config --cflags --libs wayland-client 2>/dev/null) -lrt 2>/dev/null && cp -p "$TMPDIR/foot" "$STAGING/usr/bin/foot" && cp -p "$TMPDIR/foot" "$STAGING/bin/foot" && chmod +x "$STAGING/usr/bin/foot" "$STAGING/bin/foot" && echo "[build-fs] Minimal foot client staged"
  rm -rf "$TMPDIR"
fi
# Ensure foot also at /bin/foot for PATH without /usr/bin
[ -f "$STAGING/usr/bin/foot" ] && cp -p "$STAGING/usr/bin/foot" "$STAGING/bin/foot" 2>/dev/null || true

# Copy GBM and DRI driver modules
mkdir -p "$STAGING"/usr/lib/gbm "$STAGING"/usr/lib/dri
if [ -f /usr/lib/gbm/dri_gbm.so ]; then
    cp -L /usr/lib/gbm/dri_gbm.so "$STAGING"/usr/lib/gbm/
    cp -L /usr/lib/gbm/dri_gbm.so "$STAGING"/usr/lib/gbm/virtio_gpu_gbm.so
fi
for drv in libdril_dri.so kms_swrast_dri.so swrast_dri.so virtio_gpu_dri.so vkms_dri.so; do
    [ -f "/usr/lib/dri/$drv" ] && cp -L "/usr/lib/dri/$drv" "$STAGING"/usr/lib/dri/
done

# GL probe apps (mesa-demos) + their libs missing from the base list.
# es2gears_wayland is the Wayland-native GLES2 proof app (no X server needed).
for bin in es2gears_wayland es2_info; do
    [ -f "/usr/bin/$bin" ] && cp -L "/usr/bin/$bin" "$STAGING/usr/bin/"
done
for lib in libdecor-0.so.0 libwayland-egl.so.1; do
    src=$(find /usr/lib /lib64 /lib -name "$lib" 2>/dev/null | head -n 1)
    if [ -n "$src" ] && [ -f "$src" ]; then
        cp -L "$src" "$STAGING"/lib/
        cp -L "$src" "$STAGING"/usr/lib/
    fi
done

# Host C++ standard library
[ -f /usr/lib/libstdc++.so.6 ] && cp -L /usr/lib/libstdc++.so.6 "$STAGING"/lib/libstdc++.so.6 && cp -L /usr/lib/libstdc++.so.6 "$STAGING"/usr/lib/libstdc++.so.6

# Musl dynamic linker and standard libraries
MUSL_LIB="/home/jyash8/Projects/drag_os/x86_64-linux-musl-native/lib"
[ -f /home/jyash8/Projects/drag_os/initrd/lib/ld-musl-x86_64.so.1 ] && cp -p /home/jyash8/Projects/drag_os/initrd/lib/ld-musl-x86_64.so.1 "$STAGING"/lib/
if [ -d "$MUSL_LIB" ]; then
    [ -f "$MUSL_LIB/libc.so" ] && cp -p "$MUSL_LIB/libc.so" "$STAGING"/lib/
    [ -f "$MUSL_LIB/libc.so" ] && cp -p "$MUSL_LIB/libc.so" "$STAGING"/lib/libm.so
    [ -f "$MUSL_LIB/libc.so" ] && cp -p "$MUSL_LIB/libc.so" "$STAGING"/lib/libpthread.so
    [ -f "$MUSL_LIB/libc.so" ] && cp -p "$MUSL_LIB/libc.so" "$STAGING"/lib/librt.so
    [ -f "$MUSL_LIB/libc.so" ] && cp -p "$MUSL_LIB/libc.so" "$STAGING"/lib/libdl.so
fi

# Wallpaper staging (mywima expects ~/Pictures/Wallpapers/random_wallpaper.png)
mkdir -p "$STAGING"/root/Pictures/Wallpapers
if [ -f /home/jyash8/Projects/mywima/src/assets/default_wallpaper.jpg ]; then
    cp -p /home/jyash8/Projects/mywima/src/assets/default_wallpaper.jpg "$STAGING"/root/Pictures/Wallpapers/random_wallpaper.jpg
    cp -p /home/jyash8/Projects/mywima/src/assets/default_wallpaper.jpg "$STAGING"/root/Pictures/Wallpapers/random_wallpaper.png 2>/dev/null || true
    if command -v convert >/dev/null 2>&1; then
        convert "$STAGING/root/Pictures/Wallpapers/random_wallpaper.jpg" -resize 1280x800! "$STAGING/root/Pictures/Wallpapers/random_wallpaper.jpg" 2>/dev/null || true
        convert "$STAGING/root/Pictures/Wallpapers/random_wallpaper.png" -resize 1280x800! "$STAGING/root/Pictures/Wallpapers/random_wallpaper.png" 2>/dev/null || true
    elif command -v ffmpeg >/dev/null 2>&1; then
        ffmpeg -y -i "$STAGING/root/Pictures/Wallpapers/random_wallpaper.jpg" -vf scale=1280:800 -q:v 2 "$STAGING/root/Pictures/Wallpapers/random_wallpaper.jpg" 2>/dev/null || true
    fi
    # fallback: if convert/ffmpeg not available, mywima will resize at runtime (Triangle)
fi
# Patch staged mywima config to use .jpg (JPEG) so image::open succeeds without conversion
if [ -f "$STAGING/root/.config/mywima/config.toml" ]; then
    sed -i 's|random_wallpaper.png|random_wallpaper.jpg|g' "$STAGING/root/.config/mywima/config.toml" 2>/dev/null || true
    sed -i 's|random_wallpaper.png|random_wallpaper.jpg|g' "$STAGING/root/.config/mwm/config.toml" 2>/dev/null || true
fi

# mywima compositor binary
# mywima compositor binary and configuration
mkdir -p "$STAGING"/root/.config/mywima
mkdir -p "$STAGING"/root/.config/mwm
if [ -f /home/jyash8/Projects/mywima/config.toml.example ]; then
    cp -p /home/jyash8/Projects/mywima/config.toml.example "$STAGING"/root/.config/mywima/config.toml
    cp -p /home/jyash8/Projects/mywima/config.toml.example "$STAGING"/root/.config/mwm/config.toml
fi

# Build the compositor before staging it.
#
# This script used to just `cp -p target/release/mywima`, which silently shipped
# whatever binary happened to be lying in the mywima workspace. That produced a
# real misdiagnosis: mywima's source was newer than its binary (11:39 vs 11:25),
# so the guest ran an older compositor than the one on disk, and rebuilding
# produced a different hash (docs/virgl-vs-linux.md §20). With the compositor in
# the critical path of every graphics comparison, "stale binary" is a way to
# confidently diagnose a kernel bug that is really an old client.
#
# On failure we warn loudly and fall back to the existing binary rather than
# aborting the whole filesystem build -- a compositor build failure should not
# cost us the kernel, initrd and test clients.
MYWIMA_DIR=/home/jyash8/Projects/mywima
if [ -f "$MYWIMA_DIR/Cargo.toml" ]; then
    echo "[build-fs] Building mywima compositor..."
    if ! (cd "$MYWIMA_DIR" && cargo build --release) >/tmp/dragos-mywima-build.log 2>&1; then
        echo "[WARN] mywima build FAILED -- staging the previous binary if one exists."
        echo "[WARN] This means the guest may run a compositor older than its source."
        echo "[WARN] First 15 lines:"
        head -15 /tmp/dragos-mywima-build.log | sed 's/^/[WARN]   /'
    fi

    # Verify the binary is not older than the source it came from. A failed
    # build leaves the old binary in place, and that is precisely the case worth
    # shouting about.
    MYWIMA_BIN="$MYWIMA_DIR/target/release/mywima"
    if [ -f "$MYWIMA_BIN" ]; then
        NEWEST_SRC=$(find "$MYWIMA_DIR/src" -name '*.rs' -newer "$MYWIMA_BIN" 2>/dev/null | head -1)
        if [ -n "$NEWEST_SRC" ]; then
            echo "[WARN] mywima binary is OLDER than its source:"
            echo "[WARN]   binary: $MYWIMA_BIN"
            echo "[WARN]   source: $NEWEST_SRC"
            echo "[WARN] The guest will run a compositor that does not match the tree."
        fi
    fi
fi

if [ -f /home/jyash8/Projects/mywima/target/release/mywima ]; then
    mkdir -p "$STAGING"/usr/bin
    cp -p /home/jyash8/Projects/mywima/target/release/mywima "$STAGING"/usr/bin/mywima
    chmod +x "$STAGING"/usr/bin/mywima
    stage_bin_with_libs "/home/jyash8/Projects/mywima/target/release/mywima"
elif [ -f /home/jyash8/Projects/mywima/mywima ]; then
    mkdir -p "$STAGING"/usr/bin
    cp -p /home/jyash8/Projects/mywima/mywima "$STAGING"/usr/bin/mywima
    chmod +x "$STAGING"/usr/bin/mywima
    stage_bin_with_libs "/home/jyash8/Projects/mywima/mywima"
elif [ -f /home/jyash8/Projects/drag_os/esp/efi/boot/mywima ]; then
    mkdir -p "$STAGING"/usr/bin
    cp -p /home/jyash8/Projects/drag_os/esp/efi/boot/mywima "$STAGING"/usr/bin/mywima
    chmod +x "$STAGING"/usr/bin/mywima
    stage_bin_with_libs "/home/jyash8/Projects/drag_os/esp/efi/boot/mywima"
fi

# sdl_vulkan_test: SDL2 + Vulkan render test (verifies end-to-end 3D accel:
# instance, device, swapchain, present). Built on host, staged with its guest
# runtime libs (libSDL2, libvulkan, wayland-client, xkbcommon, libEGL).
if [ -f "$ROOT/initrd/sdl_vulkan_test.c" ]; then
    SDL_CFLAGS_LIBS=$(sdl2-config --cflags --libs 2>/dev/null || echo "-lSDL2")
    SDL_TEST_BIN="/tmp/sdl_vulkan_test"
    gcc -O2 -o "$SDL_TEST_BIN" "$ROOT/initrd/sdl_vulkan_test.c" \
        $SDL_CFLAGS_LIBS -lvulkan -lm \
        || gcc -O2 -o "$SDL_TEST_BIN" "$ROOT/initrd/sdl_vulkan_test.c" -lSDL2 -lvulkan -lm
    stage_bin_with_libs "$SDL_TEST_BIN"
fi

if [ -f "$ROOT/initrd/pure_vulkan_test.c" ]; then
    PVK_TEST_BIN="/tmp/pure_vulkan_test"
    gcc -O2 -o "$PVK_TEST_BIN" "$ROOT/initrd/pure_vulkan_test.c" -lvulkan \
        || gcc -O2 -o "$PVK_TEST_BIN" "$ROOT/initrd/pure_vulkan_test.c" -lvulkan
    stage_bin_with_libs "$PVK_TEST_BIN"
fi

if [ -f /usr/bin/seatd ]; then
    mkdir -p "$STAGING"/usr/bin
    cp -p /usr/bin/seatd "$STAGING"/usr/bin/seatd
    chmod +x "$STAGING"/usr/bin/seatd
fi

# ALSA utilities (aplay, arecord, amixer, speaker-test)
for alsa_bin in aplay arecord amixer speaker-test; do
    if [ -f /usr/bin/$alsa_bin ]; then
        mkdir -p "$STAGING"/usr/bin
        cp -p /usr/bin/$alsa_bin "$STAGING"/usr/bin/$alsa_bin
        chmod +x "$STAGING"/usr/bin/$alsa_bin
    fi
done

# PipeWire (audio server for xdg-desktop-portal ScreenCast/RemoteDesktop)
for pw_bin in pipewire wireplumber pipewire-pulse pw-cli pw-dump pw-mon pw-cat; do
    if [ -f /usr/bin/$pw_bin ]; then
        stage_bin_with_libs /usr/bin/$pw_bin
    fi
done

# PipeWire libs and modules
if [ -d /usr/lib/pipewire-0.3 ]; then
    mkdir -p "$STAGING"/usr/lib/pipewire-0.3
    cp -r /usr/lib/pipewire-0.3/* "$STAGING"/usr/lib/pipewire-0.3/ 2>/dev/null || true
fi
if [ -d /usr/lib/spa-0.2 ]; then
    mkdir -p "$STAGING"/usr/lib/spa-0.2
    cp -r /usr/lib/spa-0.2/* "$STAGING"/usr/lib/spa-0.2/ 2>/dev/null || true
fi

# PipeWire config
if [ -d /usr/share/pipewire ]; then
    mkdir -p "$STAGING"/usr/share/pipewire
    cp -r /usr/share/pipewire/* "$STAGING"/usr/share/pipewire/ 2>/dev/null || true
fi

# libpipewire and libspa
for pwlib in /usr/lib/libpipewire-0.3.so* /usr/lib/libspa-0.2*.so*; do
    [ -f "$pwlib" ] && cp -L "$pwlib" "$STAGING"/usr/lib/ 2>/dev/null || true
done

# Stage sine wave test file
if [ -f "$ROOT/initrd/sine440.wav" ]; then
    mkdir -p "$STAGING"/usr/share/sounds 2>/dev/null || true
    cp -p "$ROOT/initrd/sine440.wav" "$STAGING"/usr/share/sounds/sine440.wav
fi

# ALSA configuration and library files
if [ -d /usr/share/alsa ]; then
    mkdir -p "$STAGING"/usr/share/alsa
    cp -r /usr/share/alsa/* "$STAGING"/usr/share/alsa/ 2>/dev/null || true
fi
if [ -d /usr/lib/alsa-lib ]; then
    mkdir -p "$STAGING"/usr/lib/alsa-lib
    cp -r /usr/lib/alsa-lib/* "$STAGING"/usr/lib/alsa-lib/ 2>/dev/null || true
fi

# xdg-portal: userspace D-Bus portal service (static musl, no ld.so dependency).
# Owns org.freedesktop.portal.Desktop and implements OpenURI, Inhibit, Settings,
# Trash, Secret.
if [ -d "$ROOT/userspace/xdg-portal" ]; then
  (cd "$ROOT/userspace/xdg-portal" && CARGO_NET_OFFLINE=true cargo build --release --target x86_64-unknown-linux-musl 2>/dev/null) || true
  if [ -f "$ROOT/userspace/xdg-portal/target/x86_64-unknown-linux-musl/release/xdg-portal" ]; then
    cp -p "$ROOT/userspace/xdg-portal/target/x86_64-unknown-linux-musl/release/xdg-portal" "$STAGING"/usr/bin/xdg-portal
    chmod +x "$STAGING"/usr/bin/xdg-portal
    echo "[build-fs] xdg-portal staged in /usr/bin"
  fi
fi

# draginit: Rust PID 1 (static musl, no ld.so dependency). Daemons come from
# /etc/draginit/*.toml so no shell init is needed on the boot path.
if [ -d "$ROOT/userspace/draginit" ]; then
  (cd "$ROOT/userspace/draginit" && CARGO_NET_OFFLINE=true cargo build --release --target x86_64-unknown-linux-musl 2>/dev/null) || true
  if [ -f "$ROOT/userspace/draginit/target/x86_64-unknown-linux-musl/release/draginit" ]; then
    mkdir -p "$STAGING"/usr/bin
    cp -p "$ROOT/userspace/draginit/target/x86_64-unknown-linux-musl/release/draginit" "$STAGING"/usr/bin/draginit
    chmod +x "$STAGING"/usr/bin/draginit
    echo "[build-fs] draginit staged in /usr/bin"
  fi
fi
mkdir -p "$STAGING"/etc/draginit
# Unit order matters: draginit spawns in filename order, and `delay_ms` sleeps
# BEFORE the spawn. The compositor has to be up before anything connects to
# WAYLAND_DISPLAY, so it is numbered first. This was previously 50-mywima,
# which meant foot (20) and socktest (30) ran *before* the compositor created
# its socket and both died with
#   [draginit] foot (pid 4) exited (exit 1)
#   socktest: connect(wayland-0) failed: No such file or directory
# leaving a session with no terminal at all — while the compositor carried on
# running and looked like a graphics fault.
# Clear the unit directory first. The staging tree persists between builds, so
# renaming or renumbering a unit leaves the previous file behind and draginit
# then starts each daemon twice — which is how a stale 50-mywima.toml and a
# new 10-mywima.toml ended up coexisting and racing each other.
rm -f "$STAGING"/etc/draginit/*.toml
mkdir -p "$STAGING"/etc/draginit

# ── Module self-test fixture ───────────────────────────────────────────────
# docs/module-system.md §8.1 phase 2. The kernel parses this at boot to prove
# the ELF validator agrees with a file our own toolchain actually produces,
# rather than with a hand-written byte array. Built with the exact flags §5.1
# specifies for a module, and independently of .cargo/config.toml so the
# kernel's build settings cannot leak in.
# 2.5 KiB, so it costs nothing in the initrd.
FIXTURE_SRC="$ROOT/modules/fixture/lib.rs"
FIXTURE_OBJ="$STAGING/modules/fixture.o"
if [ -f "$FIXTURE_SRC" ]; then
    mkdir -p "$STAGING/modules"
    if rustc --edition 2024 --crate-type lib --target x86_64-unknown-none \
        -C opt-level=2 -C relocation-model=static -C code-model=kernel \
        --emit=obj -o "$FIXTURE_OBJ" "$FIXTURE_SRC" 2>/tmp/dragos-fixture-err.log; then
        echo "[INFO]: module fixture staged at /modules/fixture.o ($(stat -c%s "$FIXTURE_OBJ") B)"
    else
        echo "[WARN]: module fixture build failed; selftest will report SKIPPED"
        cat /tmp/dragos-fixture-err.log | head -5
    fi
else
    echo "[WARN]: $FIXTURE_SRC missing; module selftest will report SKIPPED"
fi

# A module that asks for a symbol the kernel does not export. The loader must
# refuse it, with the name reported and nothing left behind (docs/module-system.md
# §6.2). Without this, "the load is refused" would be an untested claim.
BADFIXTURE_SRC="$ROOT/modules/fixture_unresolvable/lib.rs"
BADFIXTURE_OBJ="$STAGING/modules/unresolvable.o"
if [ -f "$BADFIXTURE_SRC" ]; then
    mkdir -p "$STAGING/modules"
    if rustc --edition 2024 --crate-type lib --target x86_64-unknown-none \
        -C opt-level=2 -C relocation-model=static -C code-model=kernel \
        --emit=obj -o "$BADFIXTURE_OBJ" "$BADFIXTURE_SRC" 2>/tmp/dragos-badfixture-err.log; then
        echo "[INFO]: module fixture staged at /modules/unresolvable.o ($(stat -c%s "$BADFIXTURE_OBJ") B)"
    else
        echo "[WARN]: module unresolvable-fixture build failed; the refusal check will report SKIPPED"
        cat /tmp/dragos-badfixture-err.log | head -5
    fi
else
    echo "[WARN]: $BADFIXTURE_SRC missing; the refusal check will report SKIPPED"
fi

# A module that binds to *another module's* export. It is loaded after the
# good fixture, so it is the test for the second half of docs/module-system.md
# §6.2: resolution looks in the kernel's export table and then in the export
# tables of the modules that are already loaded.
CONSUMER_SRC="$ROOT/modules/fixture_consumer/lib.rs"
CONSUMER_OBJ="$STAGING/modules/consumer.o"
if [ -f "$CONSUMER_SRC" ]; then
    mkdir -p "$STAGING/modules"
    if rustc --edition 2024 --crate-type lib --target x86_64-unknown-none \
        -C opt-level=2 -C relocation-model=static -C code-model=kernel \
        --emit=obj -o "$CONSUMER_OBJ" "$CONSUMER_SRC" 2>/tmp/dragos-consumer-err.log; then
        echo "[INFO]: module fixture staged at /modules/consumer.o ($(stat -c%s "$CONSUMER_OBJ") B)"
    else
        echo "[WARN]: module consumer-fixture build failed; the cross-module check will report SKIPPED"
        cat /tmp/dragos-consumer-err.log | head -5
    fi
else
    echo "[WARN]: $CONSUMER_SRC missing; the cross-module check will report SKIPPED"
fi

cat << 'EOF' > "$STAGING"/etc/draginit/05-dbus.toml
[daemon]
name = "dbus-daemon"
path = "/usr/bin/dbus-daemon"
uid = 1000
gid = 1000
delay_ms = 0
restart = "never"
restart_delay_ms = 2000
[daemon.env]
DBUS_SESSION_BUS_ADDRESS = "unix:path=/run/user/1000/bus"
EOF

cat << 'EOF' > "$STAGING"/etc/draginit/15-xdg-portal.toml
[daemon]
name = "xdg-portal"
path = "/usr/bin/xdg-portal"
uid = 1000
gid = 1000
delay_ms = 500
restart = "never"
restart_delay_ms = 2000
[daemon.env]
XDG_RUNTIME_DIR = "/run/user/1000"
DBUS_SESSION_BUS_ADDRESS = "unix:path=/run/user/1000/bus"
EOF

cat << 'EOF' > "$STAGING"/etc/draginit/26-gbmprobe.toml
[daemon]
name = "mywima"
path = "/usr/bin/mywima"
uid = 1000
gid = 1000
delay_ms = 0
restart = "never"
restart_delay_ms = 2000
[daemon.env]
LIBGL_DRIVERS_PATH = "/usr/lib/dri"
GBM_BACKEND = "dri"
GBM_BACKENDS_PATH = "/usr/lib/gbm"
LD_LIBRARY_PATH = "/lib:/usr/lib"
LIBGL_DEBUG = "verbose"
MESA_DEBUG = "1"
XDG_RUNTIME_DIR = "/run/user/1000"
WAYLAND_DISPLAY = "/run/user/1000/wayland-1"
DBUS_SESSION_BUS_ADDRESS = "unix:path=/run/user/1000/bus"
EOF

cat << 'EOF' > "$STAGING"/etc/draginit/18-pipewire.toml
[daemon]
name = "pipewire"
path = "/usr/bin/pipewire"
uid = 1000
gid = 1000
delay_ms = 300
restart = "on_failure"
restart_delay_ms = 2000
[daemon.env]
PIPEWIRE_RUNTIME_DIR = "/run/user/1000"
DBUS_SESSION_BUS_ADDRESS = "unix:path=/run/user/1000/bus"
XDG_RUNTIME_DIR = "/run/user/1000"
EOF

cat << 'EOF' > "$STAGING"/etc/draginit/20-seatd.toml
[daemon]
name = "seatd"
path = "/usr/bin/seatd"
uid = 0
gid = 0
delay_ms = 500
restart = "never"
restart_delay_ms = 2000
EOF

cat << 'EOF' > "$STAGING"/etc/draginit/26-gbmscanout.toml
[daemon]
name = "gbmscanout"
path = "/usr/bin/gbmscanout"
path = "/usr/bin/gbmscanout"
delay_ms = 0
restart = "never"

[daemon.env]
LIBGL_DRIVERS_PATH = "/usr/lib/dri"
GBM_BACKEND = "dri"
GBM_BACKENDS_PATH = "/usr/lib/gbm"
LD_LIBRARY_PATH = "/lib:/usr/lib"
EOF
cat << 'EOF' > "$STAGING"/etc/draginit/30-foot.toml
[daemon]
name = "foot"
path = "/usr/bin/foot"
uid = 1000
gid = 1000
# Long enough for mywima to finish EGL/GBM init and create the socket; it logs
# "Listening on WAYLAND_DISPLAY=..." only after the KMS backend is up.
delay_ms = 0
restart = "never"

[daemon.env]
# The full filesystem path, not a bare name.
#
# This kernel has no AF_UNIX *abstract* namespace support: sys_bind/sys_connect
# read sun_path as a NUL-terminated C string, so an abstract name (leading NUL)
# collapses to "" and the listener is keyed by a filesystem path instead.
# smithay's ListeningSocketSource::new_auto() tries the abstract socket first,
# falls back when that fails, and the kernel log shows the result:
#
#   [bind-tmp] pid=4 fd=8 path=/run/wayland-1
#
# so the compositor listens on /run/wayland-1. A bare "wayland-1" tells the
# client to use the *abstract* socket of that name, which does not exist, so
# every client failed to connect and the compositor never had a single client
# (data.state.space.elements() was always 0).
WAYLAND_DISPLAY = "/run/user/1000/wayland-1"
XDG_RUNTIME_DIR = "/run/user/1000"
EOF
cat << 'EOF' > "$STAGING"/etc/draginit/35-fixed-size-test.toml
[daemon]
name = "fixed_size_test"
path = "/usr/bin/fixed_size_test"
delay_ms = 800
restart = "never"

[daemon.env]
WAYLAND_DISPLAY = "/run/user/1000/wayland-1"
EOF
cat << 'EOF' > "$STAGING"/etc/draginit/40-socktest.toml
[daemon]
name = "socktest"
path = "/usr/bin/socktest"
delay_ms = 0
restart = "never"

[daemon.env]
# Must be the same path the compositor bound (see the note in 30-foot.toml).
WAYLAND_DISPLAY = "/run/user/1000/wayland-1"
EOF
cat << 'EOF' > "$STAGING"/etc/draginit/45-vulkansmoke.toml
[daemon]
name = "vulkansmoke"
path = "/usr/bin/vulkansmoke"
delay_ms = 0
restart = "never"

[daemon.env]
LD_LIBRARY_PATH = "/lib:/usr/lib"
VK_ICD_FILENAMES = "/usr/share/vulkan/icd.d/virtio_icd.json"
EOF
cat << 'EOF' > "$STAGING"/etc/draginit/46-sdl-vulkan-test.toml
[daemon]
name = "sdl_vulkan_test"
path = "/usr/bin/sdl_vulkan_test"
delay_ms = 1500
restart = "never"

[daemon.env]
WAYLAND_DISPLAY = "/run/user/1000/wayland-1"
WAYLAND_DEBUG = "1"
VN_DEBUG = "init,result,vtest"
LD_LIBRARY_PATH = "/lib:/usr/lib"
VK_ICD_FILENAMES = "/usr/share/vulkan/icd.d/virtio_icd.json"
EOF
cat << 'EOF' > "$STAGING"/etc/draginit/49-pure-vulkan-test.toml
[daemon]
name = "pure_vulkan_test"
path = "/usr/bin/pure_vulkan_test"
delay_ms = 2500
restart = "never"

[daemon.env]
WAYLAND_DISPLAY = "/run/user/1000/wayland-1"
LD_LIBRARY_PATH = "/lib:/usr/lib"
VK_ICD_FILENAMES = "/usr/share/vulkan/icd.d/virtio_icd.json"
EOF
cat << 'EOF' > "$STAGING"/etc/draginit/50-shell.toml
[daemon]
name = "shell"
path = "/usr/bin/ptyrun"
args = ["/bin/sh"]
delay_ms = 2000
restart = "never"
EOF

# Startup init script
cat << 'EOF' > "$STAGING"/bin/init.sh
#!/bin/sh
export PATH=/usr/local/sbin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin
export HOME=/root
export XDG_RUNTIME_DIR=/run
export XDG_CONFIG_HOME=/root/.config
export XCURSOR_THEME=Adwaita
export XCURSOR_SIZE=24
export XCURSOR_PATH=/usr/share/icons
# VirGL: QEMU --gl creates esp/efi/boot/virgl flag; mount esp and auto-select
mkdir -p /efi 2>/dev/null
mount -t vfat /dev/vda1 /efi 2>/dev/null || mount -t vfat /dev/sda1 /efi 2>/dev/null || true

# proc: libgbm/libdrm use /proc/self/fd/N symlink resolution; without it gbm_create_device fails
mkdir -p /proc/self/fd /proc/self /proc 2>/dev/null || true
mount -t proc proc /proc 2>/dev/null || true
unset GALLIUM_DRIVER
echo "[init] using auto-selected GBM/EGL driver" > /dev/console 2>/dev/null || true
# Speed: Mesa shader cache (saves ~2s on second boot)
export MESA_SHADER_CACHE_DIR=/tmp
export MESA_SHADER_CACHE_MAX_SIZE=32M
mkdir -p /tmp 2>/dev/null
mkdir -p /run/sshd /var/empty 2>/dev/null || true
/usr/sbin/sshd 2>/dev/null || /sbin/sshd 2>/dev/null || true
# Test harness (`cargo run -p tools -- test`) sets a flag and expects an
# interactive shell. It writes the flag in TWO places (test_runner.rs):
#   - esp/efi/boot/test_mode, i.e. /efi/efi/boot/test_mode once /efi is mounted
#   - /test_mode in the root filesystem, via `debugfs write`
#
# The /efi forms need `mount -t vfat`, but mount(2) is not dispatched by the
# kernel at all (no 165 arm in syscalls/mod.rs), so the vfat mount fails with
# ENOSYS and /efi is never populated. /test_mode lives in the root fs, which IS
# mounted, so it is the only flag the guest can actually see.
if [ "$DRAGOS_TEST_MODE" = "1" ] || [ -f /test_mode ] || [ -f /efi/efi/boot/test_mode ] || [ -f /efi/EFI/BOOT/test_mode ]; then
  echo "[SHELL_READY]" > /dev/ttyS0
  if [ -x /usr/bin/ptyrun ]; then
    exec /usr/bin/ptyrun /bin/sh > /dev/ttyS0 2>&1
  fi
  exec /bin/sh > /dev/ttyS0 2>&1
fi
echo "[init] starting seatd + clients"
/usr/bin/seatd &
WAYLAND_DISPLAY=wayland-1 /usr/bin/foot &
[ -x /usr/bin/socktest ] && /usr/bin/socktest
if [ -x /usr/bin/ptyrun ]; then
    /usr/bin/ptyrun /bin/sh &
fi
# Give the backgrounded seatd a moment to bind before mywima's libseat open
# (which fails with ENOENT otherwise, and mywima exits 1 with no graphics).
# NOTE: do NOT poll for /run/seatd.sock here: pathname unix binds are served
# from an in-memory table without linking a visible dirent, so no filesystem
# test (-S, -e, ls, stat) can ever observe the socket. One bounded sleep is
# the whole synchronization (bind completes in <1s once seatd runs).
echo "[init] waiting for seatd"
sleep 2
echo "[init] exec draginit"
exec /usr/bin/draginit
EOF
chmod +x "$STAGING"/bin/init.sh
# Host-gcc fallback for the small static helpers that the musl block below
# would otherwise build. That whole block is skipped when the musl cross-compiler
# is absent, and the consequence was not a warning but a *silently broken
# session*: /usr/bin/ptyrun and /usr/bin/sleep were absent, draginit's unit
# files still referenced them, and every spawn failed with
#
#   [execve] lookup failed for /usr/bin/ptyrun
#   [draginit] shell (pid 6) exited (exit 127)
#
# so there was no shell and no terminal in the guest, and the only thing left
# running was the compositor. Build them here with the host compiler, and let
# the completeness check at the end of this script report anything still
# missing rather than leaving it to be discovered in a guest log.
if command -v gcc >/dev/null 2>&1; then
  for spec in "ptyrun:ptyrun.c" "sleep:sleep.c" "socktest:socktest.c" "vulkansmoke:vulkansmoke.c"; do
    b=${spec%%:*}
    src=${spec##*:}
    if [ ! -f "$ROOT/initrd/$b" ] && [ -f "$ROOT/initrd/$src" ]; then
      # sleep.c uses dprintf, which glibc headers hide behind _GNU_SOURCE.
      if gcc -O2 -D_GNU_SOURCE -o "$ROOT/initrd/$b" "$ROOT/initrd/$src" 2>/dev/null; then
        echo "[build-fs] $b built from source (host gcc)"
      else
        echo "[build-fs] WARNING: could not build $b from $src" >&2
      fi
    fi
  done
  # Stage them: the musl block below is the *other* place these get copied, and
  # it is skipped entirely when the cross-compiler is missing.
  for b in ptyrun sleep socktest vulkansmoke; do
    if [ -x "$ROOT/initrd/$b" ]; then
      cp -p "$ROOT/initrd/$b" "$STAGING/usr/bin/$b" 2>/dev/null || true
      cp -p "$ROOT/initrd/$b" "$STAGING/bin/$b" 2>/dev/null || true
      cp -p "$ROOT/initrd/$b" "$STAGING/$b" 2>/dev/null || true
    fi
  done
  # mkdirtest probes which syscall glibc's mkdir() issues and what the kernel
  # answers to it. zellij dies with ENOSYS inside fs::create_dir, and the
  # syscall census only reports the top 12 -- so the failing number was not
  # visible and had to be measured directly.
  if [ -f "$ROOT/initrd/mkdirtest.c" ] && command -v gcc >/dev/null 2>&1; then
    if gcc -O2 -static -o "$ROOT/initrd/mkdirtest" "$ROOT/initrd/mkdirtest.c" 2>/dev/null; then
      cp -p "$ROOT/initrd/mkdirtest" "$STAGING/usr/bin/mkdirtest" 2>/dev/null || true
      cp -p "$ROOT/initrd/mkdirtest" "$STAGING/mkdirtest" 2>/dev/null || true
    fi
  fi
  # ptyposixtest walks the POSIX pty API (posix_openpt/grantpt/unlockpt/
  # ptsname_r) that glibc's openpty(3) uses. zellij stalled at "spawning tabs"
  # with no panes and no error while ptyrun worked, because ptyrun opens
  # /dev/ptmx and openpty goes through syscall 622, which was undispatched.
  # jobctltest covers sessions, process groups, controlling terminals and
  # pthread_join -- all of which previously returned plausible success while
  # changing nothing (setpgid), returned the wrong value (getpgid returned the
  # pid), or were not dispatched at all (getpgrp, pthread_join's wake). Each
  # case asserts the value, because a stub that returns 0 produces no error to
  # trace.
  # rawttytest issues TIOCSCTTY as a raw syscall as well as through glibc's
  # ioctl wrapper, because an ENOTTY with no matching kernel trace means the
  # syscall never arrived -- and that has to be distinguishable from the kernel
  # answering ENOTTY. Ground truth from Linux: both routes return EPERM.
  # tmpfsw checks that a write to a /tmp file is visible on a later read.
  # zellij's log file opens, gets written, and reads back empty, which is what
  # made it look like the session had stalled with no diagnostics at all.
  if [ -f "$ROOT/initrd/tmpfsw.c" ] && command -v gcc >/dev/null 2>&1; then
    if gcc -O2 -static -o "$ROOT/initrd/tmpfsw" "$ROOT/initrd/tmpfsw.c" 2>/dev/null; then
      cp -p "$ROOT/initrd/tmpfsw" "$STAGING/usr/bin/tmpfsw" 2>/dev/null || true
      cp -p "$ROOT/initrd/tmpfsw" "$STAGING/tmpfsw" 2>/dev/null || true
    fi
  fi
  if [ -f "$ROOT/initrd/rawttytest.c" ] && command -v gcc >/dev/null 2>&1; then
    if gcc -O2 -static -o "$ROOT/initrd/rawttytest" "$ROOT/initrd/rawttytest.c" 2>/dev/null \
       || gcc -O2 -o "$ROOT/initrd/rawttytest" "$ROOT/initrd/rawttytest.c" 2>/dev/null; then
      cp -p "$ROOT/initrd/rawttytest" "$STAGING/usr/bin/rawttytest" 2>/dev/null || true
      cp -p "$ROOT/initrd/rawttytest" "$STAGING/rawttytest" 2>/dev/null || true
    fi
  fi
  if [ -f "$ROOT/initrd/jobctltest.c" ] && command -v gcc >/dev/null 2>&1; then
    if gcc -O2 -static -pthread -o "$ROOT/initrd/jobctltest" "$ROOT/initrd/jobctltest.c" 2>/dev/null \
       || gcc -O2 -pthread -o "$ROOT/initrd/jobctltest" "$ROOT/initrd/jobctltest.c" 2>/dev/null; then
      cp -p "$ROOT/initrd/jobctltest" "$STAGING/usr/bin/jobctltest" 2>/dev/null || true
      cp -p "$ROOT/initrd/jobctltest" "$STAGING/jobctltest" 2>/dev/null || true
    fi
  fi
  # jobctltest_stop: SIGTSTP/SIGCONT stop/resume and waitpid(WUNTRACED/WCONTINUED)
  if [ -f "$ROOT/initrd/jobctltest_stop.c" ] && command -v gcc >/dev/null 2>&1; then
    if gcc -O2 -static -o "$ROOT/initrd/jobctltest_stop" "$ROOT/initrd/jobctltest_stop.c" 2>/dev/null; then
      cp -p "$ROOT/initrd/jobctltest_stop" "$STAGING/usr/bin/jobctltest_stop" 2>/dev/null || true
      cp -p "$ROOT/initrd/jobctltest_stop" "$STAGING/jobctltest_stop" 2>/dev/null || true
    fi
  fi
  # procstress: growing process-scale stress. Ramps concurrent live processes
  # and verifies every child is reaped with its own exit status.
  if [ -f "$ROOT/initrd/procstress.c" ] && command -v gcc >/dev/null 2>&1; then
    if gcc -O2 -static -o "$ROOT/initrd/procstress" "$ROOT/initrd/procstress.c" 2>/dev/null; then
      cp -p "$ROOT/initrd/procstress" "$STAGING/usr/bin/procstress" 2>/dev/null || true
      cp -p "$ROOT/initrd/procstress" "$STAGING/procstress" 2>/dev/null || true
    fi
  fi
  if [ -f "$ROOT/initrd/ptyposixtest.c" ] && command -v gcc >/dev/null 2>&1; then
    if gcc -O2 -static -o "$ROOT/initrd/ptyposixtest" "$ROOT/initrd/ptyposixtest.c" -lutil 2>/dev/null \
       || gcc -O2 -o "$ROOT/initrd/ptyposixtest" "$ROOT/initrd/ptyposixtest.c" 2>/dev/null; then
      cp -p "$ROOT/initrd/ptyposixtest" "$STAGING/usr/bin/ptyposixtest" 2>/dev/null || true
      cp -p "$ROOT/initrd/ptyposixtest" "$STAGING/ptyposixtest" 2>/dev/null || true
    fi
  fi
  # privdrop: starts as root, drops to uid 1000, and asserts that the drop
  # actually cost it something. This is the test for the file-permission
  # checks; it creates and chmods its own fixtures, so it does not depend on
  # the image's modes.
  if [ -f "$ROOT/initrd/privdrop.c" ] && command -v gcc >/dev/null 2>&1; then
    if gcc -O2 -static -o "$ROOT/initrd/privdrop" "$ROOT/initrd/privdrop.c" 2>/dev/null \
       || gcc -O2 -o "$ROOT/initrd/privdrop" "$ROOT/initrd/privdrop.c" 2>/dev/null; then
      cp -p "$ROOT/initrd/privdrop" "$STAGING/usr/bin/privdrop" 2>/dev/null || true
      cp -p "$ROOT/initrd/privdrop" "$STAGING/privdrop" 2>/dev/null || true
    fi
  fi
fi

if [ -f "$ROOT/initrd/mesarepro.c" ] && command -v gcc >/dev/null 2>&1; then
  gcc -Wall -O2 -pthread -o "$ROOT/initrd/mesarepro" "$ROOT/initrd/mesarepro.c" -lgbm || exit 1
  gcc -Wall -O2 -rdynamic -o "$ROOT/initrd/vkprobe" "$ROOT/initrd/vkprobe.c" -lvulkan || exit 1
  gcc -Wall -O2 -I/usr/include/libdrm -o "$ROOT/initrd/drmprobe" "$ROOT/initrd/drmprobe.c" -ldrm || exit 1
  echo "[build-fs] mesarepro/vkprobe/drmprobe built from source (host gcc)"
  # virglprobe and gbmprobe only need libc. Prefer the musl toolchain for a
  # static binary (it runs before any dynamic loader is up); fall back to the
  # host gcc, which the guest can also run since the image carries glibc.
  if [ ! -x "$ROOT/x86_64-linux-musl-native/bin/gcc" ]; then
    if gcc -Wall -O2 -o "$ROOT/initrd/virglprobe" "$ROOT/initrd/virglprobe.c" 2>/dev/null &&
       gcc -Wall -O2 -o "$ROOT/initrd/gbmprobe" "$ROOT/initrd/gbmprobe.c" 2>/dev/null; then
      echo "[build-fs] virglprobe/gbmprobe built from source (host gcc, dynamic)"
    else
      echo "[build-fs] WARNING: no musl toolchain and host gcc could not build" \
           "virglprobe/gbmprobe; those will be stale"
    fi
    # gbmscanout calls gbm_create_device, so it links libgbm; the musl
    # cross toolchain here ships no libgbm.a, so build it dynamically
    # against the host's libgbm (the image carries the same library).
    if gcc -Wall -O2 -o "$ROOT/initrd/gbmscanout" "$ROOT/initrd/gbmscanout.c" -lgbm 2>/dev/null; then
      echo "[build-fs] gbmscanout built from source (host gcc, dynamic)"
    else
      echo "[build-fs] WARNING: gbmscanout could not be built; it will be stale"
    fi
  fi
fi

if [ -x "$ROOT/x86_64-linux-musl-native/bin/gcc" ]; then
  "$ROOT/x86_64-linux-musl-native/bin/gcc" -Wall -Wextra -Werror -static -O2 \
    -o "$ROOT/initrd/virglprobe" "$ROOT/initrd/virglprobe.c" || exit 1
  "$ROOT/x86_64-linux-musl-native/bin/gcc" -Wall -Wextra -static -O2 \
    -o "$ROOT/initrd/gbmprobe" "$ROOT/initrd/gbmprobe.c" || exit 1
  if [ -f "$ROOT/initrd/player_musl.c" ]; then
    "$ROOT/x86_64-linux-musl-native/bin/gcc" -Wall -Wextra -static -O2 \
      -o "$STAGING/bin/player" "$ROOT/initrd/player_musl.c" \
      && chmod +x "$STAGING/bin/player" \
      && cp -p "$STAGING/bin/player" "$ROOT/initrd/player" \
      && cp -p "$STAGING/bin/player" "$ROOT/initrd/mplayer" \
      && cp -p "$STAGING/bin/player" "$STAGING/bin/mplayer" \
      && cp -p "$STAGING/bin/player" "$STAGING/usr/bin/player" \
      && cp -p "$STAGING/bin/player" "$STAGING/player" \
      && echo "[build-fs] player built from source"
  fi
  "$ROOT/x86_64-linux-musl-native/bin/gcc" -static -O2 \
    -o "$STAGING/bin/kms_player" "$ROOT/initrd/kms_player.c" \
    && chmod +x "$STAGING/bin/kms_player" \
    && echo "[build-fs] kms_player built from source"
  "$ROOT/x86_64-linux-musl-native/bin/gcc" -static -O2 \
    -o "$STAGING/bin/ptyrun" "$ROOT/initrd/ptyrun.c" \
    && chmod +x "$STAGING/bin/ptyrun" \
    && echo "[build-fs] ptyrun built from source"
  "$ROOT/x86_64-linux-musl-native/bin/gcc" -static -O2 \
    -o "$STAGING/bin/sleep" "$ROOT/initrd/sleep.c" \
    && chmod +x "$STAGING/bin/sleep" \
    && echo "[build-fs] sleep built from source"
  if [ -f "$ROOT/initrd/ext4test.c" ]; then
    "$ROOT/x86_64-linux-musl-native/bin/gcc" -static -O2 \
      -o "$ROOT/initrd/ext4test" "$ROOT/initrd/ext4test.c" \
      && chmod +x "$ROOT/initrd/ext4test" \
      && echo "[build-fs] ext4test built from source"
  fi
  if [ -f "$ROOT/initrd/wl_player.c" ] && command -v wayland-scanner >/dev/null 2>&1; then
    wayland-scanner client-header /usr/share/wayland-protocols/stable/xdg-shell/xdg-shell.xml "$ROOT/initrd/xdg-shell-client-protocol.h" 2>/dev/null || true
    wayland-scanner private-code /usr/share/wayland-protocols/stable/xdg-shell/xdg-shell.xml "$ROOT/initrd/xdg-shell-protocol.c" 2>/dev/null || true
    gcc -O2 -o "$STAGING/bin/wl_player" "$ROOT/initrd/wl_player.c" "$ROOT/initrd/xdg-shell-protocol.c" $(pkg-config --cflags --libs wayland-client 2>/dev/null) -lrt -lm 2>/dev/null \
      && chmod +x "$STAGING/bin/wl_player" \
      && cp -p "$STAGING/bin/wl_player" "$ROOT/initrd/wl_player" \
      && cp -p "$STAGING/bin/wl_player" "$STAGING/usr/bin/wl_player" \
      && cp -p "$STAGING/bin/wl_player" "$STAGING/wl_player" \
      && echo "[build-fs] wl_player (Wayland video player) built from source"
  fi
elif [ -f /tmp/opencode/kms_player ]; then
  cp -p /tmp/opencode/kms_player "$STAGING/bin/kms_player"
  chmod +x "$STAGING/bin/kms_player"
fi
# A Wayland client that declares a fixed window size
# (xdg_toplevel.set_min_size == set_max_size). The compositor is expected to
# float such a window instead of tiling it. Built with the host toolchain
# (it is a dynamically linked Wayland client, not a musl static one).
if [ -f "$ROOT/initrd/fixed_size_test.c" ] && command -v wayland-scanner >/dev/null 2>&1; then
  wayland-scanner client-header /usr/share/wayland-protocols/stable/xdg-shell/xdg-shell.xml "$ROOT/initrd/xdg-shell-client-protocol.h" 2>/dev/null || true
  wayland-scanner private-code /usr/share/wayland-protocols/stable/xdg-shell/xdg-shell.xml "$ROOT/initrd/xdg-shell-protocol.c" 2>/dev/null || true
  gcc -O2 -o "/tmp/opencode/fixed_size_test" "$ROOT/initrd/fixed_size_test.c" "$ROOT/initrd/xdg-shell-protocol.c" $(pkg-config --cflags --libs wayland-client 2>/dev/null) -lrt 2>/dev/null \
    && chmod +x "/tmp/opencode/fixed_size_test" \
    && cp -p "/tmp/opencode/fixed_size_test" "$STAGING/bin/fixed_size_test" \
    && stage_bin_with_libs "/tmp/opencode/fixed_size_test" \
    && cp -p "/tmp/opencode/fixed_size_test" "$STAGING/usr/bin/fixed_size_test" \
    && cp -p "/tmp/opencode/fixed_size_test" "$ROOT/initrd/fixed_size_test" \
    && echo "[build-fs] fixed_size_test built from source"
fi
if [ -f /tmp/opencode/vidtest.rgb32 ]; then
  cp -p /tmp/opencode/vidtest.rgb32 "$STAGING/vidtest.rgb32"
fi
if [ ! -f "$ROOT/clip.rgb32" ] && [ -f "$ROOT/video.mp4" ] && command -v ffmpeg >/dev/null 2>&1; then
  ffmpeg -y -v error -ss 0 -t 10 -i "$ROOT/video.mp4" \
    -vf scale=320:200,fps=30 -f rawvideo -pix_fmt rgb32 "$ROOT/clip.rgb32" \
    -ar 44100 -ac 2 -f s16le "$ROOT/clip.pcm" \
    && echo "[build-fs] test clip extracted"
fi
for f in clip.rgb32 clip.pcm vidtest.rgb32; do
  if [ -f "$ROOT/$f" ]; then
    cp -p "$ROOT/$f" "$STAGING/$f"
  elif [ -f /tmp/opencode/$f ]; then
    cp -p /tmp/opencode/$f "$STAGING/$f"
  fi
done

# Libinput and glvnd data files and udev seat metadata
if [ -d /usr/share/libinput ]; then
    mkdir -p "$STAGING"/usr/share
    cp -r /usr/share/libinput "$STAGING"/usr/share/
fi
if [ -d /usr/share/glvnd ]; then
    mkdir -p "$STAGING"/usr/share
    cp -r /usr/share/glvnd "$STAGING"/usr/share/
fi
# XKB keymap config (xkeyboard-config-2) - required by libxkbcommon
if [ -d /usr/share/xkeyboard-config-2 ]; then
    mkdir -p "$STAGING"/usr/share
    cp -r /usr/share/xkeyboard-config-2 "$STAGING"/usr/share/
fi
if [ -d /usr/share/X11/xkb ]; then
# Terminfo database (required by tmux, ncurses apps)
if [ -d /usr/share/terminfo ]; then
    mkdir -p "$STAGING"/usr/share/terminfo
    # Copy essential terminfo entries for tmux: screen, screen-256color, xterm, xterm-256color, linux, vt100
    for dir in a b c d e f g h i j k l m n o p q r s t u v w x y z 0 1 2 3 4 5 6 7 8 9; do
        [ -d /usr/share/terminfo/$dir ] && cp -r /usr/share/terminfo/$dir "$STAGING"/usr/share/terminfo/ 2>/dev/null || true
    done
    # Also copy uppercase directories
    for dir in A B C D E F G H I J K L M N O P Q R S T U V W X Y Z; do
        [ -d /usr/share/terminfo/$dir ] && cp -r /usr/share/terminfo/$dir "$STAGING"/usr/share/terminfo/ 2>/dev/null || true
    done
fi
# termcap (legacy, some apps still use it)
if [ -f /etc/termcap ]; then
    cp -p /etc/termcap "$STAGING"/etc/termcap 2>/dev/null || true
fi
    mkdir -p "$STAGING"/usr/share/X11
# Terminfo database (required by tmux, ncurses apps)
if [ -d /usr/share/terminfo ]; then
    mkdir -p "$STAGING"/usr/share/terminfo
    # Copy essential terminfo entries for tmux: screen, screen-256color, xterm, xterm-256color, linux, vt100
    for dir in a b c d e f g h i j k l m n o p q r s t u v w x y z 0 1 2 3 4 5 6 7 8 9; do
        [ -d /usr/share/terminfo/$dir ] && cp -r /usr/share/terminfo/$dir "$STAGING"/usr/share/terminfo/ 2>/dev/null || true
    done
    # Also copy uppercase directories
    for dir in A B C D E F G H I J K L M N O P Q R S T U V W X Y Z; do
        [ -d /usr/share/terminfo/$dir ] && cp -r /usr/share/terminfo/$dir "$STAGING"/usr/share/terminfo/ 2>/dev/null || true
    done
fi
# termcap (legacy, some apps still use it)
if [ -f /etc/termcap ]; then
    cp -p /etc/termcap "$STAGING"/etc/termcap 2>/dev/null || true
fi
    cp -r /usr/share/X11/xkb "$STAGING"/usr/share/X11/
# Terminfo database (required by tmux, ncurses apps)
if [ -d /usr/share/terminfo ]; then
    mkdir -p "$STAGING"/usr/share/terminfo
    # Copy essential terminfo entries for tmux: screen, screen-256color, xterm, xterm-256color, linux, vt100
    for dir in a b c d e f g h i j k l m n o p q r s t u v w x y z 0 1 2 3 4 5 6 7 8 9; do
        [ -d /usr/share/terminfo/$dir ] && cp -r /usr/share/terminfo/$dir "$STAGING"/usr/share/terminfo/ 2>/dev/null || true
    done
    # Also copy uppercase directories
    for dir in A B C D E F G H I J K L M N O P Q R S T U V W X Y Z; do
        [ -d /usr/share/terminfo/$dir ] && cp -r /usr/share/terminfo/$dir "$STAGING"/usr/share/terminfo/ 2>/dev/null || true
    done
fi
# termcap (legacy, some apps still use it)
if [ -f /etc/termcap ]; then
    cp -p /etc/termcap "$STAGING"/etc/termcap 2>/dev/null || true
fi
fi
# Terminfo database (required by tmux, ncurses apps)
if [ -d /usr/share/terminfo ]; then
    mkdir -p "$STAGING"/usr/share/terminfo
    # Copy essential terminfo entries for tmux: screen, screen-256color, xterm, xterm-256color, linux, vt100
    for dir in a b c d e f g h i j k l m n o p q r s t u v w x y z 0 1 2 3 4 5 6 7 8 9; do
        [ -d /usr/share/terminfo/$dir ] && cp -r /usr/share/terminfo/$dir "$STAGING"/usr/share/terminfo/ 2>/dev/null || true
    done
    # Also copy uppercase directories
    for dir in A B C D E F G H I J K L M N O P Q R S T U V W X Y Z; do
        [ -d /usr/share/terminfo/$dir ] && cp -r /usr/share/terminfo/$dir "$STAGING"/usr/share/terminfo/ 2>/dev/null || true
    done
fi
# termcap (legacy, some apps still use it)
if [ -f /etc/termcap ]; then
    cp -p /etc/termcap "$STAGING"/etc/termcap 2>/dev/null || true
fi
# Cursor themes (X11/xcursor) - required for compositor cursor rendering
if [ -d /usr/share/icons ]; then
    mkdir -p "$STAGING"/usr/share/icons/default/cursors
    mkdir -p "$STAGING"/usr/share/icons/Adwaita/cursors
    mkdir -p "$STAGING"/usr/share/icons/default
    if [ -f /usr/share/icons/default/index.theme ] && [ -s /usr/share/icons/default/index.theme ]; then
        cp -p /usr/share/icons/default/index.theme "$STAGING"/usr/share/icons/default/
    else
        printf "[Icon Theme]\nInherits=Adwaita\n" > "$STAGING"/usr/share/icons/default/index.theme
    fi
    [ -f /usr/share/icons/Adwaita/index.theme ] && cp -p /usr/share/icons/Adwaita/index.theme "$STAGING"/usr/share/icons/Adwaita/
    if [ -d /usr/share/icons/Adwaita/cursors ]; then
        cp -L /usr/share/icons/Adwaita/cursors/* "$STAGING"/usr/share/icons/Adwaita/cursors/ 2>/dev/null || true
        cp -L /usr/share/icons/Adwaita/cursors/* "$STAGING"/usr/share/icons/default/cursors/ 2>/dev/null || true
    fi
fi
rm -rf "$STAGING"/run/udev

# Firmware binaries
[ -f /home/jyash8/Projects/drag_os/initrd/lib/firmware/amdgpu/navi10_pfp.bin ] && cp -p /home/jyash8/Projects/drag_os/initrd/lib/firmware/amdgpu/navi10_pfp.bin "$STAGING"/lib/firmware/amdgpu/
[ -f /home/jyash8/Projects/drag_os/initrd/lib/firmware/i915/kbl_dmc_ver1_04.bin ] && cp -p /home/jyash8/Projects/drag_os/initrd/lib/firmware/i915/kbl_dmc_ver1_04.bin "$STAGING"/lib/firmware/i915/

# Core user-space utilities into /bin
if [ -f /home/jyash8/Projects/drag_os/initrd/bash ]; then
  cp -p /home/jyash8/Projects/drag_os/initrd/bash "$STAGING"/bin/sh
  mkdir -p "$STAGING"/usr/bin
  cp -p /home/jyash8/Projects/drag_os/initrd/bash "$STAGING"/bin/bash
  cp -p /home/jyash8/Projects/drag_os/initrd/bash "$STAGING"/usr/bin/bash
elif [ -f /home/jyash8/Projects/drag_os/initrd/sh ]; then
  cp -p /home/jyash8/Projects/drag_os/initrd/sh "$STAGING"/bin/sh
fi
if [ -f /home/jyash8/Projects/drag_os/initrd/sh ]; then
  cp -p /home/jyash8/Projects/drag_os/initrd/sh "$STAGING"/bin/dash
fi
[ -f /home/jyash8/Projects/drag_os/initrd/bin/ls ] && cp -p /home/jyash8/Projects/drag_os/initrd/bin/ls "$STAGING"/bin/ls
[ -f /home/jyash8/Projects/drag_os/initrd/bin/cat ] && cp -p /home/jyash8/Projects/drag_os/initrd/bin/cat "$STAGING"/bin/cat
[ -f /home/jyash8/Projects/drag_os/initrd/reboot ] && cp -p /home/jyash8/Projects/drag_os/initrd/reboot "$STAGING"/bin/reboot
[ -f /home/jyash8/Projects/drag_os/initrd/poweroff ] && cp -p /home/jyash8/Projects/drag_os/initrd/poweroff "$STAGING"/bin/poweroff
BUSYBOX="$ROOT/vendor/busybox-1.36.1/busybox"
if [ -f "$BUSYBOX" ]; then
  for applet in busybox mount mkdir umount; do
    cp -p "$BUSYBOX" "$STAGING"/bin/$applet
    chmod +x "$STAGING"/bin/$applet
  done
# SSH daemon and client utilities

if [ -f /usr/sbin/sshd ]; then

  mkdir -p "$STAGING"/usr/sbin

  cp -p /usr/sbin/sshd "$STAGING"/usr/sbin/

  chmod +x "$STAGING"/usr/sbin/sshd

fi

if [ -f /usr/bin/ssh ]; then

  mkdir -p "$STAGING"/usr/bin

  cp -p /usr/bin/ssh "$STAGING"/usr/bin/

  chmod +x "$STAGING"/usr/bin/ssh

fi

if [ -f /usr/bin/ssh-keygen ]; then

  mkdir -p "$STAGING"/usr/bin

  cp -p /usr/bin/ssh-keygen "$STAGING"/usr/bin/

  chmod +x "$STAGING"/usr/bin/ssh-keygen

fi

if [ -f /usr/bin/scp ]; then

  mkdir -p "$STAGING"/usr/bin

  cp -p /usr/bin/scp "$STAGING"/usr/bin/

  chmod +x "$STAGING"/usr/bin/scp

fi

if [ -f /usr/bin/sftp ]; then

  mkdir -p "$STAGING"/usr/bin

  cp -p /usr/bin/sftp "$STAGING"/usr/bin/

  chmod +x "$STAGING"/usr/bin/sftp

fi

fi

if [ -f /usr/bin/ssh-keygen ]; then

  mkdir -p "$STAGING"/usr/bin

  cp -p /usr/bin/ssh-keygen "$STAGING"/usr/bin/

  chmod +x "$STAGING"/usr/bin/ssh-keygen

fi

if [ -f /usr/bin/scp ]; then

  mkdir -p "$STAGING"/usr/bin

  cp -p /usr/bin/scp "$STAGING"/usr/bin/

  chmod +x "$STAGING"/usr/bin/scp

fi

if [ -f /usr/bin/sftp ]; then

  mkdir -p "$STAGING"/usr/bin

  cp -p /usr/bin/sftp "$STAGING"/usr/bin/

  chmod +x "$STAGING"/usr/bin/sftp
mkdir -p "$STAGING"/usr/local/sbin
mkdir -p "$STAGING"/usr/local/bin

fi


if [ -f /tmp/socktest ]; then
  cp -p /tmp/socktest "$STAGING"/bin/socktest 2>/dev/null || true
  chmod +x "$STAGING"/bin/socktest 2>/dev/null || true
fi
if [ -f /tmp/inputtest ]; then
  cp -p /tmp/inputtest "$STAGING"/bin/inputtest 2>/dev/null || true
  chmod +x "$STAGING"/bin/inputtest 2>/dev/null || true
fi
# Standard tests and demo utilities into /usr/bin
for b in hello test_musl fork_test clone_test dso_test drm_test flip_test fbtest test_write doom sig_test cpptest cppthread loop_test mmap_test eventfd_test epoll_test timerfd_test mprotect_fault_test fpu_concurrency_test efault_security_test wm test_tokio drm_event_test drm_cursor_test input_test libevdev_test libinput_test vfstest epoll_input_test ext4test privdrop netprobe dnstest virglprobe gbmprobe gbmscanout gbm_test_guest mesarepro ptyprobe pcm_test tmux mounttest ptrace_test; do
    if [ -f "/home/jyash8/Projects/drag_os/initrd/$b" ]; then
        cp -p "/home/jyash8/Projects/drag_os/initrd/$b" "$STAGING/usr/bin/$b"
        # Root compatibility alias
        cp -p "/home/jyash8/Projects/drag_os/initrd/$b" "$STAGING/$b"
    fi
done

# Doom WAD data file
if [ -f /home/jyash8/Projects/drag_os/initrd/doom1.wad ]; then
    cp -p /home/jyash8/Projects/drag_os/initrd/doom1.wad "$STAGING"/doom1.wad
fi

# Dynamic C++ and Threading test binaries from /tmp if built there
if [ -f /tmp/cpptest ] && [ ! -f /home/jyash8/Projects/drag_os/initrd/cpptest ]; then
    cp -p /tmp/cpptest "$STAGING"/usr/bin/cpptest
    cp -p /tmp/cpptest "$STAGING"/cpptest
fi
if [ -f /tmp/cppthread ] && [ ! -f /home/jyash8/Projects/drag_os/initrd/cppthread ]; then
    cp -p /tmp/cppthread "$STAGING"/usr/bin/cppthread
    cp -p /tmp/cppthread "$STAGING"/cppthread
fi

# Root compatibility symlinks/copies
cp -p "$STAGING"/bin/sh "$STAGING"/sh
[ -f "$STAGING"/bin/ls ] && cp -p "$STAGING"/bin/ls "$STAGING"/ls
[ -f "$STAGING"/bin/cat ] && cp -p "$STAGING"/bin/cat "$STAGING"/cat
[ -f /home/jyash8/Projects/drag_os/initrd/hello.txt ] && cp -p /home/jyash8/Projects/drag_os/initrd/hello.txt "$STAGING"/hello.txt

# Build differential manifest (content hash per staged file) before packaging,
# so the tar cache below can compare against it.
MANIFEST="$ROOT/.build-fs.manifest"
MANIFEST_OLD="$ROOT/.build-fs.manifest.old"
if [ -f "$MANIFEST" ]; then cp "$MANIFEST" "$MANIFEST_OLD"; else rm -f "$MANIFEST_OLD"; touch "$MANIFEST_OLD"; fi
find "$STAGING" -type f -print0 | xargs -0 -r sha256sum 2>/dev/null | while read -r h pp; do
    rel="${pp#$STAGING/}"
    case "$rel" in etc/ssh/ssh_host_*|root/Pictures/Wallpapers/*|proc/*|dev/*|sys/*) continue ;; esac
    sz=$(stat -c '%s' "$pp" 2>/dev/null || echo 0)
    printf '%s\t%s\t%s\n' "$rel" "$sz" "$h"
done | LC_ALL=C sort > "$ROOT/.build-fs.manifest.new"

# Package as TAR initrd (bootloader loads esp/efi/boot/initrd.tar)
mkdir -p /home/jyash8/Projects/drag_os/esp/efi/boot
rm -f /home/jyash8/Projects/drag_os/esp/efi/boot/initrd.tar
if [ -f "$ROOT/disk.img" ]; then
    TMP_INITRD=$(mktemp -d)
    mkdir -p "$TMP_INITRD"/bin "$TMP_INITRD"/root/Pictures/Wallpapers "$TMP_INITRD"/root/.config/mywima
    mkdir -p "$TMP_INITRD"/usr/lib/gbm "$TMP_INITRD"/usr/lib/dri
    mkdir -p "$TMP_INITRD"/lib "$TMP_INITRD"/usr/lib
    cp -p "$STAGING"/bin/sh "$TMP_INITRD"/bin/ 2>/dev/null || true
    cp -p "$STAGING"/usr/bin/tmux "$TMP_INITRD"/usr/bin/tmux 2>/dev/null || true
    cp -p "$STAGING"/usr/bin/tmux "$TMP_INITRD"/tmux 2>/dev/null || true
    cp -p "$STAGING"/usr/bin/flip_test "$TMP_INITRD"/usr/bin/flip_test 2>/dev/null || true
    cp -p "$STAGING"/usr/bin/flip_test "$TMP_INITRD"/flip_test 2>/dev/null || true
    cp -p "$STAGING"/bin/init.sh "$TMP_INITRD"/bin/ 2>/dev/null || true
     ln -sf /bin/init.sh "$TMP_INITRD"/init
     mkdir -p "$TMP_INITRD"/etc/draginit
    mkdir -p "$TMP_INITRD"/etc "$TMP_INITRD"/root
    mkdir -p "$TMP_INITRD"/usr/lib/locale
    cp -p "$STAGING"/usr/lib/locale/locale-archive "$TMP_INITRD"/usr/lib/locale/ 2>/dev/null || true
    cp -p "$STAGING"/etc/drirc "$TMP_INITRD"/etc/drirc 2>/dev/null || true
    cp -p "$STAGING"/root/.drirc "$TMP_INITRD"/root/.drirc 2>/dev/null || true
    # GBM/DRI libraries remain on disk.img
    # cp -r "$STAGING"/usr/lib/gbm/* "$TMP_INITRD"/usr/lib/gbm/ 2>/dev/null || true
    # cp -r "$STAGING"/usr/lib/dri/* "$TMP_INITRD"/usr/lib/dri/ 2>/dev/null || true
    # # Dependencies for swrast_dri.so (libgbm, libgallium, libLLVM, libSPIRV-Tools, libzstd)
    # cp -p "$STAGING"/lib/libgbm*.so* "$TMP_INITRD"/lib/ 2>/dev/null || true
    # cp -p "$STAGING"/lib/libgallium-*.so "$TMP_INITRD"/lib/ 2>/dev/null || true
    # cp -p "$STAGING"/lib/libLLVM*.so* "$TMP_INITRD"/lib/ 2>/dev/null || true
    # cp -p "$STAGING"/lib/libSPIRV-Tools*.so "$TMP_INITRD"/lib/ 2>/dev/null || true
    # cp -p "$STAGING"/lib/libzstd*.so* "$TMP_INITRD"/lib/ 2>/dev/null || true
    # Dynamic linker (required for glibc binaries like mywima)
    cp -p "$STAGING"/lib64/ld-linux-x86-64.so.2 "$TMP_INITRD"/lib64/ 2>/dev/null || true
    cp -p "$STAGING"/lib/ld-linux-x86-64.so.2 "$TMP_INITRD"/lib/ 2>/dev/null || true
    # Wallace for verification even in minimal initrd
    [ -f "$STAGING/root/Pictures/Wallpapers/random_wallpaper.png" ] && cp -p "$STAGING/root/Pictures/Wallpapers/random_wallpaper.png" "$TMP_INITRD/root/Pictures/Wallpapers/" 2>/dev/null || true
    tar -cf /home/jyash8/Projects/drag_os/esp/efi/boot/initrd.tar -C "$TMP_INITRD" .
    rm -rf "$TMP_INITRD"
    echo "[build-fs] Tiny initrd (disk.img present) - fast boot"
else
    tar --exclude='*icudata*' -cf /home/jyash8/Projects/drag_os/esp/efi/boot/initrd.tar -C "$STAGING" .
fi
# Commit the new differential manifest now that disk.img is up to date.
if [ -f "$ROOT/.build-fs.manifest.new" ]; then
    mv "$ROOT/.build-fs.manifest.new" "$ROOT/.build-fs.manifest"
fi
rm -f "$ROOT/.build-fs.manifest.old"

echo "Filesystem image setup complete."
ls -lh /home/jyash8/Projects/drag_os/esp/efi/boot/initrd.tar

# ── Sync test binaries and libraries into the ext2/ext4 root disk (disk.img) ─
if [ -f "$ROOT/disk.img" ] && command -v debugfs >/dev/null; then
  img_size=$(stat -c%s "$ROOT/disk.img" 2>/dev/null || echo 0)
  if [ "$img_size" -lt 4294967296 ]; then
    qemu-img resize -f raw "$ROOT/disk.img" 4096M >/dev/null 2>&1 || true
    e2fsck -fy "$ROOT/disk.img" >/dev/null 2>&1 || true
    resize2fs "$ROOT/disk.img" >/dev/null 2>&1 || true
  fi
  debugfs -w -R "mkdir /proc" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /proc/self" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /proc/self/fd" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /dev" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /dev/dri" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /dev/input" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /dev/snd" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /lib64" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /usr" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /usr/lib" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /usr/bin" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /usr/lib/gbm" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /usr/lib/dri" "$ROOT/disk.img" >/dev/null 2>&1 || true
  if [ -d "$STAGING/usr/lib/gbm" ]; then
    for f in $(ls "$STAGING"/usr/lib/gbm); do
      debugfs -w -R "rm /usr/lib/gbm/$f" "$ROOT/disk.img" >/dev/null 2>&1 || true
      debugfs -w -R "write $STAGING/usr/lib/gbm/$f /usr/lib/gbm/$f" "$ROOT/disk.img" >/dev/null 2>&1
    done
  fi
  if [ -d "$STAGING/usr/lib/dri" ]; then
    for f in $(ls "$STAGING"/usr/lib/dri); do
      debugfs -w -R "rm /usr/lib/dri/$f" "$ROOT/disk.img" >/dev/null 2>&1 || true
      debugfs -w -R "write $STAGING/usr/lib/dri/$f /usr/lib/dri/$f" "$ROOT/disk.img" >/dev/null 2>&1
    done
  fi
  # Batch sync runtime libraries, system config, SSL certs, and networking/SSH binaries
  BATCH="/tmp/dragos-lib-etc-sync-$$.cmd"
  {
    echo "mkdir /lib"
    echo "mkdir /lib64"
    echo "mkdir /usr"
    echo "mkdir /usr/lib"
    echo "mkdir /usr/lib/ssh"
    echo "mkdir /usr/bin"
    echo "mkdir /usr/sbin"
    echo "mkdir /bin"
    echo "mkdir /sbin"
    echo "mkdir /etc"
    echo "mkdir /etc/ssh"
    echo "mkdir /etc/ssl"
    echo "mkdir /etc/ssl/certs"
    echo "mkdir /var"
    echo "mkdir /var/empty"
    echo "mkdir /run"
    echo "mkdir /run/sshd"
    echo "mkdir /usr/share"
    echo "mkdir /usr/share/empty.sshd"
    echo "mkdir /root"
    echo "mkdir /root/.ssh"
    if [ -f "$STAGING/root/.ssh/authorized_keys" ]; then
      echo "rm /root/.ssh/authorized_keys"
      echo "write $STAGING/root/.ssh/authorized_keys /root/.ssh/authorized_keys"
    fi
    if [ -f "$STAGING/root/.drirc" ]; then
      echo "rm /root/.drirc"
      echo "write $STAGING/root/.drirc /root/.drirc"
    fi

    for f in "$STAGING"/lib/*; do
      if [ -f "$f" ]; then
        bname=$(basename "$f")
        echo "rm /lib/$bname"
        echo "write $f /lib/$bname"
        echo "rm /usr/lib/$bname"
        echo "write $f /usr/lib/$bname"
      fi
    done
    for f in "$STAGING"/usr/lib/*; do
      if [ -f "$f" ]; then
        bname=$(basename "$f")
        echo "rm /usr/lib/$bname"
        echo "write $f /usr/lib/$bname"
        echo "rm /lib/$bname"
        echo "write $f /lib/$bname"
      fi
    done
    for f in "$STAGING"/lib64/*; do
      if [ -f "$f" ]; then
        bname=$(basename "$f")
        echo "rm /lib64/$bname"
        echo "write $f /lib64/$bname"
      fi
    done
    for f in "$STAGING"/etc/*; do
      if [ -f "$f" ]; then
        bname=$(basename "$f")
        echo "rm /etc/$bname"
        echo "write $f /etc/$bname"
      fi
    done
    for f in "$STAGING"/etc/ssh/*; do
      if [ -f "$f" ]; then
        bname=$(basename "$f")
        echo "rm /etc/ssh/$bname"
        echo "write $f /etc/ssh/$bname"
      fi
    done
    if [ -f "$STAGING/etc/ssl/certs/ca-certificates.crt" ]; then
      echo "rm /etc/ssl/certs/ca-certificates.crt"
      echo "write $STAGING/etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt"
    fi
    if [ -f "$STAGING/usr/lib/locale/locale-archive" ]; then
      echo "mkdir /usr/lib/locale"
      echo "rm /usr/lib/locale/locale-archive"
      echo "write $STAGING/usr/lib/locale/locale-archive /usr/lib/locale/locale-archive"
    fi
for bin in curl wget ssh sshd ssh-keygen mpv draginit mywima zellij foot sdl_vulkan_test pure_vulkan_test dbus-daemon dbus-send xdg-portal aplay arecord amixer speaker-test pipewire wireplumber pipewire-pulse pw-cat pw-cli; do
      if [ -f "$STAGING/usr/bin/$bin" ]; then
        echo "rm /usr/bin/$bin"
        echo "write $STAGING/usr/bin/$bin /usr/bin/$bin"
      fi
      if [ -f "$STAGING/usr/sbin/$bin" ]; then
        echo "rm /usr/sbin/$bin"
        echo "write $STAGING/usr/sbin/$bin /usr/sbin/$bin"
      fi
    done
    # zellij and foot also need to be reachable as bare names, since users
    # type `zellij` not `/usr/bin/zellij` and the guest shell does not always
    # have /usr/bin on its PATH.
    for bin in zellij foot; do
      if [ -f "$STAGING/usr/bin/$bin" ]; then
        echo "rm /bin/$bin"
        echo "write $STAGING/usr/bin/$bin /bin/$bin"
      fi
    done
    for bin in draginit mywima; do
      if [ -f "$STAGING/usr/bin/$bin" ]; then
        echo "rm /bin/$bin"
        echo "write $STAGING/usr/bin/$bin /bin/$bin"
      fi
    done
    if [ -d "$STAGING/usr/lib/ssh" ]; then
      for f in "$STAGING"/usr/lib/ssh/*; do
        if [ -f "$f" ]; then
          bname=$(basename "$f")
          echo "rm /usr/lib/ssh/$bname"
          echo "write $f /usr/lib/ssh/$bname"
        fi
      done
    fi
  } > "$BATCH"
  # Remove every draginit unit already in the image before writing the new set.
  # disk.img is persistent and this sync only ever *adds* files, so renaming a
  # unit (e.g. 50-mywima -> 10-mywima, so the compositor starts before its
  # clients) left the old file in place and draginit started each daemon twice.
  # Staging is cleaned, but the image was not, so the deletion belongs here.
  for u in $(debugfs -R "ls /etc/draginit" "$ROOT/disk.img" 2>/dev/null \
             | tr ' ' '\n' | grep '\.toml$'); do
    echo "rm /etc/draginit/$u"
  done >> "$BATCH"
  for u in "$STAGING"/etc/draginit/*.toml; do
    [ -f "$u" ] || continue
    bname=$(basename "$u")
    echo "rm /etc/draginit/$bname"
    echo "write $u /etc/draginit/$bname"
  done >> "$BATCH"
  # zellij's config tree. It probes layouts/ and themes/ while loading its
  # config and rewriting it to disk; both were absent, so the probe returned
  # ENOENT and the session stopped dead before it created a pane. Stage a
  # minimal layout (one tab, one pane) so "spawning tabs" has something to
  # spawn, and a themes dir so the listing succeeds.
  #
  # The files are written here rather than pre-seeded into $STAGING, because
  # build-fs cleans $STAGING on entry -- anything copied in beforehand is gone
  # by the time this runs.
  ZJ_SRC="$ROOT/initrd/zellij-config"
  if [ -d "$ZJ_SRC" ]; then
    echo "mkdir /root/.config/zellij"
    for sub in layouts themes; do
      [ -d "$ZJ_SRC/$sub" ] || continue
      echo "mkdir /root/.config/zellij/$sub"
      for f in "$ZJ_SRC/$sub"/*; do
        [ -f "$f" ] || continue
        bname=$(basename "$f")
        echo "rm /root/.config/zellij/$sub/$bname"
        echo "write $f /root/.config/zellij/$sub/$bname"
      done
    done
  fi >> "$BATCH"
  debugfs -w -f "$BATCH" "$ROOT/disk.img" >/dev/null 2>&1 || true
  rm -f "$BATCH"

MANIFEST="$ROOT/.build-fs.manifest"
MANIFEST_OLD="$ROOT/.build-fs.manifest.old"
if [ -f "$MANIFEST" ]; then
    cp -p "$MANIFEST" "$MANIFEST_OLD"
else
    rm -f "$MANIFEST_OLD"
    touch "$MANIFEST_OLD"
fi
find "$STAGING" -type f -print0 | xargs -0 -r sha256sum 2>/dev/null | \
  awk '{ h=$1; $1=""; sub(/^  /,""); p=$0; printf "%s\t", p }' >/dev/null 2>&1
# Build manifest as "<rel>\t<size>\t<sha256>" for every staged file.
: # (already generated)
find "$STAGING" -type f -print0 | xargs -0 -r sha256sum 2>/dev/null | while read -r h p; do
    rel="${p#$STAGING/}"
    case "$rel" in
        etc/ssh/ssh_host_*|root/Pictures/Wallpapers/*|proc/*|dev/*|sys/*) continue ;;
    esac
    sz=$(stat -c '%s' "$p" 2>/dev/null || echo 0)
    printf '%s\t%s\t%s\n' "$rel" "$sz" "$h"
done | LC_ALL=C sort > "$ROOT/.build-fs.manifest.new"
_manifest_sig() {
    awk -F '\t' -v p="$1" '$1==p {print $2":"$3; exit}' "$MANIFEST_OLD"
}
_manifest_sig_new() {
    awk -F '\t' -v p="$1" '$1==p {print $2":"$3; exit}' "$ROOT/.build-fs.manifest.new"
}

  for f in fork_test clone_test sig_test test_musl test_tokio hello \
           libinput_test libevdev_test input_test epoll_input_test vfstest \
           mmap_test eventfd_test epoll_test timerfd_test \
           mprotect_fault_test fpu_concurrency_test efault_security_test \
           ext4test privdrop gbmscanout rainbow drm_event_test netprobe dnstest virglprobe gbmprobe gbm_test_guest mesarepro ptyprobe pcm_test vkprobe drmprobe \
           ptyrun socktest sleep player kms_player mkdirtest ptyposixtest jobctltest jobctltest_stop rawttytest tmpfsw procstress tmux mounttest flip_test vulkansmoke ptrace_test; do
    [ -f "$ROOT/initrd/$f" ] && {
      _r="initrd/$f"
      _sig=$(_manifest_sig     "$_r")
      _nsig=$(_manifest_sig_new "$_r")
      if [ "$_sig" = "$_nsig" ] && [ -n "$_sig" ]; then
        : # unchanged, skip
      else
        debugfs -w -R "rm /usr/bin/$f" "$ROOT/disk.img" >/dev/null 2>&1 || true
        debugfs -w -R "write $ROOT/initrd/$f /usr/bin/$f" "$ROOT/disk.img" >/dev/null 2>&1 || true
        debugfs -w -R "rm /$f" "$ROOT/disk.img" >/dev/null 2>&1 || true
        debugfs -w -R "write $ROOT/initrd/$f /$f" "$ROOT/disk.img" >/dev/null 2>&1 || true
      fi
    }
  done
  debugfs -w -R "mkdir /usr/share" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /usr/share/libinput" "$ROOT/disk.img" >/dev/null 2>&1 || true
  if [ -d "$STAGING/usr/share/libinput" ]; then
    for f in $(ls "$STAGING/usr/share/libinput"); do
      debugfs -w -R "write $STAGING/usr/share/libinput/$f /usr/share/libinput/$f" "$ROOT/disk.img" >/dev/null 2>&1 || true
    done
  fi
  debugfs -w -R "mkdir /usr/share/glvnd" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /usr/share/glvnd/egl_vendor.d" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /usr/share/vulkan" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /usr/share/vulkan/icd.d" "$ROOT/disk.img" >/dev/null 2>&1 || true
  [ -f "$STAGING/usr/share/vulkan/icd.d/virtio_icd.json" ] && { debugfs -w -R "rm /usr/share/vulkan/icd.d/virtio_icd.json" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/usr/share/vulkan/icd.d/virtio_icd.json /usr/share/vulkan/icd.d/virtio_icd.json" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  debugfs -w -R "mkdir /etc/mpv" "$ROOT/disk.img" >/dev/null 2>&1 || true
  [ -f "$STAGING/etc/mpv/mpv.conf" ] && { debugfs -w -R "rm /etc/mpv/mpv.conf" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/etc/mpv/mpv.conf /etc/mpv/mpv.conf" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  debugfs -w -R "mkdir /etc/fonts" "$ROOT/disk.img" >/dev/null 2>&1 || true
  [ -f "$STAGING/etc/fonts/fonts.conf" ] && { debugfs -w -R "rm /etc/fonts/fonts.conf" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/etc/fonts/fonts.conf /etc/fonts/fonts.conf" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  debugfs -w -R "mkdir /usr/share/fonts" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /usr/share/fonts/truetype" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /usr/share/dbus-1" "$ROOT/disk.img" >/dev/null 2>&1 || true
  [ -f "$STAGING/usr/share/dbus-1/session.conf" ] && { debugfs -w -R "rm /usr/share/dbus-1/session.conf" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/usr/share/dbus-1/session.conf /usr/share/dbus-1/session.conf" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  # ALSA config
  if [ -d "$STAGING/usr/share/alsa" ]; then
    # Create directory structure first
    find "$STAGING/usr/share/alsa" -type d | while read d; do
        rel="${d#$STAGING/}"
        [ "$rel" = "usr/share/alsa" ] && continue
        debugfs -w -R "mkdir $rel" "$ROOT/disk.img" >/dev/null 2>&1 || true
    done
    # Then write all files
    find "$STAGING/usr/share/alsa" -type f | while read f; do
        rel="${f#$STAGING/}"
        debugfs -w -R "write $f $rel" "$ROOT/disk.img" >/dev/null 2>&1 || true
    done
  fi
  debugfs -w -R "mkdir /usr/share/fonts/truetype/noto" "$ROOT/disk.img" >/dev/null 2>&1 || true
  [ -f "$STAGING/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf" ] && { debugfs -w -R "rm /usr/share/fonts/truetype/noto/NotoSans-Regular.ttf" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf /usr/share/fonts/truetype/noto/NotoSans-Regular.ttf" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  if [ -d "$STAGING/usr/share/glvnd/egl_vendor.d" ]; then
    for f in $(ls "$STAGING/usr/share/glvnd/egl_vendor.d"); do
      debugfs -w -R "rm /usr/share/glvnd/egl_vendor.d/$f" "$ROOT/disk.img" >/dev/null 2>&1 || true
      debugfs -w -R "write $STAGING/usr/share/glvnd/egl_vendor.d/$f /usr/share/glvnd/egl_vendor.d/$f" "$ROOT/disk.img" >/dev/null 2>&1 || true
    done
  fi
  # Sync XKB keymap config (xkeyboard-config-2) - required by libxkbcommon
  if [ -d "$STAGING/usr/share/xkeyboard-config-2" ]; then
    debugfs -w -R "mkdir /usr/share/xkeyboard-config-2" "$ROOT/disk.img" >/dev/null 2>&1 || true
    for d in compat geometry keycodes rules symbols types; do
      debugfs -w -R "mkdir /usr/share/xkeyboard-config-2/$d" "$ROOT/disk.img" >/dev/null 2>&1 || true
    done
    cp -r "$STAGING/usr/share/xkeyboard-config-2/." /tmp/xkb-staging/ 2>/dev/null || (rm -rf /tmp/xkb-staging && cp -r "$STAGING/usr/share/xkeyboard-config-2" /tmp/xkb-staging)
    find /tmp/xkb-staging -type f | while read -r f; do
      rel="${f#/tmp/xkb-staging/}"
      debugfs -w -R "rm /usr/share/xkeyboard-config-2/$rel" "$ROOT/disk.img" >/dev/null 2>&1 || true
      debugfs -w -R "write $f /usr/share/xkeyboard-config-2/$rel" "$ROOT/disk.img" >/dev/null 2>&1 || true
    done
    rm -rf /tmp/xkb-staging
  fi
  if [ -d "$STAGING/usr/share/X11/xkb" ]; then
    debugfs -w -R "mkdir /usr/share/X11/xkb" "$ROOT/disk.img" >/dev/null 2>&1 || true
    # Use batch file for reliability with many files
    BATCH="/tmp/xkb-sync-$$.cmd"
    { echo "mkdir /usr/share/X11/xkb";
      find "$STAGING/usr/share/X11/xkb" -type f | while read -r f; do
        rel="${f#$STAGING/usr/share/X11/xkb/}"
        d="$(dirname "/usr/share/X11/xkb/$rel")"
        echo "mkdir $d" 2>/dev/null
        echo "rm /usr/share/X11/xkb/$rel"
        echo "write $f /usr/share/X11/xkb/$rel"
      done; } > "$BATCH"
    debugfs -w -f "$BATCH" "$ROOT/disk.img" >/dev/null 2>&1 || true
    rm -f "$BATCH"
  fi
  # Sync cursor themes - required for compositor cursor rendering
  # Terminfo database (for tmux, ncurses apps)
  if [ -d "$STAGING/usr/share/terminfo" ]; then
    debugfs -w -R "mkdir /usr/share/terminfo" "$ROOT/disk.img" >/dev/null 2>&1 || true
    for dir in $(ls "$STAGING/usr/share/terminfo"); do
      debugfs -w -R "mkdir /usr/share/terminfo/$dir" "$ROOT/disk.img" >/dev/null 2>&1 || true
      for f in $(ls "$STAGING/usr/share/terminfo/$dir"); do
        debugfs -w -R "write $STAGING/usr/share/terminfo/$dir/$f /usr/share/terminfo/$dir/$f" "$ROOT/disk.img" >/dev/null 2>&1 || true
      done
    done
  fi
  # termcap (legacy)
  if [ -f "$STAGING/etc/termcap" ]; then
    debugfs -w -R "rm /etc/termcap" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $STAGING/etc/termcap /etc/termcap" "$ROOT/disk.img" >/dev/null 2>&1 || true
  fi
  if [ -d "$STAGING/usr/share/icons" ]; then
    BATCH="/tmp/cursor-sync-$$.cmd"
    { echo "mkdir /usr/share/icons";
      for theme in default Adwaita; do
        if [ -d "$STAGING/usr/share/icons/$theme" ]; then
          echo "mkdir /usr/share/icons/$theme"
          echo "mkdir /usr/share/icons/$theme/cursors"
          [ -f "$STAGING/usr/share/icons/$theme/index.theme" ] && echo "write $STAGING/usr/share/icons/$theme/index.theme /usr/share/icons/$theme/index.theme"
          if [ -d "$STAGING/usr/share/icons/$theme/cursors" ]; then
            find "$STAGING/usr/share/icons/$theme/cursors" -type f | while read -r f; do
              base="$(basename "$f")"
              echo "rm /usr/share/icons/$theme/cursors/$base"
              echo "write $f /usr/share/icons/$theme/cursors/$base"
            done
          fi
        fi
      done; } > "$BATCH"
    debugfs -w -f "$BATCH" "$ROOT/disk.img" >/dev/null 2>&1 || true
    rm -f "$BATCH"
  fi
  debugfs -w -R "rm /test_mode" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /run" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /root" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /root/.config" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /root/.config/mywima" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /root/.config/mwm" "$ROOT/disk.img" >/dev/null 2>&1 || true
  [ -f "$STAGING"/root/.config/mywima/config.toml ] && { debugfs -w -R "rm /root/.config/mywima/config.toml" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/root/.config/mywima/config.toml /root/.config/mywima/config.toml" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  [ -f "$STAGING"/root/.config/mwm/config.toml ] && { debugfs -w -R "rm /root/.config/mwm/config.toml" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/root/.config/mwm/config.toml /root/.config/mwm/config.toml" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  # Wallpaper sync to ext2 disk
  debugfs -w -R "mkdir /root/Pictures" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /root/Pictures/Wallpapers" "$ROOT/disk.img" >/dev/null 2>&1 || true
  if [ -f "$STAGING/root/Pictures/Wallpapers/random_wallpaper.png" ]; then
    debugfs -w -R "rm /root/Pictures/Wallpapers/random_wallpaper.png" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $STAGING/root/Pictures/Wallpapers/random_wallpaper.png /root/Pictures/Wallpapers/random_wallpaper.png" "$ROOT/disk.img" >/dev/null 2>&1 || true
  fi
  if [ -f "$STAGING/root/Pictures/Wallpapers/random_wallpaper.jpg" ]; then
    debugfs -w -R "rm /root/Pictures/Wallpapers/random_wallpaper.jpg" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $STAGING/root/Pictures/Wallpapers/random_wallpaper.jpg /root/Pictures/Wallpapers/random_wallpaper.jpg" "$ROOT/disk.img" >/dev/null 2>&1 || true
  fi
  if [ -f "$STAGING/bin/sh" ]; then debugfs -w -R "rm /bin/sh" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/bin/sh /bin/sh" "$ROOT/disk.img" >/dev/null 2>&1 || true; fi
  if [ -f "$STAGING/bin/bash" ]; then debugfs -w -R "rm /bin/bash" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/bin/bash /bin/bash" "$ROOT/disk.img" >/dev/null 2>&1 || true; fi
  if [ -f "$STAGING/usr/bin/bash" ]; then debugfs -w -R "rm /usr/bin/bash" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/usr/bin/bash /usr/bin/bash" "$ROOT/disk.img" >/dev/null 2>&1 || true; fi
  if [ -f "$STAGING/bin/dash" ]; then debugfs -w -R "rm /bin/dash" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/bin/dash /bin/dash" "$ROOT/disk.img" >/dev/null 2>&1 || true; fi
  if [ -f "$STAGING/bin/ptyrun" ]; then debugfs -w -R "rm /bin/ptyrun" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/bin/ptyrun /bin/ptyrun" "$ROOT/disk.img" >/dev/null 2>&1 || true; fi
  for applet in busybox mount mkdir umount; do
    if [ -f "$STAGING/bin/$applet" ]; then debugfs -w -R "rm /bin/$applet" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/bin/$applet /bin/$applet" "$ROOT/disk.img" >/dev/null 2>&1 || true; fi
  done
  if [ -f "$STAGING/bin/sleep" ]; then debugfs -w -R "rm /bin/sleep" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/bin/sleep /bin/sleep" "$ROOT/disk.img" >/dev/null 2>&1 || true; fi
  [ -f "$STAGING/bin/ls" ] && { debugfs -w -R "rm /bin/ls" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/bin/ls /bin/ls" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  [ -f "$STAGING/usr/bin/seatd" ] && { debugfs -w -R "rm /usr/bin/seatd" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/usr/bin/seatd /usr/bin/seatd" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  [ -f "$STAGING/bin/init.sh" ] && { debugfs -w -R "rm /bin/init.sh" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/bin/init.sh /bin/init.sh" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  [ -f "$STAGING/usr/bin/draginit" ] && { debugfs -w -R "rm /usr/bin/draginit" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/usr/bin/draginit /usr/bin/draginit" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  # Module self-test fixture (docs/module-system.md §8.1 phase 2). The kernel
  # reads this at boot to prove the ELF validator agrees with a file our own
  # toolchain produces. Staged into disk.img, not just the tar initrd, because
  # the self-check runs after mount_root.
  if [ -f "$STAGING/modules/fixture.o" ]; then
    debugfs -w -R "mkdir /modules" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "rm /modules/fixture.o" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $STAGING/modules/fixture.o /modules/fixture.o" "$ROOT/disk.img" >/dev/null 2>&1 || true
    echo "[INFO]: module fixture written to disk.img:/modules/fixture.o"
  fi
  if [ -f "$STAGING/modules/unresolvable.o" ]; then
    debugfs -w -R "mkdir /modules" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "rm /modules/unresolvable.o" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $STAGING/modules/unresolvable.o /modules/unresolvable.o" "$ROOT/disk.img" >/dev/null 2>&1 || true
    echo "[INFO]: module unresolvable fixture written to disk.img:/modules/unresolvable.o"
  fi
  if [ -f "$STAGING/modules/consumer.o" ]; then
    debugfs -w -R "mkdir /modules" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "rm /modules/consumer.o" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $STAGING/modules/consumer.o /modules/consumer.o" "$ROOT/disk.img" >/dev/null 2>&1 || true
    echo "[INFO]: module consumer fixture written to disk.img:/modules/consumer.o"
  fi
  debugfs -w -R "mkdir /etc/draginit" "$ROOT/disk.img" >/dev/null 2>&1 || true
  for f in "$STAGING"/etc/draginit/*.toml; do
    [ -f "$f" ] || continue
    bname=$(basename "$f")
    debugfs -w -R "rm /etc/draginit/$bname" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $f /etc/draginit/$bname" "$ROOT/disk.img" >/dev/null 2>&1 || true
  done
  [ -f "$STAGING/usr/bin/mywima" ] && { debugfs -w -R "rm /usr/bin/mywima" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/usr/bin/mywima /usr/bin/mywima" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  [ -f "$STAGING/usr/bin/socktest" ] && { debugfs -w -R "rm /usr/bin/socktest" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/usr/bin/socktest /usr/bin/socktest" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  [ -f "$STAGING/usr/bin/inputtest" ] && { debugfs -w -R "rm /usr/bin/inputtest" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/usr/bin/inputtest /usr/bin/inputtest" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  [ -f "$STAGING/usr/bin/kms_player" ] && { debugfs -w -R "rm /usr/bin/kms_player" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/usr/bin/kms_player /usr/bin/kms_player" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  if [ -f "$STAGING/usr/bin/player" ]; then
    debugfs -w -R "rm /usr/bin/player" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $STAGING/usr/bin/player /usr/bin/player" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "rm /usr/bin/mplayer" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $STAGING/usr/bin/player /usr/bin/mplayer" "$ROOT/disk.img" >/dev/null 2>&1 || true
  fi
  if [ -f "$STAGING/usr/bin/wl_player" ]; then
    debugfs -w -R "rm /usr/bin/wl_player" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $STAGING/usr/bin/wl_player /usr/bin/wl_player" "$ROOT/disk.img" >/dev/null 2>&1 || true
  fi
  if [ -f "$ROOT/video.mp4" ] && command -v ffmpeg >/dev/null 2>&1; then
    if [ ! -f "$ROOT/initrd/video.vid" ] || [ "$(head -c 4 "$ROOT/initrd/video.vid")" != "DRGJ" ]; then
      python3 "$ROOT/initrd/mkdrgj.py" "$ROOT/video.mp4" "$ROOT/initrd/video.vid" \
        && echo "[build-fs] DRGJ color video generated"
    fi
  fi
  if [ -f "$ROOT/initrd/video.vid" ]; then
    debugfs -w -R "rm /video.vid" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $ROOT/initrd/video.vid /video.vid" "$ROOT/disk.img" >/dev/null 2>&1 || true
  fi
  if [ -f "$ROOT/initrd/video.mp4" ]; then
    debugfs -w -R "rm /video.mp4" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $ROOT/initrd/video.mp4 /video.mp4" "$ROOT/disk.img" >/dev/null 2>&1 || true
  fi
  [ -f "$STAGING/vidtest.rgb32" ] && { debugfs -w -R "rm /vidtest.rgb32" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/vidtest.rgb32 /vidtest.rgb32" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  [ -f "$STAGING/clip.rgb32" ] && { debugfs -w -R "rm /clip.rgb32" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/clip.rgb32 /clip.rgb32" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  [ -f "$STAGING/clip.pcm" ] && { debugfs -w -R "rm /clip.pcm" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/clip.pcm /clip.pcm" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  # Sync terminal binaries + GL probes to disk.img
  for tbin in foot weston-terminal footclient kitty es2gears_wayland es2_info mpv fixed_size_test; do
    [ -f "$STAGING/usr/bin/$tbin" ] && { debugfs -w -R "rm /usr/bin/$tbin" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/usr/bin/$tbin /usr/bin/$tbin" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  done
  # Also ensure /usr/bin/foot exists on disk if we built minimal client at /tmp/foot fallback
  if [ -f "$STAGING/usr/bin/foot" ]; then
    debugfs -w -R "rm /usr/bin/foot" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $STAGING/usr/bin/foot /usr/bin/foot" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "rm /bin/foot" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $STAGING/usr/bin/foot /bin/foot" "$ROOT/disk.img" >/dev/null 2>&1 || true
  fi
  if [ -f "$STAGING/bin/foot" ]; then
    debugfs -w -R "rm /bin/foot" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $STAGING/bin/foot /bin/foot" "$ROOT/disk.img" >/dev/null 2>&1 || true
  fi
  # (curl/wget/ssh/sshd/ca-certs/resolv.conf already synced via batch block above)

  # Reconcile metadata after the debugfs writes above.
  #
  # debugfs implements `rm` + `write` as a real unlink followed by a fresh
  # allocation, so replacing a file orphans its old inode and leaves the group
  # bitmaps and descriptor counts describing the pre-write state. One -fy pass
  # reconnects or frees those inodes and rebuilds the summaries.
  #
  # This must run last. It is also what makes the image safe to ship as a stock
  # modern ext4: metadata_csum and metadata_csum_seed stay enabled (what
  # mkfs.ext4 produces by default) because the DragOS ext4 driver implements
  # those checksums on every write path -- inode, directory block, group
  # descriptor, bitmaps and superblock.
  if command -v e2fsck >/dev/null 2>&1; then
    e2fsck -fy "$ROOT/disk.img" >/dev/null 2>&1 || true
  fi
fi

# ── disk2.img: second ext4 image for mount(2) loopback testing ─────────────
DISK2="$ROOT/disk2.img"
if [ ! -f "$DISK2" ] && command -v mke2fs >/dev/null 2>&1; then
  S2=$(mktemp -d)
  printf 'MOUNT_SENTINEL\n' > "$S2/hello.txt"
  if mke2fs -q -t ext4 -F -b 4096 -d "$S2" "$DISK2" 8192 >/dev/null 2>&1; then
    echo "[build-fs] disk2.img built (8192 x 4K blocks)"
  else
    echo "[build-fs] WARNING: mke2fs failed for disk2.img"
  fi
  rm -rf "$S2"
fi
if [ -f "$DISK2" ] && [ -f "$ROOT/disk.img" ] && command -v debugfs >/dev/null 2>&1; then
  debugfs -w -R "rm /disk2.img" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "write $DISK2 /disk2.img" "$ROOT/disk.img" >/dev/null 2>&1 || true
  echo "[build-fs] disk2.img staged at /disk2.img"
fi

# Add mywima compositor
if [ -f "$STAGING/bin/mywima" ]; then
    chmod +x "$STAGING/bin/mywima" 2>/dev/null || true
    echo "[INFO]: mywima compositor staged in /bin"
fi

# Firefox browser (pre-built from Arch package)
FIREFOX_SRC=/home/jyash8/Downloads/firefox-extract
if [ -d "$FIREFOX_SRC/usr/lib/firefox" ]; then
    mkdir -p "$STAGING"/usr/lib/firefox
    cp -rp "$FIREFOX_SRC"/usr/lib/firefox/* "$STAGING"/usr/lib/firefox/
    mkdir -p "$STAGING"/usr/bin
    cat > "$STAGING"/usr/bin/firefox << 'FFEOF'
#!/bin/sh
exec /usr/lib/firefox/firefox --no-sandbox "$@"
FFEOF
    chmod +x "$STAGING"/usr/bin/firefox
    echo "[build-fs] Firefox browser staged"

    # Also write Firefox directory tree to disk.img (best-effort)
    if [ -d "$STAGING/usr/lib/firefox" ]; then
        echo "[build-fs] Staging Firefox directory tree to disk.img (large)..."
        find "$STAGING/usr/lib/firefox" -type d | while read -r d; do
            rel="${d#$STAGING/}"
            debugfs -w -R "mkdir $rel" "$ROOT/disk.img" >/dev/null 2>&1 || true
        done
        find "$STAGING/usr/lib/firefox" -type f | while read -r f; do
            rel="${f#$STAGING/}"
            _sig=$(_manifest_sig     "$rel")
            _nsig=$(_manifest_sig_new "$rel")
            if [ "$_sig" = "$_nsig" ] && [ -n "$_sig" ]; then
                : # unchanged, skip this file
            else
                debugfs -w -R "write $f $rel" "$ROOT/disk.img" >/dev/null 2>&1 || true
            fi
        done
        # Launcher
        if [ -f "$STAGING/usr/bin/firefox" ]; then
            _rel="usr/bin/firefox"
            _sig=$(_manifest_sig     "$_rel")
            _nsig=$(_manifest_sig_new "$_rel")
            if [ "$_sig" != "$_nsig" ] || [ -z "$_sig" ]; then
                debugfs -w -R "rm /usr/bin/firefox" "$ROOT/disk.img" >/dev/null 2>&1 || true
                debugfs -w -R "write $STAGING/usr/bin/firefox /usr/bin/firefox" "$ROOT/disk.img" >/dev/null 2>&1 || true
            fi
        fi
        echo "[build-fs] Firefox directory tree staged"
    fi
fi


echo "Filesystem image setup complete."
ls -lh /home/jyash8/Projects/drag_os/esp/efi/boot/initrd.tar

# ── Sync test binaries and libraries into the ext2/ext4 root disk (disk.img) ─
if [ -f "$ROOT/disk.img" ] && command -v debugfs >/dev/null; then
  img_size=$(stat -c%s "$ROOT/disk.img" 2>/dev/null || echo 0)
  if [ "$img_size" -lt 4294967296 ]; then
    qemu-img resize -f raw "$ROOT/disk.img" 4096M >/dev/null 2>&1 || true
    e2fsck -fy "$ROOT/disk.img" >/dev/null 2>&1 || true
    resize2fs "$ROOT/disk.img" >/dev/null 2>&1 || true
  fi
  debugfs -w -R "mkdir /proc" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /proc/self" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /proc/self/fd" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /dev" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /dev/dri" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /dev/input" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /dev/snd" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /lib64" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /usr" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /usr/lib" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /usr/bin" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /usr/lib/gbm" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /usr/lib/dri" "$ROOT/disk.img" >/dev/null 2>&1 || true
  if [ -d "$STAGING/usr/lib/gbm" ]; then
    for f in $(ls "$STAGING"/usr/lib/gbm); do
      debugfs -w -R "rm /usr/lib/gbm/$f" "$ROOT/disk.img" >/dev/null 2>&1 || true
      debugfs -w -R "write $STAGING/usr/lib/gbm/$f /usr/lib/gbm/$f" "$ROOT/disk.img" >/dev/null 2>&1
    done
  fi
  if [ -d "$STAGING/usr/lib/dri" ]; then
    for f in $(ls "$STAGING"/usr/lib/dri); do
      debugfs -w -R "rm /usr/lib/dri/$f" "$ROOT/disk.img" >/dev/null 2>&1 || true
      debugfs -w -R "write $STAGING/usr/lib/dri/$f /usr/lib/dri/$f" "$ROOT/disk.img" >/dev/null 2>&1
    done
  fi
  # Batch sync runtime libraries, system config, SSL certs, and networking/SSH binaries
  BATCH="/tmp/dragos-lib-etc-sync-$$.cmd"
  {
    echo "mkdir /lib"
    echo "mkdir /lib64"
    echo "mkdir /usr"
    echo "mkdir /usr/lib"
    echo "mkdir /usr/lib/ssh"
    echo "mkdir /usr/bin"
    echo "mkdir /usr/sbin"
    echo "mkdir /bin"
    echo "mkdir /sbin"
    echo "mkdir /etc"
    echo "mkdir /etc/ssh"
    echo "mkdir /etc/ssl"
    echo "mkdir /etc/ssl/certs"
    echo "mkdir /var"
    echo "mkdir /var/empty"
    echo "mkdir /run"
    echo "mkdir /run/sshd"
    echo "mkdir /usr/share"
    echo "mkdir /usr/share/empty.sshd"
    echo "mkdir /root"
    echo "mkdir /root/.ssh"
    if [ -f "$STAGING/root/.ssh/authorized_keys" ]; then
      echo "rm /root/.ssh/authorized_keys"
      echo "write $STAGING/root/.ssh/authorized_keys /root/.ssh/authorized_keys"
    fi
    if [ -f "$STAGING/root/.drirc" ]; then
      echo "rm /root/.drirc"
      echo "write $STAGING/root/.drirc /root/.drirc"
    fi

    for f in "$STAGING"/lib/*; do
      if [ -f "$f" ]; then
        bname=$(basename "$f")
        echo "rm /lib/$bname"
        echo "write $f /lib/$bname"
        echo "rm /usr/lib/$bname"
        echo "write $f /usr/lib/$bname"
      fi
    done
    for f in "$STAGING"/usr/lib/*; do
      if [ -f "$f" ]; then
        bname=$(basename "$f")
        echo "rm /usr/lib/$bname"
        echo "write $f /usr/lib/$bname"
        echo "rm /lib/$bname"
        echo "write $f /lib/$bname"
      fi
    done
    for f in "$STAGING"/lib64/*; do
      if [ -f "$f" ]; then
        bname=$(basename "$f")
        echo "rm /lib64/$bname"
        echo "write $f /lib64/$bname"
      fi
    done
    for f in "$STAGING"/etc/*; do
      if [ -f "$f" ]; then
        bname=$(basename "$f")
        echo "rm /etc/$bname"
        echo "write $f /etc/$bname"
      fi
    done
    for f in "$STAGING"/etc/ssh/*; do
      if [ -f "$f" ]; then
        bname=$(basename "$f")
        echo "rm /etc/ssh/$bname"
        echo "write $f /etc/ssh/$bname"
      fi
    done
    if [ -f "$STAGING/etc/ssl/certs/ca-certificates.crt" ]; then
      echo "rm /etc/ssl/certs/ca-certificates.crt"
      echo "write $STAGING/etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt"
    fi
    if [ -f "$STAGING/usr/lib/locale/locale-archive" ]; then
      echo "mkdir /usr/lib/locale"
      echo "rm /usr/lib/locale/locale-archive"
      echo "write $STAGING/usr/lib/locale/locale-archive /usr/lib/locale/locale-archive"
    fi
for bin in curl wget ssh sshd ssh-keygen mpv draginit mywima zellij foot sdl_vulkan_test pure_vulkan_test dbus-daemon dbus-send xdg-portal aplay arecord amixer speaker-test pipewire wireplumber pipewire-pulse pw-cat pw-cli; do
      if [ -f "$STAGING/usr/bin/$bin" ]; then
        echo "rm /usr/bin/$bin"
        echo "write $STAGING/usr/bin/$bin /usr/bin/$bin"
      fi
      if [ -f "$STAGING/usr/sbin/$bin" ]; then
        echo "rm /usr/sbin/$bin"
        echo "write $STAGING/usr/sbin/$bin /usr/sbin/$bin"
      fi
    done
    # zellij and foot also need to be reachable as bare names, since users
    # type `zellij` not `/usr/bin/zellij` and the guest shell does not always
    # have /usr/bin on its PATH.
    for bin in zellij foot; do
      if [ -f "$STAGING/usr/bin/$bin" ]; then
        echo "rm /bin/$bin"
        echo "write $STAGING/usr/bin/$bin /bin/$bin"
      fi
    done
    for bin in draginit mywima; do
      if [ -f "$STAGING/usr/bin/$bin" ]; then
        echo "rm /bin/$bin"
        echo "write $STAGING/usr/bin/$bin /bin/$bin"
      fi
    done
    if [ -d "$STAGING/usr/lib/ssh" ]; then
      for f in "$STAGING"/usr/lib/ssh/*; do
        if [ -f "$f" ]; then
          bname=$(basename "$f")
          echo "rm /usr/lib/ssh/$bname"
          echo "write $f /usr/lib/ssh/$bname"
        fi
      done
    fi
  } > "$BATCH"
  # Remove every draginit unit already in the image before writing the new set.
  # disk.img is persistent and this sync only ever *adds* files, so renaming a
  # unit (e.g. 50-mywima -> 10-mywima, so the compositor starts before its
  # clients) left the old file in place and draginit started each daemon twice.
  # Staging is cleaned, but the image was not, so the deletion belongs here.
  for u in $(debugfs -R "ls /etc/draginit" "$ROOT/disk.img" 2>/dev/null \
             | tr ' ' '\n' | grep '\.toml$'); do
    echo "rm /etc/draginit/$u"
  done >> "$BATCH"
  for u in "$STAGING"/etc/draginit/*.toml; do
    [ -f "$u" ] || continue
    bname=$(basename "$u")
    echo "rm /etc/draginit/$bname"
    echo "write $u /etc/draginit/$bname"
  done >> "$BATCH"
  # zellij's config tree. It probes layouts/ and themes/ while loading its
  # config and rewriting it to disk; both were absent, so the probe returned
  # ENOENT and the session stopped dead before it created a pane. Stage a
  # minimal layout (one tab, one pane) so "spawning tabs" has something to
  # spawn, and a themes dir so the listing succeeds.
  #
  # The files are written here rather than pre-seeded into $STAGING, because
  # build-fs cleans $STAGING on entry -- anything copied in beforehand is gone
  # by the time this runs.
  ZJ_SRC="$ROOT/initrd/zellij-config"
  if [ -d "$ZJ_SRC" ]; then
    echo "mkdir /root/.config/zellij"
    for sub in layouts themes; do
      [ -d "$ZJ_SRC/$sub" ] || continue
      echo "mkdir /root/.config/zellij/$sub"
      for f in "$ZJ_SRC/$sub"/*; do
        [ -f "$f" ] || continue
        bname=$(basename "$f")
        echo "rm /root/.config/zellij/$sub/$bname"
        echo "write $f /root/.config/zellij/$sub/$bname"
      done
    done
  fi >> "$BATCH"
  debugfs -w -f "$BATCH" "$ROOT/disk.img" >/dev/null 2>&1 || true
  rm -f "$BATCH"
  for f in fork_test clone_test sig_test test_musl test_tokio hello \
           libinput_test libevdev_test input_test epoll_input_test vfstest \
           mmap_test eventfd_test epoll_test timerfd_test \
           mprotect_fault_test fpu_concurrency_test efault_security_test \
           ext4test privdrop gbmscanout rainbow drm_event_test netprobe dnstest virglprobe gbmprobe gbm_test_guest mesarepro ptyprobe pcm_test vkprobe drmprobe \
           ptyrun socktest sleep player kms_player mkdirtest ptyposixtest jobctltest jobctltest_stop rawttytest tmpfsw procstress tmux mounttest flip_test vulkansmoke ptrace_test; do
    [ -f "$ROOT/initrd/$f" ] && {
      _r="initrd/$f"
      _sig=$(_manifest_sig     "$_r")
      _nsig=$(_manifest_sig_new "$_r")
      if [ "$_sig" = "$_nsig" ] && [ -n "$_sig" ]; then
        : # unchanged, skip
      else
        debugfs -w -R "rm /usr/bin/$f" "$ROOT/disk.img" >/dev/null 2>&1 || true
        debugfs -w -R "write $ROOT/initrd/$f /usr/bin/$f" "$ROOT/disk.img" >/dev/null 2>&1 || true
        debugfs -w -R "rm /$f" "$ROOT/disk.img" >/dev/null 2>&1 || true
        debugfs -w -R "write $ROOT/initrd/$f /$f" "$ROOT/disk.img" >/dev/null 2>&1 || true
      fi
    }
  done
  debugfs -w -R "mkdir /usr/share" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /usr/share/libinput" "$ROOT/disk.img" >/dev/null 2>&1 || true
  if [ -d "$STAGING/usr/share/libinput" ]; then
    for f in $(ls "$STAGING/usr/share/libinput"); do
      debugfs -w -R "write $STAGING/usr/share/libinput/$f /usr/share/libinput/$f" "$ROOT/disk.img" >/dev/null 2>&1 || true
    done
  fi
  debugfs -w -R "mkdir /usr/share/glvnd" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /usr/share/glvnd/egl_vendor.d" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /usr/share/vulkan" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /usr/share/vulkan/icd.d" "$ROOT/disk.img" >/dev/null 2>&1 || true
  [ -f "$STAGING/usr/share/vulkan/icd.d/virtio_icd.json" ] && { debugfs -w -R "rm /usr/share/vulkan/icd.d/virtio_icd.json" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/usr/share/vulkan/icd.d/virtio_icd.json /usr/share/vulkan/icd.d/virtio_icd.json" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  debugfs -w -R "mkdir /etc/mpv" "$ROOT/disk.img" >/dev/null 2>&1 || true
  [ -f "$STAGING/etc/mpv/mpv.conf" ] && { debugfs -w -R "rm /etc/mpv/mpv.conf" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/etc/mpv/mpv.conf /etc/mpv/mpv.conf" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  debugfs -w -R "mkdir /etc/fonts" "$ROOT/disk.img" >/dev/null 2>&1 || true
  [ -f "$STAGING/etc/fonts/fonts.conf" ] && { debugfs -w -R "rm /etc/fonts/fonts.conf" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/etc/fonts/fonts.conf /etc/fonts/fonts.conf" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  debugfs -w -R "mkdir /usr/share/fonts" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /usr/share/fonts/truetype" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /usr/share/dbus-1" "$ROOT/disk.img" >/dev/null 2>&1 || true
  [ -f "$STAGING/usr/share/dbus-1/session.conf" ] && { debugfs -w -R "rm /usr/share/dbus-1/session.conf" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/usr/share/dbus-1/session.conf /usr/share/dbus-1/session.conf" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  # ALSA config
  if [ -d "$STAGING/usr/share/alsa" ]; then
    # Create directory structure first
    find "$STAGING/usr/share/alsa" -type d | while read d; do
        rel="${d#$STAGING/}"
        [ "$rel" = "usr/share/alsa" ] && continue
        debugfs -w -R "mkdir $rel" "$ROOT/disk.img" >/dev/null 2>&1 || true
    done
    # Then write all files
    find "$STAGING/usr/share/alsa" -type f | while read f; do
        rel="${f#$STAGING/}"
        debugfs -w -R "write $f $rel" "$ROOT/disk.img" >/dev/null 2>&1 || true
    done
  fi
  debugfs -w -R "mkdir /usr/share/fonts/truetype/noto" "$ROOT/disk.img" >/dev/null 2>&1 || true
  [ -f "$STAGING/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf" ] && { debugfs -w -R "rm /usr/share/fonts/truetype/noto/NotoSans-Regular.ttf" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf /usr/share/fonts/truetype/noto/NotoSans-Regular.ttf" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  if [ -d "$STAGING/usr/share/glvnd/egl_vendor.d" ]; then
    for f in $(ls "$STAGING/usr/share/glvnd/egl_vendor.d"); do
      debugfs -w -R "rm /usr/share/glvnd/egl_vendor.d/$f" "$ROOT/disk.img" >/dev/null 2>&1 || true
      debugfs -w -R "write $STAGING/usr/share/glvnd/egl_vendor.d/$f /usr/share/glvnd/egl_vendor.d/$f" "$ROOT/disk.img" >/dev/null 2>&1 || true
    done
  fi
  # Sync XKB keymap config (xkeyboard-config-2) - required by libxkbcommon
  if [ -d "$STAGING/usr/share/xkeyboard-config-2" ]; then
    debugfs -w -R "mkdir /usr/share/xkeyboard-config-2" "$ROOT/disk.img" >/dev/null 2>&1 || true
    for d in compat geometry keycodes rules symbols types; do
      debugfs -w -R "mkdir /usr/share/xkeyboard-config-2/$d" "$ROOT/disk.img" >/dev/null 2>&1 || true
    done
    cp -r "$STAGING/usr/share/xkeyboard-config-2/." /tmp/xkb-staging/ 2>/dev/null || (rm -rf /tmp/xkb-staging && cp -r "$STAGING/usr/share/xkeyboard-config-2" /tmp/xkb-staging)
    find /tmp/xkb-staging -type f | while read -r f; do
      rel="${f#/tmp/xkb-staging/}"
      debugfs -w -R "rm /usr/share/xkeyboard-config-2/$rel" "$ROOT/disk.img" >/dev/null 2>&1 || true
      debugfs -w -R "write $f /usr/share/xkeyboard-config-2/$rel" "$ROOT/disk.img" >/dev/null 2>&1 || true
    done
    rm -rf /tmp/xkb-staging
  fi
  if [ -d "$STAGING/usr/share/X11/xkb" ]; then
    debugfs -w -R "mkdir /usr/share/X11/xkb" "$ROOT/disk.img" >/dev/null 2>&1 || true
    # Use batch file for reliability with many files
    BATCH="/tmp/xkb-sync-$$.cmd"
    { echo "mkdir /usr/share/X11/xkb";
      find "$STAGING/usr/share/X11/xkb" -type f | while read -r f; do
        rel="${f#$STAGING/usr/share/X11/xkb/}"
        d="$(dirname "/usr/share/X11/xkb/$rel")"
        echo "mkdir $d" 2>/dev/null
        echo "rm /usr/share/X11/xkb/$rel"
        echo "write $f /usr/share/X11/xkb/$rel"
      done; } > "$BATCH"
    debugfs -w -f "$BATCH" "$ROOT/disk.img" >/dev/null 2>&1 || true
    rm -f "$BATCH"
  fi
  # Sync cursor themes - required for compositor cursor rendering
  # Terminfo database (for tmux, ncurses apps)
  if [ -d "$STAGING/usr/share/terminfo" ]; then
    debugfs -w -R "mkdir /usr/share/terminfo" "$ROOT/disk.img" >/dev/null 2>&1 || true
    for dir in $(ls "$STAGING/usr/share/terminfo"); do
      debugfs -w -R "mkdir /usr/share/terminfo/$dir" "$ROOT/disk.img" >/dev/null 2>&1 || true
      for f in $(ls "$STAGING/usr/share/terminfo/$dir"); do
        debugfs -w -R "write $STAGING/usr/share/terminfo/$dir/$f /usr/share/terminfo/$dir/$f" "$ROOT/disk.img" >/dev/null 2>&1 || true
      done
    done
  fi
  # termcap (legacy)
  if [ -f "$STAGING/etc/termcap" ]; then
    debugfs -w -R "rm /etc/termcap" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $STAGING/etc/termcap /etc/termcap" "$ROOT/disk.img" >/dev/null 2>&1 || true
  fi
  if [ -d "$STAGING/usr/share/icons" ]; then
    BATCH="/tmp/cursor-sync-$$.cmd"
    { echo "mkdir /usr/share/icons";
      for theme in default Adwaita; do
        if [ -d "$STAGING/usr/share/icons/$theme" ]; then
          echo "mkdir /usr/share/icons/$theme"
          echo "mkdir /usr/share/icons/$theme/cursors"
          [ -f "$STAGING/usr/share/icons/$theme/index.theme" ] && echo "write $STAGING/usr/share/icons/$theme/index.theme /usr/share/icons/$theme/index.theme"
          if [ -d "$STAGING/usr/share/icons/$theme/cursors" ]; then
            find "$STAGING/usr/share/icons/$theme/cursors" -type f | while read -r f; do
              base="$(basename "$f")"
              echo "rm /usr/share/icons/$theme/cursors/$base"
              echo "write $f /usr/share/icons/$theme/cursors/$base"
            done
          fi
        fi
      done; } > "$BATCH"
    debugfs -w -f "$BATCH" "$ROOT/disk.img" >/dev/null 2>&1 || true
    rm -f "$BATCH"
  fi
  debugfs -w -R "rm /test_mode" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /run" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /root" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /root/.config" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /root/.config/mywima" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /root/.config/mwm" "$ROOT/disk.img" >/dev/null 2>&1 || true
  [ -f "$STAGING"/root/.config/mywima/config.toml ] && { debugfs -w -R "rm /root/.config/mywima/config.toml" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/root/.config/mywima/config.toml /root/.config/mywima/config.toml" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  [ -f "$STAGING"/root/.config/mwm/config.toml ] && { debugfs -w -R "rm /root/.config/mwm/config.toml" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/root/.config/mwm/config.toml /root/.config/mwm/config.toml" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  # Wallpaper sync to ext2 disk
  debugfs -w -R "mkdir /root/Pictures" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "mkdir /root/Pictures/Wallpapers" "$ROOT/disk.img" >/dev/null 2>&1 || true
  if [ -f "$STAGING/root/Pictures/Wallpapers/random_wallpaper.png" ]; then
    debugfs -w -R "rm /root/Pictures/Wallpapers/random_wallpaper.png" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $STAGING/root/Pictures/Wallpapers/random_wallpaper.png /root/Pictures/Wallpapers/random_wallpaper.png" "$ROOT/disk.img" >/dev/null 2>&1 || true
  fi
  if [ -f "$STAGING/root/Pictures/Wallpapers/random_wallpaper.jpg" ]; then
    debugfs -w -R "rm /root/Pictures/Wallpapers/random_wallpaper.jpg" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $STAGING/root/Pictures/Wallpapers/random_wallpaper.jpg /root/Pictures/Wallpapers/random_wallpaper.jpg" "$ROOT/disk.img" >/dev/null 2>&1 || true
  fi
  if [ -f "$STAGING/bin/sh" ]; then debugfs -w -R "rm /bin/sh" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/bin/sh /bin/sh" "$ROOT/disk.img" >/dev/null 2>&1 || true; fi
  if [ -f "$STAGING/bin/bash" ]; then debugfs -w -R "rm /bin/bash" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/bin/bash /bin/bash" "$ROOT/disk.img" >/dev/null 2>&1 || true; fi
  if [ -f "$STAGING/usr/bin/bash" ]; then debugfs -w -R "rm /usr/bin/bash" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/usr/bin/bash /usr/bin/bash" "$ROOT/disk.img" >/dev/null 2>&1 || true; fi
  if [ -f "$STAGING/bin/dash" ]; then debugfs -w -R "rm /bin/dash" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/bin/dash /bin/dash" "$ROOT/disk.img" >/dev/null 2>&1 || true; fi
  if [ -f "$STAGING/bin/ptyrun" ]; then debugfs -w -R "rm /bin/ptyrun" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/bin/ptyrun /bin/ptyrun" "$ROOT/disk.img" >/dev/null 2>&1 || true; fi
  for applet in busybox mount mkdir umount; do
    if [ -f "$STAGING/bin/$applet" ]; then debugfs -w -R "rm /bin/$applet" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/bin/$applet /bin/$applet" "$ROOT/disk.img" >/dev/null 2>&1 || true; fi
  done
  if [ -f "$STAGING/bin/sleep" ]; then debugfs -w -R "rm /bin/sleep" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/bin/sleep /bin/sleep" "$ROOT/disk.img" >/dev/null 2>&1 || true; fi
  [ -f "$STAGING/bin/ls" ] && { debugfs -w -R "rm /bin/ls" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/bin/ls /bin/ls" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  [ -f "$STAGING/usr/bin/seatd" ] && { debugfs -w -R "rm /usr/bin/seatd" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/usr/bin/seatd /usr/bin/seatd" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  [ -f "$STAGING/bin/init.sh" ] && { debugfs -w -R "rm /bin/init.sh" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/bin/init.sh /bin/init.sh" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  [ -f "$STAGING/usr/bin/draginit" ] && { debugfs -w -R "rm /usr/bin/draginit" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/usr/bin/draginit /usr/bin/draginit" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  # Module self-test fixture (docs/module-system.md §8.1 phase 2). The kernel
  # reads this at boot to prove the ELF validator agrees with a file our own
  # toolchain produces. Staged into disk.img, not just the tar initrd, because
  # the self-check runs after mount_root.
  if [ -f "$STAGING/modules/fixture.o" ]; then
    debugfs -w -R "mkdir /modules" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "rm /modules/fixture.o" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $STAGING/modules/fixture.o /modules/fixture.o" "$ROOT/disk.img" >/dev/null 2>&1 || true
    echo "[INFO]: module fixture written to disk.img:/modules/fixture.o"
  fi
  if [ -f "$STAGING/modules/unresolvable.o" ]; then
    debugfs -w -R "mkdir /modules" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "rm /modules/unresolvable.o" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $STAGING/modules/unresolvable.o /modules/unresolvable.o" "$ROOT/disk.img" >/dev/null 2>&1 || true
    echo "[INFO]: module unresolvable fixture written to disk.img:/modules/unresolvable.o"
  fi
  if [ -f "$STAGING/modules/consumer.o" ]; then
    debugfs -w -R "mkdir /modules" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "rm /modules/consumer.o" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $STAGING/modules/consumer.o /modules/consumer.o" "$ROOT/disk.img" >/dev/null 2>&1 || true
    echo "[INFO]: module consumer fixture written to disk.img:/modules/consumer.o"
  fi
  debugfs -w -R "mkdir /etc/draginit" "$ROOT/disk.img" >/dev/null 2>&1 || true
  for f in "$STAGING"/etc/draginit/*.toml; do
    [ -f "$f" ] || continue
    bname=$(basename "$f")
    debugfs -w -R "rm /etc/draginit/$bname" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $f /etc/draginit/$bname" "$ROOT/disk.img" >/dev/null 2>&1 || true
  done
  [ -f "$STAGING/usr/bin/mywima" ] && { debugfs -w -R "rm /usr/bin/mywima" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/usr/bin/mywima /usr/bin/mywima" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  [ -f "$STAGING/usr/bin/socktest" ] && { debugfs -w -R "rm /usr/bin/socktest" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/usr/bin/socktest /usr/bin/socktest" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  [ -f "$STAGING/usr/bin/inputtest" ] && { debugfs -w -R "rm /usr/bin/inputtest" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/usr/bin/inputtest /usr/bin/inputtest" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  [ -f "$STAGING/usr/bin/kms_player" ] && { debugfs -w -R "rm /usr/bin/kms_player" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/usr/bin/kms_player /usr/bin/kms_player" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  if [ -f "$STAGING/usr/bin/player" ]; then
    debugfs -w -R "rm /usr/bin/player" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $STAGING/usr/bin/player /usr/bin/player" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "rm /usr/bin/mplayer" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $STAGING/usr/bin/player /usr/bin/mplayer" "$ROOT/disk.img" >/dev/null 2>&1 || true
  fi
  if [ -f "$STAGING/usr/bin/wl_player" ]; then
    debugfs -w -R "rm /usr/bin/wl_player" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $STAGING/usr/bin/wl_player /usr/bin/wl_player" "$ROOT/disk.img" >/dev/null 2>&1 || true
  fi
  if [ -f "$ROOT/video.mp4" ] && command -v ffmpeg >/dev/null 2>&1; then
    if [ ! -f "$ROOT/initrd/video.vid" ] || [ "$(head -c 4 "$ROOT/initrd/video.vid")" != "DRGJ" ]; then
      python3 "$ROOT/initrd/mkdrgj.py" "$ROOT/video.mp4" "$ROOT/initrd/video.vid" \
        && echo "[build-fs] DRGJ color video generated"
    fi
  fi
  if [ -f "$ROOT/initrd/video.vid" ]; then
    debugfs -w -R "rm /video.vid" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $ROOT/initrd/video.vid /video.vid" "$ROOT/disk.img" >/dev/null 2>&1 || true
  fi
  if [ -f "$ROOT/initrd/video.mp4" ]; then
    debugfs -w -R "rm /video.mp4" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $ROOT/initrd/video.mp4 /video.mp4" "$ROOT/disk.img" >/dev/null 2>&1 || true
  fi
  [ -f "$STAGING/vidtest.rgb32" ] && { debugfs -w -R "rm /vidtest.rgb32" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/vidtest.rgb32 /vidtest.rgb32" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  [ -f "$STAGING/clip.rgb32" ] && { debugfs -w -R "rm /clip.rgb32" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/clip.rgb32 /clip.rgb32" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  [ -f "$STAGING/clip.pcm" ] && { debugfs -w -R "rm /clip.pcm" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/clip.pcm /clip.pcm" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  # Sync terminal binaries + GL probes to disk.img
  for tbin in foot weston-terminal footclient kitty es2gears_wayland es2_info mpv fixed_size_test; do
    [ -f "$STAGING/usr/bin/$tbin" ] && { debugfs -w -R "rm /usr/bin/$tbin" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $STAGING/usr/bin/$tbin /usr/bin/$tbin" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
  done
  # Also ensure /usr/bin/foot exists on disk if we built minimal client at /tmp/foot fallback
  if [ -f "$STAGING/usr/bin/foot" ]; then
    debugfs -w -R "rm /usr/bin/foot" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $STAGING/usr/bin/foot /usr/bin/foot" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "rm /bin/foot" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $STAGING/usr/bin/foot /bin/foot" "$ROOT/disk.img" >/dev/null 2>&1 || true
  fi
  if [ -f "$STAGING/bin/foot" ]; then
    debugfs -w -R "rm /bin/foot" "$ROOT/disk.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $STAGING/bin/foot /bin/foot" "$ROOT/disk.img" >/dev/null 2>&1 || true
  fi
  # (curl/wget/ssh/sshd/ca-certs/resolv.conf already synced via batch block above)

  # Reconcile metadata after the debugfs writes above.
  #
  # debugfs implements `rm` + `write` as a real unlink followed by a fresh
  # allocation, so replacing a file orphans its old inode and leaves the group
  # bitmaps and descriptor counts describing the pre-write state. One -fy pass
  # reconnects or frees those inodes and rebuilds the summaries.
  #
  # This must run last. It is also what makes the image safe to ship as a stock
  # modern ext4: metadata_csum and metadata_csum_seed stay enabled (what
  # mkfs.ext4 produces by default) because the DragOS ext4 driver implements
  # those checksums on every write path -- inode, directory block, group
  # descriptor, bitmaps and superblock.
  if command -v e2fsck >/dev/null 2>&1; then
    e2fsck -fy "$ROOT/disk.img" >/dev/null 2>&1 || true
  fi
fi

# ── disk2.img: second ext4 image for mount(2) loopback testing ─────────────
DISK2="$ROOT/disk2.img"
if [ ! -f "$DISK2" ] && command -v mke2fs >/dev/null 2>&1; then
  S2=$(mktemp -d)
  printf 'MOUNT_SENTINEL\n' > "$S2/hello.txt"
  if mke2fs -q -t ext4 -F -b 4096 -d "$S2" "$DISK2" 8192 >/dev/null 2>&1; then
    echo "[build-fs] disk2.img built (8192 x 4K blocks)"
  else
    echo "[build-fs] WARNING: mke2fs failed for disk2.img"
  fi
  rm -rf "$S2"
fi
if [ -f "$DISK2" ] && [ -f "$ROOT/disk.img" ] && command -v debugfs >/dev/null 2>&1; then
  debugfs -w -R "rm /disk2.img" "$ROOT/disk.img" >/dev/null 2>&1 || true
  debugfs -w -R "write $DISK2 /disk2.img" "$ROOT/disk.img" >/dev/null 2>&1 || true
  echo "[build-fs] disk2.img staged at /disk2.img"
fi

# Add mywima compositor
if [ -f "$STAGING/bin/mywima" ]; then
    chmod +x "$STAGING/bin/mywima" 2>/dev/null || true
    echo "[INFO]: mywima compositor staged in /bin"
fi

# Firefox browser (pre-built from Arch package)
FIREFOX_SRC=/home/jyash8/Downloads/firefox-extract
if [ -d "$FIREFOX_SRC/usr/lib/firefox" ]; then
    mkdir -p "$STAGING"/usr/lib/firefox
    cp -rp "$FIREFOX_SRC"/usr/lib/firefox/* "$STAGING"/usr/lib/firefox/
    mkdir -p "$STAGING"/usr/bin
    cat > "$STAGING"/usr/bin/firefox << 'FFEOF'
#!/bin/sh
exec /usr/lib/firefox/firefox --no-sandbox "$@"
FFEOF
    chmod +x "$STAGING"/usr/bin/firefox
    echo "[build-fs] Firefox browser staged"

    # Also write Firefox directory tree to disk.img (best-effort)
    if [ -d "$STAGING/usr/lib/firefox" ]; then
        echo "[build-fs] Staging Firefox directory tree to disk.img (large)..."
        find "$STAGING/usr/lib/firefox" -type d | while read -r d; do
            rel="${d#$STAGING/}"
            debugfs -w -R "mkdir $rel" "$ROOT/disk.img" >/dev/null 2>&1 || true
        done
        find "$STAGING/usr/lib/firefox" -type f | while read -r f; do
            rel="${f#$STAGING/}"
            _sig=$(_manifest_sig     "$rel")
            _nsig=$(_manifest_sig_new "$rel")
            if [ "$_sig" = "$_nsig" ] && [ -n "$_sig" ]; then
                : # unchanged, skip this file
            else
                debugfs -w -R "write $f $rel" "$ROOT/disk.img" >/dev/null 2>&1 || true
            fi
        done
        # Launcher
        if [ -f "$STAGING/usr/bin/firefox" ]; then
            _rel="usr/bin/firefox"
            _sig=$(_manifest_sig     "$_rel")
            _nsig=$(_manifest_sig_new "$_rel")
            if [ "$_sig" != "$_nsig" ] || [ -z "$_sig" ]; then
                debugfs -w -R "rm /usr/bin/firefox" "$ROOT/disk.img" >/dev/null 2>&1 || true
                debugfs -w -R "write $STAGING/usr/bin/firefox /usr/bin/firefox" "$ROOT/disk.img" >/dev/null 2>&1 || true
            fi
        fi
        echo "[build-fs] Firefox directory tree staged"
    fi
fi

# ── Differential manifest (only apply changed files to disk.img) ─────────────
# Save the previous manifest (if any) and generate a fresh one from the staging
# tree.  Each line is: <relative_path>\t<size>\t<mtime>.  A file is skipped on
# the write into disk.img when its previous entry matches the new one, so a
# re-run with nothing changed is effectively instant.
MANIFEST="$ROOT/.build-fs.manifest"
MANIFEST_OLD="$ROOT/.build-fs.manifest.old"
if [ -f "$MANIFEST" ]; then
    cp -p "$MANIFEST" "$MANIFEST_OLD"
else
    rm -f "$MANIFEST_OLD"
    touch "$MANIFEST_OLD"
fi

# Stage git-managed kernel? no.  Build the new manifest (relative paths only).
find "$STAGING" -type f -print0 | xargs -0 -r sha256sum 2>/dev/null | \
  awk '{ h=$1; $1=""; sub(/^  /,""); p=$0; printf "%s\t", p }' >/dev/null 2>&1
# Build manifest as "<rel>\t<size>\t<sha256>" for every staged file.
: # (already generated)
# Manifest already generated earlier this run.

# Helper: look up a file's signature in manifest given its relative path.
_manifest_sig() {
    # arg1 = relative path
    awk -F '\t' -v p="$1" '$1==p {print $2":"$3; exit}' "$MANIFEST_OLD"
}
_manifest_sig_new() {
    awk -F '\t' -v p="$1" '$1==p {print $2":"$3; exit}' "$ROOT/.build-fs.manifest.new"
}

# Deletions: paths present in the OLD manifest but absent now -> rm from disk.img.
if [ -f "$MANIFEST" ]; then
    comm -23 <(awk -F '\t' '{print $1}' "$MANIFEST_OLD") \
             <(awk -F '\t' '{print $1}' "$ROOT/.build-fs.manifest.new") 2>/dev/null \
    | while read -r p; do
        [ -z "$p" ] && continue
        debugfs -w -R "rm /$p" "$ROOT/disk.img" >/dev/null 2>&1 || true
    done
fi

# Move new manifest into place at end (rename happens below after all writes).


# Every binary referenced by a draginit unit, plus the compositor, must exist.
#
# A unit whose path is missing does not fail the build — it fails at runtime,
# once per boot, as `[execve] lookup failed for /usr/bin/ptyrun` and
# `[draginit] shell (pid 6) exited (exit 127)`. That cost a long debugging
# detour: the session had no shell and no terminal, so the compositor was the
# only thing running and it looked like a graphics fault. Derive the list from
# the units themselves so it cannot drift from what is actually started.
#
# The check is a warning, not an error: a host legitimately may lack a
# terminal. But it prints the exact missing paths and the units that want them,
# at build time, where someone is actually looking.
MISSING=""
for unit in "$STAGING"/etc/draginit/*.toml; do
  [ -f "$unit" ] || continue
  upath=$(sed -n 's/^path[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' "$unit" | head -1)
  [ -n "$upath" ] || continue
  [ -x "$STAGING$upath" ] || MISSING="$MISSING\n  $(basename "$unit") -> $upath"
done
if [ -n "$MISSING" ]; then
  echo ""
  echo "[build-fs] WARNING: draginit units reference binaries that are NOT staged:" >&2
  printf "$MISSING\n" >&2
  echo "[build-fs]   These will fail at exec with 'lookup failed' and exit 127." >&2
  echo "[build-fs]   Either build the binary, or remove the unit from the image." >&2
  echo ""
fi
