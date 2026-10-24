use crate::memory::{address::PhysAddr, error::AllocError};

pub trait FrameAllocator {
    fn alloc_pages(&mut self, pages: usize, alignment: u64) -> Result<PhysAddr, AllocError>;
    fn dealloc_pages(&mut self, addr: PhysAddr, pages: usize);
}

pub fn pages_for(size: usize) -> usize {
    const PAGE_SIZE: usize = 4096;

    size.div_ceil(PAGE_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::address::PhysAddr;

    #[derive(Default)]
    struct TestAllocator {
        next: u64,
    }

    impl FrameAllocator for TestAllocator {
        fn alloc_pages(&mut self, pages: usize, alignment: u64) -> Result<PhysAddr, AllocError> {
            let base = self.next;
            self.next += (pages * 4096) as u64;
            Ok(PhysAddr(base).align_up(alignment))
        }
        fn dealloc_pages(&mut self, _addr: PhysAddr, _pages: usize) {}
    }

    #[test]
    fn pages_for_rounds_up() {
        assert_eq!(pages_for(0), 0);
        assert_eq!(pages_for(1), 1);
        assert_eq!(pages_for(4096), 1);
        assert_eq!(pages_for(4097), 2);
    }

    #[test]
    fn allocator_trait_is_usable() {
        let mut a = TestAllocator::default();
        let p = a.alloc_pages(2, 4096).unwrap();
        assert!(p.is_aligned(4096));
        assert_eq!(a.alloc_pages(1, 4096).unwrap(), p + 8192);
    }
}
