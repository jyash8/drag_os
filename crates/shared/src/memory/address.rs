use core::ops::{Add, Sub};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct PhysAddr(pub u64);

impl PhysAddr {
    pub const fn new(addr: u64) -> Self {
        Self(addr)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }

    pub const fn align_down(self, align: u64) -> Self {
        Self(self.0 & !(align - 1))
    }

    pub const fn align_up(self, align: u64) -> Self {
        Self((self.0 + align - 1) & !(align - 1))
    }

    pub const fn is_aligned(self, align: u64) -> bool {
        self.0 & (align - 1) == 0
    }

    pub const fn as_ptr<T>(self) -> *const T {
        self.0 as *const T
    }

    pub const fn as_mut_ptr<T>(self) -> *mut T {
        self.0 as *mut T
    }
}

impl Add<u64> for PhysAddr {
    type Output = Self;
    fn add(self, rhs: u64) -> Self {
        Self(self.0 + rhs)
    }
}

impl Sub<u64> for PhysAddr {
    type Output = Self;
    fn sub(self, rhs: u64) -> Self {
        Self(self.0 - rhs)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct VirtAddr(pub u64);

impl VirtAddr {
    pub const fn new(inner: u64) -> Self {
        Self(inner)
    }
    pub const fn align_down(self, align: u64) -> Self {
        Self(self.0 & !(align - 1))
    }
    pub const fn align_up(self, align: u64) -> Self {
        Self((self.0 + align - 1) & !(align - 1))
    }
    pub const fn is_aligned(self, align: u64) -> bool {
        self.0 & (align - 1) == 0
    }

    pub const fn pml4_index(self) -> usize {
        ((self.0 >> 39) & 0x1FF) as usize
    }

    pub const fn pdpt_index(self) -> usize {
        ((self.0 >> 30) & 0x1FF) as usize
    }

    pub const fn pd_index(self) -> usize {
        ((self.0 >> 21) & 0x1FF) as usize
    }

    pub const fn pt_index(self) -> usize {
        ((self.0 >> 12) & 0x1FF) as usize
    }

    pub const fn indices(self) -> PageTableIndices {
        PageTableIndices {
            pml4: self.pml4_index(),
            pdpt: self.pdpt_index(),
            pd: self.pd_index(),
            pt: self.pt_index(),
        }
    }

    pub const fn is_canonical(self) -> bool {
        let sign = (self.0 >> 47) & 1;
        let upper = self.0 >> 48;

        if sign == 0 {
            upper == 0
        } else {
            upper == 0xFFFF
        }
    }
    pub const fn page_offset(self) -> usize {
        (self.0 & (PAGE_SIZE - 1)) as usize
    }
    pub const fn as_ptr<T>(self) -> *const T {
        self.0 as *const T
    }

    pub const fn as_mut_ptr<T>(self) -> *mut T {
        self.0 as *mut T
    }
}

impl Add<u64> for VirtAddr {
    type Output = Self;
    fn add(self, rhs: u64) -> Self {
        Self(self.0 + rhs)
    }
}

impl Sub<u64> for VirtAddr {
    type Output = Self;
    fn sub(self, rhs: u64) -> Self {
        Self(self.0 - rhs)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PageTableIndices {
    pub pml4: usize,
    pub pdpt: usize,
    pub pd: usize,
    pub pt: usize,
}

pub const PAGE_SHIFT: u64 = 12;
pub const PAGE_MASK: u64 = PAGE_SIZE - 1;
pub const PAGE_SIZE_4K: u64 = 4 * 1024;
pub const PAGE_SIZE_2M: u64 = 2 * 1024 * 1024;
pub const PAGE_SIZE_1G: u64 = 1024 * 1024 * 1024;
pub const PAGE_SIZE: u64 = PAGE_SIZE_4K;

pub const fn align_up(x: u64, align: u64) -> u64 {
    (x + align - 1) & !(align - 1)
}

pub const fn align_down(x: u64, align: u64) -> u64 {
    x & !(align - 1)
}

/// Addresses at or above this are the kernel's higher-half; below is user space.
pub const USER_ADDR_LIMIT: u64 = 0x8000_0000_0000_0000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phys_align_roundtrip() {
        assert_eq!(PhysAddr(0x1234).align_down(4096), PhysAddr(0x1000));
        assert_eq!(PhysAddr(0x1234).align_up(4096), PhysAddr(0x2000));
        assert_eq!(PhysAddr(0x1000).align_up(4096), PhysAddr(0x1000));
        assert!(PhysAddr(0x1000).is_aligned(4096));
        assert!(!PhysAddr(0x1001).is_aligned(4096));
    }

    #[test]
    fn phys_add_sub() {
        let a = PhysAddr(0x1000);
        assert_eq!(a + 0x1000, PhysAddr(0x2000));
        assert_eq!(a - 0x500, PhysAddr(0xB00));
    }

    #[test]
    fn virt_indices_roundtrip_low() {
        // Low-half canonical address built from explicit index fields.
        let off = 0x123u64;
        let pt = 4u64;
        let pd = 3u64;
        let pdpt = 2u64;
        let pml4 = 0u64;
        let v = VirtAddr(
            (pml4 << 39) | (pdpt << 30) | (pd << 21) | (pt << 12) | off,
        );
        assert_eq!(v.pml4_index() as u64, pml4);
        assert_eq!(v.pdpt_index() as u64, pdpt);
        assert_eq!(v.pd_index() as u64, pd);
        assert_eq!(v.pt_index() as u64, pt);
        assert_eq!(v.page_offset() as u64, off);
    }

    #[test]
    fn virt_indices_high_half() {
        // 0xFFFF_8000_0000_0000 = -(2^47): pml4 256, pdpt 0, pd 0, pt 0
        let v = VirtAddr(0xFFFF_8000_0000_0000);
        assert_eq!(v.pml4_index(), 0x100);
        assert_eq!(v.pdpt_index(), 0);
        assert_eq!(v.pd_index(), 0);
        assert_eq!(v.pt_index(), 0);
    }

    #[test]
    fn canonical_and_non() {
        assert!(VirtAddr(0).is_canonical());
        assert!(VirtAddr(0x0000_7FFF_FFFF_FFFF).is_canonical()); // 2^47-1
        assert!(VirtAddr(0xFFFF_8000_0000_0000).is_canonical()); // -2^47
        assert!(VirtAddr(0xFFFF_FFFF_FFFF_FFFF).is_canonical());
        assert!(!VirtAddr(0x0000_8000_0000_0000).is_canonical()); // exactly 2^47
        assert!(!VirtAddr(0x1_0000_0000_0000).is_canonical()); // bit 48 set with sign 0
        assert!(!VirtAddr(0xFFFF_0000_0000_0000).is_canonical()); // non-canonical gap above 2^47
    }

    #[test]
    fn virt_align() {
        let base = VirtAddr(0xFFFF_8000_0000_1234);
        assert_eq!(base.align_down(4096), VirtAddr(0xFFFF_8000_0000_1000));
        assert_eq!(base.align_up(4096), VirtAddr(0xFFFF_8000_0000_2000));
        assert_eq!(base.page_offset(), 0x234);
    }

    #[test]
    fn const_helpers() {
        assert_eq!(align_up(0x1234, 4096), 0x2000);
        assert_eq!(align_down(0x1234, 4096), 0x1000);
        assert_eq!(align_up(0x1234, 0x1000), 0x2000);
        assert_eq!(PAGE_MASK, 0xFFF);
    }
}
