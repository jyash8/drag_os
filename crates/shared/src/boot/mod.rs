use crate::{
    boot::boot_info::BootInfo,
    memory::address::{PAGE_SIZE, PhysAddr},
};

pub mod boot_info;

pub type KernelEntry = unsafe extern "sysv64" fn(boot_info: *const BootInfo) -> !;

/// Fixed, architecturally-defined device ranges that UEFI memory maps rarely
/// report. Mapped into HHDM with uncached semantics so any phys below
/// `0x1_0000_0000` is reachable as `HHDM_BASE + phys` for future drivers.
/// Each entry is `(start, length)`.
pub const MMIO_DEVICE_REGIONS: &[(u64, u64)] = &[
    (0x000A_0000, 0x0002_0000), // legacy VGA
    (0xFEC0_0000, 0x0001_0000), // IO-APIC
    (0xFED0_0000, 0x0000_1000), // HPET
    (0xFEE0_0000, 0x0010_0000), // local APIC
    (0xFF00_0000, 0x0100_0000), // top of the 32-bit device hole
];

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MemoryRegion {
    pub start: PhysAddr,
    pub length: u64,
    pub kind: MemoryRegionKind,
}

impl MemoryRegion {
    pub fn page_count(&self) -> usize {
        debug_assert!(self.length % PAGE_SIZE == 0);
        (self.length / PAGE_SIZE) as usize
    }
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MemoryRegionKind {
    Usable,
    Reserved,
    AcpiReclaim,
    AcpiNvs,
    Mmio,
    BadMemory,
    Bootloader,
    Kernel,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_count_uses_length() {
        let r = MemoryRegion {
            start: PhysAddr(0x1000),
            length: 4096 * 3,
            kind: MemoryRegionKind::Usable,
        };
        assert_eq!(r.page_count(), 3);
    }

    #[test]
    fn mmio_regions_are_nonempty() {
        assert!(!MMIO_DEVICE_REGIONS.is_empty());
        // All regions must have nonzero length.
        for (start, len) in MMIO_DEVICE_REGIONS {
            assert!(*len > 0);
            assert!(*start > 0);
        }
        // The list must be present in ascending address order.
        for w in MMIO_DEVICE_REGIONS.windows(2) {
            assert!(w[0].0 < w[1].0);
        }
    }

    #[test]
    fn region_kinds_are_distinguishable() {
        assert_ne!(MemoryRegionKind::Usable, MemoryRegionKind::Reserved);
        assert_ne!(MemoryRegionKind::Mmio, MemoryRegionKind::AcpiReclaim);
    }
}
