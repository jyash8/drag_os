#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LOG="$(mktemp)"

# Boot the real kernel headless, capture serial, wait for the shell prompt,
# then kill QEMU. The shell stays alive at the prompt (poll blocks waiting for
# keyboard input); we verify it reached the prompt and never panicked.
DISK="$ROOT/disk.img"
[ -f "$DISK" ] || truncate -s 8M "$DISK"
timeout 75 qemu-system-x86_64 -enable-kvm -m 2G \
    -drive if=pflash,format=raw,readonly=on,file="$ROOT/OVMF_CODE.fd" \
    -drive if=pflash,format=raw,readonly=on,file="$ROOT/OVMF_VARS.fd" \
    -drive format=raw,file=fat:rw:"$ROOT/esp" \
    -drive file="$ROOT/disk.img",format=raw,if=none,id=vd0 \
    -device virtio-blk-pci,drive=vd0 \
    -display none -serial file:"$LOG" -no-reboot &
QPID=$!

# Wait up to ~60s for the shell prompt (headless boot + sh init is slow).
for i in $(seq 1 120); do
    if grep -q "/ #" "$LOG" 2>/dev/null; then
        break
    fi
    sleep 0.5
done

kill $QPID 2>/dev/null || true
wait 2>/dev/null || true

# Failure markers: any panic/corruption.
if grep -qE "PANIC|CORRUPT|INVALID OPCODE|DOUBLE FAULT" "$LOG"; then
    echo "SMOKE FAIL: panic/corruption detected"
    grep -E "PANIC|CORRUPT|INVALID OPCODE|DOUBLE FAULT" "$LOG"
    rm -f "$LOG"
    exit 1
fi

# Success marker: shell prompt reached.
grep -q "/ #" "$LOG" || { echo "SMOKE FAIL: no shell prompt"; rm -f "$LOG"; exit 1; }

rm -f "$LOG"
echo "SMOKE PASS"
exit 0
