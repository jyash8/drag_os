//! Line-by-line translation of Linux `drivers/gpu/drm/virtio/virtgpu_ioctl.c`.
//!
//! Source: `/tmp/opencode/linuxfull/virtgpu_ioctl.c` (kernel master, 789 lines).
//!
//! ## Status: SPECIFICATION
//!
//! Ten ioctls. Six are transcribed below with their validation logic and tests.
//! The other four were listed here in `NOT_YET_PORTED` until it was checked, but
//! all four are in fact implemented and dispatched in `virtgpu.rs`
//! (`handle_virtgpu_map`, `handle_virtgpu_transfer_from_host`,
//! `handle_virtgpu_transfer_to_host`, `handle_virtgpu_context_init` -- see
//! `drm/mod.rs`). They were ported without being struck off the list, so the
//! manifest claimed four ioctls were missing that have been working the whole
//! time. Two real defects were found in the course of translating it, and both
//! are fixed in `virtgpu.rs`.
//!
//! `NOT_YET_PORTED` is kept because it is still the convention in the sibling
//! `virtgpu_*` modules -- but it is documentation only. Nothing asserts on its
//! contents, so an entry that has been implemented must be *removed*, or it
//! becomes a false claim about the kernel's own state.

use alloc::vec::Vec;

/// Empty: every ioctl in `virtgpu_ioctl.c` is now either transcribed here or
/// implemented in `virtgpu.rs`. See the module note above before adding an entry
/// -- verify the handler is absent first.
///
/// Several transcriptions in this module are still specification-only, reached
/// from `#[cfg(test)]` but not from a handler (`resource_create_validate`,
/// `wait_result`). That is legitimate for translation reference; the difference
/// from the four removed above is that those *claimed* to be unimplemented when
/// the kernel handled them.
pub const NOT_YET_PORTED: &[&str] = &[];

/// `virtio_gpu_getparam_ioctl` (virtgpu_ioctl.c:88-131).
///
/// ```c
/// switch (param->param) {
/// case VIRTGPU_PARAM_3D_FEATURES:   value = vgdev->has_virgl_3d ? 1 : 0; break;
/// case VIRTGPU_PARAM_CAPSET_QUERY_FIX: value = 1; break;
/// case VIRTGPU_PARAM_RESOURCE_BLOB: value = vgdev->has_resource_blob ? 1 : 0; break;
/// case VIRTGPU_PARAM_HOST_VISIBLE:  value = vgdev->has_host_visible ? 1 : 0; break;
/// case VIRTGPU_PARAM_CROSS_DEVICE:  value = vgdev->has_resource_assign_uuid ? 1 : 0; break;
/// case VIRTGPU_PARAM_CONTEXT_INIT: value = vgdev->has_context_init ? 1 : 0; break;
/// case VIRTGPU_PARAM_SUPPORTED_CAPSET_IDs: value = vgdev->capset_id_mask; break;
/// case VIRTGPU_PARAM_EXPLICIT_DEBUG_NAME:   value = vgdev->has_context_init ? 1 : 0; break;
/// case VIRTGPU_PARAM_BLOB_ALIGNMENT:
///         if (!vgdev->has_blob_alignment) return -ENOENT;
///         value = vgdev->blob_alignment; break;
/// default: return -EINVAL;
/// }
/// if (copy_to_user(u64_to_user_ptr(param->value), &value, sizeof(int)))
///         return -EFAULT;
/// ```
///
/// Three properties, all of which we got wrong. See `divergences`.
///
/// **I1 — every value comes from probed device state**, never a constant.
/// `has_virgl_3d`, `has_resource_blob`, `has_host_visible`, `has_context_init`
/// and `capset_id_mask` are all set during probe.
///
/// **I2 — `BLOB_ALIGNMENT` returns `-ENOENT` when unsupported.** Not 0, not
/// `-EINVAL`: a distinct code so a client can distinguish "this device has no
/// such feature" from "you asked for something invalid".
///
/// **I3 — unknown params are `-EINVAL`**, not 0.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Param {
    ThreeDFeatures { has_virgl: bool },
    CapsetQueryFix,
    ResourceBlob { supported: bool },
    HostVisible,
    CrossDevice,
    ContextInit { supported: bool },
    SupportedCapsetIds,
    ExplicitDebugName { supported: bool },
    BlobAlignment { supported: bool, alignment: u32 },
    Unknown,
}

/// The `switch`, transcribed.
pub fn getparam(p: Param) -> Result<u32, ParamError> {
    use Param::*;
    match p {
        // Upstream returns `vgdev->has_virgl_3d` etc., not a constant.
        // These variants carry the flag so the answer tracks what the device
        // actually negotiated.
        ThreeDFeatures { has_virgl } => Ok(u32::from(has_virgl)),
        CapsetQueryFix => Ok(1),      // always 1 upstream
        ResourceBlob { supported } => Ok(u32::from(supported)),
        HostVisible => Ok(1),         // has_host_visible
        CrossDevice => Ok(0),         // has_resource_assign_uuid
        ContextInit { supported } => Ok(u32::from(supported)),
        // Upstream returns `vgdev->capset_id_mask`, populated during probe.
        // Ours is a constant -- see divergences::I4 for why it must NOT be
        // "fixed" by probing without first determining which build
        // configuration populates that mask. VirGL|VirGL2|Venus = 0x16.
        SupportedCapsetIds => Ok(SUPPORTED_CAPSET_IDS_FROM_PROBE),
        ExplicitDebugName { supported } => Ok(u32::from(supported)),
        BlobAlignment { supported, alignment } => {
            if !supported {
                Err(ParamError::NoEntry)
            } else {
                Ok(alignment)
            }
        }
        // I3
        Unknown => Err(ParamError::Inval),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ParamError {
    /// `-EINVAL`: the param is not one we know.
    Inval,
    /// `-ENOENT`: the device does not have this feature. Distinct from `Inval`.
    NoEntry,
    /// `-EFAULT`: `copy_to_user` failed.
    Fault,
}

/// VirGL (bit 1) | VirGL2 (bit 2) | Venus (bit 4).
///
/// Upstream reads `vgdev->capset_id_mask`, which is populated during probe. See
/// `divergences::I4` -- we hardcode this.
pub const SUPPORTED_CAPSET_IDS_FROM_PROBE: u32 =
    (1 << 1) | (1 << 2) | (1 << 4);

/// `virtio_gpu_resource_create_ioctl` (virtgpu_ioctl.c:134-204).
///
/// The validation split is the whole point of this ioctl:
///
/// ```c
/// if (vgdev->has_virgl_3d) {
///         virtio_gpu_create_context(dev, file);
///         params.virgl = true;
///         params.target  = rc->target;
///         params.bind    = rc->bind;
///         params.depth   = rc->depth;
///         params.array_size = rc->array_size;
///         params.last_level = rc->last_level;
///         params.nr_samples = rc->nr_samples;
///         params.flags   = rc->flags;
/// } else {
///         if (rc->depth      > 1) return -EINVAL;
///         if (rc->nr_samples  > 1) return -EINVAL;
///         if (rc->last_level  > 1) return -EINVAL;
///         if (rc->target      != 2) return -EINVAL;
///         if (rc->array_size  > 1) return -EINVAL;
/// }
///
/// params.format = rc->format;
/// params.width  = rc->width;
/// params.height = rc->height;
/// params.size   = rc->size;
/// /* allocate a single page size object */
/// if (params.size == 0)
///         params.size = PAGE_SIZE;
/// ```
///
/// **I5 — the strict validation applies only on the 2D path.** With virgl, a
/// 3D texture legitimately has depth > 1, array_size > 1, mip levels and
/// multisampling, so those are all accepted. Without virgl they are all
/// rejected.
///
/// Also note `virtio_gpu_create_context(dev, file)` runs **here**, at resource
/// creation, not lazily at submit time.
///
/// **I6 — `rc->size == 0` becomes `PAGE_SIZE`**, so a zero-size request still
/// gets a real single-page object rather than a zero-length allocation.
pub fn resource_create_validate(
    has_virgl_3d: bool,
    depth: u32,
    nr_samples: u32,
    last_level: u32,
    target: u32,
    array_size: u32,
) -> Result<(), ()> {
    if has_virgl_3d {
        return Ok(());
    }
    // I5: 2D path only.
    if depth > 1 || nr_samples > 1 || last_level > 1 || target != 2 || array_size > 1 {
        return Err(());
    }
    Ok(())
}

/// `verify_blob` (virtgpu_ioctl.c:487-547), the per-mem-type rules.
///
/// **I8 — host3d blobs carry a command payload.** Mesa's virgl driver creates
/// its persistent/coherent upload buffers (`VIRGL_CAP_ARB_BUFFER_STORAGE`) as
/// `HOST3D` blobs and passes the virgl `PIPE_RESOURCE_CREATE` command in
/// `cmd`/`cmd_size` (`virgl_drm_winsys.c`, `virgl_drm_winsys_resource_create_blob`).
/// That payload is *not* part of the CREATE_BLOB wire command: upstream submits
/// it as an ordinary `SUBMIT_3D` on the context *before* creating the blob
/// (virtgpu_ioctl.c:571-582). Rejecting it outright starves `u_upload`, so the
/// compositor's draws are dropped and the render is black. Only the
/// dword-alignment is checked here; the caller submits the payload.
///
/// Guest blobs are the opposite: no command and no blob_id.
pub fn blob_create_validate(blob_mem: u32, blob_id: u64, cmd_size: u32) -> Result<(), ()> {
    use drm_uapi::{
        VIRTGPU_BLOB_MEM_GUEST, VIRTGPU_BLOB_MEM_HOST3D, VIRTGPU_BLOB_MEM_HOST3D_GUEST,
    };
    if blob_mem == VIRTGPU_BLOB_MEM_GUEST {
        if blob_id != 0 || cmd_size != 0 {
            return Err(());
        }
    } else if blob_mem == VIRTGPU_BLOB_MEM_HOST3D || blob_mem == VIRTGPU_BLOB_MEM_HOST3D_GUEST {
        if cmd_size % 4 != 0 {
            return Err(());
        }
    } else {
        return Err(());
    }
    Ok(())
}

/// `virtio_gpu_wait_ioctl` (virtgpu_ioctl.c:389-415).
///
/// ```c
/// long timeout = 15 * HZ;
/// obj = drm_gem_object_lookup(file, args->handle);
/// if (obj == NULL) return -ENOENT;
///
/// if (args->flags & VIRTGPU_WAIT_NOWAIT)
///         ret = dma_resv_test_signaled(obj->resv, DMA_RESV_USAGE_READ);
/// else
///         ret = dma_resv_wait_timeout(obj->resv, DMA_RESV_USAGE_READ,
///                                     true, timeout);
/// if (ret == 0)       ret = -EBUSY;
/// else if (ret > 0)   ret = 0;
/// ```
///
/// **I7 — `-EBUSY` means "still not signalled", not "error".** `dma_resv_wait_timeout`
/// returns 0 on timeout, and that is translated into `-EBUSY`. A client is
/// expected to treat `-EBUSY` as "retry", so it must be distinguishable from a
/// genuine failure.
///
/// The default timeout is 15 seconds, not the 50 ms used on the plane-update
/// fence wait (`virtgpu_plane.c:231`) — different waits, different budgets.
pub fn wait_result(dma_resv_ret: i64) -> i64 {
    match dma_resv_ret {
        0 => -EBUSY,
        r if r > 0 => 0,
        r => r,
    }
}

/// `-EBUSY`, the value `wait_result` returns for a timeout.
pub const EBUSY: i64 = -16;
/// `-ENOENT`
pub const ENOENT: i64 = -2;
/// `-EINVAL`
pub const EINVAL: i64 = -22;

/// Divergences found while translating `virtgpu_ioctl.c`.
pub mod divergences {
    /// I3 — unknown `getparam` returned **0**, not `-EINVAL`.
    ///
    /// Linux: `default: return -EINVAL;`. We fell through the match to
    /// `result = 0` and returned success. A client probing for an optional
    /// feature would read "present with value 0" instead of "absent", and
    /// `VIRTGPU_PARAM_BLOB_ALIGNMENT` on a device without it should be `-ENOENT`
    /// — a code we never emitted at all.
    ///
    /// **Impact: a client could believe a feature exists.** Real, and it is the
    /// same shape of error as the DIRTYFB flag collision: "looks like success,
    /// means something else".
    pub const I3_UNKNOWN_PARAM_RETURNS_ZERO: &str =
        "unknown getparam returned 0 success instead of -EINVAL; BLOB_ALIGNMENT never returned -ENOENT";

    /// I1 — `RESOURCE_BLOB`, `HOST_VISIBLE`, `CONTEXT_INIT` and `3D_FEATURES`
    /// are hardcoded rather than probed.
    ///
    /// Upstream reads `vgdev->has_resource_blob`, `has_host_visible`,
    /// `has_context_init`, `has_virgl_3d`, all set during probe. We return
    /// literal `1`. If a configuration lacked one of these, we would advertise
    /// it anyway.
    ///
    /// **Impact: latent.** All four happen to be true for this device
    /// configuration, so the answer is right by coincidence rather than by
    /// measurement. Worth deriving from the same probe data `GET_DISPLAY_INFO`
    /// should be supplying.
    pub const I1_PARAMS_NOT_PROBED: &str =
        "3D_FEATURES/RESOURCE_BLOB/HOST_VISIBLE/CONTEXT_INIT hardcoded to 1, not probed";

    /// I4 — `SUPPORTED_CAPSET_IDS` hardcoded to `virgl|virgl2|venus`.
    ///
    /// Upstream returns `vgdev->capset_id_mask`, populated during probe. We
    /// compute the same three bits as a constant.
    ///
    /// **Do not "fix" this by probing without checking which build
    /// configuration populates the mask.** In the kernel master file set,
    /// `capset_id_mask` is declared (`virtgpu_drv.h:306`), read
    /// (`virtgpu_ioctl.c:115`) and used as a reject gate
    /// (`virtgpu_ioctl.c:673`) but **assigned nowhere**. So it is populated
    /// under a configuration not present in these sources, and the hardcoded
    /// value may be exactly what upstream builds without virgl. See §40.
    pub const I4_CAPSET_IDS_HARDCODED: &str =
        "SUPPORTED_CAPSET_IDS hardcoded; upstream reads a probe-populated capset_id_mask";

    /// I5 — the 2D resource-create validation is absent.
    ///
    /// Linux rejects `depth > 1`, `nr_samples > 1`, `last_level > 1`,
    /// `target != 2` and `array_size > 1` on the non-virgl path. We perform
    /// none of these checks.
    ///
    /// **Impact: probably unreachable for us** — we always take the virgl
    /// branch, since `handle_resource_create` always issues
    /// `RESOURCE_CREATE_3D`. Recorded so that if a 2D resource-create path is
    /// ever added, the validation comes with it rather than after.
    pub const I5_NO_2D_CREATE_VALIDATION: &str =
        "non-virgl resource_create validation (depth/nr_samples/last_level/target/array_size) absent";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_param_is_einval_not_zero() {
        // I3: this used to return Ok(0) success.
        assert_eq!(getparam(Param::Unknown), Err(ParamError::Inval));
    }

    #[test]
    fn blob_alignment_absent_is_enoent_distinct_from_einval() {
        // I2: two different failures, two different codes.
        assert_eq!(
            getparam(Param::BlobAlignment { supported: false, alignment: 0 }),
            Err(ParamError::NoEntry)
        );
        assert_eq!(
            getparam(Param::BlobAlignment { supported: true, alignment: 256 }),
            Ok(256)
        );
        assert_ne!(ParamError::NoEntry as i32, ParamError::Inval as i32);
    }

    #[test]
    fn debug_name_mirrors_context_init() {
        // Upstream: both are `has_context_init ? 1 : 0`.
        assert_eq!(
            getparam(Param::ExplicitDebugName { supported: true }),
            getparam(Param::ContextInit { supported: true })
        );
    }

    #[test]
    fn virgl_accepts_3d_shaped_requests() {
        // I5: depth/array/mips/msaa are all legal for a virgl texture.
        assert_eq!(
            resource_create_validate(true, 2, 4, 3, 7, 6),
            Ok(())
        );
    }

    #[test]
    fn non_virgl_rejects_3d_shaped_requests() {
        for (d, s, l, t, a) in [
            (2, 1, 1, 2, 1),
            (1, 4, 1, 2, 1),
            (1, 1, 3, 2, 1),
            (1, 1, 1, 5, 1),
            (1, 1, 1, 2, 6),
        ] {
            assert!(
                resource_create_validate(false, d, s, l, t, a).is_err(),
                "({},{},{},{},{}) must be rejected on the 2D path",
                d, s, l, t, a
            );
        }
        // The single legal 2D shape.
        assert_eq!(resource_create_validate(false, 1, 1, 1, 2, 1), Ok(()));
    }

    #[test]
    fn wait_timeout_is_ebusy_not_an_error() {
        // I7: dma_resv_wait_timeout returns 0 on timeout -> -EBUSY, so the
        // client can tell "retry" from "failed".
        assert_eq!(wait_result(0), -EBUSY);
        assert_eq!(wait_result(1), 0);
        assert_eq!(wait_result(42), 0);
        // A genuine error passes through unchanged.
        assert_eq!(wait_result(-EINVAL), -EINVAL);
    }

    #[test]
    fn host3d_blob_allows_a_dword_aligned_command() {
        // I8: Mesa's virgl upload buffers arrive as HOST3D blobs carrying the
        // 48-byte PIPE_RESOURCE_CREATE command; both host3d mem types accept it.
        assert_eq!(blob_create_validate(0x0002, 7, 48), Ok(()));
        assert_eq!(blob_create_validate(0x0003, 7, 0), Ok(()));
        assert_eq!(blob_create_validate(0x0002, 7, 0), Ok(()));
    }

    #[test]
    fn host3d_blob_rejects_unaligned_command() {
        assert!(blob_create_validate(0x0002, 7, 46).is_err());
        assert!(blob_create_validate(0x0003, 7, 45).is_err());
    }

    #[test]
    fn guest_blob_forbids_command_and_blob_id() {
        assert_eq!(blob_create_validate(0x0001, 0, 0), Ok(()));
        assert!(blob_create_validate(0x0001, 0, 48).is_err());
        assert!(blob_create_validate(0x0001, 9, 0).is_err());
    }

    #[test]
    fn unknown_blob_mem_is_rejected() {
        assert!(blob_create_validate(0x9999, 0, 0).is_err());
    }
}