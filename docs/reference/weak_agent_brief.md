# DragOS Task Brief — Weaker Agent (Wayland Kernel Prep + Compositor Skeleton)

You are working on **DragOS**, a hobby x86_64 kernel written in Rust (no_std).
It runs in QEMU/KVM, boots via OVMF, loads ELF programs from a TAR initrd, and
has a working DRM/KMS layer (doom renders through it). You are implementing
**five self-contained tasks** toward running a Wayland compositor.

Read this whole document before writing any code. Follow the patterns exactly.

---

## 0. Environment & Workflow

**Build + deploy the kernel** (do this after every kernel change):
```bash
cargo b --release -p kernel --target x86_64-unknown-none \
  && cp target/x86_64-unknown-none/release/kernel esp/efi/boot/kernel.elf
```

**Run a one-off test program** (boots QEMU, waits for the `/ #` shell prompt,
runs your command, checks expected output):
```bash
python3 scripts/qemu_test.py "/epoll_test" "EPOLL TEST PASSED"
```

**Debug output**: kernel `dprintln!` goes to `uefi_debug.log` (truncate with
`: > uefi_debug.log` before each run). Userspace `printf` goes to serial
(stdout of QEMU).

**Test programs** are C files in `initrd/`. Compile them with the musl
toolchain so they're statically linked:
```bash
x86_64-linux-musl-native/bin/musl-gcc -static -o initrd/epoll_test initrd/epoll_test.c
```
The initrd directory is loaded as-is at boot; a binary at `initrd/epoll_test`
appears as `/epoll_test` in the guest.

**Full regression suite** (must stay green — run before declaring anything done):
```bash
for t in fork_test clone_test sig_test zombie_reaping fork_sleep_wait test_musl; do
  timeout 60 python3 scripts/qemu_test1.py "$t" 1 2>&1 | grep -E "\[PASS\]|\[FAIL\]"
done
```
All six must print `[PASS]`.

---

## 1. Hard Rules (violating these breaks the kernel in silent ways)

1. **NEVER edit these files** (the senior agent owns them):
   - `kernel/src/arch/x86_64/smp.rs`
   - `kernel/src/tasks.rs` (read-only: you may CALL its functions, not modify)
   - `kernel/src/drivers/drm/mod.rs` and `drm/kms.rs`
   - `kernel/src/drivers/virtio/*`
   - `kernel/src/arch/x86_64/interrupts.rs`, `time.rs`, `idt.rs`
   - `AGENTS.md`

2. **Spinlocks are non-reentrant.** Never call `current_thread()` while
   holding `CPUS[id]` — it deadlocks. If you need thread state, get it first,
   then lock.

3. **Every blocking wait must go through an existing wait-queue pattern.**
   Copy `sys_poll` in `fsio.rs` — do not invent new sleeping mechanisms.
   Never spin-wait in a syscall.

4. **Validate every userspace pointer** with `validate_user_write_range` /
   `validate_user_read_range` before dereferencing. Return `EFAULT` (-14) on
   failure. This is a security boundary, not a formality.

5. **Syscall return convention**: return `u64`. Success = 0 or the value.
   Error = negated errno (e.g. `EINVAL` = 22 as `-(-22)`... check how existing
   syscalls do it — `grep EINVAL kernel/src/arch/x86_64/syscalls/fsio.rs | head`
   and match the convention exactly).

6. If you must break a rule, STOP and report back instead. Do not improvise
   around lock ordering or pointer validation.

---

## 2. Where Your Code Goes

- Syscall implementations: **new file** `kernel/src/arch/x86_64/syscalls/epoll.rs`
  (eventfd/timerfd may live in the same file).
- Dispatch: add arms to the `match` in
  `kernel/src/arch/x86_64/syscalls/mod.rs` (the big `match n` around line 374).
  Add `mod epoll;` + `use` at the top following the existing import style.
- Test programs: `initrd/*.c` + compiled binaries in `initrd/`.

---

## 3. The Tasks

### W1 — epoll (syscall numbers 285, 286, 287)

```
285 epoll_create1(flags: i32) -> fd          // flags: only EPOLL_CLOEXEC=0x80000, may ignore
286 epoll_ctl(epfd: i32, op: i32, fd: i32, event: *mut EpollEvent) -> 0
287 epoll_wait(epfd: i32, events: *mut EpollEvent, maxevents: i32, timeout: i32) -> count
```

Linux UAPI struct (must match byte-for-byte):
```c
struct epoll_event { uint32_t events; uint64_t data; } // packed, 12 bytes
```
```rust
#[repr(packed)]
pub struct EpollEvent { pub events: u32, pub data: u64 }
```

Event bits (i32): `EPOLLIN=0x1, EPOLLOUT=0x4, EPOLLERR=0x8, EPOLLHUP=0x10,
EPOLLET=0x80000000` (edge-triggered: you may treat everything as level-
triggered and ignore EPOLLET — document this with a `// ponytail:` comment).

Semantics:
- Kernel keeps a per-epoll-instance interest list: `Vec<(fd, EpollEvent)>`.
  Store it in a new global registry keyed by fd (follow how the pipe code
  tracks pipe instances — read `sys_pipe` and its wait queue in `fsio.rs` first).
- `epoll_wait` polls readiness **once**; if nothing ready and timeout != 0,
  block using the same loop-and-yield pattern `sys_poll` uses
  (`fsio.rs:185` — copy it). timeout = -1 → block forever; 0 → return 0 immediately.
- Readiness sources for v1: **pipes only** (readable = pipe has data,
  writable = pipe has space). Other fd types: return EPOLLERR.
  The fd table is `crate::tasks` — find how `sys_poll` classifies fds and
  reuse its helpers.

**Acceptance test** `initrd/epoll_test.c`:
```c
// pipe(); fork(); child writes "x" after 100ms usleep;
// parent epoll_create1, epoll_ctl ADD read-end with EPOLLIN,
// epoll_wait(timeout=2000) must return 1 with EPOLLIN and data=="x";
// second epoll_wait(timeout=0) on empty pipe must return 0.
// print "EPOLL TEST PASSED" only if all checks pass.
```
Run: `python3 scripts/qemu_test.py "/epoll_test" "EPOLL TEST PASSED"`

### W2 — eventfd (syscall 284)

```
284 eventfd(initval: u32, flags: i32) -> fd
```
Semantics: kernel-side u64 counter per fd. `write(u64)` adds to counter
(EAGAIN if would overflow u64 — just return EINVAL for v1). `read()` returns
current counter as 8 bytes and resets to 0; if counter is 0, block (same
pattern as sys_poll's stdin wait) or return EAGAIN if O_NONBLOCK.
`poll()` on it: EPOLLIN when counter > 0, EPOLLOUT always.

**Acceptance test** `initrd/eventfd_test.c`: eventfd(0,0) → write(5) →
read must give 5 → write(1) → poll must show POLLIN → read gives 1 →
"EVENTFD TEST PASSED".

### W3 — timerfd (syscalls 288, 289)

```
288 timerfd_create(clockid: i32, flags: i32) -> fd     // only CLOCK_MONOTONIC=1 needed
289 timerfd_settime(fd: i32, flags: i32, new: *const Itimerspec, old: *mut Itimerspec) -> 0
```
```c
struct itimerspec { struct timespec it_interval; struct timespec it_value; }
struct timespec { int64_t tv_sec; int64_t tv_nsec; }
```
Semantics: one-shot or periodic timer. When `it_value` expires, the fd is
readable; `read()` returns u64 expiration count and resets it. If
`it_interval` is nonzero, re-arm after each expiry. Use the kernel's
monotonic time source — find what `sys_clock_gettime` (mod.rs:490) reads
and reuse it. Expiry check happens in the same poll-loop style as W1
(compute deadline, sleep/yield until passed — do NOT add timer interrupt
hooks).

**Acceptance test** `initrd/timerfd_test.c`: arm 100ms one-shot → read
returns 1 → arm 50ms periodic, read twice with 120ms gap → second read
returns 2 → "TIMERFD TEST PASSED".

### W4 — Compositor skeleton (userspace only, no kernel changes)

New dir `initrd/wm/`. A C program that:
1. `open("/dev/dri/card0", O_RDWR)`
2. `DRM_IOCTL_MODE_CREATE_DUMB` 1280x800 bpp32 (copy struct definitions and
   the full flow from `initrd/drm_test.c` and `initrd/flip_test.c` — read
   both first)
3. `DRM_IOCTL_MODE_ADDFB` then `DRM_IOCTL_MODE_SETCRTC`
4. `mmap` the dumb buffer (copy the mmap call from `flip_test.c`)
5. Animation loop: clear to dark gray, draw a 100x100 red square bouncing
   off edges (update x/y each frame), ~30fps using `usleep(33000)`
6. Handle SIGINT to exit cleanly

**Acceptance**: run `/wm` in the guest, then capture a screenshot from the
host while it runs:
```bash
# while /wm is running, in another terminal, attach to QEMU monitor via:
# (ask the senior agent for the screendump procedure if stuck)
```
The screenshot must show the gray screen with the red square. Two
screenshots 2s apart must show the square at different positions.

### W5 — Wayland wire protocol codec (pure C, host-testable, NO kernel)

New dir `initrd/wm/wire/` (or a standalone `wayland-wire/` at repo root if
the senior agent prefers). Implements:
- `wl_message` header parse/serialize: object-id (u32), opcode (u16 upper
  bits of second word), size (u16 low bits)
- Argument marshal/unmarshal for the 8 Wayland arg types: int, uint, fixed
  (24.8), string (len+data+NUL, aligned 4), object, new_id, array, fd
- Connection buffer: accumulate outgoing messages, flush; parse incoming
  byte stream into complete messages
- The `wl_display` handshake: `get_registry`, `bind`, and the initial
  `wl_registry.global` event decode

This must compile and unit-test **on the host** (no cross-compiling):
```
wayland-wire/$ make test   # builds with host gcc/cc, runs assertions, exits 0
```
Write ~10 test cases: round-trip each arg type, a full synthetic
`wl_registry.bind` message, truncated-message handling.

**Acceptance**: `make test` prints "WIRE TESTS PASSED" and exits 0 on the
host. No QEMU involved.

---

## 4. Order of Work

1. W5 first (zero risk, host-only)
2. W2 eventfd (simplest kernel task — learn the syscall pattern)
3. W1 epoll (builds on W2's registry pattern)
4. W3 timerfd
5. W4 compositor skeleton

After EACH task: run the full regression suite (§0). If anything regresses,
`git diff` your last change and fix or revert before continuing.

---

## 5. Reporting

When a task is done, report: files changed, the exact test command run, its
verbatim output, and anything you had to assume. If stuck >30 minutes on one
error, write down what you tried and escalate to the senior agent — do not
speculatively refactor shared code to "fix" it.
