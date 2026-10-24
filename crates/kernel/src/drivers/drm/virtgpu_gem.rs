//! Line-by-line translation of Linux `drivers/gpu/drm/virtio/virtgpu_gem.c`.
//!
//! Source: `/tmp/opencode/linuxfull/virtgpu_gem.c` (kernel master, 296 lines).
//!
//! ## Status: SPECIFICATION, with one real divergence found
//!
//! The file is mostly object-array plumbing (`virtio_gpu_array_*`, the
//! reservation-locking helpers). Those are listed in `NOT_YET_PORTED` because
//! they need a GEM-object lifetime model we do not have yet.
//!
//! `virtio_gpu_mode_dumb_create` is transcribed in full, because it is the only
//! place in the whole driver where a **pitch** is computed, and pitch is the
//! quantity the shear investigation kept coming back to.

use alloc::vec::Vec;

/// Present in `virtgpu_gem.c` but not yet translated.
pub const NOT_YET_PORTED: &[&str] = &[
    "virtio_gpu_gem_create()",
    "virtio_gpu_gem_object_open() / virtio_gpu_gem_object_close()",
    "virtio_gpu_array_alloc() / _free() / _add_obj() / _lock_resv() / _unlock_resv()",
    "virtio_gpu_lock_one_resv_uninterruptible()",
    "virtio_gpu_array_add_fence()",
    "virtio_gpu_array_put_free() / _put_free_delayed() / _put_free_work()",
    "virtio_gpu_object_array",
];

/// `virtio_gpu_mode_dumb_create` (virtgpu_gem.c:61-100).
///
/// ```c
/// if (args->bpp != 32)
///         return -EINVAL;
///
/// pitch = args->width * 4;
/// args->size = pitch * args->height;
/// args->size = ALIGN(args->size, PAGE_SIZE);
///
/// params.format = virtio_gpu_translate_format(DRM_FORMAT_HOST_XRGB8888);
/// params.width  = args->width;
/// params.height = args->height;
/// params.size   = args->size;
/// params.dumb   = true;
///
/// if (vgdev->has_resource_blob && !vgdev->has_virgl_3d) {
///         params.blob_mem  = VIRTGPU_BLOB_MEM_GUEST;
///         params.blob_flags = VIRTGPU_BLOB_FLAG_USE_SHAREABLE;
///         params.blob = true;
/// }
///
/// ret = virtio_gpu_gem_create(file_priv, dev, &params, &gobj, &args->handle);
/// if (ret) goto fail;
///
/// args->pitch = pitch;
/// return ret;
/// ```
///
/// **G1 — `pitch = width * 4`, with NO alignment.** This is the whole reason
/// this function is transcribed. Note also that alignment is applied to the
/// **total size**, not to the row pitch:
///
/// ```c
/// args->size = pitch * args->height;
/// args->size = ALIGN(args->size, PAGE_SIZE);
/// ```
///
/// So upstream's pitch is exactly `width * 4`, and the page alignment only
/// pads the allocation, never the rows.
///
/// We compute `align_up_32(width * 4, 64)` instead. See `divergences::G1`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DumbCreateParams {
    pub width: u32,
    pub height: u32,
    pub bpp: u32,
    pub has_resource_blob: bool,
    pub has_virgl_3d: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DumbCreateResult {
    /// `args->pitch` -- what the client is told, and what it will use.
    pub pitch: u32,
    /// `args->size` -- the allocation, page-aligned.
    pub size: u64,
    /// `params.blob` -- true when blob memory is requested.
    pub blob: bool,
}

/// Upstream's arithmetic, transcribed exactly.
pub fn dumb_create_upstream(p: DumbCreateParams) -> Result<DumbCreateResult, ()> {
    if p.bpp != 32 {
        return Err(());
    }
    // G1: no alignment on the pitch.
    let pitch = p.width * 4;
    let size = ((pitch as u64) * (p.height as u64) + PAGE_SIZE as u64 - 1)
        & !(PAGE_SIZE as u64 - 1);
    let blob = p.has_resource_blob && !p.has_virgl_3d;
    Ok(DumbCreateResult { pitch, size, blob })
}

/// Page size used for the `ALIGN(args->size, PAGE_SIZE)` above.
pub const PAGE_SIZE: u32 = 4096;

/// What we compute today: the pitch is 64-byte aligned and the size is
/// `pitch * height` with the page rounding done by the page *count*.
pub fn dumb_create_ours(p: DumbCreateParams) -> Result<DumbCreateResult, ()> {
    if p.bpp != 32 {
        return Err(());
    }
    let raw = p.width * (p.bpp / 8);
    let pitch = (raw + 63) & !63u32; // align_up_32(.., 64)
    let size = (pitch as u64) * (p.height as u64);
    let blob = p.has_resource_blob && !p.has_virgl_3d;
    Ok(DumbCreateResult { pitch, size, blob })
}

/// Divergences from `virtgpu_gem.c`.
pub mod divergences {
    /// G1 — our dumb-buffer pitch is 64-byte aligned; upstream's is not.
    ///
    /// Upstream: `pitch = args->width * 4`, with page alignment applied only to
    /// the total size. Ours: `align_up_32(width * 4, 64)`.
    ///
    /// Measured, for the widths that matter:
    ///
    /// | width | upstream | ours | |
    /// |---|---|---|---|
    /// | 1280 | 5120 | 5120 | match |
    /// | 1024 | 4096 | 4096 | match |
    /// | 800 | 3200 | 3200 | match |
    /// | 640 | 2560 | 2560 | match |
    /// | 320 | 1280 | 1280 | match |
    /// | 100 | 400 | **448** | **diverge** |
    ///
    /// Every realistic display width is a multiple of 16 pixels, so
    /// `width * 4` is already 64-byte aligned and the two agree. The
    /// divergence only appears at widths that are not multiples of 16 — and at
    /// 1280, the mode we actually run, they are identical.
    ///
    /// **This is explicitly NOT the shear.** At 1280 both produce 5120, the
    /// value we report to the client, the value we use for the 2D transfer
    /// offset (`x * 4 + y * db.pitch`, which §41 showed is arithmetically
    /// identical to Linux's `x * cpp + y * pitches[0]`), and the value we
    /// declare to the host. All four agree.
    ///
    /// It is still worth aligning, because:
    /// - a client comparing our `args->pitch` against its own expectation of
    ///   `width * 4` sees a different number, and
    /// - it is a silent behavioural difference from the reference driver on a
    ///   path that has no other divergences.
    ///
    /// The 64-byte alignment was presumably added for DMA friendliness. It is
    /// harmless for our sizes, but it is not what Linux does.
    pub const G1_PITCH_ALIGNED: &str =
        "dumb pitch is align_up_32(width*4, 64); upstream uses width*4 unaligned";

    /// G2 — page alignment is applied to the size, not derived from a page count.
    ///
    /// Upstream: `args->size = ALIGN(pitch * height, PAGE_SIZE)`. Ours computes
    /// `pages = ceil(size / PAGE_SIZE)` and allocates that many, which rounds
    /// the same way but keeps `size` itself unaligned. `size` is what the client
    /// reads back from `MAP_DUMB`, so ours can report a size that is not a
    /// multiple of the page size where upstream's always is.
    ///
    /// **Impact: cosmetic but observable.** A client computing buffer extents
    /// from the returned size rather than from its own allocation would differ.
    pub const G2_SIZE_NOT_PAGE_ALIGNED: &str =
        "reported dumb size is not page-aligned; upstream ALIGNs args->size to PAGE_SIZE";
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(width: u32) -> DumbCreateParams {
        DumbCreateParams {
            width,
            height: 800,
            bpp: 32,
            has_resource_blob: true,
            has_virgl_3d: true,
        }
    }

    #[test]
    fn upstream_pitch_is_exactly_width_times_four() {
        // G1: no alignment whatsoever.
        assert_eq!(dumb_create_upstream(p(1280)).unwrap().pitch, 5120);
        assert_eq!(dumb_create_upstream(p(100)).unwrap().pitch, 400);
        assert_eq!(dumb_create_upstream(p(1)).unwrap().pitch, 4);
    }

    #[test]
    fn upstream_size_is_page_aligned() {
        // G2
        let r = dumb_create_upstream(p(100)).unwrap();
        assert_eq!(r.size % PAGE_SIZE as u64, 0);
        assert_eq!(r.size, 320 * 1024); // 400*800 rounded up to 4096
    }

    #[test]
    fn non_32bpp_is_rejected() {
        for bpp in [0u32, 8, 16, 24, 64] {
            let mut q = p(1280);
            q.bpp = bpp;
            assert!(dumb_create_upstream(q).is_err(), "bpp={}", bpp);
            assert!(dumb_create_ours(q).is_err(), "bpp={}", bpp);
        }
    }

    #[test]
    fn ours_agrees_with_upstream_at_realistic_widths() {
        // The measured table in G1. Every mode we actually run matches.
        for w in [320u32, 640, 800, 1024, 1280, 1920] {
            assert_eq!(
                dumb_create_ours(p(w)).unwrap().pitch,
                dumb_create_upstream(p(w)).unwrap().pitch,
                "width {} must agree",
                w
            );
        }
    }

    #[test]
    fn ours_diverges_only_at_unaligned_widths() {
        // Documented divergence, pinned so a change to it is deliberate.
        assert_ne!(
            dumb_create_ours(p(100)).unwrap().pitch,
            dumb_create_upstream(p(100)).unwrap().pitch
        );
    }

    #[test]
    fn blob_requires_resource_blob_and_no_virgl() {
        // Upstream: `has_resource_blob && !has_virgl_3d`
        let mut q = p(1280);
        q.has_virgl_3d = false;
        assert!(dumb_create_upstream(q).unwrap().blob);
        q.has_resource_blob = false;
        assert!(!dumb_create_upstream(q).unwrap().blob);
        // We run with virgl, so dumb buffers are never blobs -- which is why
        // the 2D path transfers and flushes rather than using SET_SCANOUT_BLOB.
        q = p(1280);
        assert!(!dumb_create_upstream(q).unwrap().blob);
    }
}