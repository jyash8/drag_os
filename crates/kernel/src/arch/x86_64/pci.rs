// PCI Configuration Space access via I/O ports 0xCF8/0xCFC.
use shared::memory::address::{PhysAddr, VirtAddr};

use crate::memory::mmio::map_device_memory;
use crate::println;

use super::port::Port;

const CONFIG_ADDRESS_PORT: u16 = 0xCF8;
const CONFIG_DATA_PORT: u16 = 0xCFC;
const CONFIG_ENABLE_BIT: u32 = 0x8000_0000;

const REG_VENDOR_ID: u32 = 0x00;
const REG_COMMAND: u32 = 0x04;
const REG_STATUS: u32 = 0x06;
const REG_CLASS_CODE: u32 = 0x08;
const REG_HEADER_TYPE: u32 = 0x0C;
const REG_BAR_0: u32 = 0x10;
const REG_CAP_POINTER: u32 = 0x34;

const HEADER_MULTIFUNCTION: u8 = 0x80;
const STATUS_CAPABILITIES_LIST: u16 = 1 << 4;
const CMD_IO_SPACE: u16 = 1 << 0;
const CMD_MEMORY_SPACE: u16 = 1 << 1;
const CMD_BUS_MASTER: u16 = 1 << 2;
const BAR_IO_SPACE: u32 = 1 << 0;
const BAR_64_BIT: u32 = 1 << 2;
const BAR_ADDR_MASK_32: u32 = 0xFFFF_FFF0;
const DEVICE_ABSENT: u16 = 0xFFFF;

const MAX_BUSES: u8 = 255; // bus index 0..=255
const MAX_DEVICES: u8 = 32;
const MAX_FUNCTIONS: u8 = 8;

#[derive(Clone, Copy, Debug)]
pub struct PciAddress {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

impl PciAddress {
    pub const fn new(bus: u8, device: u8, function: u8) -> Self {
        Self {
            bus,
            device,
            function,
        }
    }

    fn config_offset(&self, register: u32) -> u32 {
        CONFIG_ENABLE_BIT
            | (u32::from(self.bus) << 16)
            | (u32::from(self.device) << 11)
            | (u32::from(self.function) << 8)
            | (register & 0xFC)
    }

    unsafe fn read_u32(&self, register: u32) -> u32 {
        unsafe {
            let mut address = Port::<u32>::new(CONFIG_ADDRESS_PORT);
            let data = Port::<u32>::new(CONFIG_DATA_PORT);
            address.write(self.config_offset(register));
            data.read()
        }
    }

    unsafe fn write_u32(&self, register: u32, value: u32) {
        unsafe {
            let mut address = Port::<u32>::new(CONFIG_ADDRESS_PORT);
            let mut data = Port::<u32>::new(CONFIG_DATA_PORT);
            address.write(self.config_offset(register));
            data.write(value);
        }
    }

    unsafe fn read_u16(&self, register: u32) -> u16 {
        ((unsafe { self.read_u32(register & !3) }) >> ((register & 3) * 8)) as u16
    }

    unsafe fn write_u16(&self, register: u32, value: u16) {
        let shift = (register & 3) * 8;
        let dword = unsafe { self.read_u32(register & !3) };
        let masked = (dword & !(0xFFFFu32 << shift)) | (u32::from(value) << shift);
        unsafe { self.write_u32(register & !3, masked) };
    }

    pub fn vendor_id(&self) -> u16 {
        unsafe { self.read_u16(REG_VENDOR_ID) }
    }

    pub fn device_id(&self) -> u16 {
        unsafe { self.read_u16(REG_VENDOR_ID + 2) }
    }

    /// Returns (base class, subclass, programming interface).
    pub fn class_code(&self) -> (u8, u8, u8) {
        let dword = unsafe { self.read_u32(REG_CLASS_CODE) };
        (
            (dword >> 24) as u8,
            ((dword >> 16) & 0xFF) as u8,
            ((dword >> 8) & 0xFF) as u8,
        )
    }

    pub fn header_type(&self) -> u8 {
        (unsafe { self.read_u32(REG_HEADER_TYPE) } & 0xFF) as u8
    }

    pub fn present(&self) -> bool {
        self.vendor_id() != DEVICE_ABSENT
    }

    pub fn enable_memory_and_master(&self) {
        let command = unsafe { self.read_u16(REG_COMMAND) };
        unsafe { self.write_u16(REG_COMMAND, command | CMD_IO_SPACE | CMD_MEMORY_SPACE | CMD_BUS_MASTER) };
    }

    /// Reads an I/O BAR port base address.
    pub fn read_io_bar(&self, index: u32) -> Option<u16> {
        let register = REG_BAR_0 + index * 4;
        let low = unsafe { self.read_u32(register) };
        if low & BAR_IO_SPACE != 0 {
            Some((low & 0xFFFC) as u16)
        } else {
            None
        }
    }

    /// Reads a BAR as `(physical_base, size)`; `None` for I/O-space BARs.
    pub fn read_bar(&self, index: u32) -> Option<(PhysAddr, usize)> {
        let register = REG_BAR_0 + index * 4;
        let low = unsafe { self.read_u32(register) };
        if low & BAR_IO_SPACE != 0 {
            return None;
        }
        let is_64 = low & BAR_64_BIT != 0;
        let high_orig = if is_64 {
            unsafe { self.read_u32(register + 4) }
        } else {
            0
        };

        // Temporarily disable memory/IO decoding while sizing the BAR to prevent
        // transient intermediate 64-bit addresses (e.g. 0xFFFFFFFF_00000000)
        // from being mapped by QEMU/KVM.
        let orig_cmd = unsafe { self.read_u16(REG_COMMAND) };
        if orig_cmd & (CMD_IO_SPACE | CMD_MEMORY_SPACE) != 0 {
            unsafe { self.write_u16(REG_COMMAND, orig_cmd & !(CMD_IO_SPACE | CMD_MEMORY_SPACE)) };
        }

        unsafe { self.write_u32(register, 0xFFFF_FFFF) };
        if is_64 {
            unsafe { self.write_u32(register + 4, 0xFFFF_FFFF) };
        }
        let size_low = unsafe { self.read_u32(register) };
        let size_high = if is_64 {
            unsafe { self.read_u32(register + 4) }
        } else {
            0
        };

        // Restore original BAR values before restoring command register
        if is_64 {
            unsafe { self.write_u32(register + 4, high_orig) };
        }
        unsafe { self.write_u32(register, low) };

        if orig_cmd & (CMD_IO_SPACE | CMD_MEMORY_SPACE) != 0 {
            unsafe { self.write_u16(REG_COMMAND, orig_cmd) };
        }

        let size = if is_64 {
            (!(u64::from(size_high) << 32 | u64::from(size_low & BAR_ADDR_MASK_32)) + 1) as usize
        } else {
            (!(size_low & BAR_ADDR_MASK_32) + 1) as usize
        };

        let base = if is_64 {
            (u64::from(high_orig) << 32 | u64::from(low & BAR_ADDR_MASK_32)) as usize
        } else {
            (low & BAR_ADDR_MASK_32) as usize
        };

        Some((PhysAddr::new(base as u64), size))
    }

    /// Enables the device and maps a memory BAR into the kernel MMIO window.
    pub fn map_bar(&self, index: u32) -> Result<(VirtAddr, usize), &'static str> {
        let (base, size) = self.read_bar(index).ok_or("BAR is I/O space")?;
        self.enable_memory_and_master();
        let virt = map_device_memory(base, size).map_err(|_| "no MMIO window space")?;
        Ok((virt, size))
    }

    /// First capability entry matching `capability_id`, or `None`.
    pub fn find_capability(&self, capability_id: u8) -> Option<u8> {
        if unsafe { self.read_u16(REG_STATUS) } & STATUS_CAPABILITIES_LIST == 0 {
            return None;
        }
        let mut offset = (unsafe { self.read_u32(REG_CAP_POINTER) } & 0xFF) as u8;
        while offset != 0 {
            let dword = unsafe { self.read_u32(u32::from(offset) & !3) };
            let shift = (u32::from(offset) & 3) * 8;
            if (dword >> shift) as u8 == capability_id {
                return Some(offset);
            }
            offset = (dword >> (shift + 8)) as u8;
        }
        None
    }

    pub unsafe fn read_config_u8(&self, register: u32) -> u8 {
        ((unsafe { self.read_u32(register & !3) }) >> ((register & 3) * 8)) as u8
    }

    pub unsafe fn read_config_u16(&self, register: u32) -> u16 {
        unsafe { self.read_u16(register) }
    }

    pub unsafe fn read_config_u32(&self, register: u32) -> u32 {
        unsafe { self.read_u32(register) }
    }

    pub unsafe fn write_config_u16(&self, register: u32, value: u16) {
        unsafe { self.write_u16(register, value) }
    }

    pub unsafe fn write_config_u32(&self, register: u32, value: u32) {
        unsafe { self.write_u32(register, value) }
    }
}

pub fn find_device(vendor: u16, device: u16) -> Option<PciAddress> {
    for bus in 0..=MAX_BUSES {
        for slot in 0..MAX_DEVICES {
            for function in 0..MAX_FUNCTIONS {
                let addr = PciAddress::new(bus, slot, function);
                if addr.vendor_id() == vendor && addr.device_id() == device {
                    return Some(addr);
                }
            }
        }
    }
    None
}

pub fn scan() {
    for bus in 0..=MAX_BUSES {
        for device in 0..MAX_DEVICES {
            let addr = PciAddress::new(bus, device, 0);
            if !addr.present() {
                continue;
            }
            let functions = if addr.header_type() & HEADER_MULTIFUNCTION != 0 {
                MAX_FUNCTIONS
            } else {
                1
            };
            for function in 0..functions {
                let f = PciAddress::new(bus, device, function);
                if !f.present() {
                    continue;
                }
                let (base, sub, prog_if) = f.class_code();
                println!(
                    "PCI {:02x}:{:02x}.{} {:04x}:{:04x} class {:02x}{:02x}{:02x}",
                    bus, device, function,
                    f.vendor_id(), f.device_id(),
                    base, sub, prog_if,
                );

                if (f.vendor_id() == 0x8086 && f.device_id() == 0x2415) || (base == 0x04 && sub == 0x01) {
                    if let Some(ac97) = crate::drivers::audio::ac97::Ac97Driver::init_pci(bus, device, function) {
                        *crate::drivers::audio::ac97::GLOBAL_AC97.lock() = Some(ac97);
                        crate::drivers::audio::ac97::AC97_PRESENT.store(true, core::sync::atomic::Ordering::Relaxed);
                    }
                }

                // Intel HD Audio (class 0x04, subclass 0x03)
                if base == 0x04 && sub == 0x03 {
                    if let Some(hda) = crate::drivers::audio::hda::HdaDriver::init_pci(bus, device, function) {
                        *crate::drivers::audio::hda::GLOBAL_HDA.lock() = Some(hda);
                        crate::drivers::audio::hda::HDA_PRESENT.store(true, core::sync::atomic::Ordering::Relaxed);
                    }
                }

                if base == 0x01 && sub == 0x06 {
                    if let Some(mut ahci) = crate::drivers::ahci::AhciDriver::init_pci(&f) {
                        if !ahci.ports.is_empty() {
                            let port = ahci.ports.remove(0);
                            let mut global_blk = crate::drivers::block::BLOCK_DEVICE.lock();
                            if global_blk.is_none() {
                                *global_blk = Some(alloc::boxed::Box::new(port));
                            }
                        }
                    }
                }

                if base == 0x01 && sub == 0x08 {
                    if let Some(nvme) = crate::drivers::nvme::NvmeDriver::init_pci(&f) {
                        let mut global_blk = crate::drivers::block::BLOCK_DEVICE.lock();
                        if global_blk.is_none() {
                            *global_blk = Some(alloc::boxed::Box::new(nvme));
                        }
                    }
                }

                // Detect VirtIO-GPU (vendor 0x1AF4, device 0x1050)
                if f.vendor_id() == 0x1AF4 && f.device_id() == 0x1050 {
                    if crate::drivers::virtio::gpu::init() {
                        crate::tprintln!("VirtIO-GPU detected via PCI");
                    }
                }

                // Detect VirtIO-net (transitional 0x1000 / modern 0x1041).
                if f.vendor_id() == 0x1AF4
                    && (f.device_id() == 0x1000 || f.device_id() == 0x1041)
                {
                    crate::drivers::virtio::net::init();
                }

                // Detect VirtIO-blk (transitional 0x1001 / modern 0x1042).
                if f.vendor_id() == 0x1AF4
                    && (f.device_id() == 0x1001 || f.device_id() == 0x1042)
                    && crate::drivers::virtio::blk::BLK_DEV.lock().is_none()
                {
                    if let Some(blk) = crate::drivers::virtio::blk::VirtioBlk::init() {
                        crate::dprintln!("[virtio-blk] init ok cap={} sectors", blk.capacity());
                        *crate::drivers::virtio::blk::BLK_DEV.lock() = Some(blk);
                    } else {
                        crate::dprintln!("[virtio-blk] init FAILED");
                    }
                }

                // Detect xHCI USB host controllers (class 0C/03/prog-if 30).
                if base == 0x0C && sub == 0x03 && prog_if == 0x30 {
                    if crate::drivers::usb::xhci::init() {
                        crate::tprintln!("xHCI (USB) controller initialised");
                    }
                }
            }
        }
    }
}

pub fn init() {
    println!("PCI: scanning");
    scan();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test_case]
    fn config_offset_bit_layout() {
        let a = PciAddress::new(0, 0, 0);
        assert_eq!(a.config_offset(0x00), 0x8000_0000);
        assert_eq!(a.config_offset(0x10), 0x8000_0010);
        assert_eq!(a.config_offset(0x34), 0x8000_0034);
        assert_eq!(PciAddress::new(1, 2, 3).config_offset(0), 0x8001_1300);
    }
}
