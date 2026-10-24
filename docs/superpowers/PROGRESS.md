# Progress Report — Test Harness + Shared Unit Tests

Updated: 2026-08-20

## Status: Test Harness WORKING for `shared` (42 tests) — Kernel QEMU harness next

Prior design docs: `docs/superpowers/specs/2026-08-12-smp-scheduler-design.md`,
`docs/superpowers/plans/2026-08-12-smp-mmap.md` (both complete).

## Handoff docs resolved (removed)

- `handoff.md` — 4 bugs all FIXED and verified in tree:
  1. `sys_mmap` now calls `map_demand_range` (syscalls.rs:662).
  2. `resume_with_signals` sets `ctx.rdi = sig` (tasks.rs:873).
  3. Red-zone: `ctx.rsp.saturating_sub(128)` (tasks.rs:808).
  4. `sys_execve` sets `t.stack_ptr = frame` (syscalls.rs:1564).
- `task_handoff.md` — DSO cache Task 9 COMPLETE: `loader.rs` rewritten with
  `interp_inode` (loader.rs:55,346), `VmaKind::MappedFile` + faults
  (vma.rs:23, filemmap.rs:186), `dso_test` in initrd, `DSO_HITS` counter in
  dsocache.rs, TEMP debug prints removed.
- `HANDOFF.md` — SMP AP bring-up COMPLETE: APs boot to shell, per-CPU
  GDT/TSS/IDT/GS wired, keyboard lost-wakeup fixed. Residual gaps (AP timer
  check, `signal_runnable` never called, per-CPU IST) remain open; see
  **Open work** below.

## Done (this session, verified)

- [x] **Host test harness for `shared` crate.** `shared/src/lib.rs` now uses
  `#![cfg_attr(not(test), no_std)]` so `cargo test -p shared` compiles and runs
  on the host (uses std + alloc). Verified: `cargo test -p shared` → **42 passed,
  0 failed**.
- [x] **Unit tests for every shared module**:
  - `address.rs` (9): phys align/add/sub, virt indices via roundtrip builder,
    canonical incl. `0xFFFF_8000_0000_0000`, align_up/down, PAGE_MASK.
  - `flags.rs` (5): bit identities incl. MMIO=1<<52, NO_EXECUTE=1<<63,
    contains/bitops, insert/remove, Not, prot_to_flags.
  - `page.rs` (7): layout, containing/roundtrip, address masking, custom bits,
    `clear()`, zeroed table.
  - `error.rs` (2): Display messages, Copy/Eq.
  - `layout.rs` (1): constants. `allocator.rs` (3): pages_for, trait usable.
  - `address_space.rs` (1): wraps PhysPage.
  - `mapper.rs` (9): empty pml4, translate, AlreadyMapped, demand flag strips
    PRESENT, unmap clears, unmap-unmapped ok, update_prot, huge-page split,
    hugetables.
  - `boot/mod.rs` (3): page_count, MMIO_DEVICE_REGIONS. `boot_info.rs` (4):
    layout sizes, HHDM canonical, default BootInfo.
- [x] **Bug found by the new `mapper.rs` unmap test:** `Mapper::unmap_4k`
  (shared/src/memory/mapper.rs:312) called `pte.set_flags(PageTableFlags::NONE)`
  which **ORs only** (no-op), so PRESENT survived and `translate` returned
  `Some(0)`. Fixed by adding `PageTableEntry::clear()` (page.rs) and calling it
  from `mapper.rs` AND `kernel/src/memory/space.rs:716`.
  `set_flags` remains OR-only by design.
- [x] Clean compile baseline restored: removed `[SIGDBG]`/`[SIGRET]` debug
  prints from `deliver_pending` + `sys_rt_sigreturn`; re-applied red-zone +
  `t.stack_ptr` handoff fixes. `bash scripts/build-kernel.sh` → clean.
- [x] Deleted dead code `shared/src/size.rs` (unused, referenced nowhere).

## Open work

- [ ] **Kernel QEMU selftest harness (blog_os pattern).** `lib.rs` already has
  `#![feature(custom_test_frameworks)]` + `test_runner` + `test_main` reexport;
  `kernel/src/test_runner.rs` + `scripts/test-kernel.sh` (isa-debug-exit,
  exit 33/35) exist. Missing: a `#[cfg(test)]` entry that calls `test_main()`
  (kernel.ld ENTRY is `kernel_main`; main.rs stubs it out under `cfg(not(test))`).
  Host `--lib` and bare-metal `cargo test` are blocked (E0152 panic_impl dup /
  no libtest on target) — must route through the QEMU runner.
- [ ] **Kernel pure-logic unit tests** once the QEMU harness links
  (`sched` weights/runqueue, `fs/dirent` pack, `fs/tar` walk, `graphics/color`,
  `arch/x86_64/abi` constants, `memory/vma` list ops).
- [ ] **Refactor `syscalls.rs` (3110 lines)** into submodules — pure code
  movement, no logic change, dispatch table + entry stays.
- [ ] **Rebuild + boot-verify**: `scripts/build-kernel.sh`, `scripts/smoke.sh`,
  `python3 /tmp/opencode/lstest.py`.
- [ ] **SIGCHLD `0x100000000` corruption**: pid=1 shell crashes at
  `rip=0x100000000` after child exit on 3x `ls` (lstest.py detects).

## SMP residuals (from removed HANDOFF.md, still open)

- AP LAPIC timer firing not re-verified after updates.
- `signal_runnable()` defined (smp.rs:197) but never called → no thread has
  ever been dispatched to an AP.
- Per-CPU IST / double-fault stack (APs share BSP's IST).
- Global spinlocks (`SCHEDULER`, `FRAME_ALLOCATOR`, `WRITER`) serialize; defer.