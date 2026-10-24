# A lower-privilege user — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the kernel enforce Linux's file-permission rules, and give the guest an unprivileged user (uid 1000) for its session applications to run as.

**Architecture:** A new `fs::perm` module holds the rules and is asked whether an *intent* (`Read`/`Write`/`Execute`/`Create`/`Remove`/`Chmod`/`Chown`) is allowed, given a file's mode and owner and the caller's credentials. The syscall layer supplies the intent, because only it knows it; the fs layer keeps storing the mode and owner and stays free of policy. Checks happen at open time, as on Linux, so an fd's open mode stays the authority for what it may do afterwards. The kernel's own reads bypass the checks.

**Tech Stack:** Rust (`no_std` kernel), x86_64, ext4/ext2/tmpfs, C for the guest test program, the existing `cargo run -p tools -- test` harness.

**Spec:** `docs/superpowers/specs/2026-10-22-lower-privilege-user-design.md`

## Global Constraints

- The rules are **Linux's rules, unchanged** (spec §3.1). Do not invent a variant.
- The identity is the **euid** and the **effective gid** plus the **supplementary group list**. There is no `setfsuid`, so there is no fsuid (spec §3.4).
- The kernel's **own** file reads do not go through the checks (spec §3.3). A kernel that could not read its own filesystem would not boot.
- Root bypasses the read/write and execute checks via `CAP_DAC_OVERRIDE` / `CAP_DAC_READ_SEARCH`, and does **not** bypass the sticky bit or `chmod`/`chown`'s ownership rules (spec §3.1, §3.4).
- The fs layer stays free of policy: it stores the mode and owner, it does not decide (spec §3.2).
- Every check is a `Decision`, never a silent allow: `Deny` carries the errno.
- The guest test program is a **suite case**, so a regression in the rules fails CI. The suite is `cargo run -p tools --release -- test`; a case is added to `crates/tools/src/test_runner.rs`.
- AGENTS.md §1 lock order: `process.lock()` is the innermost lock. `perm::current_creds()` takes it and **copies** the credentials out; it must not be called while another lock is held.

## Review Focus

Input classes the spec implies but no task's test exercises — each needs a test in the task that owns the code:

1. **A file owned by a group the caller is only *supplementary* to** (not the primary gid) — the group class must apply through the supplementary list, not just the primary gid. → Task 1.
2. **A file the caller can read but not write, opened `O_WRONLY`** — the check must use the *requested* access, not the file's read bit. → Task 1.
3. **A path whose *intermediate* directory is `0700` and root-owned** — the traversal check must apply to every component, not just the last. → Task 1.
4. **A root-owned file inside a world-writable *non*-sticky directory** — unlink must be allowed there (the sticky bit is what restricts it, not the directory's owner). → Task 2.
5. **A file created in a directory owned by a group the caller is in** — the new file's group must be the directory's group, not the caller's egid. → Task 2.

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/kernel/src/fs/perm.rs` (new) | The rules. `Access`, `Creds`, `FilePerms`, `Decision`, `check`, `check_sticky`, `current_creds`, `new_file_owner`, and the capability bits. No other file decides policy. |
| `crates/kernel/src/tasks.rs` | `Process` gains `groups: Vec<u32>` and `caps: u32`. |
| `crates/kernel/src/arch/x86_64/syscalls/proc.rs` | `getgroups`/`setgroups` for real; the capability set's initial value. |
| `crates/kernel/src/fs/mod.rs` | The path walk's per-component `x` check; `create` sets the new entry's owner. |
| `crates/kernel/src/arch/x86_64/syscalls/fsio.rs` | `open_path`'s check; the mutation syscalls' checks; the umask fix. |
| `crates/kernel/src/fs/ext4/mod.rs`, `crates/kernel/src/fs/tmpfs.rs` | The new inode's uid/gid/mode. |
| `crates/kernel/src/exec.rs` | The `execve` check. |
| `crates/kernel/src/fs/devdri.rs`, `crates/kernel/src/fs/devinput.rs` | The device nodes' group. |
| `userspace/draginit/src/main.rs` | Spawn the session's applications as uid 1000. |
| `initrd/privdrop.c` (new) | The test program. |
| `crates/tools/src/test_runner.rs` | The suite case. |
| `scripts/build-fs.sh` | Stage `privdrop`, create `/run/user/1000`, set the image's modes. |

---

### Task 1: the rules, the credentials, and the open and traversal checks

**Files:**
- Create: `crates/kernel/src/fs/perm.rs`
- Modify: `crates/kernel/src/tasks.rs` (the `Process` fields), `crates/kernel/src/fs/mod.rs` (the path walk), `crates/kernel/src/arch/x86_64/syscalls/fsio.rs` (`open_path`), `crates/kernel/src/arch/x86_64/syscalls/proc.rs` (`getgroups`/`setgroups`)
- Create: `initrd/privdrop.c`
- Modify: `scripts/build-fs.sh` (stage the test program), `crates/tools/src/test_runner.rs` (the suite case)

**Interfaces:**
- Consumes: `FileNode::stat()` (already returns `st_mode`, `st_uid`, `st_gid`) — the mode and owner come from there.
- Produces:
  ```rust
  // crates/kernel/src/fs/perm.rs
  pub enum Access { Read, Write, Execute, Create, Remove, Chmod, Chown }
  pub struct Creds { pub uid: u32, pub gid: u32, pub groups: alloc::vec::Vec<u32>, pub caps: u32 }
  pub struct FilePerms { pub mode: u32, pub uid: u32, pub gid: u32 }
  pub enum Decision { Allow, Deny(u64) }

  pub fn check(file: &FilePerms, creds: &Creds, access: Access) -> Decision;
  pub fn current_creds() -> Option<Creds>;
  pub fn perms_of(node: &dyn crate::fs::vfs::FileNode) -> FilePerms;

  pub const CAP_DAC_OVERRIDE: u32 = 1 << 0;
  pub const CAP_DAC_READ_SEARCH: u32 = 1 << 1;
  pub const CAP_FOWNER: u32 = 1 << 2;
  pub const CAP_CHOWN: u32 = 1 << 3;
  pub const CAP_SETUID: u32 = 1 << 4;
  pub const CAP_SETGID: u32 = 1 << 5;
  ```
  `Decision::Deny(errno)` carries `EACCES` (13) or `EPERM` (1).

- [ ] **Step 1: Write the failing test program**

  `initrd/privdrop.c`, asserting the *first* three cases of spec §5:

  ```c
  // Opens /etc/shadow-like file: a root-only file the test creates itself.
  // 1. as root: open a 0600 root file            -> succeeds
  // 2. setuid(1000)+setgid(1000): open it        -> EACCES
  // 3. as uid 1000: open a 0644 file             -> succeeds
  ```

  The program creates its own fixtures under `/tmp` so it does not depend on
  the image's contents: a `0600` file owned by root, and a `0644` file. It
  prints one line per assertion and a final `PRIVDROP_DONE`:

  ```
  PRIVDROP_ROOT_OPEN_OK
  PRIVDROP_USER_OPEN_EACCES
  PRIVDROP_USER_OPEN_OTHER_OK
  PRIVDROP_DONE
  ```

  It exits non-zero if any assertion fails, and prints the failing line
  `PRIVDROP_FAIL <what>`.

- [ ] **Step 2: Stage it and add the suite case**

  In `scripts/build-fs.sh`, build it the way the other guest programs are
  built and write it to `disk.img` at `/privdrop` (the same pattern as
  `initrd/fixed_size_test.c`).

  In `crates/tools/src/test_runner.rs`, add:

  ```rust
  TestCase {
      name: "privilege_drop",
      command: "/privdrop",
      expected: &[
          "PRIVDROP_ROOT_OPEN_OK",
          "PRIVDROP_USER_OPEN_EACCES",
          "PRIVDROP_USER_OPEN_OTHER_OK",
          "PRIVDROP_DONE",
      ],
      timeout_secs: 30,
      gl_only: false,
  },
  ```

- [ ] **Step 3: Run the test to verify it fails**

  Run: `cargo run -p tools --release -- build-kernel && cargo run -p tools --release -- build-fs && cargo run -p tools --release -- test`

  Expected: `[FAIL] Test 'privilege_drop'` — the program opens the root-only
  file successfully as uid 1000, because nothing checks.

- [ ] **Step 4: Implement `perm.rs`**

  The rules, in the order Linux applies them:

  1. `CAP_DAC_OVERRIDE` allows `Read` and `Write`.
  2. `CAP_DAC_READ_SEARCH` allows `Execute`.
  3. `Chmod` is allowed for the file's owner or with `CAP_FOWNER`; `Chown`
     needs `CAP_CHOWN` (or the owner changing only the group — Task 2).
  4. Otherwise pick the class: **owner** if `creds.uid == file.uid`; else
     **group** if `file.gid` is `creds.gid` or is in `creds.groups`; else
     **other**. The bits are `(mode >> 6) & 7`, `(mode >> 3) & 7`, `mode & 7`.
  5. The bit needed: `Read` → 4, `Write` → 2, `Execute` → 1, `Create` → 2,
     `Remove` → 2. Deny with `EACCES` if the bit is not set.

  `current_creds()` reads the current thread's process and copies out the
  euid, the egid, the group list and the capability bits. It must not be
  called with another lock held (AGENTS.md §1).

- [ ] **Step 5: Give the `Process` the group list and the capabilities**

  In `crates/kernel/src/tasks.rs`, add to `Process`:

  ```rust
  /// The supplementary group list (Linux's groups(2)).
  pub groups: alloc::vec::Vec<u32>,
  /// The capability bits this process holds (see `fs::perm`).
  pub caps: u32,
  ```

  Initialise `groups` empty and `caps` to *all* the capability bits for a
  uid-0 process and *none* for a non-root one. The uid-0 case is what keeps
  the guest working unchanged today; a process that drops to uid 1000 must
  lose the bypasses with it, so the capabilities have to be recomputed
  whenever the credentials change — in `setuid`/`setgid`/`setreuid`/
  `setregid` (`crates/kernel/src/arch/x86_64/syscalls/proc.rs`), not only at
  creation.

  The `Process` clone (in the same file) must carry `groups` and `caps` too.

- [ ] **Step 6: Implement `getgroups`/`setgroups`**

  In `crates/kernel/src/arch/x86_64/syscalls/proc.rs`, replace the stubs:
  `getgroups` copies the caller's list into the user's buffer (up to `size`
  entries, returning the full count, as Linux does); `setgroups` replaces the
  caller's list and is **root-only** (`EPERM` otherwise).

- [ ] **Step 7: Check the open path**

  In `open_path` (`crates/kernel/src/arch/x86_64/syscalls/fsio.rs`), after
  the file is found and before it is returned:

  - derive the intent from the flags: `O_WRONLY`/`O_RDWR` → `Write`,
    `O_RDONLY` → `Read`, `O_TRUNC` → also `Write`;
  - build `FilePerms` from the node's `stat()`;
  - `perm::check(...)`, and on `Deny(errno)` return that errno.

  `O_DIRECTORY` is not an access mode and does not add a check.

  The check applies to `VNode::FsNode` (a real file). The synthetic
  filesystems (`/dev`, `/proc`, `/sys`, the initrd) are not gated: their
  modes are the kernel's own (spec §6).

- [ ] **Step 8: Check the path walk**

  In `crates/kernel/src/fs/mod.rs`, `lookup_relative_to` walks the path
  component by component. For every **intermediate** component, the
  directory needs the `x` bit for the caller: `perm::check(dir, creds,
  Access::Execute)`, and `Deny(EACCES)` means the lookup fails.

  The final component is *not* checked here — `open_path` checks it with the
  real intent. Checking it here too would make a `chmod 000` file
  un-`stat`able, which is not Linux's behaviour.

- [ ] **Step 9: Run the test to verify it passes**

  Run: `cargo run -p tools --release -- test`

  Expected: `[PASS] Test 'privilege_drop'`, and the whole suite green.

  **The suite must still be green.** Root bypasses the read/write checks, so
  every existing test — which runs as root — is unaffected. If a test fails,
  the bypass is wrong: root must not be subject to the read/write rules.

- [ ] **Step 10: Run the extra checks this task's review focus names**

  Extend `initrd/privdrop.c` with:

  - a file owned by group 1000, opened as uid 1000 with **gid 2000** (so the
    primary gid does not match) and 1000 in the supplementary list → must
    succeed; the same file with an empty group list → must be `EACCES`;
  - a `0644` file opened `O_WRONLY` → must be `EACCES`;
  - a path `/rootonly/sub/file`, where `/rootonly` is `0700` root-owned and
    `sub` is `0755` → as uid 1000 the lookup must fail with `EACCES`.

  Add the matching `PRIVDROP_*` lines to the suite case's `expected` list.

  Run: `cargo run -p tools --release -- test`
  Expected: `[PASS] Test 'privilege_drop'`.

- [ ] **Step 11: Commit**

  ```bash
  git add crates/kernel/src/fs/perm.rs crates/kernel/src/tasks.rs \
          crates/kernel/src/fs/mod.rs crates/kernel/src/arch/x86_64/syscalls/fsio.rs \
          crates/kernel/src/arch/x86_64/syscalls/proc.rs initrd/privdrop.c \
          scripts/build-fs.sh crates/tools/src/test_runner.rs
  git commit -m "feat(fs): enforce the file permission bits on open and traversal"
  ```

---

### Task 2: the mutation checks, the sticky bit, and the new file's owner

**Files:**
- Modify: `crates/kernel/src/fs/perm.rs` (the `Chown` group rule, `check_sticky`, `new_file_owner`), `crates/kernel/src/fs/mod.rs` (`create`), `crates/kernel/src/arch/x86_64/syscalls/fsio.rs` (unlink, rename, link, symlink, mkdir, chmod, chown, and the umask fix), `crates/kernel/src/fs/ext4/mod.rs` and `crates/kernel/src/fs/tmpfs.rs` (the new inode's uid/gid/mode)
- Modify: `initrd/privdrop.c` (the remaining cases), `crates/tools/src/test_runner.rs`

**Interfaces:**
- Consumes: `perm::check`, `perm::Creds`, `perm::FilePerms` from Task 1.
- Produces:
  ```rust
  // crates/kernel/src/fs/perm.rs
  /// Whether the caller may remove `entry_owner`'s entry from `dir`.
  pub fn check_sticky(dir: &FilePerms, entry_owner: u32, creds: &Creds) -> Decision;
  /// The (uid, gid) a new entry created in `parent` by `creds` gets.
  pub fn new_file_owner(parent: &FilePerms, creds: &Creds) -> (u32, u32);
  ```

- [ ] **Step 1: Extend the test program with the failing cases**

  In `initrd/privdrop.c`, as uid 1000:

  - create a file in a `0700` root-owned directory → `EACCES`;
  - create a file in a `0777` directory → succeeds, and the new file is
    owned by uid 1000 with the caller's egid, and its mode is
    `mode & ~umask`;
  - unlink a root-owned file in a `1777` (sticky) directory → `EPERM`;
  - unlink a root-owned file in a `0777` (non-sticky) directory → succeeds;
  - `chmod` a root-owned file → `EPERM`;
  - `chown` a root-owned file → `EPERM`.

  Each prints a `PRIVDROP_*` line; add them all to the suite case's
  `expected` list.

- [ ] **Step 2: Run the test to verify it fails**

  Run: `cargo run -p tools --release -- test`
  Expected: `[FAIL] Test 'privilege_drop'` — the unlink, chmod and chown
  cases are not enforced yet.

- [ ] **Step 3: Implement `check_sticky` and `new_file_owner`**

  `check_sticky(dir, entry_owner, creds)`: if the directory has no sticky
  bit (`mode & 0o1000 == 0`), allow. Otherwise allow only if the caller is
  the entry's owner, the directory's owner, or holds `CAP_FOWNER`; deny with
  `EPERM`.

  `new_file_owner(parent, creds)`: the uid is `creds.uid`; the gid is
  `parent.gid` if the caller is in the parent directory's group (primary or
  supplementary), otherwise `creds.gid`.

- [ ] **Step 4: Enforce the mutation checks**

  In `crates/kernel/src/arch/x86_64/syscalls/fsio.rs`:

  - `sys_unlinkat` / `sys_unlink`: the **parent directory** needs `w`+`x`
    (`perm::check(parent, creds, Access::Remove)`), and `check_sticky`
    applies with the entry's owner;
  - `sys_rename`: both parents need `w`+`x`; the sticky bit applies to the
    source's parent and the destination's parent;
  - `sys_link` and `sys_symlink`: the parent of the new link needs `w`+`x`;
  - `sys_mkdir` / `sys_mkdirat`: the parent needs `w`+`x`
    (`Access::Create`);
  - `sys_chmod`: `perm::check(file, creds, Access::Chmod)`;
  - `sys_chown`: `perm::check(file, creds, Access::Chown)`.

  `Access::Create` and `Access::Remove` need the directory's `w`; the `x`
  comes from the traversal check in Task 1 (spec §3.3).

  For `chown`, the owner may change the *group* to one they are in, and
  only root may change the owner (Linux's rule). `perm::check` with
  `Access::Chown` implements the "root only" half; the syscall compares the
  requested uid/gid against the current ones and applies the group exception.

- [ ] **Step 5: Set the new entry's owner and mode**

  In `crates/kernel/src/fs/ext4/mod.rs`'s `create`, the new inode's
  `uid`/`gid`/`mode` currently come from `Inode::default()` and a fixed
  `0o755`/`0o644`. They must be the caller's: `perm::new_file_owner` for the
  uid/gid, and the mode the caller asked for, masked by the umask.

  The mode the caller asked for reaches `create` as a parameter — check how
  `fs::create` is called from `sys_mkdir`/`sys_open` and thread the mode
  through. `fsio.rs` already computes `let _perm = (mode & 0o777) & !umask;`
  — the value is *discarded* (the binding is named `_perm`), which is the
  bug: use it.

  In `crates/kernel/src/fs/tmpfs.rs`'s create path, the same: the new node's
  mode and owner.

- [ ] **Step 6: Run the test to verify it passes**

  Run: `cargo run -p tools --release -- test`
  Expected: `[PASS] Test 'privilege_drop'` and the suite green.

- [ ] **Step 7: Run the extra check this task's review focus names**

  Extend `initrd/privdrop.c`: create a file in a directory whose **group**
  is 1000 while the caller's primary gid is 2000 and 1000 is in the
  supplementary list → the new file's group must be the directory's group
  (1000), not the caller's egid (2000). Print the gid in the existing
  ownership line and assert it in the program.

  Run: `cargo run -p tools --release -- test`
  Expected: `[PASS] Test 'privilege_drop'`.

- [ ] **Step 8: Commit**

  ```bash
  git add crates/kernel/src/fs/perm.rs crates/kernel/src/fs/mod.rs \
          crates/kernel/src/arch/x86_64/syscalls/fsio.rs \
          crates/kernel/src/fs/ext4/mod.rs crates/kernel/src/fs/tmpfs.rs \
          initrd/privdrop.c crates/tools/src/test_runner.rs
  git commit -m "feat(fs): enforce the mutation rules, the sticky bit and file ownership"
  ```

---

### Task 3: the deployment — the session's applications run as uid 1000

**Files:**
- Modify: `userspace/draginit/src/main.rs` (spawn the session's apps as uid 1000), `crates/kernel/src/fs/devdri.rs` and `crates/kernel/src/fs/devinput.rs` (the device nodes' group), `crates/kernel/src/fs/tmpfs.rs` (the socket's mode), `scripts/build-fs.sh` (the image's modes, `/run/user/1000`)
- Modify: `crates/tools/src/test_runner.rs` (assert the compositor runs unprivileged)

**Interfaces:**
- Consumes: Task 1's `perm` (the checks are already in place; this task makes a real process hit them).
- Produces: the guest's compositor and terminal run as uid 1000/gid 1000.

- [ ] **Step 1: Write the failing test**

  In `crates/tools/src/test_runner.rs`, add a case that asserts the
  compositor is unprivileged. The compositor prints its uid at startup (a
  one-line change in `mywima`), and the case expects:

  ```
  [mywima] running as uid=1000
  ```

  The compositor is started by `draginit` from a `.toml` unit, and the
  compositor's stdout goes to the VT, not the serial — so the assertion
  needs the *kernel* to log it. The simplest reliable route: the compositor
  writes the line to `/dev/ttyS0` (the same sink `draginit` uses for
  `[SHELL_READY]`) in test mode, and to stdout otherwise.

- [ ] **Step 2: Run the test to verify it fails**

  Run: `cargo run -p tools --release -- test`
  Expected: `[FAIL] Test 'compositor_unprivileged'` — the compositor runs as
  uid 0.

- [ ] **Step 3: Spawn the session's applications as uid 1000**

  In `userspace/draginit/src/main.rs`, the daemon spawn uses
  `std::process::Command`. Add
  `std::os::unix::process::CommandExt::uid(1000)` and `.gid(1000)` to the
  compositor's and the terminal's spawns.

  The kernel's `setuid` already enforces "only root may change to another
  user" and sets uid, euid and suid together, so the child lands as a full
  uid-1000 process with no capabilities.

  **Not** the init itself, and not the services that must stay privileged
  (the ones that own the DRM master or write to /dev).

- [ ] **Step 4: Give the device nodes the group**

  In `crates/kernel/src/fs/devdri.rs` and `crates/kernel/src/fs/devinput.rs`,
  the character nodes are created with mode `0660` and **no owner**, so
  `stat` reports uid 0, gid 0. Set the gid to 1000 so a uid-1000/gid-1000
  process opens them through the group class.

  The directories (`/dev/dri`, `/dev/input`) are `0755` ✓ — traversable by
  anyone ✓.

- [ ] **Step 5: Make the Wayland socket connectable, and give the compositor a directory it can write**

  The socket is a tmpfs file, and tmpfs's default mode is `0755` — a client
  needs **write** permission to `connect()` to a unix socket, so no
  unprivileged client could connect. The socket's mode must become `0666`.

  The compositor binds the socket in `/run`, which is `0755` root-owned, so a
  uid-1000 process cannot create a file there. The socket path must move to
  **`/run/user/1000`**, and the init must create that directory owned by
  uid 1000 (`0700`, as Linux's `/run/user/<uid>` is).

  This touches `mywima` (the socket path) as well as the kernel (the
  directory, and the mode of a newly created socket file). The kernel
  creates socket files in tmpfs — the mode it gives them is the one to
  change.

- [ ] **Step 6: Run the test to verify it passes**

  Run: `cargo run -p tools --release -- test`
  Expected: `[PASS] Test 'compositor_unprivileged'`, and the whole suite
  green — the compositor still renders (the `rainbow`, `gbmscanout` and
  `drm_flip` cases are the detectors).

  If the compositor fails to start, the failure is in the modes: check that
  it can open `/dev/dri/renderD128` (the group), create its socket (the
  directory), and that its clients can connect (the socket's mode).

- [ ] **Step 7: Commit**

  ```bash
  git add userspace/draginit/src/main.rs crates/kernel/src/fs/devdri.rs \
          crates/kernel/src/fs/devinput.rs crates/kernel/src/fs/tmpfs.rs \
          scripts/build-fs.sh crates/tools/src/test_runner.rs
  git commit -m "feat(desktop): run the session's applications as uid 1000"
  ```

---

### Task 4: the exec check, and the setuid/setgid bits

**Files:**
- Modify: `crates/kernel/src/exec.rs` (the executable check), `crates/kernel/src/fs/perm.rs` (the exec rule's use of `CAP_DAC_READ_SEARCH`)
- Modify: `initrd/privdrop.c`, `crates/tools/src/test_runner.rs`

**Interfaces:**
- Consumes: `perm::check` with `Access::Execute` from Task 1.
- Produces: `execve` of a file the caller may not execute fails with
  `EACCES`; a setuid/setgid executable gives the new process the file's
  owner/group.

- [ ] **Step 1: Extend the test program with the failing cases**

  In `initrd/privdrop.c`:

  - as uid 1000, `execve` a `0755` file → succeeds;
  - as uid 1000, `execve` a `0644` file (not executable) → `EACCES`;
  - as uid 1000, `execve` a file in a directory the caller cannot traverse →
    `EACCES`.

- [ ] **Step 2: Run the test to verify it fails**

  Run: `cargo run -p tools --release -- test`
  Expected: `[FAIL] Test 'privilege_drop'` — the non-executable file runs.

- [ ] **Step 3: Check the executable**

  In `crates/kernel/src/exec.rs`, where the interpreter and the binary are
  read for `execve`, check the file the process is about to run: build
  `FilePerms` from its `stat()` and call
  `perm::check(&perms, &creds, Access::Execute)`. On `Deny(errno)`, fail the
  exec with that errno.

  The interpreter (`/lib64/ld-linux-x86-64.so.2`) is read by the kernel on
  the process's behalf — the same rule applies to it, and Linux checks it
  too.

- [ ] **Step 4: Apply the setuid/setgid bits**

  When the executable has the setuid bit (`mode & 0o4000`) and the caller
  could execute it, the new process's uid/euid/suid become the file's uid;
  the same for setgid (`0o2000`) and the gid/egid/sgid. If the caller could
  not execute it at all, the bits are ignored (Linux's rule).

- [ ] **Step 5: Run the test to verify it passes**

  Run: `cargo run -p tools --release -- test`
  Expected: `[PASS] Test 'privilege_drop'` and the suite green.

- [ ] **Step 6: Commit**

  ```bash
  git add crates/kernel/src/exec.rs crates/kernel/src/fs/perm.rs \
          initrd/privdrop.c crates/tools/src/test_runner.rs
  git commit -m "feat(fs): check the executable bit, and honour setuid and setgid"
  ```

---

## Self-Review

**Spec coverage.** §3.1's table → Task 1 (traversal, open, execute) and Task
2 (create, remove, chmod, chown) and Task 4 (execute, setuid bits). §3.2
(the `perm` module) → Task 1. §3.3 (the call sites) → Tasks 1, 2, 4. §3.4
(the credentials) → Task 1. §4 (the deployment) → Task 3. §5 (the testing) →
Tasks 1-4, each extending the same program and the same suite case. §6
(the gaps) → stated, no work. §7 (the risks) → the detector for the
deployment risk is Task 3's suite run; the "missed call site" risk is the
review focus list.

**Gaps found in the review and closed.** The spec does not say which
*directory* the sticky bit applies to on a rename; the plan pins it to both
parents. The spec does not say what happens when a caller opens a file with
`O_RDONLY` on a write-only file; the plan says the check uses the requested
access, and the review focus names it.

**Type consistency.** `perm::check`, `perm::Creds`, `perm::FilePerms`,
`perm::Decision` are defined in Task 1 and used unchanged in Tasks 2-4.
`check_sticky` and `new_file_owner` are defined in Task 2 and used only
there. `Access` gains no variants after Task 1.

**Proportion.** The plan is roughly the length of the spec, and most of it
is signatures, test assertions and the exact rules — not the code.
