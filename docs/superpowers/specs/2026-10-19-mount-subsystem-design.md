# Mount Subsystem Design (`mount(2)`, `umount2(2)`, mount table)

**Status:** design (awaiting review)
**Date:** 2026-10-19
**Sub-project:** 1 of the Desktop Usability program
**Roadmap:** `2026-10-19-desktop-usability-roadmap-design.md`

---

## Goal

Give DragOS a real mount table and the `mount(2)`/`umount2(2)` syscalls, so that
`/proc`, `/sys` and `/dev` are genuinely mounted, a second disk (or a disk image)
can be mounted, and user programs can mount and unmount filesystems themselves.

## Why this is architectural

The kernel today has **no mount table**. `crates/kernel/src/fs/mod.rs::lookup`
(line 148) is a hand-written path-prefix dispatcher: `dev/*` (line 169–312),
`proc/*` (314–351), `sys/*` (353–359), `tmp/*` (361–366), `run/*` (368–380), then
the single root (`ROOT_NODE`, line 26) or the TAR initrd. `mount_root()` (line
86) mounts exactly one filesystem; `MOUNTED_EXT4`/`MOUNTED_EXT2` (38–39) are
single globals. `mount(2)` (165) and `umount2(2)` (166) have **no arm** in
`crates/kernel/src/arch/x86_64/syscalls/mod.rs`, so `mount(2)` returns `ENOSYS`.
`statfs`/`fstatfs` (137/138, `mod.rs:727-728`) describe the one root only.

Adding `mount(2)` therefore means adding a mount table — a new subsystem layered
under every path resolution. The `AGENTS.md` invariant #0 (determinism) and #10
say every path must be a traceable trail, so the table must be introduced
without turning `lookup` into a larger pile of special cases.

## Decision: two phases

Chosen deliberately (per review): **incremental first, then rewritten for
correctness.**

- **Phase 1** adds a `MOUNTS` table that `lookup`/`create`/`statfs` consult
  *before* the existing arms, plus `mount`/`umount2` and a block-device layer.
  The existing arms stay as the fallback. This gets mounting working and
  testable quickly, with a small blast radius.
- **Phase 2** converts the synthetic arms into real `FileNode`-implementing
  filesystems registered in the table, makes resolution fully table-driven, and
  deletes the prefix arms.

**Phase 1 must not calcify:** no new hardcoded arm may be added to `lookup`.
Every new filesystem is a table entry with a `mount` entry point. Phase 2 is
what removes the remaining arms.

## Existing interfaces this builds on (verified)

```rust
// crates/kernel/src/fs/vfs.rs:32
pub trait FileNode: Send + Sync {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize, u64>;
    fn write_at(&self, offset: usize, buf: &[u8]) -> Result<usize, u64>;
    fn lookup(&self, name: &str) -> Result<Arc<dyn FileNode>, u64>;
    fn create(&self, name: &str, is_dir: bool) -> Result<Arc<dyn FileNode>, u64>;
    fn stat(&self) -> Stat;
    fn read_link(&self) -> Result<String, u64> { Err(EINVAL) }
}

// vfs.rs:113
pub enum VNode { /* ... */ FsNode { node: Arc<dyn FileNode>, cursor: usize }, /* ... */ }

// crates/kernel/src/drivers/block.rs:6
pub trait BlockDevice: Send {
    fn read_sector(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), &'static str>;
    fn write_sector(&mut self, sector: u64, data: &[u8]) -> Result<(), &'static str>;
    // ...
}
pub static BLOCK_DEVICE: SpinLock<Option<Box<dyn BlockDevice>>>; // one global

// crates/kernel/src/fs/ext4/mod.rs:82
impl Ext4Fs { pub fn new() -> Option<Arc<Self>> } // reads the one global device
```

Two facts drive the design:

1. **`BlockDevice` is a single global.** Mounting a second disk requires a
   device *registry* and filesystems that take a device handle, not the global.
2. **`FileNode::read_at`/`write_at` take `&self`.** A block device's
   `read_sector` takes `&mut self`, so a device handle needs interior mutability
   (`Arc<SpinLock<...>>`).

## New interfaces

```rust
// crates/kernel/src/fs/mount.rs  (new)

pub struct Mount {
    pub mountpoint: String,          // canonicalized, no trailing slash
    pub root: Arc<dyn FileNode>,     // filesystem root
    pub fstype: &'static str,        // "proc", "sysfs", "devtmpfs", "tmpfs", "ext4", "ext2"
    pub source: Option<String>,      // "/dev/vda1" or None for pseudo-fs
    pub flags: u64,                  // MS_RDONLY | MS_NOSUID | ...
}

pub static MOUNTS: SpinLock<Vec<Mount>>;

/// Longest-prefix match. Returns the mount and the path *relative to* its root.
pub fn resolve_mount(canonical: &str) -> Option<(Arc<Mount>, String)>;

pub fn mount_add(m: Mount) -> Result<(), u64>;
pub fn mount_remove(mountpoint: &str, flags: u64) -> Result<(), u64>;

/// A mountable filesystem type.
pub trait Filesystem: Send + Sync {
    fn name(&self) -> &'static str;
    /// `source` = a block-device handle, a memory slice, or None for pseudo-fs.
    fn mount(&self, source: MountSource, flags: u64) -> Result<Arc<dyn FileNode>, u64>;
}

pub enum MountSource {
    Block(Arc<dyn SectorReader>),  // "/dev/vda1"
    File(Arc<dyn FileNode>),       // loopback image, e.g. "/disk2.img"
    None,                          // pseudo-fs
}

/// Registry, populated at boot.
pub static FILESYSTEMS: SpinLock<Vec<Box<dyn Filesystem>>>;

// crates/kernel/src/drivers/block.rs (extend)
pub static DEVICES: SpinLock<BTreeMap<String, Arc<dyn SectorReader>>>;
// "/dev/vda" -> whole disk, "/dev/vda1" -> partition view (offset + length)
```

`MountSource::Block` uses a `SectorReader` object, so `FileNode::read_at(&self)`
can call it. The primary test vehicle is `MountSource::File` — a **file-backed
ext4 image** (loopback) staged into the guest — because virtio-blk exposes a
single global device (`drivers/virtio/blk.rs::BLK_DEV`), so a second real disk
would require enumerating a second virtio-blk PCI function. The loopback path is
strictly simpler and exercises the same `Ext4Fs::open(reader)` code.

```rust
// crates/kernel/src/fs/reader.rs (new)
pub trait SectorReader: Send + Sync {
    fn read_sector(&self, sector: u64, buf: &mut [u8]) -> Result<(), &'static str>;
    fn write_sector(&self, sector: u64, data: &[u8]) -> Result<(), &'static str>;
    fn capacity(&self) -> u64;
}
```

## Syscall surface

- `165 mount(source: *const u8, target: *const u8, fstype: *const u8,
  flags: u64, data: *const u8) -> u64`
- `166 umount2(target: *const u8, flags: u64) -> u64`

`sys_mount` resolves `source` — if `fstype` requires a device, `source` is a
path under `/dev` resolved through the normal path layer to a block-device
`VNode` (new); otherwise `source` may be NULL (pseudo-fs) or unused.
`sys_umount2` honors `MNT_FORCE`/`MNT_DETACH` as best-effort; `MNT_DETACH` at
minimum marks the mount lazy.

### Errno semantics (must match Linux; libc/mount(8) probe these)

| Condition | errno |
|---|---|
| Unknown `fstype` | `ENODEV` |
| `source` not a block device where one is required | `ENOTBLK` |
| `target` does not exist | `ENOENT` |
| `target` already a mount point (stacking) | permitted in Phase 1 only if it stacks; otherwise `EBUSY` |
| `umount2` on a non-mount / not the root | `EINVAL` |
| Unmount with busy (open) references | `EBUSY` |
| Bad superblock / unsupported fs | `EINVAL` |
| Non-root caller without `CAP_SYS_ADMIN` | `EPERM` |

Phase 1 may run everything as root (the session work in #3 lands later), but the
`EPERM` check is written now so #3 does not have to revisit it.

## Effect on path resolution

### Phase 1

`lookup_relative_to` (and `create`) gain one step immediately after
canonicalization:

```text
if let Some((mnt, rel)) = resolve_mount(&canonical) {
    // walk `rel` starting from mnt.root, exactly as the ROOT_NODE walk does today
}
// else: existing arms (dev/proc/sys/tmp/run), then ROOT_NODE, then tar
```

`resolve_mount` is longest-prefix over `MOUNTS`. Because the `/proc`, `/sys`,
`/dev` position is unchanged for existing callers (they resolve to the same
nodes through the registered pseudo-fs roots), the existing test suite stays
green.

`statfs`/`fstatfs` consult `resolve_mount` and report that mount's geometry.

### Phase 2

The `dev/*`, `proc/*`, `sys/*`, `tmp/*`, `run/*` arms move into their respective
`Filesystem::mount` implementations as `FileNode` trees. `lookup` becomes:

```text
canonical -> resolve_mount -> walk from root
          -> (no mount) ENOENT
```

The root filesystem is `MOUNTS[0]` at `/`. `ROOT_NODE` and the single
`MOUNTED_EXT4`/`MOUNTED_EXT2` globals are retired (kept only as the mount source
for `/`). `vfat`/FAT is deferred — it is only needed for removeable media, which
is out of scope for the QEMU desktop target; the `init.sh` vfat attempt is
updated to an honest expectation.

## Lock ordering (AGENTS.md §1)

`MOUNTS` is a new spinlock. It is taken during path resolution and released
before any syscall returns; it is **never** held across a scheduling boundary and
never nests with `process.lock()`. The spec fixes its place in the total order
as an **outer, leaf-adjacent** lock — taken before `process.lock()` (never after)
and never while `SCHEDULER` or `CPUS[k]` is held. `AGENTS.md` §1 must be updated
with `MOUNTS` in the same change.

`DEVICES` follows the same rule. `BlockDevice::read_sector` does I/O and must not
be called while holding `MOUNTS` (it can block on hardware); mount resolves the
device, drops the device lock, then reads.

## Testing strategy

- **New guest program `initrd/mounttest.c`** (built into the image like the other
  `*test` programs) asserting, via its own output on serial:
  1. `mount("proc", "/proc", "proc", 0, NULL) == 0`, then `stat("/proc/self")`
     succeeds.
  2. `mount("proc", "/proc", "bogus", 0, NULL) == -1 && errno == ENODEV`.
  3. Mount a pre-built **ext4 image staged as `/disk2.img`** (loopback,
     `MountSource::File`) at `/mnt`, read a sentinel file, verify contents.
  4. `umount2("/mnt", 0) == 0`, then resolution under `/mnt` → `ENOENT`.
  5. Hold a file open under the mount and `umount2` → `EBUSY`.
- **Harness def** added to `crates/tools/src/test_runner.rs` so
  `cargo run -p tools -- test` runs `mounttest` and asserts on its serial
  output (per AGENTS.md §0a, the program's output must exit via `/dev/ttyS0`).
- **`init.sh`**: `mount -t vfat /dev/vda1 /efi` — after Phase 1 this must no
  longer be an expected `ENOSYS`; the honest expectation is documented.
- **Regression:** the full suite must stay at 19 pass / 0 fail / 5 skip.

`Review focus` lines from the roadmap that this spec must pin with tests:
mount-over-existing (roadmap #1), `..` across a mount (#2), device removal →
`EIO` (#3). Each is assigned to a task in the implementation plan.

## Risks

- **`lookup` is on every syscall path.** A bug here fails everything. Mitigated
  by Phase 1's fallback-to-existing-arms and the full-suite gate.
- **`Ext4Fs::new()` is global.** Parameterizing it by device touches ext4 (and
  by symmetry ext2), the largest filesystems. Scoped to Phase 1 step "ext4 over
  a device handle"; `/` keeps using the global until Phase 2.
- **Device registry vs. the single `BLOCK_DEVICE`.** Existing drivers register
  into the one global; the registry must include it as `vda` without breaking
  the current boot path.
- **Blocking I/O under a spinlock.** Explicitly disallowed above; ext4 already
  holds no lock across `read_sector` today and must not start.

## Out of scope

- `bind` mounts, `move` mounts, mount namespaces, and `pivot_root`.
- `vfat`/FAT driver (deferred with removable media).
- `user`/`fusermount` mounts and `CAP_SYS_ADMIN` namespaces.

## Self-review

- **Placeholder scan:** no TBD; every mechanism named and every errno pinned.
- **Consistency:** the two phases share the `Mount`/`mount_add`/`resolve_mount`
  interfaces; Phase 2 only relocates resolution.
- **Scope:** one subsystem, one spec.
- **Ambiguity:** Phase 1 vs Phase 2 boundaries stated as "does a hardcoded arm
  remain in `lookup`?" — Phase 1: yes; Phase 2: no.
