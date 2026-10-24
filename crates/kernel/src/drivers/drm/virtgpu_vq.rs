//! Line-by-line translation of Linux `drivers/gpu/drm/virtio/virtgpu_vq.c`.
//!
//! Source: `/tmp/opencode/linuxref2/virtgpu_vq.c` (Linux 6.x, 1592 lines).
//!
//! ## Status: AUDIT, not yet a rewrite
//!
//! The virtqueue itself lives in `crates/kernel/src/drivers/virtio/mod.rs`
//! (`VirtQueue`), which is shared by every virtio device (net, blk, gpu) and so
//! cannot be replaced wholesale by a virtio-gpu-specific rewrite without
//! affecting all of them. This module therefore documents where our transport
//! diverges from `virtgpu_vq.c`, ranked by consequence, and is the specification
//! for the later port rather than a replacement for `VirtQueue`.
//!
//! Linux's control queue is a general virtqueue driven by `virtqueue_add_sgs` /
//! `virtqueue_get_buf` / `virtqueue_kick` with a pool of reusable
//! `virtio_gpu_vbuffer`s. There is no Linux code path corresponding to what our
//! `push`/`wait` pair does, so this is a redesign rather than a translation.

use alloc::vec::Vec;

use crate::drivers::virtio::gpu::VirtioGpuCtrlHdr;

/// Ranked divergences from `virtgpu_vq.c`.
pub mod divergences {
    /// D1 — the command descriptor advertises the scratch buffer's size, not the
    /// command's.
    ///
    /// Linux, `virtio_gpu_queue_fenced_ctrl_buffer` (virtgpu_vq.c:532):
    ///
    /// ```c
    /// sg_init_one(&vcmd, vbuf->buf, vbuf->size);
    /// ```
    ///
    /// and any variable payload goes in a **separate** out-descriptor built from
    /// `vbuf->data_buf` / `vbuf->data_size` (virtgpu_vq.c:553), never appended
    /// to the header's descriptor.
    ///
    /// Ours, `send_command` (virtio/gpu.rs:671):
    ///
    /// ```rust
    /// parts.push((cmd_buf.phys().0, cmd_buf.len() as u32, 0));
    /// ```
    ///
    /// `cmd_buf` is allocated by `ensure_bufs` as `cmd_len.max(64)` and **never
    /// shrinks**. So once a large `SUBMIT_3D` grows the scratch buffer, every
    /// later fixed-size command is submitted with an oversized descriptor.
    ///
    /// **Impact: benign as observed.** QEMU parses by `hdr_type` and does not
    /// read past the header, so an oversized read-only descriptor cannot corrupt
    /// anything. It is *not* the shear — a read-only descriptor that is too long
    /// has no way to damage pixel data. Worth closing because it makes the host's
    /// view of a command harder to reason about, and because a future
    /// length-checking host would reject us.
    pub const D1_DESCRIPTOR_LENGTH: &str =
        "command descriptor uses scratch-buffer length, not command length";

    /// D2 — the chain head is hardcoded to descriptor 0.
    ///
    /// `VirtQueue::push` (virtio/mod.rs):
    ///
    /// ```rust
    /// let head: u16 = 0;
    /// ```
    ///
    /// and `wait` then asserts the retired descriptor id is 0:
    ///
    /// ```rust
    /// if id as u16 != 0 {
    ///     return Err("unexpected used-ring descriptor id");
    /// }
    /// ```
    ///
    /// This makes the control queue **structurally single-outstanding**: one
    /// command at a time, no pipelining. Linux allocates a `virtio_gpu_vbuffer`
    /// per command from a free list and can have many in flight; the ack path
    /// (`virtio_gpu_dequeue_ctrl_func`) walks a `reclaim_list`, so completions
    /// are reaped in bulk rather than one at a time.
    ///
    /// **Impact: correctness-neutral, performance-bounded.** It cannot corrupt a
    /// transfer. It does mean the queue's depth is 1 regardless of what the
    /// device advertises, and that any command which blocks also blocks every
    /// later command. Relevant to any future asynchronous design.
    pub const D2_SINGLE_OUTSTANDING: &str = "chain head hardcoded to 0; queue depth 1";

    /// D3 — completion is a bounded busy-wait, not a sleep.
    ///
    /// `VirtQueue::wait` spins on `used_idx` up to 100M iterations of
    /// `core::hint::spin_loop()`. Called from `submit_3d`, which is reached from
    /// the execbuffer ioctl, i.e. with interrupts already masked by
    /// `IA32_FMASK`.
    ///
    /// Linux never spins: `virtio_gpu_wait_for_response` uses
    /// `wait_event_interruptible_timeout` on `vgdev->ctrlq.ack_queue`, woken by
    /// `wake_up(&vgdev->ctrlq.ack_queue)` at the end of the reclaim loop
    /// (virtgpu_vq.c:308). A command that takes milliseconds therefore costs one
    /// core, with interrupts off, for the duration.
    ///
    /// **Impact: not a correctness bug** — the spin is bounded and does complete
    /// — but it is a busy-wait in interrupt-masked context, so on a 2-CPU guest
    /// a long host command can starve the other core's ability to make progress
    /// on anything that needs an interrupt. AGENTS.md invariant 2 (cooperative
    /// interrupt masking) makes this worth fixing rather than documenting.
    pub const D3_BUSY_WAIT: &str = "bounded busy-wait in interrupt-masked context, not a sleep";

    /// D4 — notify is issued per push, with no kick batching.
    ///
    /// `push` writes the notify port unconditionally at the end. Linux batches:
    /// many commands are added, then `virtqueue_kick` runs once, guarded by
    /// `virtqueue_kick_prepare` so the notification is skipped when the device
    /// has already been kicked and has not yet used the ring.
    ///
    /// **Impact: minor.** One MMIO write per command where Linux would
    /// amortise. Correctness is unaffected because we only ever have one command
    /// outstanding (D2), so there is nothing to batch.
    pub const D4_NO_KICK_BATCHING: &str = "notify per push; no kick batching";
/// D5 — no capset cache. Two round trips per `GET_CAPSET`.
    ///
    /// Linux keeps `vgdev->cap_cache`, a list of `virtio_gpu_drv_cap_cache`
    /// keyed by `(id, version)`. `virtio_gpu_get_capset` looks it up
    /// (`virtgpu_ioctl.c:454-456`) and returns the cached copy on a hit, so only
    /// the first query per capset version costs a round trip.
    ///
    /// We fetch unconditionally, and until this turn we fetched **twice per
    /// ioctl** (the diagnostic block and the real copy, which could disagree).
    /// Both fixed; the cache is not.
    ///
    /// **Impact: latency only.** Not the shear.
    pub const D5_NO_CAPSET_CACHE: &str = "no cap_cache; GET_CAPSET always round-trips";

    /// D6 — no bounds check on the guest-supplied capset index.
    ///
    /// `get_capset_info(index: u32)` takes the index straight from the ioctl and
    /// writes `capsets[i]`-equivalent state. Linux sizes the array
    /// `VIRTIO_GPU_MAX_SCANOUTS` and the callback indexes it directly
    /// (`virtgpu_vq.c:935`), so the safety there comes from the caller's
    /// validation, not from the callback.
    ///
    /// **Impact: a guest can ask for capset index 99 and index past our table.**
    /// Needs checking before the ioctl is exposed.
    pub const D6_CAPSET_INDEX_UNBOUNDED: &str = "capset index not validated against table size";

    /// D7 — no `is_valid` / write-barrier protocol.
    ///
    /// See `CapCacheEntry::store`. Harmless while the submit path is
    /// synchronous; a correctness requirement the moment it is not.
    pub const D7_NO_PUBLISH_BARRIER: &str = "no is_valid + smp_wmb publish protocol (needed once async)";
}

/// The response-header handling from `virtio_gpu_dequeue_ctrl_func`
/// (virtgpu_vq.c:302-305), which is the part that actually matters for fences:
///
/// ```c
/// if (resp->flags & cpu_to_le32(VIRTIO_GPU_FLAG_FENCE)) {
///         fence_id = le64_to_cpu(resp->fence_id);
///         virtio_gpu_fence_event_process(vgdev, fence_id);
/// }
/// ```
///
/// Note what is *not* here: the fence id lives in the response header of the
/// same command. There is no extra used-ring entry and nothing to desynchronise.
/// An earlier comment in our `submit_3d` claimed the host signals a fence by
/// pushing an extra entry onto the named queue's used ring; that is not the
/// protocol.
///
/// Returns the fence id the host actually signalled, or `None` if it did not set
/// `FLAG_FENCE`. Never invent one: signalling an id the host did not return
/// would be a lie, and signalling nothing would hang the client.
pub fn response_fence_id(resp: &VirtioGpuCtrlHdr) -> Option<u64> {
    if resp.flags & super::super::virtio::gpu::VIRTIO_GPU_FLAG_FENCE != 0 {
        Some(resp.fence_id)
    } else {
        None
    }
}

/// Response-type classification, from the same loop (virtgpu_vq.c:288-300):
///
/// ```c
/// if (resp->type != VIRTIO_GPU_RESP_OK_NODATA) {
///         if (le32_to_cpu(resp->type) >= VIRTIO_GPU_RESP_ERR_UNSPEC) {
///                 DRM_ERROR_RATELIMITED("response 0x%x (command 0x%x)\n", ...);
///         } else
///                 DRM_DEBUG("response 0x%x\n", ...);
/// }
/// ```
///
/// i.e. anything at or above `RESP_ERR_UNSPEC` is an error worth logging loudly;
/// anything else is a non-`OK_NODATA` success worth noting quietly. A driver that
/// treats every non-`OK_NODATA` as failure would reject valid replies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RespClass {
    OkNoData,
    OkOther,
    Error(u32),
}

pub fn classify_response(hdr_type: u32) -> RespClass {
    use crate::drivers::virtio::gpu::*;
    if hdr_type == VIRTIO_GPU_RESP_OK_NODATA {
        RespClass::OkNoData
    } else if hdr_type >= VIRTIO_GPU_RESP_ERR_UNSPEC {
        RespClass::Error(hdr_type)
    } else {
        RespClass::OkOther
    }
}

/// `virtio_gpu_cmd_get_capset_info_cb` (virtgpu_vq.c:927-946).
///
/// ```c
/// int i = le32_to_cpu(cmd->capset_index);
/// spin_lock(&vgdev->display_info_lock);
/// if (vgdev->capsets) {
///         vgdev->capsets[i].id          = le32_to_cpu(resp->capset_id);
///         vgdev->capsets[i].max_version = le32_to_cpu(resp->capset_max_version);
///         vgdev->capsets[i].max_size    = le32_to_cpu(resp->capset_max_size);
/// } else {
///         DRM_ERROR("invalid capset memory.");
/// }
/// spin_unlock(&vgdev->display_info_lock);
/// wake_up(&vgdev->resp_wq);
/// ```
///
/// One entry per capset index, and **`max_size` is the only place the host's
/// capset length exists**. `virtio_gpu_get_capset` then does
/// `size = min(args->size, host_caps_size)` (virtgpu_ioctl.c:450-452).
///
/// Note the bounds: Linux indexes `capsets[i]` by the guest-supplied
/// `capset_index` with no explicit range check visible here, relying on the
/// array being sized `VIRTIO_GPU_MAX_SCANOUTS` and the caller having validated.
/// Our `get_capset_info` takes an `index: u32` straight from the ioctl; that is
/// an out-of-bounds write waiting for a guest that asks for capset 99.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct CapsetInfo {
    pub id: u32,
    pub max_version: u32,
    pub max_size: u32,
}

/// `virtio_gpu_get_capset` (virtgpu_ioctl.c:443-452), the clamp, transcribed.
///
/// ```c
/// host_caps_size = vgdev->capsets[found_valid].max_size;
/// /* only copy to user the minimum of the host caps size or the guest caps size */
/// size = min(args->size, host_caps_size);
/// ```
///
/// Returns `None` when no capset matches -- Linux returns `-EINVAL`, and the
/// caller has already rejected `args->size == 0`.
pub fn capset_transfer_len(
    capsets: &[CapsetInfo],
    cap_set_id: u32,
    cap_set_ver: u32,
    guest_size: usize,
) -> Option<usize> {
    // "don't allow userspace to pass 0"
    if guest_size == 0 {
        return None;
    }
    let mut found_valid: i32 = -1;
    for (i, c) in capsets.iter().enumerate() {
        if c.id == cap_set_id {
            if c.max_version >= cap_set_ver {
                found_valid = i as i32;
                break;
            }
        }
    }
    if found_valid == -1 {
        return None;
    }
    let host_caps_size = capsets[found_valid as usize].max_size as usize;
    Some(guest_size.min(host_caps_size))
}

/// `virtio_gpu_cmd_capset_cb` (virtgpu_vq.c:947-970): store the reply in the
/// cache, then publish it.
///
/// ```c
/// list_for_each_entry(cache_ent, &vgdev->cap_cache, head) {
///         if (cache_ent->version == le32_to_cpu(cmd->capset_version) &&
///             cache_ent->id == le32_to_cpu(cmd->capset_id)) {
///                 memcpy(cache_ent->caps_cache, resp->capset_data, cache_ent->size);
///                 /* Copy must occur before is_valid is signalled. */
///                 smp_wmb();
///                 atomic_set(&cache_ent->is_valid, 1);
///                 break;
///         }
/// }
/// ```
///
/// Two things worth carrying over verbatim:
///
/// **The cache is keyed by `(id, version)`, and the memcpy length is
/// `cache_ent->size`** -- the size recorded when the cache entry was allocated,
/// not the size the host just wrote. Copying `resp.size` bytes into a buffer
/// sized for something else is the same class of bug as the clamp we just fixed.
///
/// **The write barrier is mandatory and the comment says so**: `smp_wmb()` before
/// setting `is_valid`. A reader that samples `is_valid` and then reads
/// `caps_cache` without an acquire barrier can observe the flag before the data.
/// Our queue is synchronous today so there is no such window, but this is exactly
/// the hazard that appears the moment S6 (async submit) is implemented -- so it
/// belongs in the translation rather than being rediscovered later.
pub struct CapCacheEntry {
    pub id: u32,
    pub version: u32,
    /// Allocated capacity. The memcpy bound, per the C.
    pub size: usize,
    pub caps_cache: Vec<u8>,
    pub is_valid: bool,
}

impl CapCacheEntry {
    /// Mirrors `memcpy(cache_ent->caps_cache, resp->capset_data, cache_ent->size)`
    /// -- bounded by the entry's own capacity, not by the response length.
    ///
    /// Returns false if the response does not fit, which Linux does not check
    /// because it sized the entry from the same `max_size` it is copying.
    pub fn store(&mut self, data: &[u8]) -> bool {
        if data.len() > self.size {
            return false;
        }
        self.caps_cache[..data.len()].copy_from_slice(data);
        // "Copy must occur before is_valid is signalled." -- smp_wmb()
        self.is_valid = true;
        true
    }
}

/// Divergences from `virtgpu_vq.c`, ranked.


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_fence_flag_means_no_fence() {
        let resp = VirtioGpuCtrlHdr { flags: 0, ..Default::default() };
        assert_eq!(response_fence_id(&resp), None);
    }

    #[test]
    fn fence_id_comes_from_the_response_header() {
        use crate::drivers::virtio::gpu::VIRTIO_GPU_FLAG_FENCE;
        let resp = VirtioGpuCtrlHdr {
            flags: VIRTIO_GPU_FLAG_FENCE,
            fence_id: 0x1234_5678,
            ..Default::default()
        };
        assert_eq!(response_fence_id(&resp), Some(0x1234_5678));
    }

    #[test]
    fn non_ok_nodata_success_is_not_an_error() {
        use crate::drivers::virtio::gpu::*;
        // e.g. RESP_OK_DISPLAY_INFO -- a success, not a failure.
        assert_eq!(classify_response(VIRTIO_GPU_RESP_OK_DISPLAY_INFO), RespClass::OkOther);
        assert_eq!(classify_response(VIRTIO_GPU_RESP_OK_NODATA), RespClass::OkNoData);
        assert!(matches!(
            classify_response(VIRTIO_GPU_RESP_ERR_UNSPEC),
            RespClass::Error(_)
        ));
    }
}

#[cfg(test)]
mod tests2 {
    use super::*;

    fn capsets() -> Vec<CapsetInfo> {
        alloc::vec![
            CapsetInfo { id: 2, max_version: 2, max_size: 1408 },
            CapsetInfo { id: 3, max_version: 1, max_size: 1024 },
        ]
    }

    #[test]
    fn clamp_is_min_of_host_and_guest() {
        // Guest asks for more than the host has -> host wins.
        assert_eq!(capset_transfer_len(&capsets(), 2, 2, 4096), Some(1408));
        // Guest asks for less -> guest wins.
        assert_eq!(capset_transfer_len(&capsets(), 2, 2, 512), Some(512));
        // Exact.
        assert_eq!(capset_transfer_len(&capsets(), 2, 2, 1408), Some(1408));
    }

    #[test]
    fn zero_size_is_rejected() {
        // Linux: "don't allow userspace to pass 0" -> -EINVAL
        assert_eq!(capset_transfer_len(&capsets(), 2, 2, 0), None);
    }

    #[test]
    fn unknown_capset_or_version_is_rejected() {
        assert_eq!(capset_transfer_len(&capsets(), 99, 1, 512), None);
        // Version higher than the host supports.
        assert_eq!(capset_transfer_len(&capsets(), 2, 5, 512), None);
    }

    #[test]
    fn cache_entry_is_bounded_by_its_own_capacity() {
        let mut e = CapCacheEntry {
            id: 2,
            version: 2,
            size: 8,
            caps_cache: alloc::vec![0u8; 8],
            is_valid: false,
        };
        // Host reply larger than the entry: must NOT copy, must NOT set valid.
        assert!(!e.store(&[1u8; 16]));
        assert!(!e.is_valid);
        assert!(e.store(&[7u8; 8]));
        assert!(e.is_valid);
    }
}