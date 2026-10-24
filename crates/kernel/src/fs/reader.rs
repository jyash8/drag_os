//! Sector-level readers for filesystems.
//!
//! A filesystem no longer talks to the one global block device directly; it
//! takes an `Arc<dyn SectorReader>` so the same ext4/ext2 code can run over
//! the virtio-blk device, a loopback file (`FileReader`), or a memory slice.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::sync::spinlock::SpinLock;
use crate::fs::vfs::FileNode;

/// A sector-addressable byte source.
pub trait SectorReader: Send + Sync {
    fn read_sector(&self, sector: u64, buf: &mut [u8]) -> Result<(), &'static str>;
    fn write_sector(&self, sector: u64, data: &[u8]) -> Result<(), &'static str>;
    fn capacity(&self) -> u64;

    fn read_sectors(&self, sector: u64, buf: &mut [u8]) -> Result<(), &'static str> {
        if buf.len() % 512 != 0 {
            return Err("buffer size not a multiple of sector size");
        }
        let count = buf.len() / 512;
        for i in 0..count {
            let off = i * 512;
            self.read_sector(sector + i as u64, &mut buf[off..off + 512])?;
        }
        Ok(())
    }
    fn write_sectors(&self, sector: u64, data: &[u8]) -> Result<(), &'static str> {
        if data.len() % 512 != 0 {
            return Err("data size not a multiple of sector size");
        }
        let count = data.len() / 512;
        for i in 0..count {
            let off = i * 512;
            self.write_sector(sector + i as u64, &data[off..off + 512])?;
        }
        Ok(())
    }
}

/// Routes to whichever block device the kernel booted with (virtio-blk first,
/// then the generic BLOCK_DEVICE). This is what `/` uses.
pub struct GlobalReader;

impl SectorReader for GlobalReader {
    fn read_sector(&self, sector: u64, buf: &mut [u8]) -> Result<(), &'static str> {
        crate::drivers::block::read_sector(sector, buf)
    }
    fn write_sector(&self, sector: u64, data: &[u8]) -> Result<(), &'static str> {
        crate::drivers::block::write_sector(sector, data)
    }
    fn capacity(&self) -> u64 {
        0
    }
}

/// A block device backed by a regular file (loopback). Used to mount a
/// pre-built ext4 image staged as `/disk2.img` without a second disk.
pub struct FileReader {
    node: Arc<dyn FileNode>,
    size: u64,
}

impl FileReader {
    pub fn new(node: Arc<dyn FileNode>, size: u64) -> Self {
        FileReader { node, size }
    }
}

impl SectorReader for FileReader {
    fn read_sector(&self, sector: u64, buf: &mut [u8]) -> Result<(), &'static str> {
        if buf.len() != 512 {
            return Err("bad sector buffer size");
        }
        let off = sector * 512;
        if off + 512 > self.size {
            return Err("read past end of image");
        }
        let n = self.node.read_at(off as usize, buf).map_err(|_| "read failed")?;
        if n != 512 {
            return Err("short read");
        }
        Ok(())
    }
    fn write_sector(&self, _sector: u64, _data: &[u8]) -> Result<(), &'static str> {
        Err("read-only image")
    }
    fn capacity(&self) -> u64 {
        self.size
    }
}