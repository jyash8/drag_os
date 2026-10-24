//! Synthetic in-kernel `/dev/dri` directory listing.
//!
//! The card0/renderD128 device nodes themselves are synthesized by path in
//! `fs::lookup_relative_to` (they carry per-node minors for libdrm matching).
//! This module only provides the *directory listing* (`card0`, `renderD128`
//! as DT_CHR) so `opendir`/`readdir` discovery (libdrm `drmGetDevices2`,
//! Mesa Venus) finds them. File opens keep resolving through the exact-match
//! arms, never through `DriDir::lookup`.

#![allow(unused)]

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::arch::x86_64::syscalls::{Stat, EINVAL, EISDIR, ENOENT, ENOTDIR};
use crate::fs::dirent::{DT_CHR, DT_DIR};
use crate::fs::vfs::FileNode;

/// The `/dev/dri` synthetic directory node.
pub struct DriDir;

impl FileNode for DriDir {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize, u64> {
        Err(EISDIR)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize, u64> {
        Err(EISDIR)
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn FileNode>, u64> {
        if name == "." || name == ".." {
            return Ok(Arc::new(DriDir));
        }
        // ponytail: leaves resolve via the fs exact-match arms (which set the
        // per-node minor); the dir only needs to *list* them.
        Err(ENOENT)
    }

    fn create(&self, _name: &str, _is_dir: bool, _mode: u32, _uid: u32, _gid: u32) -> Result<Arc<dyn FileNode>, u64> {
        Err(0)
    }

    fn truncate(&self, _size: usize) -> Result<(), u64> {
        Err(0)
    }

    fn stat(&self) -> Stat {
        let mut st = Stat::default();
        st.st_mode = 0o040755; // S_IFDIR | 0755
        st.st_nlink = 2;
        st.st_size = 4096;
        st.st_blksize = 4096;
        st.st_ino = 13100;
        st.st_dev = 1;
        st
    }

    fn is_dir(&self) -> bool {
        true
    }

    fn size(&self) -> usize {
        0
    }

    fn list_dir(&self) -> Result<Vec<(String, bool)>, u64> {
        Ok(alloc::vec![
            (String::from("."), true),
            (String::from(".."), true),
            (String::from("card0"), false),
            (String::from("renderD128"), false),
        ])
    }

    fn list_dir_types(&self) -> Result<Vec<(String, u8)>, u64> {
        Ok(alloc::vec![
            (String::from("."), DT_DIR),
            (String::from(".."), DT_DIR),
            (String::from("card0"), DT_CHR),
            (String::from("renderD128"), DT_CHR),
        ])
    }
}
