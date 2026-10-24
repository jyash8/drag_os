//! Line-by-line translation of Linux `drivers/gpu/drm/virtio/virtgpu_submit.c`.
//!
//! Source: `/tmp/opencode/linuxref2/virtgpu_submit.c` (Linux 6.x, 551 lines).
//!
//! ## Status: SPECIFICATION
//!
//! This documents the authoritative submit sequence and the checks around it,
//! and compares it against `handle_virtgpu_execbuffer` in `virtgpu.rs`. The
//! handler is not restructured onto this yet -- doing that safely requires the
//! ctx/ring-index/fence plumbing that `virtgpu_ctx` and `virtgpu_fence` will
//! bring. Writing the sequence down first is what makes the eventual move
//! mechanical rather than another re-derivation.

/// Translation of `virtio_gpu_execbuffer_ioctl` (virtgpu_submit.c:478).
///
/// The ordering below is load-bearing and is the part most worth copying
/// verbatim. Our handler performs a strict subset, in a different order.
///
/// ```c
/// int virtio_gpu_execbuffer_ioctl(struct drm_device *dev, void *data,
///                                 struct drm_file *file)
/// {
///         if (!vgdev->has_virgl_3d)
///                 return -ENOSYS;
///
///         if (exbuf->flags & ~VIRTGPU_EXECBUF_FLAGS)
///                 return ret;                       /* -EINVAL */
///
///         if (exbuf->flags & VIRTGPU_EXECBUF_RING_IDX) {
///                 if (exbuf->ring_idx >= vfpriv->num_rings)
///                         return ret;
///                 if (!vfpriv->base_fence_ctx)
///                         return ret;
///                 fence_ctx = vfpriv->base_fence_ctx;
///                 ring_idx  = exbuf->ring_idx;
///         }
///
///         virtio_gpu_create_context(dev, file);       /* idempotent */
///
///         ret = virtio_gpu_init_submit(&submit, exbuf, dev, file,
///                                      fence_ctx, ring_idx);
///         if (ret) goto cleanup;
///
///         ret = virtio_gpu_parse_post_deps(&submit);  if (ret) goto cleanup;
///         ret = virtio_gpu_parse_deps(&submit);       if (ret) goto cleanup;
///
///         /* in-fences awaited at the END of the job submission path, to
///          * optimise by proceeding directly to submission after the waits */
///         ret = virtio_gpu_wait_in_fence(&submit);   if (ret) goto cleanup;
///         ret = virtio_gpu_lock_buflist(&submit);    if (ret) goto cleanup;
///
///         virtio_gpu_submit(&submit);                 /* async: queue + notify */
///         virtio_gpu_install_out_fence_fd(&submit);
///         virtio_gpu_process_post_deps(&submit);
///         virtio_gpu_complete_submit(&submit);
/// cleanup:
///         if (ret && submit.out_fence && submit.out_fence->e) {
///                 drm_event_cancel_free(dev, &submit.out_fence->e->base);
///                 submit.out_fence->e = NULL;
///         }
///         virtio_gpu_cleanup_submit(&submit);
///         return ret;
/// }
/// ```
///
/// Six properties, in the order they matter:
///
/// **S1 — reject unknown flags before anything else.** `exbuf->flags &
/// ~VIRTGPU_EXECBUF_FLAGS` is an EINVAL *before* a context is created. We do
/// not perform this check at all, so an unknown flag is silently accepted and
/// the submit proceeds.
///
/// **S2 — `RING_IDX` is validated against `vfpriv->num_rings`** and requires a
/// per-file `base_fence_ctx`. Our context carries `num_rings` from
/// `VIRTGPU_CONTEXT_PARAM_NUM_RINGS` (a count, not a boolean), and the
/// handler rejects `ring_idx >= num_rings`. This was hardcoded to 1, so
/// Mesa's ring-1 fence submits returned EINVAL and surfaced as
/// `VK_ERROR_DEVICE_LOST`; Mesa declares `MAX_RINGS` (64), not 1 or 2.
///
/// **S3 — `virtio_gpu_create_context(dev, file)` is called on EVERY submit**,
/// and is idempotent. Not "create a context once at first use": every ioctl
/// goes through it, so a context torn down underneath us is recreated rather
/// than used dangling.
///
/// **S4 — a fence is allocated if ANY of four conditions holds** (from
/// `virtio_gpu_init_submit`, virtgpu_submit.c:392-398):
///
/// ```c
/// if ((exbuf->flags & VIRTGPU_EXECBUF_FENCE_FD_OUT) ||
///     exbuf->num_out_syncobjs ||
///     exbuf->num_bo_handles ||
///     drm_fence_event) {
///         out_fence = virtio_gpu_fence_alloc(vgdev, fence_ctx, ring_idx);
///         if (!out_fence) return -ENOMEM;
/// } else {
///         out_fence = NULL;
/// }
/// ```
///
/// `exbuf->num_bo_handles` alone is enough. We allocate a fence id only when
/// `FENCE_FD_OUT` is set, so a submit that carries bo handles but no out-fence
/// gets no host fence -- and, per S6, that removes the only completion signal
/// the client has.
///
/// **S5 — `drm_fence_event` is gated on `RING_IDX` *and* the ring being in
/// `ring_idx_mask`**, i.e. per-file capability, not a global. This is how a
/// client gets completion without an fd.
///
/// **S6 — the submit is ASYNCHRONOUS.** `virtio_gpu_submit` is
/// `virtio_gpu_cmd_submit(...)` followed by `virtio_gpu_notify(...)` and
/// returns; completion arrives later through the fence. Our `submit_3d` does a
/// blocking `push` + `wait` and returns only once the host has answered.
///
/// That difference is deliberate on our side and should stay: our control queue
/// is structurally depth-1 (`virtgpu_vq::D2`), so a synchronous submit is the
/// only safe way to use it. But it means the ioctl's latency includes the host's
/// render, and it is why the whole 3D path burns a core with interrupts masked
/// (`virtgpu_vq::D3`). Making this asynchronous is a prerequisite for any real
/// fix to either, and requires the vq work first.
pub fn execbuffer_sequence_doc() {}

/// Where Linux validates `VIRTGPU_EXECBUF_RING_IDX`, transcribed as a
/// predicate so the check can be unit-tested independently of a live device.
///
/// Returns `Err(())` for the two conditions Linux rejects with `-EINVAL`.
pub fn validate_ring_idx(
    flags: u32,
    ring_idx: u32,
    num_rings: u32,
    base_fence_ctx: u64,
) -> Result<u32, ()> {
    if flags & VIRTGPU_EXECBUF_RING_IDX == 0 {
        return Ok(0);
    }
    if ring_idx >= num_rings {
        return Err(());
    }
    if base_fence_ctx == 0 {
        return Err(());
    }
    Ok(ring_idx)
}

/// S4, transcribed: does this submit need a fence allocated?
pub fn needs_fence(
    flags: u32,
    num_out_syncobjs: u32,
    num_bo_handles: u32,
    drm_fence_event: bool,
) -> bool {
    flags & VIRTGPU_EXECBUF_FENCE_FD_OUT != 0
        || num_out_syncobjs != 0
        || num_bo_handles != 0
        || drm_fence_event
}

/// S1, transcribed: reject any bit outside the defined flag mask.
///
/// Linux: `if (exbuf->flags & ~VIRTGPU_EXECBUF_FLAGS) return -EINVAL;`
pub fn flags_are_known(flags: u32) -> bool {
    flags & !VIRTGPU_EXECBUF_FLAGS == 0
}

// These three used to be redeclared here as
//     RING_IDX = 1 << 1, FENCE_FD_OUT = 1 << 0, FLAGS = |(1 << 2)
// which is upstream's bit values *rotated by one*: upstream
// virtgpu_drm.h:52-54 defines FENCE_FD_IN 0x01, FENCE_FD_OUT 0x02,
// RING_IDX 0x04. This module did not `use drm_uapi`, so the shadowed copies
// were the ones actually used, and `needs_fence` was testing bit 0x01 --
// FENCE_FD_IN. A client that passed only an in-fence therefore had a fence
// allocated for it, and `release_fence` went on to `eventfd_signal_fd()` the
// very fd the client supplied as an *input* dependency. Upstream only ever
// reads that fd (virtgpu_submit.c:444-456, sync_file_get_fence), so the
// client's poll(POLLIN) on its in-fence returned ready for work that had not
// finished.
//
// Same class as the VIRTGPU_PARAM_BLOB_ALIGNMENT bug: a wire constant
// restated locally instead of imported, then drifting. One definition, in
// drm-uapi, pinned to the header.
use drm_uapi::{VIRTGPU_EXECBUF_FENCE_FD_OUT, VIRTGPU_EXECBUF_RING_IDX};

/// `VIRTGPU_EXECBUF_FLAGS` upstream (virtgpu_drm.h:55-58): the union of the
/// three defined flag bits. Upstream's macro text names FENCE_FD_OUT,
/// RING_IDX and FENCE_FD_IN -- the old comment here claimed a
/// VIRTGPU_EXECBUF_GPU_AUDIO that does not exist.
const VIRTGPU_EXECBUF_FLAGS: u32 =
    drm_uapi::VIRTGPU_EXECBUF_FENCE_FD_IN | VIRTGPU_EXECBUF_FENCE_FD_OUT | VIRTGPU_EXECBUF_RING_IDX;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_flags_are_rejected() {
        assert!(flags_are_known(0));
        assert!(flags_are_known(VIRTGPU_EXECBUF_FENCE_FD_OUT));
        assert!(flags_are_known(VIRTGPU_EXECBUF_RING_IDX));
        // A bit outside the defined mask must be refused (S1).
        assert!(!flags_are_known(1 << 5));
        assert!(!flags_are_known(0xffff_ffff));
    }

    #[test]
    fn ring_idx_needs_ring_and_fence_ctx() {
        // Not using RING_IDX: ring_idx is ignored entirely.
        assert_eq!(validate_ring_idx(0, 7, 1, 0), Ok(0));
        // RING_IDX set: ring must exist.
        assert_eq!(validate_ring_idx(VIRTGPU_EXECBUF_RING_IDX, 7, 1, 1), Err(()));
        assert_eq!(validate_ring_idx(VIRTGPU_EXECBUF_RING_IDX, 0, 1, 1), Ok(0));
        // ...and a per-file base fence context must exist.
        assert_eq!(validate_ring_idx(VIRTGPU_EXECBUF_RING_IDX, 0, 1, 0), Err(()));
    }

    #[test]
    fn bo_handles_alone_require_a_fence() {
        // S4: num_bo_handles is sufficient on its own. Our handler only
        // allocates when FENCE_FD_OUT is set, so this is the case that currently
        // gets no host fence.
        assert!(needs_fence(0, 0, 1, false));
        assert!(needs_fence(0, 0, 0, false) == false);
        assert!(needs_fence(VIRTGPU_EXECBUF_FENCE_FD_OUT, 0, 0, false));
        assert!(needs_fence(0, 1, 0, false));
        assert!(needs_fence(0, 0, 0, true));
    }
}