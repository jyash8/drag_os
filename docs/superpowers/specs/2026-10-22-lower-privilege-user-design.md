# A lower-privilege user — DragOS access control

**Status:** design agreed, not yet implemented.
**Date:** 2026-10-22.

## 1. What this is

The kernel identifies the caller of every syscall precisely — the process, its
uid/gid/euid/egid/suid/sgid — and uses that identity for two things: which
DRM context a submit belongs to, and who may claim a controlling terminal. It
uses it for **nothing else**. The filesystem stores each file's mode and owner
and reports them in `stat`, but **never compares them against the caller**.

The practical effect is that every process in the guest has the same access to
the filesystem as the kernel does. A uid-0 process and a uid-1000 process can
open exactly the same files. `setuid` changes what `getuid` reports, not what
the process can do.

This spec adds the missing comparison — Linux's rules — and then gives the
guest an unprivileged user to run as, so the rules are actually exercised.

## 2. Why now

Two things in flight need a notion of an untrusted process:

- **`init_module(2)`** (module-system milestone 9). Loading code into the
  kernel is the most privileged thing a userland program can ask for. Linux
  gates it on `CAP_SYS_MODULE`. Here the loader exists and works
  (`docs/module-system.md`), but there is no way to say "this process may not
  load modules" — because there is no way to say "this process is not root".
- **The XDG desktop portal** (`docs/xdg-desktop-portal-plan.md`). Per-app
  consent means nothing while every process can read every file: the portal
  asks the user "may this app see your screen?" and the answer is worthless
  if the app could have read the screen file directly.

The two do not have to wait for each other, but they want the same thing: a
kernel that knows who is asking.

## 3. The model

Linux's rules, unchanged. The vocabulary:

- **fsuid** — the effective uid used for filesystem access. This kernel has no
  `setfsuid`, so it is the **euid**.
- **Supplementary groups** — the process's group list, beyond its primary gid.
  `getgroups`/`setgroups` are stubs today and become real.
- **Root's bypasses** — uid 0 holds `CAP_DAC_OVERRIDE` and
  `CAP_DAC_READ_SEARCH`, so it may read, write and execute any file. It does
  **not** bypass the sticky bit, and it does not bypass the ownership rules on
  `chmod`/`chown`.

### 3.1 The checks

| Operation | Rule | Failure |
|---|---|---|
| Path traversal | every **component** of the path needs the `x` bit | `EACCES` |
| `open` for reading | the `r` bit, against the *owner* / *group* / *other* class the caller falls in | `EACCES` |
| `open` for writing, `O_TRUNC` | the `w` bit | `EACCES` |
| `execve` | the `x` bit | `EACCES` |
| create a file | the **parent directory** needs `w`+`x`; the new file's **owner** is the caller's uid; its **group** is the parent directory's group if the caller is in that group, and otherwise the caller's effective gid (Linux's rule, which is what makes setgid directories work); the mode is `mode & ~umask` | `EACCES` |
| unlink, rename, link, symlink | the parent needs `w`+`x`; the **sticky bit** on the parent restricts *who* may remove the entry: the entry's owner, the directory's owner, or root | `EACCES` / `EPERM` |
| `chmod` | the caller is the file's owner, or root | `EPERM` |
| `chown` | root; or the owner changing only the group, to a group they are in | `EPERM` |
| setuid/setgid bit on exec | the new process's euid/egid become the file's owner/group, if the caller could execute it at all | — |

The class a caller falls in: the **owner** class if fsuid equals the file's
uid; otherwise the **group** class if the file's gid is the caller's primary
gid or is in the caller's supplementary group list; otherwise **other**.

### 3.2 Where the rules live

A new module, `crates/kernel/src/fs/perm.rs`, holds the rules and nothing
else: given a file's mode/uid/gid, the caller's credentials, and an *intent*
(`Read`, `Write`, `Execute`, `Create`, `Remove`, `Chmod`, `Chown`), it answers
allowed or not.

The **intent** is supplied by the caller, because only the caller knows it:
`open` knows whether it was `O_RDONLY` or `O_WRONLY`; the fs layer's
`read_at` does not.

This is deliberately *not* implemented inside the fs layer's `read_at`/`write_at`.
Linux checks at **open** time, and the fd's open mode is the authority for
what the fd may do afterwards — a fd opened `O_RDONLY` cannot be made to
write. Checking per operation would need every data path to know the fd's
mode, which the fs layer does not have.

The fs layer keeps storing the mode and the owner, and stays free of policy.

### 3.3 The call sites

| Call site | Check |
|---|---|
| `fs::lookup`'s path walk, per component | `Execute` (the `x` bit) |
| `open_path` | `Read` / `Write` / `Execute` per the flags |
| `fs::create` | `Create` on the parent |
| unlink / rename / link / symlink | `Remove` on the parent, plus the sticky bit |
| `chmod` / `chown` | `Chmod` / `Chown` on the file |
| `sys_execve`, on the file it is about to run | `Execute` on the file |

The kernel's **own** file reads — the module loader reading a module, the DSO
cache filling a page — do not go through these checks. Linux does not check
the kernel's own reads either, and a kernel that could not read its own
filesystem would not boot.

### 3.4 The credentials

- The identity is the **euid** and the **primary gid** plus the
  **supplementary group list**.
- The supplementary group list is a `Vec<u32>` on the `Process`, with
  `getgroups`/`setgroups` implemented for real. `setgroups` is root-only,
  as on Linux.
- Root's bypasses are explicit: a small capability set on the `Process`
  (`CAP_DAC_OVERRIDE`, `CAP_DAC_READ_SEARCH`, `CAP_FOWNER`, `CAP_CHOWN`,
  `CAP_SETUID`, `CAP_SETGID`), with root holding all of them. This is the
  vocabulary a later `init_module(2)` will use for `CAP_SYS_MODULE`, rather
  than a second, parallel mechanism.

## 4. The deployment

The session's applications run as **uid 1000, gid 1000**; the init stays
root. `/etc/passwd` already has `user:x:1000:1000` ✓.

- `draginit` spawns the compositor and the terminal with the child's uid/gid
  set to 1000. Rust's `std::os::unix::process::CommandExt` does this with
  `setgid`+`setuid` in the child, in the right order, before `exec` ✓ — and
  the kernel's `setuid` already enforces the "only root may change to another
  user" rule ✓.
- The **device nodes** the compositor needs are created by the kernel
  (`fs/devdri.rs`, `fs/devinput.rs`) with mode `0660` and **owner root, group
  1000** — the `video`/`input` group of a Linux system, expressed with the
  group id the guest already has. A uid-1000/gid-1000 process then opens
  `/dev/dri/renderD128` through the *group* class ✓.
- The **Wayland socket** must be connectable by the clients, and connecting
  needs *write* permission on the socket. The socket's mode today is tmpfs's
  default `0755` ✗, which no unprivileged client could connect to. It becomes
  `0666` ✓.
- The compositor binds the socket in **`/run/user/1000`** ✓ (Linux's layout),
  because `/run` is `0755` root and a uid-1000 process cannot create a file
  in it. The init creates `/run/user/1000` owned by uid 1000 ✓.
- The image's files stay root-owned ✓, so a uid-1000 process cannot read
  `/etc/shadow` (if it exists) or sshd's host keys ✓ — which is the point.

## 5. The testing

A new guest program, `privdrop`, does what the guest's users would do and
asserts the outcome:

1. As root: open a `0600 root` file → succeeds.
2. `setuid(1000)`, `setgid(1000)`: open the same file → `EACCES` ✓.
3. As uid 1000: open a `0644` file → succeeds ✓ (the "other" class).
4. As uid 1000: create a file in a `0700 root` directory → `EACCES` ✓ (the
   directory's `w`+`x`, and the traversal's `x`).
5. As uid 1000: a file it creates is owned by uid 1000 ✓, its group is the
   caller's effective gid unless the parent directory's group applies ✓, and
   its mode is `mode & ~umask` ✓.
6. As uid 1000: unlink a root-owned file in a world-writable sticky
   directory → `EPERM` ✓ (the sticky bit).
7. As uid 1000: `chmod` a root-owned file → `EPERM` ✓.
8. The **kernel's own** reads still work: the module self-check loads a module
   from the filesystem after the compositor has dropped privileges ✓ — that is
   the "the kernel is not subject to DAC" case, and it is asserted by the
   existing boot trail.

The program is a test case in the suite (`privilege_drop`), asserting the
results, so a regression in the rules fails CI. The suite already has a
`module_selftest` case that proves the *kernel-side* file reading is
unaffected ✓.

**A second, smaller guard:** the compositor and the terminal actually run as
uid 1000, so the boot trail can show it — a `getuid` line from each, or the
suite asserting that the compositor's own files are not root-owned.

## 6. What this does not do

Stated plainly, because "everything Linux does" is a large phrase and the
edges matter:

- **No capabilities in the fine-grained sense.** There is a capability *set*
  (to make root's bypasses explicit and to give `init_module` a vocabulary),
  but not the per-object, inheritable, ambient capability model Linux has. Root
  holds everything; a non-root process holds nothing.
- **No LSM.** No SELinux, no AppArmor, no seccomp. A process's only limits
  are its uid, its groups and the file modes.
- **No `CAP_SYS_MODULE` yet.** The capability exists in the vocabulary, but
  the `init_module(2)` syscall is a separate milestone and is not in this
  spec.
- **The kernel's own reads are unchecked** — by design, as on Linux.
- **`fsuid` is `euid`** because there is no `setfsuid`.
- **The permission model applies to the filesystems that carry a mode and an
  owner**: ext2, ext4, tmpfs. The synthetic filesystems (`/dev`, `/proc`,
  `/sys`) are their own thing; `/dev` nodes carry the mode the kernel gives
  them ✓, and `/proc` and `/sys` are per-process or read-only by
  construction ✓.

## 7. Risks

- **The guest's apps may stop working.** This is the real risk, and it is why
  the deployment is explicit about the modes: the compositor needs
  `/dev/dri`, `/dev/input`, a writable socket directory and a connectable
  socket. The boot trail and the suite's compositor tests are the detector.
- **A missed call site is a hole.** The rules are in one module, but the call
  sites are several. A syscall that mutates a file and forgets the check is a
  hole, and the test program is the detector — it must cover each operation
  the model claims to enforce.
- **Root's bypasses could be over-broad.** If a bypass is granted by default
  to a non-root process, the model is a decoration. The capability set starts
  with root holding everything and nothing else holding anything.

## 8. The order of work

1. `perm` module + the credentials (the euid, the group list, the capability
   set) + the `open` and path-traversal checks. The test program's cases 1-3.
2. The mutation checks (create, unlink/rename, chmod/chown), the sticky bit,
   the new file's owner, and the umask fix. The test program's cases 4-7.
3. The deployment: `draginit` spawning the session's apps as uid 1000, the
   device nodes' group, the socket's mode, `/run/user/1000`.
4. The `execve` check and the setuid/setgid-on-exec semantics.

Step 1 alone is already a change in behaviour for a non-root process, and
every step is separately testable.
