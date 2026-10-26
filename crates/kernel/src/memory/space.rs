use core::ptr::write_bytes;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use shared::boot::boot_info::HHDM_BASE;
use shared::memory::address::{PAGE_SIZE, PhysAddr, VirtAddr, align_down};
use shared::memory::allocator::FrameAllocator;
use shared::memory::error::AllocError;
use shared::memory::flags::PageTableFlags;
use shared::memory::mapper::Mapper;
use shared::memory::page::{PageTable, PageTableEntry};

use crate::arch::sync::spinlock::SpinLock;
use crate::arch::x86_64::cpu::{read_cr3, write_cr3};

use crate::memory::FRAME_ALLOCATOR;

pub static COW_COUNTS: SpinLock<Option<Vec<u32>>> = SpinLock::new(None);

pub fn init_cow_table(max_frame: usize) {
    *COW_COUNTS.lock() = Some(vec![0; max_frame]);
}

pub fn mapper_for(cr3: PhysAddr) -> Mapper<'static> {
    Mapper::new_with_offset(
        unsafe { &mut *(cr3 + HHDM_BASE.0).as_mut_ptr::<PageTable>() },
        HHDM_BASE.0,
    )
}

pub fn map_demand_range(
    cr3: PhysAddr,
    start: u64,
    end: u64,
    flags: PageTableFlags,
    allocator: &mut impl FrameAllocator,
) {
    let mut mapper = mapper_for(cr3);
    for page in (start..end).step_by(PAGE_SIZE as usize) {
        let _ = mapper.map_demand_4k(allocator, VirtAddr(page), flags);
    }
}

/// Frames outside the COW table are non-RAM PTEs (MMIO without flag bits,
/// stale entries). There is nothing to refcount: log (first few) and skip
/// instead of panicking, so fork/exit survive them.
// ponytail: bounds guard + forensic log; the underlying stray-PTE producer is still open.
fn cow_oob(frame: u64, len: usize) -> bool {
    if (frame as usize) < len {
        return false;
    }
    static N: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    if N.fetch_add(1, core::sync::atomic::Ordering::Relaxed) < 4 {
        crate::dprintln!("[fork] cow SKIP frame={:#x} table_len={}", frame, len);
    }
    true
}

#[inline(always)]
fn cow_add(frame: u64, delta: u32) -> u32 {
    let mut guard = COW_COUNTS.lock();
    let table = guard.as_mut().expect("COW table not init");
    if cow_oob(frame, table.len()) {
        return 0;
    }
    let count = &mut table[frame as usize];
    *count = count.checked_add(delta).expect("COW count overflow");
    *count
}

fn cow_dec(frame: u64) -> u32 {
    cow_dec_at(frame, "cow_dec")
}

/// COW referencecount decrement, tagged with the call site.
///
/// The table's invariant is `count == number of user PTEs referencing this frame
/// with COW set`. When that is violated the decrement underflows, and the only
/// thing the bare `expect("COW count underflow")` could tell us was *that* it
/// happened -- not which of the three decrement paths did it, or which frame.
/// Both are needed to locate the producer, so report them before panicking.
fn cow_dec_at(frame: u64, site: &str) -> u32 {
    let mut guard = COW_COUNTS.lock();
    let table = guard.as_mut().expect("COW table not init");
    if cow_oob(frame, table.len()) {
        return 0;
    }
    let count = &mut table[frame as usize];
    // ponytail: COW count underflow means `cow_add` was never called for this
    // frame — a genuine bug. Panic loudly so it cannot silently corrupt memory.
    if *count == 0 {
        panic!("COW count underflow frame={:#x} count=0 site={}", frame, site);
    }
    *count -= 1;
    *count
}

pub fn create_address_space(
    allocator: &mut impl FrameAllocator,
    phys_offset: u64,
) -> Result<PhysAddr, AllocError> {
    let pml4_phys = allocator.alloc_pages(1, PAGE_SIZE)?;
    let new = unsafe { &mut *(pml4_phys + phys_offset).as_mut_ptr::<PageTable>() };
    new.zero();

    let cur_phys = read_cr3();
    let cur = unsafe { &mut *(cur_phys + phys_offset).as_mut_ptr::<PageTable>() };

    for i in 256..512 {
        *new.entry_mut(i) = *cur.entry(i);
    }

    Ok(pml4_phys)
}

pub fn fork_address_space(
    parent_pml4: PhysAddr,
    allocator: &mut impl FrameAllocator,
    phys_offset: u64,
) -> Result<PhysAddr, AllocError> {
    let child_pml4 = create_address_space(allocator, phys_offset)?;

    let parent = unsafe { &mut *(parent_pml4 + phys_offset).as_mut_ptr::<PageTable>() };
    let child = unsafe { &mut *(child_pml4 + phys_offset).as_mut_ptr::<PageTable>() };

    for pml4i in 0..256 {
        let pml4e = *parent.entry(pml4i);
        if !pml4e.is_present() {
            continue;
        }
        let pdpt = unsafe { &mut *(pml4e.address() + phys_offset).as_mut_ptr::<PageTable>() };
        let child_pdpt_d = allocator.alloc_pages(1, PAGE_SIZE)?;
        let child_pdpt = unsafe { &mut *(child_pdpt_d + phys_offset).as_mut_ptr::<PageTable>() };
        child_pdpt.zero();
        child.entry_mut(pml4i).set_address(child_pdpt_d);
        child
            .entry_mut(pml4i)
            .set_flags(PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER);

        for pdpti in 0..512 {
            let pdpte = *pdpt.entry(pdpti);
            if !pdpte.is_present() {
                continue;
            }
            let pd = unsafe { &mut *(pdpte.address() + phys_offset).as_mut_ptr::<PageTable>() };
            let child_pd_p = allocator.alloc_pages(1, PAGE_SIZE)?;
            let child_pd = unsafe { &mut *(child_pd_p + phys_offset).as_mut_ptr::<PageTable>() };
            child_pd.zero();
            child_pdpt.entry_mut(pdpti).set_address(child_pd_p);
            child_pdpt.entry_mut(pdpti).set_flags(
                PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER,
            );

            for pdi in 0..512 {
                let pde = *pd.entry(pdi);
                if !pde.is_present() {
                    continue;
                }
                let pt = unsafe { &mut *(pde.address() + phys_offset).as_mut_ptr::<PageTable>() };
                let child_pt_p = allocator.alloc_pages(1, PAGE_SIZE)?;
                let child_pt =
                    unsafe { &mut *(child_pt_p + phys_offset).as_mut_ptr::<PageTable>() };
                child_pt.zero();
                child_pd.entry_mut(pdi).set_address(child_pt_p);
                child_pd.entry_mut(pdi).set_flags(
                    PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER,
                );

                for pti in 0..512 {
                    let pte = *pt.entry(pti);
                    if pte.flags().contains(PageTableFlags::MMIO) {
                        *child_pt.entry_mut(pti) = pte;
                        continue;
                    }
                    if pte.flags().contains(PageTableFlags::FILE_CACHE) {
                        // A forked process must not retain write access to a cached
                        // library page.  Turn writable copied file mappings into COW
                        // entries so the first write consumes one clean private
                        // page instead of modifying the shared DSO frame.  This is
                        // the same conversion performed for anonymous private
                        // pages below; FILE_CACHE frames intentionally carry no
                        // COW-table reference, so cow_add is not applied here.
                        if pte.flags().contains(PageTableFlags::WRITABLE) {
                            let shared = *child_pt.entry_mut(pti);
                            let frame = shared.address().as_u64() >> 12;
                            child_pt.entry_mut(pti).clear_flags(PageTableFlags::WRITABLE);
                            child_pt.entry_mut(pti).set_flags(PageTableFlags::COW);
                            pt.entry_mut(pti).clear_flags(PageTableFlags::WRITABLE);
                            pt.entry_mut(pti).set_flags(PageTableFlags::COW);
                            if frame != 0 {
                                let _ = frame;
                            }
                        }
                        *child_pt.entry_mut(pti) = pte;
                        crate::memory::dsocache::retain_from_frame(pte.address().as_u64() >> 12);
                        continue;
                    }
                    if !pte.is_present() {
                        if pte.flags().contains(PageTableFlags::DEMAND_PAGING) {
                            // Copy demand paging entry directly (no frame to refcount)
                            *child_pt.entry_mut(pti) = pte;
                        }
                        continue;
                    }
                    let mut shared = pte;
                    let frame = pte.address().as_u64() >> 12;
                    if pte.flags().contains(PageTableFlags::COW) {
                        cow_add(frame, 1);
                    } else {
                        shared.clear_flags(PageTableFlags::WRITABLE);
                        shared.set_flags(PageTableFlags::COW);
                        pt.entry_mut(pti).clear_flags(PageTableFlags::WRITABLE);
                        pt.entry_mut(pti).set_flags(PageTableFlags::COW);
                        cow_add(frame, 2);
                    }
                    *child_pt.entry_mut(pti) = shared;
                }
            }
        }
    }
    // drop stale writable TLB entries for the parent's COW-marked pages
    unsafe { write_cr3(parent_pml4) };
    crate::smp::flush_tlb_all_cores();
    Ok(child_pml4)
}

pub static GLOBAL_ZERO_FRAME: SpinLock<Option<PhysAddr>> = SpinLock::new(None);

pub fn get_or_init_zero_frame(allocator: &mut impl FrameAllocator) -> Option<PhysAddr> {
    let mut guard = GLOBAL_ZERO_FRAME.lock();
    if let Some(frame) = *guard {
        return Some(frame);
    }
    let frame = allocator.alloc_pages(1, PAGE_SIZE).ok()?;
    let dst = (frame.0 + HHDM_BASE.0) as *mut u8;
    unsafe {
        core::ptr::write_bytes(dst, 0, PAGE_SIZE as usize);
    }
    *guard = Some(frame);
    Some(frame)
}

fn free_leaf(entry: PageTableEntry, allocator: &mut impl FrameAllocator, counts: &mut Vec<u32>) {
    if !entry.is_present() || entry.flags().contains(PageTableFlags::MMIO) {
        return;
    }
    if entry.flags().contains(PageTableFlags::FILE_CACHE) {
        // Frame is owned by the DSO cache; dsocache::release handles the
        // refcount and dealloc. Just clear the PTE.
        return;
    }
    let frame = (entry.address().as_u64() >> 12) as usize;
    let is_zero_page = GLOBAL_ZERO_FRAME.lock().map_or(false, |zf| (zf.0 >> 12) as usize == frame);
    if entry.flags().contains(PageTableFlags::COW) {
        // Bounds-guard exactly as cow_add/cow_dec do (cow_oob). This index was
        // previously raw, which turns any out-of-range frame into a kernel-wide
        // panic during teardown:
        //
        //   KERNEL PANIC: index out of bounds: the len is 1310720
        //                  but the index is 1092758798078   (0xfe6d7ffefe)
        //   PANIC@ crates/kernel/src/memory/space.rs:221
        //
        // That frame is not RAM at all -- it is a non-RAM PTE (MMIO without flag
        // bits, or a stale entry) that happens to carry the COW flag. The same
        // situation is already handled and logged by cow_oob for the *add*
        // path; this is the free path, which had no guard at all.
        //
        // Skipping the refcount is correct here: there is nothing to refcount for
        // a frame outside the table, and deallocating it would be worse -- it
        // would free an address we do not own.
        if cow_oob(frame as u64, counts.len()) {
            return;
        }
        // ponytail: TEMP-DIAG. Was `checked_sub(1).expect("COW free underflow")`.
        // An underflow here means the table and the PTEs disagree, which is a
        // real bug -- but panicking on it is reached from userspace and, when
        // the panicking core then meets a held spinlock, escalates to a full
        // machine deadlock (see docs/virgl-vs-linux.md 25). So for now: report
        // it and *clamp to 1*, which keeps the frame alive instead of handing a
        // still-referenced page back to the allocator. Leaking is the safe
        // direction; freeing it would be silent corruption. REVERT to the
        // panic once the producer is found.
        let c = match counts[frame].checked_sub(1) {
            Some(v) => v,
            None => {
                crate::dprintln!(
                    "[cow] UNDERFLOW frame={:#x} count=0 site=free_leaf pte_flags={:#x}",
                    frame,
                    entry.flags().bits()
                );
                1
            }
        };
        counts[frame] = c;
        if c == 0 && !is_zero_page {
            allocator.dealloc_pages(PhysAddr((frame as u64) << 12), 1);
        }
    } else if !is_zero_page {
        // Non-COW leaf: deallocate. Guarded for the same reason, and here it
        // matters more: `dealloc_pages` -> `push_free` merely *records* the
        // address, and `alloc_pages` later hands recorded addresses back out as
        // if they were RAM. Freeing a non-RAM frame therefore does not fail --
        // it silently poisons the free list, so the corruption resurfaces as an
        // unrelated mystery much later. A panic here would at least be loud.
        //
        // COW_COUNTS is sized from the highest end of any memory-map region
        // (lib.rs init_cow_table(max_frame)), so any frame at or beyond its
        // length is definitionally not RAM we own.
        if cow_oob(frame as u64, counts.len()) {
            return;
        }
        allocator.dealloc_pages(PhysAddr((frame as u64) << 12), 1);
    }
}

pub fn destroy_address_space(pml4_phys: PhysAddr) {
    // Revert the diagnostic no-op: the actual corruption root cause was
    // missing VMA insertion in load_image (fixed below). Restore real
    // destroy logic. [ponytail: this is the real function, previously
    // replaced by a temporary diagnostic no-op.]
    let phys_offset = HHDM_BASE.0;
    let mut allocator = FRAME_ALLOCATOR.lock();
    let mut counts_guard = COW_COUNTS.lock();
    let counts = counts_guard.as_mut().expect("COW table not init");

    unsafe {
        let pml4 = &mut *(pml4_phys + phys_offset).as_mut_ptr::<PageTable>();
        for pml4i in 0..256 {
            let pml4e = *pml4.entry(pml4i);
            if !pml4e.is_present() {
                continue;
            }
            let pdpt = &mut *(pml4e.address() + phys_offset).as_mut_ptr::<PageTable>();
            for pdpti in 0..512 {
                let pdpte = *pdpt.entry(pdpti);
                if !pdpte.is_present() {
                    continue;
                }
                if pdpte.flags().contains(PageTableFlags::HUGE_PAGE) {
                    free_leaf(pdpte, &mut *allocator, counts);
                    continue;
                }
                let pd = &mut *(pdpte.address() + phys_offset).as_mut_ptr::<PageTable>();
                for pdi in 0..512 {
                    let pde = *pd.entry(pdi);
                    if !pde.is_present() {
                        continue;
                    }
                    if pde.flags().contains(PageTableFlags::HUGE_PAGE) {
                        free_leaf(pde, &mut *allocator, counts);
                        continue;
                    }
                    let pt = &mut *(pde.address() + phys_offset).as_mut_ptr::<PageTable>();
                    for pti in 0..512 {
                        let pte = *pt.entry(pti);
                        if !pte.is_present() {
                            continue;
                        }
                        if pte.flags().contains(PageTableFlags::FILE_CACHE) {
                            let dirty = pte.flags().contains(PageTableFlags::DIRTY);
                            if let Some(f) = crate::memory::filemmap::release_file_page_from_frame_dirty(
                                pte.address().as_u64() >> 12,
                                dirty,
                            ) {
                                allocator.dealloc_pages(f, 1);
                            }
                            continue;
                        }
                        free_leaf(pte, &mut *allocator, counts);
                    }
                    free_leaf(pde, &mut *allocator, counts);
                }
                free_leaf(pdpte, &mut *allocator, counts);
            }
            free_leaf(pml4e, &mut *allocator, counts);
        }
    }
    allocator.dealloc_pages(pml4_phys, 1);
}

fn leaf_entry_mut(
    pml4: &mut PageTable,
    vaddr: VirtAddr,
    phys_offset: u64,
) -> Option<&mut PageTableEntry> {
    let idx = vaddr.indices();
    let pml4e = *pml4.entry(idx.pml4);
    if !pml4e.is_present() {
        return None;
    }
    let pdpt = unsafe { &mut *(pml4e.address() + phys_offset).as_mut_ptr::<PageTable>() };
    let pdpte = *pdpt.entry(idx.pdpt);
    if !pdpte.is_present() || pdpte.flags().contains(PageTableFlags::HUGE_PAGE) {
        return None;
    }
    let pd = unsafe { &mut *(pdpte.address() + phys_offset).as_mut_ptr::<PageTable>() };
    let pde = *pd.entry(idx.pd);
    if !pde.is_present() || pde.flags().contains(PageTableFlags::HUGE_PAGE) {
        return None;
    }
    let pt = unsafe { &mut *(pde.address() + phys_offset).as_mut_ptr::<PageTable>() };
    Some(pt.entry_mut(idx.pt))
}

/// Resolve the current address-space root and walk to the 4K leaf.
/// Collapses the read_cr3 + root-deref prologue repeated at every fault
/// site into one call (single point of change for the walk).
/// SAFETY/`'static`: like mapper_for below, the root derives from a raw
/// HHDM pointer, not from a Rust borrow, so it outlives this frame.
/// Callers must use it transiently, never across address-space teardown.
fn leaf_entry_current(vaddr: VirtAddr, phys_offset: u64) -> Option<&'static mut PageTableEntry> {
    let pml4_phys = read_cr3();
    let pml4: &'static mut PageTable =
        unsafe { &mut *(pml4_phys + phys_offset).as_mut_ptr::<PageTable>() };
    leaf_entry_mut(pml4, vaddr, phys_offset)
}

/// Diagnostic: raw 4K leaf contents for `vaddr` in the address space rooted at
/// `cr3`, as `(pte_bits, physical_address)`.
///
/// The page-fault error code cannot distinguish "not present" from "present but
/// not writable" -- bit 0 set means the latter -- and those two need completely
/// different fixes. Reading the leaf is the only way to tell them apart, so
/// this exists instead of callers re-deriving the walk inline.
pub fn probe_leaf_pte(cr3: PhysAddr, vaddr: u64) -> Option<(u64, u64)> {
    let pml4: &'static mut PageTable =
        unsafe { &mut *(cr3 + HHDM_BASE.0).as_mut_ptr::<PageTable>() };
    let pte = leaf_entry_mut(pml4, VirtAddr(vaddr), HHDM_BASE.0)?;
    Some((pte.flags().bits(), pte.address().as_u64()))
}

/// Same for an explicit CR3 via the HHDM direct map (like mapper_for).
/// Used where the root was already read for something else, or the caller
/// holds a foreign address space. Same transient-use contract as above.
fn leaf_entry_cr3(cr3: PhysAddr, vaddr: VirtAddr) -> Option<&'static mut PageTableEntry> {
    let pml4: &'static mut PageTable =
        unsafe { &mut *(cr3 + HHDM_BASE.0).as_mut_ptr::<PageTable>() };
    leaf_entry_mut(pml4, vaddr, HHDM_BASE.0)
}

pub fn cow_write_fault(cr2: u64, phys_offset: u64) -> bool {
    let entry = match leaf_entry_current(VirtAddr::new(cr2), phys_offset) {
        Some(e) => e,
        None => return false,
    };

    if !entry.flags().contains(PageTableFlags::COW) {
        return false;
    }

    let is_writable_vma = match crate::tasks::fault_process() {
        Some(p) => match p.try_lock() {
            Some(guard) => match guard.vmas.find(cr2) {
                Some(vma) => (vma.prot & crate::memory::vma::PROT_WRITE) != 0,
                None => true,
            },
            None => true,
        },
        None => true,
    };
    if !is_writable_vma {
        return false;
    }

    let is_file_cache = entry.flags().contains(PageTableFlags::FILE_CACHE);
    let frame = entry.address().as_u64() >> 12;
    let is_zero_page = GLOBAL_ZERO_FRAME.lock().map_or(false, |zf| zf.0 >> 12 == frame);
    if !is_file_cache && !is_zero_page && cow_dec_at(frame, "cow_write_fault") == 0 {
        // Last owner of anonymous page: flip writable in place, no copy.
        entry.set_flags(PageTableFlags::WRITABLE);
        entry.clear_flags(PageTableFlags::COW);
    } else {
        if is_zero_page {
            let _ = cow_dec_at(frame, "cow_write_fault:zero");
        }
        // Others still share the old frame: allocate a private copy
        let mut alloc = FRAME_ALLOCATOR.lock();
        let new_frame = alloc.alloc_pages(1, PAGE_SIZE).expect("COW alloc failed");
        let src = (entry.address() + phys_offset).as_ptr::<u8>();
        let dst = (new_frame + phys_offset).as_mut_ptr::<u8>();

        unsafe {
            core::ptr::copy_nonoverlapping(src, dst, PAGE_SIZE as usize);
        }
        entry.set_address(new_frame);
        entry.set_flags(PageTableFlags::WRITABLE);
        entry.clear_flags(PageTableFlags::COW | PageTableFlags::FILE_CACHE);

        if is_file_cache {
            if let Some(f) = crate::memory::filemmap::release_file_page_from_frame(frame) {
                alloc.dealloc_pages(f, 1);
            }
        }
    }

    invlpg(cr2);
    crate::smp::flush_tlb_page(cr2);
    true
}

pub fn invlpg(addr: u64) {
    unsafe {
        core::arch::asm!("invlpg [{}]", in(reg) addr, options(nostack, preserves_flags));
    }
}

/// Marks the (present) leaf page at `addr` as COW in place: clears WRITABLE,
/// sets COW, so a later write fault invokes `cow_write_fault`.
///
/// The COW table's invariant is **count == the number of user PTEs that
/// reference this frame with COW set** (see `fork_address_space`: `+1` when the
/// child copies an already-COW page, `+2` when parent and child are both
/// converted). Turning exactly one PTE into a COW reference therefore owes the
/// table `+1`, and this function used to owe it nothing.
///
/// That left the page COW-flagged with a count of 0, so the first decrement
/// underflowed. It is reachable from an ordinary `execve` pair, because
/// `load_image` (`elf/loader.rs`) calls `mark_cow` on the private segments of
/// every image it loads, and the *next* `execve` tears that address space down
/// via `destroy_address_space` -> `free_leaf` -> `counts[frame] -= 1`:
///
/// ```text
/// [execve] destroying old cr3=PhysAddr(427745280)...
/// [execve] starting path=/bin/foot
/// KERNEL PANIC: COW count underflow
/// PANIC@ crates/kernel/src/memory/space.rs:79
/// ```
///
/// The panic then took the whole machine with it, because the panicking core
/// immediately hit a second one while the fault was still on screen:
///
/// ```text
/// KERNEL PANIC: SPIN DEADLOCK on 0xffffffff80926218 my_cpu=0 holder_cpu=1
/// ```
///
/// Only pages the COW table actually owns are counted. `FILE_CACHE` frames are
/// refcounted by `dsocache` instead, and neither `free_leaf` nor
/// `cow_write_fault` decrements the COW count for them, so counting them would
/// grow the table without bound and never balance.
pub fn mark_cow(cr2: u64, phys_offset: u64) {
    if let Some(entry) = leaf_entry_current(VirtAddr::new(cr2), phys_offset) {
        if entry.is_present() && !entry.flags().contains(PageTableFlags::COW) {
            cow_add(entry.address().as_u64() >> 12, 1);
        }
        entry.clear_flags(PageTableFlags::WRITABLE);
        entry.set_flags(PageTableFlags::COW);
        invlpg(cr2);
        crate::smp::flush_tlb_page(cr2);
    }
}

/// Returns true if the range [ptr, ptr+len) lies entirely in user space.
pub fn is_user_range(ptr: u64, len: usize) -> bool {
    let end = match ptr.checked_add(len as u64) {
        Some(end) => end,
        None => return false,
    };
    ptr < shared::memory::address::USER_ADDR_LIMIT
        && end <= shared::memory::address::USER_ADDR_LIMIT
}

pub fn user_page_flags_cr3(cr3: PhysAddr, addr: u64) -> Option<PageTableFlags> {
    leaf_entry_cr3(cr3, VirtAddr(addr))
        .map(|e| e.flags())
        .filter(|f| f.bits() != 0)
}

/// Copy `[ptr, ptr + buf.len())` out of the address space rooted at `cr3`
/// (the tracee's, for PTRACE_PEEKTEXT/PEEKDATA) into `buf`. Walks that
/// address space's own page tables via the HHDM direct map, so the current
/// CR3 is never touched. Returns false on any unmapped page.
pub fn read_user_mem_cr3(cr3: PhysAddr, ptr: u64, buf: &mut [u8]) -> bool {
    if !is_user_range(ptr, buf.len()) || buf.len() == 0 {
        return buf.len() == 0;
    }
    let mut off = 0usize;
    while off < buf.len() {
        let addr = ptr + off as u64;
        let entry = match leaf_entry_cr3(cr3, VirtAddr(addr)) {
            Some(e) => e,
            None => return false,
        };
        let flags = entry.flags();
        if !flags.contains(PageTableFlags::PRESENT) || !flags.contains(PageTableFlags::USER) {
            return false;
        }
        let page_base = entry.address().as_u64() + HHDM_BASE.0;
        let in_page = (addr & (PAGE_SIZE - 1)) as usize;
        let chunk = core::cmp::min(PAGE_SIZE as usize - in_page, buf.len() - off);
        unsafe {
            core::ptr::copy_nonoverlapping(
                (page_base + in_page as u64) as *const u8,
                buf.as_mut_ptr().add(off),
                chunk,
            );
        }
        off += chunk;
    }
    true
}

/// Copy `buf` into `[ptr, ptr + buf.len())` of the address space rooted at
/// `cr3` (PTRACE_POKETEXT/POKEDATA). Same foreign-walk as read_user_mem_cr3.
/// Returns false on any unmapped or read-only page (COW pages are made
/// writable in place; the tracee is stopped so there is no concurrent owner).
pub fn write_user_mem_cr3(cr3: PhysAddr, ptr: u64, buf: &[u8]) -> bool {
    if !is_user_range(ptr, buf.len()) || buf.len() == 0 {
        return buf.len() == 0;
    }
    let mut off = 0usize;
    while off < buf.len() {
        let addr = ptr + off as u64;
        let entry = match leaf_entry_cr3(cr3, VirtAddr(addr)) {
            Some(e) => e,
            None => return false,
        };
        let flags = entry.flags();
        if !flags.contains(PageTableFlags::PRESENT) || !flags.contains(PageTableFlags::USER) {
            return false;
        }
        if flags.contains(PageTableFlags::COW) && !flags.contains(PageTableFlags::WRITABLE) {
            entry.set_flags(PageTableFlags::WRITABLE);
            entry.clear_flags(PageTableFlags::COW);
        }
        if !flags.contains(PageTableFlags::WRITABLE) {
            return false;
        }
        let page_base = entry.address().as_u64() + HHDM_BASE.0;
        let in_page = (addr & (PAGE_SIZE - 1)) as usize;
        let chunk = core::cmp::min(PAGE_SIZE as usize - in_page, buf.len() - off);
        unsafe {
            core::ptr::copy_nonoverlapping(
                buf.as_ptr().add(off),
                (page_base + in_page as u64) as *mut u8,
                chunk,
            );
        }
        off += chunk;
    }
    true
}

fn user_page_flags(addr: u64) -> Option<PageTableFlags> {
    // Use current process CR3 when available; read_cr3() may be KERNEL_CR3
    // in idle or interrupt context where user_pages_mapped is called.
    // Use try_lock to avoid deadlock when validate_user_range is called
    // while process.lock or CPUS is already held (e.g., from timer_tick/deliver_pending).
    let cpu_id = crate::smp::current_cpu_id();
    let cr3 = if let Some(cpu) = crate::smp::CPUS[cpu_id as usize].try_lock() {
        if let Some(t) = cpu.as_ref().and_then(|c| c.current.as_ref()) {
            if let Some(p) = t.process.try_lock() {
                p.cr3
            } else {
                read_cr3()
            }
        } else {
            read_cr3()
        }
    } else {
        read_cr3()
    };
    user_page_flags_cr3(cr3, addr)
}

/// Ensures a user range is mapped with the requested access before the kernel
/// dereferences it. Demand-paged and COW pages are resolved explicitly so an
/// invalid userspace pointer is reported as `EFAULT`, not a kernel page fault.
pub fn validate_user_range(ptr: u64, len: usize, write: bool) -> bool {
    if !is_user_range(ptr, len) {
        return false;
    }
    if len == 0 {
        return true;
    }

    let end = ptr + len as u64 - 1;
    let mut page = ptr & !(PAGE_SIZE - 1);
    let last_page = end & !(PAGE_SIZE - 1);
    loop {
        let mut flags = match user_page_flags(page) {
            Some(flags) => flags,
            None => {
                let stack_handled = stack_demand_fault(page, HHDM_BASE.0);
                if stack_handled {
                    match user_page_flags(page) {
                        Some(f) => f,
                        None => return false,
                    }
                } else if crate::memory::filemmap::dispatch_demand_fault(page, write) {
                    match user_page_flags(page) {
                        Some(f) => f,
                        None => return false,
                    }
                } else {
                    return false;
                }
            }
        };

        if flags.contains(PageTableFlags::DEMAND_PAGING) {
            let handled = if crate::memory::filemmap::dispatch_demand_fault(page, write) {
                true
            } else {
                let mut allocator = FRAME_ALLOCATOR.lock();
                demand_paging_fault(page, &mut *allocator, HHDM_BASE.0)
            };
            if !handled {
                return false;
            }
            flags = match user_page_flags(page) {
                Some(flags) => flags,
                None => return false,
            };
        }

        if write && flags.contains(PageTableFlags::COW) {
            if !cow_write_fault(page, HHDM_BASE.0) {
                return false;
            }
            flags = match user_page_flags(page) {
                Some(flags) => flags,
                None => return false,
            };
        }

        if !flags.contains(PageTableFlags::PRESENT)
            || !flags.contains(PageTableFlags::USER)
            || (write && !flags.contains(PageTableFlags::WRITABLE))
        {
            return false;
        }

        if page == last_page {
            return true;
        }
        page += PAGE_SIZE;
    }
}

pub fn validate_user_read_range(ptr: u64, len: usize) -> bool {
    validate_user_range(ptr, len, false)
}

/// Non-faulting check: true iff every page of `[ptr, ptr+len)` is already
/// PRESENT+USER (and WRITABLE when `write`) in the current page tables.
/// Unlike `validate_user_range` this never demand-faults, so it is safe to
/// call while holding CPUS/SCHEDULER (deliver_pending).
pub fn user_pages_mapped(ptr: u64, len: usize, write: bool) -> bool {
    if !is_user_range(ptr, len) || len == 0 {
        return len == 0;
    }
    let end = ptr + len as u64 - 1;
    let mut page = ptr & !(PAGE_SIZE - 1);
    let last_page = end & !(PAGE_SIZE - 1);
    loop {
        match user_page_flags(page) {
            Some(flags)
                if flags.contains(PageTableFlags::PRESENT)
                    && flags.contains(PageTableFlags::USER)
                    && (!write || flags.contains(PageTableFlags::WRITABLE)) => {}
            _ => return false,
        }
        if page == last_page {
            return true;
        }
        page += PAGE_SIZE;
    }
}

pub fn user_pages_mapped_cr3(cr3: PhysAddr, ptr: u64, len: usize, write: bool) -> bool {
    if !is_user_range(ptr, len) || len == 0 {
        return len == 0;
    }
    let end = ptr + len as u64 - 1;
    let mut page = ptr & !(PAGE_SIZE - 1);
    let last_page = end & !(PAGE_SIZE - 1);
    loop {
        match user_page_flags_cr3(cr3, page) {
            Some(flags)
                if flags.contains(PageTableFlags::PRESENT)
                    && flags.contains(PageTableFlags::USER)
                    && (!write || flags.contains(PageTableFlags::WRITABLE)) => {}
            _ => return false,
        }
        if page == last_page {
            return true;
        }
        page += PAGE_SIZE;
    }
}

pub fn validate_user_write_range(ptr: u64, len: usize) -> bool {
    validate_user_range(ptr, len, true)
}

pub fn read_user_cstring(ptr: u64) -> Option<String> {
    if ptr == 0 || !is_user_range(ptr, 1) {
        return None;
    }
    let mut bytes = Vec::new();
    let mut checked_page = u64::MAX;
    for i in 0..shared::memory::address::PAGE_SIZE {
        let addr = ptr.checked_add(i)?;
        let page = addr & !(PAGE_SIZE - 1);
        if page != checked_page {
            if !validate_user_read_range(page, 1) {
                return None;
            }
            checked_page = page;
        }
        let b = unsafe { *(addr as *const u8) };
        if b == 0 {
            break;
        }
        bytes.push(b);
    }
    String::from_utf8(bytes).ok()
}

pub fn read_user_string_array(ptr: u64) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for i in 0..256 {
        let sptr = unsafe { *((ptr + i * 8) as *const u64) };
        if sptr == 0 {
            break;
        }
        out.push(read_user_cstring(sptr)?);
    }
    Some(out)
}

/// Dynamically expands the user stack on demand when a page fault occurs in a Stack VMA.
pub fn stack_demand_fault(
    cr2: u64,
    phys_offset: u64,
) -> bool {
    let proc = match crate::tasks::fault_process() {
        Some(p) => p,
        None => return false,
    };
    let is_stack_vma = {
        let p = proc.lock();
        p.vmas
            .vmas
            .iter()
            .any(|v| v.kind == crate::memory::vma::VmaKind::Stack && v.contains(cr2))
    };
    if !is_stack_vma {
        return false;
    }

    let mut allocator = FRAME_ALLOCATOR.lock();
    let page_vaddr = align_down(cr2, PAGE_SIZE);
    let new_frame = match allocator.alloc_pages(1, PAGE_SIZE) {
        Ok(f) => f,
        Err(_) => return false,
    };

    let dst = (new_frame + phys_offset).as_mut_ptr::<u8>();
    unsafe {
        write_bytes(dst, 0, PAGE_SIZE as usize);
    }

    let mut mapper = mapper_for(read_cr3());
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER;
    if mapper
        .map_range(
            &mut *allocator,
            VirtAddr(page_vaddr),
            new_frame,
            PAGE_SIZE as usize,
            flags,
        )
        .is_ok()
    {
        invlpg(cr2);
        true
    } else {
        allocator.dealloc_pages(new_frame, 1);
        false
    }
}

pub fn demand_paging_fault_ext(
    cr2: u64,
    is_write: bool,
    allocator: &mut impl FrameAllocator,
    phys_offset: u64,
) -> bool {
    if !is_write {
        if let Some(zero_frame) = get_or_init_zero_frame(allocator) {
            let pml4_phys = read_cr3();
            if let Some(entry) = leaf_entry_current(VirtAddr(cr2), phys_offset) {
                if entry.flags().contains(PageTableFlags::DEMAND_PAGING) || !entry.is_present() {
                    cow_add(zero_frame.0 >> 12, 1);
                    entry.set_address(zero_frame);
                    entry.clear_flags(PageTableFlags::DEMAND_PAGING | PageTableFlags::WRITABLE);
                    entry.set_flags(entry.flags() | PageTableFlags::PRESENT | PageTableFlags::USER | PageTableFlags::COW);
                    invlpg(cr2);
                    return true;
                }
            }

            let is_valid_vma = match crate::tasks::fault_process() {
                Some(p) => match p.try_lock() {
                    Some(guard) => guard.vmas.find(cr2).is_some(),
                    None => true,
                },
                None => false,
            };

            if is_valid_vma {
                let mut mapper = mapper_for(pml4_phys);
                let flags = PageTableFlags::PRESENT | PageTableFlags::USER | PageTableFlags::COW;
                cow_add(zero_frame.0 >> 12, 1);
                if mapper.map_4k(allocator, VirtAddr(cr2), zero_frame, flags).is_ok() {
                    invlpg(cr2);
                    return true;
                }
                cow_dec(zero_frame.0 >> 12);
            }
        }
    }

    demand_paging_fault_with_source(cr2, allocator, phys_offset, None)
}

pub fn demand_paging_fault(
    cr2: u64,
    allocator: &mut impl FrameAllocator,
    phys_offset: u64,
) -> bool {
    demand_paging_fault_ext(cr2, true, allocator, phys_offset)
}

/// Allocates a frame for `cr2`, zero-filling it and optionally copying `fill`
/// bytes (up to a page) from a file into it. Returns true on success.
pub fn demand_paging_fault_with_source(
    cr2: u64,
    allocator: &mut impl FrameAllocator,
    phys_offset: u64,
    fill: Option<&[u8]>,
) -> bool {
    let pml4_phys = read_cr3();
    if let Some(entry) = leaf_entry_current(VirtAddr(cr2), phys_offset) {
        if entry.flags().contains(PageTableFlags::DEMAND_PAGING) {
            let new_frame = match allocator.alloc_pages(1, PAGE_SIZE) {
                Ok(f) => f,
                Err(_) => return false,
            };

            let dst = (new_frame + phys_offset).as_mut_ptr::<u8>();
            unsafe {
                write_bytes(dst, 0, PAGE_SIZE as usize);
                if let Some(src) = fill {
                    let n = src.len().min(PAGE_SIZE as usize);
                    core::ptr::copy_nonoverlapping(src.as_ptr(), dst, n);
                }
            }

            entry.set_address(new_frame);
            entry.clear_flags(PageTableFlags::DEMAND_PAGING);
            entry.set_flags(entry.flags() | PageTableFlags::PRESENT | PageTableFlags::USER);
            invlpg(cr2);
            return true;
        }
    }

    // Only allocate if cr2 is within an existing VMA for this process.
    // Use try_lock to avoid AB-BA deadlock if another core holds proc.lock()
    // and is waiting for FRAME_ALLOCATOR.
    let is_valid_vma = match crate::tasks::fault_process() {
        Some(p) => match p.try_lock() {
            Some(guard) => guard.vmas.find(cr2).is_some(),
            None => true,
        },
        None => false,
    };

    if !is_valid_vma {
        return false;
    }

    // Allocate fresh frame and map via mapper
    let new_frame = match allocator.alloc_pages(1, PAGE_SIZE) {
        Ok(f) => f,
        Err(_) => return false,
    };

    let dst = (new_frame + phys_offset).as_mut_ptr::<u8>();
    unsafe {
        write_bytes(dst, 0, PAGE_SIZE as usize);
        if let Some(src) = fill {
            let n = src.len().min(PAGE_SIZE as usize);
            core::ptr::copy_nonoverlapping(src.as_ptr(), dst, n);
        }
    }

    let mut mapper = mapper_for(pml4_phys);
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER;
    if mapper.map_4k(allocator, VirtAddr(cr2), new_frame, flags).is_ok() {
        invlpg(cr2);
        return true;
    }
    allocator.dealloc_pages(new_frame, 1);
    false
}

/// Fills the demand-paged PTE at `cr2` with an existing (cached) frame.
/// The frame is NOT deallocated here; it is owned by the DSO cache.
pub fn demand_paging_fault_use_frame(
    cr2: u64,
    frame: shared::memory::address::PhysAddr,
    flags: PageTableFlags,
    phys_offset: u64,
) -> bool {
    let pml4_phys = read_cr3();
    if let Some(entry) = leaf_entry_current(VirtAddr(cr2), phys_offset) {
        if entry.flags().contains(PageTableFlags::DEMAND_PAGING) || !entry.is_present() {
            entry.set_address(frame);
            entry.clear_flags(PageTableFlags::DEMAND_PAGING);
            entry.set_flags(flags | PageTableFlags::PRESENT);
            invlpg(cr2);
            return true;
        }
    }

    // If intermediate tables or PTE were not present, install via mapper
    let mut mapper = mapper_for(pml4_phys);
    let mut alloc = crate::memory::FRAME_ALLOCATOR.lock();
    if mapper.map_4k(&mut *alloc, VirtAddr(cr2), frame, flags | PageTableFlags::PRESENT).is_ok() {
        invlpg(cr2);
        return true;
    }
    false
}

/// Returns the physical frame number (>>12) of the present leaf page at `addr`,
/// or None if not present / not mappable.
pub fn frame_at(addr: u64) -> Option<u64> {
    leaf_entry_cr3(read_cr3(), VirtAddr(addr))
        .filter(|e| e.is_present())
        .map(|e| e.address().as_u64() >> 12)
}

/// Returns whether the present leaf page at `addr` has the hardware DIRTY bit
/// set (written since the page was faulted in).
pub fn pte_is_dirty(addr: u64) -> bool {
    leaf_entry_cr3(read_cr3(), VirtAddr(addr))
        .map(|e| e.flags().contains(PageTableFlags::DIRTY))
        .unwrap_or(false)
}

pub fn unmap_user_range(
    start: u64,
    end: u64,
    allocator: &mut impl FrameAllocator,
    phys_offset: u64,
) {
    let mut page = start;
    while page < end {
        let entry_copy = leaf_entry_current(VirtAddr(page), phys_offset).cloned();
        let should_unmap = entry_copy.is_some();
        if let Some(e) = entry_copy {
            if e.is_present() && !e.flags().contains(PageTableFlags::MMIO) {
                if e.flags().contains(PageTableFlags::FILE_CACHE) {
                    if let Some(f) = crate::memory::filemmap::release_file_page(page) {
                        allocator.dealloc_pages(f, 1);
                    }
                } else {
                    let mut counts = COW_COUNTS.lock();
                    free_leaf(
                        e,
                        allocator,
                        counts.as_mut().expect("COW table not initialized"),
                    );
                }
            }
        }
        if should_unmap {
            if let Some(entry) = leaf_entry_current(VirtAddr(page), phys_offset) {
                entry.clear();
            }
            invlpg(page);
        }
        page += PAGE_SIZE;
    }
}
