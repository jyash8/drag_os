//! Line-by-line translation of the remaining two files:
//! `virtgpu_drv.c` (361 lines) and `virtgpu_prime.c` (388 lines).
//!
//! Sources: `/tmp/opencode/linuxfull/virtgpu_drv.c`, `virtgpu_prime.c`
//! (kernel master).
//!
//! ## Status: SPECIFICATION
//!
//! These are the two smallest remaining files and both are mostly plumbing.
//! `virtgpu_drv.c` is probe/release/PM; `virtgpu_prime.c` is dma-buf import and
//! export. Neither contains presentation logic, so neither is a shear candidate —
//! recorded explicitly so that is not re-derived.

/// Not separately transcribed, with the reason in each case.
pub const NOT_YET_PORTED: &[&str] = &[
    "virtio_gpu_pci_quirk() -- aperture size conflict handling, needs PCI aperture management",
    "virtio_gpu_probe() / _remove() / _release_vqs() / _shutdown() / _config_changed()",
    "virtgpu_freeze() / _restore() / virtgpu_hibernation_restore() -- PM suspend/resume",
    "virtgpu_gem_prime_export() -- dma-buf export",
    "virtgpu_dma_buf_import_sgt() / _init_obj() / _unmap() / _free_obj() -- dma-buf import",
    "virtio_gpu_resource_assign_uuid() / virtgpu_virtio_get_uuid() -- VIRTIO_GPU_F_RESOURCE_UUID",
];

// ---------------------------------------------------------------------------
// virtgpu_drv.c
// ---------------------------------------------------------------------------

/// `virtio_gpu_pci_quirk` (virtgpu_drv.c:55-72).
///
/// ```c
/// bool vga = pci_is_vga(pdev);
/// DRM_INFO("pci: %s detected at %s\n", vga ? "virtio-vga" : "virtio-gpu-pci", pname);
/// if (vga) {
///         ret = aperture_remove_conflicting_pci_devices(pdev, driver.name);
///         if (ret) return ret;
/// }
/// return 0;
/// ```
///
/// **R1 — for a `virtio-vga` (legacy VGA) device, conflicting PCI devices must
/// have their PCI apertures removed before the device is used.** Otherwise the
/// legacy VGA window and the virtio BAR can overlap, and whichever loses ends up
/// with a BAR pointing at the other's memory.
///
/// This matters for us: we run `virtio-vga-gl`, i.e. the **legacy VGA path**,
/// which is exactly the `vga == true` branch. We do not do aperture
/// arbitration.
///
/// **Impact: latent.** Safe while QEMU hands out non-overlapping windows, which
/// it does. It is the kind of thing that only bites on real hardware with two
/// display devices.
pub mod pci_quirk {
    /// Whether this device needs conflicting apertures removed.
    pub fn needs_aperture_removal(is_virtio_vga: bool) -> bool {
        is_virtio_vga
    }
}

/// `features[]` (virtgpu_drv.c:171-185) — the authoritative feature list.
///
/// ```c
/// static unsigned int features[] = {
/// #ifdef __LITTLE_ENDIAN
///         /*
///          * Gallium command stream send by virgl is native endian.
///          * Because of that we only support little endian guests on
///          * little endian hosts.
///          */
///         VIRTIO_GPU_F_VIRGL,
/// #endif
///         VIRTIO_GPU_F_EDID,
///         VIRTIO_GPU_F_RESOURCE_UUID,
///         VIRTIO_GPU_F_RESOURCE_BLOB,
///         VIRTIO_GPU_F_CONTEXT_INIT,
///         VIRTIO_GPU_F_BLOB_ALIGNMENT,
/// };
/// ```
///
/// Bit positions from `include/uapi/virtio_gpu.h`:
///
/// ```c
/// #define VIRTIO_GPU_F_VIRGL          1
/// #define VIRTIO_GPU_F_EDID           2
/// #define VIRTIO_GPU_F_RESOURCE_UUID  4
/// #define VIRTIO_GPU_F_RESOURCE_BLOB  8
/// #define VIRTIO_GPU_F_CONTEXT_INIT   16
/// #define VIRTIO_GPU_F_BLOB_ALIGNMENT 32
/// ```
///
/// **R2 — VIRGL is gated on little-endian**, because the Gallium command stream
/// virgl sends is native endian. Upstream simply omits the bit on a big-endian
/// build rather than negotiating it away.
pub const F_VIRGL: u32 = 1 << 0;
pub const F_EDID: u32 = 1 << 1;
pub const F_RESOURCE_UUID: u32 = 1 << 2;
pub const F_RESOURCE_BLOB: u32 = 1 << 3;
pub const F_CONTEXT_INIT: u32 = 1 << 4;
pub const F_BLOB_ALIGNMENT: u32 = 1 << 5;

/// The set a little-endian guest should request.
pub fn features_little_endian() -> u32 {
    F_VIRGL | F_EDID | F_RESOURCE_UUID | F_RESOURCE_BLOB | F_CONTEXT_INIT | F_BLOB_ALIGNMENT
}

/// The set a big-endian guest requests — VIRGL omitted.
pub fn features_big_endian() -> u32 {
    features_little_endian() & !F_VIRGL
}

/// **R3 — the negotiated set is `device & driver`.** A requested bit the device
/// did not offer is masked out by the virtio transport, so capability flags must
/// be derived from the *negotiated* value, never from what was requested.
///
/// We got this wrong and it is fixed (§49): the feature word was
/// `1 | (dev_lo & ...)`, which force-set VIRGL regardless of what the host
/// offered, and `has_edid` / `has_resource_blob` / `has_virgl` were all computed
/// from that word. Every one of those flags was therefore "we asked for it"
/// rather than "the device granted it".
pub fn negotiate(device_features: u32, driver_features: u32) -> u32 {
    device_features & driver_features
}

/// Divergences found in `virtgpu_drv.c`.
pub mod drv_divergences {
    /// R1 — no PCI aperture arbitration for the legacy VGA path.
    ///
    /// `virtio_gpu_pci_quirk` calls
    /// `aperture_remove_conflicting_pci_devices()` when `pci_is_vga(pdev)`, and
    /// we run exactly that path (`virtio-vga-gl`). Latent under QEMU, which does
    /// not overlap windows; real on hardware with two display devices.
    pub const R1_NO_APERTURE_ARBITRATION: &str =
        "no aperture_remove_conflicting_pci_devices for the virtio-vga path";

    /// R3 — feature flags were derived from the request, not the negotiation.
    /// Fixed; recorded because the pattern (claim a capability because we asked)
    /// also produced the `getparam` defects.
    pub const R3_FLAGS_FROM_REQUEST: &str =
        "has_virgl/has_edid/has_resource_blob derived from requested features, not negotiated (FIXED)";
}

// ---------------------------------------------------------------------------
// virtgpu_prime.c
// ---------------------------------------------------------------------------

/// `virtgpu_gem_prime_export` (virtgpu_prime.c:106-146).
///
/// Exports a GEM object as a dma-buf so another device (or process) can import
/// it. The virtio side is `virtio_gpu_resource_assign_uuid`, which gives the
/// resource a UUID the importing device can name rather than shipping the
/// memory across a bus.
///
/// **R4 — prime export requires `VIRTIO_GPU_F_RESOURCE_UUID`.** Without that
/// feature bit there is no way to name the resource on the importing side, so
/// upstream refuses.
///
/// We declare `has_resource_assign_uuid` as `0` in `getparam`
/// (`VIRTGPU_PARAM_CROSS_DEVICE => 0`), which is consistent with not
/// implementing export — a client probing `CROSS_DEVICE` correctly learns we do
/// not support it. That is the one place where our hardcoded `getparam` value
/// is provably right rather than accidentally right.
///
/// **Impact: none today.** No client primes from us.
pub mod prime {
    /// Whether dma-buf export can be offered at all.
    pub fn export_supported(has_resource_uuid: bool) -> bool {
        has_resource_uuid
    }

    /// Upstream refuses export without the UUID feature; so must we.
    pub fn may_export(has_resource_uuid: bool) -> Result<(), ()> {
        if has_resource_uuid {
            Ok(())
        } else {
            Err(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virgl_is_gated_on_endianness() {
        // R2
        assert_eq!(features_little_endian() & F_VIRGL, F_VIRGL);
        assert_eq!(features_big_endian() & F_VIRGL, 0);
        // Everything else is identical between the two.
        assert_eq!(
            features_little_endian() & !F_VIRGL,
            features_big_endian() & !F_VIRGL
        );
    }

    #[test]
    fn negotiation_can_never_invent_a_bit() {
        // R3: the whole point. A bit the device did not offer cannot survive,
        // which is what makes it safe to derive capability flags from the result.
        let dev = F_VIRGL | F_EDID;
        let drv = features_little_endian();
        assert_eq!(negotiate(dev, drv), F_VIRGL | F_EDID);
        // Driver asks for BLOB, device does not have it -> gone.
        assert_eq!(negotiate(dev, drv) & F_RESOURCE_BLOB, 0);
        // Every negotiated bit was offered by the device.
        assert_eq!(negotiate(dev, drv) & !dev, 0);
    }

    #[test]
    fn vga_path_needs_aperture_removal() {
        // R1: we run virtio-vga-gl, so this is the branch that applies to us.
        assert!(pci_quirk::needs_aperture_removal(true));
        assert!(!pci_quirk::needs_aperture_removal(false));
    }

    #[test]
    fn prime_export_requires_the_uuid_feature() {
        assert!(prime::may_export(true).is_ok());
        assert!(prime::may_export(false).is_err());
        assert!(prime::export_supported(false) == false);
    }
}