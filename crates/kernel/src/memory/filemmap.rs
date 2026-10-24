//! File-backed mmap (design: docs/superpowers/specs/2026-08-12-smp-scheduler-design.md)
//!
//! ## Demand-paging model
//!
//! `mmap_file()` records a VMA and returns the reserved address range WITHOUT
//! installing any page-table entries.  The first access to any page in the range
//! triggers a page fault which calls `file_demand_paging_fault()` to:
//!   1. find the covering File VMA,
//!   2. calculate the file offset (with overflow checks),
//!   3. consult the DSO cache: hit → map the cached frame; miss → read up to
//!      PAGE_SIZE bytes from the file (cursor-free) and insert the fresh frame,
//!   4. zero-fill the remainder (partial last page / beyond-EOF),
//!   5. install the PTE with the FILE_CACHE bit set.
//!
//! ## MAP_FIXED limitation
//! MAP_FIXED + overlapping existing VMA → EINVAL.
//! Full POSIX MAP_FIXED semantics (silently tear down overlapping VMAs) are
//! not yet implemented.
//!
//! ## MAP_SHARED
//! MAP_SHARED file pages map the shared cached frame writable in place. Dirty
//! pages (hardware PTE DIRTY bit set by a write) are written back to the file
//! on last unmap / address-space destroy (`release_file_page_from_frame_dirty`).

use alloc::sync::Arc;
use crate::arch::sync::spinlock::SpinLock;
use crate::fs::vfs::{vnode_cache_id, OpenFile, VNode};
use crate::memory::dsocache;
use crate::memory::space::{demand_paging_fault_use_frame, demand_paging_fault_with_source, mark_cow, unmap_user_range};
use crate::memory::vma::{Vma, VmaKind, MAP_PRIVATE};
use crate::memory::FRAME_ALLOCATOR;
use shared::boot::boot_info::HHDM_BASE;
use shared::memory::address::{align_down, align_up, PhysAddr, PAGE_SIZE};
use shared::memory::allocator::FrameAllocator;
use shared::memory::flags::{prot_to_flags, PageTableFlags};

/// Global side-table that keeps file-backed mmap regions alive after close(fd).
///
/// VNode types that are seekable, pageable, regular files (not dirs/pipes/dev).
fn is_regular_vnode(v: &VNode) -> bool {
    matches!(v, VNode::InitrdFile { .. } | VNode::BlockFile { .. } | VNode::FsNode { .. })
}

/// Resolves an fd to its OpenFile in the current process's fd table.
/// `current_thread()` returns an Arc clone — the per-CPU current slot is not
/// disturbed; no `set_current_thread` call is needed after this.
fn openfile_from_fd(fd: u32) -> Option<alloc::sync::Arc<SpinLock<OpenFile>>> {
    let t = crate::tasks::current_thread()?;
    let p = t.process.lock();
    p.fd_table.get(fd as usize)?.clone()
}

/// Full POSIX mmap signature: `mmap(addr, len, prot, flags, fd, offset)`.
/// Returns the mapped virtual address, or an encoded errno on failure.
///
/// No physical frames or page-table entries are installed here.  The VMA is
/// recorded; PTEs are populated lazily on the first page fault.
pub fn mmap_file(
    addr: u64,
    len: usize,
    prot: u64,
    flags: u64,
    fd: u32,
    offset: u64,
) -> u64 {
    use crate::arch::x86_64::syscalls::{EINVAL, EBADF, ENOMEM};
    use crate::memory::vma::{PROT_READ, PROT_WRITE, PROT_EXEC, MAP_FIXED};

    // ponytail: no per-mmap log (every frame/process startup spams serial).
    if len == 0 {
        return EINVAL;
    }

    // Validate prot — only defined bits are accepted.
    let valid_prot = PROT_READ | PROT_WRITE | PROT_EXEC;
    if prot & !valid_prot != 0 {
        return EINVAL;
    }

    let aligned_len = align_up(len as u64, PAGE_SIZE);

    // Overflow-check: reject if addr + aligned_len would wrap.
    if (MAP_FIXED & flags) != 0 {
        if addr % PAGE_SIZE != 0 {
            return EINVAL;
        }
        if addr.checked_add(aligned_len).is_none() {
            return EINVAL;
        }
    }

    let file = match openfile_from_fd(fd) {
        Some(f) => f,
        None => return EBADF,
    };

    // Reject dirs / pipes / char devices.
    if !is_regular_vnode(&file.lock().vnode) {
        return EINVAL;
    }

    // Record the VMA.  No PTEs are installed here.
    let t = crate::tasks::current_thread().expect("mmap with no current thread");
    let (vaddr, need_unmap, cr3, res) = {
        let mut p = t.process.lock();

        let vaddr = if (flags & MAP_FIXED) != 0 {
            addr
        } else {
            let base = p.mmap_base.0;
            // Overflow-check the mmap bump pointer.
            let new_base = match base.checked_add(aligned_len) {
                Some(v) => v,
                None => return ENOMEM,
            };
            p.mmap_base = shared::memory::address::VirtAddr(new_base);
            base
        };

        // MAP_FIXED: silently tear down any existing mappings in the new range,
        // then (re)insert. The musl loader maps a whole DSO and then MAP_FIXEDs
        // individual segments over it, so replacement is required for POSIX.
        let need_unmap = if (flags & MAP_FIXED) != 0 {
            let removed = p.vmas.remove_range(vaddr, vaddr + aligned_len);
            !removed.is_empty()
        } else {
            false
        };

        let vma = Vma {
            start: vaddr,
            end: vaddr + aligned_len,
            prot,
            flags,
            kind: VmaKind::File { file: file.clone(), offset },
        };

        let cr3 = p.cr3;
        let res = match p.vmas.insert(vma) {
            Ok(_) => vaddr,
            Err(_) => ENOMEM,
        };

        (vaddr, need_unmap, cr3, res)
    };

    if need_unmap {
        let mut alloc = FRAME_ALLOCATOR.lock();
        unmap_user_range(vaddr, vaddr + aligned_len, &mut *alloc, HHDM_BASE.0);
    }

    if res == vaddr {
        let mut alloc = FRAME_ALLOCATOR.lock();
        crate::memory::space::map_demand_range(cr3, vaddr, vaddr + aligned_len, prot_to_flags(prot), &mut *alloc);
        unsafe {
            crate::arch::x86_64::cpu::write_cr3(cr3);
        }
    }

    res
}

/// Returns `(file_arc, vma_file_offset, page_index)` for the File VMA covering `cr2`.
/// The `Arc<SpinLock<OpenFile>>` comes directly from the VMA,
/// so it works even after close(fd) and across VMA splits and forks.
fn current_file_vma_for(cr2: u64) -> Option<(Arc<SpinLock<OpenFile>>, u64, u64)> {
    let proc = crate::tasks::fault_process()?;
    let p = proc.lock();
    for v in &p.vmas.vmas {
        if let VmaKind::File { file, offset } = &v.kind {
            if v.contains(cr2) {
                let page = (cr2 - v.start) / PAGE_SIZE as u64;
                return Some((file.clone(), *offset, page));
            }
        }
    }
    None
}

/// Returns `(inode, vma_file_offset, page_index)` for a MappedFile VMA covering `cr2`.
/// Used for kernel-loaded files (interpreter) identified by inode.
fn current_mapped_file_vma_for(cr2: u64) -> Option<(u64, u64, u64)> {
    let proc = crate::tasks::fault_process()?;
    let p = proc.lock();
    for v in &p.vmas.vmas {
        if let VmaKind::MappedFile { inode, offset } = v.kind {
            if v.contains(cr2) {
                let page = (cr2 - v.start) / PAGE_SIZE as u64;
                return Some((inode, offset, page));
            }
        }
    }
    None
}

/// Reads up to `buf.len()` bytes at absolute file offset `off` into `buf`,
/// cursor-free. Returns the number of bytes actually read.
///
/// `buf` is zero-initialised; bytes not filled by the read remain zero, giving
/// partial-last-page and beyond-EOF semantics. For InitrdFile and BlockFile the
/// fd cursor is saved/restored so concurrent regular read() calls are not
/// disturbed.
/// Reads one page of a file into `buf`.
///
/// Returns `None` when the read *failed*, and `Some(n)` when it succeeded
/// with `n` bytes read. The difference matters: a short read at the end of
/// the file is normal (the rest of the page is zeros), but a failed read
/// must not be turned into a page of zeros -- see the callers.
fn read_file_page(
    file: &alloc::sync::Arc<SpinLock<OpenFile>>,
    off: usize,
    buf: &mut [u8; 4096],
) -> Option<usize> {
    let mut f = file.lock();
    match &mut f.vnode {
        VNode::InitrdFile { data, cursor } => {
            if off < 4096 {
                let magic = if data.len() >= 4 { alloc::format!("{:02x}{:02x}{:02x}{:02x}", data[0], data[1], data[2], data[3]) } else { alloc::format!("short") };
                crate::println!("[filemmap] InitrdFile read off={:#x} size={} magic={}", off, data.len(), magic);
            }
            let save = *cursor;
            let start = off.min(data.len());
            let remaining = data.len() - start;
            let to_copy = remaining.min(buf.len());
            if to_copy > 0 {
                buf[..to_copy].copy_from_slice(&data[start..start + to_copy]);
            }
            *cursor = save;
            Some(to_copy)
        }
        VNode::BlockFile { start_sector, size, cursor, .. } => {
            let save = *cursor;
            *cursor = off.min(*size);
            let n = block_read_at(*start_sector, *size, *cursor, buf);
            *cursor = save;
            Some(n)
        }
        VNode::FsNode { node, .. } => match node.read_at(off, buf) {
            Ok(n) => {
                // A read that stops in the *middle* of the page is not a
                // page: the rest of the buffer stays zeroed and would be
                // mapped as if it were file content, and the program would
                // parse the zeros. Only a short read at the end of the file
                // is legitimate (the tail of the last page is zeros).
                let end = off.saturating_add(n);
                if n < buf.len() && end < node.size() {
                    return None;
                }
                Some(n)
            }
            // A failed read is *not* a short read: the caller must not
            // present the zeros as file content.
            Err(_) => None,
        },
        _ => Some(0),
    }
}

/// File-backed demand fault: map the faulted page.
///
/// For cacheable vnodes the DSO cache is consulted first:
///   - **Hit**: the cached frame is mapped into the PTE with FILE_CACHE set and
///     its refcount bumped (`dsocache::retain`); no file read occurs.
///   - **Miss**: the file block is read into a fresh frame, inserted into the
///     cache, and mapped with FILE_CACHE set.
/// Non-cacheable vnodes fall back to a per-process fresh frame.
///
/// MAP_PRIVATE pages are additionally marked COW so a later write makes a
/// private copy. MAP_SHARED pages map the shared cached frame writable.
///
/// File-backed demand fault: map the faulted page.
///
/// For cacheable vnodes the DSO cache is consulted first:
///   - **Hit**: the cached frame is mapped into the PTE with FILE_CACHE set and
///     its refcount bumped (`dsocache::retain`); no file read occurs.
///   - **Miss**: the file block is read into a fresh frame, inserted into the
///     cache, and mapped with FILE_CACHE set.
/// Non-cacheable vnodes fall back to a per-process fresh frame.
///
/// MAP_PRIVATE pages are additionally marked COW so a later write makes a
/// private copy. MAP_SHARED pages map the shared cached frame writable.
///
/// Returns `true` if the fault was handled (PTE installed), `false` otherwise.
/// Gives back one reference to a cached page, and frees the frame if that was
/// the last one (the cache removed the entry).
fn drop_cache_ref(inode: u64, page_offset: u64) {
    if let Some(f) = dsocache::release(inode, page_offset) {
        let mut alloc = crate::memory::FRAME_ALLOCATOR.lock();
        alloc.dealloc_pages(f, 1);
        drop(alloc);
    }
}

pub fn file_demand_paging_fault(cr2: u64) -> bool {
    // Covering VMA: prot (for PTE flags) and MAP_PRIVATE (for COW).
    let (prot, is_private) = match crate::tasks::fault_process().and_then(|proc| {
        let p = proc.lock();
        p.vmas
            .find(cr2)
            .map(|v| (v.prot, v.flags & MAP_PRIVATE != 0))
    }) {
        Some(x) => x,
        None => return false,
    };

    if let Some((inode, vma_off, page)) = current_mapped_file_vma_for(cr2) {
        let page_byte_off = match page.checked_mul(PAGE_SIZE as u64) {
            Some(v) => v,
            None => return false,
        };
        let page_offset = align_down(vma_off.saturating_add(page_byte_off), PAGE_SIZE);
        let flags = prot_to_flags(prot) | PageTableFlags::FILE_CACHE;

        if let Some(frame) = dsocache::lookup_and_retain(inode, page_offset) {
            if !demand_paging_fault_use_frame(cr2, frame, flags, HHDM_BASE.0) {
                drop_cache_ref(inode, page_offset);
                return false;
            }
            if is_private && (prot & crate::memory::vma::PROT_WRITE) != 0 {
                mark_cow(cr2, HHDM_BASE.0);
            }
            return true;
        }
        return false;
    }

    let (file, vma_off, page) = match current_file_vma_for(cr2) {
        Some(x) => x,
        None => return false,
    };

    // Overflow-checked file offset: vma_off + page * PAGE_SIZE.
    let page_byte_off = match page.checked_mul(PAGE_SIZE as u64) {
        Some(v) => v,
        None => return false,
    };
    let file_off = match vma_off.checked_add(page_byte_off) {
        Some(v) => v as usize,
        None => return false,
    };

    let cache_id = vnode_cache_id(&file.lock().vnode);
    // FileNode used for dirty write-back; None for Initrd/BlockFile (static).
    let cache_node: Option<alloc::sync::Arc<dyn crate::fs::vfs::FileNode>> =
        match &file.lock().vnode {
            VNode::FsNode { node, .. } => Some(node.clone()),
            _ => None,
        };
    let page_offset = align_down(file_off as u64, PAGE_SIZE);
    let flags = prot_to_flags(prot) | PageTableFlags::FILE_CACHE;

    if let Some(cache_id) = cache_id {
        let is_cow = is_private && (prot & crate::memory::vma::PROT_WRITE) != 0;

        // Hit: map the cached frame; the DSO cache owns its lifecycle.
        if let Some(frame) = dsocache::lookup_and_retain(cache_id, page_offset) {
            if !demand_paging_fault_use_frame(cr2, frame, flags, HHDM_BASE.0) {
                drop_cache_ref(cache_id, page_offset);
                return false;
            }
            if is_cow {
                mark_cow(cr2, HHDM_BASE.0);
            }
            return true;
        }

        // Miss: read the block, allocate a fresh frame, insert it into the
        // cache, then map it. `retain` returns the canonical frame; if a
        // concurrent fault inserted the same (inode, offset) first, free ours.
        let mut frame = [0u8; 4096];
        if read_file_page(&file, file_off, &mut frame).is_none() {
            // The read failed. Mapping the (zeroed) buffer as if it were the
            // file's content hands the program silent garbage: the dynamic
            // loader, for one, parses the zeros and dereferences a null
            // pointer. Fail the fault instead, so the program sees the error.
            return false;
        }

        let new_frame = {
            let f = {
                let mut alloc = crate::memory::FRAME_ALLOCATOR.lock();
                match alloc.alloc_pages(1, PAGE_SIZE) {
                    Ok(f) => f,
                    Err(_) => {
                        // OOM: evict cold DSO cache entries (refs == 0) and
                        // return their frames to the allocator, then retry.
                        // Drop FRAME_ALLOCATOR first to avoid holding it across
                        // the DSO_CACHE lock (lock ordering).
                        drop(alloc);
                        let evicted = dsocache::evict_lru(256);
                        crate::dprintln!(
                            "[filemmap] OOM evicted {} DSO cache frames",
                            evicted.len()
                        );
                        let mut alloc2 = crate::memory::FRAME_ALLOCATOR.lock();
                        for fr in &evicted {
                            alloc2.dealloc_pages(*fr, 1);
                        }
                        match alloc2.alloc_pages(1, PAGE_SIZE) {
                            Ok(f) => f,
                            Err(_) => {
                                let (total, usable) = alloc2.memory_stats();
                                let (allocs, deallocs) = alloc2.alloc_stats();
                                crate::println!(
                                    "[filemmap] FATAL OOM: total_bytes={} usable_bytes={} allocs={} deallocs={} dso_cache_len={} free_frames={} mmap_regions={}",
                                    total, usable, allocs, deallocs,
                                    dsocache::cache_len(),
                                    alloc2.free_frames(),
                                    alloc2.memory_map().len()
                                );
                                panic!("file demand paging alloc failed after DSO cache eviction");
                            }
                        }
                    }
                }
            };
            let dst = (f + HHDM_BASE.0).as_mut_ptr::<u8>();
            unsafe {
                core::ptr::copy_nonoverlapping(frame.as_ptr(), dst, PAGE_SIZE as usize);
            }
            f
        };

        let canonical = dsocache::retain(cache_id, page_offset, new_frame, cache_node);
        if canonical != new_frame {
            let mut alloc = crate::memory::FRAME_ALLOCATOR.lock();
            alloc.dealloc_pages(new_frame, 1);
            drop(alloc);
        }

        if demand_paging_fault_use_frame(cr2, canonical, flags, HHDM_BASE.0) {
            if is_cow {
                mark_cow(cr2, HHDM_BASE.0);
            }
            return true;
        }

        // PTE install failed; undo the ref we just took.
        drop_cache_ref(cache_id, page_offset);
        return false;
    }

    // Non-cacheable vnode: per-process fresh frame, zero-fill tail beyond EOF.
    let mut frame = [0u8; 4096];
    let read = match read_file_page(&file, file_off, &mut frame) {
        Some(n) => n,
        None => return false,
    };
    let mut alloc = crate::memory::FRAME_ALLOCATOR.lock();
    let ok = demand_paging_fault_with_source(cr2, &mut *alloc, HHDM_BASE.0, Some(&frame[..read]));
    drop(alloc);

    if !ok {
        return false;
    }

    if is_private {
        mark_cow(cr2, HHDM_BASE.0);
    }

    true
}

/// Dispatches a demand fault to the file-backed path when the VMA is file-backed.
/// Returns false when not file-backed so the caller falls through to the
/// anonymous `demand_paging_fault` path.
pub fn dispatch_demand_fault(cr2: u64, _file_backed: bool) -> bool {
    if current_file_vma_for(cr2).is_some() || current_mapped_file_vma_for(cr2).is_some() {
        file_demand_paging_fault(cr2)
    } else {
        false
    }
}

/// Drops the DSO-cache refcount for the FILE_CACHE page at `cr2`. Returns the
/// frame to dealloc when refs reached 0; the caller deallocs it with its
/// already-held FRAME_ALLOCATOR (this lock is non-reentrant).
/// Marks the cached page dirty from the hardware PTE DIRTY bit (a MAP_SHARED
/// write sets it; MAP_PRIVATE pages are COW/read-only and never set it) before
/// releasing, so the page is written back on last unmap.
pub fn release_file_page(cr2: u64) -> Option<PhysAddr> {
    let Some(frame) = crate::memory::space::frame_at(cr2) else { return None };
    let dirty = crate::memory::space::pte_is_dirty(cr2);
    release_file_page_from_frame_dirty(frame, dirty)
}

/// Frame-based variant used at destroy time, when the covering process/VMA is
/// already gone. The cache key is resolved from the FRAME_TO_KEY reverse map.
/// The destroy loop passes the DIRTY bit it reads from the PTE it holds.
pub fn release_file_page_from_frame(frame: u64) -> Option<PhysAddr> {
    release_file_page_from_frame_dirty(frame, false)
}

/// Releases a FILE_CACHE frame, writing the frame contents back to its file if
/// the cached page is dirty (MAP_SHARED was written). `dirty` is the hardware
/// PTE DIRTY bit observed by the caller. Returns the frame to dealloc when refs
/// reached 0; the caller deallocs it.
pub fn release_file_page_from_frame_dirty(frame: u64, dirty: bool) -> Option<PhysAddr> {
    let Some((inode, page_offset)) = crate::memory::dsocache::FRAME_TO_KEY.lock().get(&frame).copied()
    else {
        return None;
    };
    // Write back if dirty (MAP_SHARED was written). MAP_PRIVATE cached pages are
    // COW/read-only, never dirty, so gating on `dirty` is sufficient.
    if dirty {
        let mut cache = crate::memory::dsocache::DSO_CACHE.lock();
        if let Some(entry) = cache.get_mut(&(inode, page_offset)) {
            entry.dirty = true;
            if let Some(node) = &entry.node {
                let frame_va = (entry.frame.0 + HHDM_BASE.0) as *const u8;
                let buf = unsafe { core::slice::from_raw_parts(frame_va, PAGE_SIZE as usize) };
                let _ = node.write_at(page_offset as usize, buf);
            }
        }
    }
    crate::memory::dsocache::release(inode, page_offset)
}

/// Reads up to `buf.len()` bytes from a BlockFile at absolute file offset `off`.
/// Returns the number of bytes actually read (may be less at EOF).
fn block_read_at(start_sector: u64, size: usize, off: usize, buf: &mut [u8]) -> usize {
    if off >= size {
        return 0;
    }
    let to_read = core::cmp::min(size - off, buf.len());
    let mut blk_guard = crate::drivers::virtio::blk::BLK_DEV.lock();
    let Some(blk) = blk_guard.as_mut() else {
        return 0;
    };
    let mut bytes_read = 0;
    let mut bounce = [0u8; 512];
    while bytes_read < to_read {
        let current_offset = off + bytes_read;
        let sector_offset = (current_offset / 512) as u64;
        let offset_in_sector = current_offset % 512;
        let chunk = core::cmp::min(512 - offset_in_sector, to_read - bytes_read);
        if blk.read_sector(start_sector + sector_offset, &mut bounce).is_err() {
            break;
        }
        buf[bytes_read..bytes_read + chunk]
            .copy_from_slice(&bounce[offset_in_sector..offset_in_sector + chunk]);
        bytes_read += chunk;
    }
    bytes_read
}
