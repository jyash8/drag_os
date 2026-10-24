use shared::memory::{
    address::{PAGE_SIZE, VirtAddr},
    allocator::FrameAllocator,
    error::AllocError,
    flags::PageTableFlags,
    mapper::Mapper,
    page::{PhysPage, VirtPage},
};

pub const HEAP_START: u64 = 0xFFFF_9000_0000_0000;
pub const HEAP_SIZE: u64 = 256 * 1024 * 1024; // 256 MiB (tmpfs backing is sparse: pool ftruncate only records size, pages allocate on write)

#[global_allocator]
pub static ALLOCATOR: crate::arch::sync::spinlock::SpinLock<
    crate::memory::slab::FixedSizeBlockAllocator,
> = crate::arch::sync::spinlock::SpinLock::new(crate::memory::slab::FixedSizeBlockAllocator::new());
pub fn init_heap(
    mapper: &mut Mapper,
    allocator: &mut impl FrameAllocator,
) -> Result<(), AllocError> {
    let page_count = (HEAP_SIZE / PAGE_SIZE) as usize;
    let heap_start_page = VirtPage::containing(VirtAddr::new(HEAP_START));

    let phys_pages = allocator.alloc_pages(page_count, 4096)?;

    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
    mapper.map_pages(
        allocator,
        heap_start_page,
        PhysPage::containing(phys_pages),
        page_count,
        flags,
    )?;

    unsafe {
        ALLOCATOR
            .lock()
            .init(HEAP_START as *mut u8, HEAP_SIZE as usize);
    }

    Ok(())
}
