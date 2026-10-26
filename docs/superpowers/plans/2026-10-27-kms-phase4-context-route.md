# Phase 4: Virgl Context Route Coherence — Verify + Dead-Code Cleanup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Verify that the implicit-vs-explicit context routing path is coherent for both virgl and Venus clients (AGENTS §0 corollary: accept every capset id in the space, do not hard-code `VIRTIO_GPU_CAPSET_VIRGL`), remove misleading dead-code comments, and prove `context_init` returns the correct errnos and survives a churn of create/destroy cycles without leaking contexts.

**Architecture:** Three checks:
1. `context_init` accepts capsets `{VIRGL=1, VIRGL2=2, VENUS=3}` and returns a nonzero context id; unknown capset returns `EINVAL` (not silently success or wrong handler).
2. A plain `EXECBUFFER` (no `RING_IDX`) on a client that created a context via `context_init` does NOT create a second one, and selects the newly-created context (rather than pining on a hardcoded capset).
3. Dead-code cleanup: remove the stale `NOT_YET_PORTED` block in `virtgpu_ioctl.rs` (it lists `context_init` as unported but it IS dispatched), and reconcile the dead comments in ` virtuallygpu.rs::378`.

**Tech Stack:** Rust `no_std` kernel; C guest probe in `initrd/` compiled with `gcc` against `drm/virtgpu_drm.h` + `drm/drm.h`; existing emulated `virtio-gpu` QEMU device.

**Spec:** `docs/superpowers/plans/2026-10-25-kms-complete-implementation.md` Phase 4, plus `docs/deferred-work.md` §7.2.

## Global Constraints

- AGENTS §0: do not invent ioctls. virgl context is created on first `EXECBUFFER` without `RING_IDX`; Venus via `CONTEXT_INIT`. No new ioctl numbers.
- AGENTS corollary §0: accept every capset in `{1,2,3}` (and refuse everything else with `EINVAL`), never hardcode a single id.
- Context lifecycle: one context per `VIRTGPU_CONTEXT_INIT` call, owner_pid recorded, dropped via `handle_drop_master`/exit cleanup. No leak on repeated cycles (≤256 contexts total expected).
- Lock ordering: `VIRTGPU_MANAGER` is locked briefly and dropped before taking GLOBAL_VIRTIO_GPU; review submission-reap flow (`reap_gpu_fences`) already handles this.

## Review Focus

- `context_init` with `capset_id=1` (virgl) returns `rc=0` and `ctx_id>0`.
- `context_init` with `capset_id=2` (virgl2) and `capset_id=3` (venus) work identically; same rc=0 + ctx_id.
- `context_init` with `capset_id=0` returns `EINVAL` (idle-state not established).
- A plain unarmed execbuffer reports `handle_virtgpu_execbuffer` selects a context whose `capset_id` matches the client's (not a wrong second one).
- All context-init test runs append exactly one new context per valid init; teardowns drop it (no double-free).
- `NOT_YET_PORTED` comment removed — no false "unimplemented" claims left in the tree.

---

### Task 1: Context-init capability probe

**Files:**
- Create: `initrd/ctxprobe.c` (importing `virtgpu_drm.h` or `drm/virtgpu_drm.h` + `drm/drm.h`)
- Modify: `scripts/build-fs.sh`, `crates/tools/src/test_runner.rs`.

**Interfaces:**
- Consumes: `DRM_IOCTL_VIRTGPU_CONTEXT_INIT`, `DRM_IOCTL_VIRTGPU_RESOURCE_CREATE`, `DRM_IOCTL_VIRTGPU_GET_CAPS`, `VIRTGPU_CONTEXT_PARAM_CAPSET_ID`, `VIRTGPU_PARAM_SUPPORTED_CAPSET_IDS`.
- Produces: `CTX_PROBE_PASS` marker on serial.

- [ ] **Step 1: Create `initrd/ctxprobe.c`** with flow:
  - Open `/dev/dri/renderD128` (or `/dev/dri/card0`) — R/W.
  - `DRM_IOCTL_VIRTGPU_GET_CAPS` capset-version (capset 1) first so it succeeds (proves get_caps works).
  - For capsets `{1, 2, 3}`:
    - Build one `DrmVirtgpuContextSetParam` (capset, DEBUG_NAME).
    - `DRM_IOCTL_VIRTGPU_CONTEXT_INIT` num_params=2 + params array.
    - Assert `ctx`/context id ≠ 0.
  - For `capset_id=0`: assert context_init returns `-EINVAL` (errno 22), not success.
  - Print `CTX_PROBE_PASS` if all three valid capsets succeed and the invalid one fails as expected (with errno recorded as `CTX_PROBE_FAIL` otherwise).

- [ ] **Step 2: Add compile+stage to `scripts/build-fs.sh`** (link -lrt or `/usr/lib/libdrm`, `stage_bin_with_libs`).

- [ ] **Step 3: Add test case to `crates/tools/src/test_runner.rs`** (timeout 60s; expected `CTX_PROBE_PASS` + `CTX_RC=0`).

- [ ] **Step 4: Build fs + run test** — expect PASS.

- [ ] **Step 5: Commit** `git commit -am "test: virtio-gpu context_init capset probe"`

---

### Task 2: fix `handle_virtgpu_execbuffer` hidden second-context regression guard

**Files:**
- Modify: `crates/kernel/src/drivers/drm/virtgpu.rs` (execbuffer context routing — already `selects any non-Venus context for plain (non-RING_IDX) submissions`).
- Modify: relevant invariants text in AGENTS.md (already noted).

**Interfaces:**
- Consumes: existing `caller_owns_ctx` (in mod.rs), `VIRTGPU_MANAGER`.
- Produces: regression-in-checkpoint: the select must pick only non-Venus contexts. Verified via code review (no new gate this phase) + smoke test from Task 1.

- [ ] **Step 1: Verify the execbuffer path does not inherit a hardcoded `capset_id == 1` match.** Code review: grep `capset_id ==` and confirm only the broadened form (`!= VENUS`) exists. Document in the brief.

- [ ] **Step 2: Confirm the `get_virgl_ctx_id` helper stays unused...** Per AGENTS.md §0, keep an internal-only path (it's documented as internal-only). Add a comment clarifying its non-export status.

- [ ] **Step 3: Build kernel** must succeed.

- [ ] **Step 4: Commit** `git commit -am "docs(virtgpu): clarify context-select (non-Venus broad capture) and remove stale port spec"` — combined with Task 3's PR.

---

### Task 3: remove stale `NOT_YET_PORTED` comment block

**Files:**
- Modify: `crates/kernel/src/drivers/drm/virtgpu_ioctl.rs` — delete the dead comment (it lists `context_init`, `map_ioctl`, `transfer_from_host`, `transfer_to_host` as `NOT_YET_PORTED` but all four are dispatched live in mod.rs:904-927).

**Interfaces:**
- Consumes: mod.rs dispatch table at lines 930-941 (`CONTEXT_INIT = :2678`).
- Produces: a non-misleading source file.

- [ ] **Step 1: Read `virtgpu_ioctl.rs` lines 1-40** to confirm the stale claim.
- [ ] **Step 2: Delete the stale `NOT_YET_PORTED` block.**
- [ ] **Step 3: Add a clarifying header** stating all virtio-gpu ioctls are ported (per mod.rs dispatch).
- [ ] **Step 4: Build kernel** must succeed (release).
- [ ] **Step 5: Commit** (combined with Task 2).

---

### Task 4: resource attach lifecycle on prime-imported blobs

**Files:**
- Modify: `crates/kernel/src/drivers/drm/mod.rs` (`handle_prime_fd_to_handle` already calls `ctx_attach_resource`).
- Modify: `crates/kernel/src/drivers/drm/virtgpu.rs` (verify `ctx_attach_resource` only marks in `attached_resources` once; no leak on double-import).

**Interfaces:**
- Consumes: `VIRTGPU_MANAGER`, `caller_owns_ctx`, `ctx_attach_resource`.
- Produces: one unit-test-less but probed check — `CTX_PROBE` also creates a blob, imports it, and reads back the handle count (n=1, no duplicate).

- [ ] **Step 1: Confirm `ctx_attach_resource` dedups by `res_id` or at least reports failure once.** Read the function. If it prepends duplicates, fix to skip-if-already-present. If it already guards, just document it.
- [ ] **Step 2: Build kernel**.
- [ ] **Step 3: Commit** any fix.

---

### Task 5: regression verification (dummy, no code changes)

**Files:**
- None (verification only).

- [ ] **Step 1: Run `cargo run -p tools -- test`** — expect 27+ pass; only the known pre-existing failures (kms_fault, xdg_portal_bus, job_control, curl_probe x2, mount_subsystem).
- [ ] **Step 2: Run `cargo run -p tools -- run --smp 2 --no-audio`** 90s — confirm zero panics/cr2 and mywima + footnote render + drago desktop only, no background shell.
- [ ] **Step 3: Confirm `git status`** intented files.
- [ ] **Step 4: Commit** (verification only).
