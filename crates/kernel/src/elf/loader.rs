use goblin::{
    elf::Elf,
    elf64::{
        header::ET_DYN,
        program_header::PT_LOAD,
    },
};
use shared::memory::{
    address::{PAGE_SIZE, VirtAddr, align_down, align_up},
    allocator::FrameAllocator,
    flags::{PageTableFlags, prot_to_flags},
};

use crate::{
    arch::x86_64::cpu::read_cr3,
    elf::stack::prepare_user_stack,
    memory::{
        dsocache,
        space::{mapper_for, mark_cow},
        vma::{Vma, VmaKind, MAP_PRIVATE},
    },
};

fn elf_prot(p_flags: u64) -> u64 {
    let mut p = 0;
    if p_flags & 0x4 != 0 {
        p |= 1;
    }
    if p_flags & 0x2 != 0 {
        p |= 2;
    }
    if p_flags & 0x1 != 0 {
        p |= 4;
    }
    p
}

/// Randomized base offset for ASLR.
/// PIE executables get a random base in the upper user space region.
/// The interpreter gets a small page-aligned random offset.
#[allow(dead_code)]
fn aslr_base_offset() -> u64 {
    // Use the boot RNG seed for per-boot randomness.
    // Fallback to a fixed default if not available.
    unsafe { crate::BOOT_RNG_SEED & 0x3ffffffffffu64 }
}

/// Loads a single ELF image's PT_LOAD segments into the current address space
/// at `base_offset`, caching pages in the DSO cache if `interp_inode` is Some.
/// Returns (entry_vaddr, phdr_vaddr).
fn load_image(
    file_data: &[u8],
    allocator: &mut impl FrameAllocator,
    base_offset: u64,
    interp_inode: Option<u64>,
) -> Option<(u64, u64, u64)> {
    let elf = match Elf::parse(file_data) {
        Ok(elf) => elf,
        Err(e) => {
            crate::println!("Error while loading elf file: {}", e);
            return None;
        }
    };

    let mut mapper = mapper_for(read_cr3());
    let phoff = elf.header.e_phoff;
    let mut phdr_vaddr = base_offset + phoff;
    let mut prot_updates: alloc::vec::Vec<(u64, PageTableFlags, bool)> = alloc::vec::Vec::new();

    let mut prev_end_page: Option<u64> = None;

    for phdr in &elf.program_headers {
        if phdr.p_type == PT_LOAD {
            let vaddr = phdr.p_vaddr + base_offset;
            let memsz = phdr.p_memsz as usize;
            let filesz = phdr.p_filesz as usize;

            let start_page = align_down(vaddr, PAGE_SIZE);
            let end_page = align_up(vaddr + memsz as u64, PAGE_SIZE);
            let size_to_map = (end_page - start_page) as usize;
            let prot = elf_prot(phdr.p_flags as u64);

            if let Some(prev_end) = prev_end_page {
                if start_page > prev_end {
                    // Register gap between PT_LOAD segments for dynamic linker malloc donation
                    let gap_vma = Vma {
                        start: prev_end,
                        end: start_page,
                        prot: crate::memory::vma::PROT_READ | crate::memory::vma::PROT_WRITE,
                        flags: MAP_PRIVATE | crate::memory::vma::MAP_ANONYMOUS,
                        kind: VmaKind::Anonymous,
                    };
                    if let Some(t) = crate::tasks::current_thread() {
                        let mut p = t.process.lock();
                        let _ = p.vmas.insert(gap_vma);
                    }
                    crate::memory::space::map_demand_range(
                        read_cr3(),
                        prev_end,
                        start_page,
                        PageTableFlags::USER | PageTableFlags::WRITABLE,
                        allocator,
                    );
                }
            }
            prev_end_page = Some(end_page);

            // Record VMA for the loaded segment
            let vma = Vma {
                start: start_page,
                end: end_page,
                prot,
                flags: MAP_PRIVATE,
                kind: if let Some(inode) = interp_inode {
                    VmaKind::MappedFile {
                        inode,
                        offset: phdr.p_offset,
                    }
                } else {
                    VmaKind::Anonymous
                },
            };
            if let Some(t) = crate::tasks::current_thread() {
                let mut p = t.process.lock();
                let _ = p.vmas.insert(vma);
            }

            if let Some(inode) = interp_inode {
                let is_writable = (prot & 2) != 0;
                // Map page-by-page consulting DSO cache only for read-only segments
                let mut curr_vaddr = start_page;
                while curr_vaddr < end_page {
                    let page_idx = (curr_vaddr - start_page) / PAGE_SIZE;
                    let file_page_off = align_down(phdr.p_offset + page_idx * PAGE_SIZE, PAGE_SIZE);
                    let final_flags = prot_to_flags(prot)
                        | PageTableFlags::PRESENT
                        | PageTableFlags::USER
                        | if is_writable { PageTableFlags::NONE } else { PageTableFlags::FILE_CACHE };
                    let load_flags = final_flags | PageTableFlags::WRITABLE;

                    if !is_writable && let Some(cached_frame) = dsocache::lookup_and_retain(inode, file_page_off) {
                        let _ = mapper.map_range(
                            allocator,
                            VirtAddr(curr_vaddr),
                            cached_frame,
                            PAGE_SIZE as usize,
                            load_flags,
                        );
                        prot_updates.push((curr_vaddr, final_flags, false));
                    } else {
                        // Allocate fresh frame
                        let phys_frame = allocator.alloc_pages(1, PAGE_SIZE).unwrap();
                        let _ = mapper.map_range(
                            allocator,
                            VirtAddr(curr_vaddr),
                            phys_frame,
                            PAGE_SIZE as usize,
                            load_flags,
                        );

                        // Copy segment bytes into page
                        let page_start_in_seg = if curr_vaddr < vaddr {
                            0
                        } else {
                            (curr_vaddr - vaddr) as usize
                        };
                        let page_end_in_seg = ((curr_vaddr + PAGE_SIZE) - vaddr) as usize;
                        let copy_start = page_start_in_seg.min(filesz);
                        let copy_end = page_end_in_seg.min(filesz);

                        unsafe {
                            let dest = curr_vaddr as *mut u8;
                            core::ptr::write_bytes(dest, 0, PAGE_SIZE as usize);
                            if copy_end > copy_start {
                                let src_offset = phdr.p_offset as usize + copy_start;
                                let src = file_data[src_offset..src_offset + (copy_end - copy_start)].as_ptr();
                                let dest_page_offset = if curr_vaddr < vaddr {
                                    (vaddr - curr_vaddr) as usize
                                } else {
                                    0
                                };
                                core::ptr::copy_nonoverlapping(src, dest.add(dest_page_offset), copy_end - copy_start);
                            }
                        }

                        if !is_writable {
                            // Register read-only pages in DSO cache
                            let canonical = dsocache::retain(inode, file_page_off, phys_frame, None);
                            if canonical != phys_frame {
                                allocator.dealloc_pages(phys_frame, 1);
                            }
                        }

                        prot_updates.push((curr_vaddr, final_flags, false));
                    }
                    curr_vaddr += PAGE_SIZE;
                }
            } else {
                // Regular binary segment loading
                let phys_start = allocator
                    .alloc_pages(size_to_map / PAGE_SIZE as usize, PAGE_SIZE)
                    .unwrap();

                let final_flags =
                    prot_to_flags(prot) | PageTableFlags::PRESENT | PageTableFlags::USER;
                let load_flags = final_flags | PageTableFlags::WRITABLE;
                mapper
                    .map_range(
                        allocator,
                        VirtAddr(start_page),
                        phys_start,
                        size_to_map,
                        load_flags,
                    )
                    .expect("Failed to map ELF segment!");

                unsafe {
                    core::ptr::write_bytes(start_page as *mut u8, 0, size_to_map);
                    let dest = vaddr as *mut u8;
                    let src = file_data[phdr.p_offset as usize..phdr.p_offset as usize + filesz].as_ptr();
                    if filesz > 0 {
                        core::ptr::copy_nonoverlapping(src, dest, filesz);
                    }
                }

                for page in (start_page..end_page).step_by(PAGE_SIZE as usize) {
                    prot_updates.push((page, final_flags, false));
                }
            }

            if phoff >= phdr.p_offset && phoff < phdr.p_offset + phdr.p_filesz {
                phdr_vaddr = (phdr.p_vaddr + base_offset) + (phoff - phdr.p_offset);
            }
        }
    }

    // Process RELA relocations for static PIE binaries before stripping WRITABLE.
    // Dynamically-linked binaries (with PT_INTERP) and the dynamic linker itself
    // perform their own relocations in userspace.
    if elf.header.e_type == 3 /* ET_DYN */ && elf.interpreter.is_none() && interp_inode.is_none() {
        apply_relocations(&elf, base_offset);
    }

// Ensure the entry point page is present and executable even if it falls
    // in a gap between PT_LOAD segments (which are mapped demand-paged RW only).
    let entry_vaddr = elf.entry + base_offset;
    let entry_page = align_down(entry_vaddr, PAGE_SIZE);
    let mut entry_in_segment = false;
    let mut entry_seg_prot = 0u64;
    for phdr in &elf.program_headers {
        if phdr.p_type == PT_LOAD {
            let seg_start = phdr.p_vaddr + base_offset;
            let seg_end = seg_start + phdr.p_memsz as u64;
            if entry_vaddr >= seg_start && entry_vaddr < seg_end {
                entry_in_segment = true;
                entry_seg_prot = elf_prot(phdr.p_flags as u64);
                break;
            }
        }
    }
    let entry_flags = if entry_in_segment {
        prot_to_flags(entry_seg_prot)
    } else {
        // Make the entry page RX: prot_to_flags strips NO_EXECUTE so the page
        // becomes executable when update_prot_4k clears it; add USER so the page
        // is user-mappable (the segment mapping already set USER).
        prot_to_flags(1) // PROT_READ → USER|WRITABLE via update_prot_4k, NO_EXECUTE cleared → executable
    };
    prot_updates.push((entry_page, entry_flags | PageTableFlags::PRESENT, false));

    // Now apply final page protections.
    for (page_vaddr, final_flags, cow) in prot_updates {
        let _ = mapper.update_prot_4k(VirtAddr(page_vaddr), final_flags);
        if cow {
            mark_cow(page_vaddr, shared::boot::boot_info::HHDM_BASE.0);
        }
    }

    Some((elf.entry + base_offset, phdr_vaddr, prev_end_page.unwrap_or(0)))
}

/// Applies RELA relocations for a loaded ELF image.
/// Handles R_X86_64_RELATIVE (type 8) and R_X86_64_64/GLOB_DAT/JUMP_SLOT.
/// Processes both DT_RELA and DT_JMPREL tables before RELRO is applied.
fn apply_relocations(elf: &Elf, base: u64) {
    let dyn_phdr = match elf.program_headers.iter().find(|p| p.p_type == goblin::elf::program_header::PT_DYNAMIC) {
        Some(p) => p,
        None => return,
    };

    let dyn_vaddr = dyn_phdr.p_vaddr + base;
    let dyn_sz = dyn_phdr.p_memsz as usize;
    let dyn_count = dyn_sz / 16;

    let mut rela_vaddr = 0u64;
    let mut rela_sz = 0u64;
    let mut jmprel_vaddr = 0u64;
    let mut jmprel_sz = 0u64;

    for i in 0..dyn_count {
        let off = dyn_vaddr as usize + i * 16;
        let tag = unsafe { core::ptr::read_volatile(off as *const u64) };
        let val = unsafe { core::ptr::read_volatile((off + 8) as *const u64) };
        match tag {
            7 => rela_vaddr = val,    // DT_RELA
            8 => rela_sz = val,        // DT_RELASZ
            23 => jmprel_vaddr = val,  // DT_JMPREL
            2 => jmprel_sz = val,      // DT_PLTRELSZ
            _ => {}
        }
    }

    // Helper to apply a single RELA table.
    let apply_table = |table_vaddr: u64, table_sz: u64| {
        if table_vaddr == 0 || table_sz == 0 {
            return;
        }
        let count = table_sz as usize / 24;
        for i in 0..count {
            let off = (table_vaddr + base) as usize + i * 24;
            let r_offset = unsafe { core::ptr::read_volatile(off as *const u64) };
            let r_info = unsafe { core::ptr::read_volatile((off + 8) as *const u64) };
            let r_addend = unsafe { core::ptr::read_volatile((off + 16) as *const i64) };
            let r_type = (r_info & 0xffffffff) as u32;
            let target_vaddr = r_offset + base;
            match r_type {
                8 => unsafe {
                    core::ptr::write_volatile(target_vaddr as *mut u64, base.wrapping_add(r_addend as u64));
                },
                6 | 7 | 1 if (r_info >> 32) == 0 => unsafe {
                    core::ptr::write_volatile(target_vaddr as *mut u64, base.wrapping_add(r_addend as u64));
                },
                _ => {}
            }
        }
    };

    apply_table(rela_vaddr, rela_sz);
    apply_table(jmprel_vaddr, jmprel_sz);
}

pub fn load_elf(
    file_data: &[u8],
    _file_inode: Option<u64>,
    interp_data: Option<&[u8]>,
    interp_inode: Option<u64>,
    allocator: &mut impl FrameAllocator,
    args: &[&str],
    envs: &[&str],
) -> Option<(u64, u64, u64)> {
    let elf = match Elf::parse(file_data) {
        Ok(elf) => elf,
        Err(e) => {
            crate::println!("Error while loading elf file: {}", e);
            return None;
        }
    };
    let main_base = if elf.header.e_type == ET_DYN {
        0x5555_5555_4000u64
    } else {
        0
    };

    let interp_base = 0x7f00_0000_0000u64;

    let (main_entry, main_phdr, main_end) = load_image(file_data, allocator, main_base, None)?;
    crate::dprintln!("[load_elf] main_entry={:#x} main_base={:#x} interp={:?}", main_entry, main_base, elf.interpreter);

    let (final_entry, aux_base, aux_phdr, aux_phnum, aux_phent, aux_entry) =
        if let (Some(_interp), Some(interp_data), Some(interp_inode)) =
            (elf.interpreter, interp_data, interp_inode)
        {
            let (interp_entry, _, _) = load_image(interp_data, allocator, interp_base, Some(interp_inode))?;
            crate::dprintln!("[load_elf] interp_entry={:#x} interp_base={:#x}", interp_entry, interp_base);
            (
                interp_entry,
                interp_base,
                main_phdr,
                elf.header.e_phnum as u64,
                elf.header.e_phentsize as u64,
                main_entry,
            )
        } else {
            crate::dprintln!("[load_elf] no interp, final=main {:#x}", main_entry);
            (
                main_entry,
                main_base,
                main_phdr,
                elf.header.e_phnum as u64,
                elf.header.e_phentsize as u64,
                main_entry,
            )
        };
    crate::dprintln!("[load_elf] final_entry={:#x} aux_base={:#x}", final_entry, aux_base);

    crate::dprintln!("Mapping dynamic user stack...");

    let initial_stack_pages = 4; // Initial 16KB stack
    let initial_stack_size = initial_stack_pages * PAGE_SIZE as usize;

    let stack_top = 0x0000_7FFF_FFFF_F000u64;
    let initial_stack_bottom = stack_top - initial_stack_size as u64;
    let max_stack_size = 8 * 1024 * 1024u64; // 8MB limit
    let stack_limit_bottom = stack_top - max_stack_size;

    // Register Stack VMA for dynamic growth
    if let Some(t) = crate::tasks::current_thread() {
        let mut p = t.process.lock();
        let _ = p.vmas.insert(Vma {
            start: stack_limit_bottom,
            end: stack_top,
            prot: crate::memory::vma::PROT_READ | crate::memory::vma::PROT_WRITE,
            flags: crate::memory::vma::MAP_PRIVATE | crate::memory::vma::MAP_ANONYMOUS,
            kind: VmaKind::Stack,
        });
    }

    let mut mapper = mapper_for(read_cr3());
    let stack_phys = allocator.alloc_pages(initial_stack_pages, PAGE_SIZE).unwrap();
    let stack_flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER;

    mapper
        .map_range(
            allocator,
            VirtAddr(initial_stack_bottom),
            stack_phys,
            initial_stack_size,
            stack_flags,
        )
        .expect("Failed to map user stack!");

    let random_ptr = stack_top - 16;
    unsafe {
        core::ptr::write_bytes(random_ptr as *mut u8, 0x42, 16);
    }

    let final_rsp = unsafe {
        prepare_user_stack(
            random_ptr,
            args,
            envs,
            aux_phdr,
            aux_phent,
            aux_phnum,
            aux_entry,
            random_ptr,
            aux_base,
        )
    };

    Some((final_entry, final_rsp, main_end))
}
