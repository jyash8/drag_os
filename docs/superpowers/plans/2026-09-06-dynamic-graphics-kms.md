# P5 — Dynamic Graphics/KMS Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the fixed-1280×800 framebuffer assumption with real mode negotiation (EDID → mode list → selection), connector hotplug events, and multi-output awareness — while keeping virgl as the GPU.

**Architecture:** The kernel DRM driver (`kernel/src/drivers/drm/`, 1474-line `mod.rs` + `kms.rs` + `uapi.rs` + `types.rs`) grows a connector/mode inventory populated from virtio-GPU EDID or the current hardcoded default. Reads are truthful (`GETCONNECTOR` returns real modes, `GETCRTC` real state); writes (`SETCRTC`, page-flip) keep today's scanout path. Hotplug reuses the P1 uevent broadcast (`change` action on the `dri/card0` node). Independent of P1–P4 except uevent reuse.

**Tech Stack:** kernel DRM driver, virtio-gpu command queue (find via `grep -rn "virtio.*gpu\|VIRTIO_GPU" kernel/src/drivers --include="*.rs" -l`), Smithay `DrmOutputManager` in mywima (already handles multi-output on its side).

---

### File map

- Modify: `kernel/src/drivers/drm/mod.rs` — connector inventory, EDID fetch, mode list, `GETCONNECTOR/GETRESOURCES` truthful replies.
- Modify: `kernel/src/drivers/drm/uapi.rs` — new ioctl structs if any are missing (check what's already decoded).
- Modify: `kernel/src/drivers/drm/kms.rs` — hotplug detection + `drmHandleEvent` wakeups.
- Modify: `/home/jyash8/Projects/mywima/src/backends/kms.rs` — handle hotplug events (output added/removed) instead of assuming one static output.

---

### Task 1: Inventory — connectors, EDID, mode list

**Files:**
- Modify: `kernel/src/drivers/drm/mod.rs`

- [ ] **Step 1: Connector table.** `struct Connector { id: u32, status: Connected|Disconnected, modes: Vec<Mode>, current: u32 }`, one entry per virtio-gpu scanout (start with 1). `GETRESOURCES` returns its IDs; `GETCONNECTOR` returns `status + modes` instead of the static blob (find the current static reply via `grep -n "GETCONNECTOR\|getconnector\|0xA7" kernel/src/drivers/drm/*.rs`).
- [ ] **Step 2: Mode list.** Seed from EDID if the virtio-gpu path exposes it (check the virtio-gpu driver for `EDID`/`GET_EDID` handling); otherwise seed from the QEMU command line (`virtio-vga,xres=1280,yres=800`) plus a safe fallback list: 1280×800 (preferred), 1024×768, 800×600, 640×480. `GETCONNECTOR` marks 1280×800 preferred — identical behavior to today, but now data-driven.
- [ ] **Step 3: Verify.** Boot; mywima's `[drm] available mode:` log line (already printed in `kms.rs`) must still list 1280×800 first. Zero behavior change.

Run: `cargo run -p tools -- build-kernel 2>&1 | tail -1 && cargo run -p tools -- build-fs 2>&1 | tail -1`

- [ ] **Step 4: Commit**

```bash
git add kernel/src/drivers/drm/mod.rs
git commit -m "feat(drm): connector inventory with data-driven mode list"
```

---

### Task 2: Real mode setting (SETCRTC honored, not assumed)

**Files:**
- Modify: `kernel/src/drivers/drm/mod.rs`, `kms.rs`.

- [ ] **Step 1: Honor the requested mode.** Today scanout presumably assumes the boot framebuffer size. On `SETCRTC` with mode M: validate M against the connector's list (reject unknown with `EINVAL` — this catches compositor/driver disagreements loudly instead of mis-scanning), reprogram the virtio-gpu scanout resource to M's dimensions, update `FRAMEBUFFER_INFO` so the tablet ABS→pixel scaling (`xhci.rs` decode path) and foot grid math follow automatically.
- [ ] **Step 2: Page-flip completion events.** If `PAGE_FLIP` currently blocks or fakes completion, emit real `DRM_EVENT_FLIP_COMPLETE` on the drm fd so Smithay's frame scheduling stops guessing. Check current behavior via `grep -n "FLIP\|flip\|vblank" kernel/src/drivers/drm/*.rs` before designing.
- [ ] **Step 3: Verify.** Forcing a different mode (QEMU `-device virtio-vga,xres=1024,yres=768`, needs a tools flag — add `--res WxH` to `tools/src/qemu.rs` if trivial, else hand-run QEMU once) boots with a correctly-sized desktop, correct cursor scaling, no stretched wallpaper.

- [ ] **Step 4: Commit**

```bash
git add kernel/src/drivers/drm/mod.rs kernel/src/drivers/drm/kms.rs
git commit -m "feat(drm): honor SETCRTC modes, real flip completion events"
```

---

### Task 3: Hotplug + multi-output

**Files:**
- Modify: `kernel/src/drivers/drm/kms.rs`, mywima `kms.rs`.

- [ ] **Step 1: Detect.** Poll virtio-gpu scanout/EDID state on the existing slow timer (same pattern as USB `scan_hotplug`); on change, `broadcast("change", "/devices/pci.../drm/card0", ...)` via the P1 uevent module and wake drm `read()` pollers with a `DRM_EVENT_HOTPLUG` (or equivalent wakeup the Smithay backend already handles — check what mywima's `DrmDevice`/udev backend listens for).
- [ ] **Step 2: Compositor side.** mywima already enumerates outputs at startup; extend to add/remove `DrmOutput` on hotplug events + re-run `refresh_geometry` (workspace code already handles arbitrary output geometry). Test with QEMU monitor `device_add virtio-vga` / `device_del` if supported on this machine type, else simulate by toggling connector status via a debug sysfs trigger.
- [ ] **Step 3: Verify.** Second display appears with wallpaper + tiled windows split per-output; removal re-tiles survivors onto the remaining output without panic.

- [ ] **Step 4: Commit**

```bash
git add kernel/src/drivers/drm/kms.rs
git commit -m "feat(drm): connector hotplug events and multi-output"
```

---

## Self-review

- Spec coverage: inventory, mode setting, hotplug/multi-output each have a task. GEM/PRIME sharing stays out (single-GPU virgl system — nothing to share with; revisit only with a second GPU).
- No placeholders: exact ioctl names, exact fallback mode list, exact log line to check.
- Type consistency: `Connector/Mode` defined once in Task 1, consumed in Tasks 2–3.
