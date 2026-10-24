use core::ptr::{copy_nonoverlapping, read_volatile};

use shared::memory::address::VirtAddr;

use crate::arch::x86_64::pci::find_device;

use super::{
    find_cap, CommonCfg, DmaBuf, VirtQueue, CAP_COMMON_CFG, CAP_DEVICE_CFG, CAP_NOTIFY_CFG, REG_DEVICE_FEATURE,
    REG_DEVICE_FEATURE_SELECT, REG_DEVICE_STATUS, REG_DRIVER_FEATURE, REG_DRIVER_FEATURE_SELECT,
    REG_QUEUE_ENABLE, REG_QUEUE_SELECT, REG_QUEUE_SIZE, STATUS_ACKNOWLEDGE, STATUS_DRIVER, STATUS_DRIVER_OK,
    STATUS_FEATURES_OK, VIRTQ_DESC_F_WRITE, VIRTIO_DEVICE_BLOCK, VIRTIO_DEVICE_BLOCK_LEGACY,
    VIRTIO_VENDOR_ID,
};

const SECTOR_SIZE: u64 = 512;

const BLK_T_IN: u32 = 0;
const BLK_T_OUT: u32 = 1;

#[repr(C)]
struct BlkReqHeader {
    kind: u32,
    reserved: u32,
    sector: u64,
}

pub struct VirtioBlk {
    queue: VirtQueue,
    header_dma: DmaBuf, // request header at +0, status byte at +16
    data_dma: DmaBuf,   // 4096-byte bounce buffer
    capacity: u64,
}

impl VirtioBlk {
    pub fn init() -> Option<Self> {
        let addr = find_device(VIRTIO_VENDOR_ID, VIRTIO_DEVICE_BLOCK)
            .or_else(|| find_device(VIRTIO_VENDOR_ID, VIRTIO_DEVICE_BLOCK_LEGACY))?;

        let common_cap = find_cap(&addr, CAP_COMMON_CFG)?;
        let notify_cap = find_cap(&addr, CAP_NOTIFY_CFG)?;
        let device_cap = find_cap(&addr, CAP_DEVICE_CFG)?;

        // ponytail: transitional QEMU devices spread the structures across
        // several BARs, so map each referenced BAR once and index by bar number.
        let mut bar_virts: [Option<VirtAddr>; 8] = [None; 8];
        let mut get_bar = |i: u8| -> Option<VirtAddr> {
            let idx = i as usize;
            if let Some(v) = bar_virts[idx] {
                return Some(v);
            }
            let (v, _) = addr.map_bar(u32::from(i)).ok()?;
            bar_virts[idx] = Some(v);
            Some(v)
        };

        let common = CommonCfg::new(VirtAddr::new(
            get_bar(common_cap.bar)?.0 + u64::from(common_cap.offset),
        ));
        let notify = VirtAddr::new(get_bar(notify_cap.bar)?.0 + u64::from(notify_cap.offset));
        let device_cfg = VirtAddr::new(get_bar(device_cap.bar)?.0 + u64::from(device_cap.offset));

        unsafe { common.write_u8(REG_DEVICE_STATUS, STATUS_ACKNOWLEDGE) };

        let dev_hi = unsafe {
            common.write_u32(REG_DEVICE_FEATURE_SELECT, 1);
            common.read_u32(REG_DEVICE_FEATURE)
        };
        let _dev_lo = unsafe {
            common.write_u32(REG_DEVICE_FEATURE_SELECT, 0);
            common.read_u32(REG_DEVICE_FEATURE)
        };
        if dev_hi & 1 == 0 {
            return None; // device lacks VIRTIO_F_VERSION_1
        }
        unsafe {
            common.write_u32(REG_DRIVER_FEATURE_SELECT, 1);
            common.write_u32(REG_DRIVER_FEATURE, 1); // accept only VIRTIO_F_VERSION_1
            common.write_u32(REG_DRIVER_FEATURE_SELECT, 0);
            common.write_u32(REG_DRIVER_FEATURE, 0);
        }

        unsafe {
            common.write_u8(
                REG_DEVICE_STATUS,
                STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK,
            );
        }
        let status = unsafe { common.read_u8(REG_DEVICE_STATUS) };
        if status & STATUS_FEATURES_OK == 0 {
            return None;
        }

        unsafe { common.write_u16(REG_QUEUE_SELECT, 0) };
        let _qsize = unsafe { common.read_u16(REG_QUEUE_SIZE) };
        let queue = VirtQueue::new(&common, notify, notify_cap.notify_off_multiplier)?;
        unsafe { common.write_u16(REG_QUEUE_ENABLE, 1) };

        unsafe {
            common.write_u8(
                REG_DEVICE_STATUS,
                STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK,
            );
        }

        let header_dma = DmaBuf::new(4096)?;
        let data_dma = DmaBuf::new(65536)?;

        // Capacity lives at device-config +0; read as two u32s to stay aligned.
        let cap_lo = unsafe { read_volatile(device_cfg.0 as *const u32) };
        let cap_hi = unsafe { read_volatile((device_cfg.0 + 4) as *const u32) };
        let capacity = (u64::from(cap_hi) << 32) | u64::from(cap_lo);

        Some(Self {
            queue,
            header_dma,
            data_dma,
            capacity,
        })
    }

    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    pub fn read_sector(&mut self, sector: u64, out: &mut [u8]) -> Result<(), &'static str> {
        self.read_sectors(sector, out)
    }

    pub fn write_sector(&mut self, sector: u64, data: &[u8]) -> Result<(), &'static str> {
        self.write_sectors(sector, data)
    }

    pub fn read_sectors(&mut self, mut sector: u64, mut out: &mut [u8]) -> Result<(), &'static str> {
        let sec_sz = SECTOR_SIZE as usize;
        if out.len() % sec_sz != 0 {
            return Err("buffer size not a multiple of sector size");
        }
        let total_sectors = (out.len() / sec_sz) as u64;
        if sector + total_sectors > self.capacity {
            return Err("sector out of range");
        }
        let max_chunk = self.data_dma.len();
        while !out.is_empty() {
            let chunk_len = out.len().min(max_chunk);
            let chunk_sectors = (chunk_len / sec_sz) as u64;
            let header = BlkReqHeader {
                kind: BLK_T_IN,
                reserved: 0,
                sector,
            };
            unsafe { (self.header_dma.as_mut_ptr() as *mut BlkReqHeader).write(header) };
            self.io((self.data_dma.phys().0, chunk_len as u32), true)?;
            unsafe {
                copy_nonoverlapping(self.data_dma.as_mut_ptr(), out.as_mut_ptr(), chunk_len);
            }
            sector += chunk_sectors;
            let (_, rest) = core::mem::take(&mut out).split_at_mut(chunk_len);
            out = rest;
        }
        Ok(())
    }

    pub fn write_sectors(&mut self, mut sector: u64, mut data: &[u8]) -> Result<(), &'static str> {
        let sec_sz = SECTOR_SIZE as usize;
        if data.len() % sec_sz != 0 {
            return Err("data size not a multiple of sector size");
        }
        let total_sectors = (data.len() / sec_sz) as u64;
        if sector + total_sectors > self.capacity {
            return Err("sector out of range");
        }
        let max_chunk = self.data_dma.len();
        while !data.is_empty() {
            let chunk_len = data.len().min(max_chunk);
            let chunk_sectors = (chunk_len / sec_sz) as u64;
            unsafe {
                copy_nonoverlapping(data.as_ptr(), self.data_dma.as_mut_ptr(), chunk_len);
            }
            let header = BlkReqHeader {
                kind: BLK_T_OUT,
                reserved: 0,
                sector,
            };
            unsafe { (self.header_dma.as_mut_ptr() as *mut BlkReqHeader).write(header) };
            self.io((self.data_dma.phys().0, chunk_len as u32), false)?;
            sector += chunk_sectors;
            data = &data[chunk_len..];
        }
        Ok(())
    }

    fn io(&mut self, data: (u64, u32), read: bool) -> Result<(), &'static str> {
        let parts = [
            (self.header_dma.phys().0, 16, 0u16),
            (data.0, data.1, if read { VIRTQ_DESC_F_WRITE } else { 0 }),
            (self.header_dma.phys().0 + 16, 1, VIRTQ_DESC_F_WRITE),
        ];
        self.queue.push(&parts)?;
        self.queue.wait()?;
        let status = unsafe { read_volatile((self.header_dma.as_mut_ptr().add(16)) as *const u8) };
        if status != 0 {
            return Err("virtio-blk device status != OK");
        }
        Ok(())
    }
}

impl crate::drivers::block::BlockDevice for VirtioBlk {
    fn read_sector(&mut self, sector: u64, out: &mut [u8]) -> Result<(), &'static str> {
        self.read_sector(sector, out)
    }

    fn write_sector(&mut self, sector: u64, data: &[u8]) -> Result<(), &'static str> {
        self.write_sector(sector, data)
    }

    fn read_sectors(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), &'static str> {
        self.read_sectors(sector, buf)
    }

    fn write_sectors(&mut self, sector: u64, data: &[u8]) -> Result<(), &'static str> {
        self.write_sectors(sector, data)
    }

    fn capacity(&self) -> u64 {
        self.capacity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test_case]
    fn virtio_blk_io() {
        let mut blk = VirtioBlk::init().expect("no virtio-blk device");
        assert!(blk.capacity() > 0, "empty disk geometry");

        let mut buf = [0u8; 512];
        blk.read_sector(0, &mut buf).expect("read failed");
        assert_eq!(&buf[0..12], b"VIRTIOTEST!!");

        buf[..4].copy_from_slice(&[9, 9, 9, 9]);
        blk.write_sector(1, &buf).expect("write failed");
        let mut got = [0u8; 512];
        blk.read_sector(1, &mut got).expect("read failed");
        assert_eq!(&got[..4], &[9, 9, 9, 9]);
    }
}

use crate::arch::sync::spinlock::SpinLock;

pub static BLK_DEV: SpinLock<Option<VirtioBlk>> = SpinLock::new(None);
