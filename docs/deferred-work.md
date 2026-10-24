# Deferred Tasks & Follow-ups (P1–P6)

This document tracks items explicitly deferred (YAGNI / on-demand) during the implementation of the dynamic input, USB HID, evdev ABI, TTY discipline, graphics/KMS, and session plans (P1–P6).

---

## 1. Multitouch Slots (`EVIOCGMTSLOTS`)
- **Origin**: P3 Task 4 (`docs/superpowers/plans/2026-09-06-dynamic-evdev-abi.md`)
- **Current State**: `input_evdev_ioctl` responds to `0x0A` (`EVIOCGMTSLOTS`) with a 16-byte zero-fill buffer.
- **Trigger Condition**:
  - A touchscreen or touchpad device with multi-touch report descriptors is added to QEMU configuration (`-device usb-tablet` with MT or virtio-input multitouch).
  - A Wayland client or libinput explicitly requests `EVIOCGMTSLOTS` with non-zero buffer capacity and checks slot tracking IDs.
- **Future Implementation Requirements**:
  1. Add `slots: [Slot; 10]` shadow tracking in `InputDev`.
  2. Implement tracking ID assignment, `ABS_MT_SLOT`, `ABS_MT_TRACKING_ID`, `ABS_MT_POSITION_X`, and `ABS_MT_POSITION_Y` in `input_push`.
  3. Support query of active slot tracking IDs via `EVIOCGMTSLOTS`.

---

## 2. Forwarding LED State to Physical Hardware
- **Origin**: P3 Task 3 (`docs/superpowers/plans/2026-09-06-dynamic-evdev-abi.md`)
- **Current State**:
  - `EVIOCLED` / `EVIOCSLED (0x1A)` records the LED byte into the device's `led_state` shadow field in kernel memory.
  - `EVIOCGLED (0x19)` returns this shadow byte.
  - Software round-trips for NumLock / CapsLock state succeed.
- **Trigger Condition**:
  - Keyboard physical LEDs (NumLock / CapsLock indicators) need to reflect state on physical hardware or in QEMU host keyboard indicators.
- **Future Implementation Requirements**:
  1. **PS/2 Keyboard**: Send command `0xED` followed by the LED mask byte (`bit 0: ScrollLock, bit 1: NumLock, bit 2: CapsLock`) via `ps2_keyboard_send_byte`.
  2. **USB HID Keyboard**: Issue USB control transfer `SET_REPORT` (`bmRequestType = 0x21`, `bRequest = 0x09`, `wValue = 0x0200` [Output Report], `wIndex = interface_num`) with the 1-byte LED state payload to the xHCI endpoint.

---

## 3. Advanced TTY Flow Control & Remaining Input Flags
- **Origin**: P4 Task 4 (`docs/superpowers/plans/2026-09-06-dynamic-tty-discipline.md`)
- **Current State**:
  - `termios` flags `ICANON`, `ISIG`, `ECHO`, `ECHOE`, `ECHOK`, `ICRNL`, `OPOST`, and `ONLCR` are fully honored.
  - Line buffering, ERASE (`\b \b`), KILL (`\r\n`), EOF commit, and signals (`SIGINT`, `SIGQUIT`, `SIGTSTP`) work.
  - Flow control characters (`^S`/`^Q`) and rarely-used input transforms are ignored.
- **Trigger Condition**:
  - An interactive terminal application requires software flow control or specific termios transformations and hangs/misbehaves without them.
- **Future Implementation Requirements**:
  1. **Software Flow Control (`IXON` / `IXOFF`)**:
     - Handle `VSTART` (default `^Q`, 0x11) and `VSTOP` (default `^S`, 0x13) in `PtyPair::master_write`.
     - Suspend `slave_write` queue draining on `VSTOP`, resume on `VSTART`.
  2. **`ISTRIP`**: Strip 8th bit from incoming characters when set (`b & 0x7F`).
  3. **`INLCR` / `IGNCR`**: Map `\n` to `\r` or ignore `\r`.
  4. **`ECHOCTL` / `ECHOKE`**: Refine control character visual echo (e.g., echo `^A` through `^_` literally) and visual word erase (`VWERASE` / `^W`).

---

## 4. Multi-GPU PRIME Buffer Sharing & Physical Hotplug
- **Origin**: P5 Tasks 2–3 (`docs/superpowers/plans/2026-09-06-dynamic-graphics-kms.md`)
- **Current State**:
  - Single-GPU (VirtIO-GPU / GOP bridge) scanout with data-driven mode inventory, mode validation in `SETCRTC`, `FRAMEBUFFER_INFO` dynamic resize, and netlink hotplug uevents on `dri/card0`.
  - GEM/PRIME dma-buf handle exchange is stubbed for single-GPU usage.
- **Trigger Condition**:
  - Multi-GPU setup (e.g., secondary PCIe GPU or external display bridge) needing cross-device dma-buf sharing.

---

## 5. Dedicated Input Threading & Session Manager Slices
- **Origin**: P6 Tasks 2 & 4 (`docs/superpowers/plans/2026-09-06-dynamic-session-latency.md`)
- **Current State**:
  - mywima uses render-on-demand and bounded event loop dispatches (1ms timer ticks) avoiding render-lock.
  - VT switching (`VT_ACTIVATE` / `VT_GETSTATE`) maintains active VT states and clears text/graphics mode screens.
- **Trigger Condition**:
  - Heavy multi-window GL scenes causing input frame drops despite render-on-demand, requiring a dedicated OS thread for libinput dispatch.
  - Multi-seat session management or lock-screen idle inhibition.

---

## 6. Test-Harness Serial Output Flakiness (`tools test`)

- **Origin**: found while fixing `getsid` and repairing the `tools test` harness (see §0a of `AGENTS.md` for the sink architecture this depends on).
- **Severity**: low — does not affect the kernel, only the reliability of the suite.
- **Current State**:
  - The harness works: `cargo run -p tools -- test all` boots a guest per test, and output reaches the host via `/dev/ttyS0` (`VNode::SerialDevice` → COM1). Baseline is **32/34**.
  - ~2 tests fail *per run*, and **the failing set is disjoint every run**. Observed sets:
    - run 1: `fence_accept`, `test_musl`
    - run 2: `gbm_test_guest`, `eventfd_basic`
    - run 3: `ptyprobe`, `eventfd_basic`
  - Every failing test passes when run alone (`cargo run -p tools -- test <name>`); `job_control` is 7/7 in isolation.
  - Failure signature is always the same: `Failed to match expected pattern '<X>' within timeout`, on the program's **first** output line, even though `[SHELL READY in ~4.5s]` was seen and the command was sent.
- **Ruled Out**:
  - *Command not delivered.* A failing run still shows the kernel's per-fork `User program exited with code 0` lines, so the test binary ran. The **output** is lost, not the command.
  - *Bash-startup race* (the leading hypothesis). The harness sends the command 500 ms after `[SHELL_READY]`, which `draginit` prints *before* exec'ing `ptyrun`/`bash`, so input can race bash's init (`/etc/inputrc`, `/etc/termcap`, history). Waiting for a real prompt was implemented and made things **worse** — 6 failures instead of 2 — so it was reverted. Do not re-attempt that shape without new evidence.
- **Leading Hypotheses** (unconfirmed):
  1. Serial FIFO overrun under sustained load. The port runs at 38400 baud (divisor 3) and a full suite is 34 back-to-back QEMU boots; bytes written faster than QEMU drains could be dropped rather than back-pressured.
  2. `ptyrun`'s `pump(m, 1)` (`initrd/ptyrun.c:107`) losing a burst between the pty master and fd 1 when many suites run back to back.
  3. Line framing in the harness reader: `BufReader::lines()` (`test_runner.rs`) withholds a partial final line, so a truncated write would present exactly as this timeout.
- **Diagnostic Next Steps**:
  - Compare a failing run's `uefi_debug.log` against the serial stream. If the output is present in debugcon but absent from serial, it is a sink/overrun problem; if absent from both, it is upstream of the shell.
  - Raise the pattern `timeout_secs` for one flaky test to ~120 s. If it then passes, the output is late rather than lost, which discriminates hypothesis 1 from 2/3.
  - Count `[OPEN] path='/dev/ttyS0'` occurrences per run — fewer than expected means the redirection itself failed on that boot.

---

## 7. Graphics/DRM Dead Code & Disconnected Paths

- **Origin**: audit of the display stack asked "is graphics completely linked in?".
- **Answer to that question**: the wiring is essentially complete — every module declared and reachable, all 45 dispatched DRM ioctls have live handlers, and GOP → `Display` → VT → framebuffer plus virtio-gpu probe→negotiate→scanout→flush (2D/blob/3D) are connected end to end. The gaps below are *reachability*, not wiring.
- **Why this went unnoticed**: `drivers/drm/mod.rs:15` is `#![allow(unused)]`, so ~30 dead items emit no build warning at all.
- **Correctly stubbed (do not "fix" without a client)**: `MODE_ATOMIC` → `ENOTTY` (`mod.rs:875`); unknown ioctls → `ENOTTY` (`mod.rs:900`); `GETGAMMA`/`SETGAMMA`/`AUTH_MAGIC` → literal `0`. Not implemented: atomic modesetting, gamma LUTs, DRM auth, dma-buf import/export, PCI aperture arbitration, fbdev damage/vsync/VSBLANK.
- **Highest-value dead paths** (zero callers, verified by whole-tree grep):
  1. ~~**`vt::bind_graphical` / `release_graphical`**~~ **FIXED** — see below.
  2. **`virtgpu::context_create_internal` (`virtgpu.rs:306`)** — dead, while `handle_virtgpu_context_init` (`virtgpu.rs:2410`) calls `g.ctx_create` directly. The "single implementation" that AGENTS.md §0 blesses is the unused one; context creation effectively has two routes. Same for `context_destroy_internal` (`:360`), `get_virgl_ctx_id` (`:2536`).
  3. **`diag_note_target` (`drm/mod.rs:233`)** — was zero-caller while `diag_report_flip` (`:255`) *reads* the table it populates, so `targets=` could only ever print `<none>`. **Now wired** at the page-flip path (`mod.rs:2615`), recording the presented rid and classifying it scanout-sized via the new shared `dims_are_scanout_sized` (the "scanout size" rule lived inline in `res_is_scanout_sized`, which the flip path could not call — both now delegate to one definition, avoiding a third copy alongside the `virtgpu_display::mode_valid` twin).
     **Not dynamically observed**: `diag_report_flip` only emits when `DIAG_SUBMITS > 0`, and that counter is fed only from the virtio **3D execbuffer** path (`virtgpu.rs:1477`). No staged test drives 3D execbuffer, so zero `[flipdiag]` lines appear in any run and the populated `targets=` field has not been seen in practice. The wiring is verified statically (the function has a caller and compiles); the data path is untested.
- **`GRAPHICAL_VT` binding — FIXED, but still unexercised.** Nothing called `bind_graphical`, so `GRAPHICAL_VT` was permanently 0 and the reclaim branch at `keyboard.rs:347` (`graphical != 0 && graphical != vt`) was unreachable. Wired as follows:
  - `VT_ACTIVATE` (`fsio.rs:1652`) now calls `bind_graphical(n)`, so a userspace VT takeover records the binding. This is the compositor path (`VT_OPENQRY` then `VT_ACTIVATE`) and the only one that should bind — the kernel's own `Ctrl+Alt+F<n>` still calls `vt::switch_vt` directly, so a user switching consoles does not record themselves as the graphical client.
  - `KDSETMODE(KD_TEXT)` (`fsio.rs:1678`) and DRM `handle_drop_master` (`drm/mod.rs:2854`) both call `release_graphical`, so the binding is dropped when the client hands the console back instead of going stale.
  - **Caveat: no staged client takes a VT.** `strings` on `mywima` and `seatd` finds no `VT_ACTIVATE`/`VT_OPENQRY`, and no `[VT] activate` line appears in boot logs. The path is now correct and reachable, but nothing exercises it yet, so `Ctrl+Alt+F<n>` reclaim still does not fire in practice. Making it real needs a client that performs VT takeover (or teaching the compositor to), which is a userspace change, not a kernel one.
- **Stale/alarming comments and statics**:
  - `SCANOUT_READBACK` (`drm/mod.rs:343`) — 35 lines of doc for a readback path, never read or written; its own comment says it "must go back to false once the GL scanout path is understood", and it was never enabled.
  - `PINNED_SCANOUT_RID` (`:319`) — written once (`:1250`), never read.
  - `SEEN_3D` (`:327`, "TEMP-DIAG"), `PRESENT_MADE` (`:354`) — never referenced.
  - `virtgpu_ioctl.rs:14-19` `NOT_YET_PORTED` is **stale**: it lists `map_ioctl`, `transfer_from_host`, `transfer_to_host`, `context_init` as unported, but all four are implemented in `virtgpu.rs` and dispatched.
  - `terminal.rs:102` `flush_dirty` claims to be the post-write hot path; it has no callers — `write_str`/`flush_vt` call the private `flush()`.
- **Duplicated logic that can drift**:
  - `virtio_gpu_conn_mode_valid` exists twice: transcribed+tested at `virtgpu_display.rs:132` (zero production callers) and reimplemented inline at `drm/mod.rs:1540-1557` with its own `const TOL: u32 = 16`. They agree today.
  - `Terminal::backspace` (`terminal.rs:143`) is dead while `Vt::backspace` (`vt.rs:640`) is live — same name, two implementations, only one wired.
- **Also dead** (zero callers, lower value): `Terminal::{font,cols,rows,cycle_font,erase_cursor,clear_all}` and `flush_dirty`; `Display::{draw_string,scroll_up,clear}` (`graphics/mod.rs`); `Color::with_alpha` and the `Color::BLACK..MAGENTA` consts (live paths use `ANSI_*`); `FontKind::name` and `Bitmap8x8` (only reachable via dead `cycle_font`); `gpu::last_resp_type`, `types::find_dumb_mut`, `drm/mod.rs:3492 align_up_32` (`virtgpu_gem.rs:112` inlines the same arithmetic), `drm/mod.rs:3497 current_pid` — the last is fortunate, since it is exactly the `current_thread()` pattern AGENTS.md §5 warns about.
- **Diagnostic note**: `MODE_GETCRTC` returns `EINVAL` on an uninitialised device (`mod.rs:1434`) while nine sibling handlers return `ENOSYS`. Cosmetic, but AGENTS.md §0 makes errno choice load-bearing for client fallback.

---

## 8. `abi_pin!` Cannot Validate Its Own Reference

- **Origin**: found while fixing `DRM_IOCTL_SYNCOBJ_WAIT_BINARY` (see §9).
- **Problem**: AGENTS.md §0 makes `abi_pin!` the mechanism that stops a client hitting the wrong ioctl handler. But an assert only proves the code agrees with the *pinned* value. When the pinned value is itself wrong, the assert passes and the safety net is decorative. That is exactly what happened: `SYNCOBJ_WAIT_BINARY` was pinned to `0xC01064BD` — a number `<drm/drm.h>` never defines for any syncobj — and the pin "verified" it against that same wrong value.
- **Also**: the pin block claims its reference is "libdrm 26.2", but this host only ships **libdrm 2.4.134**. Any future pin edit needs a reference of the stated vintage, or the check is against a different ABI than intended.
- **Mitigation in place**: `scripts/verify-drm-pins.sh` expands the real macros with the C preprocessor and diffs them against the pins. Independent of the kernel crate — unlike `scripts/generate-drm-ioctls.sh`, which derives from `drm-uapi` itself and would reproduce the same mistake. Run it after any `abi_pin!` or struct-layout edit.
- **Current result**: `pins=52 verified=52 mismatched=0 unverifiable=0`.

---

## 9. `DRM_IOCTL_SYNCOBJ_WAIT_BINARY` Removed (was a phantom)

- **Fixed**: `drm-uapi/src/lib.rs` defined `DRM_IOCTL_SYNCOBJ_WAIT_BINARY` as an alias of `DRM_IOCTL_MODE_CREATEPROPBLOB`.
- **Ground truth** (`/usr/include/drm/drm.h:1254-1260`): the syncobj ioctls are `CREATE(0xBF) .. SIGNAL(0xC5)` and nothing more — there is **no** `WAIT_BINARY` in the userspace ABI, so libdrm can never emit it. `0xC6` is `MODE_CREATE_LEASE` (`drm.h:1262`), not a syncobj nr.
- **Why it mattered**: it named a nonexistent ioctl while carrying a real one's number, and `abi_pin!` certified it — a green check that could never fail. It had **zero consumers** (no dispatcher arm, no other reference), making it a trap for whoever implemented it next: they would find `0xBD` taken with no explanation.
- **Scope correction**: an audit initially claimed a real client would hit `handle_create_prop_blob` and suffer silent corruption. That was overstated — no libdrm client sends `WAIT_BINARY`, so this was never a live corruption path. It was a misleading constant plus a pin that gave false assurance, which is what was actually fixed.
- **Verified**: all 9 syncobj/propblob pins independently re-derived from the header structs — 0 mismatches; rest of the family was already correct.

---

## 10. Job Control: `SIGTTOU` on a background write

- **Origin**: `initrd/jobctltest_stop.c`, test 6.
- **State**: everything below `SIGTTOU` works and is covered by the suite —
  `Stopped` status, `SIGTSTP`/`SIGSTOP`/`SIGCONT`, `waitpid(WUNTRACED/WCONTINUED)`,
  and `SIGTTIN` on a background read all pass.
- **`SIGTTOU` on `write(2)` is gated on `TOSTOP`**, matching Linux: an unconditional
  stop on background writes deadlocks ordinary shell output, because a shell that
  has merely backgrounded a job still writes its own prompt and would be stopped
  by its own diagnostics. (That regression was real — it broke `ptyprobe` and
  `job_control` — and is why the gate exists.)
- **Open**: with `TOSTOP` set the stop still does not fire. The guest has more
  than one pty (the harness shell's stdin/stdout and the program's are separate
  descriptors), and the child's `write(1, …)` resolves to a different `PtyPair`
  than the one `tcsetattr` set `TOSTOP` on. Verified by logging the `PtyPair`
  reached in `pty_slave_write`: `pty_id=0`, `oflag=0o5` (no `TOSTOP`), while the
  test had confirmed `TOSTOP` readable-back on its own descriptor.
- **Next step**: establish whether `tcsetattr` and `write` reach *different*
  `PtyPair` instances for the same terminal (fd aliasing at open time), or the
  `TOSTOP` bit is dropped on the pty the write does reach. The read-side
  `SIGTTIN` path already compares `pgid` against `pty.pgrp` correctly, which
  points at descriptor identity rather than the flag itself.

---

## 11. Summary Matrix

| Feature | Plan | Status | Trigger Condition |
| :--- | :--- | :--- | :--- |
| **`EVIOCGMTSLOTS` & MT slots** | P3 Task 4 | Zero-filled stub | Touchscreen hardware or Wayland MT consumer |
| **Physical LED output (PS/2 & USB)** | P3 Task 3 | Shadow state only | Physical hardware keyboard indicator requirement |
| **TTY `IXON` / `IXOFF` flow control**| P4 Task 4 | Stub / Not gated | App explicitly needing `^S`/`^Q` pause |
| **TTY `ISTRIP`, `INLCR`, `ECHOCTL`** | P4 Task 4 | Default fallback | Legacy terminal application requirement |
| **Cross-GPU PRIME dma-buf sharing** | P5 Task 3 | Single-GPU local | Multi-GPU hardware / external display adapter |
| **Dedicated Libinput OS Thread** | P6 Task 2 | Event-loop bounded | GL render stalls persisting across frame boundaries |
| **Idle Inhibition & Multi-seat** | P6 Task 4 | seatd-sized slices | Lock screen or multi-seat requirements |
| **Harness serial-output flake** | ad hoc | ~2/34, disjoint per run | Suite reliability; blocks trusting a green/red run |
| **Job control: stop/continue** | ad hoc | Stop/continue/`WUNTRACED`/`SIGTTIN` done; `SIGTTOU` open (§10) | `Ctrl+Z`, `fg`/`bg`/`jobs` |
| **Graphics/DRM dead code** | ad hoc | ~24 zero-caller items; `GRAPHICAL_VT` + `diag_note_target` wired | `context_create_internal` **removed**; no client takes a VT; no 3D test exercises flipdiag |
| **`abi_pin!` self-validation** | ad hoc | `verify-drm-pins.sh` added | Any future `abi_pin!` or struct-layout edit |
