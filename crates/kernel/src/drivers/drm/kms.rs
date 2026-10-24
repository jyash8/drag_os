//! KMS backend trait and GOP (Graphics Output Protocol) bridge.
//!
//! The `KmsBackend` trait abstracts the hardware scanout path.
//! `GopBridge` implements it by blitting directly into the UEFI GOP
//! linear framebuffer that was mapped by the bootloader.
//!
//! Later driver stages (VirtIO-GPU, Intel i915) will provide their own
//! `KmsBackend` implementations.

#![allow(unused)]

use alloc::{sync::Arc, vec, vec::Vec};
use shared::boot::boot_info::HHDM_BASE;
use shared::memory::address::PhysAddr;

use crate::arch::sync::spinlock::SpinLock;
use crate::drivers::virtio::gpu::VirtioGpuDriver;
use super::types::DrmModeResolution;

// ── Trait ─────────────────────────────────────────────────────────────────────

/// Abstracts the per-hardware scanout path for the DRM/KMS core.
pub trait KmsBackend: Send + Sync {
    /// Program the scanout head to display `fb_phys` at the given geometry.
    /// Program the scanout for `fb_phys`. Returns false when the host
    /// refused the request, so callers can avoid reporting a page flip that
    /// never actually reached the display.
    fn set_scanout(&self, fb_phys: u64, width: u32, height: u32, pitch: u32) -> bool;

    /// Flip to a new framebuffer (may be async; always synchronous here).
    /// Returns false if the flip could not be programmed.
    fn page_flip(&self, fb_phys: u64) -> bool;

    /// Return the list of display modes supported by this backend.
    fn get_modes(&self) -> Vec<DrmModeResolution>;
}

// ── GOP bridge ────────────────────────────────────────────────────────────────

/// KMS backend wrapping the UEFI GOP linear framebuffer.
///
/// Works on any QEMU/KVM setup where the bootloader configures a GOP frame
/// buffer; no hardware registers need to be touched.  Blits pixels via the
/// HHDM direct-physical mapping.
pub struct GopBridge {
    /// Physical address of the GOP linear framebuffer.
    pub phys_addr: u64,
    pub width:     u32,
    pub height:    u32,
    /// Row pitch in pixels (same as `stride` in `FramebufferInfo`).
    pub stride:    u32,
}

impl KmsBackend for GopBridge {
    fn set_scanout(&self, fb_phys: u64, width: u32, height: u32, pitch: u32) -> bool {
        // Blit from dumb buffer to GOP framebuffer.
        let blit_w  = width.min(self.width) as usize;
        let blit_h  = height.min(self.height) as usize;
        let dst_pitch = (self.stride * 4) as usize;
        let src_pitch = pitch as usize;

        let src = (HHDM_BASE.0 + fb_phys) as *const u8;
        let dst = (HHDM_BASE.0 + self.phys_addr) as *mut u8;

        for row in 0..blit_h {
            // SAFETY: both pointers are in HHDM (kernel-owned physical RAM).
            unsafe {
                core::ptr::copy_nonoverlapping(
                    src.add(row * src_pitch),
                    dst.add(row * dst_pitch),
                    blit_w * 4,
                );
            }
        }
        true
    }

    fn page_flip(&self, fb_phys: u64) -> bool {
        // Synchronous flip: just blit at full resolution.
        self.set_scanout(fb_phys, self.width, self.height, self.stride * 4)
    }

    fn get_modes(&self) -> Vec<DrmModeResolution> {
        alloc::vec![DrmModeResolution {
            width:      self.width,
            height:     self.height,
            refresh_hz: 60,
            clock_khz:  (self.width as u64 * self.height as u64 * 66 / 1000) as u32,
            preferred:  true,
        }]
    }
}

// ── Global KMS backend ────────────────────────────────────────────────────────

/// The active KMS backend.  Set once during `drm_init()`; read by ioctl handlers.
pub static KMS_BACKEND: SpinLock<Option<Arc<dyn KmsBackend>>> = SpinLock::new(None);

/// Simple wrapper for VirtIO-GPU dimensions to implement KmsBackend
pub struct VirtioGpuBackend {
    width: u32,
    height: u32,
}

impl KmsBackend for VirtioGpuBackend {
    fn set_scanout(&self, fb_phys: u64, width: u32, height: u32, pitch: u32) -> bool {
        let rid_opt = {
            let drm = super::GLOBAL_DRM.lock();
            if let Some(d) = drm.as_ref() {
                d.dumb_buffers.iter()
                    .filter_map(|b| b.as_ref())
                    .find(|db| db.phys.0 == fb_phys)
                    .and_then(|db| db.virtio_rid)
            } else {
                None
            }
        };

        // The host's verdict on the scanout is what decides whether this flip
        // is real; the transfer/flush afterwards only matter if it is.
        let mut ok = true;
        if let Some(mut g) = crate::drivers::virtio::gpu::GLOBAL_VIRTIO_GPU.lock().as_mut() {
            let rect = crate::drivers::virtio::gpu::VirtioGpuRect { x: 0, y: 0, width, height };
            if let Some(rid) = rid_opt {
                ok = g.set_scanout(0, rid, rect);
                if ok {
                    ok = g.transfer_to_host_2d(rid, rect, 0) && g.flush_resource(rid, rect);
                }
            } else {
                ok = g.transfer_to_host_2d(1, rect, 0) && g.flush_resource(1, rect);
            }
        }
        if let Some(fb) = unsafe { crate::graphics::FRAMEBUFFER_INFO } {
            let blit_w = width.min(fb.width as u32) as usize;
            let blit_h = height.min(fb.height as u32) as usize;
            let dst_pitch = (fb.stride * 4) as usize;
            let src_pitch = pitch as usize;
            let src = (HHDM_BASE.0 + fb_phys) as *const u8;
            let dst = (HHDM_BASE.0 + fb.address.0) as *mut u8;
            for row in 0..blit_h {
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        src.add(row * src_pitch),
                        dst.add(row * dst_pitch),
                        blit_w * 4,
                    );
                }
            }
        }
        ok
    }

    fn page_flip(&self, fb_phys: u64) -> bool {
        self.set_scanout(fb_phys, self.width, self.height, self.width * 4)
    }

    fn get_modes(&self) -> Vec<DrmModeResolution> {
        vec![DrmModeResolution {
            width: self.width,
            height: self.height,
            refresh_hz: 60,
            clock_khz: (self.width as u64 * self.height as u64 * 66 / 1000) as u32,
            preferred: true,
        }]
    }
}

/// Construct a KMS backend, preferring VirtIO-GPU if detected, falling back to
/// the boot framebuffer (GOP).  Returns `true` on success.
pub fn init_kms_backend() -> bool {
    // Prefer VirtIO-GPU when present (better performance, proper scanout)
    let (virtio_width, virtio_height) = crate::drivers::virtio::gpu::GLOBAL_VIRTIO_GPU.lock()
        .as_ref()
        .map(|gpu| (gpu.width, gpu.height))
        .unwrap_or((0, 0));

    if virtio_width > 0 && virtio_height > 0 {
        *KMS_BACKEND.lock() = Some(Arc::new(VirtioGpuBackend {
            width: virtio_width,
            height: virtio_height,
        }));
        return true;
    }

    let fb = match unsafe { crate::graphics::FRAMEBUFFER_INFO } {
        Some(f) => f,
        None    => return false,
    };

    let bridge = GopBridge {
        phys_addr: fb.address.0,
        width:     fb.width  as u32,
        height:    fb.height as u32,
        stride:    fb.stride as u32,
    };

    *KMS_BACKEND.lock() = Some(Arc::new(bridge));
    true
}

/// Poll virtio-gpu scanout/connector status and emit netlink uevent on change.
/// ponytail: rate-limited to ~1/sec — get_display_info is MMIO-heavy and
/// this runs on every idle-loop pass, which flaps constantly under load
/// (RUNNABLE_COUNT churn wakes the hlt loop). Hotplug latency of 1s is fine.
pub fn check_hotplug() {
    static LAST_CONNECTED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
    static LAST_CHECK: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

    let now = crate::time::TICKS.load(core::sync::atomic::Ordering::Relaxed) as u64;
    if now.saturating_sub(LAST_CHECK.load(core::sync::atomic::Ordering::Relaxed)) < 1000 {
        return;
    }
    LAST_CHECK.store(now, core::sync::atomic::Ordering::Relaxed);

    let is_connected = {
        if let Some(mut g) = crate::drivers::virtio::gpu::GLOBAL_VIRTIO_GPU.lock().as_mut() {
            g.get_display_info().is_some()
        } else {
            true // GOP fallback is always connected
        }
    };

    let prev = LAST_CONNECTED.swap(is_connected, core::sync::atomic::Ordering::AcqRel);
    if prev != is_connected {
        // Broadcast hotplug uevent on dri/card0
        let props: [(&str, &str); 5] = [
            ("SUBSYSTEM", "drm"),
            ("MAJOR", "226"),
            ("MINOR", "0"),
            ("DEVNAME", "dri/card0"),
            ("HOTPLUG", "1"),
        ];
        crate::drivers::uevent::broadcast("change", "/devices/pci0000:00/0000:00:02.0/drm/card0", &props);
        crate::dprintln!("[DRM] hotplug change detected: connected={}", is_connected);
    }
}
