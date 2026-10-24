//! Common BlockDevice trait for storage devices (VirtIO-blk, AHCI SATA, NVMe).

use alloc::boxed::Box;
use crate::arch::sync::spinlock::SpinLock;

pub trait BlockDevice: Send {
    /// Read a single 512-byte sector at the specified LBA.
    fn read_sector(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), &'static str>;

    /// Write a single 512-byte sector at the specified LBA.
    fn write_sector(&mut self, sector: u64, data: &[u8]) -> Result<(), &'static str>;

    /// Read multiple 512-byte sectors starting at `sector`.
    /// `buf.len()` must be a multiple of `sector_size()`.
    fn read_sectors(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), &'static str> {
        let sec_sz = self.sector_size();
        if buf.len() % sec_sz != 0 {
            return Err("buffer size not a multiple of sector size");
        }
        let count = buf.len() / sec_sz;
        for i in 0..count {
            let offset = i * sec_sz;
            self.read_sector(sector + i as u64, &mut buf[offset..offset + sec_sz])?;
        }
        Ok(())
    }

    /// Write multiple 512-byte sectors starting at `sector`.
    /// `data.len()` must be a multiple of `sector_size()`.
    fn write_sectors(&mut self, sector: u64, data: &[u8]) -> Result<(), &'static str> {
        let sec_sz = self.sector_size();
        if data.len() % sec_sz != 0 {
            return Err("data size not a multiple of sector size");
        }
        let count = data.len() / sec_sz;
        for i in 0..count {
            let offset = i * sec_sz;
            self.write_sector(sector + i as u64, &data[offset..offset + sec_sz])?;
        }
        Ok(())
    }

    /// Total capacity in 512-byte sectors.
    fn capacity(&self) -> u64;

    /// Sector size in bytes (defaults to 512).
    fn sector_size(&self) -> usize {
        512
    }
}

/// Global system block device used by Ext2 and VFS.
pub static BLOCK_DEVICE: SpinLock<Option<Box<dyn BlockDevice>>> = SpinLock::new(None);

/// Read a single sector using whichever block device is active (VirtIO-blk, AHCI, NVMe).
pub fn read_sector(sector: u64, buf: &mut [u8]) -> Result<(), &'static str> {
    if let Some(blk) = crate::drivers::virtio::blk::BLK_DEV.lock().as_mut() {
        return blk.read_sector(sector, buf);
    }
    if let Some(blk) = BLOCK_DEVICE.lock().as_mut() {
        return blk.read_sector(sector, buf);
    }
    Err("no block device available")
}

/// Write a single sector using whichever block device is active (VirtIO-blk, AHCI, NVMe).
pub fn write_sector(sector: u64, data: &[u8]) -> Result<(), &'static str> {
    if let Some(blk) = crate::drivers::virtio::blk::BLK_DEV.lock().as_mut() {
        return blk.write_sector(sector, data);
    }
    if let Some(blk) = BLOCK_DEVICE.lock().as_mut() {
        return blk.write_sector(sector, data);
    }
    Err("no block device available")
}

/// Read multiple sectors using whichever block device is active (VirtIO-blk, AHCI, NVMe).
pub fn read_sectors(sector: u64, buf: &mut [u8]) -> Result<(), &'static str> {
    if let Some(blk) = crate::drivers::virtio::blk::BLK_DEV.lock().as_mut() {
        return blk.read_sectors(sector, buf);
    }
    if let Some(blk) = BLOCK_DEVICE.lock().as_mut() {
        return blk.read_sectors(sector, buf);
    }
    Err("no block device available")
}

/// Write multiple sectors using whichever block device is active (VirtIO-blk, AHCI, NVMe).
pub fn write_sectors(sector: u64, data: &[u8]) -> Result<(), &'static str> {
    if let Some(blk) = crate::drivers::virtio::blk::BLK_DEV.lock().as_mut() {
        return blk.write_sectors(sector, data);
    }
    if let Some(blk) = BLOCK_DEVICE.lock().as_mut() {
        return blk.write_sectors(sector, data);
    }
    Err("no block device available")
}

