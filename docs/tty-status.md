# TTY / terminal implementation status

Written while completing the terminal work, so the state is recoverable without
re-deriving it from the tree.

## Done and verified

Boot-time self-checks, all three green on every boot:

```
[vt] selftest OK
[vt] render selftest OK
[tty] selftest OK
```

### Per-VT input + N_TTY line discipline (`crates/kernel/src/tty/mod.rs`)

Replaced the single global `interrupts::STDIN_BUFFER` with one `Tty` per VT:
own input queue, canonical line buffer, termios, winsize, fg pgrp. Keystrokes
route to the *active* VT, so VT 2 keeps its half-typed line while VT 1 is on
screen.

Implemented: canonical editing (`VERASE`, `VKILL`, `VWERASE`, `VREPRINT`,
`VLNEXT`), echo with correct `ECHOE`/`ECHOK`, signal chars (`VINTR`→SIGINT,
`VQUIT`→SIGQUIT, `VSUSP`→SIGTSTP) with pending-input flush, `VEOF` semantics
(both the empty-line EOF and the submit-partial-line cases), `IXON`/`IXOFF`,
`ICRNL`/`IGNCR`/`INLCR`, non-canonical passthrough, and `OPOST`/`ONLCR` output
processing as per-tty state.

Wired into: the PS/2 keyboard ISR, the COM1 ISR, the xHCI `push_char`, the
`TtyDevice` read/write paths, `TCGETS`/`TCSETS`/`TIOCGWINSZ`/`TIOCSWINSZ`/
`TIOCGPGRP`/`TIOCSPGRP` for `TtyDevice`, the `smp.rs` park-time re-check, and
epoll readiness.

### The two rendering bugs

**Cursor not cleared before newline.** The cursor is an opaque block in the
*framebuffer*, not a cell in `cells`, so a row the cursor vacates is only clean
if repainted. `newline()` marked only the destination row. Fixed by
`Vt::move_cursor`, which marks both rows; every cursor mutator now routes
through it. `carriage_return`, `tab`, `put_char`'s deferred wrap, and the CSI
handlers `G`/`C`/`D` (which marked nothing at all) had the same defect.

**Previous screen visible.** `CSI 2 J` blanked every cell, so nothing was dirty
and the dirty-range render drew nothing. Modes 2 and 3 now request a full
repaint. This is the sequence every full-screen program sends on entry and exit.

### Missing syscalls, each found by measuring rather than guessing

zellij died with `ENOSYS` in three successive places, each masking the next:

| syscall | nr | symptom | fix |
|---|---|---|---|
| `mkdirat` | 258 | not dispatched | added; `mkdir` now delegates to it |
| `fchmodat` | 268 | not dispatched | added |
| `umask` | 95 | not dispatched | added, with a per-process field |

Two subtleties worth keeping:

- **`umask` returns the *previous* mask**, so it cannot be a stub returning 0.
  ENOSYS made the caller read `-38` as the old mask, which `nix` rejects —
  hence `[BUG] umask returned invalid Mode`. It needed a real `Process.umask`
  and the `set_current_thread` write-back (invariant 5).
- **tmpfs had no `chmod` at all**, so it inherited `FileNode`'s `Err(ENOSYS)`
  default and `stat` reported a hardcoded `0777`. Added `mode`/`uid`/`gid` to
  `TmpInner` so a mode a program writes reads back. Confirmed in the field:
  zellij's temp dir reports `0o40700` after setting 0700.

`mkdirat` and `fchmodat` were real gaps regardless; the umask one was the only
actual blocker at each stage.

### Nerd font

Confirmed in use, not merely present. `FontKind::Noto20` is the default, and the
generated tables hold 7905 glyphs per size including 3714 Nerd Font PUA icons.
Spot-checked `U+E0B0` (branch), `U+E282` (folder), `U+E706` (lock) as non-blank.
The build's filter (`0xe000..=0xf8ff`) covers the font's full PUA range
`0xe000..=0xf533`.

## zellij: starts, but does not stay up

`zellij --version` works. Launched on VT 2 it reaches, from its own log:

```
Starting Zellij client!
Starting Zellij server!
FirstClientConnected: initializing session
Loading plugins using Wasmi interpreter
Wasm main thread starts
FirstClientConnected: session initialized, spawning tabs
```

Then it runs cleanly for ~2.5 minutes and the client disappears:

```
ERROR |zellij_server::screen | screen.rs:9496: No client ids in screen found
ERROR |zellij_server::screen | screen.rs:8565: No client ids in screen found
   ...repeating
```

No kernel panic, no page fault, no signal. The server outlives its client.

## Threads: root cause of zellij dying, now fixed

zellij's client vanished ~2.5 minutes after startup while its server kept
running and logged `No client ids in screen found` in a loop.

`Process.threads_count` was a bare counter with no per-process thread list, so
`exit_thread_internal` had no way to find sibling threads:

- A thread calling `exit_group` marked the process a zombie and dropped the
  count to zero while its siblings **kept running**. The process was over from
  the group's point of view; its threads were still scheduled and holding
  resources. That is precisely how the server outlived its client.
- The list-less path also meant a plain thread exit could never tell whether
  siblings remained.

Fixed by adding `Process::threads: BTreeMap<u64, Arc<Thread>>` (tid → thread):

- Registered in `sys_clone`'s `CLONE_THREAD` path **before** the thread is
  pushed onto the scheduler, so the invariant "a runnable thread is always in
  `Process::threads`" holds. A thread that was queued but unregistered could be
  asked to exit, find no entry, and leave the count permanently high.
- Registered for the initial thread in `register_exec_thread`, so it holds from
  the very first thread rather than only for cloned ones.
- `exit_thread_internal` removes the dying thread from the list on **every**
  exit path, not just group exit — the list was previously never pruned.
- `exit_group` now collects its siblings, and after dropping `process.lock()`
  (invariant 1: `process.lock()` is innermost, so no SCHEDULER under it) removes
  them from the run queue via `SCHEDULER.retain`.

Result: zellij reaches session init with tabs spawned and **no `ERROR` lines**,
and stays up past the point where it previously died.

### A note on the diagnosis

I first read QEMU's `FIN_WAIT_1` entries as a pre-existing socket issue. They
were not — they were leftovers from my own probes against the broken build. The
correct reading was that the thread-list change was the cause of a *different*
symptom, and that a clean baseline boot was needed to tell them apart. Worth
stating because I asserted from the socket state rather than measuring a
baseline first.

## Rendering: the diagnostic that found two real bugs

`Ctrl+Alt+F12` dumps a VT's cell buffer to the debug log
(`vt::debug_dump`). Added because "did the full-screen program draw anything?"
had no other answer headlessly: `screendump` reports no surface for a virgl
scanout, and a full-screen program renders into VT cells and nowhere else.

Note the keycode: `KEY_F12` is **88**, not 87. In the evdev table
`KEY_F1..KEY_F10` are 59..68 and then the numbering jumps, so `KEY_F11` is 87.
Binding to 87 makes the dump appear to do nothing.

The first dump showed a kernel panic that had been invisible until then:

```
SAME-CPU REENTRY on 0xffff9000012ad590 caller=crates/kernel/src/arch/x86_64/syscalls/epoll.rs:614
```

### Bug 1: epoll deadlock (`epoll.rs`)

```rust
for (&target_fd, &interest) in epoll_arc.lock().interest.iter() { ... }
```

A temporary in a `for` iterator expression lives for the whole loop body, so
the guard stayed held and `check_fd_readiness` ran with the epoll lock held.
Any fd whose readiness probe re-enters epoll locked it again on the same CPU —
a hard deadlock, reproducible whenever a process epolls something whose
readiness check touches its own epoll set. Fixed by snapshotting the interest
map into a `Vec` and dropping the lock before the loop.

### Bug 2: tty writes discarded in graphics mode (`fs/vfs.rs`)

`TtyDevice` writes were wrapped in a `CONSOLE_KD_MODE == 0` check. The
reasoning was that a compositor owns the framebuffer in graphics mode — but a
VT's cell buffer *is not* the framebuffer. In graphics mode the VT simply is
not painted to the screen; it still exists and still accumulates content.
Dropping the writes silently was worse than doing nothing: `write` returned
`buf.len()`, so the client believed it had drawn. zellij started, initialised a
session, spawned tabs, and rendered every frame into a buffer nobody could
read.

Also removed a `print!` that duplicated every tty byte into the serial/debugcon
stream alongside `tprint!`. That duplication is what made a VT dump look like
it contained kernel `[kms]`/`[input]` lines — the dump was right, the two sinks
were just being fed the same stream.

## Why zellij's screen still cannot be read cleanly

Both zellij's client and mywima open `/dev/tty`, and `TtyDevice { vt: 0 }`
resolves to the **active** VT on every access — correct Linux behaviour for
the `/dev/tty` and `/dev/tty0` aliases, and the reason a program that opened
`/dev/tty0` follows the user across a VT switch.

The consequence here: the compositor's stdout and zellij's UI land in the same
cell buffer, so a dump of VT2 shows the UI interleaved with `[kms]` and
`[input]` lines. Not a kernel defect, but it means there is no clean oracle
until one of them uses its own VT.

Separately, `/dev/ptmx` was never opened in those runs — zellij's panes got no
PTY, so there was no pane content and no shell inside a pane.

### Why: the POSIX pty API was undispatched

`ptyrun` worked (it opens `/dev/ptmx` directly) while `openpty(3)` did not.
glibc routes `openpty` through syscalls `622`/`624`/`625`/`626`, none of which
were dispatched, so the calls returned ENOSYS and zellij stalled silently at
"session initialized, spawning tabs" — no panic, no error, no panes.

Implemented `posix_openpt` (622), `grantpt` (624), `unlockpt` (625) and
`ptsname_r` (626), with `pty::register_master_fd` installing the new master into
the fd table the same way `open_path` does for `/dev/ptmx`. Verified with
`initrd/ptyposixtest.c`, which walks the real libc sequence:

```
posix_openpt() = 3   grantpt() = 0   unlockpt() = 0
ptsname_r() = '/dev/pts/1'      ptsname() = '/dev/pts/1'
open(slave) = 4      master->slave = 20 'posix-pty-roundtrip\n'
```

Two things that cost time and are worth recording:

- **`ptsname_r` does not reach syscall 626.** glibc composes the path itself
  from `ioctl(TIOCGPTN)`, which was already implemented. The 626 arm is kept for
  a direct caller but is not the route a glibc program takes. The first probe
  reported failure because it read `ptsname_r`'s return value as a length; on
  glibc that call returns 0 on success and writes the name into the buffer. The
  probe was wrong, not the kernel.
- **A missing syscall is invisible if nothing logs the attempt.** No `open`
  failure appears because the call never reached `open`. The evidence was the
  *absence* of `/dev/ptmx` in the log, which is a much weaker signal than an
  error and easy to talk past.

## Job control (done)

`Process` gained `pgid`, `sid`, `ctty_vt` and `ctty_owner_sid`. None of this
existed: `setpgid` returned 0 and changed nothing, `setsid` returned the pid
without creating a session, `getpgid` returned `p.pid` (the process's own id,
not its group leader's — right only when the caller happened to lead its own
group, which is the common case and why nobody noticed), `getpgrp` was not
dispatched, and `TIOCSCTTY`/`TIOCNOTTY` returned 0 without recording anything.

Implemented: real `setpgid` (with the POSIX rule that a process may only move
itself or its own children), `setsid` that fails `EPERM` for a group leader,
correct `getpgid`, new `getpgrp` (173), `TIOCSCTTY` recording the claimed VT and
refusing a second controlling terminal with `EPERM`, and `TIOCNOTTY`.

`fork_from` copies `pgid`/`sid` and clears `ctty_vt`, so a child starts in its
parent's group with no controlling terminal — the POSIX rule, and what makes a
shell's `fork(); setpgid();` the only way to background a job.

Verified by `initrd/jobctltest.c`: `failures=0`, covering the roundtrip above,
child-vs-parent group isolation, the `setsid` `EPERM` rule, `TIOCSCTTY`, and
`tcgetpgrp` agreeing with `getpgrp`.

### `pthread_join` was never the problem — reaping was

A sibling removed from the run queue by `exit_group` never runs its own exit
path, so its `CLONE_CHILD_CLEARTID` word is never zeroed and its futex is never
woken. glibc's `pthread_join` blocks on exactly that word, so reaping threads
*broke* joining — the fix for zellij would have introduced the very hang it was
meant to remove. Reaped siblings now get their ctid word cleared and their
futex woken, after the run-queue removal so a thread cannot be both reaped and
later run its own exit path.

### Four wrong expectations, and what the host is for

Every failure in this area turned out to be the *probe*, not the kernel. The
host is a working Linux, so it settles these in seconds:

| assertion | reality |
|---|---|
| second `pthread_join` must fail | glibc returns 0 (descriptor reuse) — verified on Linux 6.x |
| `TIOCSCTTY` on an ssh session must succeed | returns `EPERM` (session already has a ctty) |
| `TIOCSCTTY`'s third arg is a flag | it *is* the fd to claim; passing 0 asked for a pipe |
| a vnode diagnostic that never printed | the kernel was not running — the build had failed |

That last one is the lesson worth keeping. A broken build means the emulator
runs the *previous* binary, so a diagnostic going silent is evidence of a
compile error, not of a code path not being reached. Check the build first when
a trace stops appearing.

The general rule: **before concluding the kernel is wrong, run the same probe on
the host.** Every one of these would otherwise have become a kernel "fix" that
made DragOS *diverge* from Linux — which is the opposite of the goal.

## exec: one loader, two entry points (done)

`exec_initrd_args` and `sys_execve` were independent implementations of the same
sequence — `create_address_space`, `write_cr3`, interpreter resolution,
`load_elf` — which had already drifted: the boot path synthesised an environment
the syscall path never saw.

Now `exec::load_image` owns the whole shared half (read file, resolve `PT_INTERP`,
create the address space, install it, map segments, apply relocations, build the
stack) and takes a `register` hook. What genuinely cannot be shared is the *entry*
mechanism: `execve` runs on an existing thread and returns through the dispatcher;
the boot path has no current thread, so it registers pid 1 and calls
`execute_and_resume`.

**Environment follows Linux: `execve` passes the caller's `envp` through unchanged
and invents nothing.** The fixed block (`USER=root`, `WAYLAND_DISPLAY`, `SEATD_SOCK`,
…) is boot-only, in `boot_environment`. `draginit`'s daemons inherit from it and
would break if `execve` overrode their real environment.

Three real bugs surfaced while doing this, none of them about the refactor's shape:

1. **`exec_initrd_args` could fall through into a second image.**
   `execute_and_resume` is not `-> !`: it calls `jump_to_user_mode`, which ends in
   `iretq`. If userspace then faults, control unwinds back into this frame and
   returns to `main`, which runs its `/bin/sh` fallback and loads a *second*
   image over the first. Observed directly: two `register_exec` traces and two
   `main_entry` values in one boot. It now halts instead of returning, and only
   a genuine *load* failure returns so `main` can try the next candidate.
2. **`sys_execve` reset the VMA list after mapping.** The reset has to happen
   *before* `load_image`, or it discards the stack VMA that `load_elf` just
   inserted. Symptom was every execve'd program faulting on its first
   instruction: `mem[0x7fffffffa7a0] = <unmapped>`.
3. **Reading the file after `write_cr3`.** `read_whole` dereferences a `VNode`
   allocated in the *old* address space; after the switch the kernel runs against
   tables that do not contain it. Reads now happen first.

### Method note

The first three wrong theories here were all mine: double address-space
allocation, the `register` hook's placement, and `FRAME_ALLOCATOR` re-entrancy.
Each looked convincing and each was wrong. What settled it was reading the fault
dump (`<unmapped>`, two `main_entry` values) instead of reasoning about what could
possibly be wrong. Symptom-first beat mechanism-first again.

## Still unproven

- `pthread_join` semantics. The thread list makes reaping possible, but nothing
  implements join's blocking behaviour yet.
- Whether zellij actually gets panes and a working shell now that the POSIX pty
  API is present. The API is verified in isolation by `ptyposixtest`, but zellij
  has not been re-run against it since.
- Whether zellij's UI is correct once its output can be isolated from the
  compositor's logging. Both write to `/dev/tty`, which correctly follows the
  active VT, so until one of them uses its own VT there is no clean oracle.

## Tooling notes

`scripts/build-fs.sh` stages zellij with its ldd closure into `/usr/bin` and
`/bin`. `initrd/mkdirtest.c` is the probe that found the syscall gaps — it
issues raw syscalls and prints the number and errno, because the syscall census
only reports the top 12 and the failing numbers never appeared in it.

`DRAGOS_FULLSCREEN=1` runs QEMU fullscreen for visual checks.

## Working practice

**Shut the emulator down when a measurement is read.** QEMU left running
between steps burns CPU and RAM in the background for nothing. Once a run's
logs have been read, kill it:

```
for p in $(pgrep -f 'qemu-sys[t]em'); do kill -9 "$p"; done
```

Note the `[t]` in the pattern: `pgrep -f qemu-system-x86_64` matches the shell
command that contains it, so it kills its own shell.

A run whose QEMU exits without a `KERNEL PANIC` is usually just the launching
command timing out and taking the process group with it, not a guest fault.
Check for the panic line before treating an exit as a bug.

## disk.img

`disk.img` accumulated ext4 directory-checksum corruption from `debugfs write`
calls (the same mechanism that stages binaries), which made zellij spin on
`statx` against an inode whose directory block failed its checksum. Repaired
with `e2fsck -fy`; `e2fsck -fn` is clean as of the last check. If it gets flaky
again, regenerate rather than hand-patching — the corruption is structural, not
a bad file.
