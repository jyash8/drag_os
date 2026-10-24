# AGENTS.md — DragOS Kernel Invariants

This kernel prioritizes **determinism**: every execution path must follow
a predictable trail. The invariants below are rules that must not be
broken without a stated reason, a safety argument, and a reference to
where the invariant is established.

When modifying the kernel, verify your changes against these invariants.
If you must break one, state: (1) why it is safe, (2) where the
replacement invariant is established.

---

## Build & Test Quick Reference

All build, run, and test operations are via the `tools` crate:

```
cargo run -p tools -- build          # Build fs, kernel, bootloader
cargo run -p tools -- build-kernel   # Build kernel (release, x86_64-unknown-none)
cargo run -p tools -- build-fs       # Build filesystem staging (initrd.tar & disk.img)
cargo run -p tools -- run             # Launch QEMU (2GB RAM, SMP 2, VirtIO, Audio)
cargo run -p tools -- test            # Run automated test suite
```

Use `cargo run -p tools -- --help` for full command list.

---

## 0a. The test harness talks to the guest over COM1 — and only COM1

`cargo run -p tools -- test` drives QEMU with `-serial mon:stdio` and both
writes and reads that stream. The kernel side is **asymmetric**, and the
asymmetry is silent:

| direction | path |
|---|---|
| harness → guest | QEMU serial → COM1 RX IRQ → `tty::input_byte(1, ..)` → VT 1 |
| guest → harness | a **`TtyDevice` write** → VT cell buffer + debugcon |

A `TtyDevice` write — which is what fds 0/1/2 are, and therefore what *every*
program's stdout is — goes to `vt::write_str` **and** `dprint!`. `dprint!` is
the debugcon port `0x402`, wired by QEMU to `file:uefi_debug.log`
(`debugcon.rs:10`). It does **not** reach COM1. Only `print!` does.

**So no userspace stdout is visible to the harness by default.** Test-mode
output is redirected to `/dev/ttyS0` (`VNode::SerialDevice`, the COM1 port
itself) in `userspace/draginit/src/main.rs: exec_shell`.

**Rules:**
- Anything a test asserts on must reach serial. Adding a program whose output
  the harness needs to read means adding a `/dev/ttyS0` redirection, not
  relying on stdout.
- Do not "tidy" the `headless: true` in `test_runner.rs`. `spawn_headless()`
  only redirects stdio; `build_command()` branches on `headless`, and `false`
  makes it call `clean_test_mode()` — deleting the flag the harness just wrote.
- Do not reintroduce a `print!` into the `TtyDevice` write arm. That duplication
  was deliberately undone: it made full-screen VT dumps unreadable by mixing
  program bytes into the kernel's `[kms]`/`[input]` lines (`vfs.rs`, write arm).

**Symptom when broken:** every test fails identically with "Timed out waiting
for shell prompt" — including tests that pass in isolation. That uniformity is
the tell: a real kernel bug fails one test, a sink bug fails all of them.

**Also note:** `mount(2)` is now dispatched (`165 => sys_mount`) and the pseudo mount table is consulted
by the lookup path. Real FAT support is best-effort (`vfat` shim returns an empty directory), but the
mount call itself no longer fails with ENOSYS and `/efi` is reached if a vfat child actually exists.
(`/proc` only *appears* to mount — `/proc/self/fd/N` readlink is faked inside `sys_readlinkat`.)

**Not every user-visible failure is a test failure.** The same sink problem
silently disabled `ssh_probe`/`sshd_probe` for as long as it stood, because
their output was as unreachable as the test program's. If a feature "does not
work over SSH", check the sink before concluding the daemon is broken.

---

## 0. Userspace ABI — a program must not be able to tell this isn't Linux

A userspace program (libdrm, Mesa, foot, anything) compiles against
`<drm/*.h>` and passes the resulting macro to `ioctl(2)` **verbatim**. There is
no negotiation and no fallback: if the kernel's number differs, the ioctl is
either unservable or — much worse — serviced by the *wrong handler*, which
returns success while doing something else. The second case is not a bug that
shows up as an error; it is silent corruption that surfaces much later as
garbage geometry or a hang.

**Rules:**
- Every `DRM_IOCTL_*` constant is computed from the `_IOC` encoding of
  (direction, `'d'`, request number, `size_of::<payload struct>()`) — the same
  arithmetic `<asm-generic/ioctl.h>` does — never a hand-written hex literal.
  A struct whose layout changes therefore changes the encoded size with it.
  Established: `crates/drm-uapi/src/lib.rs` (`ioc`/`drm_ioctl`/`drm_ioctl_t`).
- Every number is pinned to the value libdrm actually sends by the `abi_pin!`
  block in the same file. Editing a struct layout or a request number without
  updating that reference is a **compile error**, not a runtime surprise. This
  is the mechanism that makes the rest of this section self-enforcing.
- **`abi_pin!` cannot validate its own reference, so run
  `scripts/verify-drm-pins.sh` after editing the pin block or any probed struct.**
  An assert only proves the code agrees with the *pinned* value. `SYNCOBJ_WAIT_BINARY`
  was pinned to a number `<drm/drm.h>` never defines, and the pin "verified" it
  against that same wrong value — a green check structurally incapable of failing.
  The script expands the real macros with the C preprocessor and diffs them. It
  is independent of the kernel crate; `scripts/generate-drm-ioctls.sh` is **not**,
  since it derives from `drm-uapi` itself and would reproduce the same mistake.
  Note the pin block's stated reference is **libdrm 26.2**; this host ships
  **2.4.134**, so confirm the vintage before treating a diff as authoritative.
- The dispatcher matches the **full 32-bit value**, so direction and size bits
  matter as much as `nr`.
- An ioctl that exists upstream but is unimplemented here must return
  **`ENOTTY`**, never `EINVAL` and never a success. `ENOTTY` is what libdrm and
  Mesa probe for to fall back gracefully. `EINVAL` reads as "you sent garbage".
  Established: `drm_ioctl`'s catch-all arm in `crates/kernel/src/drivers/drm/mod.rs`.
- Do **not** invent ioctls. Linux has no `VIRTGPU_CONTEXT_CREATE`/
  `CONTEXT_DESTROY`; a virgl context is created implicitly by the first
  `EXECBUFFER` and a Venus context by `VIRTGPU_CONTEXT_INIT`. Claiming a
  request number that upstream assigns to something else is how `RESOURCE_INFO`
  came to silently create a context instead of answering. Establishing a
  context internally is fine; giving it an ioctl is not.
- Guest test programs must `#include "drm_ioctls.h"`, which is generated from
  this crate by `scripts/generate-drm-ioctls.sh`. Hand-copied hex literals in
  `initrd/*.c` are how a probe and the kernel ended up agreeing on a wrong
  number and passing a test that proved nothing. Run the generator after any
  change to the constants.
- Probe structs are part of the ABI too. A field-order mistake in a probe's own
  `struct` silently tests the wrong thing: `mesarepro` declared `uint64_t addr`
  before the `uint32_t` fields, so 8-byte alignment inserted 4 bytes of padding
  and the kernel read `size = 0` and rejected every capset query. Declare
  probe structs in the same order as the kernel's.

**If you break this:** clients fail in ways they cannot diagnose. Two real
instances: `MODE_GETGAMMA` aliased `MODE_CURSOR` (a cursor update returned 0
having done nothing), and `drm_virtgpu_execbuffer` carried a 4-byte `pad` Linux
does not have, which shifted the whole syncobj tail and made every Venus/virgl
submit read 8 bytes past the end of the client's payload.

**Corollary — correct numbers are necessary, not sufficient.** Fixing the ABI
made a previously-latent routing bug fatal. `handle_virtgpu_execbuffer` picked a
context for a plain (non-`RING_IDX`) submit by matching `capset_id ==
VIRTIO_GPU_CAPSET_VIRGL`; a real client asks for **VIRGL2** (2), matched nothing,
and the kernel built a *second* context that the client did not know about — so
the compositor drew into a context the host never gave it and the screen went
black. A plain submit now selects any non-Venus context
(`virtgpu.rs`, `handle_virtgpu_execbuffer`). The general rule: when a client
enumerates an ID space, accept every value in it rather than one hard-coded
member. A narrow match is a silent no-match.

---

## Mywima Dependency Invariants

- **Always optimize mywima's dependencies for size**: Disable unused crate default features (e.g. in `smithay`, `image`, and other dependencies), strip unneeded image decoders / protocols / backends, and ensure binary footprint on the root filesystem and initrd remains minimal.

---

## 1. Lock Ordering

All spinlocks must be acquired in a consistent global order. Violating
this produces cross-core deadlocks that are non-deterministic and hard
to reproduce.

**Established order (outermost → innermost):**

```
SCHEDULER  →  CPUS[k]  →  wait-queues (KEYBOARD/CHILD/PIPE/FUTEX)  →  process.lock()
```

**Rules:**
- Never hold `CPUS[k]` and then acquire `SCHEDULER`. The reverse
  (`SCHEDULER` → `CPUS[k]`) is the only permitted direction.
  Established: `smp.rs:376-378` (`timer_tick_on_core`).
- Never call `current_thread()` or `set_current_thread()` while
  already holding `CPUS[cpu_id]`. These functions lock `CPUS` internally.
  Use the already-held `cpu_slot` guard directly.
  Established: `spinlock.rs:26-43` (non-reentrant SpinLock).
- `process.lock()` is always the **innermost** lock. Always take
  `SCHEDULER` (if needed) before `process.lock()`, never the reverse.
  Established: `tasks.rs:496-501` (exit_current_thread comment).
- Wait-queue locks (`KEYBOARD_WAIT_QUEUE`, `CHILD_WAIT_QUEUE`,
  `PIPE_WAIT_QUEUE`, `FUTEX_WAIT_QUEUE`, `INPUT_WAIT_QUEUE`) are taken
  **after** `SCHEDULER` and `CPUS[k]` but **before** `process.lock()`.
  Established: `smp.rs` park branch.
- **Data-queue locks vs `SCHEDULER` (`STDIN_BUFFER`, `INPUT_EVENT_QUEUE`):**
  producers take the data queue, push, **release it**, and only then take
  `SCHEDULER` (via `wake_*_waiters`). Park branches hold
  `SCHEDULER → CPUS[k]` and probe the data queue with **`try_lock`
  only — never a blocking lock**. On try_lock failure they treat the
  queue as non-empty (the holder is mid-push and its wake-up will find
  them registered in the wait queue). This asymmetric pattern is what
  makes the producer/park pair deadlock-free despite being "reversed".
  Established: `smp.rs` Keyboard + Input park branches;
  `ps2_mouse.rs` / `usb/xhci.rs` `push_event`.

**Exception:** `exit_current_thread` takes `PROCESS_TABLE` briefly,
drops it, then takes `process.lock()`, drops it, then takes `SCHEDULER`.
This is safe because `PROCESS_TABLE` is never held across a scheduling
boundary. Established: `tasks.rs:503-519`.

---

## 2. Interrupt Disable/Enable

The kernel uses a cooperative interrupt-masking model. Spinlocks
automically disable interrupts on acquire and restore on release.

**Rules:**
- `SpinLock::lock()` calls `cli()`; `Drop` calls `sti()` only if
  interrupts were enabled before lock.
  Established: `spinlock.rs:27-28, 103-105`.
- `syscall_entry` runs with `IF=0` (masked by `IA32_FMASK = 0x200`).
  The entire Rust dispatcher is non-preemptible.
  Established: `syscalls.rs:136`.
- The only place a syscall explicitly re-enables interrupts is
  `block_current_thread` (`sti; int 0x81`) — required so keyboard/serial
  IRQs can fire while waiting for input. The `sti` takes effect after
  the next instruction (`int 0x81`), which is an interrupt gate that
  re-disables `IF`. So the yield handler runs with `IF=0`.
  Established: `tasks.rs:398-402`.
- `cpu_idle_loop` calls `sti()` before `hlt`. Required so the LAPIC
  timer or any IRQ can retire the `hlt`.
  Established: `smp.rs:601-604`.
- `ap_entry` calls `sti()` before entering `ap_idle_loop`.
  Established: `smp.rs:361`.
- Timer/keyboard/COM1 IDT entries are **interrupt gates** (`type_attr
  = 0x8E`), which auto-clear `IF` on entry. No handler needs to
  explicitly `cli()`.
  Established: `idt.rs:32`.

**If you break this:** you either deadlock (spinlock held across an
interrupt that tries the same lock) or you lose IRQs (IF never
re-enabled after a handler). Both are fatal and non-deterministic.

---

## 3. Timer — One-Shot with Rearm

The LAPIC timer is configured in **one-shot mode**, not periodic.
Every timer interrupt handler must re-arm the timer.

**Rules:**
- `lapic_timer_start` writes `LAPIC_LVT_TIMER = vector` (bit 17 = 0
  = one-shot). Established: `apic.rs:148-151`.
- `timer_rust_handler` must call `lapic_timer_rearm()` after `lapic_eoi()`
  and before scheduling work. Removing the rearm call dead-stalls the
  kernel — the timer fires once and never again.
  Established: `time.rs:204-206`.
- `yield_rust_handler` does **NOT** call `lapic_eoi()` or rearm — it
  handles `int 0x81` (software interrupt, no LAPIC involvement).
  Established: `time.rs:192-196`.
- EOI order: `lapic_eoi()` → `lapic_timer_rearm()` →
  `timer_tick_on_core()`. This is the SDM-recommended sequence.
  Established: `time.rs:204-206`.

**If you break this:** the kernel either deadlocks (no timer = no
preemption) or floods with interrupts (rearm called twice = double-fire).

---

## 4. Context Switch Frame Layout

The timer/yield interrupt handlers save and restore exactly 15 GPRs
(r15..rax) on the kernel stack, followed by the CPU's iretq frame
(RIP, CS, RFLAGS, [RSP, SS]).

**Rules:**
- The GPR save order in `timer_interrupt_handler` and
  `yield_interrupt_handler` is: r15, r14, r13, r12, r11, r10, r9, r8,
  rbp, rdi, rsi, rdx, rcx, rbx, rax. The restore is the exact reverse.
  Established: `time.rs:102-116` (push), `time.rs:120-134` (pop).
- `CpuContext` (`tasks.rs:100-109`) is `#[repr(C)]` with fields in
  this same order. Any code that reads/writes the frame via
  `CpuContext` must match this layout exactly.
- `thread.stack_ptr` points to the rax slot (lowest address). The
  iretq frame starts at `stack_ptr + 15*8 = stack_ptr + 120`.
- For **user-mode** interrupts, the CPU pushes 5 words (RIP, CS,
  RFLAGS, RSP, SS). For **kernel-mode** interrupts, only 3 words
  (RIP, CS, RFLAGS). `can_deliver()` checks `CS == USER_CODE_SELECTOR`
  to distinguish. Signals are only delivered on user-mode frames.
  Established: `tasks.rs:618-626`.

**If you break this:** `iretq` returns to garbage addresses, or signal
delivery corrupts the user's register state.

---

## 4b. `swapgs` on every `extern "x86-interrupt"` handler entering from user mode

Rust's `x86-interrupt` ABI does **not** emit `swapgs`. The IDT entries
are interrupt gates (auto-clear IF), but GS is left exactly as the user
had it. Any `x86-interrupt` handler that can fire from user mode must
swap GS on entry and again on every exit path that returns to user via
`iretq`. Panicking handlers don't need it.

**Rules:**
- On entry, check `(stack_frame.code_segment & 3) == 3`. If true, swapgs.
- Save the `from_user` flag; mirror the swapgs on **every** `return` path.
- If the handler exits via `switch_to_stack_and_iretq` (e.g. calling
  `exit_current_thread`), that helper does its own swapgs when returning
  to user — entering the same handler with swapgs + exiting via that
  helper = 2 swaps, balanced. Don't add a third.
- Established: `interrupts.rs:104-152` (page fault handler).
- The timer/yield naked handlers in `time.rs` already do their own
  conditional swapgs. The `x86-interrupt` keyboard/COM1 handlers do it
  too. They are correct.

**If you forget:** After one user PF that triggers
`exit_current_thread`, `KERNEL_GS_BASE` ends up holding whatever the
user's GS was (often 0). The next `syscall` on that core tries to write
`gs:[8]` (= `0x8`), faults in the kernel, and panics with `cr2=0x8,
err=0b1010`. Symptom: doom (or any SIGSEGV-ing user process) leaves the
core stuck on every subsequent syscall.

---

## 5. `current_thread()` Returns a Clone

`current_thread()` returns `Option<Thread>` (a **clone**, not a
reference). The `Thread` struct contains `Arc<SpinLock<Process>>`, so
the process is shared. But any **value field** (fs_base, sigmask,
pending, etc.) mutated on the local clone is **lost** unless
`set_current_thread(Some(t))` is called to write it back.

**Rules:**
- After mutating any `Thread` field on a local variable obtained from
  `current_thread()`, always call `set_current_thread(Some(t))` before
  returning from the syscall.
  Established: `tasks.rs:324-333`.
- Never call `current_thread()` while holding `CPUS[cpu_id]` — it
  re-locks the same spinlock and deadlocks.
  Established: `spinlock.rs:29-37` (non-reentrant).

**If you break this:** either the change is silently lost (fs_base not
set, signal mask not updated) or the kernel deadlocks.

---

## 6. Execution Trail — kernel_main to First User Instruction

The boot-to-userspace path must remain deterministic and
uninterruptible until the first `iretq` to user mode.

**Trail:**
1. `kernel_main` → `kernel::init` (GDT, IDT, syscalls, FRAME_ALLOCATOR).
2. `pci::init` → `fs::mount_root` → `smp::boot_aps`.
3. `init_lapic()` → `lapic_timer_start()` (BSP timer).
4. `exec_initrd(path)` → `fs::lookup` → `load_elf` → `load_image`
   (map PT_LOAD segments) → `apply_relocations` (fix up GOT for PIE) →
   `prepare_user_stack` (build auxv) → `register_exec_thread` (create
   Thread with `stack_ptr=0`) → `write_cr3(new_pml4)` →
   `execute_and_resume(entry, stack)`.
5. `execute_and_resume` saves callee-saved regs to `SHELL_CTX`, calls
   `jump_to_user_mode`.
6. `jump_to_user_mode` builds iretq frame (SS, RSP, RFLAGS=0x202, CS,
   RIP=entry), clears GPRs, `iretq` → **first user instruction**.

**Rules:**
- Relocations for PIE binaries must be applied **after** segments are
  mapped but **before** `update_prot_4k` strips WRITABLE from reloc
  target pages. If pages are already read-only, the write faults in
  supervisor mode and panics. Established: `loader.rs:216`.
- `register_exec_thread` creates the initial thread with `stack_ptr=0`.
  The first timer tick sets it to the real value. The thread is never
  enqueued with `stack_ptr=0` — it's installed as `current` directly.
  The guard at `smp.rs:412, 474` is belt-and-suspenders.
  Established: `tasks.rs:431`.

**If you break this:** the user binary runs with an unpopulated GOT
(all function pointers are null), causing an infinite loop or fault
in libc init. This is the symptom: only `arch_prctl` fires, then
silence.

---

## 7. LAPIC Initialization

The LAPIC must be explicitly enabled before the timer is started.

**Rules:**
- `init_lapic()` sets `APIC_BASE_ENABLE` (bit 11) in the MSR and
  `SVR_APIC_ENABLE` (bit 8) in the Spurious Vector Register.
  Established: `apic.rs:62-67`.
- On the BSP, `init_lapic()` is called **before** `lapic_timer_start()`.
  Established: `main.rs:41-45`.
- On APs, `init_lapic()` is **not** called (relies on QEMU
  enabling the APIC at reset). On real hardware, APs must call
  `init_lapic()` before `lapic_timer_start()`.
  Established: `smp.rs:352` (missing — see TODO).
- `calibrate_lapic()` is a **stub** that stores `LAPIC_TICKS_PER_MS =
  1_000_000`. It is **never called**. If wired up with a different
  value, the timer period changes by that factor. The default
  `1_000_000` with divide-by-1 gives ~1ms on QEMU's 1GHz LAPIC.
  Established: `apic.rs:156-163`.

**If you break this:** the timer either never fires (APIC disabled) or
fires at the wrong rate (wrong calibration constant).

---

## 8. Signal Delivery — Only on Return to User Mode

Signals are delivered only when the saved interrupt frame has
`CS == USER_CODE_SELECTOR` (0x23). Kernel-mode frames skip delivery.

**Rules:**
- `can_deliver(sp)` checks `ctx.cs == USER_CODE_SELECTOR`. If the
  frame is a kernel frame, signals are deferred until the next
  user-mode boundary.
  Established: `tasks.rs:618-626`.
- `resume_with_signals` is called on every dispatch path (timer tick,
  yield, run_thread_on_core, exit). It either returns `Some(sp)` (no
  signal or handler installed) or `None` (SIG_DFL termination). The
  caller must handle `None` by calling `exit_current_thread`.
  Established: `tasks.rs:819-827`.
- `Delivery::Die(code)` carries the signal number + 128. Call sites
  must propagate `code`, not hardcode `128 + 2`.
  Established: `tasks.rs:717-719` (bug at `smp.rs:448, 493, 519` —
  hardcodes `128+2` instead of propagating `code`).

**If you break this:** either signals are delivered in kernel mode
(corrupting the kernel stack) or the wrong exit code is reported.

---

## 9. CR3 and Address Space

Each `Process` has its own PML4 (top-level page table). `process.cr3`
holds the physical address. Context switch compares and conditionally
flushes.

**Rules:**
- `write_cr3` is only called when `read_cr3() != target_cr3`. This
  avoids unnecessary TLB flushes when switching between threads of
  the same process.
  Established: `smp.rs:429-431, 500-502` (timer tick),
  `tasks.rs:554-557` (exit).
- `exec_initrd` switches CR3 to `new_pml4` **before** loading segments,
  so `load_image`'s `mapper_for(read_cr3())` operates on the user
  address space.
  Established: `exec.rs:70-72`.
- `KERNEL_CR3` is used when no user thread is running (idle loop).
  Established: `smp.rs:418-420, 460-462, 570-572`.

**If you break this:** either the kernel writes to the wrong address
space (corrupting another process) or performs unnecessary TLB flushes
(degraded performance).

---

## 10. Determinism Principles

These are the meta-rules that the specific invariants above serve.

1. **Every execution path is a trail.** From `kernel_main` to the first
   user instruction, from a syscall entry to its return, from a timer
   interrupt to `iretq` — every step must be traceable. No hidden
   state, no "magic" jumps. If you can't draw the control flow on
   paper, it's wrong.

2. **No hidden preemption.** Interrupts are disabled in syscall
   handlers and spinlock-held regions. The only place a syscall
   voluntarily yields is `block_current_thread` (with `sti; int 0x81`).
   Everywhere else, the syscall runs to completion atomically.

3. **Lock ordering is total.** There is one global order for all
   spinlocks. No lock is ever taken in reverse order. This eliminates
   one entire class of non-deterministic deadlocks.

4. **The timer is the heartbeat.** The LAPIC timer drives preemption.
   It fires in one-shot mode and must be re-armed every tick. If the
   rearm is removed, the kernel freezes. If the timer fires too fast,
   the kernel floods. The calibration constant must match the LAPIC
   bus clock.

5. **Thread state lives on the kernel stack.** `stack_ptr` points to
   the saved `CpuContext` on the thread's kernel stack. There is no
   separate "thread context" struct. This avoids dual-representation
   bugs. The only exception is `fs_base` (stored in the `Thread` struct
   and restored via `wrmsr` on dispatch).

6. **`current_thread()` returns a clone.** Mutations need write-back.
   This is a deliberate trade-off (no lifetime gymnastics in no_std)
   but a persistent footgun. If you forget `set_current_thread`, the
   change is silently lost.

---

## 16. Undocumented-but-Load-Bearing Invariants

The invariants in §1–§15 are stated. These are **not**: they are assumptions
the code depends on, which are enforced only by a constant happening to be
right today. Each was verified against the current tree. A change to any of
them fails silently, or fails as an unrelated-looking fault far from the cause.

---

### 16.1 AP trampoline slot offsets are hand-computed constants

`trampoline.rs` patches three u64 slots in the assembled trampoline blob:

```rust
pub const BLOB_ENTRY_OFF: usize = 0xA8;
pub const BLOB_CR3_OFF:   usize = 0xB0;
pub const BLOB_STACK_OFF: usize = 0xB8;
```

The blob is produced by `trampoline.asm` (192 bytes as of this writing), and
`trampoline.asm` does **not** export its symbol offsets — its own comment says
"via symbols in an object, or just by these bytes". So the Rust constants and
the assembler layout are two independent hand-maintained copies of the same
fact, and **nothing checks that they agree**.

Verified today: the blob is 0xC0 bytes and the three slots occupy exactly
0xA8..0xC0, so the values are correct *now*. If a single byte is inserted in
`trampoline.asm` above the `gdt`/slots, the kernel patches the wrong three
bytes and the APs jump to garbage after INIT-SIPI.

**Rules:**
- Any edit to `trampoline.asm` above the slot block requires recomputing
  `BLOB_ENTRY_OFF`/`BLOB_CR3_OFF`/`BLOB_STACK_OFF` by reassembling and reading
  the offsets, in the same change.
- `gdt_desc` computes its own base as `gdt` with an *absolute* reference
  (`dd gdt`) and relies on the blob being linked with `ORG=0x8000`. If the
  blob is ever linked at a different ORG, the descriptor points at the wrong
  place — this is why `nasm -w+implicit-abs-deprecated` is only a warning here.
- The fix is cheap and available: `crates/kernel/build.rs` already assembles
  the blob with `nasm` and hands the path to the crate via
  `cargo:rustc-env=TRAMPOLINE_BIN`. Have it *also* `include_bytes!` the blob,
  locate the three slots (they are the trailing 24 bytes before `end:`, or
  better, assemble with `-f elf64` and read the `entry_slot`/`cr3_slot`/
  `stack_slot` symbols), and emit the offsets as
  `cargo:rustc-env=TRAMPOLINE_ENTRY_OFF=…`. Then add a build-time assert that
  the offsets lie within the blob length. The two copies then cannot diverge
  because only one exists. **Until that is done, treat the three constants as
  load-bearing and recompute them on any `.asm` edit.**

**If you break this:** APs either triple-fault or jump into random kernel
memory. With `smp: 2` the symptom is a hang during `boot_aps`, not a crash
attributable to the trampoline.

### 16.2 `MAX_CPUS` bounds every per-CPU array, and QEMU must be run under it

`MAX_CPUS = 4` in `smp.rs`, and `CPUS`, `CPU_APIC_IDS`, `IDLE_STACK_TOPS`, and
`CpuStateArray` are all fixed-size arrays of that length. `detect_cpu_count()`
counts MADT entries and stores CPU index only `while count < MAX_CPUS`
(`smp.rs:130`) — the `count += 1` is outside the bound, so discovery does not
stop, it just stops *recording*.

Separately, `APIC_ID_TO_CPU_INDEX` is `[AtomicU8; 256]` indexed by **raw
apic_id**, and `smp.rs:133` writes `APIC_ID_TO_CPU_INDEX[apic_id as usize]`
with no bound check.

**Rules:**
- The test harness runs `-smp 2` (`qemu.rs:20`) against `MAX_CPUS = 4`. Do not
  raise `-smp` above `MAX_CPUS` to "test SMP harder" without raising
  `MAX_CPUS` first. Above 4 CPUs, `CPUS[k]` for the 5th index does not exist,
  and the `CPUS` array is indexed by kernel CPU index throughout.
- Only 8-bit (xAPIC) APIC IDs are supported. A machine presenting an APIC ID
  ≥ 256 would index out of bounds at `smp.rs:133`. There is no runtime guard.
- `MAX_CPUS` is a compile-time constant baked into array types, so raising it
  changes the size of several statics. It is not a runtime knob.

**If you break this:** with more CPUs than `MAX_CPUS`, the extra cores are
discovered but get no `CPUS[]` slot, so `timer_tick_on_core` on them indexes
past the array — memory corruption whose symptom appears on a *different*
core than the cause.

### 16.3 The BADRIP kernel-text bounds are a hand-maintained constant

```rust
const KTEXT_START: u64 = 0xffffffff80000000;
const KTEXT_END: u64 = 0xffffffff80160000;   // tasks.rs:1320
```

Used by **two** call sites (`tasks.rs:1330` and `tasks.rs:1661`) to `panic` on
a kernel-mode RIP outside the kernel's own text.

Measured: the real kernel image spans `0xffffffff80000000 .. 0xffffffff80911af8`
(9286 KiB) across its four `PT_LOAD` segments. `.text` ends near
`0xffffffff80140000`. So `KTEXT_END` covers `.text` with ~128 KiB of slack but
is far short of the image end — which is correct for its purpose (RIP should
never be in data), and the slack is what the comment means by "end has slack
for growth".

**Rules:**
- `KTEXT_END` must be re-derived when `.text` grows past it. The comment says
  so, but nothing enforces it. A RIP in kernel *data* would start tripping the
  panic.
- The window is deliberately the kernel's own `.text` only. A loadable-module
  window (see `docs/module-system.md` §12.1) must be added as a **separate**
  accepted range, not by widening this one — widening it would mask a genuine
  wild pointer into kernel data.
- User frames (`cs == 0x23`) are exempt; only `cs == 8` is checked.

**If you break this:** a stale `KTEXT_END` turns every scheduler dispatch into a
`panic!("BADRIP…")` under load; a too-wide one silently accepts corruption.

### 16.4 Slab size classes: allocations over 2048 bytes take a different path

`BLOCK_SIZES = [16, 32, 64, 128, 256, 512, 1024, 2048]`
(`memory/slab.rs:6`). `list_index` returns `None` for anything larger, and
those go to `fallback_alloc` → `LinkedListAllocator::allocate_first_fit`
bump-style allocation.

**Rules:**
- Adding a size class changes the meaning of the array indices in the free
  lists, so it is not a purely additive change. `list_heads` is indexed by
  position in `BLOCK_SIZES`.
- The `assert!`s at `slab.rs:127-128` require `size_of::<ListNode>()` and its
  alignment to be ≤ the *smallest* class. They are debug asserts, so this is
  not checked in release.
- `layout.size().max(layout.align())` is what selects the class, not
  `layout.size()` alone. An over-aligned allocation can land in a larger class
  than its size implies.
- A separate tracer fires for `size() >= 1 MiB` (`slab.rs:63`) — that is a
  diagnostic threshold, **not** an allocation limit. There is no large-allocation
  cap; the fallback just consumes the heap until it fails.

**If you break this:** an over-aligned `alloc` silently returns a block from
the wrong class, and the mismatch surfaces later as corruption in whatever
assumed the requested alignment.

### 16.5 HHDM and the `phys_offset` convention: physical memory is mapped twice

`HHDM_BASE = 0xffff800000000000` maps **all** physical memory, so any physical
address is valid as `HHDM_BASE + phys`. `Mapper::new_with_offset` carries this
`phys_offset` (`shared/src/memory/mapper.rs:22`) and every page-table walk
re-adds it (`table_from_phys`).

**Rules:**
- A `PhysAddr` is **never** dereferenced directly. It must be offset by
  `HHDM_BASE` first. `space.rs:236` reads
  `let phys_offset = HHDM_BASE.0;` for exactly this.
- The high half of a PML4 is the kernel's, and that split is load-bearing:
  `create_address_space` copies entries **256..512** from the current CR3 into
  the new table, so a fresh address space inherits the kernel's mappings and
  therefore stays kernel-accessible (`space.rs:96`). `fork_address_space`
  additionally copies entries 0..256 for the user half. Any code building a
  new address space must preserve the 256 boundary — a table that copies the
  user half but not the kernel half faults on every kernel address, and one
  that copies all 512 shares user pages it should not.
- This is also why a module or heap address must be checked against HHDM's
  covered range before being treated as unambiguous — see
  `docs/module-system.md` §4.

**If you break this:** dereferencing a raw `PhysAddr` faults in supervisor
mode at an address that looks like a wild kernel pointer.

### 16.6 The kernel heap is outside the `code-model=kernel` sign-extendable range

`HEAP_START = 0xFFFF_9000_0000_0000` (`memory/heap.rs:10`), 256 MiB. This is
**below** `0xffffffff80000000`, the lowest address representable as a
sign-extended 32-bit value, which is what the x86-64 `kernel` code model
assumes.

**Rules:**
- The `code-model=kernel` assumption covers kernel text and kernel data, not
  the heap. The kernel already relies on this: `HEAP_START as *mut u8` is a
  64-bit pointer.
- A 32-bit-representable pointer may **not** be used for heap memory. This is
  the constraint that fixes the loadable-module window to
  `0xffffffffa0000000` and not a `0xFFFF_B…` address.
- `R_X86_64_32S` relocations are only valid for addresses that sign-extend
  back to themselves. Any new subsystem that introduces a 32-bit relocation
  must check this.

**If you break this:** an address truncates silently, and the fault appears at
a plausible-looking low address.

### 16.7 There are two GDTs — a global singleton and a per-CPU copy

`GDT` is a global `Lazy<Gdt>` built from the global `TSS` (`gdt.rs:100`), and
`CpuLocal` *also* carries its own `gdt: Gdt` field (`smp.rs:77`), built per
core by `build_per_cpu_gdt` and installed with `load_per_cpu` (`smp.rs:241,246`).
Both exist and both are loaded with `lgdt`.

**Rules:**
- Any `lgdt` on a core must be paired with `reload_cs_after_lgdt()` on that
  core (`smp.rs:347` on the AP path). The first `iretq` after an `lgdt` would
  fault reloading `CS=0x18` from the *previous* descriptor table, because the
  far-return reload re-reads the GDT. Skipping this is the single easiest way
  to break AP bring-up.
- The per-CPU `Gdt` is an **in-memory struct referenced by descriptor**, not a
  pointer to a static. It must live in kernel memory that outlives its use —
  hence the `gdt must outlive use` requirement documented at `gdt.rs:148`. It
  lives inside `CPUS[k]`, which is a kernel static, so this holds today.
- The TSS is likewise per-CPU. Each `CpuLocal` embeds its own `tss`, and
  `build_per_cpu_gdt(tss_static)` bakes a descriptor to *that* field's address;
  the address is stable because `CPUS` is a static that never moves
  (`smp.rs:236-241`, with the safety argument in the comment). `ltr` then
  points the TR at that core's own TSS, and `rsp0` is set per core. Using the
  global `TSS` on a second core would give that core a descriptor naming the
  *first* core's TSS, so a ring-3 entry would land on the wrong stack.
- The BSP path is the exception that proves the rule: `lib.rs:110` notes
  `init_bsp()` re-`lgdt`/`ltr`s this core's own GDT, so the global `GDT` is
  effectively a bootstrap artifact. Do not add a third path that loads one and
  forgets the other.

**If you break this:** the core faults on its first `iretq` after the reload,
which reads as a mysterious userspace-entry crash rather than a GDT problem —
and if `rsp0` is wrong, it reads as stack corruption on a *different* core.

### 16.8 Syscall stack is shared per-CPU and reused across syscalls

`SYSCALL_STACK` is a single static (`syscalls/mod.rs:268`, 16 KiB). The comment
at `syscalls/mod.rs:187` records the invariant: the CPU's
`CpuState.kernel_stack` is *overwritten* with the calling thread's
`syscall_stack_top`, and **two CPUs never run syscalls on this stack**.

**Rules:**
- A syscall handler must not assume it can grow the stack. The 16 KiB budget
  is the real limit; a deep call chain in a handler overflows into whatever is
  below and corrupts the next syscall.
- Because the stack is per-CPU and reused, a handler must not leave live state
  on it across a voluntary yield. `block_current_thread` re-enters via
  `sti; int 0x81` and the resume path must reconstruct everything it needs.
- Handlers that never return are typed `-> !` (`syscalls/mod.rs:307`) so the
  dispatcher does not emit a return. This is a type-level invariant, not a
  convention.

**If you break this:** the corruption appears in an unrelated later syscall on
the same core, attributed to the wrong handler.

### 16.9 IDT is global, not per-CPU

`IDT` is a single `Lazy<Idt>` loaded once (`interrupts.rs:101`). The GDT is
per-CPU (§16.7) but the IDT is not.

**Rules:**
- The IDT descriptor points at a kernel static, so it is valid on every core
  without a reload. A **new** per-CPU IDT would need an `lidt` on each AP.
- Interrupt gate entries (not trap gates) are required for handler 0x81/yield
  and the timer: the `crt` vs `tf` bit changes whether IF survives entry
  (`idt.rs:32`, invariant 2).

**If you break this:** a core that has not loaded the IDT takes a triple fault
on its first interrupt.

### 16.10 COW table is sized from the highest physical frame, and is bounds-checked

`init_cow_table(max_frame)` allocates `vec![0; max_frame]` where `max_frame` is
the **highest** end of any memory-map region divided by 4096
(`lib.rs:71-77`).

**Rules:**
- The table is indexed by **frame number**, and every index is bounds-checked
  via `cow_oob(frame, table.len())` (`space.rs:64, 75`). A frame beyond the
  table returns 0 rather than indexing out of bounds. Keep it that way: a new
  COW access that skips `cow_oob` is an OOB write on a shared `SpinLock<Vec>`.
- `max_frame` comes from the bootloader memory map. If the map is ever clamped
  (e.g. a hotplug range or a reserved-hole heuristic), the table shrinks with
  it and COW accounting silently stops working for high frames.
- Counts are `u32` and use `checked_add(...).expect("COW count overflow")` —
  an overflow panics rather than wrapping. A frame shared more than 2^32 times
  is a bug, and this makes it loud.

**If you break this:** a missing bounds check writes past the `Vec`, and since
it holds a `SpinLock`, the corruption is visible to every core.

---

## Known Bugs (as of this writing)

These bugs violate invariants above. They are marked `// ponytail:` or
are in the process of being fixed:

1. ~~**`smp.rs:484` — deadlock in same-thread path.** Calls
   `current_thread()` while `cpu_slot` is held. Violates invariant 5.~~
   **FIXED:** reads `cpu_slot.as_mut().unwrap().current.as_mut()`
   directly; also simplified by dropping the SIG_DFL termination branch
   (no `exit_current_thread` call in same-thread path — signals deferred
   to next user-mode boundary).

2. ~~**`smp.rs:448, 493, 519` — hardcoded `128+2` exit code.** Violates
   invariant 8.~~ **FIXED:** propagates `Delivery::Die(code)` to `exit_current_thread(code)`.

3. ~~**`apic.rs:65` — SVR spurious vector not set.** Relies on
   bootloader/QEMU default.~~ **FIXED:** `lapic_write(LAPIC_SVR, 0xFF | SVR_APIC_ENABLE)`
   explicitly executed on both BSP and APs.

4. ~~**`loader.rs:216` — relocations applied after `update_prot_4k`.**
   Violates invariant 6.~~ **FIXED:** relocations are applied before
   `update_prot_4k` loops over `prot_updates`.

5. ~~**`apic.rs:162` — `calibrate_lapic` stub stores 1000 (wrong).**~~
   **FIXED:** now stores `1_000_000` (the correct QEMU value).

6. ~~**`tasks.rs:595` — `FPU_SCRATCH` is global, not per-CPU.** Racy
   under SMP signal delivery.~~ **FIXED:** each thread has per-thread `fpu`
   and `uc_fpu: Arc<FpuBuf>` buffers (`fxsave`/`fxrstor`); `FPU_SCRATCH` global eliminated.

7. ~~**`interrupts.rs:104` — page fault handler missing swapgs on user
   entry/exit.** Violates invariant 4b.~~ **FIXED:** entry/exit swapgs balanced.

8. ~~**`smp.rs` Keyboard park — SMP lost-wakeup on stdin.**~~ **FIXED:** lock-free atomic `STDIN_HAS_DATA` signal and asymmetric try_lock park logic.

9. ~~**No FPU save/restore on context switch.** FPU state leaks between
   threads.~~ **FIXED:** eager `current.fpu_save()` on park/preempt in `smp.rs`
   and `next.fpu_restore()` before iretq.

10. ~~**`epoll.rs` — `sys_epoll_wait` parks on `PipeWait` but input IRQs
   only wake `INPUT_WAIT_QUEUE`.** epoll-based input consumers (mywima/smithay)
   never wake from keyboard or mouse events.~~ **FIXED:** all input producers
   (`keyboard.rs`, `ps2_mouse.rs`, `usb/xhci.rs`) now call
   `wake_pipe_waiters()` after `wake_input_waiters()` so both wait queues
   are drained. Lock ordering is preserved: each `wake_*` call acquires
   `SCHEDULER → wait-queue` independently (no nesting).
