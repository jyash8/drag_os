# Phase 6: Examples & Tooling Tests — Consolidation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Confirm the existing example/probe binaries (`mesarepro`, `gbmscanout`, `vkprobe`) run clean under `--gl`, and lift test-level regression visibility to the full suite (phases 1-5 probes pass, the 6 pre-existing failures unchanged). No kernel changes expected — purely tooling verification.

**Architecture:** The example binaries (`mesarepro.c`, `gbmscanout.c`) already exist in initrd and are built by build-fs.sh (host gcc). Run them via the test harness with `--gl` to exercise Gallium (virgl) + GBM (single-scanout), and confirm expected markers land on serial. If any fail, route through `systematic-debugging`.

**Tech Stack:** Rust `no_std` kernel; C guest probes in `initrd/` compiled with host gcc against libgbm/libdrm; QEMU virtio-gpu with GL display (venus+virgl).

**Spec:** `docs/superpowers/plans/2026-10-25-kms-complete-implementation.md` Phase 6.

## Global Constraints

- GL-only tests must pass through the harness's `--gl` path (`DRAGOS_TEST_GL=1` or `cargo run -p tools -- test <name> --gl`).
- Pre-existing failures (kms_fault, xdg_portal_bus, job_control, curl_probe x2, mount_subsystem) remain accepted; no new failures OK.
- Verified kernel binary must be the latest commit (already built today).

## Review Focus

- `mesarepro` prints `MESAREPRO_PASS` (Gallium draws a textured frame into a virgl context).
- `gbmscanout` prints `GBMSCANOUT_PASS` (single composite frame reaches the scanout).
- `stress_exec` (60-iter vkprobe) already passes without GL — validates context churn.
- Full suite returns ≥27 passed (baseline with Phase 4-5 probes).

## Tasks

### Task 1: Run `mesarepro` (GL) via harness
- [ ] Step 1: Check whether test_runner has a `mesarepro` row.
- [ ] Step 2: If missing, add a provisional row (command `/usr/bin/mesarepro; echo MESAREPRO_RC=$?`, expected `MESAREPRO_PASS`, 30s, gl_only=true).
- [ ] Step 3: Run `cargo run -p tools -- test mesarepro --gl`.
- [ ] Step 4: Commit nothing if PASS; commit only if test entry added.

### Task 2: Run `gbmscanout` (GL) via harness
- [ ] Step 1: Repeat step 1-4 for `gbmscanout`.
- [ ] Step 2: Confirm `GBMSCANOUT_PASS` lands on serial.

### Task 3: Verify collation (kms_self_test) probe status
- [ ] Step 1: Check `scripts/build-fs.sh` for `kms_self_test` staging.
- [ ] Step 2: If absent, document as NOT attempted (skip).
- [ ] Step 3: Commit nothing.

### Task 4: Run full test suite (final baseline) + SMP-2 90s smoke
- [ ] Step 1: `cargo run -p tools -- test` — confirm 29+ passes, ≤6 same pre-existing failures.
- [ ] Step 2: `cargo run -p tools -- run --smp 2 --no-audio` 90s.
- [ ] Step 3: Confirm `git status` shows untracked probe binaries only.
- [ ] Step 4: Mark Phase 6 complete in the macro plan.
- [ ] Step 5: Commit plan docs (untracked templates -> commit).