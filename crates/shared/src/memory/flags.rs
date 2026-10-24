use core::ops::{BitAnd, BitAndAssign, BitOr, BitOrAssign, Not};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct PageTableFlags(u64);

impl PageTableFlags {
    pub const NONE: Self = Self(0);
    pub const PRESENT: Self = Self(1 << 0);
    pub const WRITABLE: Self = Self(1 << 1);
    pub const USER: Self = Self(1 << 2);
    pub const WRITE_THROUGH: Self = Self(1 << 3);
    pub const CACHE_DISABLE: Self = Self(1 << 4);
    pub const ACCESSED: Self = Self(1 << 5);
    pub const DIRTY: Self = Self(1 << 6);
    pub const HUGE_PAGE: Self = Self(1 << 7);
    pub const GLOBAL: Self = Self(1 << 8);

    /// Execute Disable (NX)
    pub const NO_EXECUTE: Self = Self(1 << 63);

    /// not considered by hardware
    pub const COW: Self = Self(1 << 9);
    pub const DEMAND_PAGING: Self = Self(1 << 10);

    /// Page owned by the DSO cache; its frame is refcounted, not COW-accounted.
    pub const FILE_CACHE: Self = Self(1 << 11);
    // ponytail: MMIO must live above the 4K address field (bits 12-51); bit 12
    // used to be used and silently shifted every MMIO mapping by one page.
    pub const MMIO: Self = Self(1 << 52);

    pub const fn bits(self) -> u64 {
        self.0
    }

    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    pub fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }
}

impl BitOr for PageTableFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for PageTableFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for PageTableFlags {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

impl BitAndAssign for PageTableFlags {
    fn bitand_assign(&mut self, rhs: Self) {
        self.0 &= rhs.0;
    }
}

impl Not for PageTableFlags {
    type Output = Self;

    fn not(self) -> Self {
        Self(!self.0)
    }
}

impl Default for PageTableFlags {
    fn default() -> Self {
        Self::NONE
    }
}

/// Convert user prot bits (PROT_READ=1, PROT_WRITE=2, PROT_EXEC=4) to page flags.
/// PRESENT is intentionally omitted: demand-paging maps add it on fault.
pub fn prot_to_flags(prot: u64) -> PageTableFlags {
    let mut f = PageTableFlags::USER;
    if (prot & 2) != 0 {
        f |= PageTableFlags::WRITABLE;
    }
    if (prot & 4) != 0 {
        // Allow execute: ensure NO_EXECUTE is not set
        f &= !PageTableFlags::NO_EXECUTE;
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_identities() {
        assert_eq!(PageTableFlags::PRESENT.bits(), 1 << 0);
        assert_eq!(PageTableFlags::WRITABLE.bits(), 1 << 1);
        assert_eq!(PageTableFlags::USER.bits(), 1 << 2);
        assert_eq!(PageTableFlags::HUGE_PAGE.bits(), 1 << 7);
        assert_eq!(PageTableFlags::NO_EXECUTE.bits(), 1 << 63);
        assert_eq!(PageTableFlags::MMIO.bits(), 1 << 52);
    }

    #[test]
    fn contains_and_bitops() {
        let f = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
        assert!(f.contains(PageTableFlags::PRESENT));
        assert!(f.contains(PageTableFlags::WRITABLE));
        assert!(!f.contains(PageTableFlags::USER));
        assert_eq!(f & PageTableFlags::PRESENT, PageTableFlags::PRESENT);
    }

    #[test]
    fn insert_remove() {
        let mut f = PageTableFlags::NONE;
        f.insert(PageTableFlags::USER);
        assert!(f.contains(PageTableFlags::USER));
        f.remove(PageTableFlags::USER);
        assert!(!f.contains(PageTableFlags::USER));
    }

    #[test]
    fn not_flips_all_bits() {
        let f = !PageTableFlags::NONE;
        assert!(f.contains(PageTableFlags::PRESENT));
        assert!(f.contains(PageTableFlags::NO_EXECUTE));
    }

    #[test]
    fn prot_to_flags_maps_write_only() {
        assert_eq!(prot_to_flags(1), PageTableFlags::USER); // PROT_READ
        assert_eq!(
            prot_to_flags(2),
            PageTableFlags::USER | PageTableFlags::WRITABLE
        ); // PROT_WRITE
        assert_eq!(prot_to_flags(4), PageTableFlags::USER); // PROT_EXEC
        assert!(!prot_to_flags(1).contains(PageTableFlags::PRESENT)); // PRESENT deferred to fault
    }
}
