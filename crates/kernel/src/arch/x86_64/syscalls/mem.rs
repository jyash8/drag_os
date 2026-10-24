use super::*;

pub(super) fn sys_mmap(addr: u64, len: usize, prot: u64, flags: u64) -> u64 {
    if len == 0 {
        return EINVAL;
    }
    if (flags & MAP_DEVICE) != 0 {
        return mmap_device(addr, len, prot, flags);
    }
    if (flags & MAP_ANONYMOUS) == 0 {
        return EINVAL;
    }
    if (flags & MAP_FIXED) != 0 && addr & PAGE_SIZE - 1 != 0 {
        return EINVAL;
    }
    let aligned_len = align_up(len as u64, PAGE_SIZE);

    let (vaddr, need_unmap, cr3) = match with_current_process(|p| {
        let (vaddr, need_unmap) = if (flags & MAP_FIXED) != 0 {
            let removed = p.vmas.remove_range(addr, addr + aligned_len);
            (addr, !removed.is_empty())
        } else {
            let base = p.mmap_base.0;
            p.mmap_base = VirtAddr(base + aligned_len);
            (base, false)
        };

        let vma = Vma {
            start: vaddr,
            end: vaddr + aligned_len,
            prot,
            flags,
            kind: VmaKind::Anonymous,
        };

        if p.vmas.insert(vma).is_err() {
            return Err(());
        }
        Ok((vaddr, need_unmap, p.cr3))
    }) {
        Ok(x) => x,
        Err(_) => return ENOMEM,
    };

    if need_unmap {
        let mut alloc = FRAME_ALLOCATOR.lock();
        unmap_user_range(vaddr, vaddr + aligned_len, &mut *alloc, HHDM_BASE.0);
    }

    let mut alloc = FRAME_ALLOCATOR.lock();
    map_demand_range(cr3, vaddr, vaddr + aligned_len, prot_to_flags(prot), &mut *alloc);
    crate::dprintln!("[mmap anon] addr={:#x} len={:#x} prot={:#x} flags={:#x} -> ret={:#x}", addr, len, prot, flags, vaddr);
    vaddr
}

pub(super) fn mmap_device(phys: u64, len: usize, prot: u64, flags: u64) -> u64 {
    let start = align_down(phys, PAGE_SIZE);
    let aligned_len = align_up(len as u64, PAGE_SIZE);

    let (vaddr, cr3) = match with_current_process(|p| {
        let vaddr = p.mmap_base.0;
        p.mmap_base = VirtAddr(vaddr + aligned_len);
        let vma = Vma {
            start: vaddr,
            end: vaddr + aligned_len,
            prot,
            flags,
            kind: VmaKind::Device { phys: start },
        };
        if p.vmas.insert(vma).is_err() {
            return Err(());
        }
        Ok((vaddr, p.cr3))
    }) {
        Ok(x) => x,
        Err(_) => return ENOMEM,
    };

    let pt_flags = prot_to_flags(prot);
    let mut mapper = mapper_for(cr3);
    if mapper
        .map_range(
            &mut *FRAME_ALLOCATOR.lock(),
            VirtAddr(vaddr),
            PhysAddr(start),
            aligned_len as usize,
            pt_flags,
        )
        .is_ok()
    {
        // ponytail: TEMP-DIAG. Verify the endpoints actually resolved to the
        // physical range we asked for. A fault dump showing a Device VMA whose
        // PTEs point somewhere other than its recorded `phys` cannot be
        // diagnosed without knowing whether the mapping was wrong from the
        // start or was overwritten later -- so record it at creation.
        {
            let last = vaddr + aligned_len - PAGE_SIZE;
            for (tag, va, want) in [
                ("first", vaddr, start),
                ("last", last, start + aligned_len - PAGE_SIZE),
            ] {
                let got = crate::memory::space::probe_leaf_pte(cr3, va)
                    .map(|(_, p)| p)
                    .unwrap_or(u64::MAX);
                crate::dprintln!(
                    "[mmap dev] {} vaddr={:#x} want_phys={:#x} got={:#x} len={:#x} prot={:#x} {}",
                    tag, va, want, got, aligned_len, prot,
                    if got == want { "OK" } else { "*** MISMATCH ***" }
                );
            }
        }
        vaddr
    } else {
        ENOMEM
    }
}

pub(super) fn sys_mprotect(addr: u64, len: usize, prot: u64) -> u64 {
    let start = align_down(addr, PAGE_SIZE);
    let end = align_up(addr + len as u64, PAGE_SIZE);
    if start >= end {
        return 0;
    }

    let cr3 = with_current_process(|p| p.vmas.mprotect_range(start, end, prot).then_some(p.cr3));
    let Some(cr3) = cr3 else {
        return 0;
    };

    let pt_flags = prot_to_flags(prot);
    let mut mapper = mapper_for(cr3);
    for page in (start..end).step_by(PAGE_SIZE as usize) {
        let _ = mapper.update_prot_4k(VirtAddr(page), pt_flags);
        invlpg(page);
    }
    0
}

pub(super) fn sys_munmap(addr: u64, len: usize) -> u64 {
    if len == 0 {
        return 0;
    }
    let start = align_down(addr, PAGE_SIZE);
    let end = align_up(addr + len as u64, PAGE_SIZE);

    let removed = with_current_process(|p| p.vmas.remove_range(start, end));
    if removed.is_empty() {
        return 0;
    }
    // Needed to build a Mapper for the device-unmap path below. Taken from the
    // process rather than read_cr3(), which may be KERNEL_CR3 here.
    let cr3 = with_current_process(|p| p.cr3);

    let mut alloc = FRAME_ALLOCATOR.lock();
    for v in &removed {
        if let VmaKind::Device { phys } = v.kind {
            // Device mappings DO get their page-table entries torn down here.
            //
            // This branch used to `continue`, leaving the PTEs mapped after the
            // VMA was gone. That produced a live mapping that no VMA describes,
            // which nothing ever accounted for at teardown, and it is why the
            // compositor's repeated mmap/munmap of the framebuffer grew the
            // page tables without bound (~1040 PTEs per mapping, measured).
            //
            // `unmap_user_range` is deliberately NOT used, because it calls
            // `free_leaf` and would hand the frames back to the allocator --
            // but a device mapping's frames are not ours to free: they are the
            // dumb buffer's backing, still owned by the DRM device and possibly
            // still being scanned out. `Mapper::unmap_4k` only clears the PTE
            // and returns no frames, which is exactly the right semantics.
            //
            // The intermediate tables are left in place: they are shared with
            // sibling mappings, and reclaiming them safely needs the same
            // revalidation work as `destroy_address_space`. Leaking a table is
            // bounded and harmless; freeing a live one is not.
            let mut mapper = mapper_for(cr3);
            let mut page = v.start;
            while page < v.end {
                if mapper.unmap_4k(VirtAddr(page)).is_ok() {
                    invlpg(page);
                }
                page += PAGE_SIZE;
            }
            crate::dprintln!(
                "[munmap dev] {:#x}..{:#x} phys={:#x} PTEs cleared (frames left to the DRM device)",
                v.start, v.end, phys
            );
            continue;
        }
        unmap_user_range(v.start, v.end, &mut *alloc, HHDM_BASE.0);
    }
    0
}

pub(super) fn sys_brk(addr: u64) -> u64 {
    with_current_process(|p| {
        if p.heap_brk.0 == 0 {
            let max_end = p.vmas.vmas.iter()
                .filter(|v| v.kind != VmaKind::Stack && v.start < 0x7000_0000_0000)
                .map(|v| v.end)
                .max()
                .unwrap_or(0x5555_5576_0000);
            p.heap_brk = VirtAddr(max_end);
        }
        if addr == 0 {
            return p.heap_brk.0;
        }
        let current_brk = p.heap_brk.0;
        let _has_heap_vma = p.vmas.has_heap();

        if addr > current_brk {
            let start_page = align_down(current_brk, PAGE_SIZE);
            let end_page = align_up(addr, PAGE_SIZE);
            if end_page > start_page {
                p.vmas.remove_range(start_page, end_page);
                let _ = p.vmas.insert(Vma {
                    start: start_page,
                    end: end_page,
                    prot: 3,
                    flags: 0,
                    kind: VmaKind::Heap,
                });
                let mut alloc = FRAME_ALLOCATOR.lock();
                map_demand_range(
                    p.cr3,
                    start_page,
                    end_page,
                    PageTableFlags::USER | PageTableFlags::WRITABLE,
                    &mut *alloc,
                );
                drop(alloc);
            }
        } else if addr < current_brk {
            let start_page = align_up(addr, PAGE_SIZE);
            let end_page = align_up(current_brk, PAGE_SIZE);
            let removed = p.vmas.remove_range(start_page, end_page);
            let mut alloc = FRAME_ALLOCATOR.lock();
            for v in &removed {
                unmap_user_range(v.start, v.end, &mut *alloc, HHDM_BASE.0);
            }
            drop(alloc);
            if let Some(v) = p.vmas.heap_mut() {
                v.end = addr;
            }
        }
        p.heap_brk = VirtAddr(addr);
        addr
    })
}

pub(super) fn sys_madvise(_addr: u64, _length: usize, _advice: i32) -> u64 {
    0
}
