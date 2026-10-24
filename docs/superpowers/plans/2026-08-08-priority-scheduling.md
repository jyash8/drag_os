# Priority-Based Scheduling Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Repo note:** this is not a git repo, so there are no `git commit` steps. Each task ends with the build + QEMU verification run (the same command is the "checkpoint").

**Goal:** Replace the FIFO round-robin scheduler (`SCHEDULER: SpinLock<VecDeque<Thread>>`) with a priority-aware scheduler using a modern design: a vruntime-keyed runqueue (CFS-style) with priority weights, so interactive tasks get CPU promptly and all threads make progress. Keep the interface change minimal so the SMP plan (which splits the runqueue per-CPU) is a mechanical swap.

**Architecture:** Replace the single `VecDeque<Thread>` with an ordered structure keyed by `vruntime`. Each tick: pick the thread with the *smallest* vruntime (most under-served); advance vruntime by `exec_time / weight(priority)` when a thread runs. Priorities come from a `nice`-like value stored on `Thread`; the `nice` syscall adjusts it. The scheduler exposes a fixed API (`pick_next`, `enqueue`, `dequeue`, `reschedule`) so `time.rs`, `tasks.rs`, and `syscalls.rs` never touch the queue structure directly.

**Tech Stack:** Rust `#![no_std]` kernel, x86_64, `alloc` collections (`BinaryHeap` with a reversed custom `Ord`), `cargo test -p kernel --lib --target x86_64-unknown-none` for pure scheduler-logic tests (vruntime math, heap ordering, weight table), `./scripts/smoke.sh` for boot behavior.

---

## Current state

- `kernel/src/tasks.rs:127-131`: `SCHEDULER: SpinLock<VecDeque<Thread>>`, `CURRENT_THREAD`, three wait queues.
- `kernel/src/time.rs:71-169`: `timer_rust_handler` pops front, pushes back (round-robin), calls `deliver_pending` on a real tick, and handles the `PENDING_BLOCK` protocol. **The 98 scheduling lines live in time.rs** — this plan extracts them.
- `kernel/src/tasks.rs:155-167`: `block_current_thread` sets `PENDING_BLOCK` and traps a timer tick to park the thread.
- `syscall_rust_handler`: the `sched_yield`/`nice`/`getpriority`/`setpriority` arms are stubs.

## Design

### Runqueue: vruntime min-heap

A `BinaryHeap` in Rust is a max-heap by default; wrap with reversed `Ord` so the smallest vruntime sits on top. `Thread` needs `vruntime: u64` (atomic-free; only the owning CPU touches it — but the SMP plan splits the queue, so keep reads/writes behind the queue's lock).

```rust
// in kernel/src/tasks.rs (scheduler module extracted in Task 1)

/// Scheduling weight per nice level. Higher = gets more CPU.
const WEIGHTS: [u64; 40] = [
    // nice -20 .. +19. 1024 is baseline (nice 0).
    // Computed as 1024 * 1.25^(-nice) for negative nice, /1.25 for positive.
    // (filled in Task 2 with the exact table)
];

/// A thread in the runqueue, ordered by vruntime (min first).
pub struct RunQueueEntry {
    pub thread: Thread,
    pub vruntime: u64,
}
```

### Thread changes

```rust
// in Thread
pub nice: i32,       // -20..=19, default 0
pub vruntime: u64,   // maintained by the scheduler
```

### Scheduler API (all behind `SCHEDULER` lock)

```rust
pub fn sched_enqueue(t: Thread) { /* compute weight, push, sift by vruntime */ }
pub fn sched_dequeue(tid: u64) -> bool { /* remove by tid */ }
pub fn sched_pick_next() -> Option<Thread> { /* pop min vruntime */ }
pub fn sched_account(thread: &mut Thread, ticks: u64) { /* vruntime += ticks * 1024 / weight(nice) */ }
pub fn sched_yield_current() { /* re-enqueue current at back, pick next */ }
```

`time.rs`'s `timer_rust_handler` becomes: EOI → account current thread's runtime → `deliver_pending` (unchanged) → `pick_next` → switch (via `switch_to_thread` from the refactor plan). The `PENDING_BLOCK` parking logic moves into `tasks.rs` as `sched_block(target)`.

---

### Task 1: Extract scheduler from `time.rs` into `kernel/src/sched.rs`
- [x] **Step 1: Move the runqueue + blocking protocol**
- [x] **Step 2: Verify the pure move**

---

### Task 2: vruntime accounting
- [x] **Step 1: Add fields to `Thread`**
- [x] **Step 2: Weight table**
- [x] **Step 3: `sched_account`**
- [x] **Step 4: Unit test the vruntime math**
- [x] **Step 5: Verify**

---

### Task 3: Ordered runqueue (min-vruntime first)
- [x] **Step 1: Replace the `VecDeque` with an ordered structure**
- [x] **Step 2: Rewrite the accessors**
- [x] **Step 3: Handle the "pick same thread again" case**
- [x] **Step 4: Unit test ordering**
- [x] **Step 5: Verify**

---

### Task 4: `nice` / `getpriority` / `setpriority` / `sched_yield`
- [x] **Step 1: `nice(inc)` (34)**
- [x] **Step 2: `getpriority(which, who)` (140) / `setpriority(which, who, prio)` (141)**
- [x] **Step 3: `sched_yield()` (24)**
- [x] **Step 4: Probe**

---

### Task 5: Fix interactivity — small vruntime boost for wakeups
- [x] **Step 1: Boost on wake**
- [x] **Step 2: Verify interactivity**

---

### Task 6: Regression + instrumentation off
- [x] **Step 1: Full suite**

```bash
./scripts/build-kernel.sh && cargo test -p kernel --lib --target x86_64-unknown-none && ./scripts/smoke.sh
```

- [ ] **Step 2: Long-run stability**

Run smoke 8×. Then run a 60s QEMU session with 4 busy user processes + shell; assert zero panics and that all 4 processes' counters advance monotonically (a counter that freezes = starvation = failure).

- [ ] **Step 3: Remove all temporary probes/counters**

`grep -rn "DBG\|PROBE\|TEMP" kernel/src/` returns nothing new.

---

## Deferred (explicitly out of scope)
- Per-CPU runqueues / work stealing — SMP plan (`2026-08-08-smp-multicore.md`) owns the queue split; this plan keeps the queue behind the `sched_*` API so that split is mechanical.
- RT scheduling classes (SCHED_FIFO/RR) — future work; the priority bands here are the foundation.
- Sleep-time averaging / `slack` accounting, wakee preemption heuristics (CFS's `wakeup_preempt_entity`).
- Cgroup bandwidth, autogroup.
- Tickless / hrtimer-based scheduling — needs the APIC timer resolution to be worth it.
