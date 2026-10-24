# Kernel Refactor — Phased Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate the confirmed code smells in the kernel — a 1281-line syscall dispatch, triplicated context-switch code, 5+ duplicate TAR walks, 9× inline user-pointer magic checks, dead code — in independently verified phases. Each phase leaves the kernel compiling and booting green before the next starts.

**Architecture:** Six phases ordered by risk (pure renames → mechanical extraction → dead-code removal → larger splits). Every phase is verified the same three ways: (1) `cargo build`, (2) the existing QEMU unit-test suite via `cargo test -p kernel --lib --target x86_64-unknown-none`, (3) a boot smoke test that greps the serial log for the shell prompt and absence of panics. No phase is started until the previous one is green.

**Tech Stack:** Rust nightly (`no_std` kernel, `x86_64-unknown-none`), QEMU KVM, OVMF UEFI, isa-debug-exit test runner. Not a git repo — no commit steps.

**Verification protocol (used by every phase):**
1. `./scripts/build-kernel.sh` — must finish with `==> Kernel copied to:` and zero errors.
2. `cargo test -p kernel --lib --target x86_64-unknown-none` — must end with `Tests passed!` (boots QEMU via `scripts/test-kernel.sh`, exits 33 on success).
3. `./scripts/smoke.sh` — must exit 0.

---

### Task 0: Verification harness (smoke script)

**Files:**
- Create: `scripts/smoke.sh`
- Modify: none

- [ ] **Step 1: Create `scripts/smoke.sh`**

```bash
#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LOG="$(mktemp)"

# Boot the real kernel headless, capture serial, kill after 12s.
timeout 25 qemu-system-x86_64 -enable-kvm -m 512M \
    -drive if=pflash,format=raw,readonly=on,file="$ROOT/OVMF_CODE.fd" \
    -drive if=pflash,format=raw,readonly=on,file="$ROOT/OVMF_VARS.fd" \
    -drive format=raw,file=fat:rw:"$ROOT/esp" \
    -display none -serial file:"$LOG" -no-reboot &
QPID=$!
sleep 12
kill $QPID 2>/dev/null || true
wait 2>/dev/null || true

# Success markers: shell prompt reached, sh exited cleanly.
grep -q "/ #" "$LOG" || { echo "SMOKE FAIL: no shell prompt"; rm -f "$LOG"; exit 1; }
grep -q "User program exited with code 0." "$LOG" || { echo "SMOKE FAIL: sh did not exit 0"; rm -f "$LOG"; exit 1; }

# Failure markers: any panic/corruption.
if grep -qE "PANIC|CORRUPT|INVALID OPCODE|DOUBLE FAULT" "$LOG"; then
    echo "SMOKE FAIL: panic/corruption detected"
    grep -E "PANIC|CORRUPT|INVALID OPCODE|DOUBLE FAULT" "$LOG"
    rm -f "$LOG"
    exit 1
fi

rm -f "$LOG"
echo "SMOKE PASS"
exit 0
```

- [ ] **Step 2: Make executable and run**

```bash
chmod +x scripts/smoke.sh && ./scripts/build-kernel.sh && ./scripts/smoke.sh
```

Expected: `SMOKE PASS` (a current boot of `main.rs` prints `/ # ` and `User program exited with code 0.` to serial).

- [ ] **Step 3: Confirm test suite baseline is green**

Run: `cargo test -p kernel --lib --target x86_64-unknown-none`
Expected: ends with `Tests passed!` (isa-debug-exit code 33).

---

### Task 1: Named constants + user-pointer helper (pure rename)

**Files:**
- Modify: `shared/src/memory/address.rs`
- Modify: `kernel/src/memory/space.rs`
- Modify: `kernel/src/arch/x86_64/syscalls.rs`

Replace the 9× inline `>= 0x8000_0000_0000_0000` magic checks with a named constant + helper. Zero behavior change.

- [ ] **Step 1: Add constants to `shared/src/memory/address.rs`**

Append after line 143 (`pub const PAGE_SIZE: u64 = PAGE_SIZE_4K;`):

```rust
/// Addresses at or above this are the kernel's higher-half; below is user space.
pub const USER_ADDR_LIMIT: u64 = 0x8000_0000_0000_0000;
```

- [ ] **Step 2: Add helper to `kernel/src/memory/space.rs`**

Add near `read_user_cstring` (line 270):

```rust
/// Returns true if the range [ptr, ptr+len) lies entirely in user space.
pub fn is_user_range(ptr: u64, len: usize) -> bool {
    ptr < shared::memory::address::USER_ADDR_LIMIT
        && ptr.saturating_add(len as u64) <= shared::memory::address::USER_ADDR_LIMIT
}
```

- [ ] **Step 3: Use the helper in `space.rs`**

In `read_user_cstring` (line 270) and `read_user_string_array` (line 285), replace the inline `ptr >= 0x8000_0000_0000_0000` check with `if !is_user_range(ptr, len) { return None; }`. Use `shared::memory::address::PAGE_SIZE` instead of the bare `4096` literals in the same functions.

- [ ] **Step 4: Replace the 7 inline checks in `syscalls.rs`**

Replace every occurrence of `>= 0x8000_0000_0000_0000` (lines 240, 291, 593, 601, 704, 1344, 1353) with `!is_user_range(buf_ptr as u64, count)` (import `is_user_range` from `crate::memory::space`). Match each existing `return EFAULT;` branch:

- Line 240 (sys_read): `if !is_user_range(buf_ptr as u64, count) { return EFAULT; }`
- Line 291 (sys_write): same shape with its `count`.
- Lines 593/601 (rt_sigaction): `if !is_user_range(oldact as u64, core::mem::size_of::<SigAction>()) { return EFAULT; }` (and `act`).
- Line 704 (pipe): `if !is_user_range(pipefd as u64, 2 * core::mem::size_of::<i32>()) { return EFAULT; }`
- Lines 1344/1353 (sigprocmask): `if !is_user_range(oldset as u64, core::mem::size_of::<u64>()) { return EFAULT; }`

This also fixes the existing bug where read/write only validated the start pointer, not `ptr+count`.

- [ ] **Step 5: Use `PAGE_SIZE`/`PAGE_MASK` in `syscalls.rs`**

Replace the `0xFFF`/`0x1000` literals at lines 501, 508–509, 530, 541–542, 557, 568–569, 1279–1280, 1291 with `shared::memory::address::PAGE_MASK`/`PAGE_SIZE`. (Import them at the top.)

- [ ] **Step 6: Verify**

```bash
./scripts/build-kernel.sh
cargo test -p kernel --lib --target x86_64-unknown-none
./scripts/smoke.sh
```

Expected: build clean, `Tests passed!`, `SMOKE PASS`.

---

### Task 2: TAR iterator refactor (one walker, kill duplicates)

**Files:**
- Modify: `kernel/src/fs/tar.rs`
- Modify: `kernel/src/fs/mod.rs`
- Modify: `kernel/src/shell.rs`
- Test: `kernel/src/lib.rs` (add `#[test_case]`)

Replace the three hand-rolled TAR walks (`parse_tar`, `find_initrd_file`, `find_vnode`) plus the two full-archive passes in shell's `ls`/`cat` with one iterator. `find_initrd_file` is dead — delete it.

- [ ] **Step 1: Rewrite `kernel/src/fs/tar.rs` header parser + iterator**

Replace the file body (keep the `TarFile` struct). `VNode::InitrdFile` holds `data: &'static [u8]` (vfs.rs:34); `INITRD.lock()` yields `&'static [u8]` so `TarEntries<'static>` items are `&'static`, matching the VNode field:

```rust
use alloc::{string::String, vec::Vec};

use crate::fs::{INITRD, vfs::VNode};

#[derive(Debug)]
pub struct TarFile<'a> {
    pub name: &'a str,
    pub size: usize,
    pub data: &'a [u8],
}

/// Parses one 512-byte TAR header into (name, size). Returns None at end-of-archive.
fn parse_header<'a>(archive: &'a [u8], offset: usize) -> Option<(&'a str, usize)> {
    let header = &archive[offset..offset + 512];
    if header[0] == 0 {
        return None; // empty block = end of archive
    }
    let name_end = header[0..100].iter().position(|&b| b == 0).unwrap_or(100);
    let name = core::str::from_utf8(&header[0..name_end]).unwrap_or("unknown");
    let name = name.strip_prefix("./").unwrap_or(name); // GNU tar dir prefix
    let size_str = core::str::from_utf8(&header[124..135]).unwrap_or("0");
    let size = usize::from_str_radix(size_str.trim(), 8).unwrap_or(0);
    Some((name, size))
}

/// Iterates the files in a TAR archive.
pub struct TarEntries<'a> {
    archive: &'a [u8],
    offset: usize,
}

impl<'a> Iterator for TarEntries<'a> {
    type Item = TarFile<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.offset + 512 > self.archive.len() {
            return None;
        }
        let (name, size) = parse_header(self.archive, self.offset)?;
        self.offset += 512;
        let data = &self.archive[self.offset..self.offset + size];
        let padding = (512 - (size % 512)) % 512;
        self.offset += size + padding;
        Some(TarFile { name, size, data })
    }
}

/// Parses a raw TAR byte slice and returns a list of files.
pub fn parse_tar(archive: &[u8]) -> Vec<TarFile<'_>> {
    TarEntries { archive, offset: 0 }.collect()
}

pub fn find_vnode(path: &str) -> Option<crate::fs::vfs::VNode> {
    if path == "/" || path == "" {
        return Some(crate::fs::vfs::VNode::Directory {
            name: alloc::string::String::from("/"),
        });
    }
    let search_path = path.strip_prefix("/").unwrap_or(path);
    let initrd = *INITRD.lock();
    let data = initrd?;
    let dir_prefix = alloc::format!("{}/", search_path);
    let mut dir_seen = false;
    for entry in TarEntries { archive: data, offset: 0 } {
        if entry.name == search_path {
            return Some(crate::fs::vfs::VNode::InitrdFile {
                data: entry.data,
                cursor: 0,
            });
        }
        if entry.name.starts_with(&dir_prefix) || entry.name == dir_prefix {
            dir_seen = true;
        }
    }
    if dir_seen {
        Some(crate::fs::vfs::VNode::Directory {
            name: String::from(search_path),
        })
    } else {
        None
    }
}
```

Note: `find_vnode`'s signature stays `-> Option<crate::fs::vfs::VNode>` exactly as today — only the body changes to use `TarEntries`. No placeholder return.

- [ ] **Step 2: Delete `find_initrd_file` from `tar.rs`**

Remove lines 45–69 entirely (dead — no callers). Confirm with `grep -rn "find_initrd_file" kernel/` before deleting; if any caller appears, keep it and return its slice from the matching entry instead.

- [ ] **Step 3: Rewrite shell `ls`/`cat` to use `parse_tar`**

In `kernel/src/shell.rs`, both `"ls"` (line 37) and `"cat"` (line 109) currently iterate a full archive manually. Replace each with a loop over `crate::fs::tar::parse_tar(&data)`:
- `ls`: print each `entry.name`.
- `cat`: find the entry whose `name == target` and print `entry.data` (as bytes/str, same as today).

- [ ] **Step 4: Add a unit test to `kernel/src/lib.rs`**

```rust
#[cfg(test)]
#[test_case]
fn tar_iterator_matches_parse_tar() {
    // Build a minimal in-memory tar: one file "hello" of 5 bytes.
    let mut archive = [0u8; 1024];
    let name = b"./hello";
    archive[0..name.len()].copy_from_slice(name);
    let size = format!("{:011o}", 5); // 11 octal digits, TAR field is bytes 124..135
    archive[124..135].copy_from_slice(size.as_bytes());
    archive[512..517].copy_from_slice(b"hello");
    let files = crate::fs::tar::parse_tar(&archive);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].name, "hello");
    assert_eq!(files[0].size, 5);
    assert_eq!(files[0].data, b"hello");
}
```

- [ ] **Step 5: Verify**

```bash
./scripts/build-kernel.sh
cargo test -p kernel --lib --target x86_64-unknown-none
./scripts/smoke.sh
```

Expected: build clean, `Tests passed!` (includes the new tar test), `SMOKE PASS`.

---

### Task 3: Dead-code removal (zero behavior change)

**Files:**
- Modify: `kernel/src/tasks.rs`
- Modify: `kernel/src/arch/x86_64/syscalls.rs`
- Modify: `kernel/src/shell.rs`
- Modify: `kernel/src/memory/frame.rs`
- Modify: `kernel/src/arch/x86_64/gdt.rs`
- Delete: `kernel/src/interrupts.rs.orig`, `kernel/src/interrupts.rs.rej`

Remove confirmed-dead code. Each item verified by grep before removal.

- [ ] **Step 1: Remove `Process.canary`**

`tasks.rs:109` field; written at `tasks.rs:184`, `syscalls.rs:872`, `syscalls.rs:1037`; read only by the debug tool `print_sizes.rs:5`.
- Remove the field from `Process`.
- Remove `canary: 0xCAFEBABE,` from `register_exec_thread` (tasks.rs:184) and from the `Process { .. }` literals in fork (syscalls.rs:859–873) and execve (syscalls.rs:1024–1038).
- `print_sizes.rs` reads it — update `print_sizes.rs` to drop that line (it's a debug tool; keep the rest).

- [ ] **Step 2: Remove `Process.wait_queue`**

`tasks.rs:106` field; only initialized (tasks.rs:181, syscalls.rs:1021, 1034), never pushed/popped (superseded by `CHILD_WAIT_QUEUE`).
- Remove the field from `Process` and the three `wait_queue: VecDeque::new(),` lines.

- [ ] **Step 3: Remove `CpuContext` struct**

`tasks.rs:78–87`; always `default()`, layout duplicated by raw `frame.add(n)` writes.
- Remove the struct, the `context: CpuContext` field from `Thread`, `context: CpuContext::default()` from `register_exec_thread` (tasks.rs:197), and `CpuContext::default()` from syscalls.rs fork/clone (lines ~819, 899).
- `CpuContext` is imported in syscalls.rs line 34 — remove from the import list.

- [ ] **Step 4: Remove `Shell.buffer` / `SHELL` static**

`shell.rs:19–24` — `buffer` written nowhere, `SHELL` never read. Remove the struct fields that are unused and the `SHELL` static if nothing else references it. Keep the `Shell` struct only if it still holds used state; otherwise delete it.

- [ ] **Step 5: Remove `free_frames`**

`frame.rs:35–41` — never called. Delete the function (keep `BootInfoFrameAllocator`, `init`, `memory_stats`).

- [ ] **Step 6: Remove fstat dead block + empty arm in `syscalls.rs`**

- fstat `vnode_opt` block at syscalls.rs:1388–1406: computed and discarded (the `else` branch recomputes). Delete the dead branch.
- syscalls.rs:1069 `if sig == 9 {}` — delete the empty body (keep the surrounding kill logic).
- syscalls.rs:1326 stale comment "Old sys_wait4 removed…" — delete.
- syscalls.rs:232 commented-out `// crate::println!(...)` — delete.
- syscalls.rs:1375 tautology `if sys_no == 4 { arg2 } else { arg2 }` → `arg2`.

- [ ] **Step 7: Remove leftover debug `println!` in `gdt.rs`**

`gdt.rs:55` `crate::println!("tss_descriptor: TSS address is {:#x}", ptr);` — remove the line (debug leftover).

- [ ] **Step 8: Delete stale merge artifacts**

`rm kernel/src/interrupts.rs.orig kernel/src/interrupts.rs.rej`

- [ ] **Step 9: Verify**

```bash
./scripts/build-kernel.sh
cargo test -p kernel --lib --target x86_64-unknown-none
./scripts/smoke.sh
```

Expected: build clean, `Tests passed!`, `SMOKE PASS`. Then `grep -rn "canary\|wait_queue\|CpuContext\|find_initrd_file\|free_frames" kernel/src/` returns nothing relevant.

---

### Task 4: Split the 1281-line syscall dispatch

**Files:**
- Modify: `kernel/src/arch/x86_64/syscalls.rs`

Mechanical extraction: the giant `match sys_no { ... }` becomes a thin dispatch; each arm's body moves into a `fn sys_<name>(...) -> u64`. Behavior-preserving.

- [ ] **Step 1: Create the per-syscall function stubs**

For each arm in `syscall_rust_handler` (lines 234–1495), extract the body into a top-level `fn` in the same file, taking the syscall args it uses and any registers (rbx/rbp/r12..r15) the body reads. Return the value the arm returned. Name by syscall:

| Arm | Function |
|---|---|
| 0 | `fn sys_read(fd: usize, buf: *mut u8, count: usize) -> u64` |
| 1 | `fn sys_write(fd: usize, buf: *const u8, count: usize) -> u64` |
| 2 | `fn sys_open(path: *const u8, flags: u64, mode: u64) -> u64` |
| 3 | `fn sys_close(fd: usize) -> u64` |
| 4/5 | `fn sys_fstat(sys_no: u64, fd: usize, buf: *mut Stat) -> u64` |
| 7 | `fn sys_poll(fds: *const PollFd, nfds: usize, timeout: i64) -> u64` |
| 8 | `fn sys_lseek(fd: usize, offset: i64, whence: u64) -> u64` |
| 9 | `fn sys_mmap(addr: u64, len: usize, prot: u64, flags: u64, fd: usize, off: u64) -> u64` |
| 10 | `fn sys_mprotect(addr: u64, len: usize, prot: u64) -> u64` |
| 11 | `fn sys_munmap(addr: u64, len: usize) -> u64` |
| 12 | `fn sys_brk(addr: u64) -> u64` |
| 13 | `fn sys_rt_sigaction(sig: i32, act: *const SigAction, oldact: *mut SigAction) -> u64` |
| 14 | `fn sys_rt_sigprocmask(how: u64, set: *const u64, oldset: *mut u64) -> u64` |
| 15 | `fn sys_rt_sigreturn() -> u64` (reads CPU_STATE) |
| 16 | `fn sys_ioctl(fd: usize, request: u64, arg: u64) -> u64` (includes pipe/pipe2; split pipe into `fn sys_pipe(pipefd: *mut i32)` if the arm is a distinct block) |
| 20 | `fn sys_writev(fd: usize, iov: *const Iovec, iovcnt: usize) -> u64` |
| 32 | `fn sys_dup(oldfd: usize) -> u64` |
| 33 | `fn sys_dup2(oldfd: usize, newfd: usize) -> u64` |
| 56 | `fn sys_clone(flags: u64, stack: u64, ...) -> u64` |
| 57 | `fn sys_fork() -> u64` |
| 59 | `fn sys_execve(path: *const u8, argv: *const *const u8, envp: *const *const u8) -> u64` |
| 61 | `fn sys_wait4(pid: i32, status: *mut i32, options: u64) -> u64` |
| 62 | `fn sys_kill(pid: i32, sig: i32) -> u64` |
| 63 | `fn sys_uname(buf: *mut u8) -> u64` |
| 72 | `fn sys_fcntl(fd: usize, cmd: u64, arg: u64) -> u64` |
| 79 | `fn sys_getcwd(buf: *mut u8, size: usize) -> u64` |
| 102–108 | `fn sys_uid_stub() -> u64` (returns 0) |
| 109 | `fn sys_setpgid(...) -> u64` (stub) |
| 158 | `fn sys_arch_prctl(code: u64, addr: u64) -> u64` |
| 186 | `fn sys_gettid() -> u64` |
| 218 | `fn sys_getrandom() -> u64` (returns 1) |
| 234 | `fn sys_exit_group(status: i32) -> u64` (actually `-> !`, calls `exit_current_thread`) |
| default | `fn sys_unknown(n: u64) -> u64` |

Each function body is the arm's code verbatim, with the `return`/final expression moved to the function's return. Keep `handle_sigpipe` as-is (already a helper, line 1499).

- [ ] **Step 2: Replace the match body with dispatch calls**

`syscall_rust_handler` keeps its exact signature (`extern "C"`, all 13 params) but its body becomes:

```rust
match sys_no {
    0 => sys_read(arg1 as usize, arg2 as *mut u8, arg3 as usize),
    1 => sys_write(arg1 as usize, arg2 as *const u8, arg3 as usize),
    2 => sys_open(arg1 as *const u8, arg2, arg3),
    3 => sys_close(arg1 as usize),
    4 | 5 => sys_fstat(sys_no, arg1 as usize, arg2 as *mut Stat),
    7 => sys_poll(arg1 as *const PollFd, arg2 as usize, arg3 as i64),
    8 => sys_lseek(arg1 as usize, arg2 as i64, arg3),
    9 => sys_mmap(arg1, arg2 as usize, arg3, arg4, arg5 as usize, arg6),
    10 => sys_mprotect(arg1, arg2 as usize, arg3),
    11 => sys_munmap(arg1, arg2 as usize),
    12 => sys_brk(arg1),
    13 => sys_rt_sigaction(arg1 as i32, arg2 as *const SigAction, arg3 as *mut SigAction),
    14 => sys_rt_sigprocmask(arg1, arg2 as *const u64, arg3 as *mut u64),
    15 => sys_rt_sigreturn(),
    16 => sys_ioctl(arg1 as usize, arg2, arg3),
    20 => sys_writev(arg1 as usize, arg2 as *const Iovec, arg3 as usize),
    32 => sys_dup(arg1 as usize),
    33 => sys_dup2(arg1 as usize, arg2 as usize),
    56 => sys_clone(arg1, arg2, arg3, arg4),
    57 => sys_fork(),
    59 => sys_execve(arg1 as *const u8, arg2 as *const *const u8, arg3 as *const *const u8),
    61 => sys_wait4(arg1 as i32, arg2 as *mut i32, arg3),
    62 => sys_kill(arg1 as i32, arg2 as i32),
    63 => sys_uname(arg1 as *mut u8),
    72 => sys_fcntl(arg1 as usize, arg2, arg3),
    79 => sys_getcwd(arg1 as *mut u8, arg2 as usize),
    102..=108 => sys_uid_stub(),
    109 => sys_setpgid(arg1 as i32, arg2 as i32),
    158 => sys_arch_prctl(arg1, arg2),
    186 => sys_gettid(),
    218 => sys_getrandom(),
    234 => sys_exit_group(arg1 as i32),
    _ => sys_unknown(sys_no),
}
```

Match the actual arm arguments by reading each arm body before extracting — do not guess. Arms that read `rbx`/`rbp`/`r12`..`r15` (e.g. clone) must receive those as parameters.

- [ ] **Step 3: Verify no arm changed behavior**

Diff-review by eye: for each extracted function, the body must be byte-identical to the original arm except (a) removed `sys_no`/arg casts moved to the call site, (b) `return x;` → tail expression or kept `return`.

- [ ] **Step 4: Verify**

```bash
./scripts/build-kernel.sh
cargo test -p kernel --lib --target x86_64-unknown-none
./scripts/smoke.sh
```

Expected: build clean, `Tests passed!`, `SMOKE PASS`. Boot smoke exercises exec, read, write, clone/fork paths — a regression here is loud.

---

### Task 5: Extract context-switch helper (kill triplication)

**Files:**
- Modify: `kernel/src/tasks.rs`
- Modify: `kernel/src/time.rs`

The CR3+FS_BASE+rsp0+CPU_STATE+return-stack_ptr block is copy-pasted at `tasks.rs:282–299` (inside exit), `time.rs:105–116`, and `time.rs:148–160`. Extract one helper.

- [ ] **Step 1: Add `switch_to_thread` to `kernel/src/tasks.rs`**

```rust
/// Switches CPU state (CR3, FS base, TSS rsp0, syscall kernel stack) to `next`
/// and returns the stack pointer iretq should resume from.
pub fn switch_to_thread(next: &Thread) -> u64 {
    let next_cr3 = next.process.lock().cr3;
    unsafe {
        if crate::arch::x86_64::cpu::read_cr3() != next_cr3 {
            crate::arch::x86_64::cpu::write_cr3(next_cr3);
        }
        crate::arch::x86_64::msr::wrmsr(crate::arch::x86_64::msr::IA32_FS_BASE, next.fs_base);
        crate::arch::x86_64::tss::set_rsp0(next.kernel_stack_top);
        crate::arch::x86_64::syscalls::CPU_STATE.kernel_stack = next.kernel_stack_top;
    }
    next.stack_ptr
}
```

- [ ] **Step 2: Use it in `time.rs`**

Replace both duplicated blocks:
- Lines 104–116 (blocked-path switch): keep the wait-queue push, then `let new_rsp = crate::tasks::switch_to_thread(&next); *current_thread_lock = Some(next); return new_rsp;`
- Lines 148–160 (round-robin switch): `let new_rsp = crate::tasks::switch_to_thread(&next); *current_thread_lock = Some(next); return new_rsp;`

Remove the now-unused imports from `time.rs` if they become unused (`read_cr3`, `write_cr3`, `wrmsr`, `IA32_FS_BASE`, `set_rsp0` — check each).

- [ ] **Step 3: Check `tasks.rs` exit path — likely leave as-is**

`exit_current_thread` (tasks.rs:234–305) has a *different* structure: it switches CR3 at 282–286, then calls `destroy_address_space` at 288–290, then switches FS_BASE/rsp0/CPU_STATE at 295–299, then branches `switch_to_stack_and_iretq` vs `return_to_shell` at 292–304. It also handles the no-next-thread case. **Do NOT force it through `switch_to_thread`** — the interleaved address-space destroy makes it non-identical. Leave this path untouched; the helper covers the two `time.rs` copies which are the actual duplication.

- [ ] **Step 4: Verify**

```bash
./scripts/build-kernel.sh
cargo test -p kernel --lib --target x86_64-unknown-none
./scripts/smoke.sh
```

Expected: build clean, `Tests passed!`, `SMOKE PASS`. Then `grep -n "set_rsp0\|wrmsr(IA32_FS_BASE" kernel/src/time.rs` returns nothing — the only remaining call sites are inside `tasks::switch_to_thread`.

---

### Task 6: Full regression pass

**Files:** none (verification only)

- [ ] **Step 1: Full suite**

```bash
./scripts/build-kernel.sh && cargo test -p kernel --lib --target x86_64-unknown-none && ./scripts/smoke.sh
```

- [ ] **Step 2: Repeat smoke 8 times**

Run `./scripts/smoke.sh` 8× in a row. All must pass (matches the 8/8 clean-boot bar from the corruption fix).

- [ ] **Step 3: Confirm no leftover smell references**

`grep -rn "0x8000_0000_0000_0000\|canary\|find_initrd_file\|timer_rust_handler.*scheduling" kernel/src/` — only expected hits remain (the helper constant is named `USER_ADDR_LIMIT`).

---

## Deferred (explicitly out of scope, revisit later)

- iretq-frame offset constants shared across `build_child_kernel_stack`/`deliver_pending`/`rt_sigreturn` (needs the signal-delivery refactor).
- Extracting a `scheduler` module out of `time.rs`/`tasks.rs` (entangled via `PENDING_BLOCK`; risky, worth its own plan).
- Unifying shell's `exec` (shell.rs:50–107) with syscall 59 (bigger change, touches process teardown).
- `fork_address_space`/`destroy_address_space`/`leaf_entry_mut` → shared 4-level walker.
- Real `getrandom`/`setpgid` implementations (stubs intentionally left).
