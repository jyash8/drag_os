use crate::{
    boot::MemoryRegion,
    memory::address::{PhysAddr, VirtAddr},
};

pub const BOOT_INFO_VERSION: u32 = 1;

pub const HHDM_BASE: VirtAddr = VirtAddr::new(0xffff_8000_0000_0000);

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct HhdmInfo {
    pub base: VirtAddr,
    pub size: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct BootInfo {
    pub version: u32,
    pub reserved: u32,
    /// Virtual address of the loaded kernel
    pub kernel_start: VirtAddr,
    /// End of the kernel image
    pub kernel_end: VirtAddr,
    /// Base virtual address of the Higher Half Direct Map.
    pub hhdm_info: HhdmInfo,
    /// Firmware memory map
    pub memory_map: &'static [MemoryRegion],
    /// Framebuffer information
    pub framebuffer: Option<FramebufferInfo>,
    /// Physical address of the ACPI RDSP
    pub rdsp: Option<u64>,
    /// Runtime-loaded initrd (tar archive) placed in physical memory
    pub initrd: Option<InitrdInfo>,
    /// Random seed derived from boot-time values (for ASLR)
    pub rng_seed: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct InitrdInfo {
    pub base: PhysAddr,
    pub size: usize,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FramebufferInfo {
    pub address: PhysAddr,
    pub size: usize,
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub format: PixelFormat,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy)]
pub enum PixelFormat {
    Rgb,
    Bgr,
    Bitmask,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_sizes() {
        assert_eq!(core::mem::size_of::<HhdmInfo>(), 16);
        assert_eq!(core::mem::size_of::<InitrdInfo>(), 16);
        // FramebufferInfo: 8 + 8 + 8 + 8 + 8 + 1 (repr C, padded to 8)
        assert_eq!(core::mem::size_of::<FramebufferInfo>(), 48);
    }

    #[test]
    fn hhdm_base_is_high_half() {
        assert_eq!(HHDM_BASE.0, 0xffff_8000_0000_0000);
        assert!(HHDM_BASE.is_canonical());
    }

    #[test]
    fn boot_info_default_version() {
        let b = BootInfo::default();
        assert_eq!(b.version, 0);
        assert_eq!(b.memory_map.len(), 0);
        assert!(b.framebuffer.is_none());
        assert!(b.rdsp.is_none());
        assert!(b.initrd.is_none());
        assert_eq!(b.rng_seed, 0);
    }

    #[test]
    fn initrd_with_phys_base() {
        let i = InitrdInfo {
            base: PhysAddr(0x2000),
            size: 512,
        };
        assert_eq!(i.base.as_u64(), 0x2000);
        assert_eq!(i.size, 512);
    }
}
