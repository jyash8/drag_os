#!/usr/bin/env bash
set -euo pipefail

ROOT="$(pwd)"

KERNEL="$ROOT/kernel"
ESP="$ROOT/esp/efi/boot"

echo "==> Building initrd"
"$ROOT/scripts/build-fs.sh"

echo "==> Building kernel"

cargo b --release -p kernel --target x86_64-unknown-none

mkdir -p "$ESP"

cp \
    "$ROOT/target/x86_64-unknown-none/release/kernel" \
    "$ESP/kernel.elf"

echo "==> Kernel copied to:"
echo "    $ESP/kernel.elf"
