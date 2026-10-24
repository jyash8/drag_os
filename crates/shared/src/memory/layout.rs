use crate::memory::address::VirtAddr;

pub const BOOT_INFO_BASE: VirtAddr = VirtAddr(0xFFFF_8001_0000_0000);
pub const MEMORY_MAP_BASE: VirtAddr = VirtAddr(0xFFFF_8001_0001_0000);
pub const KERNEL_STACK_BASE: VirtAddr = VirtAddr(0xFFFF_8001_0002_0000);
pub const FRAMEBUFFER_BASE: VirtAddr = VirtAddr(0xFFFF_8001_0100_0000);

// stack size initially may increase later on
pub const KERNEL_STACK_PAGES: usize = 16;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_are_distinct_high_half() {
        assert_eq!(BOOT_INFO_BASE.0, 0xFFFF_8001_0000_0000);
        assert_eq!(MEMORY_MAP_BASE.0, 0xFFFF_8001_0001_0000);
        assert_eq!(KERNEL_STACK_BASE.0, 0xFFFF_8001_0002_0000);
        assert_eq!(FRAMEBUFFER_BASE.0, 0xFFFF_8001_0100_0000);
        assert!(FRAMEBUFFER_BASE.0 > KERNEL_STACK_BASE.0);
        assert_eq!(KERNEL_STACK_PAGES * 4096, 64 * 1024);
    }
}
