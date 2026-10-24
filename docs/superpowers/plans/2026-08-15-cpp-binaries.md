# C++ Binaries on DragOS

Date: 2026-08-15

## Objective

Run C++ binaries (iostream, std::vector/string, exceptions, std::thread/mutex) on DragOS.
Static C++ already works today; this plan closes the dynamic-linking and threading gaps.

## Important Details

- **Toolchain:** `x86_64-linux-musl-native/` at project root. Ships its OWN `libstdc++` —
  the earlier "C++ needs glibc" conclusion was mesa-specific, not C++ in general.
- **Key discovery:** kernel interpreter/DSO wiring is ALREADY DONE in current code
  (`load_image` takes `interp_inode`, creates `VmaKind::MappedFile` VMAs at loader.rs:80-98,
  `file_demand_paging_fault` handles them at filemmap.rs:230). No ELF-loader changes needed.
- **Verified static:** `g++ -static /tmp/cpptest.cpp` → `static-pie`, runs with zero kernel changes.
- **Dynamic NEEDED chain:** libstdc++.so.6 → libc.so + libgcc_s.so.1 → libc.so.
  interp `/lib/ld-musl-x86_64.so.1`.
- **Initrd IS the disk:** build-fs.sh tars `/tmp/dragos-fs` → `esp/efi/boot/initrd.tar` (kernel
  falls back to TAR when the 8M empty disk.img ext2 fails). C++ libs must go in build-fs.sh.
- **Futex is fake** (syscalls.rs:2954-2983): WAIT = nanosleep/yield, WAKE = `0`. `std::thread`,
  `std::mutex`, `condition_variable`, `pthread_join` all need real futex + thread-exit wake.
- **set_tid_address** (syscalls.rs:1814) returns hardcoded `1`; `sys_clone` (1213) handles
  CLONE_CHILD_SETTID/CLEARTID writes but nothing stores `child_tid` for exit-time wake.
  `exit_current_thread` (tasks.rs:425) wakes `CHILD_WAIT_QUEUE` only.
- **Verification harness:** `python3 scripts/boot_test.py "<cmd>" "<expected-output>"` (pexpect,
  `./scripts/qemu.sh -display none`, waits `'/ #'`). `scripts/smoke.sh` greps PANIC/CORRUPT, needs `/ #`.
- Ponytail full mode: shortest working diff per task. Repo NOT git — each task ends with build +
  QEMU verification, not commits.

## Tasks

### 1. Ship musl C++ runtime on initrd

Copy musl libs into build-fs.sh staging so dynamic C++ can load:
- `/home/jyash8/Projects/drag_os/x86_64-linux-musl-native/lib/libc.so` → `/tmp/dragos-fs/lib/libc.so`
- `libstdc++.so.6.0.29` → `/tmp/dragos-fs/lib/libstdc++.so.6` (copy, then rename to SONAME)
- `libgcc_s.so.1` → `/tmp/dragos-fs/lib/libgcc_s.so.1`
- Ensure `ld-musl-x86_64.so.1` present (build-fs.sh already copies it).

musl's `ld-musl-x86_64.so.1` is a symlink to `libc.so`; the kernel's interp load uses `interp_inode`
so a real file must exist at `/lib/libc.so`.

- [ ] Edit `scripts/build-fs.sh` to copy the three libs.
- [ ] `./scripts/build-fs.sh` succeeds; `tar -tf esp/efi/boot/initrd.tar | grep lib` shows libc.so, libstdc++.so.6, libgcc_s.so.1, ld-musl.
- [ ] `bash scripts/smoke.sh` → SMOKE PASS.

### 2. Dynamic C++ smoke test (interpreters + DSO cache)

- [ ] Build dynamic test: `g++ -o /tmp/dragos-fs/cpptest /tmp/cpptest.cpp` (dynamic, not -static).
- [ ] `./scripts/build-fs.sh` re-tars.
- [ ] `python3 scripts/boot_test.py "cpptest" "sum="` prints iostream/vector/string output (expect `sum=6`).
  This exercises interp load, libstdc++.so.6 DSO demand-paging, and __cxa_atexit/exit.
- [ ] If it fails: check serial for the failing syscall (rseq/set_tid_address are stubs but non-fatal; report which). Fix only the blocking syscall.

### 3. Real futex (sys_futex WAIT/WAKE)

`std::mutex` contention, `condition_variable`, and musl `pthread_join` all block on futex.

Design (mirror pipe waiters):
- Add `PendingBlock::Futex(uaddr: u64)` variant (tasks.rs:339).
- Add `FUTEX_WAIT_QUEUE: SpinLock<VecDeque<Thread>>` (pattern: PIPE_WAIT_QUEUE tasks.rs:331).
- `sys_futex` FUTEX_WAIT (no timeout): if `*uaddr == val`, `block_current_thread(Futex(uaddr))`, return 0;
  else `EAGAIN`. With timeout: validate+loop nanosleep like sys_nanosleep (no queue, keeps semantics).
- FUTEX_WAKE: pop threads whose `pending_block` is `Futex(uaddr)`, `SCHEDULER.enqueue_with_boost(t,4)`; return count.
- Handle `Futex(uaddr)` in `timer_tick_on_core` match (smp.rs:369-379) → push to FUTEX_WAIT_QUEUE.
- Ignore FUTEX_PRIVATE_FLAG/CLOCK_REALTIME flags (already masked).

- [ ] Implement PendingBlock::Futex + FUTEX_WAIT_QUEUE + smp.rs park arm.
- [ ] Rewrite sys_futex WAIT/WAKE.
- [ ] `cargo build` clean; `bash scripts/smoke.sh` → SMOKE PASS.
- [ ] `python3 scripts/boot_test.py "cpptest" "sum="` still passes.

### 4. Thread exit + join (CLONE_CHILD_CLEARTID)

`std::thread` join = futex-wait on the child's TID, woken when the child exits.
- Store `child_tid: u64` + `child_tid_written: bool` on Thread. Set in `sys_clone` when
  `CLONE_CHILD_SETTID`/`CLONE_CHILD_CLEARTID` flag present (ctid_ptr → new_tid already written; keep ptr).
- `sys_set_tid_address` (syscalls.rs:1814): store tidptr on current thread (replaces hardcoded 1).
- In `exit_current_thread` (tasks.rs:425): if thread has child_tid, write 0 to it and
  FUTEX_WAKE that address (enqueue all threads blocked on `Futex(child_tid)`).
- [ ] Add child_tid fields; wire sys_clone + set_tid_address + exit wake.
- [ ] `cargo build` clean; `bash scripts/smoke.sh` → SMOKE PASS.
- [ ] `python3 scripts/boot_test.py "cppthread" "joined"` with a new `/tmp/cppthread.cpp`
  using `std::thread` (join) + `std::mutex` (lock/inc/unlock in loop) — expect final count + "joined".

### 5. Full C++ verification + cleanup

- [ ] `g++ -static` and dynamic builds of one binary exercising: iostream, vector/string, exceptions
  (try/catch throw), std::thread join, std::mutex increment.
- [ ] Run static (task-free path) and dynamic under `scripts/boot_test.py` — all expected output.
- [ ] `cargo build` clean; `bash scripts/smoke.sh` → SMOKE PASS (regression).
- [ ] Remove any debug prints; tprintln stays for diagnostics.

## Definition of Done

- `cpptest` (dynamic) and `cppthread` (std::thread + mutex) run under `scripts/boot_test.py`
  printing expected output.
- `bash scripts/smoke.sh` → SMOKE PASS.
- Static C++ regression still passes.
