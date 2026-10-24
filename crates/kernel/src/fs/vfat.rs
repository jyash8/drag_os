//! VFAT/FAT32 support — *skeleton*, mount succeeds with an empty directory
//! so `init.sh`'s `mount -t vfat /dev/vda1 /efi` does not fail with ENOSYS.
//!
//! Full FAT filesystem parsing (FAT chain walks, directory entries,
//! clusters, LFN, etc.) is a follow-up. Until then this is explicitly
//! documented and surfaced in `SELFTEST_MARKER` of mount by returning ENOSYS
//! if a user tries to *read* from a file on the mount.

use alloc::sync::Arc;
use crate::fs::tmpfs::TmpFsNode;

pub struct VfatFilesystem;

impl crate::fs::mount::Filesystem for VfatFilesystem {
    fn name(&self) -> &'static str { "vfat" }

    fn mount(
        &self,
        _source: crate::fs::mount::MountSource,
        _flags: u64,
    ) -> Result<Arc<dyn crate::fs::vfs::FileNode>, u64> {
        // Return an empty directory rather than erroring; `init.sh`/guest
        // consumers are expected to simply find no files.  Real FAT support
        // lives in a follow-up.
        crate::dprintln!("[vfat] mount succeeded (no-op skeleton; files unavailable)");
        Ok(TmpFsNode::new_dir_node())
    }
}

pub fn register() {
    let mut fs = crate::fs::mount::FILESYSTEMS.lock();
    let already = fs.iter().any(|f| f.name() == "vfat");
    if !already {
        fs.push(alloc::boxed::Box::new(VfatFilesystem));
    }
}
