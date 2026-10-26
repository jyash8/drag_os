# Phase 1: PRIME DMA-buf Cross-Process Import/Export Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Complete the PRIME DMA-buf path for the virtio-gpu DRM driver: export a dumb/blob BO handle to a real VFS fd, import that fd back into a fresh handle in the same *or a different* process, map the dma-buf fd into the guest address space, and make `DMA_BUF_IOCTL_SYNC` + sync-file fence probes succeed. This unblocks multi-process compositors (DRI3, libgbm buffer passing) and replaces the current single-process-only fd registry.

**Architecture:** The kernel already hands out real VFS fds for PRIME exports (`VNode::DmaBuf`). What is missing is (a) a global resource registry keyed by virtio resource id so a fd can be re-imported in a sibling process, (b) an `mmap` path for `VNode::DmaBuf` that maps the underlying BO physical memory, (c) `DMA_BUF_IOCTL_SYNC` no-ops/validation, and (d) a fence/`SYNC_FILE` minimal surface. All changes are additive to `drm-uapi`, `drm/types.rs`, `drm/mod.rs`, `syscalls/fsio.rs`, and `syscalls/mem.rs`; no ABI pin changes.

**Tech Stack:** Rust `no_std` x86_64-unknown-none kernel; virtio-gpu; C guest probes in `initrd/` compiled with `gcc` against `drm/drm.h`.

**Spec:** `docs/superpowers/plans/2026-10-25-kms-complete-implementation.md` Phase 1, and `docs/deferred-work.md` §4 + §7.

## Global Constraints

- Every new ABI constant is computed via `drm_ioctl_t::<T>` (no hand-written hex literals); add `abi_pin!` entries for any new ioctl number.
- Unimplemented ioctls return `ENOTTY`; `EINVAL` means "garbage payload", never "not supported".
- `DRM_IOCTL_PRIME_HANDLE_TO_FD` = `0xC00C642D`; `DRM_IOCTL_PRIME_FD_TO_HANDLE` = `0xC00C642E`; `DrmPrimeHandle { handle: u32, flags: u32, fd: i32 }` (12 bytes).
- `DMA_BUF_IOCTL_SYNC` = `_IOWR('b', 0, struct dma_buf_sync)` = `0xC0086200`; `struct dma_buf_sync { __u64 flags; }` (8 bytes).
- `DmaBufFile { handle: u32, res_id: Option<u32>, size: u64, is_blob: bool }` is the payload of `VNode::DmaBuf`.
- The PRIME fd must survive `fcntl(F_DUPFD_CLOEXEC)` and duplication across fork (VFS `OpenFile` in `fd_table`).
- Do not change existing lock ordering or interrupt masking; `GLOBAL_DRM` is a `SpinLock` held briefly.

## Review Focus

- A PRIME fd duplicated into a child process must re-import correctly: the fd number in the child differs; lookup must resolve by the fd's `OpenFile` vnode, not by a global fd→handle table assuming the same fd number.
- `mmap(fd, len, prot, MAP_SHARED, …)` on a `DmaBuf` fd must succeed and yield a guest-accessible mapping of the BO, not `EINVAL`.
- `DMA_BUF_IOCTL_SYNC` with `DMA_BUF_SYNC_RW | START/END` must return 0 for a valid 8-byte payload and `EINVAL` for flags outside the defined mask; an unrelated ioctl on the fd must return `ENOTTY`.
- Importing a fd that is not a `DmaBuf` vnode returns `EINVAL`; a fd number past the table returns `ENOENT`.
- After export→import, the imported handle must reference the same BO memory (same `phys`), verified by a readback pattern test.

---

### Task 1: Add DMA-buf UAPI constants and `dma_buf_sync` struct

**Files:**
- Modify: `crates/drm-uapi/src/lib.rs` (add before `abi_pin!` block)

**Interfaces:**
- Consumes: existing `drm_ioctl_t<..>` macro.
- Produces: `DMA_BUF_IOCTL_SYNC: u64`, `DMA_BUF_SYNC_READ/WRITE/RW/START/END: u32`, `DmaBufSync { flags: u64 }` (repr C, 8 bytes). `DMA_BUF_SYNC_RW = READ | WRITE`.

- [ ] **Step 1: Write failing unit tests in `abi_regression_tests`**

```rust
#[test]
fn dma_buf_sync_is_8_bytes() {
    assert_eq!(core::mem::size_of::<DmaBufSync>(), 8);
    assert_eq!(core::mem::align_of::<DmaBufSync>(), 8);
}

#[test]
fn dma_buf_sync_ioctl_number_is_upstream() {
    assert_eq!(DMA_BUF_IOCTL_SYNC, 0xC0086200);
    assert_eq!(DMA_BUF_SYNC_RW, DMA_BUF_SYNC_READ | DMA_BUF_SYNC_WRITE);
}
```

- [ ] **Step 2: Run `cargo test -p drm-uapi`** — expect FAIL (constants undefined).

- [ ] **Step 3: Add the struct and constants to `crates/drm-uapi/src/lib.rs`**

```rust
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DmaBufSync {
    pub flags: u64,
}
pub const DMA_BUF_SYNC_READ: u32 = 1 << 0;
pub const DMA_BUF_SYNC_WRITE: u32 = 1 << 1;
pub const DMA_BUF_SYNC_RW: u32 = DMA_BUF_SYNC_READ | DMA_BUF_SYNC_WRITE;
pub const DMA_BUF_SYNC_START: u32 = 1 << 2;
pub const DMA_BUF_SYNC_END: u32 = 1 << 3;
pub const DMA_BUF_IOCTL_SYNC: u64 = ioc(IOC_RW, b'b', 0x00, core::mem::size_of::<DmaBufSync>() as u32); // _IOWR('b', 0, struct dma_buf_sync)
```

- [ ] **Step 4: Run `cargo test -p drm-uapi`** — expect PASS.

- [ ] **Step 5: Commit** `git commit -am "feat(drm-uapi): add DMA_BUF_IOCTL_SYNC UAPI"`

---

### Task 2: Dispatch `DMA_BUF_IOCTL_SYNC` on `VNode::DmaBuf`

**Files:**
- Modify: `crates/kernel/src/arch/x86_64/syscalls/fsio.rs` (in the `sys_ioctl` match, the `VNode::DmaBuf(_)` arm that the previous session added as a raw mask check).

**Interfaces:**
- Consumes: `DMA_BUF_IOCTL_SYNC`, `DmaBufSync`, `DMA_BUF_SYNC_*` from `drm-uapi`; `validate_user_read_range(argp, 8)`.
- Produces: correct errno behavior for sync ioctls on dma-buf fds.

- [ ] **Step 1: Write a failing guest-side probe expectation** (this task's deliverable is kernel errno behavior; test is part of Task 5, but the code contract is):

```c
// expects: sync ioctl returns 0, unknown 0xdead returns -1 errno=ENOTTY
```

- [ ] **Step 2: Replace the placeholder arm in `fsio.rs`**

Current (placeholder):
```rust
crate::fs::vfs::VNode::DmaBuf(_) => {
    drop(open_file);
    if (cmd & 0xffffff00) == 0xc0086200 & 0xffffff00 {
        return 0;
    }
    return super::ENOTTY;
}
```

Replace with exact-match + payload validation:
```rust
crate::fs::vfs::VNode::DmaBuf(_) => {
    drop(open_file);
    if cmd == crate::drm_uapi::DMA_BUF_IOCTL_SYNC {
        if !validate_user_read_range(argp, 8) {
            return EFAULT;
        }
        let flags = unsafe { core::ptr::read_unaligned(argp as *const u64) };
        const SYNC_MASK: u64 = (DMA_BUF_SYNC_RW | DMA_BUF_SYNC_START | DMA_BUF_SYNC_END) as u64;
        if flags & !SYNC_MASK != 0 {
            return EINVAL;
        }
        return 0; // memory is shared/coherent; sync is a no-op
    }
    return super::ENOTTY;
}
```

The `DMA_BUF_IOCTL_SYNC` constant must be imported into scope at the top of `fsio.rs` (via `use crate::drm_uapi::{DMA_BUF_IOCTL_SYNC, DMA_BUF_SYNC_*};` or fully-qualified paths matching the file's existing import style).

- [ ] **Step 3: Build kernel** — `cargo run -p tools -- build-kernel` must succeed.

- [ ] **Step 4: Commit** `git commit -am "feat(kernel): DMA_BUF_IOCTL_SYNC dispatch on dma-buf fds"`

---

### Task 3: mmap support for `VNode::DmaBuf` (BO physical mapping)

**Files:**
- Modify: `crates/kernel/src/arch/x86_64/syscalls/mod.rs` (syscall 9 dispatch, the `else` branch that currently resolves `prime_phys` from `GLOBAL_DRM.prime_handles`).

**Interfaces:**
- Consumes: `VNode::DmaBuf(DmaBufFile)`; `DrmDevice::find_dumb(handle) -> Option<&DumbBuffer>` (`phys`, `size`); `mmap_device(phys, len, prot, flags)`.
- Produces: a mmap path that resolves the fd's vnode directly, so it works for **any** fd number in any process (not just the exporter's number).

- [ ] **Step 1: Write a failing guest expectation** — `mmap` on an imported PRIME fd returns a non-`MAP_FAILED` address in a child process (probe in Task 5 verifies).

- [ ] **Step 2: Change the mmap fd-resolution logic** in `syscalls/mod.rs`. In the `else` branch (non-device, non-ring), **before** falling back to `filemmap::mmap_file`, look up the fd's `OpenFile` vnode:

```rust
let is_dma_buf = {
    let t = crate::tasks::current_thread();
    if let Some(t) = t {
        let p = t.process.lock();
        p.fd_table.get(fd as usize).cloned().flatten().map(|of| {
            matches!(of.lock().vnode, crate::fs::vfs::VNode::DmaBuf(_))
        }).unwrap_or(false)
    } else { false }
};
if is_dma_buf {
    let drm = crate::drivers::drm::GLOBAL_DRM.lock();
    let maybe = drm.as_ref().and_then(|dev| {
        let t = crate::tasks::current_thread()?;
        let p = t.process.lock();
        let of = p.fd_table.get(fd as usize)?.clone();
        drop(p);
        let ofg = of.lock();
        match &ofg.vnode {
            crate::fs::vfs::VNode::DmaBuf(db) => {
                let d = db.lock();
                dev.find_dumb(d.handle).map(|b| (b.phys.0, b.size))
            }
            _ => None,
        }
    });
    if let Some((phys, size)) = maybe {
        let len = if arg2 == 0 { size as usize } else { arg2 as usize };
        return mmap_device(phys, len, arg3, arg4 | MAP_DEVICE);
    }
}
```

Keep the existing `prime_phys` table lookup as fallback, then `mmap_file`.

- [ ] **Step 3: Build kernel** — must succeed.

- [ ] **Step 4: Commit** `git commit -am "feat(kernel): mmap PRIME dma-buf fds via vnode resolution"`

---

### Task 4: Cross-process import — make `handle_prime_fd_to_handle` robust to fd renumbering

**Files:**
- Modify: `crates/kernel/src/drivers/drm/mod.rs` (`handle_prime_fd_to_handle`, line ~4042).

**Interfaces:**
- Consumes: existing VFS fd table lookup.
- Produces: import resolves the fd's `DmaBufFile` by vnode (already does) and returns its `handle`; add the `prime_handles` registry **on import too**, keyed by the *new* fd number → handle, so subsequent mmap in the importing process works even without the Task 3 vnode path for the old registry.

- [ ] **Step 1: Guest expectation** — child re-importing the inherited fd gets a handle whose bytes match the parent's write pattern.

- [ ] **Step 2: In `handle_prime_fd_to_handle`, after computing `handle`, register the new fd mapping:**

```rust
GLOBAL_DRM.lock().as_mut().map(|d| {
    d.prime_handles.insert(fd as u32, handle);
});
```

(right before `req.handle = handle;`). This makes mmap-by-table also work in the importer.

- [ ] **Step 3: Build kernel** — must succeed.

- [ ] **Step 4: Commit** `git commit -am "feat(kernel): register PRIME fd on import for mmap"`

---

### Task 5: Guest probe + test-harness wiring

**Files:**
- Create: `initrd/primeprobe.c`
- Modify: `scripts/build-fs.sh` (compile + stage `primeprobe`), `crates/tools/src/test_runner.rs` (add `primeprobe` test case).

**Interfaces:**
- Consumes: `DRM_IOCTL_PRIME_HANDLE_TO_FD`, `DRM_IOCTL_PRIME_FD_TO_HANDLE`, `mmap` syscall.
- Produces: `PRIME_PROBE_PASS` marker on serial.

- [ ] **Step 1: Write `initrd/primeprobe.c`**

```c
#include <stdio.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <drm/drm.h>
#include <drm/drm_mode.h>

int main(void) {
    int fd = open("/dev/dri/card0", O_RDWR);
    if (fd < 0) { printf("PRIME_FAIL open errno=%d\n", errno); return 1; }

    // create dumb 64x64
    struct drm_mode_create_dumb c = {0};
    c.width = 64; c.height = 64; c.bpp = 32;
    if (ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &c) != 0) {
        printf("PRIME_FAIL create_dumb errno=%d\n", errno); return 2; }

    // map dumb
    struct drm_mode_map_dumb m = {0};
    m.handle = c.handle;
    if (ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &m) != 0) {
        printf("PRIME_FAIL map_dumb errno=%d\n", errno); return 3; }
    struct drm_mode_destroy_dumb d = {0};
    d.handle = c.handle;

    // export
    struct drm_prime_handle ph = {0};
    ph.handle = c.handle; ph.flags = DRM_CLOEXEC;
    if (ioctl(fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &ph) != 0 || ph.fd < 0) {
        printf("PRIME_FAIL export errno=%d\n", errno); return 4; }

    // mmap the dma-buf fd and scribble a pattern
    unsigned char *p = mmap(NULL, 64*64*4, PROT_READ|PROT_WRITE, MAP_SHARED, ph.fd, 0);
    if (p == MAP_FAILED) {
        printf("PRIME_FAIL mmap errno=%d\n", errno); return 5; }
    for (int i = 0; i < 64*64*4; i++) p[i] = (unsigned char)(i & 0xff);
    munmap(p, 64*64*4);

    // fork child, re-import inherited fd
    int st = 0;
    pid_t pid = fork();
    if (pid == 0) {
        struct drm_prime_handle ph2 = {0};
        ph2.fd = ph.fd;
        if (ioctl(fd, DRM_IOCTL_PRIME_FD_TO_HANDLE, &ph2) != 0) {
            printf("PRIME_FAIL child import errno=%d\n", errno); _exit(6); }
        unsigned char *q = mmap(NULL, 64*64*4, PROT_READ, MAP_SHARED, ph.fd, 0);
        if (q == MAP_FAILED) {
            printf("PRIME_FAIL child mmap errno=%d\n", errno); _exit(7); }
        int ok = 1;
        for (int i = 0; i < 64*64*4; i++) if (q[i] != (unsigned char)(i & 0xff)) { ok = 0; break; }
        munmap(q, 64*64*4);
        printf(ok ? "PRIME_CHILD_READBACK_OK\n" : "PRIME_CHILD_READBACK_BAD\n");
        _exit(ok ? 0 : 8);
    }
    waitpid(pid, &st, 0);
    close(ph.fd);
    ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &d);
    close(fd);

    if (WIFEXITED(st) && WEXITSTATUS(st) == 0) {
        printf("PRIME_PROBE_PASS\n"); return 0; }
    printf("PRIME_FAIL child status=%d\n", st); return 9;
}
```

- [ ] **Step 2: Add compile+stage to `scripts/build-fs.sh`** following the `fixed_size_test` template (compile with `gcc -O2 -o "$STAGING/bin/primeprobe" "$ROOT/initrd/primeprobe.c"` and `stage_bin_with_libs`).

- [ ] **Step 3: Add test case to `crates/tools/src/test_runner.rs`**

```rust
TestCase {
    name: "primeprobe",
    command: "/usr/bin/primeprobe; echo PRIME_RC=$?",
    expected: &["PRIME_PROBE_PASS", "PRIME_RC=0"],
    timeout_secs: 30,
    gl_only: false,
},
```

- [ ] **Step 4: Build fs + run test** — `cargo run -p tools -- build-fs`, then `cargo run -p tools -- test primeprobe` (expect PASS).

- [ ] **Step 5: Commit** `git commit -am "test: PRIME dma-buf cross-process probe"`

---

### Task 6: Run full suite & verify no regressions

**Files:**
- None (verification only).

- [ ] **Step 1: Run `cargo run -p tools -- test`** — confirm baseline ~20/22 still passing and no new failures beyond the known serial flake set.

- [ ] **Step 2: Run `cargo run -p tools -- run --smp 2`** for 90s — confirm zero panics and desktop still reaches mywima main loop.

- [ ] **Step 3: Confirm `git status`** shows only the intended files.
