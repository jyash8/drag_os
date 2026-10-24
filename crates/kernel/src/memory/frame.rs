use shared::{
    boot::{MemoryRegion, MemoryRegionKind},
    memory::{
        address::{PAGE_SIZE, PhysAddr},
        allocator::FrameAllocator,
        error::AllocError,
    },
};

const MAX_FREE_RUNS: usize = 4096;

pub struct BootInfoFrameAllocator {
    memory_map: &'static [MemoryRegion],
    next_region: usize,
    next_frame_offset: u64,
    free: [(u64, u64); MAX_FREE_RUNS], // (phys_addr, count_in_frames)
    free_count: usize,
    /// Total pages allocated via alloc_pages (for leak detection).
    alloc_count: u64,
    /// Total pages deallocated via dealloc_pages (for leak detection).
    dealloc_count: u64,
    /// Single-page free stack (LIFO) for the common 1-page alloc case.
    /// Entries are physical addresses of 1-page runs.
    single_free: [u64; 1024],
    single_count: usize,
}

impl BootInfoFrameAllocator {
    pub const fn new() -> Self {
        Self {
            memory_map: &[],
            next_region: 0,
            next_frame_offset: 0,
            free: [(0, 0); MAX_FREE_RUNS],
            free_count: 0,
            alloc_count: 0,
            dealloc_count: 0,
            single_free: [0; 1024],
            single_count: 0,
        }
    }

    pub unsafe fn init(&mut self, memory_map: &'static [MemoryRegion]) {
        self.memory_map = memory_map;
    }

    /// Returns a reference to the memory map (for diagnostics).
    pub fn memory_map(&self) -> &'static [MemoryRegion] {
        self.memory_map
    }

    /// Returns total allocations and deallocations (for leak detection).
    pub fn alloc_stats(&self) -> (u64, u64) {
        (self.alloc_count, self.dealloc_count)
    }

    pub fn free_frames(&self) -> u64 {
        self.free
            .iter()
            .take(self.free_count)
            .map(|&(_, c)| c)
            .sum()
    }

    fn alloc_from_free(&mut self, count: usize) -> Option<PhysAddr> {
        for i in 0..self.free_count {
            let (addr, size) = self.free[i];
            if size >= count as u64 {
                if size == count as u64 {
                    self.free[i] = self.free[self.free_count - 1];
                    self.free_count -= 1;
                } else {
                    self.free[i] = (
                        addr + (count as u64) * PAGE_SIZE,
                        size - count as u64,
                    );
                }
                return Some(PhysAddr(addr));
            }
        }
        None
    }

    /// Return pages to the free pool, coalescing with adjacent runs so the
    /// free list does not bloat. Without coalescing, every individual page
    //  free becomes a separate entry, and once `free_count` hits `MAX_FREE_RUNS`
    //  every subsequent `dealloc_pages` silently leaks the frames — eventually
    //  exhausting physical memory (OOM).
    fn push_free(&mut self, addr: PhysAddr, count: usize) {
        let addr = addr.as_u64();
        let count = count as u64;

        // Scan for an adjacent run to coalesce with: one ending at `addr`
        // (extend backward) or one starting at `addr + count*PAGE_SIZE`
        // (extend forward).
        for i in 0..self.free_count {
            let (f_addr, f_count) = self.free[i];
            if f_addr + f_count * PAGE_SIZE == addr {
                // Existing run ends right before our new run → extend backward.
                self.free[i] = (f_addr, f_count + count);
                return;
            }
            if addr + count * PAGE_SIZE == f_addr {
                // Our new run ends right before existing run → extend forward.
                self.free[i] = (addr, count + f_count);
                return;
            }
        }

        // No adjacent run to merge with; add as a new entry.
        if self.free_count < MAX_FREE_RUNS {
            self.free[self.free_count] = (addr, count);
            self.free_count += 1;
        }
        // Else: free list is full, pages leaked. With coalescing this should
        // be rare; without it, every free leaked once the list filled.
    }

    pub fn memory_stats(&self) -> (u64, u64) {
        let mut total_pages = 0;
        let mut usable_pages = 0;
        for region in self.memory_map.iter() {
            total_pages += region.page_count();
            if region.kind == MemoryRegionKind::Usable {
                usable_pages += region.page_count();
            }
        }
        let total_bytes = total_pages as u64 * PAGE_SIZE;
        let usable_bytes = usable_pages as u64 * PAGE_SIZE;

        (total_bytes, usable_bytes)
    }
}

impl FrameAllocator for BootInfoFrameAllocator {
    fn alloc_pages(&mut self, count: usize, _align: u64) -> Result<PhysAddr, AllocError> {
        // Note: _align is safely ignored here because physical pages
        // are inherently 4KB (PAGE_SIZE) aligned by definition.

        // Fast path: single-page allocation from the LIFO single-page free stack.
        if count == 1 && self.single_count > 0 {
            self.single_count -= 1;
            let addr = self.single_free[self.single_count];
            self.alloc_count += 1;
            return Ok(PhysAddr(addr));
        }

        if let Some(p) = self.alloc_from_free(count) {
            self.alloc_count += count as u64;
            return Ok(p);
        }
        while self.next_region < self.memory_map.len() {
            let region = &self.memory_map[self.next_region];

            // Skip unusable regions
            if region.kind != MemoryRegionKind::Usable {
                self.next_region += 1;
                self.next_frame_offset = 0;
                continue;
            }

            // Do we have enough contiguous pages left in this region?
            if self.next_frame_offset as usize + (count) <= region.page_count() {
                let phys_addr = region.start.as_u64() + (self.next_frame_offset * PAGE_SIZE);

                // Advance the bump pointer by the requested number of pages
                self.next_frame_offset += count as u64;
                self.alloc_count += count as u64;
                return Ok(PhysAddr::new(phys_addr));
            }

            // Not enough space here, move to the next region
            self.next_region += 1;
            self.next_frame_offset = 0;
        }

        // We ran out of usable physical RAM
        Err(AllocError::OutOfMemory) // (Adjust to whatever your out-of-memory variant is)
    }

    fn dealloc_pages(&mut self, addr: PhysAddr, pages: usize) {
        self.dealloc_count += pages as u64;
        if pages == 1 && self.single_count < 1024 {
            // Fast path: push 1-page frees onto the LIFO single-page stack.
            self.single_free[self.single_count] = addr.as_u64();
            self.single_count += 1;
            return;
        }
        self.push_free(addr, pages);
    }
}
