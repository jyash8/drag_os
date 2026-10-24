#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
KERNEL_BIN="$1"

# Copy the test binary to the ESP so the bootloader can load it
cp "$KERNEL_BIN" "$ROOT/esp/efi/boot/kernel.elf"

# Create the Ext2 disk image with smoke.sh
"$ROOT/scripts/build-fs.sh"

DISK="$ROOT/disk.img"
# Write the virtio-blk test magic string to sector 0 (Ext2 leaves sector 0-1 (first 1024 bytes) unused)
printf 'VIRTIOTEST!!' | dd of="$DISK" bs=1 seek=0 conv=notrunc status=none

# Run QEMU with isa-debug-exit and no display
set +e
qemu-system-x86_64 -enable-kvm \
    -m 2G \
    -drive if=pflash,format=raw,readonly=on,file="$ROOT/OVMF_CODE.fd" \
    -drive if=pflash,format=raw,readonly=on,file="$ROOT/OVMF_VARS.fd" \
    -drive format=raw,file=fat:rw:"$ROOT/esp" \
    -drive file="$ROOT/disk.img",format=raw,if=none,id=vd0 \
    -device virtio-blk-pci,drive=vd0 \
    -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
    -display none -serial stdio -no-reboot

QEMU_EXIT_CODE=$?
set -e

# isa-debug-exit returns (value << 1) | 1.
# We chose 0x10 (16) for Success, which becomes (16 << 1) | 1 = 33
if [ $QEMU_EXIT_CODE -eq 33 ]; then
    echo "Tests passed!"
    exit 0
elif [ $QEMU_EXIT_CODE -eq 35 ]; then
    echo "Tests failed!"
    exit 1
else
    echo "QEMU exited with unexpected code: $QEMU_EXIT_CODE"
    exit 1
fi
