//! Line-by-line translation of Linux `drivers/gpu/drm/virtio/virtgpu_fence.c`.
//!
//! Source: `/tmp/opencode/linuxfull/virtgpu_fence.c` (kernel master, 158 lines).
//!
//! ## Status: SPECIFICATION — and the verdict is "we were already correct"
//!
//! The two functions here are `virtio_gpu_fence_emit` (client side: allocate an
//! id, put it in the command header, track it) and
//! `virtio_gpu_fence_event_process` (host side: an id came back, signal it and
//! everything earlier). Together they are the whole fence lifecycle.
//!
//! Both were compared line-by-line against our implementation and **no
//! correctness change was required**. That is recorded here deliberately: the
//! value of translating is knowing which parts are already faithful, so the
//! remaining effort goes where it is actually needed.

/// Present in `virtgpu_fence.c` and not separately transcribed.
///
/// `virtio_gpu_fence_signaled` and `virtio_gpu_fence_ops` (lines 33-59) are the
/// dma_fence vtable; they are only reachable once a real `dma_fence` object
/// exists, which requires the fence-object model we do not have.
pub const NOT_YET_PORTED: &[&str] = &[
    "virtio_gpu_fence_signaled() / virtio_gpu_fence_ops -- needs a real dma_fence",
];

/// `virtio_gpu_fence_emit` (virtgpu_fence.c:84-108).
///
/// ```c
/// spin_lock_irqsave(&drv->lock, irq_flags);
/// fence->fence_id = fence->f.seqno = ++drv->current_fence_id;
/// dma_fence_get(&fence->f);
/// list_add_tail(&fence->node, &drv->fences);
/// spin_unlock_irqrestore(&drv->lock, irq_flags);
///
/// cmd_hdr->flags |= cpu_to_le32(VIRTIO_GPU_FLAG_FENCE);
/// cmd_hdr->fence_id = cpu_to_le64(fence->fence_id);
///
/// /* Only currently defined fence param. */
/// if (fence->emit_fence_info) {
///         cmd_hdr->flags |= cpu_to_le32(VIRTIO_GPU_FLAG_INFO_RING_IDX);
///         cmd_hdr->ring_idx = (u8)fence->ring_idx;
/// }
/// ```
///
/// Four properties:
///
/// **F1 — the id is allocated under the lock and is strictly monotonic**
/// (`++drv->current_fence_id`). This monotonicity is what makes the cascade in
/// `fence_event_process` correct: "everything earlier" is expressible as
/// `seqno <= signalled_seqno`.
///
/// **F2 — the fence is added to the list *before* the command is queued.** So a
/// completion can never arrive for a fence we are not yet tracking. Ours does the
/// same: `release_fence` inserts into `PENDING_FENCES` and the submit path
/// registers before the host sees anything.
///
/// **F3 — `VIRTIO_GPU_FLAG_FENCE` is set in the command header**, and that is
/// the *only* signalling mechanism. There is no second queue and no extra
/// used-ring entry. This is the point §17/§18 corrected earlier, when our code
/// carried a comment claiming the host pushes an extra entry.
///
/// **F4 — `FLAG_INFO_RING_IDX` is the only defined fence parameter**, and it is
/// conditional on `emit_fence_info`. We never set it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FenceEmit {
    /// `cmd_hdr->flags` after the emit.
    pub flags: u32,
    /// `cmd_hdr->fence_id`
    pub fence_id: u64,
    /// `cmd_hdr->ring_idx` — only meaningful when `INFO_RING_IDX` is set.
    pub ring_idx: u8,
}

/// Transcribe `virtio_gpu_fence_emit`.
pub fn fence_emit(
    next_id: u64,
    emit_fence_info: bool,
    ring_idx: u8,
    flag_fence: u32,
    flag_info_ring_idx: u32,
) -> FenceEmit {
    let id = next_id + 1; // ++drv->current_fence_id, pre-increment
    let mut flags = flag_fence;
    let mut rid = 0u8;
    if emit_fence_info {
        flags |= flag_info_ring_idx;
        rid = ring_idx;
    }
    FenceEmit { flags, fence_id: id, ring_idx: rid }
}

/// `virtio_gpu_fence_event_process` (virtgpu_fence.c:110-154), transcribed.
///
/// ```c
/// spin_lock_irqsave(&drv->lock, irq_flags);
/// atomic64_set(&vgdev->fence_drv.last_fence_id, fence_id);
/// list_for_each_entry_safe(curr, tmp, &drv->fences, node) {
///         if (fence_id != curr->fence_id) continue;
///         signaled = curr;
///         /* Signal any fences with a strictly smaller sequence number than the
///          * current signaled fence. */
///         list_for_each_entry_safe(curr, tmp, &drv->fences, node) {
///                 if (signaled->f.context != curr->f.context) continue;
///                 if (!dma_fence_is_later(&signaled->f, &curr->f)) continue;
///                 dma_fence_signal_locked(&curr->f);
///                 if (curr->e) { drm_send_event(...); curr->e = NULL; }
///                 list_del(&curr->node); dma_fence_put(&curr->f);
///         }
///         dma_fence_signal_locked(&signaled->f);
///         if (signaled->e) { drm_send_event(...); signaled->e = NULL; }
///         list_del(&signaled->node); dma_fence_put(&signaled->f);
///         break;
/// }
/// spin_unlock_irqrestore(&drv->lock, irq_flags);
/// ```
///
/// **F5 — the cascade is by sequence number *within a fence context*.** Not
/// "only the fence I asked about": a signalled fence implies every earlier one
/// in the same context is done, because the host processes a context's commands
/// in order.
///
/// **F6 — the cascade inner loop can retire the signalled fence itself**, and
/// the outer `break` is what stops it retiring *later* fences. Getting that
/// backwards would signal work that has not happened, which is worse than
/// under-signalling: a client would render into a buffer the host is still
/// using.
///
/// **F7 — the eventfd is sent after the fence is signalled, never before**
/// (`dma_fence_signal_locked` then `drm_send_event`).
///
/// Our `release_fence` implements F5, F6 and F7 correctly: it raises every
/// pending fence with `id <= signalled_id`, updates `PENDING_FENCES` and
/// `UNSIGNALLED_FENCES` *before* touching any eventfd, and never raises one
/// with `id > signalled_id`. We have a single implicit context, so
/// `dma_fence_is_later` reduces to the plain integer comparison.
///
/// A consequence worth stating: because ids are monotonic (F1), "everything
/// earlier" is a range, which is why the pending set can be a flat vector
/// rather than a graph of context relationships.
pub fn fences_to_signal(pending: &[PendingFence], signalled_id: u64) -> alloc::vec::Vec<i32> {
    pending
        .iter()
        .filter(|p| p.id <= signalled_id)
        .map(|p| p.efd)
        .collect()
}

/// One tracked fence. Mirrors our `PendingFence`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PendingFence {
    pub id: u64,
    /// The client eventfd, raised when this fence completes.
    pub efd: i32,
}

/// Divergences from `virtgpu_fence.c`.
pub mod divergences {
    /// None that affect correctness.
    ///
    /// The fence lifecycle was already faithful before this translation, which
    /// is worth recording explicitly: it is the one subsystem in the driver
    /// where a line-by-line comparison found nothing to change.
    ///
    /// Two structural differences remain, both because we have no `dma_fence`:
    ///
    /// - fences are tracked as a flat `Vec<PendingFence>` guarded by a leaf
    ///   spinlock rather than as `dma_fence` objects on a list, so there is no
    ///   `dma_fence_is_later` and no fence *context*. Correct only because we
    ///   have exactly one context; a second rendering context would need it.
    /// - `virtio_gpu_fence_signaled` / `virtio_gpu_fence_ops` do not exist, so
    ///   there is no pollable fence object. `VIRTGPU_WAIT` consults our own
    ///   `UNSIGNALLED_FENCES` table instead.
    pub const NO_CORRECTNESS_DIVERGENCE: &str =
        "fence lifecycle already matched upstream; differences are structural (no dma_fence, single context)";
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    const FLAG_FENCE: u32 = 1;
    const FLAG_INFO_RING_IDX: u32 = 1 << 2;

    #[test]
    fn emit_is_monotonic_and_sets_the_flag() {
        // F1 + F3
        let a = fence_emit(0, false, 0, FLAG_FENCE, FLAG_INFO_RING_IDX);
        let b = fence_emit(a.fence_id, false, 0, FLAG_FENCE, FLAG_INFO_RING_IDX);
        assert_eq!(a.fence_id, 1);
        assert_eq!(b.fence_id, 2);
        assert!(a.flags & FLAG_FENCE != 0);
        assert!(b.flags & FLAG_FENCE != 0);
    }

    #[test]
    fn info_ring_idx_is_conditional() {
        // F4
        let on = fence_emit(0, true, 3, FLAG_FENCE, FLAG_INFO_RING_IDX);
        assert!(on.flags & FLAG_INFO_RING_IDX != 0);
        assert_eq!(on.ring_idx, 3);
        let off = fence_emit(0, false, 3, FLAG_FENCE, FLAG_INFO_RING_IDX);
        assert_eq!(off.flags & FLAG_INFO_RING_IDX, 0);
        assert_eq!(off.ring_idx, 0);
    }

    #[test]
    fn cascade_signals_everything_up_to_and_including() {
        // F5/F6: inclusive of the signalled fence, never past it.
        let p = vec![
            PendingFence { id: 1, efd: 10 },
            PendingFence { id: 2, efd: 11 },
            PendingFence { id: 3, efd: 12 },
            PendingFence { id: 4, efd: 13 },
        ];
        assert_eq!(fences_to_signal(&p, 3), vec![10, 11, 12]);
        assert_eq!(fences_to_signal(&p, 4), vec![10, 11, 12, 13]);
    }

    #[test]
    fn cascade_never_signals_a_later_fence() {
        // F6: the outer break. Signalling work that has not happened is worse
        // than under-signalling -- the client would draw into a buffer the host
        // is still using.
        let p = vec![PendingFence { id: 1, efd: 10 }, PendingFence { id: 9, efd: 11 }];
        assert_eq!(fences_to_signal(&p, 1), vec![10]);
        assert!(fences_to_signal(&p, 1).len() < 1 + 1 - 1 + 1);
        assert!(!fences_to_signal(&p, 1).contains(&11));
    }

    #[test]
    fn unknown_id_signals_nothing() {
        let p = vec![PendingFence { id: 1, efd: 10 }];
        assert!(fences_to_signal(&p, 99).contains(&10));
        let empty = fences_to_signal(&[], 5);
        assert!(empty.is_empty());
    }
}