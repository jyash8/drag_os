# KMS Subsystem — Complete Implementation Plan

Date: 2026-10-25. Stable baseline: SMP-2 desktop, zero panics, 20/22 tests pass.

## Goal

Bring `drivers/drm` + `drivers/virtio/gpu.rs` + `graphics/` + `fsio` VT-fb bridges to **Linux KMS UAPI parity** sufficient for:
1. Single virtio-gpu scanout compositing (already works).
2. Multi-plane primary/overlay/cursor scanouts.
3. PRIME dma-buf cross-process import/export (multi-process compositor buffers).
4. Atomic modeset with page flip + fences + damage.
5. Full VT takeover (bind/release, Alt+Fx console switch).
6. Gamma / properties / lease ioctls so libdrm's `drmMode*` helpers stop falling through catch-all arms.

Non-goals: multi-GPU hotplug, PRIME hot-plug, fbdev emulation, eDP physical display (QEMU always virtio scanout).

## Phase status

| Phase | Scope | Status |
|-------|-------|--------|
| 0 | Dead code & errno alignment | Not started |
| 1 | DMA-buf PRIME export/import/mmap + SYNC | **DONE** (primeprobe PASSED) |
| 2 | Atomic modeset verify + gap-fill (page-flip events, fences, property GET, cursor/overlay planes) | Next |
| 3 | VT takeover, damage, gamma LUT | Not started |
| 4 | Virgl context route coherence (single context_create path) | Not started |
| 5 | Virgl execbuffer fence-in/out + syncobj + 3D blob | Not started |
| 6 | Examples & tooling tests (kms_self_test, gbm multi-import, mesademo) | Not started |
| 7 | Final review + whole-suite green + SMP-2 smoke | Not started |

## Scope Inventory
- `crates/drm-uapi`: constants, structs, ioctls (currently 45 dispatched).
- `crates/kernel/src/drivers/drm/mod.rs`: ~4k LOC.
- `crates/kernel/src/drivers/drm/virtgpu.rs` + `virtgpu_display.rs` + `virtgpu_plane.rs`.
- `crates/kernel/src/graphics/` (display/vt/framebuffer).
- `crates/kernel/src/arch/x86_64/syscalls/fsio.rs` (VT ioctls).
- `crates/kernel/src/drivers/virtio/gpu.rs`.

## Global Constraints (all phases)

- Every new ABI constant is computed via `drm_ioctl_t::<T>` / `ioc` (no hand-written hex literals); add `abi_pin!` entries for any new ioctl number.
- Unimplemented ioctls return `ENOTTY`; `EINVAL` means "garbage payload", never "not supported".
- `DRM_CAP_PRIME` advertises `IMPORT | EXPORT`; all PRIME ioctl numbers pinned and verified (52/52).
- Lock ordering `SCHEDULER → CPUS[k] → wait-queues → process.lock()` must never be violated; `GLOBAL_DRM` is never held across `process.lock()`.
- Context identifiers accepted across the full range (virgl 1 and venus 2) — no hard-coded single capset.

## Review Focus (all phases)

