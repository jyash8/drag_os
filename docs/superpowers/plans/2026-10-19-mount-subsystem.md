# Mount Subsystem Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give DragOS a mount table and the `mount(2)`/`umount2(2)` syscalls, so `/proc`, `/sys`, `/dev` are genuinely mounted and a filesystem image can be mounted at an arbitrary path.

**Architecture:** Two phases. Phase 1 layers a `MOUNTS` table under path resolution and adds a loopback (`File`-backed) reader so a real ext4 image can be mounted at `/mnt`; the existing hardcoded arms in `fs/mod.rs::lookup` remain as fallback. Phase 2 converts the pseudo-filesystems (`proc`, `sysfs`, `devtmpfs`, `tmpfs`) into real `FileNode` trees and deletes the arms.

**Tech Stack:** Rust `no_std` kernel (`crates/kernel`, target `x86_64-unknown-none`); C test programs in `initrd/` run by the QEMU harness (`crates/tools`); `debugfs`/`mke2fs` at image-build time.

**Spec:** `docs/superpowers/specs/2026-10-19-mount-subsystem-design.md`

## Global Constraints

- Every task ends **green** on `cargo run -p tools -- test` (baseline 19 pass / 0 fail / 5 skip). A refactor task's acceptance test is the pre-existing suite; a feature task's is the new `mount_subsystem` case.
- Obey `AGENTS.md` invariants. In particular: lock ordering (§1), and **do not** take `MOUNTS`/`DEVICES` while holding `SCHEDULER`, `CPUS[k]`, or `process.lock()`, and never across a scheduling boundary.
- Never call `crate::drivers::block::read_sector` while holding `MOUNTS` or `DEVICES` (it does blocking I/O).
- Do not add new hardcoded path arms to `fs/mod.rs::lookup`. Every new filesystem is a table entry.
- Kernel build: `cargo run -p tools -- build-kernel`. Image: `cargo run -p tools -- build-fs`. Single test: `cargo run -p tools -- test <name>`. Suite: `cargo run -p tools -- test`.
- Guest test programs print to the pty, which `init.sh` redirects to `/dev/ttyS0`; no extra redirection is needed for a *foreground* test program.
- `initrd/` is untracked; commit kernel/tools/scripts changes with `git add` on those paths only.

## Review Focus

Conditions this subsystem will meet that no single task's happy-path test exercises; each is pinned by the task named:

1. **Mount over an existing mount point** (stacking) — must not silently drop the old mount. Task 4.
2. **`..` traversal across a mount point** — must stay within the mount where the path began. Task 10.
3. **Backing device/file disappears or read fails** — reads return `EIO`, never fault. Task 10.

---

## Phase 1 — incremental mount table

### Task 1: Failing harness test + `mounttest` skeleton

**Files:**
- Create: `initrd/mounttest.c`
- Modify: `scripts/build-fs.sh` (stage `mounttest` in the `*test` loop and the disk.img sync list)
- Modify: `crates/tools/src/test_runner.rs` (add `mount_subsystem` to `all_tests()`)

**Interfaces:**
- Consumes: nothing (raw `mount(2)`/`umount2(2)` from musl `<sys/mount.h>`).
- Produces: a `mount_subsystem` `TestCase` expecting `MOUNTTEST_DONE failures=0`.

- [ ] **Step 1: Write the test program**

Create `initrd/mounttest.c` (musl-static). It must print each section's verdict and a final `MOUNTTEST_DONE failures=<n>`. Initial content: the ext4-loopback section only.

```c
#include <stdio.h>
#include <fcntl.h>
#include <string.h>
#include <errno.h>
#include <sys/mount.h>
#include <sys/stat.h>

static int failures;

static void check(const char *name, int ok) {
    printf("MOUNTTEST_%s %s\n", name, ok ? "OK" : "FAIL");
    if (!ok) failures++;
}

int main(void) {
    printf("MOUNTTEST_START\n");
    /* Section 1: mount a file-backed ext4 image and read its sentinel. */
    int r = mount("/disk2.img", "/mnt", "ext4", 0, NULL);
    check("MOUNT_EXT4_RC", r == 0);
    if (r == 0) {
        int fd = open("/mnt/hello.txt", O_RDONLY);
        char buf[32] = {0};
        int n = fd >= 0 ? read(fd, buf, sizeof(buf) - 1) : -1;
        if (fd >= 0) close(fd);
        check("EXT4_READ", n > 0 && strstr(buf, "MOUNT_SENTINEL") != NULL);
        check("UMOUNT_RC", umount2("/mnt", 0) == 0);
        struct stat st;
        check("GONE_AFTER_UMOUNT", stat("/mnt/hello.txt", &st) != 0);
    }
    printf("MOUNTTEST_DONE failures=%d\n", failures);
    return failures ? 1 : 0;
}
```

- [ ] **Step 2: Stage it in the image**

In `scripts/build-fs.sh`, add `mounttest` to the `for b in hello test_musl ... ptyprobe pcm_test tmux;` list (the `/usr/bin` copy loop) and to the disk.img sync list near the bottom (`for tbin in foot ...` / the explicit `debugfs write` block). The `/disk2.img` staging arrives in Task 3; until then the program fails at `mount`, which is the intended red.

- [ ] **Step 3: Add the test case**

In `crates/tools/src/test_runner.rs::all_tests()`, add:

```rust
TestCase {
    name: "mount_subsystem",
    command: "/mounttest",
    expected: &["MOUNTTEST_DONE failures=0"],
    timeout_secs: 30,
    gl_only: false,
},
```

- [ ] **Step 4: Run it and confirm it fails**

Run: `cargo run -p tools -- build-fs && cargo run -p tools -- test mount_subsystem`
Expected: FAIL — program prints `MOUNTTEST_MOUNT_EXT4_RC FAIL` and `MOUNTTEST_DONE failures=1`, because `mount(2)` returns `ENOSYS`.

- [ ] **Step 5: Commit**

```bash
git add crates/tools/src/test_runner.rs scripts/build-fs.sh
git commit -m "test: add failing mount_subsystem harness case + mounttest"
```
(`initrd/mounttest.c` is untracked and travels with the tree.)

---

### Task 2: `SectorReader` abstraction; parameterize ext4/ext2 by reader

**Files:**
- Create: `crates/kernel/src/fs/reader.rs`
- Modify: `crates/kernel/src/fs/mod.rs` (declare `pub mod reader;`)
- Modify: `crates/kernel/src/fs/ext2/cache.rs` (hold `Arc<dyn SectorReader>`)
- Modify: `crates/kernel/src/fs/ext2/mod.rs`, `crates/kernel/src/fs/ext4/mod.rs` (`new()` → `open(reader)`; keep `new()` as a delegate)

**Interfaces:**
- Produces:
  ```rust
  pub trait SectorReader: Send + Sync {
      fn read_sector(&self, sector: u64, buf: &mut [u8]) -> Result<(), &'static str>;
      fn write_sector(&self, sector: u64, data: &[u8]) -> Result<(), &'static str>;
      fn capacity(&self) -> u64;
  }
  pub struct GlobalReader;                 // routes to virtio::blk::BLK_DEV / BLOCK_DEVICE
  impl SectorReader for GlobalReader { ... }
  // ext4/mod.rs, ext2/mod.rs:
  impl Ext4Fs { pub fn open(reader: Arc<dyn SectorReader>) -> Option<Arc<Self>>; pub fn new() -> Option<Arc<Self>> { Self::open(Arc::new(GlobalReader)) } }
  impl Ext2Fs { pub fn open(reader: Arc<dyn SectorReader>) -> Option<Arc<Self>>; pub fn new() -> Option<Arc<Self>> { Self::open(Arc::new(GlobalReader)) } }
  ```
- Consumes: `crate::drivers::block::{read_sector, write_sector, ...}` for `GlobalReader`.

- [ ] **Step 1: Write `fs/reader.rs`**

`GlobalReader::read_sector` calls `crate::drivers::block::read_sector`. `capacity()` returns 0 (the global path does not expose it; callers that need capacity use `MountSource::File`).

- [ ] **Step 2: Thread the reader through `BlockCache`**

In `crates/kernel/src/fs/ext2/cache.rs`, `BlockCache` stores `reader: Arc<dyn SectorReader>` and every `crate::drivers::block::read_sector`/`write_sector` call becomes `self.reader.read_sector(...)`. Add `BlockCache::new_with_reader(block_size, reader)`; keep `BlockCache::new(block_size)` delegating with `GlobalReader`.

- [ ] **Step 3: `Ext4Fs::open` / `Ext2Fs::open`**

Replace the two direct `crate::drivers::block::read_sector(2/3, ...)` calls in `Ext4Fs::new` (`ext4/mod.rs:85-86`) with `reader.read_sector(...)`, and pass the reader into `BlockCache::new_with_reader`. Same for `Ext2Fs`. `new()` remains as the `GlobalReader` delegate so `mount_root()` is unchanged.

- [ ] **Step 4: Verify no regression**

Run: `cargo run -p tools -- build-kernel && cargo run -p tools -- test ext4_rw`
Expected: PASS (`EXT4TEST_PASS`). Then `cargo run -p tools -- test` — expect 19 pass / 0 fail / 5 skip.

- [ ] **Step 5: Commit**

```bash
git add crates/kernel/src/fs/reader.rs crates/kernel/src/fs/mod.rs crates/kernel/src/fs/ext2 crates/kernel/src/fs/ext4
git commit -m "refactor(ext4/ext2): read through a SectorReader, not the global device"
```

---

### Task 3: `FileReader` (loopback) + staged `disk2.img`

**Files:**
- Modify: `crates/kernel/src/fs/reader.rs` (add `FileReader`)
- Modify: `scripts/build-fs.sh` (build + stage `disk2.img`)
- Modify: `initrd/mounttest.c` (add a raw image-magic diagnostic section)

**Interfaces:**
- Produces:
  ```rust
  pub struct FileReader { node: Arc<dyn FileNode>, size: u64 }
  impl FileReader { pub fn new(node: Arc<dyn FileNode>, size: u64) -> Self; }
  impl SectorReader for FileReader { /* read_sector/write_sector via node.read_at(offset) */ }
  ```

- [ ] **Step 1: Implement `FileReader`**

`read_sector` reads 512 bytes at `sector*512` via `node.read_at`; reject reads past `size`. `write_sector` returns `Err("read-only image")` for now (the test mounts read-only).

- [ ] **Step 2: Build `disk2.img` in `build-fs.sh`**

Add near the disk.img section (requires `mke2fs`; skip with a warning if absent):

```bash
DISK2="$ROOT/disk2.img"
if [ ! -f "$DISK2" ] && command -v mke2fs >/dev/null 2>&1; then
  S2=$(mktemp -d); printf 'MOUNT_SENTINEL\n' > "$S2/hello.txt"
  mke2fs -q -t ext4 -F -b 1024 -d "$S2" "$DISK2" 8192 && echo "[build-fs] disk2.img built"
  rm -rf "$S2"
fi
[ -f "$DISK2" ] && { debugfs -w -R "rm /disk2.img" "$ROOT/disk.img" >/dev/null 2>&1 || true; debugfs -w -R "write $DISK2 /disk2.img" "$ROOT/disk.img" >/dev/null 2>&1 || true; }
```

Evaluate the chosen size against the 1 KiB-block geometry the ext4 reader accepts; if it rejects 1 KiB blocks, use `-b 4096` and a larger block count.

- [ ] **Step 3: Add the diagnostic to `mounttest.c`**

Before the mount section, open `/disk2.img`, read the first 1024 bytes, and `check("IMAGE_MAGIC", buf+1080? ...)`. Simpler: `lseek` to 0x438 (1080) and read 2 bytes, compare to `0x53EF` (ext magic at offset 1024+56 = 1080).

```c
int img = open("/disk2.img", O_RDONLY);
unsigned char mb[2] = {0};
if (img >= 0) { pread(img, mb, 2, 1080); close(img); }
check("IMAGE_MAGIC", mb[0] == 0x53 && mb[1] == 0xEF);
```

- [ ] **Step 4: Run and confirm partial progress**

Run: `cargo run -p tools -- build-fs && cargo run -p tools -- test mount_subsystem`
Expected: `MOUNTTEST_IMAGE_MAGIC OK`, `MOUNTTEST_MOUNT_EXT4_RC FAIL`, overall FAIL (still no `mount(2)`).

- [ ] **Step 5: Commit**

```bash
git add crates/kernel/src/fs/reader.rs scripts/build-fs.sh
git commit -m "feat(fs): FileReader loopback source + staged disk2.img"
```

---

### Task 4: Mount table, `mount(2)`/`umount2(2)`, ext4-by-source

**Files:**
- Create: `crates/kernel/src/fs/mount.rs`
- Modify: `crates/kernel/src/fs/mod.rs` (declare module; register `/` at mount; consult table in `lookup_relative_to`/`create`; register `ext4`/`ext2` filesystems at boot)
- Modify: `crates/kernel/src/arch/x86_64/syscalls/mod.rs` (arms `165`/`166`; imports)
- Modify: `crates/kernel/src/arch/x86_64/syscalls/fsio.rs` (add `sys_mount`/`sys_umount2`)

**Interfaces:**
- Produces:
  ```rust
  pub struct Mount { pub mountpoint: String, pub root: Arc<dyn FileNode>, pub fstype: &'static str, pub source: Option<String>, pub flags: u64 }
  pub static MOUNTS: SpinLock<Vec<Mount>>;
  pub fn resolve_mount(canonical: &str) -> Option<(Arc<Mount>, String)>; // longest-prefix; rel has no leading '/'
  pub fn mount_add(m: Mount) -> Result<(), u64>;
  pub fn mount_remove(mountpoint: &str, flags: u64) -> Result<(), u64>;
  pub trait Filesystem: Send + Sync { fn name(&self) -> &'static str; fn mount(&self, src: MountSource, flags: u64) -> Result<Arc<dyn FileNode>, u64>; }
  pub static FILESYSTEMS: SpinLock<Vec<Box<dyn Filesystem>>>;
  pub(super) fn sys_mount(source: u64, target: u64, fstype: u64, flags: u64, data: u64) -> u64;
  pub(super) fn sys_umount2(target: u64, flags: u64) -> u64;
  ```
- Consumes: `fs::reader::{GlobalReader, FileReader}`, `fs::ext4::Ext4Fs::open`, `fs::lookup`.

- [ ] **Step 1: Implement `fs/mount.rs`**

`resolve_mount` returns the longest `mountpoint` that is a string-prefix of `canonical` (with a `/` boundary), plus the remainder. `mount_add` rejects duplicates. `mount_remove` refuses the root (`/`) with `EINVAL`.

- [ ] **Step 2: Register `/` and the fstypes at boot**

In `fs::mount_root()`, after the root is set, push `Mount { mountpoint: "/", root: ROOT_NODE, fstype: "ext4", source: Some("vda"), flags: 0 }`. Register `ext4` and `ext2` `Filesystem` impls that call `Ext4Fs::open`/`Ext2Fs::open` with a reader derived from `MountSource`.

- [ ] **Step 3: Consult the table in resolution**

In `lookup_relative_to`, immediately after canonicalization and before the hardcoded arms:

```rust
if let Some((mnt, rel)) = mount::resolve_mount(&canonical) {
    if mnt.mountpoint != "/" || rel.is_empty() {
        // walk rel from mnt.root (same loop used for ROOT_NODE below)
    }
}
```

Preserve current behavior for `/` by falling through to the existing code when `mnt.mountpoint == "/"`. `create` gets the same consult before its `tmp/`/`run/` arms.

- [ ] **Step 4: Add the syscalls**

`sys_mount`: read the three C strings; look up `target` via `fs::lookup` (parent must exist); resolve `source` — if the `Filesystem` needs a device/file, `fs::lookup(source)` → `VNode::FsNode { node }` → `MountSource::File(node)`. Call the registered `Filesystem::mount`, then `mount_add`. `sys_umount2`: `mount_remove`. Add `165 => sys_mount(...)` and `166 => sys_umount2(...)` to the dispatcher, and import both in the `use self::fsio::{...}` list.

- [ ] **Step 5: Run the test**

Run: `cargo run -p tools -- build-kernel && cargo run -p tools -- build-fs && cargo run -p tools -- test mount_subsystem`
Expected: `MOUNTTEST_MOUNT_EXT4_RC OK`, `MOUNTTEST_EXT4_READ OK`, `MOUNTTEST_UMOUNT_RC OK`, `MOUNTTEST_GONE_AFTER_UMOUNT OK`, `MOUNTTEST_DONE failures=0` → PASS.

- [ ] **Step 6: Full suite**

Run: `cargo run -p tools -- test`
Expected: 20 pass / 0 fail / 5 skip (19 + `mount_subsystem`).

- [ ] **Step 7: Commit**

```bash
git add crates/kernel/src/fs/mount.rs crates/kernel/src/fs/mod.rs crates/kernel/src/arch/x86_64/syscalls/mod.rs crates/kernel/src/arch/x86_64/syscalls/fsio.rs
git commit -m "feat(kernel): mount table + mount(2)/umount2(2) for ext4/ext2"
```

---

### Task 5: Errno semantics

**Files:**
- Modify: `crates/kernel/src/fs/mount.rs`
- Modify: `crates/kernel/src/arch/x86_64/syscalls/fsio.rs`
- Modify: `initrd/mounttest.c` (errno section)

**Interfaces:**
- Consumes: `sys_mount`/`sys_umount2`, `EINVAL`/`ENODEV`/`ENOTBLK`/`EBUSY`/`ENOENT` (add `ENODEV`/`ENOTBLK` next to the existing `mod.rs:80-100` constants if missing).

- [ ] **Step 1: Add the errno section to `mounttest.c`**

```c
errno = 0; check("ENODEV", mount("/disk2.img", "/mnt", "nosuchfs", 0, NULL) == -1 && errno == ENODEV);
errno = 0; check("EINVAL", umount2("/not-mounted-xyz", 0) == -1 && errno == EINVAL);
errno = 0; check("ENOENT", mount("/disk2.img", "/no/such/dir", "ext4", 0, NULL) == -1 && errno == ENOENT);
/* busy: hold a file open, then unmount */
mount("/disk2.img", "/mnt", "ext4", 0, NULL);
int busy_fd = open("/mnt/hello.txt", O_RDONLY);
errno = 0; check("EBUSY", umount2("/mnt", 0) == -1 && errno == EBUSY);
if (busy_fd >= 0) close(busy_fd);
umount2("/mnt", 0);
```

- [ ] **Step 2: Implement matching kernel behavior**

- `Filesystem::mount` for an unregistered name → `ENODEV` (the lookup in `FILESYSTEMS` finds nothing).
- `mount_remove` of a non-mounted path → `EINVAL`.
- Missing target → `ENOENT` (the `sys_mount` target lookup).
- Busy unmount → `EBUSY`: track an open-reference count per mount. Add `open_count: AtomicUsize` to `Mount`, incremented when an `open` on a vnode under the mount succeeds and decremented on close; `mount_remove` returns `EBUSY` when non-zero. (If refcounting open fds is too invasive for Phase 1, gate `EBUSY` on the presence of a dentry-cached vnode whose mount is this mount and document the approximation — but the test above must pass.)

- [ ] **Step 3: Run the test**

Run: `cargo run -p tools -- build-kernel && cargo run -p tools -- build-fs && cargo run -p tools -- test mount_subsystem`
Expected: all `MOUNTTEST_*` lines `OK`, `failures=0`.

- [ ] **Step 4: Full suite green**

Run: `cargo run -p tools -- test`
Expected: 20 / 0 / 5.

- [ ] **Step 5: Commit**

```bash
git add crates/kernel/src/fs/mount.rs crates/kernel/src/arch/x86_64/syscalls/fsio.rs
git commit -m "feat(mount): Linux-compatible errno for mount/umount2 (ENODEV/EINVAL/ENOENT/EBUSY)"
```

---

### Task 6: Pseudo-filesystems mountable (Phase-1 passthrough)

**Files:**
- Modify: `crates/kernel/src/fs/mount.rs` (register `proc`/`sysfs`/`devtmpfs`/`tmpfs` placeholder filesystems)
- Modify: `crates/kernel/src/fs/mod.rs` (gate the arms on the presence of the corresponding mount)
- Modify: `initrd/mounttest.c` (proc section)

**Interfaces:**
- Produces: `Filesystem` impls for `proc`, `sysfs`, `devtmpfs`, `tmpfs` whose `mount` returns a marker root and whose presence in `MOUNTS` enables the existing arms.

- [ ] **Step 1: Extend `mounttest.c`**

```c
check("PROC_MOUNT", mount("proc", "/proc", "proc", 0, NULL) == 0);
struct stat pst; check("PROC_SELF", stat("/proc/self", &pst) == 0);
```

- [ ] **Step 2: Register passthrough filesystems**

For each of `proc`/`sysfs`/`devtmpfs`/`tmpfs`, `mount` inserts a `Mount` with a placeholder `FileNode` root and the canonical mountpoint; the `lookup` arms for `/proc`, `/sys`, `/dev`, `/tmp` remain the resolver. This keeps current behavior while making `mount -t proc proc /proc` return 0.

- [ ] **Step 3: Boot with the canonical mounts present**

In `fs/mod.rs`, after `mount_root()`, register `/proc` (procfs), `/sys` (sysfs), `/dev` (devtmpfs), `/tmp` (tmpfs) so the arms resolve exactly as they do today.

- [ ] **Step 4: Run**

Run: `cargo run -p tools -- build-kernel && cargo run -p tools -- build-fs && cargo run -p tools -- test mount_subsystem`
Expected: `MOUNTTEST_PROC_MOUNT OK`, `MOUNTTEST_PROC_SELF OK`, `failures=0`.

- [ ] **Step 5: Full suite green**

Run: `cargo run -p tools -- test`
Expected: 20 / 0 / 5 (no regression to `/proc`, `/sys`, `/dev` consumers such as `vfstest`, `zellij`, `libinput`).

- [ ] **Step 6: Commit**

```bash
git add crates/kernel/src/fs/mount.rs crates/kernel/src/fs/mod.rs
git commit -m "feat(mount): register proc/sysfs/devtmpfs/tmpfs as mountable"
```

---

### Task 7: Per-mount `statfs`, `AGENTS.md` §1, `init.sh` expectation

**Files:**
- Modify: `crates/kernel/src/arch/x86_64/syscalls/fsio.rs` (`sys_statfs`/`sys_fstatfs` consult `MOUNTS`)
- Modify: `AGENTS.md` (§1 lock ordering: add `MOUNTS`/`DEVICES`)
- Modify: `scripts/build-fs.sh` (the `init.sh` vfat line + comment)

**Interfaces:**
- Consumes: `mount::resolve_mount`.
- Produces: `sys_statfs` reports the resolved mount's `f_type`.

- [ ] **Step 1: Make `statfs` mount-aware**

Replace the prefix-based magic selection in `sys_statfs` (`fsio.rs:2606`) with `mount::resolve_mount(&canonical).map(|(m, _)| m.fstype)` → magic table (`ext4`→`EXT4_SUPER_MAGIC`, `tmpfs`→`TMPFS_MAGIC`, `proc`→`PROC_SUPER_MAGIC`, `sysfs`→`SYSFS_MAGIC`). Do the same in `sys_fstatfs`.

- [ ] **Step 2: Extend `mounttest.c`**

```c
mount("/disk2.img", "/mnt", "ext4", 0, NULL);
struct statfs sf; check("STATFS_EXT4", statfs("/mnt", &sf) == 0 && sf.f_type == 0xEF53);
umount2("/mnt", 0);
```

- [ ] **Step 3: Update `AGENTS.md` §1**

Add `MOUNTS` and `DEVICES` to the established order as outer, leaf-adjacent locks (never held while `SCHEDULER`/`CPUS[k]`/`process.lock()` is held, never across a scheduling boundary), with the established line pointing at `fs/mount.rs`.

- [ ] **Step 4: Update the `init.sh` vfat expectation**

In `scripts/build-fs.sh`, the generated `init.sh` currently attempts `mount -t vfat /dev/vda1 /efi` and relies on `|| true`. Add a comment recording that vfat is deferred (removable media out of scope for the QEMU target) and that `mount(2)` is now implemented, so the failure is `ENODEV` (no vfat driver), not `ENOSYS`. No behavioral change to the script's control flow.

- [ ] **Step 5: Run**

Run: `cargo run -p tools -- build-kernel && cargo run -p tools -- build-fs && cargo run -p tools -- test mount_subsystem`
Expected: `MOUNTTEST_STATFS_EXT4 OK`, `failures=0`.

- [ ] **Step 6: Full suite + commit**

Run: `cargo run -p tools -- test` → 20 / 0 / 5.

```bash
git add crates/kernel/src/arch/x86_64/syscalls/fsio.rs AGENTS.md scripts/build-fs.sh
git commit -m "feat(mount): per-mount statfs; document MOUNTS lock order; init.sh vfat note"
```

---

## Phase 2 — correctness rewrite

### Task 8: `proc` as a real `FileNode` tree

**Files:**
- Create: `crates/kernel/src/fs/procfs.rs`
- Modify: `crates/kernel/src/fs/mount.rs` (register the real `proc` filesystem)
- Modify: `crates/kernel/src/fs/mod.rs` (drop the `proc/*` arms)
- Modify: `initrd/mounttest.c`

**Interfaces:**
- Produces: `pub struct ProcFs;` impl `Filesystem`; a `FileNode` tree rooted at `/` exposing `self`, `self/fd/N`, `self/exe`, and `mounts`.
- Consumes: the fd-table logic currently in `lookup_relative_to` (`proc/self/fd/*`).

- [ ] **Step 1: Extend the test**

```c
check("PROC_REMOUNT", mount("proc", "/mnt2", "proc", 0, NULL) == 0);
struct stat s2; check("PROC_ELSEWHERE", stat("/mnt2/self", &s2) == 0);
umount2("/mnt2", 0);
```

- [ ] **Step 2: Implement `ProcFs`**

Move the `proc/self`, `proc/self/fd/*`, and `proc/self/exe` resolution into `ProcFs::lookup`, reading the current thread's fd table through the same accessor the arms used. Add a generated `mounts` file listing `MOUNTS`.

- [ ] **Step 3: Delete the `proc/*` arms**

Remove lines 314–351 of `fs/mod.rs`. The `/proc` mount at boot now resolves through the table.

- [ ] **Step 4: Run**

Run: `cargo run -p tools -- build-kernel && cargo run -p tools -- build-fs && cargo run -p tools -- test mount_subsystem && cargo run -p tools -- test`
Expected: `MOUNTTEST_PROC_REMOUNT OK`, `MOUNTTEST_PROC_ELSEWHERE OK`; suite 20 / 0 / 5.

- [ ] **Step 5: Commit**

```bash
git add crates/kernel/src/fs/procfs.rs crates/kernel/src/fs/mount.rs crates/kernel/src/fs/mod.rs
git commit -m "refactor(procfs): real FileNode tree; drop hardcoded /proc arms"
```

---

### Task 9: `sysfs`, `devtmpfs`, `tmpfs` as real trees

**Files:**
- Create: `crates/kernel/src/fs/devfs.rs` (devtmpfs `FileNode` tree over the existing `devinput`/`devdri` providers)
- Modify: `crates/kernel/src/fs/mount.rs`, `crates/kernel/src/fs/mod.rs`, `crates/kernel/src/fs/sysfs.rs`, `crates/kernel/src/fs/tmpfs.rs`
- Modify: `initrd/mounttest.c`

**Interfaces:**
- Produces: `SysFsFs`, `DevTmpFs`, `TmpFs` implementing `Filesystem`, each returning a real `FileNode` root.

- [ ] **Step 1: Extend the test**

```c
check("DEV_REMOUNT", mount("devtmpfs", "/mnt3", "devtmpfs", 0, NULL) == 0);
struct stat d; check("DEV_ELSEWHERE", stat("/mnt3/null", &d) == 0);
umount2("/mnt3", 0);
```

- [ ] **Step 2: Implement each as a `FileNode` tree** and register it.

- [ ] **Step 3: Delete the `dev/*`, `sys/*`, `tmp/*`, `run/*` arms** from `lookup_relative_to`/`create`; boot registers the canonical mounts.

- [ ] **Step 4: Run**

Run: `cargo run -p tools -- build-kernel && cargo run -p tools -- build-fs && cargo run -p tools -- test`
Expected: 20 / 0 / 5, including `vfs_tmpfs_sysfs` and `mount_subsystem`.

- [ ] **Step 5: Commit**

```bash
git add crates/kernel/src/fs crates/kernel/src/fs/mod.rs
git commit -m "refactor(fs): sysfs/devtmpfs/tmpfs as real FileNode trees"
```

---

### Task 10: Mount semantics + delete `ROOT_NODE` special-casing

**Files:**
- Modify: `crates/kernel/src/fs/mount.rs` (stacking, `..` handling)
- Modify: `crates/kernel/src/fs/vfs.rs` (a vnode's parent/mount link if needed)
- Modify: `crates/kernel/src/fs/mod.rs` (final resolution)
- Modify: `initrd/mounttest.c`

**Interfaces:**
- Produces: `Mount` stack per mountpoint; `..` at a mount root resolves to the mountpoint's parent within the *same* mount; a failed read on the backing file yields `EIO`.

- [ ] **Step 1: Add the Review-Focus tests**

```c
/* stacking */
check("STACK1", mount("/disk2.img", "/mnt", "ext4", 0, NULL) == 0);
check("STACK2", mount("/disk2.img", "/mnt", "ext4", 0, NULL) == 0);   /* top wins */
check("STACK_TOP", open("/mnt/hello.txt", O_RDONLY) >= 0);
check("STACK_POP", umount2("/mnt", 0) == 0);                          /* reveals lower mount */
check("STACK_LOWER", open("/mnt/hello.txt", O_RDONLY) >= 0);
umount2("/mnt", 0);
/* .. across a mount stays inside the mount */
mkdir("/mnt", 0755);
mount("/disk2.img", "/mnt", "ext4", 0, NULL);
struct stat up; check("DOTDOT", stat("/mnt/..", &up) == 0);
umount2("/mnt", 0);
/* backing image truncated/disappears -> EIO, not a fault */
```

For the "device disappears" case, add a filesystem variant or a fault-injection flag in `FileReader` (e.g. `DRAGOS_MOUNT_EIO` env parsed at boot) that makes `read_sector` return an error; assert `read()` on `/mnt/hello.txt` returns `-1`/`EIO`.

- [ ] **Step 2: Implement stacking** (a `Vec` of mounts per mountpoint; top-of-stack resolves) and the `..`-within-mount rule.

- [ ] **Step 3: Retire `ROOT_NODE`** as a resolution special-case; `/` is `MOUNTS` entry 0. Keep the symbol only as the mount source for `/`.

- [ ] **Step 4: Run**

Run: `cargo run -p tools -- build-kernel && cargo run -p tools -- build-fs && cargo run -p tools -- test`
Expected: 20 / 0 / 5.

- [ ] **Step 5: Commit**

```bash
git add crates/kernel/src/fs crates/kernel/src/fs/mod.rs
git commit -m "feat(mount): mount stacking, ..-within-mount, EIO on backing failure"
```

---

## Self-Review

**Spec coverage:** mount table (Task 4), `mount`/`umount2` (Task 4), block/file source + reader (Tasks 2–3), errno table (Task 5), pseudo-fs mounting (Task 6), per-mount `statfs` (Task 7), lock-order update (Task 7), Phase 2 real filesystems + arms deleted (Tasks 8–10), vfat deferred (Task 7). All spec sections map to a task.

**Type consistency:** `SectorReader`, `MountSource`, `Mount`, `resolve_mount`, `mount_add`, `mount_remove`, `Filesystem::mount`, `Ext4Fs::open`/`new` are used with the same signatures across Tasks 2–10.

**Review Focus coverage:** stacking (Task 4 boundary, pinned in Task 10), `..` across mounts (Task 10), backing failure → `EIO` (Task 10). Mount-over-existing is exercised from Task 4's duplicate-mount path and formalized in Task 10.

**Proportion:** the plan names interfaces and tests; it does not transcribe the resolution loop or the ext4 reader bodies, which the signatures and tests determine.
