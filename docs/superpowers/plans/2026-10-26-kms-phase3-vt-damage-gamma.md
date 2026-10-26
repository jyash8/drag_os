# Phase 3: VT Takeover + Damage + Gamma — Verify + Gap-Fill Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Verify and (where needed) gap-fill three already-partially-wired subsystems of the KMS bridge: damage clipping (DIRTYFB — implemented but unexercised by a staged client), gamma lookup tables (SETGAMMA/GETGAMMA currently return literal 0), and VT takeover (bind_graphical/release_graphical already wired into the VT/console ioctls but no client invokes VT_OPENQRY/VT_ACTIVATE or KDSETMODE). Output: working probes + zero-total-success on the suite + SMP-2 smoke.

**Architecture:** Damage needs no driver change unless a defect surfaces in the probe (per-fb damage filed/consumed by the next flip; overflow table forces full-frame). Gamma is implemented as a no-op that returns 0 for SETGAMMA and exposes range 0 for GETGAMMA — no LUT is wired into QEMU scanout; the call must at least return success so libdrm falls back. VT takeover exercises the existing fsio.rs bind/release_graphical path and must not panic. All work is additive tests in `initrd/` + small fixups in `drivers/drm/mod.rs` (Gamma no-op) + `fsio.rs` (VT) that surface during the probes.

**Tech Stack:** Rust `no_std` kernel; C guest probes in `initrd/` compiled with `gcc` against `drm/drm.h` + `<sys/vt.h>`; debugcon (`uefi_debug.log`) for TRACE.

**Spec:** `docs/superpowers/plans/2026-10-25-kms-complete-implementation.md` Phase 3, and `docs/deferred-work.md` §5 + §7 (VT binding caveat + damage note).

## Global Constraints

- MODE_DIRTYFB (already wired) must still return the right errno: EFAULT on unreadable clips, EINVAL for odd ANNOTATE_COPY clip counts, ENOENT for unknown fb. Never nests GLOBAL_DRM with FB_DAMAGE.
- SETGAMMA/GETGAMMA must return 0 (success) once a valid ramp payload passes, never ENOSYS (AGENTS §0: errno is load-bearing, libdrm probe failure makes it fall back to software gamma and loses hot-plug optimization). Currently returns literal 0.
- VT takeover must not panic: VT_OPENQRY → VT_ACTIVATE → KDSETMODE(KD_GRAPHICS) → (compositor work) → KDSETMODE(KD_TEXT) → release_graphical all succeed or return defined errnos without dereferencing None (bind_graphical/release_graphical on GRAPHICAL_VT=0).
- Lock ordering: GLOBAL_DRM briefly, never nested with process.lock(). VT ioctls follow fsio.rs existing pattern.
- Damage is filed per-fb_id and consumed by the flip to that same fb_id; overflow table forces full-frame transfer rather than silently dropping damage.

## Review Focus

- DIRTYFB with num_clips==0 returns success (no-op) without marking damage; ANNOTATE_COPY with odd clip count returns EINVAL; FULL flags/insane counts return success and mark fb for full transfer.
- DIRTYFB with 1-256 clips returns success and files damage onto the pending FB's slot; overflow forces full-transfer.
- Setting damage on an fb_id with no matching mmap/commit doesn't crash the table.
- SETGAMMA (MODE_SETGAMMA, nr 0xA5) returns 0 with a 256-entry ramp payload; GETGAMMA (0xA4) returns 0 with gamma_size=256.
- VT_OPENQRY succeeds (returns first available VT).
- VT_ACTIVATE on a valid VT returns 0 and calls bind_graphical.
- KDSETMODE(KD_GRAPHICS) (mode 1) and KDSETMODE(KD_TEXT) (mode 0) succeed on a DRM fd that is master.
- handle_drop_master calls release_graphical (no stale binding).
- Ctrl+Alt+Fx still reclaims via the `graphical != 0 && graphical != vt` path (no crash with graphical=0).

---

### Task 1: Damage clipping guest probe (verify filing + consumption)

**Files:**
- Create: `initrd/damageprobe.c`
- Modify: `scripts/build-fs.sh`, `crates/tools/src/test_runner.rs`.

**Interfaces:**
- Consumes: `DRM_IOCTL_MODE_DIRTYFB`, `DRM_IOCTL_MODE_ADDFB`, `DRM_IOCTL_MODE_PAGE_FLIP`, `DRM_IOCTL_MODE_GETRESOURCES`, `MODE_GETCRTC`.
- Produces: `DAMAGE_PROBE_PASS` marker on serial.

- [ ] **Step 1: Write `initrd/damageprobe.c`**

Probe flow:
1. Open card0. Parse resources → crtc_id, connector_id, (plane id isn't necessary for DIRTYFB).
2. Create two dumb BOs (call them A and B), 1280×800. ADDFB → fb_id_A, fb_id_B.
3. SetMode (crtc_id → fb_id_A) via SETCRTC (so fb_id_A's damage is consumable).
4. DIRTYFB(fb_id_A, num_clips=0 [full]). Expect return 0 (success). Re-call DIRTYFB with 2 clips on [50,50 100x100] & [200,200 50x50] each -> expect 0; the kernel merges/clips the damage.
5. DIRTYFB with ANNOTATE_COPY + odd (3) clips → expect EINVAL (rejects).
6. DIRTYFB with num_clips = 300 (overflow cap 256) → expect 0 (handled as full-frame).
7. DIRTYFB on fb_id_unknown → expect ENOENT.
8. (Optional but useful) DIRTYFB with unreadable clips_ptr (pointer in unmapped space) → expect EFAULT.
9. Print `DAMAGE_PROBE_PASS` if all sub-checks pass, else `DAMAGE_PROBE_FAIL stage=N`.

- [ ] **Step 2: Add compile+stage to `scripts/build-fs.sh`** (gcc -O2, link -lrt, stage_bin_with_libs, copy to STAGING/bin + usr/bin + initrd).

- [ ] **Step 3: Add test case to `crates/tools/src/test_runner.rs`** (timeout 60s, expected `DAMAGE_PROBE_PASS` + `DAMAGE_RC=0`).

- [ ] **Step 4: Build fs + run test** — expect PASS.

- [ ] **Step 5: Commit** `git commit -am "test: DIRTYFB damage clipping probe"`

---

### Task 2: Gamma (SETGAMMA/GETGAMMA) verification probe

**Files:**
- Create: `initrd/gammaprobe.c`
- Modify: `scripts/build-fs.sh`, `crates/tools/src/test_runner.rs`.

**Interfaces:**
- Consumes: `DRM_IOCTL_MODE_GETGAMMA`, `DRM_IOCTL_MODE_SETGAMMA`, `DRM_IOCTL_MODE_GETCRTC`.
- Produces: `GAMMA_PROBE_PASS` marker on serial.

- [ ] **Step 1: Write `initrd/gammaprobe.c`**

Probe flow:
1. Open card0, GETRESOURCES → crtc0.
2. GETGAMMA (size 256) on crtc0 → expect return 0 (kernel's stub returns success), crate→gamma_size reports 256.
3. SETGAMMA (256 ramps of `(0..255) << 8`, stride-by-gamma) on crtc0 → expect return 0 (no-op).
4. Print `GAMMA_PROBE_PASS` if both return 0 (no panic).
NOTE: does NOT compare visible gamma (kernel has no LUT wired; the probe is compatibility-only).
5. If a gap surfaces (e.g., GETGAMMA returns EFAULT because gamma_size differs), bump to Task 3.

- [ ] **Step 2: Add compile+stage to `scripts/build-fs.sh`.**
- [ ] **Step 3: Add test case to `crates/tools/src/test_runner.rs`** (timeout 60s, expected `GAMMA_PROBE_PASS`).
- [ ] **Step 4: Build fs + run test** — expect PASS.

- [ ] **Step 5: Commit** `git commit -am "test: gamma GET/SET compatibility probe"`

---

### Task 3: VT takeover probe (bind/release, no panic)

**Files:**
- Create: `initrd/vtprobe.c` (uses `<sys/vt.h>`, `<sys/ioctl.h>`, kds).
- Modify: `scripts/build-fs.sh`, `crates/tools/src/test_runner.rs`.

**Interfaces:**
- Consumes: `VT_OPENQRY`, `VT_ACTIVATE`, `VT_GETSTATE`, `KDSETMODE`, open(`/dev/dri/card0`) with master capability.
- Produces: `VT_PROBE_PASS` marker on serial.

- [ ] **Step 1: Write `initrd/vtprobe.c`**

Probe flow:
1. Open `/dev/tty1` and `/dev/dri/card0` (host master for this console).
2. VT_OPENQRY(&vtno) → expect success, vtno ∈ [1, 63].
3. KDSETMODE(KD_GRAPHICS) (mode 1) → expect 0 OR defined errno (no panic).
4. VT_ACTIVATE(1) → expect 0 OR ENOTTY/ENODEV (no panic).
5. KDSETMODE(KD_TEXT) (mode 0) → expect 0 OR defined errno.
6. Closing the DRM fd triggers handle_drop_master → release_graphical; the process must not fault.
7. Print `VT_PROBE_PASS` if all ioctls return (0 or defined errno) — note: undefined behavior (fault/crash) means FAIL.

- [ ] **Step 2: Add compile+stage to `scripts/build-fs.sh`.**
- [ ] **Step 3: Add test case to `crates/tools/src/test_runner.rs`** (timeout 60s).
- [ ] **Step 4: Build fs + run test** — expect PASS.

- [ ] **Step 5: Commit** `git commit -am "test: VT activate + KDSETMODE takeover probe"`

---

### Task 4: Damage perf smoke (many dirtyfb per second)

**Files:**
- None (uses atomicprobe.c or new short probe). Optional — extend damageprobe to run DIRTYFB 100× per second within a 2-second window and check no `PANIC`/`Table full` storm in debugcon; if Table full appears once or twice, that's acceptable (bounded).

- [ ] **Step 1: Extend `initrd/damageprobe.c`** to issue DIRTYFB 100× (random 4-clip list) in <2s.
- [ ] **Step 2: Run + check debugcon for `Table full` (allowed once, max).**
- [ ] **Step 3: Commit** damage table performance note.

---

### Task 5: Gap-fill — gamma LUT storage (if probe shows value needed)

**Files:**
- Modify: `crates/kernel/src/drivers/drm/mod.rs` (handle_setgamma, handle_getgamma, dispatch at lines 904-905).
- Modify: `PROJECT ED section` (docs).

**Interfaces:**
- Consumes: DrmModeGamma UAPI from libdrm; kernel nothing currently parses gamma ramps beyond SETGAMMA/GETGAMMA.
- Produces: SETGAMMA with a 256-entry ramp returns 0 (no-op but successful); GETGAMMA returns `gamma_size=256` so a client can assume a ramp is settable.

- [ ] **Step 1: Inspect current stubs.** Current: `DRM_IOCTL_MODE_GETGAMMA => 0,` and `DRM_IOCTL_MODE_SETGAMMA => 0`. These return "success" but don't actually parse anything — SETGAMMA with any payload returns 0 (no validation), GETGAMMA reports no gamma_size.

- [ ] **Step 2 (design decision):** AGENTS.md §0 demands ENOTTY for unimplemented ioctls. Gamma LUTs are not truly implemented (no GPU gamma support under virtio). Right now these are `=> 0` which gives a false "success". This violates §0 " ENOTTY for unimplemented". DECISION: return `ENOTTY` for SETGAMMA/GETGAMMA since there's no gamma LUT wired. OR keep as 0 deliberately (current behavior).

  → Recommend returning ENOTTY for both, matching AGENTS.md §0 policy. But this may break any compositor probing gamma. Since no staged compositor does, this is safe.

- [ ] **Step 3 (only if Step 2 picks ENOTTY):** Add `DRM_IOCTL_MODE_GETGAMMA => ENOTTY, DRM_IOCTL_MODE_SETGAMMA => ENOTTY,` to dispatcher. Update docs/deferred-work.md to say gamma is now claimed (ENOTTY).

- [ ] **Step 4: Build kernel + probe update (gammaprobe.c expects ENOTTY, returns 0 *somewhere*).** Decision approach: tracked in Task 5 completion.

- [ ] **Step 5: Commit** the gamma-policy decision.

---

### Task 6: Run regression suite + SMP-2 smoke

**Files:**
- None (verification only).

- [ ] **Step 1: Run `cargo run -p tools -- test`** — confirm 27+ passes; only known flaky/unrelated failures (kms_fault, xdg_portal_bus, job_control, curl_probe x2, mount_subsystem, network).
- [ ] **Step 2: Run `cargo run -p tools -- run --smp 2 --no-audio`** for 90s — 0 panics, 0 INV, 0 cr2 faults, no graphical regressions.
- [ ] **Step 3: Confirm `git status`** shows intended files.
- [ ] **Step 4: Commit** (verification only).
