//! Minimal modern-transport virtio driver: one split virtqueue, polling.
//! Virtio spec 1.2: PCI transport (4.1), block device (5.2).
pub mod blk;
pub mod gpu;
pub mod net;

use core::{
    ptr::{read_volatile, write_volatile},
    sync::atomic::{fence, Ordering},
};

use shared::{
    boot::boot_info::HHDM_BASE,
    memory::{
        address::{PAGE_SIZE, PhysAddr, VirtAddr},
        allocator::FrameAllocator,
    },
};

use crate::{
    arch::x86_64::pci::PciAddress,
    memory::FRAME_ALLOCATOR,
};

pub const VIRTIO_VENDOR_ID: u16 = 0x1AF4;
pub const VIRTIO_DEVICE_BLOCK: u16 = 0x1042; // modern
pub const VIRTIO_DEVICE_BLOCK_LEGACY: u16 = 0x1001; // transitional
pub const VIRTIO_DEVICE_NET: u16 = 0x1041; // modern
pub const VIRTIO_DEVICE_NET_LEGACY: u16 = 0x1000; // transitional

const CAP_ID_VNDR: u8 = 0x09;
const CAP_COMMON_CFG: u8 = 1;
const CAP_NOTIFY_CFG: u8 = 2;
#[allow(dead_code)]
const CAP_ISR_CFG: u8 = 3;
const CAP_DEVICE_CFG: u8 = 4;

const REG_DEVICE_FEATURE_SELECT: u32 = 0;
const REG_DEVICE_FEATURE: u32 = 4;
const REG_DRIVER_FEATURE_SELECT: u32 = 8;
const REG_DRIVER_FEATURE: u32 = 12;
const REG_DEVICE_STATUS: u32 = 20;
const REG_QUEUE_SELECT: u32 = 22;
const REG_QUEUE_SIZE: u32 = 24;
const REG_QUEUE_ENABLE: u32 = 28;
const REG_QUEUE_NOTIFY_OFF: u32 = 30;
const REG_QUEUE_DESC: u32 = 32;
const REG_QUEUE_DRIVER: u32 = 40;
const REG_QUEUE_DEVICE: u32 = 48;

const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FEATURES_OK: u8 = 8;

const VIRTQ_DESC_F_NEXT: u16 = 1;
pub const VIRTQ_DESC_F_WRITE: u16 = 2;

/// Physically-contiguous DMA buffer, reached through the HHDM mapping.
pub struct DmaBuf {
    phys: PhysAddr,
    virt: VirtAddr,
    len: usize,
}

impl DmaBuf {
    pub fn new(bytes: usize) -> Option<Self> {
        let pages = bytes.div_ceil(PAGE_SIZE as usize);
        let phys = FRAME_ALLOCATOR.lock().alloc_pages(pages, PAGE_SIZE).ok()?;
        Some(Self {
            phys,
            virt: VirtAddr::new(HHDM_BASE.0 + phys.0),
            len: pages * PAGE_SIZE as usize,
        })
    }

    /// DMA buffer aligned to `align` (must be a multiple of PAGE_SIZE).
    /// Over-allocates and rounds the physical base up; head pages are wasted.
    pub fn new_aligned(bytes: usize, align: usize) -> Option<Self> {
        let pages = bytes.div_ceil(PAGE_SIZE as usize);
        let extra = align / PAGE_SIZE as usize;
        let raw = FRAME_ALLOCATOR
            .lock()
            .alloc_pages(pages + extra, PAGE_SIZE)
            .ok()?;
        let phys = PhysAddr::new((raw.0 + align as u64 - 1) & !(align as u64 - 1));
        Some(Self {
            phys,
            virt: VirtAddr::new(HHDM_BASE.0 + phys.0),
            len: pages * PAGE_SIZE as usize,
        })
    }

    pub fn phys(&self) -> PhysAddr {
        self.phys
    }

    pub fn virt_addr(&self) -> VirtAddr {
        self.virt
    }

    pub fn as_mut_ptr(&self) -> *mut u8 {
        self.virt.0 as *mut u8
    }

    pub fn len(&self) -> usize {
        self.len
    }
}

#[derive(Clone, Copy)]
pub struct CommonCfg {
    base: VirtAddr,
}

impl CommonCfg {
    pub fn new(base: VirtAddr) -> Self {
        Self { base }
    }

    pub unsafe fn read_u8(&self, off: u32) -> u8 {
        unsafe { read_volatile((self.base.0 + u64::from(off)) as *const u8) }
    }
    pub unsafe fn write_u8(&self, off: u32, v: u8) {
        unsafe { write_volatile((self.base.0 + u64::from(off)) as *mut u8, v) }
    }
    pub unsafe fn read_u16(&self, off: u32) -> u16 {
        unsafe { read_volatile((self.base.0 + u64::from(off)) as *const u16) }
    }
    pub unsafe fn write_u16(&self, off: u32, v: u16) {
        unsafe { write_volatile((self.base.0 + u64::from(off)) as *mut u16, v) }
    }
    pub unsafe fn read_u32(&self, off: u32) -> u32 {
        unsafe { read_volatile((self.base.0 + u64::from(off)) as *const u32) }
    }
    pub unsafe fn write_u32(&self, off: u32, v: u32) {
        unsafe { write_volatile((self.base.0 + u64::from(off)) as *mut u32, v) }
    }
}

struct Cap {
    bar: u8,
    offset: u32,
    notify_off_multiplier: u32,
}

/// Locates a virtio structure (common/notify/isr/device config) by walking the
/// PCI capability list and matching the vendor-cap `cfg_type` field.
fn find_cap(addr: &PciAddress, want: u8) -> Option<Cap> {
    if unsafe { addr.read_config_u16(0x06) } & (1 << 4) == 0 {
        return None;
    }
    let mut off = u32::from(unsafe { addr.read_config_u8(0x34) });
    while off != 0 {
        if unsafe { addr.read_config_u8(off) } == CAP_ID_VNDR {
            let len = u32::from(unsafe { addr.read_config_u8(off + 2) });
            if unsafe { addr.read_config_u8(off + 3) } == want {
                return Some(Cap {
                    bar: unsafe { addr.read_config_u8(off + 4) },
                    offset: unsafe { addr.read_config_u32(off + 8) },
                    notify_off_multiplier: if want == CAP_NOTIFY_CFG && len >= 20 {
                        unsafe { addr.read_config_u32(off + 16) }
                    } else {
                        4
                    },
                });
            }
        }
        off = u32::from(unsafe { addr.read_config_u8(off + 1) });
    }
    None
}

#[repr(C)]
struct VirtqDesc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

/// Single-flight split virtqueue: descriptors 0..n are rewritten per request,
/// and every request is completed (polled) before the next is submitted.
pub struct VirtQueue {
    size: u16,
    desc: DmaBuf,  // 16 * size bytes
    avail: DmaBuf, // 4 + 2 * size bytes
    used: DmaBuf,  // 4 + 8 * size bytes
    last_used: u16,
    notify_addr: u64,
    /// Descriptor slots currently handed out and not yet retired.
    ///
    /// This is what makes more than one command outstanding possible. It was
    /// absent because `push` hardcoded `head = 0` and `wait` asserted the
    /// retired id was 0, which structurally limited the queue to depth 1
    /// regardless of what the device advertises. Linux instead allocates a
    /// `virtio_gpu_vbuffer` per command from a free list
    /// (virtgpu_vq.c, `virtio_gpu_alloc_cmd`).
    ///
    /// Bounded by `MAX_INFLIGHT` rather than by queue size so the bitmap stays
    /// a fixed array: a queue of 256 would otherwise need 32 bytes of bitmap
    /// per queue, and the control queue only ever needs a handful.
    in_flight: [bool; MAX_INFLIGHT],
}

/// Descriptors that may be outstanding at once. Deeper than any single virtio
/// GPU client needs, and small enough that `in_flight` is a fixed array.
pub const MAX_INFLIGHT: usize = 16;
const _: () = assert!(MAX_INFLIGHT <= 256);

impl VirtQueue {
    pub fn new(common: &CommonCfg, notify: VirtAddr, multiplier: u32) -> Option<Self> {
        let size = unsafe { common.read_u16(REG_QUEUE_SIZE) };
        if size == 0 || !size.is_power_of_two() {
            return None;
        }
        let desc = DmaBuf::new(16 * size as usize)?;
        let avail = DmaBuf::new(4 + 2 * size as usize)?;
        let used = DmaBuf::new(4 + 8 * size as usize)?;

        // ponytail: QEMU gives rings below 4G; the 32-bit address fields cap us at 4G.
        unsafe {
            common.write_u32(REG_QUEUE_DESC, desc.phys().0 as u32);
            common.write_u32(REG_QUEUE_DRIVER, avail.phys().0 as u32);
            common.write_u32(REG_QUEUE_DEVICE, used.phys().0 as u32);
        }

        let notify_off = unsafe { common.read_u16(REG_QUEUE_NOTIFY_OFF) };
        Some(Self {
            size,
            desc,
            avail,
            used,
            last_used: 0,
            notify_addr: notify.0 + u64::from(notify_off) * u64::from(multiplier),
            in_flight: [false; MAX_INFLIGHT],
        })
    }

    /// Reserve a descriptor slot for a command.
    ///
    /// Returns `None` when `MAX_INFLIGHT` commands are already outstanding,
    /// which is the queue's real depth limit. Callers must pair every `Some`
    /// with a `wait_for` (or `try_retire`) so the slot is released; a leaked
    /// slot permanently costs one unit of queue depth.
    pub fn alloc_head(&mut self) -> Option<u16> {
        let slots = core::cmp::min(self.size as usize, MAX_INFLIGHT);
        for i in 1..slots {
            if !self.in_flight[i] {
                self.in_flight[i] = true;
                return Some(i as u16);
            }
        }
        None
    }

    /// Release a reserved slot without waiting for completion.
    ///
    /// For the error paths: a command that could not even be submitted must
    /// not consume queue depth.
    pub fn release_head(&mut self, head: u16) {
        if (head as usize) < MAX_INFLIGHT {
            self.in_flight[head as usize] = false;
        }
    }

    /// Drain newly-retired used-ring entries, returning any that match `head`.
    ///
    /// Advances `last_used` past every entry examined, so a completion for a
    /// *different* head is consumed and discarded rather than blocking the one
    /// we asked about. That is the whole point of a multi-outstanding queue:
    /// completions arrive in submission order but callers wait on their own.
    fn consume_used_until(&mut self, head: u16) -> Option<u32> {
        let base = (self.used.virt_addr().0 + 4) as *const u32;
        loop {
            let used_idx = unsafe {
                read_volatile((self.used.virt_addr().0 + 2) as *const u16)
            };
            if used_idx == self.last_used {
                return None;
            }
            fence(Ordering::Acquire);
            let slot = (self.last_used & (self.size - 1)) as usize;
            // used ring entry: {le32 id; le32 len} -- two u32 words
            let id = unsafe { read_volatile(base.add(slot * 2)) };
            // The second word is the device-written length. It is not
            // decoration: for virtio-gpu it *is* the response size, and for
            // variable-length responses (DISPLAY_INFO, GET_CAPSET) it is the
            // only authoritative statement of how much the host sent. Reading
            // a length out of the payload instead is a guess that happens to
            // work for fixed-size responses and silently fails for these.
            let len = unsafe { read_volatile(base.add(slot * 2 + 1)) };
            self.last_used = self.last_used.wrapping_add(1);
            let matched = id as u16 == head;
            if (id as usize) < MAX_INFLIGHT {
                self.in_flight[id as usize] = false;
            }
            if matched {
                return Some(len);
            }
        }
    }

    /// As `consume_used_until`, but reports the device-written response length.
    fn consume_used_len(&mut self, head: u16) -> Option<u32> {
        self.consume_used_until(head)
    }

    /// Wait until the command occupying `head` has been retired by the device.
    /// Wait for `head` to retire, returning the device-written response length.
    ///
    /// The length matters: see `consume_used_until`. Callers that only care
    /// whether the command completed can ignore it.
    pub fn wait_for(&mut self, head: u16) -> Result<u32, &'static str> {
        for _ in 0..1_000_000 {
            if let Some(len) = self.consume_used_until(head) {
                return Ok(len);
            }
            core::hint::spin_loop();
        }
        self.release_head(head);
        Err("virtio device timeout")
    }

    /// Non-blocking completion check for one head.
    pub fn try_retire(&mut self, head: u16) -> bool {
        self.consume_used_len(head).is_some()
    }

    /// Writes a descriptor chain starting at `head` and kicks the device.
    ///
    /// `head` is the descriptor index reserved by `alloc_head`; the chain is
    /// written at `head..head+parts.len()` and `head` is published to the
    /// avail ring. Chains are therefore per-command rather than all starting at
    /// descriptor 0, which is what allows depth > 1.
    pub fn push_at(&mut self, head: u16, parts: &[(u64, u32, u16)]) -> Result<(), &'static str> {
        let h = head as usize;
        if parts.len() > self.size as usize || h + parts.len() > self.size as usize {
            return Err("descriptor chain too long");
        }
        let desc = self.desc.as_mut_ptr() as *mut VirtqDesc;
        for (i, (addr, len, flags)) in parts.iter().enumerate() {
            let last = i + 1 == parts.len();
            unsafe {
                let d = &mut *desc.add(h + i);
                d.addr = *addr;
                d.len = *len;
                d.flags = *flags;
                d.next = if last { 0 } else { (h + i + 1) as u16 };
                if !last {
                    d.flags |= VIRTQ_DESC_F_NEXT;
                }
            }
        }

        let avail_idx = unsafe { read_volatile((self.avail.virt_addr().0 + 2) as *const u16) };
        let ring = (self.avail.virt_addr().0 + 4) as *mut u16;
        unsafe { write_volatile(ring.add((avail_idx & (self.size - 1)) as usize), head) };
        fence(Ordering::Release);
        unsafe {
            write_volatile((self.avail.virt_addr().0 + 2) as *mut u16, avail_idx.wrapping_add(1));
        }
        unsafe { write_volatile(self.notify_addr as *mut u16, 0) };
        Ok(())
    }

    /// Head-0 convenience wrapper, retained for the net and block users which
    /// remain synchronous. The GPU control queue uses `push_at`/`wait_for`.
    pub fn push(&mut self, parts: &[(u64, u32, u16)]) -> Result<(), &'static str> {
        self.push_at(0, parts)
    }

    /// Spins until the device completes the in-flight request.
    /// Wait for the next used entry (descriptor 0 chains), returning the
    /// device-written response length.
    ///
    /// The length is the used ring's `len` word. Callers may ignore it, but for
    /// variable-length virtio-gpu responses it is the only authoritative measure
    /// of how much the host sent -- see `consume_used_until`.
    pub fn wait(&mut self) -> Result<u32, &'static str> {
        for _ in 0..100_000_000 {
            let used_idx = unsafe { read_volatile((self.used.virt_addr().0 + 2) as *const u16) };
            if used_idx != self.last_used {
                fence(Ordering::Acquire);
                let slot = (self.last_used & (self.size - 1)) as usize;
                let base = (self.used.virt_addr().0 + 4) as *const u32;
                let id = unsafe { read_volatile(base.add(slot * 2)) };
                let len = unsafe { read_volatile(base.add(slot * 2 + 1)) };
                self.last_used = self.last_used.wrapping_add(1);
                if id as u16 != 0 {
                    return Err("unexpected used-ring descriptor id");
                }
                return Ok(len);
            }
            core::hint::spin_loop();
        }
        Err("virtio device timeout")
    }

    /// Post a single buffer to a specific descriptor slot and add to available ring.
    pub fn post_buffer(&mut self, desc_idx: u16, addr: u64, len: u32, flags: u16) {
        let desc = self.desc.as_mut_ptr() as *mut VirtqDesc;
        unsafe {
            let d = &mut *desc.add(desc_idx as usize);
            d.addr = addr;
            d.len = len;
            d.flags = flags;
            d.next = 0;
            let avail_idx = read_volatile((self.avail.virt_addr().0 + 2) as *const u16);
            let ring = (self.avail.virt_addr().0 + 4) as *mut u16;
            write_volatile(ring.add((avail_idx & (self.size - 1)) as usize), desc_idx);
            fence(Ordering::Release);
            write_volatile((self.avail.virt_addr().0 + 2) as *mut u16, avail_idx.wrapping_add(1));
        }
    }

    /// Notify device of new available buffers.
    pub fn notify(&self) {
        unsafe { write_volatile(self.notify_addr as *mut u16, 0) };
    }

    /// Non-blocking check for a completed descriptor in the used ring.
    /// Returns Some((desc_id, written_len)) if a descriptor was retired by the device.
    pub fn try_pop_used(&mut self) -> Option<(u32, u32)> {
        let used_idx = unsafe { read_volatile((self.used.virt_addr().0 + 2) as *const u16) };
        if used_idx == self.last_used {
            return None;
        }
        fence(Ordering::Acquire);
        let slot = (self.last_used & (self.size - 1)) as usize;
        let base = (self.used.virt_addr().0 + 4) as *const u32;
        let id = unsafe { read_volatile(base.add(slot * 2)) };
        let len = unsafe { read_volatile(base.add(slot * 2 + 1)) };
        self.last_used = self.last_used.wrapping_add(1);
        Some((id, len))
    }
}
