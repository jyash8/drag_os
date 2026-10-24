use crate::memory::{
    address::{PAGE_SIZE, PAGE_SIZE_1G, PAGE_SIZE_2M, PhysAddr, VirtAddr},
    allocator::FrameAllocator,
    error::AllocError,
    flags::PageTableFlags,
    page::{PageTable, PageTableEntry, PhysPage, VirtPage},
};

pub struct Mapper<'a> {
    pml4: &'a mut PageTable,
    phys_offset: u64,
}

impl<'a> Mapper<'a> {
    pub fn new(pml4: &'a mut PageTable) -> Self {
        Self {
            pml4,
            phys_offset: 0,
        }
    }

    pub fn new_with_offset(pml4: &'a mut PageTable, phys_offset: u64) -> Self {
        Self { pml4, phys_offset }
    }

    unsafe fn table_from_phys(phys: PhysAddr, phys_offset: u64) -> &'static mut PageTable {
        let virt_addr = phys + phys_offset;
        unsafe { &mut *virt_addr.as_mut_ptr::<PageTable>() }
    }

    fn ensure_table<A: FrameAllocator>(
        phys_offset: u64,
        allocator: &mut A,
        entry: &mut PageTableEntry,
    ) -> Result<&'static mut PageTable, AllocError> {
        if entry.is_present() {
            let phys = entry.address();
            return Ok(unsafe { Self::table_from_phys(phys, phys_offset) });
        }
        let phys = allocator.alloc_pages(1, PAGE_SIZE)?;

        let table = unsafe { Self::table_from_phys(phys, phys_offset) };
        table.zero();
        entry.set_address(phys);
        entry.set_flags(PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER);
        Ok(table)
    }

    fn ensure_pdpt<A: FrameAllocator>(
        &mut self,
        allocator: &mut A,
        virt: VirtAddr,
    ) -> Result<&mut PageTable, AllocError> {
        let idx = virt.indices();

        Self::ensure_table(self.phys_offset, allocator, self.pml4.entry_mut(idx.pml4))
    }

    fn ensure_pd<A: FrameAllocator>(
        &mut self,
        allocator: &mut A,
        virt: VirtAddr,
    ) -> Result<&mut PageTable, AllocError> {
        let idx = virt.indices();
        let phys_offset = self.phys_offset;

        let pdpt = self.ensure_pdpt(allocator, virt)?;

        Self::ensure_table(phys_offset, allocator, pdpt.entry_mut(idx.pdpt))
    }

    fn ensure_pt<A: FrameAllocator>(
        &mut self,
        allocator: &mut A,
        virt: VirtAddr,
    ) -> Result<&mut PageTable, AllocError> {
        let idx = virt.indices();
        let phys_offset = self.phys_offset;
        let pd = self.ensure_pd(allocator, virt)?;

        Self::ensure_table(phys_offset, allocator, pd.entry_mut(idx.pd))
    }

    pub fn map_4k(
        &mut self,
        allocator: &mut impl FrameAllocator,
        virt: VirtAddr,
        phys: PhysAddr,
        flags: PageTableFlags,
    ) -> Result<(), AllocError> {
        // log::info!("map {:>#018x} -> {:>#018x}", virt.0, phys.0,);
        let idx = virt.indices();
        let pt = self.ensure_pt(allocator, virt)?;
        let entry = pt.entry_mut(idx.pt);
        if entry.is_present() {
            if entry.address() != phys {
                return Err(AllocError::AlreadyMapped);
            }
            entry.set_flags(entry.flags() | flags | PageTableFlags::PRESENT);
            return Ok(());
        }
        entry.set_address(phys);
        if flags.contains(PageTableFlags::DEMAND_PAGING) {
            entry.set_flags(flags);
        } else {
            entry.set_flags(flags | PageTableFlags::PRESENT);
        }

        Ok(())
    }

    pub fn map_2m(
        &mut self,
        allocator: &mut impl FrameAllocator,
        virt: VirtAddr,
        phys: PhysAddr,
        flags: PageTableFlags,
    ) -> Result<(), AllocError> {
        debug_assert!(virt.is_aligned(PAGE_SIZE_2M));
        debug_assert!(phys.is_aligned(PAGE_SIZE_2M));
        let idx = virt.indices();
        let pd = self.ensure_pd(allocator, virt)?;
        let entry = pd.entry_mut(idx.pd);
        if entry.is_present() {
            if entry.address() != phys {
                return Err(AllocError::AlreadyMapped);
            }
            entry.set_flags(
                entry.flags() | flags | PageTableFlags::PRESENT | PageTableFlags::HUGE_PAGE,
            );
            return Ok(());
        }
        entry.set_address(phys);
        entry.set_flags(flags | PageTableFlags::PRESENT | PageTableFlags::HUGE_PAGE);

        Ok(())
    }

    pub fn map_1g(
        &mut self,
        allocator: &mut impl FrameAllocator,
        virt: VirtAddr,
        phys: PhysAddr,
        flags: PageTableFlags,
    ) -> Result<(), AllocError> {
        debug_assert!(virt.is_aligned(PAGE_SIZE_1G));
        debug_assert!(phys.is_aligned(PAGE_SIZE_1G));

        let idx = virt.indices();
        let pdpt = self.ensure_pdpt(allocator, virt)?;
        let entry = pdpt.entry_mut(idx.pdpt);

        if entry.is_present() {
            if entry.address() != phys {
                return Err(AllocError::AlreadyMapped);
            }

            entry.set_flags(
                entry.flags() | flags | PageTableFlags::HUGE_PAGE | PageTableFlags::PRESENT,
            );

            return Ok(());
        }

        entry.set_address(phys);

        entry.set_flags(flags | PageTableFlags::HUGE_PAGE | PageTableFlags::PRESENT);

        Ok(())
    }

    pub fn map_range(
        &mut self,
        allocator: &mut impl FrameAllocator,
        mut virt: VirtAddr,
        mut phys: PhysAddr,
        mut length: usize,
        flags: PageTableFlags,
    ) -> Result<(), AllocError> {
        while length != 0 {
            if length >= PAGE_SIZE_1G as usize
                && virt.is_aligned(PAGE_SIZE_1G)
                && phys.is_aligned(PAGE_SIZE_1G)
            {
                self.map_1g(allocator, virt, phys, flags)?;

                virt = virt + PAGE_SIZE_1G;
                phys = phys + PAGE_SIZE_1G;
                length -= PAGE_SIZE_1G as usize;
            } else if length >= PAGE_SIZE_2M as usize
                && virt.is_aligned(PAGE_SIZE_2M)
                && phys.is_aligned(PAGE_SIZE_2M)
            {
                self.map_2m(allocator, virt, phys, flags)?;

                virt = virt + PAGE_SIZE_2M;
                phys = phys + PAGE_SIZE_2M;
                length -= PAGE_SIZE_2M as usize;
            } else {
                self.map_4k(allocator, virt, phys, flags)?;

                virt = virt + PAGE_SIZE;
                phys = phys + PAGE_SIZE;
                length -= PAGE_SIZE as usize;
            }
        }

        Ok(())
    }

    pub fn map_pages<A: FrameAllocator>(
        &mut self,
        allocator: &mut A,
        virt: VirtPage,
        phys: PhysPage,
        page_count: usize,
        flags: PageTableFlags,
    ) -> Result<(), AllocError> {
        self.map_range(
            allocator,
            virt.start(),
            phys.start(),
            page_count * PAGE_SIZE as usize,
            flags,
        )
    }

    /// Mainly, for debugging, usually done
    /// by MMU itself
    pub fn translate(&self, addr: VirtAddr) -> Option<PhysAddr> {
        let idx = addr.indices();
        let pml4e = self.pml4.entry(idx.pml4);

        if !pml4e.is_present() {
            return None;
        }

        let pdpt = unsafe { Self::table_from_phys(pml4e.address(), self.phys_offset) };
        let pdpte = pdpt.entry(idx.pdpt);
        if !pdpte.is_present() {
            return None;
        }

        if pdpte.flags().contains(PageTableFlags::HUGE_PAGE) {
            let offset = addr.0 & (PAGE_SIZE_1G - 1);

            return Some(pdpte.address() + offset);
        }

        let pd = unsafe { Self::table_from_phys(pdpte.address(), self.phys_offset) };
        let pde = pd.entry(idx.pd);
        if !pde.is_present() {
            return None;
        }

        if pde.flags().contains(PageTableFlags::HUGE_PAGE) {
            let offset = addr.0 & (PAGE_SIZE_2M - 1);
            return Some(pde.address() + offset);
        }

        let pt = unsafe { Self::table_from_phys(pde.address(), self.phys_offset) };
        let pte = pt.entry(idx.pt);
        if !pte.is_present() {
            return None;
        }

        Some(pte.address() + addr.page_offset() as u64)
    }

    pub fn map_demand_4k(
        &mut self,
        allocator: &mut impl FrameAllocator,
        virt: VirtAddr,
        flags: PageTableFlags,
    ) -> Result<(), AllocError> {
        let idx = virt.indices();
        let pt = self.ensure_pt(allocator, virt)?;
        let entry = pt.entry_mut(idx.pt);
        if entry.is_present() {
            return Err(AllocError::AlreadyMapped);
        }
        
        // Strip the PRESENT flag if it was passed accidentally, since demand-paged memory
        // must trigger a page fault when accessed.
        let safe_flags = flags.bits() & !PageTableFlags::PRESENT.bits();
        let final_flags = PageTableFlags::from_bits(safe_flags) | PageTableFlags::DEMAND_PAGING;
        
        entry.set_flags(final_flags);
        Ok(())
    }

    pub fn unmap_4k(&mut self, virt: VirtAddr) -> Result<(), AllocError> {
        let idx = virt.indices();
        let pml4e = self.pml4.entry_mut(idx.pml4);
        if !pml4e.is_present() {
            return Ok(());
        }
        let pdpt = unsafe { Self::table_from_phys(pml4e.address(), self.phys_offset) };
        let pdpte = pdpt.entry_mut(idx.pdpt);
        if !pdpte.is_present() || pdpte.flags().contains(PageTableFlags::HUGE_PAGE) {
            return Ok(());
        }
        let pd = unsafe { Self::table_from_phys(pdpte.address(), self.phys_offset) };
        let pde = pd.entry_mut(idx.pd);
        if !pde.is_present() || pde.flags().contains(PageTableFlags::HUGE_PAGE) {
            return Ok(());
        }

        let pt = unsafe { Self::table_from_phys(pde.address(), self.phys_offset) };
        let pte = pt.entry_mut(idx.pt);

        pte.clear();
        Ok(())
    }

    pub fn unmap_range(&mut self, mut virt: VirtAddr, mut length: usize) -> Result<(), AllocError> {
        while length > 0 {
            self.unmap_4k(virt)?;
            virt = virt + PAGE_SIZE;
            if length < PAGE_SIZE as usize {
                break;
            }
            length -= PAGE_SIZE as usize;
        }
        Ok(())
    }

    pub fn update_prot_4k(
        &mut self,
        virt: VirtAddr,
        prot_flags: PageTableFlags,
    ) -> Result<(), AllocError> {
        let idx = virt.indices();
        let pml4e = self.pml4.entry_mut(idx.pml4);
        if !pml4e.is_present() {
            return Ok(());
        }

        let pdpt = unsafe { Self::table_from_phys(pml4e.address(), self.phys_offset) };
        let pdpte = pdpt.entry_mut(idx.pdpt);
        if !pdpte.is_present() || pdpte.flags().contains(PageTableFlags::HUGE_PAGE) {
            return Ok(());
        }

        let pd = unsafe { Self::table_from_phys(pdpte.address(), self.phys_offset) };
        let pde = pd.entry_mut(idx.pd);
        if !pde.is_present() || pde.flags().contains(PageTableFlags::HUGE_PAGE) {
            return Ok(());
        }

        let pt = unsafe { Self::table_from_phys(pde.address(), self.phys_offset) };
        let pte = pt.entry_mut(idx.pt);

        if pte.is_present() || pte.flags().contains(PageTableFlags::DEMAND_PAGING) {
            let is_cow = pte.flags().contains(PageTableFlags::COW);
            let mut final_flags = prot_flags;
            if is_cow && final_flags.contains(PageTableFlags::WRITABLE) {
                final_flags.remove(PageTableFlags::WRITABLE);
                final_flags.insert(PageTableFlags::COW);
            }
            pte.clear_flags(
                PageTableFlags::WRITABLE | PageTableFlags::USER | PageTableFlags::NO_EXECUTE,
            );
            pte.set_flags(final_flags);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::address::{PAGE_SIZE_1G, PAGE_SIZE_2M, PhysAddr, VirtAddr};
    use crate::memory::allocator::FrameAllocator;

    /// Bump allocator handing out pages that are real, aligned, dereferenceable
    /// memory: each "frame" is a leaked `Box<PageTable>` (align 4096), so
    /// `table_from_phys` can safely cast the returned addresses back to
    /// `&mut PageTable` (phys == virt in the host test).
    struct BumpAllocator {
        frames: Vec<PhysAddr>,
        next: usize,
    }

    impl BumpAllocator {
        fn new(n: usize) -> Self {
            let mut frames = Vec::with_capacity(n);
            for _ in 0..n {
                let table = Box::new(PageTable::new());
                let base = Box::into_raw(table) as usize as u64;
                frames.push(PhysAddr(base));
            }
            Self { frames, next: 0 }
        }
    }

    impl FrameAllocator for BumpAllocator {
        fn alloc_pages(&mut self, _pages: usize, _alignment: u64) -> Result<PhysAddr, AllocError> {
            if self.next >= self.frames.len() {
                return Err(AllocError::OutOfMemory);
            }
            let addr = self.frames[self.next];
            self.next += 1;
            Ok(addr)
        }
        fn dealloc_pages(&mut self, _addr: PhysAddr, _pages: usize) {}
    }

    fn test_mapper() -> (Mapper<'static>, BumpAllocator) {
        let alloc = BumpAllocator::new(64);
        let mut pml4 = Box::new(PageTable::new());
        let mapper: Mapper<'static> = Mapper::new(Box::leak(pml4));
        (mapper, alloc)
    }

    fn virt_low(n: u64) -> VirtAddr {
        VirtAddr(0x0040_0000_0000 + n * 4096)
    }

    #[test]
    fn new_pml4_is_empty() {
        let (mapper, _) = test_mapper();
        assert_eq!(mapper.translate(virt_low(0)), None);
    }

    #[test]
    fn map_then_translate_4k() {
        let (mut mapper, mut alloc) = test_mapper();
        let v = virt_low(0);
        let p = PhysAddr(0x1000);
        mapper.map_4k(&mut alloc, v, p, PageTableFlags::PRESENT | PageTableFlags::WRITABLE).unwrap();
        assert_eq!(mapper.translate(v), Some(p));
        assert_eq!(mapper.translate(v + 0x123), Some(p + 0x123));
    }

    #[test]
    fn map_same_phys_twice_ok_other_errors() {
        let (mut mapper, mut alloc) = test_mapper();
        let v = virt_low(1);
        mapper.map_4k(&mut alloc, v, PhysAddr(0x2000), PageTableFlags::PRESENT).unwrap();
        // Same phys: allowed, just re-flags.
        mapper.map_4k(&mut alloc, v, PhysAddr(0x2000), PageTableFlags::PRESENT | PageTableFlags::NO_EXECUTE).unwrap();
        assert_eq!(mapper.translate(v), Some(PhysAddr(0x2000)));
        // Different phys, still present: AlreadyMapped.
        let r = mapper.map_4k(&mut alloc, v, PhysAddr(0x3000), PageTableFlags::PRESENT);
        assert_eq!(r, Err(AllocError::AlreadyMapped));
    }

    fn entry_flags_at_v(mapper: &Mapper, v: VirtAddr) -> PageTableFlags {
        // Walk the table chain through a raw pointer to avoid alias conflicts.
        let pt_ptr = mapper.pml4 as *const PageTable;
        let pt = unsafe { &*pt_ptr };
        let idx = v.indices();
        let pdpt = unsafe { Mapper::table_from_phys(pt.entry(idx.pml4).address(), 0) };
        let pd = unsafe { Mapper::table_from_phys(pdpt.entry(idx.pdpt).address(), 0) };
        let table = unsafe { Mapper::table_from_phys(pd.entry(idx.pd).address(), 0) };
        table.entry(idx.pt).flags()
    }

    #[test]
    fn map_demand_4k_sets_flag_not_present() {
        let (mut mapper, mut alloc) = test_mapper();
        let v = virt_low(2);
        mapper.map_demand_4k(&mut alloc, v, PageTableFlags::WRITABLE | PageTableFlags::USER).unwrap();
        // Not PRESENT => translate() is None.
        assert_eq!(mapper.translate(v), None);
        let f = entry_flags_at_v(&mapper, v);
        assert!(f.contains(PageTableFlags::DEMAND_PAGING));
        assert!(f.contains(PageTableFlags::WRITABLE));
        // PRESENT stripped even if passed in.
        let v2 = virt_low(3);
        mapper.map_demand_4k(&mut alloc, v2, PageTableFlags::PRESENT).unwrap();
        let f2 = entry_flags_at_v(&mapper, v2);
        assert!(!f2.contains(PageTableFlags::PRESENT));
    }

    #[test]
    fn unmap_4k_clears_forward_translation() {
        let (mut mapper, mut alloc) = test_mapper();
        let v = virt_low(4);
        mapper.map_4k(&mut alloc, v, PhysAddr(0x4000), PageTableFlags::PRESENT).unwrap();
        assert_eq!(mapper.translate(v), Some(PhysAddr(0x4000)));
        mapper.unmap_4k(v).unwrap();
        assert_eq!(mapper.translate(v), None);
    }

    #[test]
    fn unmap_unmapped_page_is_ok() {
        let (mut mapper, _) = test_mapper();
        mapper.unmap_4k(virt_low(99)).unwrap();
    }

    #[test]
    fn update_prot_changes_flags() {
        let (mut mapper, mut alloc) = test_mapper();
        let v = virt_low(5);
        mapper.map_4k(&mut alloc, v, PhysAddr(0x5000), PageTableFlags::PRESENT | PageTableFlags::USER).unwrap();
        mapper.update_prot_4k(v, PageTableFlags::PRESENT | PageTableFlags::NO_EXECUTE).unwrap();
        let f = entry_flags_at_v(&mapper, v);
        assert!(f.contains(PageTableFlags::NO_EXECUTE));
        assert!(!f.contains(PageTableFlags::USER));
        assert!(!f.contains(PageTableFlags::WRITABLE));
    }

    #[test]
    fn map_range_splits_sizes() {
        let (mut mapper, mut alloc) = test_mapper();
        // 1G-aligned virt+phys -> single huge page walk
        let v = VirtAddr(0x4000_0000_0000);
        mapper.map_range(&mut alloc, v, PhysAddr(0x4000_0000), (1 << 30) as usize, PageTableFlags::PRESENT).unwrap();
        assert_eq!(mapper.translate(v), Some(PhysAddr(0x4000_0000)));
        assert_eq!(mapper.translate(v + PAGE_SIZE_1G - 1), Some(PhysAddr(0x4000_0000) + PAGE_SIZE_1G - 1));
        assert_eq!(mapper.translate(v + PAGE_SIZE_1G), None);
    }

    #[test]
    fn map_2m_and_1g_huge_pages() {
        let (mut mapper, mut alloc) = test_mapper();
        let v2m = VirtAddr(0x8040_0000); // aligned 2M
        mapper.map_2m(&mut alloc, v2m, PhysAddr(0x200000), PageTableFlags::PRESENT).unwrap();
        assert_eq!(mapper.translate(v2m), Some(PhysAddr(0x200000)));
        assert_eq!(mapper.translate(v2m + PAGE_SIZE_2M - 1), Some(PhysAddr(0x200000) + PAGE_SIZE_2M - 1));

        let v1g = VirtAddr(0xC000_0000); // aligned 1G
        mapper.map_1g(&mut alloc, v1g, PhysAddr(0x40000000), PageTableFlags::PRESENT).unwrap();
        assert_eq!(mapper.translate(v1g), Some(PhysAddr(0x40000000)));
        assert_eq!(mapper.translate(v1g + PAGE_SIZE_1G - 1), Some(PhysAddr(0x40000000) + PAGE_SIZE_1G - 1));
    }
}
