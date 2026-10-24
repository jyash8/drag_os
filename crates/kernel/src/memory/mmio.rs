use core::sync::atomic::{AtomicU64, Ordering};

use shared::{
    memory::{
        address::{PAGE_SIZE, PhysAddr, VirtAddr},
        error::AllocError,
        flags::PageTableFlags,
    },
};

use crate::{
    arch::x86_64::cpu::read_cr3,
    memory::{space::mapper_for, FRAME_ALLOCATOR},
};

pub const KERNEL_MMIO_BASE: VirtAddr = VirtAddr(0xFFFF_A000_0000_0000);

static MMIO_NEXT: AtomicU64 = AtomicU64::new(0);

pub fn map_device_memory(phys: PhysAddr, len: usize) -> Result<VirtAddr, AllocError> {
    // Replace with align up/down
    let len = (len as u64 + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    if len == 0 {
        return Err(AllocError::InvalidAlignment);
    }
    let virt = KERNEL_MMIO_BASE + MMIO_NEXT.fetch_add(len, Ordering::SeqCst);
    let mut mapper = mapper_for(read_cr3());

    let flags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::NO_EXECUTE
        | PageTableFlags::MMIO
        | PageTableFlags::CACHE_DISABLE;

    mapper.map_range(
        &mut *FRAME_ALLOCATOR.lock(),
        virt,
        phys,
        len as usize,
        flags,
    )?;
    Ok(virt)
}

pub fn unmap_device_memory(virt: VirtAddr, len: usize) {
    let len = (len as u64 + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let mut mapper = mapper_for(read_cr3());
    let _ = mapper.unmap_range(virt, len as usize);
}
