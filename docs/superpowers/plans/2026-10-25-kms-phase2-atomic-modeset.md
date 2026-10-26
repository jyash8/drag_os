# Phase 2: Atomic Modesetting — Verify + Gap-Fill Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Verify the atomic modeset path works end-to-end and fill the remaining gaps so libdrm's atomic helpers (`drmModeAtomicCommit`, page flip with event, property GET) produce correct results on DragOS. The handler (`handle_atomic_commit`, `handle_page_flip`, `queue_flip_event`) is substantially implemented; this phase validates it and plugs the gaps (cursor/overlay planes skipped, property GET values, fence delivery).

**Architecture:** Add guest probes that exercise atomic commit + page flip + event drain, verify flip-complete events arrive on the DRM fd, confirm non-primary planes are rejected safely, and confirm property GET returns sensible IDs/values. All changes are additive tests + small handler fixes in `drivers/drm/mod.rs`, `drm-uapi`, and `initrd/`.

**Tech Stack:** Rust `no_std` kernel; C guest probes in `initrd/` compiled with `gcc` against `drm/drm.h`.

**Spec:** `docs/superpowers/plans/2026-10-25-kms-complete-implementation.md` Phase 2.

## Global Constraints

- `handle_atomic_commit` must not mutate state when `DRM_MODE_ATOMIC_TEST_ONLY` is set (already returns 0 before touching `GLOBAL_DRM`).
- Non-primary planes (cursor/overlay) must never be passed to `primary_plane_update` (QEMU `set_scanout(res=0)` kills console scanout).
- `MODE_ATOMIC` with `count_objs == 0` returns 0 (already true).
- Unknown property IDs on any object are ignored (no error) — matches current `_ => {}` arms.
- Lock ordering: `GLOBAL_DRM` held briefly, never nested across `process.lock()`.
- Every new ABI constant is computed via `drm_ioctl_t::<T>` / `ioc` (no hand-written hex literals).
- Unimplemented ioctls return `ENOTTY`; `EINVAL` means "garbage payload".

## Review Focus

- Atomic commit `TEST_ONLY` leaves `d.crtcs`, `d.planes`, `d.connectors`, `d.framebuffers` unchanged.
- Atomic commit real (non-TEST_ONLY) updates `pl.fb_id`/`pl.crtc_id`/`crtc.active` and calls `primary_plane_update` for primary planes.
- Page flip with `DRM_MODE_PAGE_FLIP_EVENT` queues a `DRM_EVENT_FLIP_COMPLETE` with the client's `user_data` and wakes DRM-fd waiters.
- `queue_flip_event` caps `DRM_EVENTS` at 64 entries and drops oldest silently when full.
- Property GET (`MODE_GETPROPERTY`) returns property ID/name/flags for `CRTC_ID`, `FB_ID`, `MODE_ID`, `ACTIVE` on CRTC/plane/connector objects.
- `MODE_CREATEPROPBLOB`/`DESTROYPROPBLOB` round-trip: create returns a nonzero `blob_id`, destroy frees it, get returns the bytes.

---

### Task 1: Atomic commit + page flip + event guest probe

**Files:**
- Create: `initrd/atomicprobe.c`
- Modify: `scripts/build-fs.sh` (compile + stage `atomicprobe`), `crates/tools/src/test_runner.rs` (add `atomicprobe` test case).

**Interfaces:**
- Consumes: `DRM_IOCTL_MODE_ATOMIC`, `DRM_IOCTL_MODE_PAGE_FLIP`, `DRM_IOCTL_MODE_GETRESOURCES`, `mmap`, `poll`/`read` on DRM fd.
- Produces: `ATOMIC_PROBE_PASS` marker on serial; verifies TEST_ONLY no-op, real commit changes fb, flip event drains with matching user_data.

- [ ] **Step 1: Write `initrd/atomicprobe.c`**

Probe flow:
1. Open `/dev/dri/card0`, `MODE_GETRESOURCES` → get crtc id, connector id.
2. `MODE_CREATE_DUMB` 1280×800 bpp32 → handle; `MODE_MAP_DUMB` → offset; `MODE_ADDFB` → fb_id.
3. `MODE_ATOMIC` TEST_ONLY with plane→fb→crtc→active (count_objs>0). Expect return 0. Record crtc state before/after — must be unchanged.
4. `MODE_ATOMIC` real commit (non-TEST_ONLY) with same props. Expect return 0. Read back `MODE_GETCRTC` — crtc.fb_id must equal the new fb_id, crtc.active must be true.
5. `MODE_PAGE_FLIP` with `DRM_MODE_PAGE_FLIP_EVENT`, user_data=0xDEADBEEF, fb_id = (same or second fb). Expect return 0.
6. `poll(POLLIN)` on DRM fd → readable. `read()` → drain ≥ 2 events (VBLANK + FLIP_COMPLETE). Verify one event has `event_type == DRM_EVENT_FLIP_COMPLETE` and `user_data == 0xDEADBEEF`.
7. Print `ATOMIC_PROBE_PASS` if all sub-checks pass, else `ATOMIC_PROBE_FAIL <stage>`.

- [ ] **Step 2: Add compile+stage to `scripts/build-fs.sh`** (gcc -O2, link -lrt, stage_bin_with_libs, copy to STAGING/bin + usr/bin + initrd).

- [ ] **Step 3: Add test case to `crates/tools/src/test_runner.rs`**

```rust
TestCase {
    name: "atomicprobe",
    command: "/usr/bin/atomicprobe; echo ATOMIC_RC=$?",
    expected: &["ATOMIC_PROBE_PASS", "ATOMIC_RC=0"],
    timeout_secs: 60,
    gl_only: false,
},
```

- [ ] **Step 4: Build fs + run test** — `cargo run -p tools -- build-fs`, then `cargo run -p tools -- test atomicprobe` (expect PASS).

- [ ] **Step 5: Commit** `git commit -am "test: atomic modeset + page flip + event probe"`

---

### Task 2: Property GET + propblob round-trip guest probe

**Files:**
- Create: `initrd/propblobprobe.c`
- Modify: `scripts/build-fs.sh` (compile + stage), `crates/tools/src/test_runner.rs` (add `propblobprobe` test case).

**Interfaces:**
- Consumes: `DRM_IOCTL_MODE_GETPROPERTY`, `DRM_IOCTL_MODE_GETPROPBLOB`, `DRM_IOCTL_MODE_CREATEPROPBLOB`, `DRM_IOCTL_MODE_DESTROYPROPBLOB`.
- Produces: `PROPBLOB_PROBE_PASS` marker.

- [ ] **Step 1: Write `initrd/propblobprobe.c`**

Probe flow:
1. Open `/dev/dri/card0`, `MODE_GETRESOURCES` → crtc id.
2. `MODE_GETPROPERTY` for property IDs in a small range (e.g. 0..32) on the crtc object; collect any that return success with non-empty name.
3. Verify at least one of {CRTC_ID, FB_ID, MODE_ID, ACTIVE} appears with sensible flags (IMMUTABLE/RANGE/ENUM/BLOB).
4. `MODE_CREATEPROPBLOB` with 16 bytes of payload → expect nonzero blob_id.
5. `MODE_GETPROPBLOB` with that blob_id → expect length==16 and data matches payload.
6. `MODE_DESTROYPROPBLOB` with blob_id → expect 0.
7. `MODE_GETPROPBLOB` again with same blob_id → expect ENOENT/EINVAL (destroyed).
8. Print `PROPBLOB_PROBE_PASS` if all sub-checks pass.

- [ ] **Step 2: Add compile+stage to `scripts/build-fs.sh`.**
- [ ] **Step 3: Add test case to `crates/tools/src/test_runner.rs`** (timeout 60s, expected `PROPBLOB_PROBE_PASS` + `PROPBLOB_RC=0`).
- [ ] **Step 4: Build fs + run test** — expect PASS.

- [ ] **Step 5: Commit** `git commit -am "test: property GET + propblob round-trip probe"`

---

### Task 3: Gap-fill — non-primary plane rejection in atomic commit

**Files:**
- Modify: `crates/kernel/src/drivers/drm/mod.rs` (the `for c in &commits` loop that already `continue`s on non-primary planes).

**Interfaces:**
- Consumes: `c.plane_type` from `PlaneCommitInfo`.
- Produces: explicit guard + diagnostic so cursor/overlay planes never reach `primary_plane_update`.

- [ ] **Step 1: Verify current code** — the loop at `~1905` already `continue`s when `c.plane_type != DRM_PLANE_TYPE_PRIMARY` with a trace log. Confirm no path passes a non-primary plane to `primary_plane_update`.
- [ ] **Step 2 (if gap found):** Add an explicit `match c.plane_type { DRM_PLANE_TYPE_PRIMARY => apply, _ => continue }` arm and a trace line `[ATOMIC_COMMIT] reject non-primary plane_id={} type={}`.
- [ ] **Step 3: Build kernel** — `cargo run -p tools -- build-kernel` must succeed.
- [ ] **Step 4: Commit** `git commit -am "fix(kernel): atomic commit rejects non-primary planes"` (or "verified no change needed" if already correct).

---

### Task 4: Gap-fill — flip event delivery verification via atomicprobe

**Files:**
- Modify: `initrd/atomicprobe.c` (already covers this in Task 1 Step 1) — no separate code change unless Task 1 reveals events aren't delivered.

**Interfaces:**
- Consumes: `queue_flip_event`, `DRM_EVENTS`, `wake_pipe_waiters`, `drain_events`.
- Produces: confirmed flip-complete event with matching user_data on the DRM fd.

- [ ] **Step 1: Run `atomicprobe`** (already in Task 1 Step 4). If flip-event drain passes, Task 4 is a no-op.
- [ ] **Step 2 (if gap found):** Fix `queue_flip_event` / `handle_page_flip` / `handle_atomic_commit` so events are queued + waiters woken.
- [ ] **Step 3: Re-run `atomicprobe`** — expect PASS.
- [ ] **Step 4: Commit** any fix.

---

### Task 5: Run regression suite + SMP-2 smoke

**Files:**
- None (verification only).

- [ ] **Step 1: Run `cargo run -p tools -- test`** — confirm baseline still passing; the only acceptable new failures are the known disjoint serial-flake set (~2/34).
- [ ] **Step 2: Run `cargo run -p tools -- run --smp 2 --no-audio`** for 90s — confirm zero panics and mywima reaches main loop.
- [ ] **Step 3: Confirm `git status`** shows only intended files.
- [ ] **Step 4: Commit** nothing (verification only) — ledger line records result.
