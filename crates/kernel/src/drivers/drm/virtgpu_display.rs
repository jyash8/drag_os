//! Line-by-line translation of Linux `drivers/gpu/drm/virtio/virtgpu_display.c`.
//!
//! Source: `/tmp/opencode/linuxfull/virtgpu_display.c` (kernel master, 404 lines).
//!
//! ## Status: SPECIFICATION
//!
//! The connector/mode-discovery path is transcribed in full because §47/§48
//! turned on it. The CRTC vtable is summarised in `crtc_vtable` below, including
//! one comment that is load-bearing for the whole driver.

/// Present in `virtgpu_display.c` and not separately transcribed.
pub const NOT_YET_PORTED: &[&str] = &[
    "virtio_gpu_conn_detect() / virtio_gpu_conn_destroy()",
    "virtio_gpu_crtc_atomic_enable() -- drm_crtc_vblank_on only",
    "virtio_gpu_crtc_atomic_check() -- returns 0 unconditionally upstream",
    "virtio_gpu_crtc_atomic_flush() -- vblank event plumbing",
    "virtio_gpu_enc_mode_set() / _enable() / _disable()",
    "vgdev_output_init() / virtio_gpu_modeset_init() / _fini()",
];

/// The CRTC vtable, summarised (`virtio_gpu_crtc_funcs`, lines 53-62).
///
/// **D1 — `atomic_disable` programs scanout to resource 0, not to "nothing".**
///
/// ```c
/// static void virtio_gpu_crtc_atomic_disable(struct drm_crtc *crtc, ...)
/// {
///         drm_crtc_vblank_off(crtc);
///         virtio_gpu_cmd_set_scanout(vgdev, output->index, 0, 0, 0, 0, 0);
///         virtio_gpu_notify(vgdev);
/// }
/// ```
///
/// Six zeroes: scanout_id, resource_id 0, and the four geometry components. So
/// disabling a CRTC actively points the display at **no resource** — the
/// equivalent of blanking the output. This is the same idea as the "nofb" arm of
/// `virtio_gpu_primary_plane_update` (`virtgpu_plane.c`), which also does
/// `cmd_set_scanout(vgdev, output->index, 0, ...)`.
///
/// We have no equivalent: our `handle_page_flip` returns `ENOENT` when the fb is
/// missing, which leaves the **previous** buffer still scanning out. A client
/// that unmaps its framebuffer without a CRTC disable therefore keeps seeing
/// stale content instead of a blanked output.
pub mod crtc_vtable {
    /// `virtio_gpu_crtc_atomic_disable` (line 109) sets scanout resource 0.
    pub const DISABLE_SETS_SCANOUT_TO: u32 = 0;
}

/// `virtio_gpu_conn_get_modes` (virtgpu_display.c:182-211).
///
/// ```c
/// count = drm_edid_connector_add_modes(connector);
/// if (count)
///         return count;                          /* EDID primary */
///
/// width  = le32_to_cpu(output->info.r.width);
/// height = le32_to_cpu(output->info.r.height);
/// count = drm_add_modes_noedid(connector, XRES_MAX, YRES_MAX);
///
/// if (width == 0 || height == 0) {
///         drm_set_preferred_mode(connector, XRES_DEF, YRES_DEF);
/// } else {
///         mode = drm_cvt_mode(dev, width, height, 60, false, false, false);
///         if (!mode) return count;
///         mode->type |= DRM_MODE_TYPE_PREFERRED;
///         drm_mode_probed_add(connector, mode);
///         count++;
/// }
/// return count;
/// ```
///
/// **D2 — three sources, in order, and the preferred mode only ever comes from
/// `display_info` or the `XRES_DEF` fallback.** Not from EDID: EDID contributes
/// its own modes and returns early if it has any, but the `PREFERRED` flag is
/// only applied on the `display_info` path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ModeSource {
    /// `drm_edid_connector_add_modes` returned >= 1 mode; use those.
    Edid,
    /// `display_info` gave a real geometry; synthesise a 60 Hz mode and mark it
    /// preferred.
    DisplayInfo { width: u32, height: u32 },
    /// `width == 0 || height == 0`, so fall back to `XRES_DEF/YRES_DEF`.
    Default,
}

/// The decision, transcribed.
pub fn mode_source(edid_mode_count: u32, display_w: u32, display_h: u32) -> ModeSource {
    if edid_mode_count > 0 {
        return ModeSource::Edid;
    }
    if display_w == 0 || display_h == 0 {
        return ModeSource::Default;
    }
    ModeSource::DisplayInfo { width: display_w, height: display_h }
}

/// `virtio_gpu_conn_mode_valid` (virtgpu_display.c:213-234).
///
/// ```c
/// width  = le32_to_cpu(output->info.r.width);
/// height = le32_to_cpu(output->info.r.height);
///
/// if (!(mode->type & DRM_MODE_TYPE_PREFERRED))
///         return MODE_OK;
/// if (mode->hdisplay == XRES_DEF && mode->vdisplay == YRES_DEF)
///         return MODE_OK;
/// if (mode->hdisplay <= width  && mode->hdisplay >= width - 16 &&
///     mode->vdisplay <= height && mode->vdisplay >= height - 16)
///         return MODE_OK;
///
/// return MODE_BAD;
/// ```
///
/// **D3 — a mode is only rejected if it claims to be PREFERRED.** Non-preferred
/// modes are always `MODE_OK`; the validity check is entirely about protecting
/// the *native* mode from being displaced by something close to it.
///
/// The tolerance is **±16 pixels on each axis** — so a preferred mode within
/// 16px of the native geometry is accepted, and anything else preferred is
/// rejected outright.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ModeStatus {
    Ok,
    Bad,
}

/// The half-width/half-height tolerance Linux allows on the native mode.
pub const NATIVE_TOLERANCE: u32 = 16;

/// Transcribe `virtio_gpu_conn_mode_valid`.
pub fn mode_valid(
    is_preferred: bool,
    hdisplay: u32,
    vdisplay: u32,
    native_w: u32,
    native_h: u32,
    xres_def: u32,
    yres_def: u32,
) -> ModeStatus {
    // D3: only preferred modes are checked at all.
    if !is_preferred {
        return ModeStatus::Ok;
    }
    if hdisplay == xres_def && vdisplay == yres_def {
        return ModeStatus::Ok;
    }
    // Careful: these must be computed on u32 with the width-16 case guarded,
    // or a native width < 16 wraps. Upstream has the same unsigned expression;
    // we saturate, which only differs for an absurd native size.
    let w_lo = native_w.saturating_sub(NATIVE_TOLERANCE);
    let h_lo = native_h.saturating_sub(NATIVE_TOLERANCE);
    if hdisplay <= native_w && hdisplay >= w_lo && vdisplay <= native_h && vdisplay >= h_lo {
        return ModeStatus::Ok;
    }
    ModeStatus::Bad
}

/// Divergences from `virtgpu_display.c`.
pub mod divergences {
    /// D1 — no CRTC-disable path, so a dropped framebuffer leaves the previous
    /// buffer on screen.
    ///
    /// Linux's `virtio_gpu_crtc_atomic_disable` explicitly points scanout at
    /// resource 0, blanking the output. We return `ENOENT` from the flip path
    /// instead, which is an error to the *caller* but changes nothing on the
    /// display: whatever was last programmed stays programmed.
    ///
    /// **Impact: latent.** No client currently disables a CRTC — they unmap
    /// their framebuffer and exit. Recorded because it is the difference between
    /// "output goes blank" and "output shows a stale buffer" once VT switching
    /// or client teardown goes through the CRTC path.
    pub const D1_NO_CRTC_DISABLE: &str =
        "CRTC disable does not program scanout to resource 0; previous buffer stays on screen";

    /// D2 — the mode list is a hand-built ladder, not derived from any source.
    ///
    /// We never call `get_edid` (until §48 added it) and `get_display_info`
    /// returns `None`, so `add_standard_modes` anchors on the GOP boot
    /// resolution and appends 1920x1080 / 1280x800 / 1280x720 / 1024x768 /
    /// 800x600. Upstream takes its preferred mode from `display_info` and its
    /// mode set from EDID.
    ///
    /// **Impact: real and user-visible** — the reported resolution is the boot
    /// GOP size regardless of what the device says. §48 wires the EDID read in
    /// and logs which of the three sources won; §47's correction explains why
    /// `display_info` remains load-bearing for this device.
    pub const D2_FABRICATED_MODE_LIST: &str =
        "mode list hand-built from GOP + hardcoded ladder, not from EDID or display_info";

    /// D3 — no `mode_valid` check at all.
    ///
    /// Upstream rejects a PREFERRED mode that is not the native geometry (within
    /// ±16px). We accept whatever `add_standard_modes` produced, which is how
    /// a preferred 1280x800 survives even when the device is something else.
    pub const D3_NO_MODE_VALIDATION: &str =
        "no virtio_gpu_conn_mode_valid equivalent; a non-native PREFERRED mode is never rejected";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edid_short_circuits_before_display_info() {
        // D2: EDID wins outright when it yields anything.
        assert_eq!(mode_source(3, 1280, 800), ModeSource::Edid);
        assert_eq!(mode_source(1, 1280, 800), ModeSource::Edid);
        // No EDID modes -> display_info.
        assert_eq!(
            mode_source(0, 1280, 800),
            ModeSource::DisplayInfo { width: 1280, height: 800 }
        );
    }

    #[test]
    fn zero_geometry_falls_back_to_default() {
        // Upstream: `if (width == 0 || height == 0) drm_set_preferred_mode(XRES_DEF...)`
        assert_eq!(mode_source(0, 0, 800), ModeSource::Default);
        assert_eq!(mode_source(0, 1280, 0), ModeSource::Default);
        assert_eq!(mode_source(0, 0, 0), ModeSource::Default);
    }

    #[test]
    fn non_preferred_modes_are_always_ok() {
        // D3: the check is entirely about the native mode.
        assert_eq!(mode_valid(false, 1920, 1080, 1280, 800, 1280, 800), ModeStatus::Ok);
        assert_eq!(mode_valid(false, 1, 1, 1280, 800, 1280, 800), ModeStatus::Ok);
    }

    #[test]
    fn default_mode_is_always_acceptable() {
        assert_eq!(mode_valid(true, 1024, 768, 1280, 800, 1024, 768), ModeStatus::Ok);
    }

    #[test]
    fn native_mode_is_acceptable_and_near_misses_are_too() {
        // Exact.
        assert_eq!(mode_valid(true, 1280, 800, 1280, 800, 1024, 768), ModeStatus::Ok);
        // Within the 16px tolerance on each axis, in both directions.
        assert_eq!(mode_valid(true, 1296, 816, 1280, 800, 1024, 768), ModeStatus::Ok);
        assert_eq!(mode_valid(true, 1264, 784, 1280, 800, 1024, 768), ModeStatus::Ok);
        // One pixel outside the tolerance -> rejected.
        assert_eq!(mode_valid(true, 1297, 800, 1280, 800, 1024, 768), ModeStatus::Bad);
        assert_eq!(mode_valid(true, 1280, 817, 1280, 800, 1024, 768), ModeStatus::Bad);
    }

    #[test]
    fn a_very_different_preferred_mode_is_rejected() {
        // This is the case our hardcoded ladder would fail: it advertises
        // 1280x800 PREFERRED regardless of the real device.
        assert_eq!(mode_valid(true, 1920, 1080, 1280, 800, 1024, 768), ModeStatus::Bad);
    }
}