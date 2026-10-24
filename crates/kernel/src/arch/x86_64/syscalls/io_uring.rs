//! io_uring implementation (Linux ABI).
//!
//! The user-visible contract is liburing's:
//!
//! ```c
//! int fd = io_uring_setup(entries, &p);
//! sq_ring = mmap(NULL, sz, PROT_READ|PROT_WRITE, MAP_SHARED, fd, IORING_OFF_SQ_RING);
//! cq_ring = mmap(NULL, sz, PROT_READ|PROT_WRITE, MAP_SHARED, fd, IORING_OFF_CQ_RING);
//! sqes   = mmap(NULL, sz, PROT_READ|PROT_WRITE, MAP_SHARED, fd, IORING_OFF_SQES);
//! ```
//!
//! so the ring fd must support `mmap` at three distinct file offsets, and
//! `params.sq_off` / `params.cq_off` must be byte offsets *within the mapped
//! region*, not offsets into some kernel-private header.
//!
//! The layout follows Linux exactly (include/uapi/linux/io_uring.h):
//!
//! ```text
//! SQ ring region (IORING_OFF_SQ_RING), one page:
//!   0x00 u32 head            (kernel writes, user reads)
//!   0x04 u32 tail            (user writes, kernel reads)
//!   0x08 u32 ring_mask
//!   0x0c u32 ring_entries
//!   0x10 u32 flags
//!   0x14 u32 dropped
//!   0x18 u32 array[]         (sq_entries indices into the SQE array)
//!
//! CQ ring region (IORING_OFF_CQ_RING), one page:
//!   0x00 u32 head
//!   0x04 u32 tail
//!   0x08 u32 ring_mask
//!   0x0c u32 ring_entries
//!   0x10 u32 overflow
//!   0x14 u32 flags
//!   0x18 u32 cqes[]          (cq_entries completion entries)
//!
//! SQE array (IORING_OFF_SQES), sq_entries * 64 bytes
//! ```
//!
//! `IORING_FEAT_SINGLE_MMAP` is advertised: Linux then makes the CQ ring
//! live at the *same* file offset as the SQ ring, and liburing maps it with
//! the larger of the two sizes. We keep them as separate mappings (simpler,
//! and liburing falls back to two mmap calls when the feature is absent), so
//! the feature is NOT advertised.

use super::*;
use alloc::sync::Arc;
use alloc::vec;
use crate::fs::vfs::{OpenFile, VNode};
use crate::memory::space::{mapper_for, validate_user_read_range, validate_user_write_range};
use crate::memory::vma::MAP_PRIVATE;
use shared::memory::allocator::FrameAllocator;

// io_uring opcode constants (Linux values)
pub const IORING_OP_NOP: u8 = 0;
pub const IORING_OP_READV: u8 = 1;
pub const IORING_OP_WRITEV: u8 = 2;
pub const IORING_OP_FSYNC: u8 = 3;
pub const IORING_OP_READ_FIXED: u8 = 5;
pub const IORING_OP_WRITE_FIXED: u8 = 6;
pub const IORING_OP_POLL_ADD: u8 = 7;
pub const IORING_OP_POLL_REMOVE: u8 = 8;
pub const IORING_OP_SYNC_FILE_RANGE: u8 = 9;
pub const IORING_OP_SENDMSG: u8 = 10;
pub const IORING_OP_RECVMSG: u8 = 11;
pub const IORING_OP_TIMEOUT: u8 = 12;
pub const IORING_OP_TIMEOUT_REMOVE: u8 = 13;
pub const IORING_OP_ACCEPT: u8 = 14;
pub const IORING_OP_ASYNC_CANCEL: u8 = 15;
pub const IORING_OP_LINK_TIMEOUT: u8 = 16;
pub const IORING_OP_CONNECT: u8 = 17;
pub const IORING_OP_FALLOCATE: u8 = 18;
pub const IORING_OP_OPENAT: u8 = 19;
pub const IORING_OP_CLOSE: u8 = 21;
pub const IORING_OP_STATX: u8 = 24;

// io_uring_flags
pub const IOSQE_FIXED_FILE: u8 = 1 << 0;
pub const IOSQE_IO_DRAIN: u8 = 1 << 1;
pub const IOSQE_IO_LINK: u8 = 1 << 2;
pub const IOSQE_IO_HARDLINK: u8 = 1 << 3;
pub const IOSQE_ASYNC: u8 = 1 << 4;
pub const IOSQE_BUFFER_SELECT: u8 = 1 << 5;

// io_uring_setup flags
pub const IORING_SETUP_IOPOLL: u32 = 1 << 0;
pub const IORING_SETUP_SQPOLL: u32 = 1 << 1;
pub const IORING_SETUP_SQ_AFF: u32 = 1 << 2;
pub const IORING_SETUP_CQSIZE: u32 = 1 << 3;
pub const IORING_SETUP_CLAMP: u32 = 1 << 4;
pub const IORING_SETUP_ATTACH_WQ: u32 = 1 << 5;
pub const IORING_SETUP_RW_POS: u32 = 1 << 6;

// io_uring features
pub const IORING_FEAT_SINGLE_MMAP: u32 = 1 << 0;
pub const IORING_FEAT_NODROP: u32 = 1 << 1;
pub const IORING_FEAT_SUBMIT_STABLE: u32 = 1 << 2;
pub const IORING_FEAT_RW_CUR_POS: u32 = 1 << 3;
pub const IORING_FEAT_CUR_PERSONALITY: u32 = 1 << 4;
pub const IORING_FEAT_FAST_POLL: u32 = 1 << 5;
pub const IORING_FEAT_POLL_32BITS: u32 = 1 << 6;
pub const IORING_FEAT_SQPOLL_NONFIXED: u32 = 1 << 7;
pub const IORING_FEAT_EXT_ARG: u32 = 1 << 8;
pub const IORING_FEAT_NATIVE_WORKERS: u32 = 1 << 9;
pub const IORING_FEAT_RSRC_TAGS: u32 = 1 << 10;

// io_uring_register opcodes
pub const IORING_REGISTER_BUFFERS: u32 = 0;
pub const IORING_REGISTER_FILES: u32 = 1;
pub const IORING_REGISTER_FILES_UPDATE: u32 = 2;
pub const IORING_REGISTER_EVENTFD: u32 = 3;
pub const IORING_REGISTER_EVENTFD_ASYNC: u32 = 4;
pub const IORING_REGISTER_PROBE: u32 = 5;
pub const IORING_REGISTER_PERSONALITY: u32 = 6;
pub const IORING_REGISTER_ENABLE_RINGS: u32 = 7;
pub const IORING_REGISTER_REG2: u32 = 8;
pub const IORING_REGISTER_FILES_UPDATE2: u32 = 9;
pub const IORING_REGISTER_BUFFERS2: u32 = 10;
pub const IORING_REGISTER_FILES_UPDATE3: u32 = 11;
pub const IORING_REGISTER_EVENTFD2: u32 = 12;
pub const IORING_REGISTER_BUFFERS_UPDATE: u32 = 13;
pub const IORING_REGISTER_PERSONALITY2: u32 = 14;

// io_uring_enter flags
pub const IORING_ENTER_GETEVENTS: u32 = 1 << 0;
pub const IORING_ENTER_SQ_WAIT: u32 = 1 << 1;
pub const IORING_ENTER_SQ_FIXED_FILE: u32 = 1 << 2;
pub const IORING_ENTER_EXT_ARG: u32 = 1 << 3;

// mmap file offsets (Linux)
pub const IORING_OFF_SQ_RING: u64 = 0;
pub const IORING_OFF_CQ_RING: u64 = 0x8000000;
pub const IORING_OFF_SQES: u64 = 0x10000000;

/// Mask for the "which region" part of an mmap offset.
const IORING_OFF_MMAP_MASK: u64 = 0xf0000000;

// io_uring_sqe (64 bytes, matches Linux ABI)
//
// The tail fields matter: the SQE stride is 64 bytes, so a struct that is
// short by even one field shifts every later SQE and makes the client read
// garbage user_data. Layout (from include/uapi/linux/io_uring.h):
//
//   0  opcode      u8      32  user_data   u64
//   1  flags       u8      40  buf_index   u16
//   2  ioprio      u16     42  personality u16
//   4  fd          i32     44  file_index  u32
//   8  off         u64     48  addr3       u64
//  16  addr        u64     56  __pad2      u64
//  24  len         u32
//  28  rw_flags    u32
#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
pub struct IoUringSqe {
    pub opcode: u8,
    pub flags: u8,
    pub ioprio: u16,
    pub fd: i32,
    pub off: u64,
    pub addr: u64,
    pub len: u32,
    pub rw_flags: u32,
    pub user_data: u64,
    pub buf_index: u16,
    pub personality: u16,
    pub file_index: u32,
    pub addr3: u64,
    pub __pad2: u64,
}

// io_uring_cqe (16 bytes, matches Linux ABI)
//
// 16 bytes unless IORING_SETUP_CQE32 doubles it. Linux's default stride is
// 16, and the client computes the CQE address as
// `cq_ring + cq_off.cqes + index * sizeof(cqe)`, so the kernel must use the
// same stride when it writes completions.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
pub struct IoUringCqe {
    pub user_data: u64,
    pub res: i32,
    pub flags: u32,
}

// io_sqring_offsets
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct IoSqringOffsets {
    pub head: u32,
    pub tail: u32,
    pub ring_mask: u32,
    pub ring_entries: u32,
    pub flags: u32,
    pub dropped: u32,
    pub array: u32,
    pub resv1: u32,
    pub user_addr: u64,
}

// io_cqring_offsets
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct IoCqringOffsets {
    pub head: u32,
    pub tail: u32,
    pub ring_mask: u32,
    pub ring_entries: u32,
    pub overflow: u32,
    pub cqes: u32,
    pub flags: u32,
    pub resv1: u32,
    pub user_addr: u64,
}

// io_uring_params
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct IoUringParams {
    pub sq_entries: u32,
    pub cq_entries: u32,
    pub flags: u32,
    pub sq_thread_cpu: u32,
    pub sq_thread_idle: u32,
    pub features: u32,
    pub wq_fd: u32,
    pub resv: [u32; 3],
    pub sq_off: IoSqringOffsets,
    pub cq_off: IoCqringOffsets,
}

const SQE_SIZE: usize = core::mem::size_of::<IoUringSqe>();   // 64
const CQE_SIZE: usize = core::mem::size_of::<IoUringCqe>();   // 32

/// Byte offsets inside the SQ ring page.
const SQ_HEAD: u64 = 0x00;
const SQ_TAIL: u64 = 0x04;
const SQ_RING_MASK: u64 = 0x08;
const SQ_RING_ENTRIES: u64 = 0x0c;
const SQ_FLAGS: u64 = 0x10;
const SQ_DROPPED: u64 = 0x14;
const SQ_ARRAY: u64 = 0x18;

/// Byte offsets inside the CQ ring page.
const CQ_HEAD: u64 = 0x00;
const CQ_TAIL: u64 = 0x04;
const CQ_RING_MASK: u64 = 0x08;
const CQ_RING_ENTRIES: u64 = 0x0c;
const CQ_OVERFLOW: u64 = 0x10;
const CQ_FLAGS: u64 = 0x14;
const CQ_CQES: u64 = 0x18;

/// One io_uring ring context.
#[derive(Clone)]
struct RingContext {
    sq_entries: u32,
    cq_entries: u32,
    /// Physical base of the SQ ring page.
    sq_ring_phys: u64,
    /// Physical base of the CQ ring page.
    cq_ring_phys: u64,
    /// Physical base of the SQE array.
    sqes_phys: u64,
    /// Userspace virtual address of the SQ ring page (0 until mmap'd).
    sq_ring_uaddr: u64,
    /// Userspace virtual address of the CQ ring page (0 until mmap'd).
    cq_ring_uaddr: u64,
    /// Userspace virtual address of the SQE array (0 until mmap'd).
    sqes_uaddr: u64,
    /// Registered fixed files (IOSQE_FIXED_FILE). Index -> fd.
    registered_files: alloc::vec::Vec<i32>,
    /// Kernel's read position in the SQ ring.
    sq_head: u32,
    /// Kernel's write position in the CQ ring.
    cq_tail: u32,
    /// Backing frames, kept so they are not freed while the ring lives.
    frames: Vec<u64>,
}

/// Global registry of ring contexts, indexed by a small integer.
static RING_CTX: SpinLock<alloc::vec::Vec<Option<RingContext>>> =
    SpinLock::new(alloc::vec::Vec::new());

fn alloc_ring_slot() -> u32 {
    let mut rings = RING_CTX.lock();
    for (i, slot) in rings.iter().enumerate() {
        if slot.is_none() {
            return i as u32;
        }
    }
    rings.push(None);
    (rings.len() - 1) as u32
}

fn ring_get(slot: u32) -> Option<RingContext> {
    let rings = RING_CTX.lock();
    rings.get(slot as usize).and_then(|s| s.as_ref().cloned())
}

fn ring_put(slot: u32, ctx: RingContext) {
    let mut rings = RING_CTX.lock();
    if (slot as usize) < rings.len() {
        rings[slot as usize] = Some(ctx);
    } else {
        rings.resize_with(slot as usize + 1, || None);
        rings[slot as usize] = Some(ctx);
    }
}

/// Build the io_uring_params structure for userspace.
///
/// The offsets are byte offsets *within* the mapped region, matching Linux:
/// the SQ ring page starts at 0 and the CQ ring page starts at 0.
fn build_params(sq_entries: u32, cq_entries: u32) -> IoUringParams {
    IoUringParams {
        sq_entries,
        cq_entries,
        flags: 0,
        sq_thread_cpu: 0,
        sq_thread_idle: 0,
        // SINGLE_MMAP is NOT advertised: the CQ ring is a separate mapping
        // here, and liburing only uses the shared mapping when the feature is
        // present.
        features: IORING_FEAT_NODROP | IORING_FEAT_SUBMIT_STABLE,
        wq_fd: 0,
        resv: [0, 0, 0],
        sq_off: IoSqringOffsets {
            head: SQ_HEAD as u32,
            tail: SQ_TAIL as u32,
            ring_mask: SQ_RING_MASK as u32,
            ring_entries: SQ_RING_ENTRIES as u32,
            flags: SQ_FLAGS as u32,
            dropped: SQ_DROPPED as u32,
            array: SQ_ARRAY as u32,
            resv1: 0,
            user_addr: 0,
        },
        cq_off: IoCqringOffsets {
            head: CQ_HEAD as u32,
            tail: CQ_TAIL as u32,
            ring_mask: CQ_RING_MASK as u32,
            ring_entries: CQ_RING_ENTRIES as u32,
            overflow: CQ_OVERFLOW as u32,
            cqes: CQ_CQES as u32,
            flags: CQ_FLAGS as u32,
            resv1: 0,
            user_addr: 0,
        },
    }
}

/// Allocate `npages` physical pages. Returns the physical addresses.
fn alloc_pages(npages: usize) -> Result<Vec<u64>, u64> {
    let mut phys_list = Vec::with_capacity(npages);
    for _ in 0..npages {
        match FRAME_ALLOCATOR.lock().alloc_pages(1, PAGE_SIZE as u64) {
            Ok(phys) => phys_list.push(phys.0),
            Err(_) => return Err(ENOMEM),
        }
    }
    Ok(phys_list)
}

/// Map already-allocated physical pages into the current process at a fresh
/// userspace address, and record a VMA so munmap can tear it down.
fn map_pages_at_new_vaddr(cr3: u64, phys_list: &[u64]) -> Result<u64, u64> {
    let npages = phys_list.len();
    let size = (npages * PAGE_SIZE as usize) as u64;
    let aligned = align_up(size, PAGE_SIZE);

    let uaddr = with_current_process(|p| {
        let base = p.mmap_base.0;
        p.mmap_base = VirtAddr(base + aligned);

        let mut va = base;
        for &phys in phys_list {
            let vma = Vma {
                start: va,
                end: va + PAGE_SIZE,
                prot: 3,
                flags: MAP_PRIVATE | MAP_ANONYMOUS,
                kind: VmaKind::Anonymous,
            };
            if p.vmas.insert(vma).is_err() {
                return Err(ENOMEM);
            }
            va += PAGE_SIZE;
        }
        Ok(base)
    })?;

    let mut mapper = mapper_for(PhysAddr(cr3));
    let mut alloc = FRAME_ALLOCATOR.lock();
    let mut va = uaddr;
    for &phys in phys_list {
        if mapper
            .map_range(
                &mut *alloc,
                VirtAddr(va),
                PhysAddr(phys),
                PAGE_SIZE as usize,
                PageTableFlags::USER | PageTableFlags::WRITABLE | PageTableFlags::PRESENT,
            )
            .is_err()
        {
            return Err(ENOMEM);
        }
        va += PAGE_SIZE;
    }
    drop(alloc);

    Ok(uaddr)
}

/// Map already-allocated physical pages at a *specific* userspace address
/// (used by mmap on the ring fd, where the user picks the address).
fn map_pages_at_vaddr(cr3: u64, uaddr: u64, phys_list: &[u64]) -> Result<(), u64> {
    let npages = phys_list.len();
    let mut mapper = mapper_for(PhysAddr(cr3));
    let mut alloc = FRAME_ALLOCATOR.lock();
    let mut va = uaddr;
    for &phys in phys_list {
        if mapper
            .map_range(
                &mut *alloc,
                VirtAddr(va),
                PhysAddr(phys),
                PAGE_SIZE as usize,
                PageTableFlags::USER | PageTableFlags::WRITABLE | PageTableFlags::PRESENT,
            )
            .is_err()
        {
            return Err(ENOMEM);
        }
        va += PAGE_SIZE;
    }
    drop(alloc);

    // Record the VMA.
    with_current_process(|p| {
        let mut va = uaddr;
        for _ in 0..npages {
            let vma = Vma {
                start: va,
                end: va + PAGE_SIZE,
                prot: 3,
                flags: MAP_PRIVATE | MAP_ANONYMOUS,
                kind: VmaKind::Anonymous,
            };
            if p.vmas.insert(vma).is_err() {
                return Err(ENOMEM);
            }
            va += PAGE_SIZE;
        }
        Ok(())
    })
}

/// Zero a physical page range (the ring pages start zeroed).
fn zero_phys(phys_list: &[u64]) {
    for &phys in phys_list {
        unsafe {
            core::ptr::write_bytes((phys + HHDM_BASE.0) as *mut u8, 0, PAGE_SIZE as usize);
        }
    }
}

/// sys_io_uring_setup(entries, params) -> fd
pub(super) fn sys_io_uring_setup(entries: u32, params_ptr: u64) -> u64 {
    if entries == 0 || (entries & (entries - 1)) != 0 {
        return EINVAL;
    }
    if entries > 4096 {
        return EINVAL;
    }

    if params_ptr == 0
        || !validate_user_read_range(params_ptr, core::mem::size_of::<IoUringParams>())
    {
        return EFAULT;
    }

    let params: IoUringParams = unsafe { core::ptr::read_unaligned(params_ptr as *const _) };

    let mut sq_entries = entries;
    let mut cq_entries = params.cq_entries;
    if cq_entries == 0 {
        cq_entries = sq_entries * 2;
    }
    if (params.flags & IORING_SETUP_CLAMP) == 0 {
        if sq_entries > 4096 {
            sq_entries = 4096;
        }
        if cq_entries > 8192 {
            cq_entries = 8192;
        }
    }
    if (sq_entries & (sq_entries - 1)) != 0 || (cq_entries & (cq_entries - 1)) != 0 {
        return EINVAL;
    }

    let cr3 = with_current_process(|p| p.cr3.0);

    // One page for the SQ ring, one for the CQ ring, and the SQE array.
    let sq_pages = 1usize;
    let cq_pages = 1usize;
    let sqe_pages = (align_up(
        (sq_entries as u64) * (SQE_SIZE as u64),
        PAGE_SIZE,
    ) / PAGE_SIZE) as usize;

    let sq_frames = match alloc_pages(sq_pages) {
        Ok(f) => f,
        Err(e) => return e,
    };
    let cq_frames = match alloc_pages(cq_pages) {
        Ok(f) => f,
        Err(e) => return e,
    };
    let sqe_frames = match alloc_pages(sqe_pages) {
        Ok(f) => f,
        Err(e) => return e,
    };

    zero_phys(&sq_frames);
    zero_phys(&cq_frames);
    zero_phys(&sqe_frames);

    // The SQ ring's index array lives in the SQ page right after the header.
    // Initialize it to identity, as Linux does, and publish the ring size.
    unsafe {
        let sq_base = (sq_frames[0] + HHDM_BASE.0) as *mut u32;
        core::ptr::write_unaligned(sq_base.add((SQ_RING_MASK / 4) as usize), sq_entries - 1);
        core::ptr::write_unaligned(
            sq_base.add((SQ_RING_ENTRIES / 4) as usize),
            sq_entries,
        );
        for i in 0..sq_entries as usize {
            core::ptr::write_unaligned(
                sq_base.add((SQ_ARRAY / 4) as usize + i),
                i as u32,
            );
        }
        let cq_base = (cq_frames[0] + HHDM_BASE.0) as *mut u32;
        core::ptr::write_unaligned(cq_base.add((CQ_RING_MASK / 4) as usize), cq_entries - 1);
        core::ptr::write_unaligned(
            cq_base.add((CQ_RING_ENTRIES / 4) as usize),
            cq_entries,
        );
    }

    let slot = alloc_ring_slot();

    let ctx = RingContext {
        sq_entries,
        cq_entries,
        sq_ring_phys: sq_frames[0],
        cq_ring_phys: cq_frames[0],
        sqes_phys: sqe_frames[0],
        sq_ring_uaddr: 0,
        cq_ring_uaddr: 0,
        sqes_uaddr: 0,
        registered_files: alloc::vec::Vec::new(),
        sq_head: 0,
        cq_tail: 0,
        frames: sq_frames
            .iter()
            .chain(cq_frames.iter())
            .chain(sqe_frames.iter())
            .copied()
            .collect(),
    };

    let ret_params = build_params(sq_entries, cq_entries);

    if !validate_user_write_range(params_ptr, core::mem::size_of::<IoUringParams>()) {
        return EFAULT;
    }
    unsafe {
        core::ptr::write_unaligned(params_ptr as *mut IoUringParams, ret_params);
    }

    ring_put(slot, ctx);

    // Allocate an fd in the current process pointing to this ring slot.
    let t = match crate::tasks::current_thread() {
        Some(t) => t,
        None => return EBADF,
    };
    let mut p = t.process.lock();
    let fd = p
        .fd_table
        .iter()
        .position(|e| e.is_none())
        .unwrap_or(p.fd_table.len());
    if fd >= p.fd_table.len() {
        p.fd_table.resize(fd + 1, None);
    }
    p.fd_table[fd] = Some(Arc::new(SpinLock::new(OpenFile {
        vnode: VNode::IoUring { slot },
        flags: IORING_FD_FLAG,
        path: alloc::string::String::from("io_uring"),
    })));
    drop(p);

    crate::tasks::set_current_thread(Some(t));

    fd as u64
}

/// Marker bit in `OpenFile::flags` marking a file descriptor as an io_uring
/// ring. The ring's VNode is a `DmaBuf` (which other DRM paths also use), so
/// the fd needs a distinct tag for `mmap` to route it here.
const IORING_FD_FLAG: u64 = 1 << 30;

/// True if `fd` is an io_uring ring descriptor.
pub(super) fn is_ring_fd(fd: usize) -> bool {
    let t = match crate::tasks::current_thread() {
        Some(t) => t,
        None => return false,
    };
    let p = t.process.lock();
    match p.fd_table.get(fd).cloned().flatten() {
        Some(of) => of.lock().flags & IORING_FD_FLAG != 0,
        None => false,
    }
}

/// Look up the RingContext for an io_uring fd.
fn ring_from_fd(fd: usize) -> Option<RingContext> {
    let t = crate::tasks::current_thread()?;
    let p = t.process.lock();
    let open = p.fd_table.get(fd).cloned().flatten()?;
    drop(p);
    let vnode = open.lock().clone_vnode();
    if let VNode::IoUring { slot } = vnode {
        ring_get(slot)
    } else {
        None
    }
}

/// mmap on an io_uring ring fd.
///
/// `offset` selects the region:
///   * `IORING_OFF_SQ_RING` (0)          -> the SQ ring page
///   * `IORING_OFF_CQ_RING` (0x8000000)  -> the CQ ring page
///   * `IORING_OFF_SQES`    (0x10000000) -> the SQE array
///
/// The offset is matched EXACTLY, as Linux does. Masking it instead is
/// wrong: `IORING_OFF_CQ_RING & 0xf0000000 == 0`, so a masked comparison
/// hands the CQ-ring mmap the SQ-ring page -- the client then reads the
/// submission array as if it were completions.
///
/// Returns the userspace address, or an encoded errno.
pub(super) fn io_uring_mmap(fd: usize, addr: u64, len: usize, prot: u64, flags: u64, offset: u64) -> u64 {
    use crate::memory::vma::MAP_FIXED;

    if len == 0 {
        return EINVAL;
    }
    let region = match offset {
        IORING_OFF_SQ_RING => 0u8,
        IORING_OFF_CQ_RING => 1u8,
        IORING_OFF_SQES => 2u8,
        // The pbuf-ring offset (0x20000000 | bgid << 24) is not implemented.
        _ => return EINVAL,
    };

    let mut ctx = match ring_from_fd(fd) {
        Some(c) => c,
        None => return EBADF,
    };

    let cr3 = with_current_process(|p| p.cr3.0);
    let aligned_len = align_up(len as u64, PAGE_SIZE) as usize;

    // Pick the physical pages backing this region.
    let (phys_list, region_size) = match region {
        0 => {
            let mut v = alloc::vec::Vec::new();
            v.push(ctx.sq_ring_phys);
            (v, ctx.sq_entries as usize * 4 + 0x18)
        }
        1 => {
            let mut v = alloc::vec::Vec::new();
            v.push(ctx.cq_ring_phys);
            (v, ctx.cq_entries as usize * CQE_SIZE + CQ_CQES as usize)
        }
        _ => {
            // The SQE array: ceil(sq_entries * 64 / PAGE) pages starting at
            // sqes_phys. The pages are contiguous because they were allocated
            // in one call.
            let npages = (align_up(
                (ctx.sq_entries as u64) * (SQE_SIZE as u64),
                PAGE_SIZE,
            ) / PAGE_SIZE) as usize;
            let mut v = alloc::vec::Vec::with_capacity(npages);
            for i in 0..npages {
                v.push(ctx.sqes_phys + (i as u64) * PAGE_SIZE);
            }
            (v, (ctx.sq_entries as usize) * SQE_SIZE)
        }
    };

    if aligned_len < region_size {
        return EINVAL;
    }
    if phys_list.len() * (PAGE_SIZE as usize) < aligned_len {
        return EINVAL;
    }

    // Choose the userspace address.
    let uaddr = if (flags & MAP_FIXED) != 0 {
        if addr % PAGE_SIZE != 0 {
            return EINVAL;
        }
        addr
    } else {
        let base = with_current_process(|p| {
            let b = p.mmap_base.0;
            p.mmap_base = VirtAddr(b + aligned_len as u64);
            b
        });
        base
    };

    // Map the pages and record the VMA.
    {
        let mut mapper = mapper_for(PhysAddr(cr3));
        let mut alloc = FRAME_ALLOCATOR.lock();
        let mut va = uaddr;
        for &phys in &phys_list {
            if mapper
                .map_range(
                    &mut *alloc,
                    VirtAddr(va),
                    PhysAddr(phys),
                    PAGE_SIZE as usize,
                    PageTableFlags::USER | PageTableFlags::WRITABLE | PageTableFlags::PRESENT,
                )
                .is_err()
            {
                return ENOMEM;
            }
            va += PAGE_SIZE;
        }
    }

    // Record the VMA.
    if with_current_process(|p| {
        let mut va = uaddr;
        for _ in 0..phys_list.len() {
            let vma = Vma {
                start: va,
                end: va + PAGE_SIZE,
                prot: 3,
                flags: MAP_PRIVATE | MAP_ANONYMOUS,
                kind: VmaKind::Anonymous,
            };
            if p.vmas.insert(vma).is_err() {
                return Err(ENOMEM);
            }
            va += PAGE_SIZE;
        }
        Ok(())
    })
    .is_err()
    {
        return ENOMEM;
    }

    // Record the mapping in the ring context.
    match region {
        0 => ctx.sq_ring_uaddr = uaddr,
        1 => ctx.cq_ring_uaddr = uaddr,
        _ => ctx.sqes_uaddr = uaddr,
    }
    let slot = match fd_slot_from_fd(fd) {
        Some(s) => s,
        None => return uaddr,
    };
    ring_put(slot, ctx);

    uaddr
}

/// sys_io_uring_register(fd, opcode, arg, nr_args) -> count
pub(super) fn sys_io_uring_register(fd: u64, opcode: u32, arg: u64, nr_args: u32) -> u64 {
    let mut ctx = match ring_from_fd(fd as usize) {
        Some(c) => c,
        None => return EBADF,
    };

    match opcode {
        IORING_REGISTER_FILES => {
            if nr_args == 0 || arg == 0 {
                return EINVAL;
            }
            let nr = nr_args as usize;
            if !validate_user_read_range(arg, nr * 4) {
                return EFAULT;
            }
            let mut fds = alloc::vec::Vec::with_capacity(nr);
            for i in 0..nr {
                let f = unsafe { core::ptr::read_unaligned((arg + (i * 4) as u64) as *const i32) };
                fds.push(f);
            }
            ctx.registered_files = fds;

            let slot = match fd_slot_from_fd(fd as usize) {
                Some(s) => s,
                None => return EBADF,
            };
            ring_put(slot, ctx);
            nr_args as u64
        }
        IORING_REGISTER_EVENTFD => {
            let _ = arg;
            0
        }
        IORING_REGISTER_BUFFERS => {
            let _ = arg;
            let _ = nr_args;
            0
        }
        _ => EINVAL,
    }
}

/// sys_io_uring_enter(fd, to_submit, min_complete, flags, sig) -> submitted
pub(super) fn sys_io_uring_enter(
    fd: u64,
    to_submit: u32,
    _min_complete: u32,
    flags: u32,
    _sig: u64,
) -> u64 {
    let mut ctx = match ring_from_fd(fd as usize) {
        Some(c) => c,
        None => return EBADF,
    };

    if ctx.sq_ring_uaddr == 0 || ctx.cq_ring_uaddr == 0 || ctx.sqes_uaddr == 0 {
        // The rings have not been mapped yet; there is nothing to submit.
        return 0;
    }

    let sq_mask = ctx.sq_entries - 1;
    let cq_mask = ctx.cq_entries - 1;

    // Read the SQ tail the user wrote.
    let sq_tail = unsafe {
        core::ptr::read_volatile((ctx.sq_ring_uaddr + SQ_TAIL) as *const u32)
    };

    let available = (sq_tail.wrapping_sub(ctx.sq_head)) & sq_mask;
    let to_submit = to_submit.min(available);
    if to_submit == 0 {
        return 0;
    }

    // Read the SQEs. The SQ ring's `array[]` maps each ring slot to an SQE
    // index, so the kernel must follow it -- reading SQEs sequentially is
    // only correct when the client happened to fill them in order.
    let sqe_array_size = (to_submit as usize) * SQE_SIZE;
    if !validate_user_read_range(ctx.sqes_uaddr, sqe_array_size) {
        return EFAULT;
    }
    if !validate_user_read_range(ctx.sq_ring_uaddr + SQ_ARRAY, 4) {
        return EFAULT;
    }
    let mut sqes: alloc::vec::Vec<IoUringSqe> = alloc::vec::Vec::with_capacity(to_submit as usize);
    for i in 0..to_submit {
        let slot = (ctx.sq_head.wrapping_add(i)) & sq_mask;
        let sqe_index = unsafe {
            core::ptr::read_volatile((ctx.sq_ring_uaddr + SQ_ARRAY + slot as u64 * 4) as *const u32)
        } & sq_mask;
        let sqe = unsafe {
            core::ptr::read_unaligned(
                (ctx.sqes_uaddr + sqe_index as u64 * SQE_SIZE as u64) as *const IoUringSqe,
            )
        };
        sqes.push(sqe);
    }

    // Process each SQE.
    let mut cqes: alloc::vec::Vec<IoUringCqe> = alloc::vec::Vec::with_capacity(to_submit as usize);
    let t = match crate::tasks::current_thread() {
        Some(t) => t,
        None => return EBADF,
    };
    let p = t.process.lock();

    for sqe in &sqes {
        let res: i64 = match sqe.opcode {
            IORING_OP_NOP => 0,
            IORING_OP_READV => {
                let target_fd = if sqe.flags & IOSQE_FIXED_FILE != 0 {
                    ctx.registered_files
                        .get(sqe.fd as usize)
                        .copied()
                        .unwrap_or(-1)
                } else {
                    sqe.fd
                };
                match do_read(&p, target_fd, sqe.addr, sqe.len) {
                    Ok(n) => n as i64,
                    Err(e) => e as i64,
                }
            }
            IORING_OP_WRITEV => {
                let target_fd = if sqe.flags & IOSQE_FIXED_FILE != 0 {
                    ctx.registered_files
                        .get(sqe.fd as usize)
                        .copied()
                        .unwrap_or(-1)
                } else {
                    sqe.fd
                };
                match do_write(&p, target_fd, sqe.addr, sqe.len) {
                    Ok(n) => n as i64,
                    Err(e) => e as i64,
                }
            }
            IORING_OP_FSYNC => 0,
            IORING_OP_FALLOCATE => 0,
            IORING_OP_READ => {
                // IORING_OP_READ carries an explicit offset; it is not a
                // positional read.
                let target_fd = if sqe.flags & IOSQE_FIXED_FILE != 0 {
                    ctx.registered_files
                        .get(sqe.fd as usize)
                        .copied()
                        .unwrap_or(-1)
                } else {
                    sqe.fd
                };
                match do_read_at(&p, target_fd, sqe.addr, sqe.len, sqe.off) {
                    Ok(n) => n as i64,
                    Err(e) => e as i64,
                }
            }
            IORING_OP_WRITE => {
                let target_fd = if sqe.flags & IOSQE_FIXED_FILE != 0 {
                    ctx.registered_files
                        .get(sqe.fd as usize)
                        .copied()
                        .unwrap_or(-1)
                } else {
                    sqe.fd
                };
                match do_write_at(&p, target_fd, sqe.addr, sqe.len, sqe.off) {
                    Ok(n) => n as i64,
                    Err(e) => e as i64,
                }
            }
            _ => -(EINVAL as i64),
        };
        cqes.push(IoUringCqe {
            user_data: sqe.user_data,
            res: res as i32,
            flags: 0,
        });
    }

    drop(p);

    // Write the CQEs into the CQ ring at the kernel's tail position.
    let cqe_array_size = cqes.len() * CQE_SIZE;
    if !validate_user_write_range(ctx.cq_ring_uaddr + CQ_CQES, cqe_array_size) {
        return EFAULT;
    }
    for (i, cqe) in cqes.iter().enumerate() {
        unsafe {
            core::ptr::write_unaligned(
                (ctx.cq_ring_uaddr + CQ_CQES + i as u64 * CQE_SIZE as u64) as *mut IoUringCqe,
                *cqe,
            );
        }
    }

    // Publish the new CQ tail, then advance the SQ head.
    let new_cq_tail = ctx.cq_tail.wrapping_add(cqes.len() as u32) & cq_mask;
    unsafe {
        core::ptr::write_volatile((ctx.cq_ring_uaddr + CQ_TAIL) as *mut u32, new_cq_tail);
        core::ptr::write_volatile(
            (ctx.sq_ring_uaddr + SQ_HEAD) as *mut u32,
            ctx.sq_head.wrapping_add(to_submit) & sq_mask,
        );
    }
    ctx.cq_tail = new_cq_tail;
    ctx.sq_head = ctx.sq_head.wrapping_add(to_submit) & sq_mask;

    let slot = match fd_slot_from_fd(fd as usize) {
        Some(s) => s,
        None => return to_submit as u64,
    };
    ring_put(slot, ctx);

    crate::tasks::set_current_thread(Some(t));

    let _ = flags;
    to_submit as u64
}

/// Read `len` bytes from `fd` into the userspace buffer `addr`.
fn do_read(p: &crate::tasks::Process, fd: i32, addr: u64, len: u32) -> Result<usize, u64> {
    if fd < 0 {
        return Err(EBADF);
    }
    let open_opt = p.fd_table.get(fd as usize).cloned().flatten();
    let Some(open_arc) = open_opt else {
        return Err(EBADF);
    };
    let mut open = open_arc.lock();

    if addr == 0 || len == 0 {
        return Ok(0);
    }
    if !validate_user_write_range(addr, len as usize) {
        return Err(EFAULT);
    }

    let mut buf = vec![0u8; len as usize];
    let n = open.read(&mut buf, 0)?;
    if n > 0 {
        unsafe {
            core::ptr::copy_nonoverlapping(buf.as_ptr(), addr as *mut u8, n);
        }
    }
    Ok(n)
}

/// Write `len` bytes from the userspace buffer `addr` to `fd`.
fn do_write(p: &crate::tasks::Process, fd: i32, addr: u64, len: u32) -> Result<usize, u64> {
    if fd < 0 {
        return Err(EBADF);
    }
    let open_opt = p.fd_table.get(fd as usize).cloned().flatten();
    let Some(open_arc) = open_opt else {
        return Err(EBADF);
    };
    let mut open = open_arc.lock();

    if addr == 0 || len == 0 {
        return Ok(0);
    }
    if !validate_user_read_range(addr, len as usize) {
        return Err(EFAULT);
    }

    let buf = unsafe { core::slice::from_raw_parts(addr as *const u8, len as usize) };
    open.write(buf)
}

/// Read `len` bytes from `fd` at `offset` into the userspace buffer `addr`.
fn do_read_at(
    p: &crate::tasks::Process,
    fd: i32,
    addr: u64,
    len: u32,
    offset: u64,
) -> Result<usize, u64> {
    if fd < 0 {
        return Err(EBADF);
    }
    let open_opt = p.fd_table.get(fd as usize).cloned().flatten();
    let Some(open_arc) = open_opt else {
        return Err(EBADF);
    };
    let open = open_arc.lock();

    if addr == 0 || len == 0 {
        return Ok(0);
    }
    if !validate_user_write_range(addr, len as usize) {
        return Err(EFAULT);
    }

    let mut buf = vec![0u8; len as usize];
    let n = match &open.vnode {
        crate::fs::vfs::VNode::FsNode { node, .. } => {
            match node.read_at(offset as usize, &mut buf) {
                Ok(n) => n,
                Err(e) => return Err(e),
            }
        }
        _ => {
            // Fall back to a positional read for vnodes without read_at.
            let mut o = open_arc.lock();
            match o.read(&mut buf, 0) {
                Ok(n) => n,
                Err(e) => return Err(e),
            }
        }
    };
    if n > 0 {
        unsafe {
            core::ptr::copy_nonoverlapping(buf.as_ptr(), addr as *mut u8, n);
        }
    }
    Ok(n)
}

/// Write `len` bytes from the userspace buffer `addr` to `fd` at `offset`.
fn do_write_at(
    p: &crate::tasks::Process,
    fd: i32,
    addr: u64,
    len: u32,
    offset: u64,
) -> Result<usize, u64> {
    if fd < 0 {
        return Err(EBADF);
    }
    let open_opt = p.fd_table.get(fd as usize).cloned().flatten();
    let Some(open_arc) = open_opt else {
        return Err(EBADF);
    };

    if addr == 0 || len == 0 {
        return Ok(0);
    }
    if !validate_user_read_range(addr, len as usize) {
        return Err(EFAULT);
    }
    let buf = unsafe { core::slice::from_raw_parts(addr as *const u8, len as usize) };

    let mut open = open_arc.lock();
    match &mut open.vnode {
        crate::fs::vfs::VNode::FsNode { node, .. } => match node.write_at(offset as usize, buf) {
            Ok(n) => Ok(n),
            Err(e) => Err(e),
        },
        _ => open.write(buf),
    }
}

/// Look up the ring slot number from an io_uring fd.
fn fd_slot_from_fd(fd: usize) -> Option<u32> {
    let t = crate::tasks::current_thread()?;
    let p = t.process.lock();
    let open = p.fd_table.get(fd).cloned().flatten()?;
    let vnode = open.lock().clone_vnode();
    drop(p);
    if let VNode::IoUring { slot } = vnode {
        Some(slot)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqe_size_is_64() {
        assert_eq!(SQE_SIZE, 64);
    }

    #[test]
    fn cqe_size_is_16() {
        assert_eq!(CQE_SIZE, 16);
    }

    #[test]
    fn params_layout_matches_linux() {
        // io_uring_params is 120 bytes on x86-64: 8 u32 + 3 u32 resv +
        // 2 * 40 bytes of ring offsets.
        assert_eq!(core::mem::size_of::<IoUringParams>(), 120);
        assert_eq!(core::mem::size_of::<IoSqringOffsets>(), 40);
        assert_eq!(core::mem::size_of::<IoCqringOffsets>(), 40);
    }
}
