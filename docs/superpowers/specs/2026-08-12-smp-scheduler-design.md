# Dual-Core SMP + Shared-Queue Scheduler (Redox-Hybrid)

Date: 2026-08-12
Status: Draft

## Baseline (already landed, not re-litigated)

- Earlier this session: the sporadic `KERNEL PANIC: INV1[enter]` false-positive
  crash was fixed. `tasks.rs` now validates thread pointers via `kernel_ptr_ok()`
  (canonical high-half kernel address) instead of the fragile 16MB heap-window
  check `in_heap()`. Rationale: the initial thread is legitimately interrupted
  on the boot stack (`0xffff8001…`) before switching to its heap ring-0 stack.
  Verified: `songplayer /clip20.wav` runs clean under the same PA QEMU config
  that previously crashed.
- `ls` was broken: the root filesystem is ext2 (`mount_root`), but
  `sys_getdents64` and `sys_stat_path` still read the old TAR initrd, so `ls`
  printed nothing (getdents → `ENOTDIR`) and errored on every entry (stat →
  `ENOENT`). Fixed by adding `list_dir()`/`dir_entries()` to `FileNode`
  (`vfs.rs`/`ext2/mod.rs`), handling `VNode::FsNode` in `sys_getdents64`, and
  switching `sys_stat_path` to `fs::lookup`. Verified: `/bin/ls` lists all
  entries and exits 0.
- TAR initrd is now a **fallback only**: `mount_root()` returns `bool`, and
  `set_initrd` runs only when ext2 fails to mount (moved from `init()` to
  `main.rs`). When ext2 mounts, the TAR is never loaded.

## Goal

Refactor the single-CPU scheduler into a per-CPU-shaped design (Redox-flavored
"hybrid") and bring up a **second CPU core**, using a **shared run queue**
across both cores. The intent is that later adding per-thread multithreading
(threads within a process) slots in without reworking the scheduler core.

Scope is correctness first: the system must boot with 2 CPUs detected and run
all existing user programs correctly. Measurable parallel speedup is out of
scope now.

## Approach

- **C (hybrid)**: keep the current by-value `Thread` + round-robin + software
  parking (`PENDING_BLOCK`) semantics, but restructure the global
  `CURRENT_THREAD` into a **per-CPU current-thread slot** (`CpuLocal.current`),
  and add dual-core SMP.
- **SMP = (a) shared run queue**: one global `SCHEDULER` + 3 wait queues; each
  CPU owns its own current thread, TSS, GDT, syscall state, and timer. Both
  cores pull runnable work from the same queue.
- **Wake mechanism (pinned)**: APs wake via a **condition on the shared-queue
  lock** — simplest correct option for 2 cores. An IPI-based wake is a later
  refinement (comes with per-CPU queues).

## Long-run design goal: decouple the switch from the ISR

Today the context switch is fused into the timer naked-asm handler
(`time.rs:87`): the ISR both saves the frame *and* performs the round-robin
switch. SMP does not strictly require changing this, but the single highest
long-run value change is extracting a proper `context_switch(from, to)`
primitive that the **scheduler** calls:

- the ISR only saves a frame and hands control to the scheduler;
- the scheduler decides (park / rr / block) and calls `context_switch`.

This is the Redox shape. It is the change that later unlocks priorities,
per-CPU run queues, and per-process threads. It is therefore an explicit design
goal of this work (the current fused switch stays correct meanwhile; the
`CpuLocal` shape makes this an internal change, not a rewrite).

## Target architecture

### Per-CPU state

Each CPU gets its own `CpuLocal` block held in a kernel-static array (high-half,
never shared):

```rust
struct CpuLocal {
    id: u32,
    tss: Tss,                  // per-CPU TSS → rsp0
    gdt: Gdt,                  // per-CPU GDT (identical selectors, own TSS entry)
    state: CpuState,           // syscall state — replaces global CPU_STATE
    current: Option<Thread>,   // this core's running thread
    idle_stack_top: u64,       // AP park/wake stack
    active: bool,              // has this core started?
}

static CPUS: [CpuLocal; MAX_CPUS] = ...;   // kernel-static
static CPU_COUNT: AtomicUsize;             // detected at boot
```

### Globals (shared, lock-protected, unchanged)

`SCHEDULER`, `KEYBOARD_WAIT_QUEUE`, `CHILD_WAIT_QUEUE`, `PIPE_WAIT_QUEUE`,
`PROCESS_TABLE`. All queue mutations take the same global lock.

### Per-core timer switch

Replaces the single-core handler in `time.rs`. On core `k`:

1. Take `CPUS[k].current`, save `stack_ptr = current_stack_ptr`.
2. If a pending block and a runnable alternative exists on the shared queue →
   park current into the right wait queue.
3. Else round-robin: push current to shared `SCHEDULER` back, pop front as
   `next`.
4. Set `CPUS[k].current = next`; switch `cr3` (if differs), `fs_base`,
   `tss.rsp0 = next.kernel_stack_top`, `CPUS[k].state.kernel_stack =
   next.syscall_stack_top`.
5. Return `next.stack_ptr` → naked asm pops the saved frame; `iretq` resumes
   exactly where `next` was interrupted.

INV5 ("current is None at tick") becomes per-core: only a core that has a
current thread and `THREADS_ACTIVE` set panics.

### AP bring-up & wake model

- **Bring-up**: BSP detects CPU count via MADT/APIC (`acpi.rs`). Take CPU0 =
  BSP; wake CPU1 (QEMU: 2 vCPUs).
- **Trampoline**: real-mode stub in low memory → 32-bit → long mode via a
  low identity-mapped page table → jump to Rust `ap_entry(id)`.
- **`ap_entry(id)`**: build per-CPU GDT/TSS/IDT from `CPUS[id]`, set rsp0,
  fs/gs, install AP timer, mark `active`, then enter the idle wake loop.
- **Wake model (confirmed)**: APs do **not** busy-poll the shared queue. A
  global wake event (`RUNNABLE_COUNT: AtomicUsize` + an IPI, or a condition on
  the shared-queue lock) signals parked APs when a runnable thread is available.
  BSP never blocks; it keeps running the shell and pulls from the same queue.
  - When a thread is pushed to `SCHEDULER` and an AP is parked idle, send a
    wake (IPI).
  - AP wakes, takes the global lock, pops a thread, runs it; if the queue
    empties, it re-parks.
- Exec/shell bootstrap stays on the BSP.

## File-backed mmap (new feature, part of this spec)

### Current state

- `sys_mmap` (`syscalls.rs:565`) supports only `MAP_DEVICE` (mmap a phys
  address) and `MAP_ANONYMOUS`. Anything else returns `EINVAL`. It takes no
  `fd`/`offset` args.
- `VmaKind` is only `Anonymous`, `Heap`, `Device` — no file-backed kind.
- Demand paging works but **only for anonymous zero-fill**: `map_demand_range`
  marks pages `DEMAND_PAGING`; `demand_paging_fault` (`space.rs:413`) allocates
  a zeroed frame on first touch. No file content is ever read into a page.

### Design

Extend `mmap` to the POSIX shape `mmap(addr, len, prot, flags, fd, offset)`
with `MAP_SHARED`/`MAP_PRIVATE` (no `MAP_ANONYMOUS`) backed by a file:

- **VMA records the file source.** Keep `Vma` cheap/`Copy` by storing the
  descriptor + file offset rather than an `Arc`:
  ```rust
  enum VmaKind {
      Anonymous,
      Heap,
      Device { phys: u64 },
      File { fd: u32, offset: u64 },   // offset = file offset of VMA start
  }
  ```
- **`sys_mmap`**: when not `MAP_ANONYMOUS`/`MAP_DEVICE`, resolve `fd` to a
  `VNode` via the process fd table (validate it is a regular file, not a dir/pipe
  — `EBADF`/`EINVAL` otherwise), create the `VmaKind::File` VMA, and
  `map_demand_range` the whole range (no reads yet).
- **Fault path** (`demand_paging_fault`): for a `File` VMA, on first touch of
  page `i` compute `file_off = vma.offset + i*PAGE_SIZE`, look up the fd's
  `VNode` through the *current process* fd table, and `read_at(file_off, frame)`
  instead of zero-fill (short reads leave the tail zeroed — handles EOF).
  `MAP_PRIVATE` (COW): first touch maps a read-only page, and a write fault
  triggers the existing `cow_write_fault` path; `MAP_SHARED`: page stays
  writable, and (later) dirty pages are written back via `write_at` on
  `msync`/`munmap` — see out of scope.
- **fault handler wiring**: `page_fault_handler` (`interrupts.rs:103`) currently
  hands `cr2` to `demand_paging_fault` with no VMA context. The fault path must
  find the current process's `VmaList` (via the per-CPU current thread), locate
  the containing VMA, and dispatch on `VmaKind::File`.

### Why in the spec

It uses the same infrastructure this work touches (per-CPU current thread,
demand paging, VMA list), so designing it together avoids a later rework of the
fault path.

## Design invariants / acceptance criteria

1. **No cross-core stack sharing**: kernel stack, syscall stack, interrupt
   (rsp0) stack strictly per-thread *and* per-CPU as appropriate. Audit every
   `static` that is really per-CPU (currently global `CPU_STATE`, `SYSCALL_STACK`
   usage, TSS, `FPU_SCRATCH`).
2. **Syscall/vfs cleanup**: remove syscalls with no caller; route fd file-ops
   (read/write/seek) that duplicate VFS paths through the VFS layer. Do a
   caller/usage sweep rather than guess.
3. **rsp0 correctness**: TSS.rsp0 = the interrupted thread's kernel stack on
   every switch, on every CPU.
4. **Interrupt invariants preserved**: naked handler keeps the exact
   register-save/restore contract; a switch resumes exactly where interrupted
   (no register clobber across the boundary).
5. **Stack alignment** (16-byte) everywhere: interrupt frames, syscall entry,
   call frames.
6. **cr3 switched correctly**: reload target's page tables on the core before
   entering a new thread's user context.
7. **Resume from the right point**: don't break the iretq-to-current-rsp model
   when a switch crosses from an ISR.

### Per-CPU debug invariants

- The INV1-5 checks (`tasks.rs`) are reworked to be per-CPU-aware:
  - INV5 ("current is None at tick") fires only on a core that *has* a current
    thread and `THREADS_ACTIVE` is set.
  - `check_no_double_owner` semantics under 2 cores: a thread may appear in at
    most one place — exactly one `CpuLocal.current` OR the shared `SCHEDULER`
    OR one wait queue — never two.
  - `check_thread` / `check_scheduler` keep validating frame shape (CS, RIP,
    aligned, canonical kernel addresses) on every switch path, both cores.

## Testing / verification

- Boot logs assert both cores are up: `CPUS[1].active == true` and CPUID count.
- Existing boot tests (doom, songplayer, fork/wait, shell) pass on 2 cores.
- Add a 2-core parity check: two busy loops with distinct counters, verifying
  both cores actually run (e.g. CPUID read inside each loop reports both ids).

## Out of scope

- Measurable parallel speedup / benchmark.
- Per-process multithreading.
- True per-CPU run queues + load balancing + cross-CPU IPI wakeups for
  arbitrary blocking (later refinement; the `CpuLocal` shape makes this an
  internal change, not a rewrite).
- File-backed mmap *write-back* for `MAP_SHARED` (`msync`/`munmap` dirty-page
  flush via `write_at`) — the read/COW side is in scope, write-back later.
- `MAP_FIXED` beyond the existing address handling; `MAP_SHARED` semantics for
  cross-process sharing of file pages (later, with a shared page cache).
