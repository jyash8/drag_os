# P6 — Session, VT, and Input-Latency Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the `libinput ... lagging behind by ~19s` class of problems structurally (input dispatch isolated from render stalls), and make session/VT handling real instead of stubbed — last because everything else must work first.

**Architecture:** Two independent halves. (A) Latency: split mywima's single-threaded render+input loop so libinput dispatch runs on a bounded budget every iteration regardless of GL stall; no new threads initially (calloop timer re-budgeting), threads only if measurement says timers aren't enough. (B) Session: seatd already owns the seat; add VT switch semantics (save fb state, pause clients, switch, restore) behind the existing `ChangeVt` action that's currently wired to nothing real.

**Tech Stack:** mywima `kms.rs` event loop (~1438 lines), seatd/libseat session, kernel VT ioctls (`KDSETMODE`, `VT_ACTIVATE` — check current stubs via `grep -rn "VT_\|KDSETMODE\|KDGKBMODE" kernel/src --include="*.rs"`).

---

### File map

- Modify: `/home/jyash8/Projects/mywima/src/backends/kms.rs` — event-loop budgeting, optional input thread.
- Modify: `kernel/src/...` VT ioctls (locate via the grep above) — real switch semantics.
- Measure with: `[input] Key` → `[foot] key` arrival delta (both already logged with ms timestamps in the event loop; add an explicit `eprintln!` delta line if needed).

---

### Task 1: Measure first (no code changes to scheduling yet)

- [ ] **Step 1: Quantify the stall.** Interactive run: hold a key (autorepeat) for 5s during heavy rendering (two terminals + wallpaper + window drag). Collect: max `[input] Key`→`[foot] key` delta, count of `lagging behind` warnings, frame numbers where input lines cluster (post-stall bursts prove dispatch starvation vs. steady slowness).
- [ ] **Step 2: Decide the mechanism.** If deltas spike only during GL-heavy frames → render starves dispatch (fix: Task 2). If deltas are uniformly large even idle → the whole loop is under-clocked (check LAPIC tick rate / calloop timer source first — cheaper than restructuring).

No commit (measurement only; record numbers in the task tracker).

---

### Task 2: Bounded input budget per loop iteration

**Files:**
- Modify: `/home/jyash8/Projects/mywima/src/backends/kms.rs` event loop.

- [ ] **Step 1: Dispatch-before-render with cap.** Reorder the loop to: drain libinput (bounded N events per iteration, N=64), dispatch Wayland, then render at most one frame, then repeat. Today render-on-demand can sit inside GL for unbounded time while input queues. The bound keeps worst-case input latency ≈ one frame time.
- [ ] **Step 2: Verify against Task 1 numbers.** Same 5s autorepeat test: max delta must drop by an order of magnitude, zero `lagging behind` warnings. If timers alone don't fix it, THEN split input onto its own thread (calloop channel → main loop); do not preemptively thread.
- [ ] **Step 3: Commit**

```bash
git add src/backends/kms.rs
git commit -m "perf(compositor): bounded input dispatch ahead of render"
```

---

### Task 3: Real VT switching behind the existing action

**Files:**
- Modify: kernel VT ioctl stubs, mywima `ChangeVt` handling in `kms.rs`.

- [ ] **Step 1: Kernel semantics for `VT_ACTIVATE(n)`.** Save current scanout buffer handle, `KDSETMODE(KD_TEXT)` equivalent (release DRM master to the switcher), pause client frame callbacks, switch `FRAMEBUFFER_INFO` to the VT's buffer, `KDSETMODE(KD_GRAPHICS)` on switch back, resume. Text-mode rendering for non-graphical VTs is explicitly out of scope — non-graphical VTs show a static "VT N (no console)" slate until/unless fbcon exists (separate project).
- [ ] **Step 2: Wire `ChangeVt`.** The compositor already intercepts `Ctrl+Alt+F1..F12`; route to the ioctl instead of the current stub. VT1 = compositor; VT2+ = slate.
- [ ] **Step 3: Verify.** `Ctrl+Alt+F2` shows slate, input still dispatches (no stall), `Ctrl+Alt+F1` returns to an intact desktop (no dead windows, focus preserved).
- [ ] **Step 4: Commit**

```bash
git add kernel/src/... mywima/src/backends/kms.rs
git commit -m "feat(session): real VT switching with scanout save/restore"
```

---

### Task 4: Session lifecycle (logind-shaped, seatd-sized)

Only after Tasks 1–3. Scope: session pause/resume on seat switch (`libseat` events already plumbed — `suspend()/resume()` calls exist in `kms.rs:378-386`), idle inhibition hooks, and lock-screen signaling. Each is a small vertical slice; do not build a session manager — seatd remains the manager, mywima just honors its events completely. Trigger: implement a slice when a real use case (screen lock, multi-seat) demands it.

---

## Self-review

- Spec coverage: latency (Tasks 1–2), VT (Task 3), session lifecycle (Task 4, correctly scoped as on-demand slices).
- No placeholders: measurement comes before mechanism; threading is conditional on numbers, not assumed.
- Type consistency: reuses existing `ChangeVt` action, existing suspend/resume plumbing, existing log lines as instruments.
