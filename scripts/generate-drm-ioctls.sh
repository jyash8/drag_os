#!/usr/bin/env bash
# Regenerate initrd/drm_ioctls.h from the kernel's own drm-uapi crate.
set -euo pipefail
cd "$(dirname "$0")/.."
python3 scripts/generate-drm-ioctls.py initrd/drm_ioctls.h
