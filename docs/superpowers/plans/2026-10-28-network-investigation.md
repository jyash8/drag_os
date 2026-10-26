# Phase 8: Network Investigation Implementation Plan

## Goal
Isolate and fix the kernel's smoltcp networking path so `curl` (DNS + HTTP) works.
Currently logic is correct but blocks because NET.lock() starvation. The bug is now precisely bounded (smoltcp `Interface` + `virtio-net` device glue).

## Confirmed empirical failure signature
1. virtio-net `init ok mac=52:54:00:12:34:56` (IRQ5, driver-ready) broadcasts MAC; smoltcp's `HardwareAddress::Ethernet`.
2. `net::poll()` (timer tick, every ~1ms, _try_lock_) reports "activity" FALSE even with sockets bound, SO TX/RX never happen. TX_ATTEMPT=0 always.
3. Zero bytes sent/received across virtual NIC for the entire boot, even when open sockets exist.

## Most likely fixes (ordered)
A. Context mismatch: smoltcp `Config` vs netlink-less virtio-net spawn. Device does `.capabilities()` but its `MAC` from QEMU-probe needs `\n` handling? Maybe correct but the TX queue doesn't actually notify QEMU.
B. **Deadlock hierarchy:** `sys_connect` outer holds `NET.lock()` (blocking). Hold across `block_current_thread` — dangerous. Make non-blocking: only save `net_guard` state into deferred tasks and do poll-only polling.
C. timer-tick gated vs syscall-time: `net::poll` called in syscall paths (every syscall!) WHILE NET lock is locked by another thread. `try_lock` masked the failure. Use per-CPU NET buffer/ring shared but avoid double lock.

## Constraints (AGENTS.md s1, s2, s10.9)
- SCHEDULER->CPUS[k]->wait-queues->process.lock(); NET/DEVSS are tossed.
- SpinLock doesn't reenter (track_caller panic). So any holding of NET lock across an ISR (block_current_thread sti;int 0x81) can cause irq-level pollution.
- NEVER call `current_thread()` while holding CPUS (binds s1).
- `net::poll` only runs from `crate::time::timer_rust_handler` at CPU0 with IF=0; `sys_*` calls it via synchronous `crate::net::poll()` guarded by try_lock.

## Review Focus
- Is `net::poll` called at ALL wire-level (any synchronous RxToken delivered via irq context).
- `TX_ATTEMPT` reach non-zero (proof of TX).
- sys_connect tcp path returns/exits correctly (glibc `strerror` debug through syscall returns only).
- 10.0.2.3/53 UDP DNS query from glibc -> smoltcp -> virtio-net TX -> QEMU replies -> socket recv (glibc gets results).
- `curl http://10.0.2.2/` should then succeed.