#!/usr/bin/env bash
set -euo pipefail
ROOT="$(pwd)"
DISK="$ROOT/disk.img"
[ -f "$DISK" ] || truncate -s 8M "$DISK"

qemu-system-x86_64 -enable-kvm -smp 2 \
    -m 2G \
    -drive if=pflash,format=raw,readonly=on,file="$ROOT/OVMF_CODE.fd" \
    -drive if=pflash,format=raw,readonly=on,file="$ROOT/OVMF_VARS.fd" \
    -drive format=raw,file=fat:rw:"$ROOT/esp" \
    -drive file="$DISK",format=raw,if=none,id=vd0 \
    -device virtio-blk-pci,drive=vd0 \
    -device edu \
    -debugcon file:uefi_debug.log \
    -global isa-debugcon.iobase=0x402 \
    -serial mon:stdio \
    -device virtio-vga,xres=1280,yres=800 \
    -no-reboot \
    "$@"
