# SMP / Multi-Core Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Repo note:** this is not a git repo, so there are no `git commit` steps. Each task ends with the build + QEMU verification run (the same command is the "checkpoint").

**Goal:** Boot application processors (APs) via INIT-SIPI, give each CPU its own TSS/GDT/IDT, per-CPU data, and IPI delivery, then split the scheduler into per-CPU runqueues so threads genuinely run in parallel. Preserve single-core correctness as a fallback (`-smp 1` must behave like today).

**Architecture:** The APIC plan (`2026-08-08-apic.md`) is the prerequisite — IPIs and the LAPIC are the SMP substrate. Core concept: **per-CPU areas** (`PerCpu { cpu_id, current_thread, scheduler queue, ... }`) reachable through `GS`-base relative addressing, so all the global `static` scheduler/task state becomes CPU-local. APs boot into a trampoline that sets up their own GDT/TSS/IDT, enables paging, and enters a `ap_entry` function.

**Tech Stack:** Rust `#![no_std]` kernel, x86_64, LAPIC (SIPI/ICR), GS-based per-CPU access (`swapgs`/`wrmsr(IA32_KERNEL_GS_BASE)`), GDT/TSS per CPU, QEMU `-smp N -enable-kvm`, `-serial file:` verification.

---

## Current state (what this changes)

- **Single CPU**: `KERNEL_CR3` (`kernel/src/arch/x86_64/cpu.rs:13`), global `SCHEDULER`/`CURRENT_THREAD`/wait queues (`kernel/src/tasks.rs:127-131`), one global `IDT`/`TSS`/`GDT` (`kernel/src/arch/x86_64/{gdt,idt,tss}.rs`).
- **All spinlocks** use `interrupts_enabled()`/`disable`/`enable` (`kernel/src/arch/x86_64`/`interrupts.rs:25-50`) — disabling IF on one CPU does **not** block another CPU, so every lock that currently relies on IF to prevent deadlock will deadlock or corrupt under SMP.
- **Timer**: single PIT tick drives `timer_rust_handler` → scheduling. Under SMP, each CPU needs its own LAPIC timer IPI to self.
- **No IPI code** (APIC plan will add `lapic_send_ipi`).

## Design overview

```
Per-CPU layout (one per CPU, allocated in kernel memory; KERNEL_GS_BASE = &PerCpu):
  struct PerCpu {
      cpu_id: u32,
      is_bsp: bool,
      lapic_id: u32,
      current_thread: SpinLock<Option<Thread>>,  // was global CURRENT_THREAD
      scheduler: SpinLock<VecDeque<Thread>>,      // was global SCHEDULER
      // wait queues stay global (shared by all CPUs)
      tss: &'static TaskStateSegment,
      gdt: Gdt,
      idt: Idt,
      cr3: PhysAddr,
  }
```

**Important existing constraint:** the kernel already uses `swapgs` on syscall entry/exit with `IA32_KERNEL_GS_BASE` (`0xC0000102`) pointing at the global `CPU_STATE` (kernel/src/arch/x86_64/syscalls.rs:121). Under SMP that single global is shared by all CPUs and *must* become per-CPU. The clean way: put `CpuState` **inside** `PerCpu` and point `IA32_KERNEL_GS_BASE` at `&PerCpu`. After a `swapgs` in kernel mode, `gs:` reads now reach `this_cpu().cpu_state`. Do **not** introduce `IA32_GS_BASE` (`0xC0000101`) — it is the user-visible GS base and is already used by user TLS (`IA32_FS_BASE` for user code; GS is the swapgs scratch slot). The per-CPU access pattern stays exactly the existing one (`swapgs` in the syscall/timer entry asm), just pointing at a larger struct.

Every access to `CURRENT_THREAD`/`SCHEDULER` goes through `this_cpu().current_thread`. The `PENDING_BLOCK` handoff protocol (tasks.rs:145-167) becomes per-CPU — a thread blocks on *its* CPU's queue, the *local* LAPIC timer fires the switch.

## Lock discipline under SMP (the hard part)

The current `SpinLock` does `cli`-based critical sections. Under SMP:
- **Never** rely on IF to guarantee mutual exclusion (it only masks local interrupts).
- Keep the spinlock itself (it already spins on `xchg`), but the *wait* must not hold IF-cli across another CPU's critical section. The current design holds the lock AND cli — that's fine for short critical sections on one CPU, but under SMP the right pattern is: lock without cli for pure memory sections; cli+lock only when the section must also be atomic w.r.t. local interrupts.
- The timer handler path (time.rs) must switch to **per-CPU** spinlocks: it takes `SCHEDULER` and `CURRENT_THREAD` — both become `this_cpu()` fields, so a CPU locks its own.

**This plan makes spinlock correctness explicit but does not re-architect it** — each task that touches a previously-global lock explains whether it stays global (shared wait queues) or becomes per-CPU (scheduler), and verifies by running `-smp 2` workloads.

---

### Task 1: Per-CPU data structures

**Files:**
- Create: `kernel/src/arch/x86_64/percpu.rs`
- Modify: `kernel/src/arch/x86_64/mod.rs`

- [ ] **Step 1: Create `kernel/src/arch/x86_64/percpu.rs`**

```rust
/// Per-CPU state. Reached via the existing swapgs/KERNEL_GS_BASE pattern.
#[repr(C)]
pub struct PerCpu {
    pub cpu_id: u32,
    pub lapic_id: u32,
    pub is_bsp: bool,
    pub current_thread: SpinLock<Option<Thread>>,
    pub scheduler: SpinLock<VecDeque<Thread>>,
    pub cpu_state: crate::arch::x86_64::syscalls::CpuState, // was the global CPU_STATE
    pub cr3: shared::memory::address::PhysAddr,
}

static mut PERCPU_AREAS: alloc::vec::Vec<PerCpu> = alloc::vec::Vec::new();

pub fn init_percpu_areas(cpu_count: usize) {
    unsafe {
        PERCPU_AREAS = (0..cpu_count).map(|_| PerCpu {
            cpu_id: 0, lapic_id: 0, is_bsp: false,
            current_thread: SpinLock::new(None),
            scheduler: SpinLock::new(VecDeque::new()),
            cpu_state: Default::default(),
            cr3: shared::memory::address::PhysAddr(0),
        }).collect();
    }
}

pub fn cpu_count() -> usize {
    unsafe { PERCPU_AREAS.len() }
}

/// Returns this CPU's PerCpu. Only valid in kernel mode after swapgs
/// has switched KERNEL_GS_BASE to the per-CPU pointer.
pub fn this_cpu() -> &'static PerCpu {
    unsafe {
        let gs: u64;
        core::arch::asm!("mov {}, gs:0", out(reg) gs);
        &*(gs as *const PerCpu)
    }
}

/// Called once per CPU: points KERNEL_GS_BASE at this CPU's PerCpu.
/// Note: the syscall/timer entry asm does `swapgs` on entry; the *exit*
/// path swaps back. The base written here is the kernel-mode GS base.
pub unsafe fn set_kernel_gs_base(ptr: *const PerCpu) {
    crate::arch::x86_64::msr::wrmsr(IA32_KERNEL_GS_BASE, ptr as u64);
}
```

(`gs:0` reads the first qword *at* the GS base — so the first field of `PerCpu` must be at offset 0. `repr(C)` guarantees field order. Verify the offset trick on the BSP first.)

- [ ] **Step 2: Register module**

Add `pub mod percpu;` to `kernel/src/arch/x86_64/mod.rs`.

- [ ] **Step 3: BSP sets its GS base in `init()`**

In `kernel/src/lib.rs::init()`, after `init_percpu_areas(1)` (single CPU for now):
```rust
unsafe {
    let pc = &mut crate::arch::x86_64::percpu::PERCPU_AREAS[0];
    pc.cpu_id = 0; pc.is_bsp = true;
    pc.lapic_id = crate::arch::x86_64::apic::lapic_id();
    crate::arch::x86_64::percpu::set_kernel_gs_base(pc);
}
```
Note: `set_kernel_gs_base` writes `IA32_KERNEL_GS_BASE`; because `swapgs` is used on syscall/timer entry, code that reads `this_cpu()` *inside* a syscall/timer handler (kernel mode after swapgs) is correct, and the exit path's `swapgs` restores user GS. Add a smoke print of `this_cpu().cpu_id`.

- [ ] **Step 4: Verify**

```bash
./scripts/build-kernel.sh && ./scripts/smoke.sh
```
Expected: `SMOKE PASS`, serial shows `cpu_id: 0`.

---

### Task 2: AP boot trampoline (INIT-SIPI)

**Files:**
- Create: `kernel/src/arch/x86_64/smp.rs`
- Modify: `kernel/src/arch/x86_64/mod.rs`
- Modify: `kernel/src/arch/x86_64/percpu.rs` (add `ap_entry`)

- [ ] **Step 1: Count CPUs from the ACPI MADT**

Parse the ACPI tables located via `BootInfo.rdsp` (already passed by the bootloader). Walk RSDP → RSDT/XSDT → MADT (`Signature "APIC"`); count `Local APIC` entries (type 0). Fallback: if `rdsp` is `None`, use `cpu_count = 1`.

**Files:** Create `kernel/src/arch/x86_64/acpi.rs` with:
```rust
pub fn count_cpus(rdsp: Option<u64>) -> usize
pub fn lapic_base_from_madt(rdsp: Option<u64>) -> Option<u64> // if it differs from 0xFEE00000
```

- [ ] **Step 2: Allocate the trampoline**

INIT-SIPI wakes an AP at its real-mode entry (a page below 1 MiB with the startup code). Allocate a page below 1 MiB (use the frame allocator to get physical memory below `0xA0000` if available; otherwise set up the trampoline in a low page the bootloader left Usable). The trampoline is a tiny 16-bit → 32-bit → 64-bit stub that:
1. Sets up a minimal GDT (flat segments) and enables paging with the BSP's `KERNEL_CR3`.
2. Reads its own APIC ID from the LAPIC (`0xFEE0_0020`).
3. Jumps to a `u64` entry-point passed via a fixed memory slot (e.g. the first bytes of the trampoline page hold the 64-bit entry address).

- [ ] **Step 3: Send INIT-SIPI from the BSP**

```rust
// INI: ICR = 0x000C4500 (INIT, assert, all-except-self or targeted APIC ID)
// SIPI: ICR = 0x000C4600 | (trampoline_page >> 12)  (SIPI, startup vector)
pub fn lapic_send_ipi(target_lapic_id: u32, vector: u8, level: bool) {
    // ICR is at LAPIC offset 0x300 (write) / 0x310 (read), 64-bit via two 32-bit writes:
    // low: delivery mode + dest shorthand + vector; high: target APIC ID.
}
```
Add this to `kernel/src/arch/x86_64/apic.rs` (the APIC plan owns `apic.rs`; this task adds the ICR write function).

Sequence per target: write ICR low = `0x4500` (INIT), wait 10ms, write ICR low = `0x4600 | (page >> 12)` (SIPI), wait 200µs, repeat SIPI once. Poll until the AP reports in via a `static AtomicUsize AP_READY` increment.

- [ ] **Step 4: `ap_entry`**

```rust
#[unsafe(no_mangle)]
pub extern "C" fn ap_entry() -> ! {
    let id = crate::arch::x86_64::apic::lapic_id();
    let cpu = crate::arch::x86_64::percpu::per_cpu_by_lapic_id(id);
    unsafe {
        crate::arch::x86_64::percpu::set_kernel_gs_base(cpu);
    }
    cpu.cpu_id = id; // from MADT order or map lapic_id -> index
    crate::arch::x86_64::gdt::init();   // per-CPU GDT/TSS
    crate::arch::x86_64::tss::init();   // per-CPU TSS
    crate::arch::x86_64::idt::load();   // shared IDT is fine, but per-CPU IST stack
    crate::arch::x86_64::apic::init_lapic();
    crate::arch::x86_64::apic::lapic_timer_start(TIMER_INTERRUPT_ID, 1);
    crate::arch::x86_64::cpu::sti();
    crate::tasks::ap_loop(); // idle loop: run per-CPU queue, hlt when empty
}
```

- [ ] **Step 5: Verify 2 CPUs boot**

Run QEMU with `-smp 2`. Serial must show `cpu_id: 0` and `cpu_id: 1` (from `ap_entry` printing once).

```bash
./scripts/build-kernel.sh && ./scripts/smoke.sh   # still -smp 1 in qemu.sh? add -smp 2 to the smoke QEMU line for this check
```
Expected: both CPU ids appear, no panic, shell prompt still works.

---

### Task 3: Per-CPU TSS/GDT/IDT

The shared GDT has one TSS. Each CPU needs its own TSS (its own kernel stack for ring transitions) and, critically, its own `RSP0` and IST. GDT selectors can be shared if the TSS base points per-CPU; the task is to give each `PerCpu` its own `TaskStateSegment` and load it.

**Files:**
- Modify: `kernel/src/arch/x86_64/gdt.rs` (parameterize by CPU)
- Modify: `kernel/src/arch/x86_64/tss.rs` (per-CPU instance + `set_rsp0` for this_cpu)
- Modify: `kernel/src/arch/x86_64/percpu.rs`

- [ ] **Step 1: Per-CPU TSS + GDT**

Give `PerCpu` fields `tss: &'static TaskStateSegment` and `gdt: Gdt`. `gdt::init(cpu: &mut PerCpu)` builds a GDT whose TSS descriptor points at `&cpu.tss`. Load `lgdt` into that CPU's GDTR. `set_rsp0(stack_top)` writes `this_cpu().tss.rsp0`.

- [ ] **Step 2: Per-CPU IST**

The double-fault handler needs a per-CPU IST stack. Allocate `DOUBLE_FAULT_STACK` (16 KiB) per CPU in `ap_entry`/BSP init, point `cpu.tss.ist[DOUBLE_FAULT_IST_INDEX]` at it.

- [ ] **Step 3: Verify**

`-smp 2`, both CPUs running timer ticks. Add a temporary counter in `timer_rust_handler` keyed by `this_cpu().cpu_id`; after ~3s each counter should be ≈3000. Remove the counters.

```bash
./scripts/build-kernel.sh && ./scripts/smoke.sh
```

---

### Task 4: Per-CPU scheduler runqueues

**Files:**
- Modify: `kernel/src/tasks.rs` (replace global `SCHEDULER`/`CURRENT_THREAD` with per-CPU accessors)
- Modify: `kernel/src/time.rs` (timer handler uses `this_cpu()`)
- Modify: `kernel/src/interrupts.rs` (keyboard wake path uses global wait queues)

- [ ] **Step 1: Move `SCHEDULER`/`CURRENT_THREAD` into `PerCpu`**

Delete the globals `SCHEDULER` (tasks.rs:127) and `CURRENT_THREAD` (tasks.rs:128). Add accessor functions:
```rust
pub fn this_cpu_scheduler() -> &'static SpinLock<VecDeque<Thread>> { &crate::arch::x86_64::percpu::this_cpu().scheduler }
pub fn this_cpu_current() -> &'static SpinLock<Option<Thread>> { &crate::arch::x86_64::percpu::this_cpu().current_thread }
```
Update every call site in `tasks.rs`, `time.rs`, `syscalls.rs`, `interrupts.rs` that referenced `SCHEDULER`/`CURRENT_THREAD` directly (grep them all). Wait queues (`CHILD_WAIT_QUEUE`, `KEYBOARD_WAIT_QUEUE`, `PIPE_WAIT_QUEUE`) stay global — they're cross-CPU rendezvous.

- [ ] **Step 2: Timer handler locks only the local runqueue**

`timer_rust_handler` (time.rs) switches to `this_cpu_scheduler()`/`this_cpu_current()`. The `PENDING_BLOCK` protocol (tasks.rs:145) becomes `this_cpu().pending_block` — a thread blocks on its own CPU's queue. Add `pending_block: SpinLock<Option<PendingBlock>>` to `PerCpu`.

- [ ] **Step 3: Load balancing (minimal)**

Do NOT implement work stealing yet. For round-robin fairness with multiple threads and multiple CPUs, push an idle CPU to pull from other runqueues only when its own is empty:
```rust
pub fn try_steal(cpu: &PerCpu) -> Option<Thread> {
    for other in all_cpus() {
        if other.cpu_id != cpu.cpu_id {
            if let Some(t) = other.scheduler.lock().pop_front() {
                return Some(t);
            }
        }
    }
    None
}
```
Called from `ap_loop`/idle path. Keep it simple (`ponytail:` — O(N_cpus) scan on idle only).

- [ ] **Step 4: Verify parallel execution**

Write a temporary test: `fork()` twice, each child does a busy loop of `TICKS` sampling, then print each child's observed tick count. Under `-smp 2`, run three times and confirm ticks are distributed across >1 tick rate and no panic. Remove the test.

```bash
./scripts/build-kernel.sh && ./scripts/smoke.sh
```
Expected: `SMOKE PASS` with `-smp 2`, shell responsive, no deadlock.

---

### Task 5: IPI + spinlock sanity under SMP

**Files:**
- Modify: `kernel/src/arch/x86_64/apic.rs` (finish IPI path)
- Modify: `kernel/src/arch/sync/spinlock.rs` (document/verify IRQ-save semantics)

- [ ] **Step 1: IPI for TLB shootdown**

Add `flush_tlb_range_other_cpus()` that sends an IPI to all other CPUs (ICR broadcast `0x40000` delivery mode, fixed, vector e.g. 0x30) and each CPU's handler runs `invlpg` for a range recorded in a global (small, one at a time) `pending_tlb_range`. Needed before `fork`/`munmap` can be safe across CPUs. Hook it into `destroy_address_space` and `unmap_range` only when `cpu_count() > 1`.

- [ ] **Step 2: Verify `fork` under SMP**

Run the fork-tick test from Task 4 Step 4, plus the existing test suite, under `-smp 2`. Confirm no page-fault storms from stale TLB entries.

```bash
./scripts/build-kernel.sh && ./scripts/smoke.sh
```

- [ ] **Step 3: Full regression**

```bash
./scripts/build-kernel.sh && cargo test -p kernel --lib --target x86_64-unknown-none && ./scripts/smoke.sh
```
Run smoke with `-smp 1` and `-smp 2`. Both green. Document in `scripts/qemu.sh` the `-smp` knob.

---

## Deferred (explicitly out of scope)
- Work stealing / CFS per-CPU load balancing (scheduling plan owns the algorithm; this plan only splits the queues).
- `spin_lock_irqsave` full API — KPI plan (`2026-08-08-linux-kpi-complexity.md`) formalizes it.
- CPU hotplug, offline/online.
- NUMA topology awareness.
- Preemption between cores for the *same* runqueue (single-owner invariant instead).
