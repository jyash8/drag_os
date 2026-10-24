//! Mount table and mount/unmount primitives.
//!
//! `MOUNTS` is consulted by path resolution (`fs::mod::lookup_relative_to`)
//! before the legacy hardcoded arms. Every filesystem is a table entry.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::arch::sync::spinlock::SpinLock;
use crate::fs::reader::{FileReader, GlobalReader, SectorReader};
use crate::fs::tmpfs::TmpFsNode;
use crate::fs::vfs::FileNode;

pub struct Mount {
    pub mountpoint: String,
    pub root: Arc<dyn FileNode>,
    pub fstype: &'static str,
    pub source: Option<String>,
    pub flags: u64,
    pub open_count: AtomicUsize,
}

pub struct MountNamespace {
    /// Process-rooted mount list. Initially shared with the global table.
    pub mounts: SpinLock<Vec<Arc<Mount>>>,
    /// Parent namespace used for copy-on-open or pivot_root.
    pub parent: Option<Arc<MountNamespace>>,
}

impl MountNamespace {
    pub fn new_empty() -> Self {
        Self {
            mounts: SpinLock::new(Vec::new()),
            parent: None,
        }
    }
}

/// Global root namespace per tasks process. Per-process binding lands later.
pub static ROOT_MOUNT_NAMESPACE: MountNamespace = MountNamespace {
    mounts: SpinLock::new(Vec::new()),
    parent: None,
};

pub static MOUNTS: SpinLock<Vec<Arc<Mount>>> = SpinLock::new(Vec::new());
pub static ROOT_NS_MOUNTS: SpinLock<Vec<Arc<Mount>>> = SpinLock::new(Vec::new());

pub fn resolve_mount(canonical: &str) -> Option<(Arc<Mount>, String)> {
    let mounts = MOUNTS.lock();
    let mut best: Option<(usize, Arc<Mount>)> = None;
    for m in mounts.iter() {
        let mp = &m.mountpoint;
        // Skip pseudo-filesystems in Phase 1: their resolution is still handled
        // by the legacy arms in fs::mod::lookup. They are tracked in the table
        // for mount()/umount() accounting only.
        if is_pseudo(m.fstype) {
            continue;
        }
        let matches = if mp == "/" {
            true
        } else if canonical == *mp {
            true
        } else {
            canonical.starts_with(mp.as_str())
                && canonical.as_bytes().get(mp.len()) == Some(&b'/')
        };
        if matches {
            let len = mp.len();
            if best.as_ref().map_or(true, |(bl, _)| len > *bl) {
                best = Some((len, Arc::clone(m)));
            }
        }
    }
    drop(mounts);
    best.map(|(_, m)| {
        let rel = if m.mountpoint == "/" {
            canonical.trim_start_matches('/').to_string()
        } else if canonical == m.mountpoint {
            String::new()
        } else {
            canonical[m.mountpoint.len() + 1..].to_string()
        };
        (m, rel)
    })
}

fn is_pseudo(fstype: &str) -> bool {
    matches!(fstype, "proc" | "sysfs" | "devtmpfs" | "tmpfs")
}

pub fn mount_add(m: Mount) -> Result<(), u64> {
    let mut mounts = MOUNTS.lock();
    // stacking: allow multiple mounts on the same point (top wins)
    mounts.push(Arc::new(m));
    Ok(())
}

pub fn mount_remove(mountpoint: &str, _flags: u64) -> Result<(), u64> {
    if mountpoint == "/" {
        return Err(crate::arch::x86_64::syscalls::EINVAL);
    }
    let mut mounts = MOUNTS.lock();
    let idx = mounts.iter().rposition(|m| m.mountpoint == mountpoint);
    match idx {
        Some(i) => {
            if mounts[i].open_count.load(Ordering::Relaxed) > 0 {
                return Err(crate::arch::x86_64::syscalls::EBUSY);
            }
            mounts.remove(i);
            Ok(())
        }
        None => Err(crate::arch::x86_64::syscalls::EINVAL),
    }
}

pub enum MountSource {
    Block,
    File(Arc<dyn FileNode>),
    None,
}

pub trait Filesystem: Send + Sync {
    fn name(&self) -> &'static str;
    fn mount(&self, source: MountSource, flags: u64) -> Result<Arc<dyn FileNode>, u64>;
}

pub static FILESYSTEMS: SpinLock<Vec<Box<dyn Filesystem>>> = SpinLock::new(Vec::new());

pub fn mount_fs(name: &str, source: MountSource, flags: u64) -> Result<Arc<dyn FileNode>, u64> {
    let fs = FILESYSTEMS.lock();
    for f in fs.iter() {
        if f.name() == name {
            return f.mount(source, flags);
        }
    }
    Err(crate::arch::x86_64::syscalls::ENODEV)
}

// ── Filesystem implementations ─────────────────────────────────────────────

pub struct ProcFs;
pub struct SysFs;
pub struct DevTmpFs;
pub struct TmpFs;
pub struct Ext4Filesystem;
pub struct Ext2Filesystem;
pub struct FuseFilesystem;

impl Filesystem for ProcFs {
    fn name(&self) -> &'static str { "proc" }
    fn mount(&self, _source: MountSource, _flags: u64) -> Result<Arc<dyn FileNode>, u64> {
        Ok(TmpFsNode::new_dir_node())
    }
}

impl Filesystem for SysFs {
    fn name(&self) -> &'static str { "sysfs" }
    fn mount(&self, _source: MountSource, _flags: u64) -> Result<Arc<dyn FileNode>, u64> {
        Ok(TmpFsNode::new_dir_node())
    }
}

impl Filesystem for DevTmpFs {
    fn name(&self) -> &'static str { "devtmpfs" }
    fn mount(&self, _source: MountSource, _flags: u64) -> Result<Arc<dyn FileNode>, u64> {
        Ok(TmpFsNode::new_dir_node())
    }
}

impl Filesystem for TmpFs {
    fn name(&self) -> &'static str { "tmpfs" }
    fn mount(&self, _source: MountSource, _flags: u64) -> Result<Arc<dyn FileNode>, u64> {
        Ok(TmpFsNode::new_dir_node())
    }
}

impl Filesystem for Ext4Filesystem {
    fn name(&self) -> &'static str { "ext4" }
    fn mount(&self, source: MountSource, _flags: u64) -> Result<Arc<dyn FileNode>, u64> {
        let reader = match source {
            MountSource::File(node) => {
                let size = node.size() as u64;
                Arc::new(crate::fs::reader::FileReader::new(node, size))
            }
            MountSource::Block => Arc::new(crate::fs::reader::GlobalReader) as Arc<dyn crate::fs::reader::SectorReader>,
            MountSource::None => return Err(crate::arch::x86_64::syscalls::EINVAL),
        };
        let fs = crate::fs::ext4::Ext4Fs::open(reader).ok_or(22u64)?;
        let root = crate::fs::ext4::Ext4Node::new(fs, crate::fs::ext4::structs::EXT4_INODE_ROOT)
            .map_err(|_| 22u64)?;
        Ok(Arc::new(root))
    }
}

impl Filesystem for Ext2Filesystem {
    fn name(&self) -> &'static str { "ext2" }
    fn mount(&self, source: MountSource, _flags: u64) -> Result<Arc<dyn FileNode>, u64> {
        let reader = match source {
            MountSource::File(node) => {
                let size = node.size() as u64;
                Arc::new(crate::fs::reader::FileReader::new(node, size))
            }
            MountSource::Block => Arc::new(crate::fs::reader::GlobalReader) as Arc<dyn crate::fs::reader::SectorReader>,
            MountSource::None => return Err(crate::arch::x86_64::syscalls::EINVAL),
        };
        let fs = crate::fs::ext2::Ext2Fs::open(reader).ok_or(22u64)?;
        let root = crate::fs::ext2::Ext2Node::new(fs, crate::fs::ext2::structs::EXT2_INODE_ROOT)
            .map_err(|_| 22u64)?;
        Ok(Arc::new(root))
    }
}

impl Filesystem for FuseFilesystem {
    fn name(&self) -> &'static str { "fuse" }
    fn mount(&self, _source: MountSource, _flags: u64) -> Result<Arc<dyn FileNode>, u64> {
        // FUSE mount: returns an empty directory as the filesystem root.
        // The actual FUSE protocol (via /dev/fuse) handles file operations.
        // For now, this is a skeleton that allows `mount -t fuse` to succeed.
        Ok(TmpFsNode::new_dir_node())
    }
}

pub fn register_default_filesystems() {
    let mut fs = FILESYSTEMS.lock();
    fs.push(Box::new(ProcFs));
    fs.push(Box::new(SysFs));
    fs.push(Box::new(DevTmpFs));
    fs.push(Box::new(TmpFs));
    fs.push(Box::new(Ext4Filesystem));
    fs.push(Box::new(Ext2Filesystem));
    fs.push(Box::new(FuseFilesystem));
    fs.push(Box::new(crate::fs::vfat::VfatFilesystem));
}

pub fn register_pseudo_mounts() {
    // Phase 1: register the canonical pseudo-filesystems so `mount`/`umount`
    // accounting works. Resolution still goes through the legacy arms.
    let mounts = [
        ("/proc", "proc"),
        ("/sys", "sysfs"),
        ("/dev", "devtmpfs"),
        ("/tmp", "tmpfs"),
    ];
    for (mp, fs) in mounts {
        let root = TmpFsNode::new_dir_node();
        let m = Mount {
            mountpoint: String::from(mp),
            root,
            fstype: fs,
            source: None,
            flags: 0,
            open_count: AtomicUsize::new(0),
        };
        let _ = mount_add(m);
    }
}
pub fn record_open(path: &str) {
    let canonical = crate::fs::canonicalize_path("/", path);
    if let Some((m, _)) = resolve_mount(&canonical) {
        m.open_count.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn record_close(path: &str) {
    let canonical = crate::fs::canonicalize_path("/", path);
    if let Some((m, _)) = resolve_mount(&canonical) {
        let c = m.open_count.load(Ordering::Relaxed);
        if c > 0 {
            m.open_count.fetch_sub(1, Ordering::Relaxed);
        }
    }
}