# Syscall Completion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Repo note:** this is not a git repo, so there are no `git commit` steps. Each task ends with the build + QEMU verification run (the same command is the "checkpoint").

**Goal:** Replace stub/inaccurate syscalls with Linux-accurate behavior so musl-linked user programs (busybox, bash) get correct semantics: real uid/gid, real `getrandom`, working `setpgid`/`fcntl(F_DUPFD)`, correct `brk`/`mmap` interplay, POSIX signal edge cases, and `clock_gettime`. Add the missing syscalls the ported binaries already attempt.

**Architecture:** First, a per-syscall audit table mapping every arm in `syscall_rust_handler` to its Linux man-page contract. Then fix stubs one at a time, each with a userspace probe (a small C program compiled against musl, run via the existing `exec` path) that asserts the expected return/errno. Syscalls are pure kernel logic — every fix is testable headlessly via serial prints.

**Tech Stack:** Rust `#![no_std]` kernel, x86_64, musl cross-toolchain already at repo root (`x86_64-linux-musl-native/`), busybox/bash in initrd, `./scripts/build-kernel.sh` + `cargo test -p kernel --lib --target x86_64-unknown-none` + `./scripts/smoke.sh`.

---

## Current state (from the syscall audit)

Dispatch lives in `kernel/src/arch/x86_64/syscalls.rs` (`syscall_rust_handler`, currently a 1281-line match — **execute the refactor plan first**, `2026-08-08-kernel-refactor-phases.md` Phase 4, so each syscall is its own function). Known problems:

| Syscall | Current behavior | Linux-accurate behavior |
|---|---|---|
| `getuid/getgid` 102/104/107/108 | always `0` | real per-process creds; return `0` only if uid set to 0 |
| `setuid/setgid` 105/106 | `0` (pretend root) | `EPERM` unless CAP_SETUID; update process creds |
| `getppid` 110 | wrong arm comment; shares stub | read from process table |
| `getrandom` (318) | falls to default/unknown arm | fill buffer with entropy, return byte count; `EINVAL` on bad flags |
| `set_tid_address` (218) | `return 1` | set the thread's `clear_child_tid` futex, return the tid |
| `setpgid` 109 | stub returns 0 | validate pgid, update pgrp; `ESRCH`/`EACCES` on error |
| `fcntl` F_DUPFD (72, cmd 0) | returns 0 | return lowest available fd ≥ arg |
| `getcwd` 79 | partial | correct for non-initrd cwd |
| `stat/fstat` 4/5 | `st_mode` from VNode only | full stat fields, correct `st_size` |
| `read` 0 / `write` 1 | start-ptr check only (fixed by refactor Task 1) | validate full `[ptr, ptr+len)`; `EINVAL`/`EFAULT` |
| `mmap` 9 | maps without COW/demand flags | honor MAP_ANONYMOUS vs file, PROT, offset; ENOMEM |
| `brk` 12 | bump within heap | grow/shrink heap, `ENOMEM` at limit |
| `poll` 7 | readiness only, no blocking | blocking poll on empty pipe/char |
| `wait4` 61 | reaps zombies | `WNOHANG`, `WUNTRACED`, correct status encoding |
| `kill` 62 | signal to pid | pid 0 = process group, pid -1 = all, `ESRCH` |
| `signal`/`rt_sigaction` 13 | blocked sigs rejected | real-time signal semantics, SA_RESTART/SA_SIGINFO |
| `rt_sigreturn` 15 | FPU restore | full frame restore including `ucontext` |
| `uname` 63 | hardcoded | fill from kernel build info; correct `release`/`version` |
| `writev` 20 | re-implements write | validate iovec array fully; partial writes |
| `ioctl` 16 | termios emulation | TCGETS/TCSETS/TCGWINSZ correct; `ENOTTY` on non-tty |

**Missing syscalls** the binaries call (add stubs → implement): `clock_gettime` (228), `clock_getres`, `nanosleep` (35), `getpid` (39, exists via arm), `gettid` (186, exists), `close_range` (436), `getdents`/`getdents64` (217/220, needed for busybox `ls`), `fstatat` (262), `newfstatat`, `access` (21), `faccessat`, `readlink` (89), `mkdir`/`mkdirat`, `openat` (257), `pipe2` (293, exists), `sched_yield` (124), `getpriority`/`setpriority`, `prctl` (157), `getrusage`, `times` (100), `clock_nanosleep`.

---

### Task 1: Syscall audit table (foundation)

**Files:**
- Create: `docs/superpowers/notes/syscall-audit.md`

- [ ] **Step 1: Generate the audit table**

For each arm number in `syscall_rust_handler`, record: number, Linux name, current behavior (read the code), discrepancy vs man page, and a one-line acceptance probe. Use `grep` to enumerate arms, then fill from the man pages. The table drives every later task; each later task references its audit row. Example row format:

```
| 102 | getuid | returns 0 always | return process.uid | probe: getuid()==0 when root, ==1000 after setuid |
```

- [ ] **Step 2: Verify the table is complete**

`grep -oE "^\s+[0-9]+(\s*\|\s*[0-9]+)?\s*=>" kernel/src/arch/x86_64/syscalls.rs` must produce a set of numbers matching the table rows. Any number in code not in the table is a gap — fix the table before proceeding.

---

### Task 2: Process credentials (uid/gid)

**Files:**
- Modify: `kernel/src/tasks.rs` (`Process`)
- Modify: `kernel/src/arch/x86_64/syscalls.rs` (uid/gid arms)

- [ ] **Step 1: Add creds to `Process`**

```rust
// in Process
pub uid: u32,
pub gid: u32,
pub euid: u32,
pub egid: u32,
```
Initialize all to 0 (root) in `register_exec_thread` and the fork/exec `Process { .. }` literals.

- [ ] **Step 2: Implement uid/gid arms**

`getuid` → `process.uid`; `getgid` → `process.gid`; `geteuid` → `process.euid`; `getegid` → `process.egid`; `getppid` → `process.ppid`. `setuid` → `EPERM` unless `euid == 0`, then set all; `setgid` analogous.

- [ ] **Step 3: Probe**

Compile with the repo's musl toolchain:
```c
// probe_creds.c
#include <unistd.h>
#include <stdio.h>
int main() {
    printf("uid=%d gid=%d\n", getuid(), getgid());
    if (setuid(1000) == 0) printf("setuid ok\n");
    else printf("setuid errno=%d\n", __errno_location()[0]);
    return 0;
}
```
Add to initrd, run via `exec probe_creds`. Expected serial: `uid=0 gid=0` then `setuid errno=1` (EPERM, not success). Verify the `setuid` path never returns 0 for a non-root process.

```bash
./scripts/build-kernel.sh && ./scripts/smoke.sh
```

---

### Task 3: `getrandom` (318) and `set_tid_address` (218)

**Files:**
- Modify: `kernel/src/arch/x86_64/syscalls.rs`
- Create: `kernel/src/arch/x86_64/random.rs`

- [ ] **Step 1: Entropy source (RDRAND, QEMU supports it with `-cpu host`)**

```rust
// kernel/src/arch/x86_64/random.rs
pub fn rdrand_u64() -> Option<u64> {
    let mut val: u64 = 0;
    let mut ok: u8 = 0;
    unsafe { core::arch::asm!("rdrand {}", out(reg) val, "setc {}", out(reg) ok); }
    if ok != 0 { Some(val) } else { None }
}

/// Fills buf with hardware RNG bytes; falls back to a xorshift seeded from
/// TICKS + lapic_id if RDRAND is absent.
pub fn fill_random(buf: &mut [u8]) {
    for chunk in buf.chunks_mut(8) {
        let r = rdrand_u64().unwrap_or_else(|| fallback_u64());
        for (i, b) in chunk.iter_mut().enumerate() {
            *b = (r >> (i * 8)) as u8;
        }
    }
}
```

- [ ] **Step 2: Implement the syscalls**

Linux `getrandom(void *buf, size_t buflen, unsigned int flags)`: flags must be 0 or `GRND_NONBLOCK`(1)/`GRND_RANDOM`(2); `EINVAL` for anything else. Copy `buflen` bytes via `is_user_range` + `from_raw_parts_mut`; return `buflen`. **Dispatch number 318.**

`set_tid_address(void *tidptr)`: store `tidptr` on the current `Thread`; if a previously-registered tidptr exists, write the tid there (glibc/musl use this for thread exit); return the tid.

- [ ] **Step 3: Probe**

`dd if=/dev/urandom` isn't present; use a C probe: `getrandom(buf, 16, 0)`, print the 16 bytes as hex. Expected: non-zero, varying across two runs.

```bash
./scripts/build-kernel.sh && ./scripts/smoke.sh
```

---

### Task 4: `fcntl(F_DUPFD)` + `close_range`

**Files:**
- Modify: `kernel/src/arch/x86_64/syscalls.rs`

- [ ] **Step 1: `fcntl(F_DUPFD)`**

cmd 0: `dup(oldfd)` with `newfd >= arg`. Scan `fd_table` for the lowest unused fd ≥ `arg`; if none, `EMFILE` (24). Reuse the existing `dup` fd-slot logic; factor a `dup_fd(old, new_min)` helper so `dup`/`dup2`/`F_DUPFD` share it.

- [ ] **Step 2: `close_range(first, last, flags)` (436)**

Loop fds `first..=last`, close each, `flags==0` only (else `EINVAL`).

- [ ] **Step 3: Probe**

C probe: open a file twice (two fds 3,4), `fcntl(3, F_DUPFD, 10)` → expect 10; `close_range(3, 100, 0)` → `fcntl(3, F_GETFD)` → `EBADF` (-9).

```bash
./scripts/build-kernel.sh && ./scripts/smoke.sh
```

---

### Task 5: `setpgid` + process groups

**Files:**
- Modify: `kernel/src/tasks.rs` (`Process.pgid`, group bookkeeping)
- Modify: `kernel/src/arch/x86_64/syscalls.rs` (kill pid-0/-1 semantics use groups)

- [ ] **Step 1: Add `pgid` to `Process`**

Init `pgid = pid` in `register_exec_thread`/fork. `setpgid(pid, pgid)`: validate `pid` exists (`ESRCH`), `pgid` must be 0 (→ pid) or an existing group (`ESRCH`); only the process itself or its parent may call it (`EACCES`); a process that has `exec`ed may not change groups (`EACCES`).

- [ ] **Step 2: `kill` group semantics**

`kill(0, sig)` → signal the caller's process group; `kill(-1, sig)` → all processes except the kernel; `kill(-pgid, sig)` → that group. Implement by scanning `PROCESS_TABLE`.

- [ ] **Step 3: Probe**

C probe: `fork()` child, child calls `setpgid(0, 0)`, parent `kill(0, SIGUSR1)` → both exit non-zero with `128+SIGUSR1`. Verify via `wait4` status.

```bash
./scripts/build-kernel.sh && ./scripts/smoke.sh
```

---

### Task 6: `clock_gettime` + `nanosleep`

**Files:**
- Create: `kernel/src/time/clock.rs` (or extend `time.rs`)
- Modify: `kernel/src/arch/x86_64/syscalls.rs`

- [ ] **Step 1: Time source**

`CLOCK_MONOTONIC` = `TICKS` scaled to ns (1ms tick → `ticks * 1_000_000`). `CLOCK_REALTIME` = RTC time at boot + monotonic delta (`rtc::read_datetime()` already exists, `kernel/src/drivers/rtc.rs`). Both as `timespec { tv_sec: i64, tv_nsec: i64 }`.

- [ ] **Step 2: Implement `clock_gettime` (228) + `clock_getres` (229)**

Validate the clock id (0=REALTIME, 1=MONOTONIC; else `EINVAL`). Copy the 16-byte `timespec` into user memory. Return 0.

- [ ] **Step 3: Implement `nanosleep` (35)**

Block the current thread for `req.tv_sec`/`req.tv_nsec` via a new `PendingBlock::Sleep { until_ticks: u64 }`; the timer wakes it (add a sleep-wake path in `time.rs` that checks `until_ticks <= TICKS`). Use the existing block/resume machinery; do not busy-wait.

- [ ] **Step 4: Probe**

C probe: `clock_gettime(CLOCK_MONOTONIC, &ts)`, `nanosleep(0.1s)`, then print elapsed ns — expect ≈100ms ±20%. Two runs must differ.

```bash
./scripts/build-kernel.sh && ./scripts/smoke.sh
```

---

### Task 7: `getdents64` (busybox `ls`)

**Files:**
- Modify: `kernel/src/arch/x86_64/syscalls.rs`

- [ ] **Step 1: Directory iteration**

Implement `getdents64(fd, dirp, count)` against `VNode::Directory`. Build `struct linux_dirent64 { d_ino, d_off, d_reclen, d_type, d_name[] }` entries by scanning the initrd TAR for names under the directory's prefix (reuse the `TarEntries` iterator from the refactor). Track per-fd read offset so repeated calls continue.

- [ ] **Step 2: Verify with busybox `ls`**

Add `/bin/ls` (busybox symlink already in initrd) to the shell's `ls` fallback or run `exec ls /`. Expected: same listing as the shell builtin, no `EINVAL`/`EBADF`.

```bash
./scripts/build-kernel.sh && ./scripts/smoke.sh
```

---

### Task 8: `poll` blocking + `openat`/`fstatat`/`newfstatat`/`access`

**Files:**
- Modify: `kernel/src/arch/x86_64/syscalls.rs`

- [ ] **Step 1: Blocking `poll`**

When all polled fds are `PIPE_WAIT`-ready-false and `timeout != 0`, block via `PendingBlock::PipeWait` and retry on wake (loop). Honoring `timeout` requires the sleep path from Task 6; wire `poll(timeout)` to wake at deadline.

- [ ] **Step 2: `openat` (257) / `fstatat` (262) / `newfstatat` (256)**

Route to the existing `open`/`fstat` logic with `dirfd` honored as `AT_FDCWD` (-100) for now; `AT_SYMLINK_NOFOLLOW` unimplemented (no symlinks yet). `access` (21): check the fd resolves (R_OK on InitrdFile always OK; no permission bits in the TAR).

- [ ] **Step 3: Probe**

busybox `cat /bin/sh` and `cat /bin/busybox` via `exec`/shell fallback. Expected: file contents print; `access("/bin/sh", R_OK) == 0`.

```bash
./scripts/build-kernel.sh && ./scripts/smoke.sh
```

---

### Task 9: `wait4` status encoding + signal edge cases

**Files:**
- Modify: `kernel/src/arch/x86_64/syscalls.rs`
- Modify: `kernel/src/tasks.rs` (`deliver_pending`, `exit_current_thread`)

- [ ] **Step 1: `wait4` status**

Encode `WEXITSTATUS` (byte 8-15 of status for normal exit), `WIFSIGNALED` (bit 7 = 1, low 7 bits = signal), and honor `WNOHANG` (return 0 if a child still running), `WUNTRACED` (return stopped child). Check `__WALL`-style flags (bits 30) for the `getpid` edge used by bash.

- [ ] **Step 2: Signal defaults**

`SIGCHLD` disposition default ignore; `SIGPIPE` default terminate (already via `handle_sigpipe`); `SIGSEGV`/`SIGILL` default → terminate with `128+sig` coreless. Ensure a defaulted signal that is *blocked* stays pending and delivers when unblocked (`rt_sigprocmask` arm already handles mask).

- [ ] **Step 3: Probe**

C probe: child `raise(SIGTERM)`; parent `wait4(pid, &status, 0, 0)` → `WIFSIGNALED(status) && WTERMSIG(status)==15`, exit code 128+15. Then a child `exit(7)` → `WEXITSTATUS(status)==7`.

```bash
./scripts/build-kernel.sh && ./scripts/smoke.sh
```

---

### Task 10: Regression + syscall self-test

**Files:**
- Create: `kernel/src/test_runner.rs` additions or a standalone `#[test_case]`

- [ ] **Step 1: Kernel-side assertions**

Add `#[cfg(test)] #[test_case]` functions for pure logic now separable from hardware: errno encoding, `timespec` math, dirent64 packing, `WEXITSTATUS`/`WIFSIGNALED` encoding. Run under `cargo test -p kernel --lib --target x86_64-unknown-none`.

- [ ] **Step 2: Full probe suite**

Rebuild initrd with all probes (`probe_creds`, `probe_random`, `probe_fcntl`, `probe_pgid`, `probe_clock`, `probe_wait`), run each through `exec`, capture serial, assert each expected output line appears exactly once.

```bash
./scripts/build-kernel.sh && ./scripts/smoke.sh && cargo test -p kernel --lib --target x86_64-unknown-none
```

---

## Deferred (explicitly out of scope)
- Real `execve` argv/envp correctness beyond current — separate plan.
- `mmap` file-backed + COW semantics — needs storage (Phase 5) and is intertwined with the fork plan; keep anonymous-only for now.
- Signals: `SA_SIGINFO` siginfo fields, signal masks in `fork` — partial here, full in the fork/exec plan.
- `select`/`epoll`/`signalfd` — poll-only for now.
- 32-bit `x86` compat layer, io_uring, futex (threads need SMP first).
