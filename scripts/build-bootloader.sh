#!/usr/bin/env bash
set -euo pipefail

ROOT="$(pwd)"

BOOTLOADER="$ROOT/bootloader"
ESP="$ROOT/esp/efi/boot"

echo "==> Building bootloader"

cargo b -r -p bootloader --target x86_64-unknown-uefi

mkdir -p "$ESP"

cp \
    "$ROOT/target/x86_64-unknown-uefi/release/bootloader.efi" \
    "$ESP/bootx64.efi"

echo "==> Bootloader copied to:"
echo "    $ESP/bootx64.efi"
