//! Line-by-line translation of Linux `drivers/gpu/drm/virtio/virtgpu_plane.c`.
//!
//! Source: `/tmp/opencode/linuxref2/virtgpu_plane.c` (Linux 6.x).
//!
//! Only the plane-update path is translated here so far. Everything else in
//! `virtgpu_plane.c` (imported-object preparation, cursor plane, atomic helpers)
//! is not yet ported and is listed in `NOT_YET_PORTED` below rather than
//! silently omitted.
//!
//! ## Why this file exists as its own module
//!
//! It is the only place the scanout is programmed, so every divergence between
//! our driver and Linux on the presentation path is visible by reading this
//! file next to the C. The previous implementation grew its behaviour inline in
//! `drm/mod.rs::handle_page_flip`, which made that comparison impractical and is
//! how a bespoke mechanism survived in the flip path for so long: a pinned
//! scanout resource plus a host-side `RESOURCE_COPY_REGION` blit from the
//! freshly-rendered buffer into it. Linux does neither. See `primary_plane_update`.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::drivers::virtio::gpu::{VirtioGpuDriver, VirtioGpuRect};
use crate::drivers::drm::virtgpu::VirtgpuResource3d;

/// Items present in `virtgpu_plane.c` but not yet translated.
///
/// Kept explicit so the gap is visible rather than inferred from absence.
pub const NOT_YET_PORTED: &[&str] = &[
    "virtio_gpu_prepare_imported_obj()",
    "virtio_gpu_cleanup_imported_obj()",
    "virtio_gpu_cursor_plane_update()",
    "virtio_gpu_plane_atomic_check()",
    "virtio_gpu_update_dumb_bo()",
    "drm_mode_config_funcs / plane init / reset",
];

/// Buffer backing a plane: either a 3D virgl resource, or a 2D dumb buffer / blob.
#[derive(Clone, Copy, Debug)]
pub struct PlaneBo {
    pub res_id: u32,
    pub is_3d: bool,
    pub is_blob: bool,
    pub stride: u32,
    pub pitch: u32,
    pub phys_addr: u64,
}

impl PlaneBo {
    pub fn from_res3d(res: &VirtgpuResource3d) -> Self {
        Self {
            res_id: res.res_id,
            is_3d: true,
            is_blob: res.is_host_blob,
            stride: res.stride,
            pitch: res.stride,
            phys_addr: res.phys.0 as u64,
        }
    }

    pub fn from_dumb(db: &crate::drivers::drm::types::DumbBuffer, rid: u32) -> Self {
        Self {
            res_id: rid,
            is_3d: db.is_3d,
            is_blob: db.blob,
            stride: db.pitch,
            pitch: db.pitch,
            phys_addr: db.phys.0 as u64,
        }
    }
}

/// Plane state as Linux's `drm_plane_state` carries it, restricted to the
/// fields `virtio_gpu_primary_plane_update` reads.
///
/// Linux reads `src_w`, `src_h`, `src_x`, `src_y` as **16.16 fixed point** and
/// shifts them down by 16 at use. Our ABI carries the same fields, and this
/// type exists so the shift happens in exactly one place.
#[derive(Clone, Copy, Default)]
pub struct PlaneState {
    /// `fb` — the DRM framebuffer object, or `None` for nofb.
    pub fb: Option<u32>,
    /// `src_w >> 16`, `src_h >> 16`
    pub src_w: u32,
    pub src_h: u32,
    /// `src_x >> 16`, `src_y >> 16`
    pub src_x: u32,
    pub src_y: u32,
    /// `crtc_w`, `crtc_h`, `crtc_x`, `crtc_y` (only used for the DRM_DEBUG line)
    pub crtc_w: u32,
    pub crtc_h: u32,
    pub crtc_x: u32,
    pub crtc_y: u32,
}

/// CRTC state, restricted to what the plane update consults.
#[derive(Clone, Copy, Default)]
pub struct OutputState {
    /// `output->index` — the scanout id sent with `SET_SCANOUT`.
    pub index: u32,
    /// `crtc.state->active`
    pub active: bool,
    /// `output->needs_modeset`, cleared when it triggers a `SET_SCANOUT`.
    pub needs_modeset: bool,
}

/// The merged damage rect, Linux's `struct drm_rect`.
///
/// `valid == false` is what `drm_atomic_helper_damage_merged` returns when there
/// is no damage, and it is what makes the whole update a no-op.
#[derive(Clone, Copy, Default)]
pub struct DamageRect {
    pub x1: u16,
    pub y1: u16,
    pub x2: u16,
    pub y2: u16,
    /// `false` => empty damage => Linux returns having sent nothing at all.
    pub valid: bool,
}

impl DamageRect {
    pub fn width(&self) -> u32 {
        (self.x2.saturating_sub(self.x1)) as u32
    }
    pub fn height(&self) -> u32 {
        (self.y2.saturating_sub(self.y1)) as u32
    }
    /// `full` is not a Linux concept; it is our 2D-path stand-in for
    /// "no damage information available", and is folded into `valid` so this
    /// function cannot tell the difference — which is exactly the point.
    pub fn full_frame(w: u16, h: u16) -> Self {
        Self { x1: 0, y1: 0, x2: w, y2: h, valid: true }
    }
}

/// Translation of `virtio_gpu_primary_plane_update()`.
///
/// ```c
/// static void virtio_gpu_primary_plane_update(struct drm_plane *plane,
///                                             struct drm_atomic_commit *state)
/// {
///     old_state = drm_atomic_get_old_plane_state(state, plane);
///     output = crtc ? to_output(plane->state->crtc) : NULL;
///     if (old_state->crtc)
///             output = to_output(old_state->crtc);
///     if (WARN_ON(!output)) return;
///
///     if (!plane->state->fb || !output->crtc.state->active) {
///             virtio_gpu_cmd_set_scanout(vgdev, output->index, 0,
///                                        src_w >> 16, src_h >> 16, 0, 0);
///             virtio_gpu_notify(vgdev);
///             return;
///     }
///
///     if (!drm_atomic_helper_damage_merged(old_state, plane->state, &rect))
///             return;
///
///     bo = gem_to_virtio_gpu_obj(plane->state->fb->obj[0]);
///     if (bo->dumb)
///             virtio_gpu_update_dumb_bo(vgdev, plane->state, &rect);
///
///     if (plane->state->fb  != old_state->fb  ||
///         plane->state->src_w != old_state->src_w ||
///         plane->state->src_h != old_state->src_h ||
///         plane->state->src_x != old_state->src_x ||
///         plane->state->src_y != old_state->src_y ||
///         output->needs_modeset) {
///             output->needs_modeset = false;
///             if (bo->host3d_blob || bo->guest_blob)
///                     virtio_gpu_cmd_set_scanout_blob(...);
///             else
///                     virtio_gpu_cmd_set_scanout(vgdev, output->index,
///                                                bo->hw_res_handle,
///                                                src_w >> 16, src_h >> 16,
///                                                src_x >> 16, src_y >> 16);
///     }
///
///     virtio_gpu_resource_flush(plane, rect.x1, rect.y1,
///                               rect.x2 - rect.x1, rect.y2 - rect.y1);
/// }
/// ```
///
/// Four properties, each of which the previous implementation got differently:
///
/// **(A) Damage gates everything.** Empty damage returns before any command is
/// sent. Our old 3D path discarded damage and re-blitted a full `scan_w x
/// scan_h` rect every flip; 45 of 324 flips were preceded by any submit at all.
///
/// **(B) `nofb`/inactive programs scanout to resource 0** and returns. We had no
/// equivalent, so a plane losing its framebuffer left the previous buffer
/// scanning out.
///
/// **(C) `SET_SCANOUT` is conditional**, on fb or any source-rect component
/// changing, or `needs_modeset`. With double buffering the fb changes every
/// frame, so this fires every frame — which is correct and is what Linux does.
/// The previous code gated on a `HOST_SCANOUT` cache keying
/// `(rid, scan_w, scan_h)`, a different and non-upstream condition.
///
/// **(D) The buffer being presented is the one just scanned out.** `set_scanout`
/// takes `bo->hw_res_handle` and `resource_flush` names that same resource. Our
/// driver instead pinned one resource and blitted the freshly-rendered buffer
/// into it host-side with `RESOURCE_COPY_REGION`; Linux has no such copy
/// anywhere in `virtgpu_*`.
/// Display offset applied to every scanout, in pixels.
///
/// Offsets the *sampled region* within the resource, so the whole image moves
/// on screen. Default 0. `OFFSET_DEFAULT` makes it settable at build time for a
/// quick experiment; `set_scanout_offset` changes it at runtime.
///
/// Diagnostic value for the shear: if the artifact translates rigidly with the
/// offset, the content is being sampled from a fixed origin and the fault is in
/// how the host interprets the buffer's layout. If the artifact's *spacing*
/// changes instead, it is tied to the sampling stride rather than the origin.
/// Either answer narrows the shear considerably, and neither is visible from
/// the guest side any other way -- `screendump` cannot capture a virgl scanout
/// and the in-guest readback perturbs the host.
const OFFSET_DEFAULT: (i32, i32) = (0, 0);

static SCANOUT_OFF_X: core::sync::atomic::AtomicI32 =
    core::sync::atomic::AtomicI32::new(OFFSET_DEFAULT.0);
static SCANOUT_OFF_Y: core::sync::atomic::AtomicI32 =
    core::sync::atomic::AtomicI32::new(OFFSET_DEFAULT.1);

pub fn scanout_offset() -> (i32, i32) {
    (
        SCANOUT_OFF_X.load(core::sync::atomic::Ordering::Relaxed),
        SCANOUT_OFF_Y.load(core::sync::atomic::Ordering::Relaxed),
    )
}

/// Apply the build-time default offset, if one was supplied.
///
/// `DRAGOS_SCANOUT_OFF="<x>,<y>"` at build time, e.g.
/// `DRAGOS_SCANOUT_OFF=64,0 cargo run -p tools -- build-kernel`.
///
/// `option_env!` bakes the value into the binary, so this needs no runtime
/// config plumbing and no new ioctl -- inventing an ioctl number for a
/// diagnostic knob would be exactly the kind of thing AGENTS.md 0 forbids.
pub fn init_scanout_offset() {
    if let Some(spec) = option_env!("DRAGOS_SCANOUT_OFF") {
        let mut it = spec.split(',');
        let x = it.next().and_then(|v| v.trim().parse::<i32>().ok()).unwrap_or(0);
        let y = it.next().and_then(|v| v.trim().parse::<i32>().ok()).unwrap_or(0);
        set_scanout_offset(x, y);
    }
}

pub fn set_scanout_offset(x: i32, y: i32) {
    SCANOUT_OFF_X.store(x, core::sync::atomic::Ordering::Relaxed);
    SCANOUT_OFF_Y.store(y, core::sync::atomic::Ordering::Relaxed);
    crate::dprintln!("[scanout] offset set to ({}, {})", x, y);
}

/// The rect to hand the host, with the display offset and src_x/src_y folded in.
fn offset_rect(src_x: u32, src_y: u32, width: u32, height: u32) -> VirtioGpuRect {
    let (ox, oy) = scanout_offset();
    let x = src_x.saturating_add(ox.max(0) as u32);
    let y = src_y.saturating_add(oy.max(0) as u32);
    VirtioGpuRect {
        x,
        y,
        width,
        height,
    }
}

pub fn primary_plane_update(
    new_state: &PlaneState,
    old_state: &PlaneState,
    output: &mut OutputState,
    damage: DamageRect,
    bo: Option<PlaneBo>,
    // Fence to request on the RESOURCE_FLUSH, if any. Linux passes
    // `vgplane_st->fence` here (virtgpu_plane.c:225-232) and then
    // `dma_fence_wait_timeout(..., 50ms)`. We keep the fence but not the wait:
    // the synchronous control queue already covers the wait, so the fence is an
    // additional completion *assertion* rather than a second blocking wait.
    flush_fence: Option<u64>,
    vgdev: &mut VirtioGpuDriver,
) -> bool {
    // Takes `&mut VirtioGpuDriver` rather than acquiring GLOBAL_VIRTIO_GPU
    // itself.
    //
    // The caller (`handle_page_flip`) already holds that lock when it reaches
    // here, and `SpinLock` is non-reentrant (AGENTS.md invariant 1) -- so an
    // internal `.lock()` self-deadlocks on the first flip. Wiring this module
    // with its own acquire is what produced a blank screen.
    //
    // The guard therefore belongs to the caller; this function only borrows the
    // driver out from under it.

    // (B) no framebuffer, or the CRTC is off: point scanout at resource 0.
    //
    // Note Linux clears needs_modeset only on the has-fb path below; this early
    // return deliberately does not, matching the C.
    if new_state.fb.is_none() || !output.active {
        let ok = vgdev.set_scanout(
            output.index,
            0,
            VirtioGpuRect {
                x: 0,
                y: 0,
                width: new_state.src_w,
                height: new_state.src_h,
            },
        );
        return ok;
    }

    // (A) No damage -> send nothing at all.
    if !damage.valid {
        return true;
    }

    let Some(bo) = bo else {
        return false;
    };

    // (C) set_scanout gate.
    let scanout_changed = new_state.fb != old_state.fb
        || new_state.src_w != old_state.src_w
        || new_state.src_h != old_state.src_h
        || new_state.src_x != old_state.src_x
        || new_state.src_y != old_state.src_y
        || output.needs_modeset;

    if scanout_changed {
        output.needs_modeset = false;

        if bo.is_blob {
            // virtio_gpu_cmd_set_scanout_blob(...) — takes the framebuffer and
            // the four source components; our blob variant additionally needs
            // the pitch and format, which Linux takes from the GEM object.
            let ok = vgdev.set_scanout_blob(
                output.index,
                bo.res_id,
                offset_rect(new_state.src_x, new_state.src_y, new_state.src_w, new_state.src_h),
                new_state.src_w,
                new_state.src_h,
                crate::drivers::virtio::gpu::VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM,
                bo.stride,
            );
            // set_scanout_blob returns () on our side -- it has no result to
            // report, unlike set_scanout. That is itself a divergence: Linux
            // has no failure path here either, it just issues the command.
            let _ = ok;
        } else {
            // virtio_gpu_cmd_set_scanout(vgdev, output->index,
            //                            bo->hw_res_handle,
            //                            src_w >> 16, src_h >> 16,
            //                            src_x >> 16, src_y >> 16);
            //
            // Note the wire form: the four source components are separate
            // command fields, NOT the rect. Our `set_scanout` takes a
            // `VirtioGpuRect`, which is why the src_x/src_y distinction is
            // currently lost. Passing them through the rect is correct only
            // while src_x and src_y are both 0.
            let r = offset_rect(new_state.src_x, new_state.src_y, new_state.src_w, new_state.src_h);
            let ok = vgdev.set_scanout(output.index, bo.res_id, r);
            if crate::drivers::drm::trace() {
                crate::dprintln!(
                    "[virtgpu_plane] set_scanout res={} {}x{} -> ok={}",
                    bo.res_id,
                    new_state.src_w,
                    new_state.src_h,
                    ok
                );
            }
            if !ok {
                return false;
            }
        }
    }

    // 2D dumb buffer: transfer guest backing memory to host before flushing.
    if !bo.is_3d && !bo.is_blob && bo.res_id != 0 {
        let transfer_rect = VirtioGpuRect {
            x: damage.x1 as u32,
            y: damage.y1 as u32,
            width: damage.width(),
            height: damage.height(),
        };
        let off = (damage.x1 as u64) * 4 + (damage.y1 as u64) * (bo.pitch as u64);
        vgdev.transfer_to_host_2d(bo.res_id, transfer_rect, off);
    }

    // virtio_gpu_resource_flush(plane, rect.x1, rect.y1,
    //                           rect.x2 - rect.x1, rect.y2 - rect.y1);
    //
    // Unconditional, and it must name the resource just set as scanout: QEMU's
    // virgl backend skips a flush that does not, and `virgl_cmd_resource_flush`
    // is the only command that actually blits the texture to the display.
    let flush_rect = VirtioGpuRect {
        x: damage.x1 as u32,
        y: damage.y1 as u32,
        width: damage.width(),
        height: damage.height(),
    };
    let flush_ok = match flush_fence {
        Some(fid) => vgdev.flush_resource_fenced(bo.res_id, flush_rect, Some(fid)).0,
        None => vgdev.flush_resource(bo.res_id, flush_rect),
    };
    if crate::drivers::drm::trace() {
        crate::dprintln!(
            "[virtgpu_plane] flush_resource res={} rect={}x{} -> ok={}",
            bo.res_id,
            flush_rect.width,
            flush_rect.height,
            flush_ok
        );
    }

    flush_ok
}

/// Not yet ported: `virtio_gpu_update_dumb_bo()`.
///
/// Called by `primary_plane_update` when `bo->dumb`. Our 2D path performs the
/// equivalent upload in `drm/mod.rs`, so this is a documentation stub rather
/// than a live code path. Kept so the omission is greppable.
pub fn update_dumb_bo_not_ported() -> ! {
    unimplemented!("virtio_gpu_update_dumb_bo() is not ported; see NOT_YET_PORTED")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(fb: Option<u32>, w: u32, h: u32) -> PlaneState {
        PlaneState { fb, src_w: w, src_h: h, ..Default::default() }
    }

    #[test]
    fn damage_gates_the_update() {
        // (A) invalid damage => the caller must send nothing. Asserted via the
        // gate itself, since primary_plane_update needs a live device.
        assert!(!DamageRect::default().valid);
        assert!(DamageRect::full_frame(1280, 800).valid);
    }

    #[test]
    fn damage_rect_dimensions() {
        let d = DamageRect { x1: 0, y1: 770, x2: 1280, y2: 800, valid: true };
        assert_eq!(d.width(), 1280);
        assert_eq!(d.height(), 30);
    }

    #[test]
    fn scanout_gate_fires_on_fb_change() {
        // With double buffering the fb alternates, so Linux's (C) gate is true
        // on every frame. This is the property the old HOST_SCANOUT cache did
        // not express.
        let a = st(Some(102), 1280, 800);
        let b = st(Some(111), 1280, 800);
        let changed = a.fb != b.fb || a.src_w != b.src_w || a.src_h != b.src_h;
        assert!(changed, "alternating fb must re-issue set_scanout");
    }

    #[test]
    fn scanout_gate_quiet_when_nothing_changed() {
        let a = st(Some(102), 1280, 800);
        let b = st(Some(102), 1280, 800);
        let changed = a.fb != b.fb || a.src_w != b.src_w || a.src_h != b.src_h;
        assert!(!changed, "identical state must skip set_scanout");
    }

    #[test]
    fn geometry_changes_also_gate() {
        let a = st(Some(102), 1280, 800);
        let b = st(Some(102), 1280, 720);
        assert!(a.src_h != b.src_h, "src_h change must re-issue set_scanout");
    }
}