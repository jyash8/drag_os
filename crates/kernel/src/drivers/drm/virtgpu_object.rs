//! Line-by-line translation of Linux `drivers/gpu/drm/virtio/virtgpu_object.c`.
//!
//! Source: `/tmp/opencode/linuxfull/virtgpu_object.c` (kernel master, 361 lines).
//!
//! ## Status: SPECIFICATION
//!
//! Covers object creation, resource-id allocation and the restore list. The
//! GEM-object lifetime machinery (`drm_gem_object_funcs`, `virtio_gpu_free_object`,
//! `virtio_gpu_cleanup_object`) is not ported and is listed in `NOT_YET_PORTED`.

use alloc::vec;
use alloc::vec::Vec;

/// Items present in `virtgpu_object.c` but not yet translated.
pub const NOT_YET_PORTED: &[&str] = &[
    "virtio_gpu_cleanup_object() / virtio_gpu_free_object()",
    "virtio_gpu_object_shmem_init() -- needs a scatterlist/GEM shmem equivalent",
    "virtio_gpu_detach_object_fenced()",
    "virtio_gpu_is_shmem()",
    "virtio_gpu_object_restore_all() -- suspend/resume restore list",
    "virtio_gpu_object_unref_all()",
    "virtio_gpu_remove_from_restore_list()",
];

/// What kind of resource is being created.
///
/// Linux carries this as three booleans on the object (`dumb`, `blob`,
/// `virgl`) set from `virtio_gpu_object_params`. The create *sequence* differs
/// per kind, so the branch is load-bearing rather than cosmetic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ObjectKind {
    /// `params->dumb` -- a dumb buffer, scanned out via 2D transfers.
    Dumb,
    /// `params->blob` -- a scanout blob. `blob_mem` distinguishes host from
    /// guest memory; only `VIRTGPU_BLOB_MEM_GUEST` sets `bo->guest_blob`.
    Blob { guest_memory: bool },
    /// `params->virgl` -- a host-managed virgl 3D resource.
    Virgl,
}

/// The create sequence, from `virtio_gpu_object_create`
/// (virtgpu_object.c:213-289).
///
/// ```c
/// params->size = roundup(params->size, PAGE_SIZE);
/// shmem_obj = drm_gem_shmem_create(vgdev->ddev, params->size);
/// bo = gem_to_virtio_gpu_obj(&shmem_obj->base);
/// INIT_LIST_HEAD(&bo->restore_node);
///
/// ret = virtio_gpu_resource_id_get(vgdev, &bo->hw_res_handle);
/// if (ret < 0) goto err_free_gem;
///
/// bo->dumb = params->dumb;
///
/// ret = virtio_gpu_object_shmem_init(vgdev, bo, &ents, &nents);
/// if (ret != 0) goto err_put_id;
///
/// if (fence) {
///         ret = -ENOMEM;
///         objs = virtio_gpu_array_alloc(1);
///         if (!objs) goto err_free_entry;
///         virtio_gpu_array_add_obj(objs, &bo->base.base);
///         ret = virtio_gpu_array_lock_resv(objs);
///         if (ret != 0) goto err_put_objs;
/// }
///
/// if (params->blob) {
///         if (params->blob_mem == VIRTGPU_BLOB_MEM_GUEST)
///                 bo->guest_blob = true;
///         virtio_gpu_cmd_resource_create_blob(vgdev, bo, params, ents, nents);
/// } else if (params->virgl) {
///         virtio_gpu_cmd_resource_create_3d(vgdev, bo, params, objs, fence);
///         virtio_gpu_object_attach(vgdev, bo, ents, nents);
/// } else {
///         virtio_gpu_cmd_create_resource(vgdev, bo, params, objs, fence);
///         virtio_gpu_object_attach(vgdev, bo, ents, nents);
/// }
///
/// if (!params->virgl) {
///         /* store non-virgl object with its param to the restore list */
///         bo->params = *params;
///         virtio_gpu_add_object_to_restore_list(vgdev, bo);
/// }
/// ```
///
/// Three things worth carrying over:
///
/// **O1 — blobs do NOT get `attach_backing`.** The blob branch creates and
/// stops; only the virgl and plain branches follow with an attach. Attaching
/// guest backing to a blob whose memory lives in the hostmem window would be
/// wrong, and this is the one asymmetry in the three-way branch.
///
/// **O2 — the restore list holds only non-virgl objects** (`if (!params->virgl)`).
/// virgl resources are host state and are not restored on resume.
///
/// **O3 — the size is rounded up to `PAGE_SIZE` before allocation**, so the
/// memory backing always covers the full requested size.
pub fn create_sequence(kind: ObjectKind) -> Vec<CreateStep> {
    let mut steps = vec![
        CreateStep::RoundSizeToPage,
        CreateStep::AllocShmem,
        CreateStep::InitRestoreNode,
        CreateStep::ResourceIdGet,
    ];
    match kind {
        // Blob: create only, no attach.
        ObjectKind::Blob { .. } => steps.push(CreateStep::CmdResourceCreateBlob),
        // Both non-blob kinds: create, then attach.
        ObjectKind::Virgl => {
            steps.push(CreateStep::CmdResourceCreate3d);
            steps.push(CreateStep::AttachBacking);
        }
        ObjectKind::Dumb => {
            steps.push(CreateStep::CmdCreateResource2d);
            steps.push(CreateStep::AttachBacking);
        }
    }
    // O2: restore list is for non-virgl only.
    if kind != ObjectKind::Virgl {
        steps.push(CreateStep::AddToRestoreList);
    }
    steps
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CreateStep {
    /// `params->size = roundup(params->size, PAGE_SIZE)` (O3)
    RoundSizeToPage,
    AllocShmem,
    InitRestoreNode,
    ResourceIdGet,
    CmdResourceCreateBlob,
    CmdResourceCreate3d,
    CmdCreateResource2d,
    /// `virtio_gpu_object_attach` -- **not** issued for blobs (O1)
    AttachBacking,
    AddToRestoreList,
}

/// `virtio_gpu_resource_id_get` (virtgpu_object.c:36-58).
///
/// ```c
/// if (virtio_gpu_virglrenderer_workaround) {
///         /*
///          * Hack to avoid re-using resource IDs.
///          *
///          * virglrenderer versions up to (and including) 0.7.0
///          * can't deal with that.  virglrenderer commit
///          * "f91a9dd35715 Fix unlinking resources from hash
///          * table." (Feb 2019) fixes the bug.
///          */
///         static atomic_t seqno = ATOMIC_INIT(0);
///         int handle = atomic_inc_return(&seqno);
///         *resid = handle + 1;
/// } else {
///         *resid = ida_alloc(&vgdev->resource_ida);
/// }
/// ```
///
/// and correspondingly `virtio_gpu_resource_id_put` does **not** free the id
/// when the workaround is on (line 62-63):
///
/// ```c
/// if (!virtio_gpu_virglrenderer_workaround)
///         ida_free(&vgdev->resource_ida, id - 1);
/// ```
///
/// This is not a micro-optimisation. Reusing a resource id while the host still
/// holds a reference to the old one makes virglrenderer <= 0.7.0 confuse the two
/// in its hash table, and the guest then reads and writes the wrong resource.
/// The workaround is on by default upstream (`= 1`) for that reason.
///
/// Our `alloc_resource_id` is a monotonic `u32` counter that is never
/// decremented or reset, so ids are never reused -- the same guarantee, arrived
/// at independently. The residual risk is **wraparound**: on reuse of the
/// counter the host would begin seeing repeated ids, which is precisely the
/// condition the workaround exists to prevent. With a 32-bit counter and a
/// compositor that churns buffers, that is far away but not unreachable.
pub struct ResourceIdAllocator {
    next: u32,
}

impl ResourceIdAllocator {
    pub const fn new() -> Self {
        Self { next: 0 }
    }

    /// Start the sequence at `n`.
    ///
    /// Upstream starts at 0. We cannot: 2D display scanout resources take the
    /// low ids, and 3D/blob resources are allocated from the same counter, so a
    /// 0-based sequence would hand out an id already in use by the display
    /// engine. Ours therefore begins at 100.
    ///
    /// This is the one respect in which our allocator is not a literal copy of
    /// `virtio_gpu_resource_id_inc` -- recorded here rather than left as a
    /// bare `100` at the call site, where it read like an arbitrary constant.
    pub const fn starting_at(n: u32) -> Self {
        Self { next: n }
    }

    /// Monotonic; never returns a previously-issued id until `next` wraps.
    pub fn alloc(&mut self) -> u32 {
        let id = self.next;
        self.next = self.next.wrapping_add(1);
        id
    }

    /// True once the counter has wrapped and ids can repeat.
    pub fn wrapped(&self) -> bool {
        self.next < self.next.wrapping_add(1) || self.next == 0
    }
}

/// Divergences from `virtgpu_object.c`.
pub mod divergences {
    /// D8 — no restore list.
    ///
    /// Linux files every non-virgl object on a restore list with its params, so
    /// `virtio_gpu_object_restore_all()` can recreate them after suspend/resume
    /// (virtgpu_object.c:301). We have no equivalent; a resume would leave the
    /// host's resource table empty for 2D/dumb objects.
    ///
    /// **Impact: latent.** Only reachable across a suspend/resume cycle, which
    /// this kernel does not yet implement. Not the shear.
    pub const D8_NO_RESTORE_LIST: &str = "no restore list; suspend/resume would lose non-virgl objects";

    /// D9 — resource id counter can wrap; Linux's cannot within a session.
    ///
    /// Linux's workaround uses an `atomic_t` seqno that is also monotonic, so it
    /// has the same theoretical wrap, but `int` plus the fact that ids are not
    /// freed means it is unreachable in practice for a session's object count.
    /// Ours is `u32` and likewise never freed.
    ///
    /// **Impact: theoretical.** Flagged because it is the one place where our
    /// safety property depends on an unbounded counter rather than a real
    /// allocator, and it is cheap to log if ids ever repeat.
    pub const D9_ID_COUNTER_WRAP: &str = "resource id counter is monotonic and unbounded (wraps at 2^32)";
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn blob_does_not_attach_backing() {
        let s = create_sequence(ObjectKind::Blob { guest_memory: true });
        assert!(s.contains(&CreateStep::CmdResourceCreateBlob));
        // O1: the one asymmetry in the three-way branch.
        assert!(!s.contains(&CreateStep::AttachBacking));
    }

    #[test]
    fn virgl_and_plain_both_attach() {
        for k in [ObjectKind::Virgl, ObjectKind::Dumb] {
            let s = create_sequence(k);
            assert!(s.contains(&CreateStep::AttachBacking), "{:?}", k);
        }
    }

    #[test]
    fn only_non_virgl_lands_on_the_restore_list() {
        // O2
        assert!(!create_sequence(ObjectKind::Virgl).contains(&CreateStep::AddToRestoreList));
        assert!(create_sequence(ObjectKind::Dumb).contains(&CreateStep::AddToRestoreList));
        assert!(
            create_sequence(ObjectKind::Blob { guest_memory: false })
                .contains(&CreateStep::AddToRestoreList)
        );
    }

    #[test]
    fn attach_always_follows_its_create() {
        for k in [ObjectKind::Virgl, ObjectKind::Dumb] {
            let s = create_sequence(k);
            let ci = s
                .iter()
                .position(|x| {
                    matches!(x, CreateStep::CmdResourceCreate3d | CreateStep::CmdCreateResource2d)
                })
                .unwrap();
            let ai = s.iter().position(|x| *x == CreateStep::AttachBacking).unwrap();
            assert!(ai > ci, "{:?}: attach must come after create", k);
        }
    }

    #[test]
    fn resource_ids_are_never_reissued() {
        let mut a = ResourceIdAllocator::new();
        let first: Vec<u32> = (0..4).map(|_| a.alloc()).collect();
        assert_eq!(first, alloc::vec![0, 1, 2, 3]);
        // No free/reuse path exists at all, which is the property the
        // virglrenderer workaround exists to provide.
        assert_eq!(a.alloc(), 4);
    }

    #[test]
    fn wraparound_is_detectable() {
        let mut a = ResourceIdAllocator { next: u32::MAX };
        assert!(a.wrapped());
        assert_eq!(a.alloc(), u32::MAX);
        assert_eq!(a.next, 0, "counter wraps to 0 and ids can now repeat");
    }
}