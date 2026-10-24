# P3 — evdev ABI Completion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Grow the evdev ioctl surface from the current libinput-minimum subset to a complete-enough ABI: real key state, absolute axes, LEDs, and honest grabs — so libinput stops guessing and tools like `evtest`/`libinput debug-events` work against our nodes.

**Architecture:** All state already flows through `input_push`; what's missing is queryable device state. Add per-device shadow state (pressed-keys bitmap, ABS values, LED state) updated in `input_push`, served by the ioctls. No new locks: shadow state lives inside the existing `InputDev` struct behind its `SpinLock`. Depends on P1 (caps-driven ioctl dispatch already done there).

**Tech Stack:** `kernel/src/arch/x86_64/syscalls/fsio.rs:518-612`, `kernel/src/drivers/keyboard.rs` device structs. Reference: Linux `include/uapi/linux/input-event-codes.h` numbers only (no code copied).

---

### File map

- Modify: `kernel/src/drivers/keyboard.rs` — `InputDev` gains `key_state: [u8; 96]`, `abs_vals: [i32; 8]`, `led_state: u8`.
- Modify: `kernel/src/arch/x86_64/syscalls/fsio.rs` — `input_evdev_ioctl`: real `EVIOCGKEY`, `EVIOCGABS`, `EVIOCGLED`+`EVIOCLED`, honest `EVIOCGRAB`, `EVIOCGPROP` bits, ABS bitmap in `EVIOCGBIT`.
- Test: guest-side `evtest`-equivalent — no evtest binary exists in initrd; verify via a 30-line C program in `initrd/` compiled by the existing cross toolchain, or via libinput's own behavior change (`DeviceAdded ... ptr=1` for the mouse node).

---

### Task 1: Shadow key state + EVIOCGKEY

**Files:**
- Modify: `kernel/src/drivers/keyboard.rs`, `kernel/src/arch/x86_64/syscalls/fsio.rs`

- [ ] **Step 1: Track pressed keys.** In `input_push`, when `type_ == EV_KEY(1)`: set/clear bit `code` in `dev.key_state` (guard `code < 768`). SYN/REL/ABS events don't touch it.
- [ ] **Step 2: Serve it.** `EVIOCGKEY (nr 0x18)` currently returns 32 zero bytes. Return `key_state` clamped to the ioctl size field (existing `put()` helper already clamps).
- [ ] **Step 3: Verify.** Boot, press-and-hold a key in the terminal, check libinput no longer misclassifies after disconnect/reconnect cycles. Build:

Run: `cargo run -p tools -- build-kernel 2>&1 | tail -1`

- [ ] **Step 4: Commit**

```bash
git add kernel/src/drivers/keyboard.rs kernel/src/arch/x86_64/syscalls/fsio.rs
git commit -m "feat(evdev): real EVIOCGKEY from per-device shadow state"
```

---

### Task 2: Absolute axes (EVIOCGBIT ABS + EVIOCGABS)

**Files:**
- Modify: same two files.

- [ ] **Step 1: Advertise ABS where true.** Tablet-class devices (P2's layout says absolute) set `ev` bit 3 and ABS bitmap bits 0 (X), 1 (Y). Add `abs_info: [(min,max,fuzz,flat,res); 8]` to caps; `EVIOCGBIT(3)` serves the bitmap.
- [ ] **Step 2: Serve `EVIOCGABS(nr 0x40..0x5F)`.** `abs = nr - 0x40`; return `struct input_absinfo { value, minimum, maximum, fuzz, flat, resolution }` (6×u32 LE = 24 bytes) from shadow `abs_vals[abs]` + caps range. Update `abs_vals` in `input_push` for `type_ == EV_ABS(3)`.
- [ ] **Step 3: Emit EV_ABS for tablets.** P2 currently converts ABS→REL in the driver. Keep REL for compat, but ALSO emit the raw `EV_ABS/ABS_X/ABS_Y` + SYN so multitouch-capable consumers see the real device. (Do NOT add MT slots yet — that's Task 4.)
- [ ] **Step 4: Verify.** `DeviceAdded` for the tablet node should now show pointer+absolute capability; cursor still moves (REL path intact).

Run: `cargo run -p tools -- build-kernel 2>&1 | tail -1 && cargo run -p tools -- build-fs 2>&1 | tail -1`

- [ ] **Step 5: Commit**

```bash
git add kernel/src/drivers/keyboard.rs kernel/src/arch/x86_64/syscalls/fsio.rs kernel/src/drivers/usb/xhci.rs
git commit -m "feat(evdev): absolute axes bitmaps and EVIOCGABS"
```

---

### Task 3: LEDs and honest grabs

**Files:**
- Modify: `kernel/src/arch/x86_64/syscalls/fsio.rs`, PS/2 + USB keyboard drivers.

- [ ] **Step 1: LED state.** `EVIOCLED (nr 0x1A)` stores the u8 mask into `dev.led_state`; `EVIOCGLED (0x19)` returns it. NumLock/CapsLock toggles then round-trip. Forwarding to physical LEDs (PS/2 `0xED` command, USB boot `SET_REPORT`) is explicitly out of scope — note as follow-up in the commit message.
- [ ] **Step 2: Honest EVIOCGRAB.** Track `grabbed_by: Option<usize>` (opener key) per device. `EVIOCGRAB(1)` fails with `EBUSY` if another opener holds it; `input_push` fan-out skips non-grabber queues while grabbed. Today it always "succeeds" and ignores the grab — this breaks `libinput debug-events --grab`-style tools.
- [ ] **Step 3: Verify.** Two-opener test still works ungrabbed (seatd + libinput co-read, the P1 invariant); grabbed device delivers to grabber only.

Run: `cargo run -p tools -- build-kernel 2>&1 | tail -1`

- [ ] **Step 4: Commit**

```bash
git add kernel/src/arch/x86_64/syscalls/fsio.rs kernel/src/drivers/keyboard.rs
git commit -m "feat(evdev): LED state and honest exclusive grabs"
```

---

### Task 4: Multitouch slots (only if a consumer needs it)

Do NOT implement speculatively. Trigger condition: a touchscreen device appears in QEMU config or a Wayland client requests `EVIOCGMTSLOTS` with nonzero size. When triggered: `EVIOCGMTSLOTS` returns per-slot tracking IDs, `ABS_MT_SLOT/TRACKING_ID/POSITION_X/Y` events flow through `input_push`, shadow `slots: [Slot; 10]`. Until then, the zero-fill stays.

---

## Self-review

- Spec coverage: key state, ABS, LEDs, grabs each have a task; MT correctly deferred with a trigger condition (YAGNI).
- No placeholders: exact nr values, exact struct layouts, exact files.
- Type consistency: shadow state defined once in Task 1's struct, extended in later tasks.
