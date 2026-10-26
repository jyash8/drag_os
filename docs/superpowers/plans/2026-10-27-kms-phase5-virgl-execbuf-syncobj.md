# Phase 5: Virgl Execbuffer + Syncobj + 3D Blob — Verify + Gap-Fill Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prove that every Ven/Virgl entry in the execbuffer path works end-to-end (fence-in / fence-out eventfds, syncobj wait/signal, transfer_to_host, 3D blob create/import), confirm no consumer uses a deprecated code path (only SYS_INNotFound etc), and lock the regression baseline.

**Architecture:** All probe binaries in initrd/ already exist: vkprobe, mesarepro, pure_vulkan_test, vulkansmoke, sdl_vulkan_test. They exercise execbuffer with/without RING_IDX, syncobj handle-to-fd, transfer_to_host_3d, FENCE_FD_OUT. Phase 5 verifies they survive under Phase 4 changes and under current virtio-gpu state. No kernel code changes expected unless a probe exposes a regression; the existing code already implements everything listed.

**Tech Stack:** Rust `no_std` kernel; C guest probe vkprobe (already staged); QEMU virtio-gpu with Venus+virgl capset; libvulkan + libgbm on guest.

**Spec:** `docs/superpowers/plans/2026-10-25-kms-complete-implementation.md` Phase 5.

## Global Constraints

- AGENTS §0: no new ioctl numbers. Capset values accepted {1=virgl, 2=virgl2, 4=venus+ring_idx}. Must accept all in {1,2,4} and refuse others with EINVAL.
- FENCE_FD_IN/OUT: accept with success regardless of device (ignore dependency, sync world busy-waits).
- Syncobj binary only (no timelines; CREATE_SIGNALED supported).
- Syncobj wait must park via task::block (no spin if signalling required) — already implemented.
- No additional context creation beyond what's used (verify no double context due to execbuffer implicit path).

## Review Focus

- `vkprobe` probes virgl 3D execbuffer + FENCE_FD_OUT and prints VKPROBE_PASS.
- `mesarepro` probes render buffer → virgl context → scanout (a full frame-present) and prints MESAREPRO_PASS.
- `pure_vulkan_test` prints an image to a Wayland surface using Venus blob, must not lose device (VK_ERROR_DEVICE_LOST).
- `vulkan-smoke` creation of VkInstance w/o display succeeds.
- `sdl_vulkan_test` SDL+Vulkan end-to-end.

---

### Task 1: Run `vkprobe` via harness + verify PASS

**Files:**
- None (uses existing test_runner case; verify if `vkprobe` still there and passes).
- Read `crates/tools/src/test_runner.rs` to determine whether there's a `vkprobe` test case.

- [ ] **Step 1:** Check test_runner for vkprobe/test cases referencing vkprobe/mesarepro.
- [ ] **Step 2:** Run `cargo run -p tools -- test vkprobe` (expect VKPROBE_PASS).
- [ ] **Step 3:** Commit nothing (verification).

---

### Task 2: Run `mesarepro` via harness + verify PASS

**Files:**
- None.

- [ ] **Step 1:** Check if mesa-probe is in test_runner.
- [ ] **Step 2:** Run via harness; show MESAREPRO_PASS.
- [ ] **Step 3:** Commit nothing.

---

### Task 3: Run `pure_vulkan_test` via harness + verify PASS (no device-lost error)

**Files:**
- None.

- [ ] **Step 1:** Locate or add entry; verify vk call doesn't crash/lose device.
- [ ] **Step 2:** Run test; verify PASS.
- [ ] **Step 3:** Commit nothing.

---

### Task 4: Remove stale `virtgpu_ioctl.rs` NOT_YET_PORTED claim dupes (already done → just doc cleanup)

**Files:**
- `crates/kernel/src/drivers/drm/virtgpu_ioctl.rs` (already clean; confirmed).

- [ ] **Step 1:** Re-verify the file has empty NOT_YET_PORTED and additionally explain in comment that every dispatch in mod.rs is live.
- [ ] **Step 2:** Build kernel.

---

### Task 5: Regression suite + SMP-2 smoke

**Files:**
- None.

- [ ] **Step 1:** Run full suite (target 29+ passes; same set of pre-existing unrelated failures).
- [ ] **Step 2:** SMP-2 90s smoke: log KMS renders, virgl/Venus synthetic test if present.
- [ ] **Step 3:** Confirm `git status`.
