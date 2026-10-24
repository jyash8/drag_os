//! VirtIO-GPU 2D driver (VirtIO 1.0 spec §5.7).
//!
//! Provides hardware-accelerated scanout in QEMU/KVM via:
//!   -device virtio-gpu-pci
//!   -device virtio-vga
//!
//! Integrates with the DRM/KMS core as a KmsBackend.

#![allow(unused)]

use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};

use shared::memory::address::VirtAddr;

use crate::arch::x86_64::pci::{find_device, PciAddress};
use crate::arch::sync::spinlock::SpinLock;
use core::sync::atomic::Ordering;

use super::super::drm::kms::KmsBackend;
use super::super::drm::types::DrmModeResolution;

use super::{
    find_cap, CommonCfg, DmaBuf, VirtQueue, CAP_COMMON_CFG, CAP_DEVICE_CFG, CAP_NOTIFY_CFG,
    REG_DEVICE_FEATURE, REG_DEVICE_FEATURE_SELECT, REG_DEVICE_STATUS, REG_DRIVER_FEATURE,
    REG_DRIVER_FEATURE_SELECT, REG_QUEUE_ENABLE, REG_QUEUE_SELECT, STATUS_ACKNOWLEDGE, STATUS_DRIVER,
    STATUS_DRIVER_OK, STATUS_FEATURES_OK, VIRTIO_VENDOR_ID,
};

// ============================================================================
// VirtIO-GPU PCI Device IDs
// ============================================================================

/// VirtIO PCI Vendor ID (Red Hat, Inc.)
pub const VIRTIO_GPU_VENDOR_ID: u16 = 0x1AF4;

/// VirtIO-GPU PCI Device ID (Modern transitional/non-transitional GPU device)
pub const VIRTIO_GPU_DEVICE_ID: u16 = 0x1050;

// ============================================================================
// VirtIO-GPU Command Types
// ============================================================================

/// 2D commands
pub const VIRTIO_GPU_CMD_GET_DISPLAY_INFO: u32 = 0x0100;
pub const VIRTIO_GPU_CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
pub const VIRTIO_GPU_CMD_RESOURCE_UNREF: u32 = 0x0102;
pub const VIRTIO_GPU_CMD_SET_SCANOUT: u32 = 0x0103;
pub const VIRTIO_GPU_CMD_RESOURCE_FLUSH: u32 = 0x0104;
pub const VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
pub const VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
pub const VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING: u32 = 0x0107;

/// 3D commands (VirtIO 1.0 / 1.2 §5.7.6.7)
pub const VIRTIO_GPU_CMD_CTX_CREATE: u32 = 0x0200;
pub const VIRTIO_GPU_CMD_CTX_DESTROY: u32 = 0x0201;
pub const VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE: u32 = 0x0202;
pub const VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE: u32 = 0x0203;

/// Response header flag: this command completed a fence.
///
/// Linux (virtgpu_vq.c:303) reaps it as
/// `if (resp->flags & VIRTIO_GPU_FLAG_FENCE) { fence_id = resp->fence_id; }`,
/// i.e. the fence comes back in the response header of the same command.
pub const VIRTIO_GPU_FLAG_FENCE: u32 = 1 << 0;
pub const VIRTIO_GPU_CMD_RESOURCE_CREATE_3D: u32 = 0x0204;
pub const VIRTIO_GPU_CMD_TRANSFER_TO_HOST_3D: u32 = 0x0205;
pub const VIRTIO_GPU_CMD_TRANSFER_FROM_HOST_3D: u32 = 0x0206;
pub const VIRTIO_GPU_CMD_SUBMIT_3D: u32 = 0x0207;
pub const VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB: u32 = 0x0208;
pub const VIRTIO_GPU_CMD_RESOURCE_UNMAP_BLOB: u32 = 0x0209;

/// Capset IDs for hardware acceleration
pub const VIRTIO_GPU_CAPSET_VIRGL: u32 = 1;
pub const VIRTIO_GPU_CAPSET_VIRGL2: u32 = 2;
pub const VIRTIO_GPU_CAPSET_GFXSTREAM_VULKAN: u32 = 3;
pub const VIRTIO_GPU_CAPSET_VENUS: u32 = 4;
pub const VIRTIO_GPU_CAPSET_CROSS_DOMAIN: u32 = 5;

/// Success responses
// Response classification comes from the translated `virtgpu_vq`, so the
// OK_NODATA / OK-other / error rule is stated once (and unit-tested) rather
// than hand-written at each of the ten call sites that need it.
use crate::drivers::drm::virtgpu_vq::{classify_response, RespClass};

pub const VIRTIO_GPU_RESP_OK_NODATA: u32 = 0x1100;
pub const VIRTIO_GPU_RESP_OK_DISPLAY_INFO: u32 = 0x1101;
pub const VIRTIO_GPU_RESP_OK_CAPSET_INFO: u32 = 0x1102;
pub const VIRTIO_GPU_RESP_OK_CAPSET: u32 = 0x1103;
pub const VIRTIO_GPU_RESP_OK_EDID: u32 = 0x1104;

/// Error responses
pub const VIRTIO_GPU_RESP_ERR_UNSPEC: u32 = 0x1200;
pub const VIRTIO_GPU_RESP_ERR_OUT_OF_MEMORY: u32 = 0x1201;
pub const VIRTIO_GPU_RESP_ERR_INVALID_SCANOUT_ID: u32 = 0x1202;
pub const VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID: u32 = 0x1203;
pub const VIRTIO_GPU_RESP_ERR_INVALID_CONTEXT_ID: u32 = 0x1204;
pub const VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER: u32 = 0x1205;

/// Feature bits (low feature word)
pub const VIRTIO_GPU_F_VIRGL: u32 = 1 << 0;
pub const VIRTIO_GPU_F_EDID: u32 = 1 << 1;
pub const VIRTIO_GPU_F_RESOURCE_BLOB: u32 = 1 << 3;
/// Independent of RESOURCE_BLOB. Upstream requests it too (see the negotiate
/// comment below); until it is in `driver_features` `has_blob_alignment` is
/// always false and `VIRTGPU_PARAM_BLOB_ALIGNMENT` correctly answers -ENOENT.
pub const VIRTIO_GPU_F_BLOB_ALIGNMENT: u32 = 1 << 5;

/// Blob memory types (RESOURCE_CREATE_BLOB)
pub const VIRTIO_GPU_BLOB_MEM_GUEST: u32 = 0x0001;

/// Blob commands
pub const VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB: u32 = 0x010C;
pub const VIRTIO_GPU_CMD_SET_SCANOUT_BLOB: u32 = 0x010D;

/// Cursor commands (sent on the cursor virtqueue, queue 1)
pub const VIRTIO_GPU_CMD_UPDATE_CURSOR: u32 = 0x0300;
pub const VIRTIO_GPU_CMD_MOVE_CURSOR: u32 = 0x0301;

/// Query commands (in 2D range since the 0x09xx draft was renumbered)
pub const VIRTIO_GPU_CMD_GET_CAPSET_INFO: u32 = 0x0108;
pub const VIRTIO_GPU_CMD_GET_CAPSET: u32 = 0x0109;
pub const VIRTIO_GPU_CMD_GET_EDID: u32 = 0x010a;

// ============================================================================
// VirtIO-GPU Formats
// ============================================================================

/// 2D Resource pixel formats (VirtIO 1.0 §5.7.6.8)
pub const VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM: u32 = 1;
pub const VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM: u32 = 2;

/// Maximum number of scanouts supported by VirtIO-GPU
pub const VIRTIO_GPU_MAX_SCANOUTS: usize = 16;

/// How many capset slots we are willing to probe.
///
/// The virtio-gpu *spec* does not define a capset count -- QEMU's
/// `VIRTIO_GPU_MAX_CAPSETS` is host-internal and absent from the uapi header --
/// so there is no spec value to defer to here. What we can bound against is our
/// own enumeration depth, which is 8. Binding the index to that makes it
/// impossible for any caller, present or future, to walk past the slots we
/// actually probe.
pub const CAPSET_PROBE_LIMIT: u32 = 8;

// ============================================================================
// VirtIO-GPU Header Structures (C-compatible)
// ============================================================================

/// Standard control header prefixing every request and response.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VirtioGpuCtrlHdr {
    pub hdr_type: u32,
    pub flags: u32,
    pub fence_id: u64,
    pub ctx_id: u32,
    pub ring_idx: u8,
    pub padding: [u8; 3],
}

/// 2D rectangle describing screen coordinates and dimensions.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VirtioGpuRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// One scanout entry: `struct virtio_gpu_display_one`.
///
/// EIGHT le32 = 32 bytes: x, y, width, height, flag, type, physical_width,
/// physical_height.
///
/// This was previously modelled as a 24-byte `{enabled, flags, rect}` with no
/// `num_modes` in the enclosing struct. Both halves were wrong, and they
/// compounded: the header is 24 bytes and the spec puts `le32 num_modes`
/// immediately after it, so reading `pmodes[0]` at offset 24 meant our `enabled`
/// field was actually `num_modes` -- which is 0 on this host, hence "OK but no
/// enabled pmode" on every boot. The element being 24 rather than 32 also
/// walked every subsequent mode at the wrong stride.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VirtioGpuDisplayOne {
    pub rect: VirtioGpuRect,
    /// `VIRTUIO_GPU_MODE_F_ENABLED` (bit 0) is what says the mode is active.
    pub flag: u32,
    pub type_: u32,
    pub physical_width: u32,
    pub physical_height: u32,
}

impl VirtioGpuDisplayOne {
    pub const SIZE: usize = 32;
    pub const MODE_F_ENABLED: u32 = 1 << 0;
    pub fn enabled(&self) -> bool {
        self.flag & Self::MODE_F_ENABLED != 0
    }
}

/// `struct virtio_gpu_resp_display_info`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VirtioGpuRespDisplayInfo {
    pub hdr: VirtioGpuCtrlHdr,
    /// Immediately after the 24-byte header. Its absence is why every parse
    /// of this response has been off by one word.
    pub num_modes: u32,
    pub modes: [VirtioGpuDisplayOne; VIRTIO_GPU_MAX_SCANOUTS],
}

/// Command struct: Create a 2D host resource.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VirtioGpuResourceCreate2d {
    pub hdr: VirtioGpuCtrlHdr,
    pub resource_id: u32,
    pub format: u32,
    pub width: u32,
    pub height: u32,
}

/// Command struct: Unreference / destroy a host resource.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VirtioGpuResourceUnref {
    pub hdr: VirtioGpuCtrlHdr,
    pub resource_id: u32,
    pub _pad: u32,
}

/// Command struct: Attach a host resource to a scanout output.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VirtioGpuSetScanout {
    pub hdr: VirtioGpuCtrlHdr,
    pub r: VirtioGpuRect,
    pub scanout_id: u32,
    pub resource_id: u32,
}

/// Command struct: Flush host resource modifications to the display window.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VirtioGpuResourceFlush {
    pub hdr: VirtioGpuCtrlHdr,
    pub r: VirtioGpuRect,
    pub resource_id: u32,
    pub _pad: u32,
}

/// Command struct: Transfer pixel data from guest backing memory to host 2D resource.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VirtioGpuTransferToHost2d {
    pub hdr: VirtioGpuCtrlHdr,
    pub r: VirtioGpuRect,
    pub offset: u64,
    pub resource_id: u32,
    pub _pad: u32,
}

/// Command struct: Attach guest memory pages as backing store for a resource.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VirtioGpuResourceAttachBacking {
    pub hdr: VirtioGpuCtrlHdr,
    pub resource_id: u32,
    pub nr_entries: u32,
}

/// Memory entry descriptor describing a guest physical memory slice.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VirtioGpuMemEntry {
    pub addr: u64,
    pub length: u32,
    pub _pad: u32,
}

/// Command struct: Create a guest-backed or host-backed blob resource.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VirtioGpuResourceCreateBlob {
    pub hdr: VirtioGpuCtrlHdr,
    pub resource_id: u32,
    pub blob_mem: u32,
    pub blob_flags: u32,
    pub nr_entries: u32,
    pub blob_id: u64,
    pub size: u64,
}

/// Command struct: map a host blob into the hostmem window at our offset.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VirtioGpuResourceMapBlob {
    pub hdr: VirtioGpuCtrlHdr,
    pub resource_id: u32,
    pub _pad: u32,
    pub offset: u64,
}

/// Command struct: Set scanout with explicit framebuffer metadata (blob resources).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VirtioGpuSetScanoutBlob {
    pub hdr: VirtioGpuCtrlHdr,
    pub r: VirtioGpuRect,
    pub scanout_id: u32,
    pub resource_id: u32,
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub _pad: u32,
    /// [4] plane strides in bytes; plane 0 = pitch
    pub strides: [u32; 4],
    /// [4] plane offsets from start of backing
    pub offsets: [u32; 4],
}

/// `virtio_gpu_cursor_pos` from <linux/virtio_gpu.h> (part of the
/// update/move-cursor commands).
///
/// `scanout_id` is the **first** field of the position, not a member of
/// `virtio_gpu_update_cursor`. The kernel previously modelled the position as
/// {x, y, pad} and then carried a stray `scanout_id` on the update struct,
/// which is not the wire layout at all: `update_cursor` is
/// {hdr, pos, resource_id, hot_x, hot_y, padding} = 56 bytes, and this code was
/// emitting 48.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VirtioGpuPosition {
    pub scanout_id: u32,
    pub x: u32,
    pub y: u32,
    pub _pad: u32,
}

/// `virtio_gpu_update_cursor` from <linux/virtio_gpu.h>: 56 bytes. Shows or
/// updates the hardware cursor image and position.
///
/// The hotspot and trailing padding are not decoration — the host reads them,
/// and omitting them shifts every following field. Found by
/// scripts/audit-virtio-gpu-abi.py, which diffs these layouts against the uapi
/// header; see that script for why the check exists.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VirtioGpuUpdateCursor {
    pub hdr: VirtioGpuCtrlHdr,
    pub pos: VirtioGpuPosition,
    /// Update only: the resource holding the new cursor image.
    pub resource_id: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    pub _pad: u32,
}

/// Command struct: Query a capset's size/version.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VirtioGpuGetCapsetInfo {
    pub hdr: VirtioGpuCtrlHdr,
    pub capset_index: u32,
    pub _pad: u32,
}

/// Response payload for GET_CAPSET_INFO.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VirtioGpuRespCapsetInfo {
    pub hdr: VirtioGpuCtrlHdr,
    pub capset_id: u32,
    pub capset_version: u32,
    pub capset_size: u32,
}

/// Command struct: Fetch a capset's raw data.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VirtioGpuGetCapset {
    pub hdr: VirtioGpuCtrlHdr,
    pub capset_id: u32,
    pub capset_version: u32,
}

/// Command struct: Fetch a connected display's EDID block.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VirtioGpuGetEdid {
    pub hdr: VirtioGpuCtrlHdr,
    pub scanout_id: u32,
    pub _pad: u32,
}

/// Response payload for GET_EDID.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VirtioGpuRespEdid {
    pub hdr: VirtioGpuCtrlHdr,
    pub size: u32,
    pub _pad: u32,
    pub edid: [u8; 1024],
}

// ============================================================================
// VirtIO-GPU 3D Header Structures
// ============================================================================

/// 3D box describing a sub-volume in 3D resources.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VirtioGpuBox {
    pub x: u32,
    pub y: u32,
    pub z: u32,
    pub w: u32,
    pub h: u32,
    pub d: u32,
}

/// Command struct: Create a 3D rendering context (VirGL / Venus).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VirtioGpuCtxCreate {
    pub hdr: VirtioGpuCtrlHdr,
    pub nlen: u32,
    pub context_init: u32,
    pub debug_name: [u8; 64],
}

impl Default for VirtioGpuCtxCreate {
    fn default() -> Self {
        Self {
            hdr: VirtioGpuCtrlHdr::default(),
            nlen: 0,
            context_init: 0,
            debug_name: [0; 64],
        }
    }
}

/// Command struct: Destroy a 3D rendering context.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VirtioGpuCtxDestroy {
    pub hdr: VirtioGpuCtrlHdr,
}

/// Command struct: Attach or detach a resource to/from a 3D context.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VirtioGpuCtxResource {
    pub hdr: VirtioGpuCtrlHdr,
    pub resource_id: u32,
    pub _pad: u32,
}

/// Command struct: Create a 3D host resource (textures, buffers, render targets).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VirtioGpuResourceCreate3d {
    pub hdr: VirtioGpuCtrlHdr,
    pub resource_id: u32,
    pub target: u32,
    pub format: u32,
    pub bind: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub array_size: u32,
    pub last_level: u32,
    pub nr_samples: u32,
    pub flags: u32,
    pub _pad: u32,
}

/// Command struct: Transfer data to/from 3D host resource.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VirtioGpuTransferHost3d {
    pub hdr: VirtioGpuCtrlHdr,
    pub box_: VirtioGpuBox,
    pub offset: u64,
    pub resource_id: u32,
    pub level: u32,
    pub stride: u32,
    pub layer_stride: u32,
}

/// Command struct: Submit a 3D command stream to a rendering context.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VirtioGpuCmdSubmit3d {
    pub hdr: VirtioGpuCtrlHdr,
    pub size: u32,
    pub _pad: u32,
}

// ============================================================================
// VirtIO-GPU Driver Struct
// ============================================================================

/// VirtIO-GPU 2D Driver instance managing device state and queues.
/// One in-flight command's private DMA buffers.
pub struct SlotBuf {
    pub cmd: DmaBuf,
    pub resp: DmaBuf,
}

/// Exact count of 3D submits currently outstanding.
///
/// Module-level rather than a struct field so the 1ms timer tick can ask "is
/// there anything to reap?" WITHOUT taking `GLOBAL_VIRTIO_GPU`. That lock is
/// the one the compositor contends on for every flip; a per-tick poll that
/// grabbed it would be a self-inflicted stall. One relaxed word load is free.
///
/// While submits are synchronous this is non-zero only for the duration of one
/// `wait_for`, so the reaper usually finds nothing -- which is correct and
/// harmless. It becomes load-bearing the moment a submitter stops waiting.
static INFLIGHT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Lock-free "is any 3D submit outstanding?" for the timer tick.
///
/// Deliberately a free function over the module static, not a method on the
/// driver: the caller must be able to ask without holding the device lock.
pub fn any_inflight() -> bool {
    INFLIGHT.load(core::sync::atomic::Ordering::Relaxed) != 0
}

pub struct VirtioGpuDriver {
    /// MMIO virtual address of GPU config space
    pub config_bar: usize,
    /// MMIO virtual address for control queue notify register
    pub ctrl_queue_notify: usize,
    /// Active display width detected from GET_DISPLAY_INFO
    pub width: u32,
    /// Active display height detected from GET_DISPLAY_INFO
    pub height: u32,
    /// Counter for allocating monotonic resource IDs
    pub resource_id_counter: u32,
    /// Host negotiated VIRTIO_GPU_F_RESOURCE_BLOB
    pub has_resource_blob: bool,
    /// Host negotiated VIRTIO_GPU_F_BLOB_ALIGNMENT (bit 5) -- an *independent*
    /// feature from RESOURCE_BLOB (bit 3).
    ///
    /// Upstream gates `VIRTGPU_PARAM_BLOB_ALIGNMENT` on this, not on
    /// `has_resource_blob` (`virtgpu_kms.c:219` vs `:245-249`), and answers
    /// `-ENOENT` when it is absent. Conflating the two made a device with blob
    /// support but no alignment reporting claim the feature with value 0 --
    /// and a client computing `ALIGN(size, 0)` gets undefined behaviour.
    pub has_blob_alignment: bool,
    /// Alignment the host requires of blob sizes.
    ///
    /// Upstream reads this from `struct virtio_gpu_config.blob_alignment` in
    /// PCI config space. We do not read virtio_gpu_config at all, so we cannot
    /// report the host's number. Rather than fabricate one, this is the
    /// alignment *we* guarantee and enforce in `RESOURCE_CREATE_BLOB`: guest
    /// blob sizes are rounded to a page, because that is the granularity the
    /// backing allocator can actually honour.
    pub blob_alignment: u32,
    /// Host negotiated VIRTIO_GPU_F_EDID
    pub has_edid: bool,
    /// Host negotiated VIRTIO_GPU_F_VIRGL (3D hardware acceleration)
    pub has_virgl: bool,
    /// Venus hostmem window (PCI BAR 4) for HOST3D blob mappings.
    /// Recorded, never ioremapped here — mmap_device maps on demand.
    pub hostmem_base: Option<shared::memory::address::PhysAddr>,
    pub hostmem_len: usize,
    /// Bump allocator cursor (guest-chosen offsets) into the hostmem window.
    pub hostmem_next: u64,
    /// Control virtqueue for submitting GPU commands
    ctrl_queue: Option<VirtQueue>,
    /// Cursor virtqueue (queue 1) for the hardware cursor plane
    cursor_queue: Option<VirtQueue>,
    /// DMA buffer for command submission
    /// Per-in-flight buffer slots, indexed by descriptor head.
    ///
    /// Linux allocates a `virtio_gpu_vbuffer` per command from a free list
    /// (`virtio_gpu_alloc_cmd`), each carrying its OWN command and response
    /// buffer. We had exactly one shared pair, which is safe only because the
    /// submit path blocked: a second command in flight would overwrite the
    /// first's command buffer and its response slot.
    ///
    /// Grown lazily -- slots are allocated on first use of that head, so the
    /// synchronous path (which always uses head 0) costs one pair, exactly as
    /// before. This is the prerequisite for a non-blocking submit; it is not
    /// itself the async change.
    slot_bufs: [Option<SlotBuf>; crate::drivers::virtio::MAX_INFLIGHT],
    cmd_buf: Option<DmaBuf>,
    /// DMA buffer for command responses
    resp_buf: Option<DmaBuf>,
    /// DMA buffer for memory entry descriptors
    mem_entry_buf: Option<DmaBuf>,
    /// Context that owns each in-flight submit, indexed by descriptor head.
    ///
    /// The reaper reports these so the DRM layer can clear that context's
    /// "busy" mark, which is what releases any submit deferred behind it.
    head_ctx: [u32; crate::drivers::virtio::MAX_INFLIGHT],
    /// Device-written length of the most recent response (the used-ring `len`).
    pub last_used_len: u32,
    /// Scratch destination for `probe_rows`. Never used on the ordering path.
    probe_buf: Option<DmaBuf>,
}

/// Resources whose unref was deferred because they were still scanned out.
/// Reaped from `set_scanout` once the scanout moves elsewhere.
static PENDING_UNREF: SpinLock<alloc::vec::Vec<u32>> =
    SpinLock::named("PENDING_UNREF", alloc::vec::Vec::new());

impl VirtioGpuDriver {
    /// Probes and initializes a VirtIO-GPU device at the given PCI address.
    ///
    /// # Safety
    /// Reads and writes PCI configuration registers and sets up virtqueues.
    pub fn probe(pci_addr: PciAddress) -> Option<Self> {
        let common_cap = find_cap(&pci_addr, CAP_COMMON_CFG)?;
        let notify_cap = find_cap(&pci_addr, CAP_NOTIFY_CFG)?;
        let device_cap = find_cap(&pci_addr, CAP_DEVICE_CFG)?;

        let mut bar_virts: [Option<VirtAddr>; 8] = [None; 8];
        let mut get_bar = |i: u8| -> Option<VirtAddr> {
            let idx = i as usize;
            if let Some(v) = bar_virts[idx] {
                return Some(v);
            }
            let (v, _) = pci_addr.map_bar(u32::from(i)).ok()?;
            bar_virts[idx] = Some(v);
            Some(v)
        };

        let common = CommonCfg::new(VirtAddr::new(
            get_bar(common_cap.bar)?.0 + u64::from(common_cap.offset),
        ));
        let notify = VirtAddr::new(get_bar(notify_cap.bar)?.0 + u64::from(notify_cap.offset));
        let _device_cfg = VirtAddr::new(get_bar(device_cap.bar)?.0 + u64::from(device_cap.offset));

        unsafe { common.write_u8(REG_DEVICE_STATUS, STATUS_ACKNOWLEDGE) };

        let dev_hi = unsafe {
            common.write_u32(REG_DEVICE_FEATURE_SELECT, 1);
            common.read_u32(REG_DEVICE_FEATURE)
        };
        let dev_lo = unsafe {
            common.write_u32(REG_DEVICE_FEATURE_SELECT, 0);
            common.read_u32(REG_DEVICE_FEATURE)
        };
        if dev_hi & 1 == 0 {
            return None; // device lacks VIRTIO_F_VERSION_1
        }
        // Acknowledge VIRTIO_F_VERSION_1 plus the features we implement (including VirGL 3D).
        // Feature negotiation, per virtio_gpu_probe's `features[]`
        // (virtgpu_drv.c:171-185):
        //
        //   VIRTIO_GPU_F_VIRGL,          <-- only under #ifdef __LITTLE_ENDIAN
        //   VIRTIO_GPU_F_EDID,
        //   VIRTIO_GPU_F_RESOURCE_UUID,
        //   VIRTIO_GPU_F_RESOURCE_BLOB,
        //   VIRTIO_GPU_F_CONTEXT_INIT,
        //   VIRTIO_GPU_F_BLOB_ALIGNMENT,
        //
        // Two corrections to what this used to do:
        //
        // 1. `1 | ...` force-set bit 0 (VIRGL) unconditionally, i.e. we
        //    *requested* 3D acceleration whether or not the host offered it.
        //    virtio masks a requested bit the device did not offer out of the
        //    final set, but we then derived `has_virgl` from our own request,
        //    so a host without virgl would still be told we have it.
        // 2. Same error for `has_edid` and `has_resource_blob`: they were
        //    computed from `want`, not from what was actually negotiated. The
        //    negotiated set is `dev_features & driver_features`, so it can
        //    never contain a bit the device did not offer.
        //
        // VIRGL is still requested (we are little-endian, which is upstream's
        // condition), but now by *asking* rather than by asserting.
        // The masking rule is the translated `virtgpu_drv::negotiate`, stated
        // once and unit-tested, rather than an inline `dev_lo & (...)`. This is
        // the R3 fix: capability flags must come from the *negotiated* set, so
        // they can never claim a bit the device did not offer. `negotiate` is
        // what makes "requested" and "held" the same thing.
        //
        // BLOB_ALIGNMENT is now requested and honoured: it is an independent
        // bit from RESOURCE_BLOB, and we do enforce an alignment on blob sizes
        // (see `blob_alignment`). RESOURCE_UUID and CONTEXT_INIT are still not
        // requested -- we do not implement them.
        let driver_features = VIRTIO_GPU_F_VIRGL
            | VIRTIO_GPU_F_RESOURCE_BLOB
            | VIRTIO_GPU_F_EDID
            | VIRTIO_GPU_F_BLOB_ALIGNMENT;
        let want = crate::drivers::drm::virtgpu_drv::negotiate(dev_lo, driver_features);
        unsafe {
            common.write_u32(REG_DRIVER_FEATURE_SELECT, 1);
            common.write_u32(REG_DRIVER_FEATURE, 1); // accept only VIRTIO_F_VERSION_1
            common.write_u32(REG_DRIVER_FEATURE_SELECT, 0);
            common.write_u32(REG_DRIVER_FEATURE, want);
        }

        unsafe {
            common.write_u8(
                REG_DEVICE_STATUS,
                STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK,
            );
        }
        let status = unsafe { common.read_u8(REG_DEVICE_STATUS) };
        if status & STATUS_FEATURES_OK == 0 {
            return None;
        }

        // Set up control virtqueue (queue 0)
        unsafe { common.write_u16(REG_QUEUE_SELECT, 0) };
        let queue = VirtQueue::new(&common, notify, notify_cap.notify_off_multiplier)?;
        unsafe { common.write_u16(REG_QUEUE_ENABLE, 1) };

        // Set up cursor virtqueue (queue 1) for the hardware cursor plane
        unsafe { common.write_u16(REG_QUEUE_SELECT, 1) };
        let cursor_queue = VirtQueue::new(&common, notify, notify_cap.notify_off_multiplier);
        if cursor_queue.is_some() {
            unsafe { common.write_u16(REG_QUEUE_ENABLE, 1) };
        } else {
            // Re-select queue 0 so REG_QUEUE_ENABLE state stays consistent.
            unsafe { common.write_u16(REG_QUEUE_SELECT, 0) };
        }

        unsafe {
            common.write_u8(
                REG_DEVICE_STATUS,
                STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK,
            );
        }

        let notify_addr = notify.0 as usize;
        let config_bar = pci_addr.map_bar(0).ok().map(|(v, _)| v.0 as usize).unwrap_or(0);

        let mut driver = VirtioGpuDriver {
            config_bar,
            ctrl_queue_notify: notify_addr,
            width: 0,
            height: 0,
            resource_id_counter: 1,
            has_resource_blob: want & VIRTIO_GPU_F_RESOURCE_BLOB != 0,
            has_blob_alignment: want & VIRTIO_GPU_F_BLOB_ALIGNMENT != 0,
            // Page alignment: what we actually enforce on blob sizes.
            blob_alignment: 4096,
            has_edid: want & VIRTIO_GPU_F_EDID != 0,
            has_virgl: want & VIRTIO_GPU_F_VIRGL != 0,
            hostmem_base: None,
            hostmem_len: 0,
            hostmem_next: 0,
            ctrl_queue: Some(queue),
            cursor_queue,
            slot_bufs: [const { None }; crate::drivers::virtio::MAX_INFLIGHT],
            cmd_buf: None,
            resp_buf: None,
            mem_entry_buf: None,
            head_ctx: [0; crate::drivers::virtio::MAX_INFLIGHT],
            last_used_len: 0,
            probe_buf: None,
        };

        // ponytail: Venus HOST3D blobs live in the hostmem window (BAR 4);
        // record it for MAP_BLOB offset allocation. Absent without hostmem=.
        if let Some((base, len)) = pci_addr.read_bar(4) {
            if len > 0 {
                driver.hostmem_base = Some(base);
                driver.hostmem_len = len;
            }
        }

        // Query display info from host to learn screen dimensions
        match driver.get_display_info() {
            Some(rect) => {
                crate::dprintln!(
                    "[VIRTGPU] display_info OK: {}x{}",
                    rect.width,
                    rect.height
                );
                driver.width = rect.width;
                driver.height = rect.height;
            }
            None => {
                // TEMP-DIAG. A None here is silent and fatal in effect: drm_init
                // falls back to the GOP framebuffer, so the guest adopts the boot
                // resolution rather than the one QEMU was told. That divergence
                // is worth seeing explicitly.
                crate::dprintln!(
                    "[VIRTGPU] display_info FAILED (None) -> drm_init will fall back to GOP"
                );
            }
        }

        Some(driver)
    }

    /// Lazily allocates/grows DMA scratch buffers to fit `cmd_len`/`resp_len`.
    fn ensure_bufs(&mut self, cmd_len: usize, resp_len: usize) {
        let grow_cmd = self.cmd_buf.as_ref().map(|b| b.len() < cmd_len).unwrap_or(true);
        let grow_resp = self.resp_buf.as_ref().map(|b| b.len() < resp_len).unwrap_or(true);
        if grow_cmd {
            self.cmd_buf = DmaBuf::new(cmd_len.max(64));
        }
        if grow_resp {
            self.resp_buf = DmaBuf::new(resp_len.max(64));
        }
        if self.mem_entry_buf.is_none() {
            self.mem_entry_buf = DmaBuf::new(64);
        }
    }

    /// Sends a command and waits for its response.
    /// Returns the response bytes (excluding the header).
    fn send_command(&mut self, cmd: &VirtioGpuCtrlHdr, extra: &[(&DmaBuf, u32)]) -> Option<VirtioGpuCtrlHdr> {
        let queue = self.ctrl_queue.as_mut()?;
        let cmd_buf = self.cmd_buf.as_mut()?;
        let resp_buf = self.resp_buf.as_mut()?;

        unsafe {
            core::ptr::write_bytes(cmd_buf.as_mut_ptr(), 0, cmd_buf.len());
            core::ptr::copy_nonoverlapping(
                cmd as *const _ as *const u8,
                cmd_buf.as_mut_ptr(),
                core::mem::size_of::<VirtioGpuCtrlHdr>(),
            );
        }

        let mut parts: Vec<(u64, u32, u16)> = Vec::new();
        parts.push((cmd_buf.phys().0, cmd_buf.len() as u32, 0));
        for (buf, len) in extra {
            parts.push((buf.phys().0, *len, 0));
        }
        parts.push((resp_buf.phys().0, resp_buf.len() as u32, 2)); // VIRTQ_DESC_F_WRITE = 2

        queue.push(&parts).ok()?;
        let used_len = queue.wait().ok()?;

        unsafe {
            let resp_ptr = resp_buf.as_mut_ptr() as *const VirtioGpuCtrlHdr;
            self.last_used_len = used_len;
            Some(read_volatile(resp_ptr))
        }
    }

    /// Queries the host for connected display resolutions and geometry.
    pub fn get_display_info(&mut self) -> Option<VirtioGpuRect> {
        self.ensure_bufs(64, core::mem::size_of::<VirtioGpuRespDisplayInfo>());

        let cmd = VirtioGpuCtrlHdr {
            hdr_type: VIRTIO_GPU_CMD_GET_DISPLAY_INFO,
            flags: 0,
            fence_id: 0,
            ctx_id: 0,
            ring_idx: 0,
            padding: [0; 3],
        };

        let resp = self.send_command(&cmd, &[])?;
        if resp.hdr_type != VIRTIO_GPU_RESP_OK_DISPLAY_INFO {
            crate::dprintln!(
                "[VGPU] get_display_info: hdr_type={:#x} (want {:#x} OK_DISPLAY_INFO, {:#x} = ERR_UNSPEC)",
                resp.hdr_type,
                VIRTIO_GPU_RESP_OK_DISPLAY_INFO,
                VIRTIO_GPU_RESP_ERR_UNSPEC
            );
            return None;
        }

        // Parse from the raw bytes rather than through a struct cast: the
        // response is variable-length, so the only safe bound is what the host
        // said it wrote. `last_used_len` is the used ring's `len` word, which for
        // virtio-gpu is exactly `28 + num_modes * 32`.
        let used_len = self.last_used_len as usize;
        const HDR: usize = 24;
        const PER_MODE: usize = 32;
        let body = used_len.saturating_sub(HDR);
        let cap_by_len = body / PER_MODE;

        let resp_buf = self.resp_buf.as_ref()?;
        let base = resp_buf.virt_addr().0 as *const u8;

        // The mode array starts IMMEDIATELY after the 24-byte header on this
        // host -- there is no `num_modes` word in front of it.
        //
        // Evidence, all from one run:
        //   * `used_len = 408`, and 408 = 24 + 12*32 exactly, while
        //     28 + k*32 has no integer solution. So the prefix is 24 bytes.
        //   * Read at offset 24 the first entry is x=0 y=0 **640x480 flag=1**
        //     -- a coherent, enabled mode.
        //   * Read at offset 28 (i.e. assuming the spec's `le32 num_modes` at
        //     24) the same bytes become 480x1 at y=640, which is garbage.
        //
        // The spec does define `le32 num_modes` there. Rather than guess which
        // is wrong upstream, the count is derived from the device-written length
        // alone, so the parse does not depend on that field existing.
        let num_modes_at = unsafe { core::ptr::read_volatile(base.add(HDR) as *const u32) };
        crate::dprintln!(
            "[VGPU] display_info: used_len={} -> {} modes; word@24 (spec num_modes)={}",
            used_len, cap_by_len, num_modes_at
        );
        let count = cap_by_len.min(VIRTIO_GPU_MAX_SCANOUTS);

        for i in 0..count {
            let off = HDR + i * PER_MODE;
            let m = unsafe { &*(base.add(off) as *const VirtioGpuDisplayOne) };
            crate::dprintln!(
                "[VGPU] display_info: mode[{}] flag={:#x} enabled={} {}x{}+{}+{}",
                i, m.flag, m.enabled(), m.rect.width, m.rect.height, m.rect.x, m.rect.y
            );
            if m.enabled() {
                return Some(m.rect);
            }
        }
        crate::dprintln!(
            "[VGPU] get_display_info: OK but no enabled mode among {} parsed (used_len={})",
            count, used_len
        );
        None
    }
    /// Creates a 2D host resource with the given dimensions and pixel format.
    /// Creates a 2D host resource. Returns false if the host rejected it, in
    /// which case the id must not be used or registered.
    pub fn create_resource_2d(&mut self, resource_id: u32, width: u32, height: u32, format: u32) -> bool {
        self.ensure_bufs(64, 64);

        let mut cmd = VirtioGpuResourceCreate2d::default();
        cmd.hdr.hdr_type = VIRTIO_GPU_CMD_RESOURCE_CREATE_2D;
        cmd.resource_id = resource_id;
        cmd.format = format;
        cmd.width = width;
        cmd.height = height;

        let cmd_buf = self.cmd_buf.as_ref().unwrap();
        unsafe {
            core::ptr::write_bytes(cmd_buf.as_mut_ptr(), 0, cmd_buf.len());
            core::ptr::copy_nonoverlapping(
                &cmd as *const _ as *const u8,
                cmd_buf.as_mut_ptr(),
                core::mem::size_of::<VirtioGpuResourceCreate2d>(),
            );
        }

        match self.send_command_raw() {
            Some(resp) if classify_response(resp.hdr_type) == RespClass::OkNoData => true,
            Some(resp) => {
                crate::dprintln!(
                    "[VGPU] create_resource_2d({}) {}x{} fmt={} host error {:#x}",
                    resource_id, width, height, format, resp.hdr_type
                );
                false
            }
            None => {
                crate::dprintln!("[VGPU] create_resource_2d: queue failed");
                false
            }
        }
    }

    fn send_command_raw(&mut self) -> Option<VirtioGpuCtrlHdr> {
        let cmd_buf = self.cmd_buf.as_ref()?;
        let resp_buf = self.resp_buf.as_ref()?;

        // Remember what we asked for, so the diagnostic below can report
        // "we requested a fence and the host did/did not return one".
        let cb = cmd_buf.as_mut_ptr();
        let (req_type, req_flags, req_fence) = unsafe {
            let h = cb as *const VirtioGpuCtrlHdr;
            (
                core::ptr::read_volatile(cb as *const u32),
                core::ptr::read_volatile(core::ptr::addr_of!((*h).flags)),
                core::ptr::read_volatile(core::ptr::addr_of!((*h).fence_id)),
            )
        };

        let parts: [(u64, u32, u16); 2] = [
            (cmd_buf.phys().0, cmd_buf.len() as u32, 0),
            (resp_buf.phys().0, resp_buf.len() as u32, 2),
        ];

        let queue = self.ctrl_queue.as_mut()?;
        queue.push(&parts).ok()?;
        queue.wait().ok()?;

        unsafe {
            let resp_ptr = resp_buf.as_mut_ptr() as *const VirtioGpuCtrlHdr;
            let resp = read_volatile(resp_ptr);
            // DIAGNOSTIC: report the response's fence fields.
            //
            // Linux (virtgpu_vq.c:303) reads the fence out of exactly this
            // header: `if (resp->flags & VIRTIO_GPU_FLAG_FENCE) { fence_id =
            // resp->fence_id; ... }`. We were reading `hdr_type` and discarding
            // the rest, on the belief -- recorded at the call site below -- that
            // the host signals fences by pushing an extra used-ring entry. That
            // is not how the protocol works, so whether a fence actually comes
            // back here was never observable. Logged now to establish the
            // mechanism before relying on it.
            //
            // Bounded: only the first few, plus any fence event, so a hot path
            // is not flooded.
            {
                static SEEN: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
                let n = SEEN.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                let has_fence = resp.flags & VIRTIO_GPU_FLAG_FENCE != 0;
                // Log unconditionally when a fence was *requested* -- that is
                // the question being measured. Keying on position (as an earlier
                // revision did) stopped logging at the sixth command, which is
                // all capability queries, so no submit was ever observed.
                let asked_for_fence = req_flags & VIRTIO_GPU_FLAG_FENCE != 0;
                if n < 4 || (asked_for_fence && n < 80) || (has_fence && n < 80) {
                    crate::dprintln!(
                        "[fence] cmd={req_type:#x} req_flags={req_flags:#x} req_fence_id={req_fence} \
                         -> resp={:#x} resp_flags={:#x} resp_fence_id={} FENCE={}",
                        resp.hdr_type, resp.flags, resp.fence_id, has_fence
                    );
                }
            }
            Some(resp)
        }
    }

    /// Attaches guest physical memory frames as backing store to a resource.
    ///
    /// NOTE: the mem-entry array must be CONTIGUOUS with the command in the
    /// out descriptor chain — QEMU reads entries at combined out-offset
    /// `sizeof(VirtioGpuResourceAttachBacking)`, not per-descriptor.
    /// Returns false if the host rejected the attach, in which case the
    /// resource has no backing and every later access to it will fail.
    pub fn attach_backing(&mut self, resource_id: u32, pages: &[(u64, u32)]) -> bool {
        let out_len = core::mem::size_of::<VirtioGpuResourceAttachBacking>()
            + pages.len() * core::mem::size_of::<VirtioGpuMemEntry>();
        self.ensure_bufs(out_len, 64);

        let mut cmd = VirtioGpuResourceAttachBacking::default();
        cmd.hdr.hdr_type = VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING;
        cmd.resource_id = resource_id;
        cmd.nr_entries = pages.len() as u32;

        let cmd_buf = self.cmd_buf.as_ref().unwrap();
        unsafe {
            core::ptr::write_bytes(cmd_buf.as_mut_ptr(), 0, cmd_buf.len());
            core::ptr::copy_nonoverlapping(
                &cmd as *const _ as *const u8,
                cmd_buf.as_mut_ptr(),
                core::mem::size_of::<VirtioGpuResourceAttachBacking>(),
            );
            // Entries follow the command IN THE SAME BUFFER.
            let entries_ptr =
                cmd_buf.as_mut_ptr().add(core::mem::size_of::<VirtioGpuResourceAttachBacking>())
                    as *mut VirtioGpuMemEntry;
            for (i, (addr, len)) in pages.iter().enumerate() {
                core::ptr::write_volatile(
                    entries_ptr.add(i),
                    VirtioGpuMemEntry { addr: *addr, length: *len, _pad: 0 },
                );
            }
        }

        let out_len = core::mem::size_of::<VirtioGpuResourceAttachBacking>()
            + pages.len() * core::mem::size_of::<VirtioGpuMemEntry>();
        if out_len > cmd_buf.len() {
            crate::dprintln!("[VGPU] attach_backing({}): {} entries overflow cmd_buf", resource_id, pages.len());
            return false;
        }

        let resp_buf = self.resp_buf.as_ref().unwrap();
        let queue = self.ctrl_queue.as_mut().unwrap();

        let parts: [(u64, u32, u16); 2] = [
            (cmd_buf.phys().0, out_len as u32, 0),
            (resp_buf.phys().0, resp_buf.len() as u32, 2),
        ];
        let ok = queue.push(&parts).is_ok() && queue.wait().is_ok();
        let resp = unsafe { read_volatile(resp_buf.as_mut_ptr() as *const VirtioGpuCtrlHdr) };
        if !ok || classify_response(resp.hdr_type) != RespClass::OkNoData {
            // Log the EXACT address/length the host is told to use. This is one
            // end of the display-buffer address chain; the other end is the
            // mmap the compositor writes through. If those two disagree, the
            // host is displaying a buffer nobody wrote to.
            crate::dprintln!(
                "[VGPU] attach_backing({}) entries: {:?}",
                resource_id, pages
            );
            crate::dprintln!(
                "[VGPU] attach_backing({}) nr={} q_ok={} resp={:#x}",
                resource_id, pages.len(), ok, resp.hdr_type
            );
            return false;
        }
        true
    }

    /// Creates a blob resource with backing attached in the same command.
    /// GUEST blobs carry guest pages inline (host reads backing directly).
    /// HOST3D blobs pass no entries (host allocates shm; mapping comes via
    /// RESOURCE_MAP_BLOB + the hostmem window). No TRANSFER needed after.
    pub fn create_resource_blob(
        &mut self,
        ctx_id: u32,
        resource_id: u32,
        blob_mem: u32,
        blob_flags: u32,
        blob_id: u64,
        size: u64,
        pages: &[(u64, u32)],
    ) -> bool {
        if !self.has_resource_blob {
            return false;
        }
        let out_len = core::mem::size_of::<VirtioGpuResourceCreateBlob>()
            + pages.len() * core::mem::size_of::<VirtioGpuMemEntry>();
        self.ensure_bufs(out_len.max(256), 64);

        let mut cmd = VirtioGpuResourceCreateBlob::default();
        cmd.hdr.hdr_type = VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB;
        cmd.hdr.ctx_id = ctx_id;
        cmd.resource_id = resource_id;
        cmd.blob_mem = blob_mem;
        cmd.blob_flags = blob_flags;
        cmd.blob_id = blob_id;
        cmd.size = size;
        cmd.nr_entries = pages.len() as u32;

        let cmd_buf = self.cmd_buf.as_ref().unwrap();
        unsafe {
            core::ptr::write_bytes(cmd_buf.as_mut_ptr(), 0, cmd_buf.len());
            core::ptr::copy_nonoverlapping(
                &cmd as *const _ as *const u8,
                cmd_buf.as_mut_ptr(),
                core::mem::size_of::<VirtioGpuResourceCreateBlob>(),
            );
            // Entries contiguous with the command (QEMU reads at combined
            // out-offset sizeof(VirtioGpuResourceCreateBlob)).
            let entries_ptr =
                cmd_buf.as_mut_ptr().add(core::mem::size_of::<VirtioGpuResourceCreateBlob>())
                    as *mut VirtioGpuMemEntry;
            for (i, (addr, len)) in pages.iter().enumerate() {
                core::ptr::write_volatile(
                    entries_ptr.add(i),
                    VirtioGpuMemEntry { addr: *addr, length: *len, _pad: 0 },
                );
            }
        }

        let out_len = core::mem::size_of::<VirtioGpuResourceCreateBlob>()
            + pages.len() * core::mem::size_of::<VirtioGpuMemEntry>();
        if out_len > cmd_buf.len() {
            crate::dprintln!("[VGPU] create_blob({}): overflow", resource_id);
            return false;
        }

        let cmd_phys = cmd_buf.phys().0;

        let resp_buf = self.resp_buf.as_ref().unwrap();
        let queue = self.ctrl_queue.as_mut().unwrap();
        let parts: [(u64, u32, u16); 2] = [
            (cmd_phys, out_len as u32, 0),
            (resp_buf.phys().0, resp_buf.len() as u32, 2),
        ];
        if queue.push(&parts).is_err() {
            crate::dprintln!("[VGPU] create_blob({}): push failed", resource_id);
            return false;
        }
        if queue.wait().is_err() {
            crate::dprintln!("[VGPU] create_blob({}): wait failed", resource_id);
            return false;
        }
        let resp = unsafe { read_volatile(resp_buf.as_mut_ptr() as *const VirtioGpuCtrlHdr) };
        if classify_response(resp.hdr_type) != RespClass::OkNoData {
            crate::dprintln!("[VGPU] create_blob({}) resp={:#x}", resource_id, resp.hdr_type);
            return false;
        }
        true
    }

    /// Suballocates `size` (page-aligned) from the hostmem window for HOST3D
    /// blobs. The guest chooses offsets; the host maps the blob there on
    /// RESOURCE_MAP_BLOB. Returns the window offset, or None when exhausted.
    pub fn hostmem_alloc(&mut self, size: u64) -> Option<u64> {
        if self.hostmem_base.is_none() || self.hostmem_len == 0 {
            return None;
        }
        let aligned = size.div_ceil(4096) * 4096;
        let off = self.hostmem_next;
        let end = off.checked_add(aligned)?;
        if end > self.hostmem_len as u64 {
            return None;
        }
        self.hostmem_next = end;
        Some(off)
    }

    /// Maps a host blob into the hostmem window at `offset` (RESOURCE_MAP_BLOB).
    pub fn map_blob(&mut self, ctx_id: u32, resource_id: u32, offset: u64) -> bool {
        self.ensure_bufs(core::mem::size_of::<VirtioGpuResourceMapBlob>(), 64);
        let mut cmd = VirtioGpuResourceMapBlob::default();
        cmd.hdr.hdr_type = VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB;
        cmd.hdr.ctx_id = ctx_id;
        cmd.resource_id = resource_id;
        cmd.offset = offset;

        {
            let cmd_buf = match self.cmd_buf.as_ref() {
                Some(b) => b,
                None => return false,
            };
            unsafe {
                core::ptr::write_bytes(cmd_buf.as_mut_ptr(), 0, cmd_buf.len());
                core::ptr::copy_nonoverlapping(
                    &cmd as *const _ as *const u8,
                    cmd_buf.as_mut_ptr(),
                    core::mem::size_of::<VirtioGpuResourceMapBlob>(),
                );
            }
        }

        match self.send_command_raw() {
            Some(resp) => {
                // OK_MAP_INFO (0x1106) on success.
                if resp.hdr_type != 0x1106 {
                    crate::dprintln!("[VGPU] map_blob({}) resp={:#x}", resource_id, resp.hdr_type);
                    return false;
                }
                true
            }
            None => {
                crate::dprintln!("[VGPU] map_blob({}): queue failed", resource_id);
                false
            }
        }
    }

    /// Sets scanout from a blob resource with explicit framebuffer metadata.
    pub fn set_scanout_blob(&mut self, scanout_id: u32, resource_id: u32, rect: VirtioGpuRect, width: u32, height: u32, format: u32, pitch: u32) {
        self.ensure_bufs(256, 64);

        let mut cmd = VirtioGpuSetScanoutBlob::default();
        cmd.hdr.hdr_type = VIRTIO_GPU_CMD_SET_SCANOUT_BLOB;
        cmd.r = rect;
        cmd.scanout_id = scanout_id;
        cmd.resource_id = resource_id;
        cmd.width = width;
        cmd.height = height;
        cmd.format = format;
        cmd.strides[0] = pitch;

        let cmd_buf = self.cmd_buf.as_ref().unwrap();
        unsafe {
            core::ptr::write_bytes(cmd_buf.as_mut_ptr(), 0, cmd_buf.len());
            core::ptr::copy_nonoverlapping(
                &cmd as *const _ as *const u8,
                cmd_buf.as_mut_ptr(),
                core::mem::size_of::<VirtioGpuSetScanoutBlob>(),
            );
        }

        if let Some(resp) = self.send_command_raw() {
            if classify_response(resp.hdr_type) != RespClass::OkNoData {
                crate::dprintln!(
                    "[VGPU] set_scanout_blob({},{}) resp={:#x}",
                    scanout_id, resource_id, resp.hdr_type
                );
            }
        } else {
            crate::dprintln!("[VGPU] set_scanout_blob: queue push/wait failed");
        }
    }

    /// Associates a resource with a scanout output for display.
    ///
    /// Returns false if the host rejected the request (or the command never
    /// completed). Callers on the dumb-buffer path use this to avoid reporting
    /// a successful page flip for a scanout the host refused.
    pub fn set_scanout(&mut self, scanout_id: u32, resource_id: u32, rect: VirtioGpuRect) -> bool {
        // The scanout is moving to `resource_id`, so anything that was only
        // being held back because it *was* the scanout can be released now.
        self.reap_deferred_unrefs(resource_id);
        self.ensure_bufs(64, 64);

        let mut cmd = VirtioGpuSetScanout::default();
        cmd.hdr.hdr_type = VIRTIO_GPU_CMD_SET_SCANOUT;
        cmd.r = rect;
        cmd.scanout_id = scanout_id;
        cmd.resource_id = resource_id;

        let cmd_buf = self.cmd_buf.as_ref().unwrap();
        unsafe {
            core::ptr::write_bytes(cmd_buf.as_mut_ptr(), 0, cmd_buf.len());
            core::ptr::copy_nonoverlapping(
                &cmd as *const _ as *const u8,
                cmd_buf.as_mut_ptr(),
                core::mem::size_of::<VirtioGpuSetScanout>(),
            );
        }

        match self.send_command_raw() {
            Some(resp) if classify_response(resp.hdr_type) == RespClass::OkNoData => true,
            Some(resp) => {
                crate::dprintln!(
                    "[VGPU] set_scanout(sc={},rid={}) host error {:#x} rect={}x{}",
                    scanout_id, resource_id, resp.hdr_type, rect.width, rect.height
                );
                false
            }
            None => {
                crate::dprintln!("[VGPU] set_scanout: queue push/wait failed");
                false
            }
        }
    }

    /// Flushes modified rectangular regions of a resource to the host display.
    ///
    /// Returns false if the host rejected the flush.
    /// Flush a resource, optionally asking the host to signal `fence_id` when the
    /// flush has actually completed.
    ///
    /// Linux's `virtio_gpu_resource_flush` (virtgpu_plane.c:199-232) passes
    /// `vgplane_st->fence` and then waits on it with
    /// `dma_fence_wait_timeout(..., msecs_to_jiffies(50))`. The wait is what
    /// makes the flip meaningful: without it the guest learns only that the
    /// host *took* the command, not that the blit to the display has finished.
    ///
    /// Returns `(accepted, signalled_fence_id)`. `signalled` is `Some` only when
    /// the host set VIRTIO_GPU_FLAG_FENCE, so the caller can tell "the host said
    /// it finished" from "the host consumed the command".
    pub fn flush_resource_fenced(
        &mut self,
        resource_id: u32,
        rect: VirtioGpuRect,
        fence_id: Option<u64>,
    ) -> (bool, Option<u64>) {
        self.ensure_bufs(64, 64);

        let mut cmd = VirtioGpuResourceFlush::default();
        cmd.hdr.hdr_type = VIRTIO_GPU_CMD_RESOURCE_FLUSH;
        cmd.r = rect;
        cmd.resource_id = resource_id;
        if let Some(fid) = fence_id {
            cmd.hdr.flags = VIRTIO_GPU_FLAG_FENCE;
            cmd.hdr.fence_id = fid;
        }

        let cmd_buf = self.cmd_buf.as_ref().unwrap();
        unsafe {
            core::ptr::write_bytes(cmd_buf.as_mut_ptr(), 0, cmd_buf.len());
            core::ptr::copy_nonoverlapping(
                &cmd as *const _ as *const u8,
                cmd_buf.as_mut_ptr(),
                core::mem::size_of::<VirtioGpuResourceFlush>(),
            );
        }

        // Route through send_command_raw so the response is read once; report
        // the signalled fence id back to the caller.
        match self.send_command_raw() {
            Some(r) => (
                classify_response(r.hdr_type) == RespClass::OkNoData,
                if r.flags & VIRTIO_GPU_FLAG_FENCE != 0 { Some(r.fence_id) } else { None },
            ),
            None => (false, None),
        }
    }

    /// Reap one completed control-queue command without blocking.
    ///
    /// The completion half of the async path. Linux reaps in
    /// `virtio_gpu_dequeue_ctrl_func`, walking a `reclaim_list` and invoking
    /// each entry's `resp_cb`; our `wait_for` spins in the caller's context
    /// instead, which is why `virtgpu_vq::D3` (busy-wait with interrupts
    /// masked) is still open.
    ///
    /// This does the part that does not need a worker thread: if a command has
    /// already completed, consume its used-ring entry, raise its fence, and
    /// report the id. Returns `None` when nothing is ready, so a caller can
    /// poll from a timer tick without ever blocking.
    ///
    /// Must be called with `GLOBAL_VIRTIO_GPU` **not** held: it takes the
    /// queue lock and then calls out to the fence layer.
    /// Drain **every** retired head and return the fence ids the host sent.
    ///
    /// The completion wakeup mechanism. Linux reaps in
    /// `virtio_gpu_dequeue_ctrl_func` on a workqueue; we have none, so this is
    /// driven opportunistically -- from `submit_3d`/`submit_3d_async` before a
    /// head is allocated, with the timer tick as a backstop.
    ///
    /// Reap-on-submit is sufficient because a client submits back to back: each
    /// submit first collects whatever the host has already finished, so slots
    /// are recycled at the rate commands complete rather than at some polling
    /// interval. That matters because the timer tick is ~10s, and polling at
    /// that interval cannot support async submission at all (§54): the 16 slots
    /// were exhausted in about a second of rendering.
    ///
    /// Draining *all* retired heads (not just the first) is what keeps the pool
    /// from accumulating; returning after the first would free one slot per
    /// submit and no better.
    ///
    /// Must be called with `GLOBAL_VIRTIO_GPU` not held by the caller beyond
    /// this borrow, because raising fences reaches userspace eventfds.
    /// Drain every retired head, collecting the fence ids the host returned and
    /// the contexts those heads belonged to.
    ///
    /// The context list is what unblocks deferred submits: a context is only
    /// safe to submit to again once its previous submit has actually retired.
    pub fn reap_all_completions(
        &mut self,
        out: &mut alloc::vec::Vec<u64>,
        out_ctx: &mut alloc::vec::Vec<u32>,
    ) {
        for head in 0..crate::drivers::virtio::MAX_INFLIGHT as u16 {
            let retired = match self.ctrl_queue.as_mut() {
                Some(q) => q.try_retire(head),
                None => return,
            };
            if !retired {
                continue;
            }
            // Read the fence from THIS head's private response buffer. Reading
            // the shared one would report whichever command last used it, which
            // after the per-head pool is never the one that just completed.
            let (fid, hdr) = {
                match self.slot_bufs[head as usize].as_ref() {
                    Some(s) => {
                        let r = unsafe {
                            read_volatile(s.resp.virt_addr().0 as *const VirtioGpuCtrlHdr)
                        };
                        (r.fence_id, r.hdr_type)
                    }
                    None => continue,
                }
            };
            if hdr < VIRTIO_GPU_RESP_ERR_UNSPEC && fid != 0 {
                out.push(fid);
            }
            // Retired either way: a host error still consumed the descriptor, so
            // leaving it counted would strand the counter above zero forever and
            // make every later tick take the slow path for nothing.
            out_ctx.push(self.head_ctx[head as usize]);
            self.head_ctx[head as usize] = 0;
            INFLIGHT.fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Flush without a fence. Kept for the 2D path, where the transfer is
    /// synchronous and there is nothing to wait for.
    pub fn flush_resource(&mut self, resource_id: u32, rect: VirtioGpuRect) -> bool {
        self.flush_resource_fenced(resource_id, rect, None).0
    }

    /// Transfers guest backing store updates to the host 2D resource.
    ///
    /// Returns false if the host rejected the transfer.
    pub fn transfer_to_host_2d(&mut self, resource_id: u32, rect: VirtioGpuRect, offset: u64) -> bool {
        self.ensure_bufs(64, 64);

        let mut cmd = VirtioGpuTransferToHost2d::default();
        cmd.hdr.hdr_type = VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D;
        cmd.r = rect;
        cmd.offset = offset;
        cmd.resource_id = resource_id;

        let cmd_buf = self.cmd_buf.as_ref().unwrap();
        unsafe {
            core::ptr::write_bytes(cmd_buf.as_mut_ptr(), 0, cmd_buf.len());
            core::ptr::copy_nonoverlapping(
                &cmd as *const _ as *const u8,
                cmd_buf.as_mut_ptr(),
                core::mem::size_of::<VirtioGpuTransferToHost2d>(),
            );
        }

        self.send_command_raw()
            .map(|r| classify_response(r.hdr_type) == RespClass::OkNoData)
            .unwrap_or(false)
    }

    /// Allocates a new resource ID (monotonic).
    /// Return the private buffer pair for descriptor `head`, provisioning it on
    /// first use. Returns `None` only if the DMA allocator refuses, in which
    /// case the caller must not queue.
    pub fn slot_bufs(&mut self, head: u16, cmd_len: usize, resp_len: usize) -> Option<(u64, u64)> {
        let i = head as usize;
        if i >= crate::drivers::virtio::MAX_INFLIGHT {
            return None;
        }
        if self.slot_bufs[i].is_none() {
            self.slot_bufs[i] = Some(SlotBuf {
                cmd: DmaBuf::new(cmd_len.max(64))?,
                resp: DmaBuf::new(resp_len.max(64))?,
            });
        }
        let s = self.slot_bufs[i].as_ref()?;
        Some((s.cmd.phys().0, s.resp.phys().0))
    }

    pub fn alloc_resource_id(&mut self) -> u32 {
        let id = self.resource_id_counter;
        self.resource_id_counter += 1;
        id
    }

    /// Detach backing and unref resource on host.
    /// Release a resource's backing and drop the guest's reference.
    ///
    /// Deferred while the resource is still the one being scanned out.
    ///
    /// Unreffing (and especially detaching the backing of) a resource that the
    /// host is *currently scanning out* leaves the QEMU frontend holding a
    /// reference to a destroyed virglrenderer resource — the host then samples
    /// a stale texture. That is a known failure mode on this path, and with
    /// double buffering (rids alternating 102 <-> 111) it is trivially
    /// reachable: close whichever buffer happens to be on screen.
    ///
    /// Note the ordering hazard independently of the unref: DETACH_BACKING is
    /// sent *before* UNREF, so tearing down a live scanout resource leaves the
    /// scanout pointing at a resource with no backing at all.
    ///
    /// Linux refuses to free a resource that is still scanned out and reclaims
    /// it later, once the plane points elsewhere. That is what this does:
    /// queue the resource, and reap it from `set_scanout` at the moment the
    /// scanout actually moves. Leaking is the safe direction; handing the host
    /// a dangling texture is not.
    pub fn detach_backing_and_unref(&mut self, resource_id: u32) {
        let current = crate::drivers::drm::HOST_SCANOUT.load(Ordering::Relaxed) >> 32;
        if current == resource_id as u64 {
            let mut q = PENDING_UNREF.lock();
            if !q.contains(&resource_id) {
                q.push(resource_id);
            }
            crate::dprintln!(
                "[unref] rid={} DEFERRED (still current scanout; {} pending)",
                resource_id,
                q.len()
            );
            return;
        }
        self.detach_and_unref_now(resource_id);
    }

    /// Reap resources whose unref was deferred while they were scanned out.
    fn reap_deferred_unrefs(&mut self, new_scanout_rid: u32) {
        let ready: alloc::vec::Vec<u32> = {
            let mut q = PENDING_UNREF.lock();
            let ready: alloc::vec::Vec<u32> = q
                .iter()
                .copied()
                .filter(|r| *r != new_scanout_rid)
                .collect();
            q.retain(|r| *r == new_scanout_rid);
            ready
        };
        for r in ready {
            crate::dprintln!("[unref] rid={} reaped (scanout moved to {})", r, new_scanout_rid);
            self.detach_and_unref_now(r);
        }
    }

    fn detach_and_unref_now(&mut self, resource_id: u32) {
        // DETACH_BACKING
        let mut cmd = VirtioGpuCtrlHdr { hdr_type: VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING, flags: 0, fence_id: 0, ctx_id: 0, ring_idx: 0, padding: [0; 3] };
        // Reuse cmd_buf for simple header
        self.ensure_bufs(64, 64);
        let cmd_buf = self.cmd_buf.as_ref().unwrap();
        unsafe {
            core::ptr::write_bytes(cmd_buf.as_mut_ptr(), 0, cmd_buf.len());
            core::ptr::copy_nonoverlapping(&cmd as *const _ as *const u8, cmd_buf.as_mut_ptr(), core::mem::size_of::<VirtioGpuCtrlHdr>());
            // append resource_id after header (driver expects header + u32)
            core::ptr::write_unaligned(cmd_buf.as_mut_ptr().add(core::mem::size_of::<VirtioGpuCtrlHdr>()) as *mut u32, resource_id);
        }
        let _ = self.send_command_raw();
        // UNREF
        let mut cmd2 = VirtioGpuResourceUnref { hdr: VirtioGpuCtrlHdr { hdr_type: VIRTIO_GPU_CMD_RESOURCE_UNREF, ..Default::default() }, resource_id, _pad: 0 };
        let cmd_buf2 = self.cmd_buf.as_ref().unwrap();
        unsafe {
            core::ptr::write_bytes(cmd_buf2.as_mut_ptr(), 0, cmd_buf2.len());
            core::ptr::copy_nonoverlapping(&cmd2 as *const _ as *const u8, cmd_buf2.as_mut_ptr(), core::mem::size_of::<VirtioGpuResourceUnref>());
        }
        let _ = self.send_command_raw();
    }
}

// ============================================================================
// DRM/KMS Backend Trait Implementation
// ============================================================================

impl KmsBackend for VirtioGpuDriver {
    fn set_scanout(&self, _fb_phys: u64, _width: u32, _height: u32, _pitch: u32) -> bool {
        // Stub: this backend never programs a scanout, so report failure
        // rather than a flip that never happened. VirtioGpuBackend is the
        // implementation actually installed as KMS_BACKEND.
        false
    }

    fn page_flip(&self, _fb_phys: u64) -> bool {
        false
    }

    fn get_modes(&self) -> Vec<DrmModeResolution> {
        let mut modes = Vec::new();
        let w = if self.width > 0 { self.width } else { 1024 };
        let h = if self.height > 0 { self.height } else { 768 };
        modes.push(DrmModeResolution {
            width: w,
            height: h,
            refresh_hz: 60,
            clock_khz: 148500,
            preferred: true,
        });
        modes
    }
}

// ============================================================================
// Global Driver Instance
// ============================================================================

/// Global singleton instance of the VirtIO-GPU driver.
pub static GLOBAL_VIRTIO_GPU: SpinLock<Option<VirtioGpuDriver>> = SpinLock::new(None);

/// Initializes the global VirtIO-GPU driver by probing PCI for the device.
/// Returns true if a GPU was found and initialized.
pub fn init() -> bool {
    if let Some(addr) = find_device(VIRTIO_GPU_VENDOR_ID, VIRTIO_GPU_DEVICE_ID) {
        if let Some(driver) = VirtioGpuDriver::probe(addr) {
            *GLOBAL_VIRTIO_GPU.lock() = Some(driver);
            return true;
        }
    }
    false
}

impl VirtioGpuDriver {
    /// Type field of the last command response (diagnostics).
    pub fn last_resp_type(&self) -> u32 {
        match self.resp_buf.as_ref() {
            Some(rb) => unsafe { core::ptr::read_volatile(rb.as_mut_ptr() as *const u32) },
            None => 0xFFFF_FFFF,
        }
    }

    /// Sends a command on the cursor virtqueue and waits for its response.
    fn send_cursor_command(&mut self, cmd: &VirtioGpuUpdateCursor) -> Option<VirtioGpuCtrlHdr> {
        self.ensure_bufs(core::mem::size_of::<VirtioGpuUpdateCursor>(), 64);
        let queue = self.cursor_queue.as_mut()?;
        let cmd_buf = self.cmd_buf.as_mut()?;
        let resp_buf = self.resp_buf.as_mut()?;

        unsafe {
            core::ptr::write_bytes(cmd_buf.as_mut_ptr(), 0, cmd_buf.len());
            core::ptr::copy_nonoverlapping(
                cmd as *const _ as *const u8,
                cmd_buf.as_mut_ptr(),
                core::mem::size_of::<VirtioGpuUpdateCursor>(),
            );
        }

        let parts: [(u64, u32, u16); 2] = [
            (cmd_buf.phys().0, core::mem::size_of::<VirtioGpuUpdateCursor>() as u32, 0),
            (resp_buf.phys().0, resp_buf.len() as u32, 2),
        ];
        queue.push(&parts).ok()?;
        queue.wait().ok()?;
        Some(unsafe { read_volatile(resp_buf.as_mut_ptr() as *const VirtioGpuCtrlHdr) })
    }

    /// Shows/updates the hardware cursor: binds `resource_id` to `scanout_id`
    /// and positions it at (x, y). resource_id=0 hides the cursor.
    pub fn update_cursor(&mut self, resource_id: u32, scanout_id: u32, x: u32, y: u32) -> bool {
        if self.cursor_queue.is_none() {
            return false;
        }
        let cmd = VirtioGpuUpdateCursor {
            hdr: VirtioGpuCtrlHdr { hdr_type: VIRTIO_GPU_CMD_UPDATE_CURSOR, ..Default::default() },
            // `scanout_id` belongs to the position, and the hotspot is the
            // cursor's active pixel. Both are read by the host; the previous
            // layout put scanout_id where hot_x belongs and sent 8 bytes too
            // few, so the host decoded every cursor update from misaligned
            // fields.
            pos: VirtioGpuPosition { scanout_id, x, y, _pad: 0 },
            resource_id,
            hot_x: 0,
            hot_y: 0,
            _pad: 0,
        };
        match self.send_cursor_command(&cmd) {
            Some(r) if classify_response(r.hdr_type) == RespClass::OkNoData => true,
            Some(r) => {
                crate::dprintln!("[VGPU] update_cursor(rid={}) resp={:#x}", resource_id, r.hdr_type);
                false
            }
            None => {
                crate::dprintln!("[VGPU] update_cursor: cursor queue failed");
                false
            }
        }
    }

    /// Moves an already-bound hardware cursor to (x, y).
    pub fn move_cursor(&mut self, scanout_id: u32, x: u32, y: u32) -> bool {
        if self.cursor_queue.is_none() {
            return false;
        }
        let cmd = VirtioGpuUpdateCursor {
            hdr: VirtioGpuCtrlHdr { hdr_type: VIRTIO_GPU_CMD_MOVE_CURSOR, ..Default::default() },
            // MOVE_CURSOR carries no resource: the host keeps the image bound
            // by the previous UPDATE_CURSOR and only the position travels.
            pos: VirtioGpuPosition { scanout_id, x, y, _pad: 0 },
            resource_id: 0,
            hot_x: 0,
            hot_y: 0,
            _pad: 0,
        };
        matches!(self.send_cursor_command(&cmd),
            Some(r) if classify_response(r.hdr_type) == RespClass::OkNoData)
    }

    /// Fetches a connected display's EDID block into `out`.
    /// Returns the number of valid EDID bytes written.
    pub fn get_edid(&mut self, scanout_id: u32, out: &mut [u8; 1024]) -> Option<usize> {
        if !self.has_edid {
            return None;
        }
        self.ensure_bufs(core::mem::size_of::<VirtioGpuGetEdid>(), 64);
        let mut resp_dma = DmaBuf::new(core::mem::size_of::<VirtioGpuRespEdid>())?;

        let mut cmd = VirtioGpuGetEdid::default();
        cmd.hdr.hdr_type = VIRTIO_GPU_CMD_GET_EDID;
        cmd.scanout_id = scanout_id;

        {
            let cmd_buf = self.cmd_buf.as_ref()?;
            unsafe {
                core::ptr::write_bytes(cmd_buf.as_mut_ptr(), 0, cmd_buf.len());
                core::ptr::copy_nonoverlapping(
                    &cmd as *const _ as *const u8,
                    cmd_buf.as_mut_ptr(),
                    core::mem::size_of::<VirtioGpuGetEdid>(),
                );
            }
        }
        let cmd_buf = self.cmd_buf.as_ref()?;
        let queue = self.ctrl_queue.as_mut()?;
        let parts: [(u64, u32, u16); 2] = [
            (cmd_buf.phys().0, core::mem::size_of::<VirtioGpuGetEdid>() as u32, 0),
            (resp_dma.phys().0, core::mem::size_of::<VirtioGpuRespEdid>() as u32, 2),
        ];
        queue.push(&parts).ok()?;
        queue.wait().ok()?;

        // SAFETY: device wrote the response chain before wait() returned.
        let resp = unsafe { &*(resp_dma.as_mut_ptr() as *const VirtioGpuRespEdid) };
        if resp.hdr.hdr_type != VIRTIO_GPU_RESP_OK_EDID {
            crate::dprintln!("[VGPU] get_edid({}) resp={:#x}", scanout_id, resp.hdr.hdr_type);
            return None;
        }
        let n = (resp.size as usize).min(out.len()).min(resp.edid.len());
        out[..n].copy_from_slice(&resp.edid[..n]);
        Some(n)
    }

    /// Queries capset metadata (virgl/venus capability reports).
    /// Returns `(capset_id, capset_version, capset_size)` for capset `index`.
    pub fn get_capset_info(&mut self, index: u32) -> Option<(u32, u32, u32)> {
        // Bound the guest-supplied index before it is used for anything. Our
        // caller happens to pass a bounded `0..8` loop today, but the API takes
        // a raw `u32` and `virtgpu_vq.rs` documents exactly this shape as "an
        // out-of-bounds write waiting for a guest that asks for capset 99".
        // Enforcing it here means the guarantee does not depend on every caller
        // getting it right.
        if index >= CAPSET_PROBE_LIMIT {
            crate::dprintln!(
                "[VGPU] get_capset_info: index {} out of range (limit {})",
                index, CAPSET_PROBE_LIMIT
            );
            return None;
        }
        self.ensure_bufs(
            core::mem::size_of::<VirtioGpuGetCapsetInfo>(),
            core::mem::size_of::<VirtioGpuRespCapsetInfo>(),
        );

        let mut cmd = VirtioGpuGetCapsetInfo::default();
        cmd.hdr.hdr_type = VIRTIO_GPU_CMD_GET_CAPSET_INFO;
        cmd.capset_index = index;

        {
            let cmd_buf = self.cmd_buf.as_ref()?;
            unsafe {
                core::ptr::write_bytes(cmd_buf.as_mut_ptr(), 0, cmd_buf.len());
                core::ptr::copy_nonoverlapping(
                    &cmd as *const _ as *const u8,
                    cmd_buf.as_mut_ptr(),
                    core::mem::size_of::<VirtioGpuGetCapsetInfo>(),
                );
            }
        }

        let resp = self.send_command_raw()?;
        if resp.hdr_type != VIRTIO_GPU_RESP_OK_CAPSET_INFO {
            crate::dprintln!(
                "[VGPU] get_capset_info({}) resp={:#x} flags={:#x} fence={} ctx={}",
                index, resp.hdr_type, resp.flags, resp.fence_id, resp.ctx_id
            );
            return None;
        }
        let resp_buf = self.resp_buf.as_ref()?;
        // SAFETY: response written before send_command_raw returned.
        let info = unsafe { &*(resp_buf.as_mut_ptr() as *const VirtioGpuRespCapsetInfo) };
        Some((info.capset_id, info.capset_version, info.capset_size))
    }

    /// Fetches a capset's raw data block into `out`.
    /// Returns the number of valid bytes written.
    pub fn get_capset(&mut self, capset_id: u32, version: u32, host_max_size: u32, out: &mut [u8]) -> Option<usize> {
        let cmd_len = core::mem::size_of::<VirtioGpuGetCapset>();
        let resp_len = core::mem::size_of::<VirtioGpuCtrlHdr>() + out.len();
        self.ensure_bufs(cmd_len, resp_len);

        let mut cmd = VirtioGpuGetCapset::default();
        cmd.hdr.hdr_type = VIRTIO_GPU_CMD_GET_CAPSET;
        cmd.capset_id = capset_id;
        cmd.capset_version = version;

        let cmd_buf = self.cmd_buf.as_ref()?;
        unsafe {
            core::ptr::write_bytes(cmd_buf.as_mut_ptr(), 0, cmd_buf.len());
            core::ptr::copy_nonoverlapping(
                &cmd as *const _ as *const u8,
                cmd_buf.as_mut_ptr(),
                cmd_len,
            );
        }

        let resp = self.send_command_raw()?;
        if resp.hdr_type != VIRTIO_GPU_RESP_OK_CAPSET {
            crate::dprintln!("[VGPU] get_capset(id={}, ver={}) resp={:#x}", capset_id, version, resp.hdr_type);
            return None;
        }
        let resp_buf = self.resp_buf.as_ref()?;
        let hdr_size = core::mem::size_of::<VirtioGpuCtrlHdr>();
        // Clamp to the host's advertised `max_size`, obtained from
        // GET_CAPSET_INFO by the caller.
        //
        // The capset reply carries no length field, so this is the only correct
        // bound. The previous code used
        // `resp_buf.len().saturating_sub(hdr_size).min(out.len())`, which is an
        // identity rather than a clamp -- `ensure_bufs` only grows, so the
        // response buffer is always at least `hdr + out.len()` and the second
        // term always wins. Everything the host did not write this time was
        // stale bytes from a previous reply, presented to the client as current
        // capability data.
        //
        // Linux does the same clamp against max_size (virtgpu_ioctl.c:450-452).
        let available = resp_buf.len().saturating_sub(hdr_size);
        let mut data_size = (host_max_size as usize).min(out.len()).min(available);
        if host_max_size as usize > available {
            crate::dprintln!(
                "[VGPU] get_capset(id={}, ver={}) host max_size {} exceeds buffer {}, clamping to {}",
                capset_id, version, host_max_size, available, data_size
            );
        }
        unsafe {
            core::ptr::copy_nonoverlapping(
                resp_buf.as_mut_ptr().add(hdr_size),
                out.as_mut_ptr(),
                data_size,
            );
        }
        Some(data_size)
    }

    // ========================================================================
    // VirtIO-GPU 3D Acceleration Interface (VirGL / Venus)
    // ========================================================================

    /// Creates a 3D rendering context on the host GPU.
    pub fn ctx_create(&mut self, ctx_id: u32, context_init: u32, debug_name: &str) -> bool {
        self.ensure_bufs(core::mem::size_of::<VirtioGpuCtxCreate>(), 64);
        let mut cmd = VirtioGpuCtxCreate::default();
        cmd.hdr.hdr_type = VIRTIO_GPU_CMD_CTX_CREATE;
        cmd.hdr.ctx_id = ctx_id;
        let bytes = debug_name.as_bytes();
        let len = bytes.len().min(63);
        cmd.nlen = len as u32;
        cmd.context_init = context_init;
        cmd.debug_name[..len].copy_from_slice(&bytes[..len]);

        let cmd_buf = match self.cmd_buf.as_ref() {
            Some(b) => b,
            None => return false,
        };
        unsafe {
            core::ptr::write_bytes(cmd_buf.as_mut_ptr(), 0, cmd_buf.len());
            core::ptr::copy_nonoverlapping(
                &cmd as *const _ as *const u8,
                cmd_buf.as_mut_ptr(),
                core::mem::size_of::<VirtioGpuCtxCreate>(),
            );
        }

        match self.send_command_raw() {
            Some(resp) => classify_response(resp.hdr_type) == RespClass::OkNoData,
            None => false,
        }
    }

    /// Destroys a 3D rendering context.
    pub fn ctx_destroy(&mut self, ctx_id: u32) -> bool {
        self.ensure_bufs(core::mem::size_of::<VirtioGpuCtxDestroy>(), 64);
        let mut cmd = VirtioGpuCtxDestroy::default();
        cmd.hdr.hdr_type = VIRTIO_GPU_CMD_CTX_DESTROY;
        cmd.hdr.ctx_id = ctx_id;

        let cmd_buf = match self.cmd_buf.as_ref() {
            Some(b) => b,
            None => return false,
        };
        unsafe {
            core::ptr::write_bytes(cmd_buf.as_mut_ptr(), 0, cmd_buf.len());
            core::ptr::copy_nonoverlapping(
                &cmd as *const _ as *const u8,
                cmd_buf.as_mut_ptr(),
                core::mem::size_of::<VirtioGpuCtxDestroy>(),
            );
        }

        match self.send_command_raw() {
            Some(resp) => classify_response(resp.hdr_type) == RespClass::OkNoData,
            None => false,
        }
    }

    /// Attaches a resource to a 3D rendering context.
    pub fn ctx_attach_resource(&mut self, ctx_id: u32, resource_id: u32) -> bool {
        self.ensure_bufs(core::mem::size_of::<VirtioGpuCtxResource>(), 64);
        let mut cmd = VirtioGpuCtxResource::default();
        cmd.hdr.hdr_type = VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE;
        cmd.hdr.ctx_id = ctx_id;
        cmd.resource_id = resource_id;

        let cmd_buf = match self.cmd_buf.as_ref() {
            Some(b) => b,
            None => return false,
        };
        unsafe {
            core::ptr::write_bytes(cmd_buf.as_mut_ptr(), 0, cmd_buf.len());
            core::ptr::copy_nonoverlapping(
                &cmd as *const _ as *const u8,
                cmd_buf.as_mut_ptr(),
                core::mem::size_of::<VirtioGpuCtxResource>(),
            );
        }

        match self.send_command_raw() {
            Some(resp) => classify_response(resp.hdr_type) == RespClass::OkNoData,
            None => false,
        }
    }

    /// Detaches a resource from a 3D rendering context.
    pub fn ctx_detach_resource(&mut self, ctx_id: u32, resource_id: u32) -> bool {
        self.ensure_bufs(core::mem::size_of::<VirtioGpuCtxResource>(), 64);
        let mut cmd = VirtioGpuCtxResource::default();
        cmd.hdr.hdr_type = VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE;
        cmd.hdr.ctx_id = ctx_id;
        cmd.resource_id = resource_id;

        let cmd_buf = match self.cmd_buf.as_ref() {
            Some(b) => b,
            None => return false,
        };
        unsafe {
            core::ptr::write_bytes(cmd_buf.as_mut_ptr(), 0, cmd_buf.len());
            core::ptr::copy_nonoverlapping(
                &cmd as *const _ as *const u8,
                cmd_buf.as_mut_ptr(),
                core::mem::size_of::<VirtioGpuCtxResource>(),
            );
        }

        match self.send_command_raw() {
            Some(resp) => classify_response(resp.hdr_type) == RespClass::OkNoData,
            None => false,
        }
    }

    /// Allocates a 3D resource (texture, buffer, surface) on the host GPU.
    pub fn resource_create_3d(&mut self, res: &VirtioGpuResourceCreate3d) -> bool {
        self.ensure_bufs(core::mem::size_of::<VirtioGpuResourceCreate3d>(), 64);
        let mut cmd = *res;
        cmd.hdr.hdr_type = VIRTIO_GPU_CMD_RESOURCE_CREATE_3D;

        let cmd_buf = match self.cmd_buf.as_ref() {
            Some(b) => b,
            None => return false,
        };
        unsafe {
            core::ptr::write_bytes(cmd_buf.as_mut_ptr(), 0, cmd_buf.len());
            core::ptr::copy_nonoverlapping(
                &cmd as *const _ as *const u8,
                cmd_buf.as_mut_ptr(),
                core::mem::size_of::<VirtioGpuResourceCreate3d>(),
            );
        }

        match self.send_command_raw() {
            Some(resp) => classify_response(resp.hdr_type) == RespClass::OkNoData,
            None => false,
        }
    }

    /// Transfers data from guest backing memory to a 3D host resource.
    pub fn transfer_to_host_3d(
        &mut self,
        resource_id: u32,
        box_: VirtioGpuBox,
        offset: u64,
        level: u32,
        stride: u32,
        layer_stride: u32,
    ) -> bool {
        self.ensure_bufs(core::mem::size_of::<VirtioGpuTransferHost3d>(), 64);
        let mut cmd = VirtioGpuTransferHost3d::default();
        cmd.hdr.hdr_type = VIRTIO_GPU_CMD_TRANSFER_TO_HOST_3D;
        cmd.resource_id = resource_id;
        cmd.box_ = box_;
        cmd.offset = offset;
        cmd.level = level;
        cmd.stride = stride;
        cmd.layer_stride = layer_stride;

        let cmd_buf = match self.cmd_buf.as_ref() {
            Some(b) => b,
            None => return false,
        };
        unsafe {
            core::ptr::write_bytes(cmd_buf.as_mut_ptr(), 0, cmd_buf.len());
            core::ptr::copy_nonoverlapping(
                &cmd as *const _ as *const u8,
                cmd_buf.as_mut_ptr(),
                core::mem::size_of::<VirtioGpuTransferHost3d>(),
            );
        }

        match self.send_command_raw() {
            Some(resp) => classify_response(resp.hdr_type) == RespClass::OkNoData,
            None => false,
        }
    }

    /// Transfers data from a 3D host resource to guest memory.
    /// Read back `h` full rows of `w` pixels starting at `(x, y)` into
    /// `probe_buf`, returning the buffer for inspection.
    ///
    /// TEMP-DIAG. Deliberately reads a whole ROW per command rather than
    /// scattered 1x1 pixels: one command yields 1280 pixels, which is enough to
    /// see a 64-byte (16px) horizontal repeat directly, whereas 1x1 sampling
    /// would need dozens of commands to say the same thing. Fewer commands also
    /// means less perturbation -- the previous full-frame probe was removed
    /// precisely because it changed the host's tex_id and blacked the screen.
    ///
    /// Only ever called on a couple of frames, after the flip has been
    /// programmed, and never on the ordering path.
    /// Read host texture rows back into guest memory. **Always returns `None`.**
///
/// This used to allocate a `probe_buf`, issue `TRANSFER_FROM_HOST_3D`, and
/// return the buffer's address for the caller to read. That never worked:
///
/// `TRANSFER_FROM_HOST_3D` has **no destination descriptor**. The command
/// carries a resource id, box, offset, stride and layer_stride, and the host
/// writes into *that resource's own attached iov* -- QEMU's virgl path calls
///
/// ```c
/// virgl_renderer_transfer_read_iov(tf3d.resource_id, tf3d.hdr.ctx_id,
///                                  tf3d.level, tf3d.stride,
///                                  tf3d.layer_stride, &tf3d.box,
///                                  tf3d.offset, NULL, 0);
/// ```
///
/// with a NULL destination iov (`hw/display/virtio-gpu-virgl.c`). `probe_buf`
/// was never placed in any descriptor chain, so every `[probe] f#… rep16=…`
/// line this emitted was a statistic over **uninitialised DMA**.
///
/// That matters beyond the wasted work: the docs cited `rep16` -- "the host row
/// repeats with a 16-px stride" -- as measured evidence for the shear. It was
/// never evidence of anything.
///
/// A working version would have to re-attach the resource's backing to the
/// probe buffer, read, then restore -- i.e. DETACH_BACKING + ATTACH_BACKING on
/// the resource that is currently the active scanout. That is exactly the
/// destructive operation previously recorded as changing the host's `tex_id`
/// and blackening the screen, so it cannot serve as the non-perturbing oracle
/// this was introduced to be.
///
/// The host texture layout is not observable from the guest. It needs the
/// §9.5 patched-QEMU measurement (log stride/tiling in
/// `virgl_cmd_resource_flush`) or `VIRGL_CAP_V2_SCANOUT_USES_GBM` plus a
/// host-side capture. Returning `None` is the honest answer; fabricating a
/// buffer address is not.
#[allow(unused_variables)]
pub fn probe_rows(
        &mut self,
        resource_id: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Option<(u64, usize)> {
        let _ = (resource_id, x, y, w, h);
        None
    }

    pub fn transfer_from_host_3d(
        &mut self,
        resource_id: u32,
        box_: VirtioGpuBox,
        offset: u64,
        level: u32,
        stride: u32,
        layer_stride: u32,
    ) -> bool {
        self.ensure_bufs(core::mem::size_of::<VirtioGpuTransferHost3d>(), 64);
        let mut cmd = VirtioGpuTransferHost3d::default();
        cmd.hdr.hdr_type = VIRTIO_GPU_CMD_TRANSFER_FROM_HOST_3D;
        cmd.resource_id = resource_id;
        cmd.box_ = box_;
        cmd.offset = offset;
        cmd.level = level;
        cmd.stride = stride;
        cmd.layer_stride = layer_stride;

        let cmd_buf = match self.cmd_buf.as_ref() {
            Some(b) => b,
            None => return false,
        };
        unsafe {
            core::ptr::write_bytes(cmd_buf.as_mut_ptr(), 0, cmd_buf.len());
            core::ptr::copy_nonoverlapping(
                &cmd as *const _ as *const u8,
                cmd_buf.as_mut_ptr(),
                core::mem::size_of::<VirtioGpuTransferHost3d>(),
            );
        }

        match self.send_command_raw() {
            Some(resp) => classify_response(resp.hdr_type) == RespClass::OkNoData,
            None => false,
        }
    }

    /// Submits a raw 3D command stream (VirGL Gallium / Venus Vulkan) to a rendering context.
    /// Submit a 3D command stream (VirGL Gallium / Venus bytecode) to the host GPU.
    /// Copies directly from the caller buffer into the DMA command buffer —
    /// one copy total (no intermediate heap Vec).
    /// SAFETY: caller must have validated [cmd_ptr, cmd_ptr+cmd_len) readable.
    /// Queue a 3D submit **without waiting**, as `virtio_gpu_submit` does
    /// (virtgpu_submit.c:353-359).
    ///
    /// `cmd_ptr` must be a KERNEL-readable buffer, not a user pointer. A
    /// deferred submit outlives the ioctl, so the command has to be copied
    /// before the client can unmap or rewrite it -- see the caller in
    /// `handle_virtgpu_execbuffer`.
    ///
    /// Returns the descriptor head so the caller can correlate completions.
    pub fn submit_3d_async(
        &mut self,
        ctx_id: u32,
        ring_idx: u8,
        cmd_ptr: u64,
        cmd_len: usize,
        fence_id: Option<u64>,
    ) -> Option<u16> {
        // The wire command is `virtio_gpu_cmd_submit`: ctrl_hdr + size + pad +
        // payload. `VirtioGpuCmdSubmit3d` is exactly that head (size_of == 32),
        // so it -- not the bare 24-byte ctrl_hdr -- is the header size used for
        // both `total_out` and the payload offset. Using the bare ctrl_hdr
        // dropped the 8-byte size/pad fields, so the host read the first
        // payload word as `size` and parsed an 8-byte-shifted command stream.
        let hdr_size = core::mem::size_of::<VirtioGpuCmdSubmit3d>();
        let total_out = hdr_size + cmd_len;

        let head = self.ctrl_queue.as_mut().and_then(|q| q.alloc_head())?;
        if self.slot_bufs(head, total_out, 64).is_none() {
            if let Some(q) = self.ctrl_queue.as_mut() {
                q.release_head(head);
            }
            return None;
        }

        let mut cmd = VirtioGpuCmdSubmit3d::default();
        cmd.hdr.hdr_type = VIRTIO_GPU_CMD_SUBMIT_3D;
        cmd.hdr.ctx_id = ctx_id;
        cmd.hdr.ring_idx = ring_idx;
        if let Some(fid) = fence_id {
            cmd.hdr.flags = VIRTIO_GPU_FLAG_FENCE;
            cmd.hdr.fence_id = fid;
        }
        cmd.size = cmd_len as u32;

        let (cmd_phys, resp_phys, resp_len) = {
            let slot = self.slot_bufs[head as usize].as_ref()?;
            (slot.cmd.phys().0, slot.resp.phys().0, slot.resp.len())
        };

        unsafe {
            let slot = self.slot_bufs[head as usize].as_ref()?;
            let cmd_buf = &slot.cmd;
            core::ptr::write_bytes(cmd_buf.as_mut_ptr(), 0, cmd_buf.len());
            core::ptr::copy_nonoverlapping(
                &cmd as *const _ as *const u8,
                cmd_buf.as_mut_ptr(),
                hdr_size,
            );
            if cmd_len > 0 {
                core::ptr::copy_nonoverlapping(
                    cmd_ptr as *const u8,
                    cmd_buf.as_mut_ptr().add(hdr_size),
                    cmd_len,
                );
            }
        }

        let parts: [(u64, u32, u16); 2] = [
            (cmd_phys, total_out as u32, 0),
            (resp_phys, resp_len as u32, 2),
        ];
        let queue = self.ctrl_queue.as_mut()?;
        if queue.push_at(head, &parts).is_err() {
            queue.release_head(head);
            return None;
        }
        INFLIGHT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        self.head_ctx[head as usize] = ctx_id;
        Some(head)
    }

    pub fn submit_3d(
        &mut self,
        ctx_id: u32,
        ring_idx: u8,
        cmd_ptr: u64,
        cmd_len: usize,
        fence_id: Option<u64>,
    ) -> (bool, Option<u64>) {
        let hdr_size = core::mem::size_of::<VirtioGpuCmdSubmit3d>();
        let total_out = hdr_size + cmd_len;
        self.ensure_bufs(total_out, 64);

        let mut cmd = VirtioGpuCmdSubmit3d::default();
        cmd.hdr.hdr_type = VIRTIO_GPU_CMD_SUBMIT_3D;
        cmd.hdr.ctx_id = ctx_id;
        cmd.hdr.ring_idx = ring_idx;
        if let Some(fid) = fence_id {
            cmd.hdr.flags = 1; // VIRTIO_GPU_FLAG_FENCE
            cmd.hdr.fence_id = fid;
        }
        // SUPERSEDED: the old note here said a per-submission fence "made
        // presentation worse" and that the host "signals a fence by pushing an
        // extra entry onto the named queue's used ring", so the accounting
        // desynchronised.
        //
        // That mechanism does not exist in the protocol. Linux reaps the fence
        // out of the response header of the same command
        // (virtgpu_vq.c:303):
        //
        //   if (resp->flags & VIRTIO_GPU_FLAG_FENCE) {
        //           fence_id = le64_to_cpu(resp->fence_id);
        //           virtio_gpu_fence_event_process(vgdev, fence_id);
        //   }
        //
        // so there is no extra used-ring entry and nothing to desynchronise. Our
        // send_command_raw already reads that header. The likelier cause of the
        // old regression is that a fence was requested and never signalled on our
        // side -- exactly the failure this change is meant to avoid.
        //
        // Whether the host honours it is being measured first; see the fence
        // probe above. Do not wire the bounded wait until that measurement says
        // the mechanism works.
        cmd.size = cmd_len as u32;

        // Reserve the descriptor head FIRST, so the private buffer pair can be
        // chosen for it. Previously the buffers were fetched before the head was
        // known, which is why they had to be the single shared pair.
        // Reserve the head first so the private buffer pair can be chosen for
        // it, then take the buffer borrows and the queue borrow separately --
        // two overlapping `&mut self` borrows will not compile.
        let head = match self.ctrl_queue.as_mut().and_then(|q| q.alloc_head()) {
            Some(h) => h,
            None => return (false, None),
        };
        if self
            .slot_bufs(head, hdr_size + cmd_len, 64)
            .is_none()
        {
            if let Some(q) = self.ctrl_queue.as_mut() {
                q.release_head(head);
            }
            return (false, None);
        }
        // Safe: the slot is owned by `head`, and no other in-flight command can
        // hold the same head.
        let slot = self.slot_bufs[head as usize].as_ref().unwrap();
        let cmd_buf = &slot.cmd;
        let resp_buf = &slot.resp;

        unsafe {
            core::ptr::write_bytes(cmd_buf.as_mut_ptr(), 0, cmd_buf.len());
            core::ptr::copy_nonoverlapping(
                &cmd as *const _ as *const u8,
                cmd_buf.as_mut_ptr(),
                hdr_size,
            );
            if cmd_len > 0 {
                core::ptr::copy_nonoverlapping(
                    cmd_ptr as *const u8,
                    cmd_buf.as_mut_ptr().add(hdr_size),
                    cmd_len,
                );
            }
        }

        let parts: [(u64, u32, u16); 2] = [
            (cmd_buf.phys().0, total_out as u32, 0),
            (resp_buf.phys().0, resp_buf.len() as u32, 2),
        ];
        let queue = match self.ctrl_queue.as_mut() {
            Some(q) => q,
            None => return (false, None),
        };
        // Submit on the head we already reserved. `wait_for(head)` retires only
        // this command's completion; others arriving first are consumed and
        // their slots released.
        if queue.push_at(head, &parts).is_err() {
            queue.release_head(head);
            return (false, None);
        }
        INFLIGHT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let ok = queue.wait_for(head).is_ok();
        // `wait_for` retires this head (and any it consumed on the way), so the
        // counter has to follow the queue's own bookkeeping, not ours.
        INFLIGHT.fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
        let resp = unsafe { read_volatile(resp_buf.as_mut_ptr() as *const VirtioGpuCtrlHdr) };

        // Linux contract (virtgpu_vq.c:303), now measured to hold on this host
        // (docs/virgl-vs-linux.md §17): a fence requested in the command header
        // comes back in the *response* header, with FLAG_FENCE set and fence_id
        // populated. Report whichever id the host actually signalled so the
        // caller can tie it to the eventfd it handed out -- signalling an id the
        // host did not return would be a lie, and signalling nothing would hang
        // anyone polling the fence.
        // Compare the SYNC path's rejection rate against the async path's.
        // Same instrument in both places, or the two numbers mean nothing.
        if classify_response(resp.hdr_type) != RespClass::OkNoData {
            use core::sync::atomic::AtomicU64;
            static E: AtomicU64 = AtomicU64::new(0);
            static T: AtomicU64 = AtomicU64::new(0);
            let e = E.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
            let t = T.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
            if e == 1 || e % 100 == 0 {
                crate::dprintln!(
                    "[SYNC] virgl submit rejected: resp={:#x} cmd={:#x} | {}/{} rejected",
                    resp.hdr_type, VIRTIO_GPU_CMD_SUBMIT_3D, e, t
                );
            }
        }
        // Linux checks the response type and reports it. For SUBMIT_3D it does
        // NOT fail the submit -- `virtio_gpu_cmd_submit_3d` (virtgpu_vq.c:1355)
        // calls `virtio_gpu_queue_fenced_ctrl_buffer`, which has no error
        // return, and the only type check lives in the reaper
        // (virtgpu_vq.c:291) as a rate-limited DRM_ERROR. So this mirrors that:
        // log, do not fail.
        //
        // The gap this closes: we previously looked ONLY at FLAG_FENCE, and
        // QEMU sets FLAG_FENCE on error responses too. So a host rejection was
        // indistinguishable from success. `0x1205`
        // (VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER) on a SUBMIT_3D is specifically
        // QEMU reporting a failed `virgl_renderer_submit_cmd()` -- exactly the
        // signal that was being swallowed.
        if classify_response(resp.hdr_type) != RespClass::OkNoData {
            use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
            static ERR: AtomicU64 = AtomicU64::new(0);
            static TOT: AtomicU64 = AtomicU64::new(0);
            let e = ERR.fetch_add(1, Relaxed) + 1;
            let t = TOT.fetch_add(1, Relaxed) + 1;
            // First one in full, then a running ratio: a single line cannot tell
            // a systematic failure from a startup artefact.
            if e == 1 || e % 100 == 0 {
                crate::dprintln!(
                    "[submit3d] host rejected submit: resp={:#x} cmd={:#x} | {}/{} rejected",
                    resp.hdr_type, VIRTIO_GPU_CMD_SUBMIT_3D, e, t
                );
            }
        }

        let signalled = if resp.flags & VIRTIO_GPU_FLAG_FENCE != 0 {
            Some(resp.fence_id)
        } else {
            None
        };
        if fence_id.is_some() && signalled.is_none() {
            // Requested but not returned. That is a host that ignored the
            // request; report it rather than pretending a fence exists.
            crate::dprintln!(
                "[fence3d] WARN: fence {} requested but host did not signal it (resp_flags={:#x})",
                fence_id.unwrap_or(0), resp.flags
            );
        }

        (ok && classify_response(resp.hdr_type) == RespClass::OkNoData, signalled)
    }

    /// Copies a region from src_res to dst_res via virgl CCMD 17 (RESOURCE_COPY_REGION) on host GPU.
    pub fn resource_copy_region_3d(
        &mut self,
        ctx_id: u32,
        dst_res: u32,
        src_res: u32,
        width: u32,
        height: u32,
    ) -> bool {
        self.ctx_attach_resource(ctx_id, dst_res);
        self.ctx_attach_resource(ctx_id, src_res);
        let cmd: [u32; 14] = [
            (13 << 16) | 17, // VIRGL_CMD0(VIRGL_CCMD_RESOURCE_COPY_REGION, 0, 13)
            dst_res,
            0, // dst_level
            0, // dst_x
            0, // dst_y
            0, // dst_z
            src_res,
            0, // src_level
            0, // src_x
            0, // src_y
            0, // src_z
            width,
            height,
            1, // depth
        ];
        let (ok, _) = self.submit_3d(ctx_id, 0, cmd.as_ptr() as u64, core::mem::size_of_val(&cmd), None);
        ok
    }
}
