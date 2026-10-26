#![allow(unused)]

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use core::mem::size_of;
use crate::arch::sync::spinlock::SpinLock;
use shared::memory::address::PhysAddr;

// Reuse the VirtIO-GPU rectangle type so DRM framebuffers and the VirtIO-GPU
// driver share the same rect type.
pub use crate::drivers::virtio::gpu::VirtioGpuRect;

#[derive(Debug, Clone, Copy)]
pub struct DrmModeResolution {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub clock_khz: u32,
    pub preferred: bool,
}

#[derive(Debug, Clone)]
pub struct DrmCrtcState {
    pub id: u32,
    pub connector_id: u32,
    pub encoder_id: u32,
    pub fb_id: u32,
    pub mode: Option<DrmModeResolution>,
    pub mode_blob_id: u32,
    pub active: bool,
    pub x: u32,
    pub y: u32,
    pub gamma_size: u32,
    pub gamma_red: alloc::vec::Vec<u16>,
    pub gamma_green: alloc::vec::Vec<u16>,
    pub gamma_blue: alloc::vec::Vec<u16>,
}

#[derive(Debug, Clone)]
pub struct DrmConnectorState {
    pub id: u32,
    pub crtc_id: u32,
    pub connected: bool,
    pub connector_type: u32,
    pub encoder_id: u32,
    pub modes: Vec<DrmModeResolution>,
}

#[derive(Debug, Clone, Copy)]
pub struct DrmEncoderState {
    pub id: u32,
    pub encoder_type: u32,
    pub crtc_id: u32,
    pub possible_crtcs: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct DrmFramebuffer {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bpp: u32,
    pub depth: u32,
    pub handle: u32,
    pub rect: VirtioGpuRect,
}

#[derive(Debug, Clone, Copy)]
pub struct DumbBuffer {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub bpp: u32,
    pub pitch: u32,
    pub size: u64,
    pub phys: PhysAddr,
    pub pages: usize,
    pub virtio_rid: Option<u32>,
    /// True when rid is a guest-blob resource (host reads backing directly;
    /// no TRANSFER/FLUSH required, only SET_SCANOUT_BLOB).
    pub blob: bool,
    /// True when rid came from VIRGL_RESOURCE_CREATE_3D, i.e. it is a
    /// host-managed virgl texture rather than a 2D resource with guest
    /// backing. No TRANSFER_TO_HOST_2D: there is no guest memory for the
    /// host to copy from, the rendered content is already resident in the
    /// resource. RESOURCE_FLUSH *is* required and is not 2D-only -- it is
    /// what makes QEMU blit the registered scanout texture to the display
    /// (see `virgl_cmd_resource_flush` in hw/display/virtio-gpu-virgl.c).
    pub is_3d: bool,
}

/// A PRIME DMA-BUF file descriptor backing.
///
/// Wraps a virtio-gpu resource handle (res_id) and/or GEM handle so that
/// `DRM_IOCTL_PRIME_HANDLE_TO_FD` can hand out a real VFS fd, `fcntl
/// F_DUPFD_CLOEXEC` can duplicate it across the Wayland socket, and
/// `DRM_IOCTL_PRIME_FD_TO_HANDLE` can recover the handle on the receiving
/// side (mywima).
#[derive(Debug, Clone)]
pub struct DmaBufFile {
    /// The GEM / DRM handle (bo_handle) used internally by the kernel.
    pub handle: u32,
    /// The virtio-gpu resource ID (res_id) for blob resources.
    pub res_id: Option<u32>,
    /// Size of the buffer in bytes.
    pub size: u64,
    /// Whether this is a blob resource (vs a dumb buffer).
    pub is_blob: bool,
}

#[derive(Debug, Clone)]
pub struct DrmPlaneState {
    pub id: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub crtc_x: i32,
    pub crtc_y: i32,
    pub crtc_w: u32,
    pub crtc_h: u32,
    pub src_x: u32,
    pub src_y: u32,
    pub src_w: u32,
    pub src_h: u32,
    pub possible_crtcs: u32,
    pub plane_type: u32,
    pub formats: Vec<u32>,
}

pub struct DrmDevice {
    pub crtcs: Vec<DrmCrtcState>,
    pub connectors: Vec<DrmConnectorState>,
    pub encoders: Vec<DrmEncoderState>,
    pub planes: Vec<DrmPlaneState>,
    pub framebuffers: BTreeMap<u32, DrmFramebuffer>,
    /// Fixed-size pool of dumb buffers (Linux-style pre-allocation).
    /// ponytail: 128 slots avoids dynamic Vec::grow in hot path (allocator crash)
    pub dumb_buffers: [Option<DumbBuffer>; 128],
    /// PRIME dma-buf registry: synthetic fd -> underlying dumb-buffer handle.
    pub prime_handles: BTreeMap<u32, u32>,
    pub next_prime_fd: u32,
    pub next_id: u32,
    /// PID of the current DRM master (None = no master / restored to text mode).
    pub master_pid: Option<u64>,
    /// Previous keyboard mode before the master switched to graphics (KD_GRAPHICS=1).
    /// Restored on DROP_MASTER or process exit.
    pub prev_kb_mode: i32,
    /// Previous VT mode (VT_AUTO=0 vs VT_PROCESS=1). Restored on DROP_MASTER.
    pub prev_vt_mode: i32,
    pub property_blobs: BTreeMap<u32, Vec<u8>>,
    pub atomic_enabled: bool,
}

impl DrmDevice {
pub fn new() -> Self {
        Self {
            crtcs: Vec::new(),
            connectors: Vec::new(),
            encoders: Vec::new(),
            planes: Vec::new(),
            framebuffers: BTreeMap::new(),
            dumb_buffers: [None; 128],
            prime_handles: BTreeMap::new(),
            property_blobs: BTreeMap::new(),
            next_prime_fd: 100,
            next_id: 1,
            master_pid: None,
            prev_kb_mode: 0,
            prev_vt_mode: 0,
            atomic_enabled: false,
        }
    }

    pub fn alloc_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

/// Allocate a dumb buffer slot from the fixed-size pool (Linux-style).
    /// ponytail: linear scan reuses destroy()ed slots; None only if 128 live
    pub fn alloc_dumb_slot(&mut self) -> Option<usize> {
        self.dumb_buffers.iter().position(|b| b.is_none())
    }

    /// Find a dumb buffer by handle in the fixed pool.
    pub fn find_dumb(&self, handle: u32) -> Option<&DumbBuffer> {
        self.dumb_buffers
            .iter()
            .filter_map(|b| b.as_ref())
            .find(|b| b.id == handle)
    }

    /// Find a dumb buffer by handle in the fixed pool (mutable).
    pub fn find_dumb_mut(&mut self, handle: u32) -> Option<&mut DumbBuffer> {
        self.dumb_buffers
            .iter_mut()
            .filter_map(|b| b.as_mut())
            .find(|b| b.id == handle)
    }

    /// Check if a dumb buffer with handle exists.
    pub fn has_dumb(&self, handle: u32) -> bool {
        self.find_dumb(handle).is_some()
    }

    /// Validate a userspace mmap of the DRM node and resolve it to the
    /// physical range it is allowed to touch.
    ///
    /// `MAP_DUMB` reports the buffer's *physical base* as the mmap offset (see
    /// `handle_map_dumb`), so `offset` arrives as a physical address. Passing
    /// it straight to `mmap_device` would let any process holding a DRM fd
    /// map arbitrary physical memory read-write, so require instead that it
    /// fall inside a currently-live dumb buffer, and clamp the length to that
    /// buffer's end.
    ///
    /// Returns `Some((phys, len))` when the range is inside a live buffer.
    pub fn resolve_dumb_mmap(&self, offset: u64, len: usize) -> Option<(PhysAddr, usize)> {
        let db = self
            .dumb_buffers
            .iter()
            .filter_map(|b| b.as_ref())
            .find(|b| {
                let base = b.phys.0;
                let end = base.checked_add(b.size);
                match end {
                    Some(end) => offset >= base && offset < end,
                    None => false,
                }
            })?;
        let end = db.phys.0.checked_add(db.size)?;
        let phys = offset;
        let remaining = (end - phys) as usize;
        Some((PhysAddr(phys), core::cmp::min(len, remaining)))
    }

    /// initial mode-set), followed by common fallback modes.
    pub fn add_standard_modes(native_w: u32, native_h: u32) -> Vec<DrmModeResolution> {
        let mut modes = Vec::new();
        // Native mode first (preferred)
        modes.push(DrmModeResolution {
            width: native_w,
            height: native_h,
            refresh_hz: 60,
            clock_khz: clock_for(native_w, native_h),
            preferred: true,
        });
        // Common fallbacks (skip if same as native)
        for &(w, h, clk) in &[
            (1920u32, 1080u32, 148500u32),
            (1280u32,  800u32,  83500u32),
            (1280u32,  720u32,  74250u32),
            (1024u32,  768u32,  65000u32),
            ( 800u32,  600u32,  40000u32),
        ] {
            if w != native_w || h != native_h {
                modes.push(DrmModeResolution {
                    width: w,
                    height: h,
                    refresh_hz: 60,
                    clock_khz: clk,
                    preferred: false,
                });
            }
        }
        modes
    }
}

fn clock_for(w: u32, h: u32) -> u32 {
    // Rough pixel-clock estimate: w × h × 60 Hz × 1.1 (blanking) / 1000
    (w as u64 * h as u64 * 66 / 1000) as u32
}
