use crate::memory::{
    address::{PAGE_SIZE, PhysAddr, VirtAddr},
    flags::PageTableFlags,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysPage {
    start: PhysAddr,
}

impl PhysPage {
    pub fn containing(addr: PhysAddr) -> Self {
        Self {
            start: addr.align_down(PAGE_SIZE),
        }
    }
    pub fn start(self) -> PhysAddr {
        self.start
    }
    pub fn next(self) -> PhysPage {
        Self {
            start: self.start + PAGE_SIZE,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VirtPage {
    start: VirtAddr,
}

impl VirtPage {
    pub fn containing(addr: VirtAddr) -> Self {
        Self {
            start: addr.align_down(PAGE_SIZE),
        }
    }
    pub const fn start(self) -> VirtAddr {
        self.start
    }

    pub fn next(self) -> VirtPage {
        Self {
            start: self.start + PAGE_SIZE,
        }
    }
}

#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct PageTableEntry(u64);

impl PageTableEntry {
    pub const ADDRESS_MASK: u64 = 0x000F_FFFF_FFFF_F000;
    /// Bits that are valid in hardware x86-64 PTE (bits 52-62 are reserved)
    /// Bit 0: Present, 1: Write, 2: User, 3: Write-through, 4: Cache disable
    /// Bit 5: Accessed, 6: Dirty, 7: Page Size, 8: Global
    /// Bits 9-11: Available to software
    /// Bits 12-51: Physical address (handled by ADDRESS_MASK)
    /// Bits 52-62: RESERVED (must be 0)
    /// Bit 63: NX (Execute Disable)
    pub const VALID_HARDWARE_BITS: u64 = 0x8000_0000_0000_0FFF;

    pub const fn new() -> Self {
        Self(0)
    }

    pub const fn address(self) -> PhysAddr {
        PhysAddr(self.0 & Self::ADDRESS_MASK)
    }

    pub fn set_address(&mut self, addr: PhysAddr) {
        self.0 &= !Self::ADDRESS_MASK;
        self.0 |= addr.0 & Self::ADDRESS_MASK;
    }

    pub fn set_flags(&mut self, flags: PageTableFlags) {
        self.0 |= flags.bits() & Self::VALID_HARDWARE_BITS;
    }
    pub fn clear_flags(&mut self, flags: PageTableFlags) {
        self.0 &= !(flags.bits() & Self::VALID_HARDWARE_BITS);
    }
    pub fn flags(self) -> PageTableFlags {
        PageTableFlags::from_bits(self.0 & !Self::ADDRESS_MASK)
    }

    pub fn is_present(self) -> bool {
        self.flags().contains(PageTableFlags::PRESENT)
    }

    /// Clears the entire entry (address and flags). Used by unmap paths;
    /// `set_flags(PageTableFlags::NONE)` does NOT work because `set_flags` only ORs.
    pub fn clear(&mut self) {
        self.0 = 0;
    }
}

#[repr(C, align(4096))]
pub struct PageTable {
    entries: [PageTableEntry; 512],
}

impl PageTable {
    pub fn new() -> Self {
        Self {
            entries: [PageTableEntry::new(); 512],
        }
    }

    pub fn zero(&mut self) {
        unsafe {
            core::ptr::write_bytes(
                self as *mut Self as *mut u8,
                0,
                core::mem::size_of::<Self>(),
            );
        }
    }

    pub fn entry(&self, index: usize) -> &PageTableEntry {
        &self.entries[index]
    }
    pub fn entry_mut(&mut self, index: usize) -> &mut PageTableEntry {
        &mut self.entries[index]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::address::{PAGE_SIZE, PhysAddr, VirtAddr};
    use crate::memory::flags::PageTableFlags;

    #[test]
    fn page_table_layout() {
        assert_eq!(core::mem::size_of::<PageTable>(), 4096);
        assert_eq!(core::mem::align_of::<PageTable>(), 4096);
    }

    #[test]
    fn phys_page_roundtrip() {
        let page = PhysPage::containing(PhysAddr(0x1234));
        assert_eq!(page.start(), PhysAddr(0x1000));
        assert_eq!(page.next().start(), PhysAddr(0x2000));
    }

    #[test]
    fn virt_page_roundtrip() {
        let page = VirtPage::containing(VirtAddr(0xFFFF_8000_0000_1234));
        assert_eq!(page.start(), VirtAddr(0xFFFF_8000_0000_1000));
        assert_eq!(page.next().start(), VirtAddr(0xFFFF_8000_0000_2000));
    }

    #[test]
    fn entry_address_masked_to_page() {
        let mut e = PageTableEntry::new();
        e.set_address(PhysAddr(0x1234));
        assert_eq!(e.address(), PhysAddr(0x1000));
        e.set_address(PhysAddr(0xFFFF_0000_0000));
        assert_eq!(e.address(), PhysAddr(0xFFFF_0000_0000));
    }

    #[test]
    fn entry_flags_preserve_custom_bits() {
        let mut e = PageTableEntry::new();
        e.set_address(PhysAddr(0x3000));
        e.set_flags(PageTableFlags::PRESENT | PageTableFlags::DEMAND_PAGING);
        assert!(e.is_present());
        assert!(e.flags().contains(PageTableFlags::DEMAND_PAGING));
        assert_eq!(e.address(), PhysAddr(0x3000));
    }

    #[test]
    fn cleared_entry_not_present() {
        let mut e = PageTableEntry::new();
        e.set_flags(PageTableFlags::PRESENT);
        assert!(e.is_present());
        e.clear_flags(PageTableFlags::PRESENT);
        assert!(!e.is_present());
    }

    #[test]
    fn zeroed_table_gives_not_present_entries() {
        let mut table = PageTable::new();
        table.zero();
        for i in 0..512 {
            assert!(!table.entry(i).is_present());
        }
    }

    #[test]
    fn page_constants() {
        assert_eq!(PAGE_SIZE, 4096);
        assert_eq!(core::mem::size_of::<PageTableEntry>(), 8);
    }

    #[test]
    fn valid_hardware_bits_excludes_reserved() {
        let mut e = PageTableEntry::new();
        e.set_address(PhysAddr(0x1000));
        e.set_flags(PageTableFlags::MMIO);
        // MMIO (bit 52) is reserved in hardware, should be masked out
        assert_eq!(e.0 & (1 << 52), 0, "MMIO bit 52 should not be in hardware PTE");
    }

    #[test]
    fn valid_hardware_bits_includes_no_execute() {
        let mut e = PageTableEntry::new();
        e.set_address(PhysAddr(0x1000));
        e.set_flags(PageTableFlags::NO_EXECUTE);
        // NO_EXECUTE (bit 63) is valid in hardware
        assert_eq!(e.0 & (1 << 63), 1 << 63, "NO_EXECUTE bit 63 should be in hardware PTE");
    }
}
