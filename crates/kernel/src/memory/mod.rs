use crate::{arch::sync::spinlock::SpinLock, memory::frame::BootInfoFrameAllocator};

pub mod dma;
pub mod frame;
pub mod heap;
pub mod dsocache;
pub mod mmio;
pub mod space;
pub mod vma;
pub mod filemmap;

pub static FRAME_ALLOCATOR: SpinLock<BootInfoFrameAllocator> =
    SpinLock::named("FRAME_ALLOCATOR", BootInfoFrameAllocator::new());
pub mod slab;
