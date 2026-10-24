
## 63. MEDIUM-2 closed: `RESOURCE_CREATE_3D` bind/flags now match upstream

### What the deviation was

`handle_virtgpu_resource_create` sent two values upstream never modifies:

```rust
let mut usage = req.flags;
if is_scanout { usage |= VIRGL_RESOURCE_FLAG_MAP_COHERENT; }
cmd3d.bind = bind & !VIRGL_BIND_SCANOUT;   // clear a flag the client asked for
```

Upstream is a pass-through, both fields
(`virtgpu_ioctl.c: params.bind = rc->bind; params.flags = rc->flags;`
`virtgpu_vq.c: cmd_p->bind = cpu_to_le32(params->bind);`).

Both mutations existed to chase the shear, under the theory that
`VIRGL_BIND_SCANOUT` makes the host allocate a hardware-tiled GBM surface
("Intel Tile-Y, 32-pixel columns").

### Why it is now removed rather than kept

Three reasons, in order of weight:

1. **It was measured and it did not work.** §60 records the shear persisting
   *with* the flag cleared. The theory it was written for is not supported by
   the only measurement taken of it.
2. **It contradicted the client.** The in-tree comment says *forcing* the flag
   on large textures caused tiling, so forcing was removed -- but Mesa sets the
   flag itself for real scanout buffers, and this code then cleared it. The
   guest was therefore refusing the compositor's own request. Measured this
   pass: `raw_bind=0x14000a is_scanout=true` on 1280x800, i.e. Mesa asks for
   `VIRGL_BIND_SCANOUT | SHARED | RENDER_TARGET | SAMPLER_VIEW`.
3. **The host allocates scanout anyway.** The capset now reads
   `SCANOUT_USES_GBM=1`, so the host picks its own scanout storage irrespective
   of what arrives in `bind`. The flag is not the lever the theory assumed.

The record/wire disagreement is fixed too: `res.bind` stored the *unmasked*
`req.bind` while the host received the masked value, so anything reading the
record reasoned about a resource the host had never seen.

Both mutations are preserved behind `DRAGOS_VIRGL_BIND_SCANOUT=1` so the two
arms can be A/B'd deliberately -- one rebuild apart -- and a `MUTATED bind`
log line fires only when they differ. Unset is upstream parity. Verified:
`MUTATED bind` count 0, boot clean (0 panics, 5/5 selftests).

### The measurement that invalidates the previous shear conclusion

§60 concluded "no guest-side fix exists; the fault is host-side". That was
measured **while the compositor was being served a zeroed capset** (§62.3 --
now fixed), so the conclusion was invalid. With correct capsets and proper
bind/flags handling, the shear can be re-measured.

---


## 64. MEDIUM-5 closed: `[probe]` readback now returns `None` instead of uninitialised DMA

### What the deviation was

The `probe_rows` function issued `TRANSFER_FROM_HOST_3D` (which writes into
the resource's own attached backing) but returned the address of a separate
`probe_buf` that was never attached to any descriptor chain. Every `[probe]`
line consequently read uninitialised DMA, making the "row repeats at a 16-px
offset" statistic meaningless.

### Why it is now fixed

The function now returns `None` always with a explanatory comment, and the
call site logs a one-time message explaining that host texture readback
requires a dedicated scratch resource (which would necessitate
DETACH_BACKING+ATTACH_BACKING on the scanout resource—a perturbing operation
that changes the host's `tex_id` and may blacken the screen). This eliminates
a source of misleading evidence while being honest about measurement
limitations.

---


## 65. MEDIUM-6 closed: `ADDFB` now validates framebuffer parameters

### What the deviation was

Both `handle_add_fb` and `handle_add_fb2` checked only that the handle named a
known dumb buffer, with no validation of width/height/pitch/format or whether
the framebuffer fit within the buffer's actual allocation. This allowed
clients to name framebuffers far larger than their allocations, leading to
out-of-bounds reads via HHDM in the GOP blit path.

### Why it is now fixed

Added upstream-style validation (`framebuffer_check`):
- Reject width=0 or height=0 (`EINVAL`)
- Reject handle=0 (`EINVAL`)
- Require bpp=32 ( scanout paths assume 4-byte pixels)
- Check pitch ≥ width*4 (`EINVAL`)
- Ensure height*pitch fits in u32 (`ERANGE`)
- Verify the framebuffer fits within the dumb buffer's actual size (`EINVAL`)

This prevents guest-physical out-of-bounds reads while maintaining compatibility
with valid clients.

---


## 66. MEDIUM-10 closed: `GETPARAM` now returns `EFAULT` on bad addresses

### What the deviation was

The getparam handler failed to return `EFAULT` when `vr_write` failed to write
the result value to userspace, instead returning success while failing to
actually modify userspace memory.

### Why it is now fixed

Split the conditional `if out_ptr != 0 && vr_write(...)` into two steps:
1. Check `out_ptr != 0` first
2. If true, call `vr_write` and return `EFAULT` on failure
3. Only then write the value with `write_unaligned`

This matches the error-handling pattern used elsewhere in the driver and
ensures bad addresses are properly reported.

---


## 67. MEDIUM-12 closed: `CONTEXT_INIT` now rejects unknown parameters

### What the deviation was

The context_init handler silently ignored unknown parameters via a catch-all
`_ => {}` branch, whereas upstream returns `-EINVAL` for unsupported
parameters.

### Why it is now fixed

Replaced the catch-all with explicit error logging and `return EINVAL`,
matching upstream's behavior and preventing clients from mistakenly believing
invalid parameters were accepted.

---


## 68. MEDIUM-13 closed: `TRANSFER_TO/FROM_HOST` now validates stride parameters

### What the deviation was

The transfer handlers accepted stride and layer_stride values that could be
nonsensical (e.g., zero stride or layer_stride < stride), relying entirely on
host-side validation.

### Why it is now fixed

Added basic sanity checks in both transfer handlers:
- Reject stride == 0 (`EINVAL`)
- Reject layer_stride < stride (`EINVAL`)

These catch clearly bogus values while allowing legitimate tight-packed
layouts (stride = layer_stride). The host still performs full validation of
resource bounds and format-specific constraints.

---


## 69. MEDIUM-14 closed: `DIRTYFB` now validates ANNOTATE_COPY clip pairing

### What the deviation was

When `DRM_MODE_FB_DIRTY_ANNOTATE_COPY` was set, the handler did not verify
that clip coordinates came in pairs (source/destination rectangles), silently
accepting odd-length clip lists that upstream would reject with `-EINVAL`.

### Why it is now fixed

Added validation: if `annotate_copy` is set and `num_clips` is odd, return
`EINVAL` with an explanatory log message. This matches upstream's
`drm_mode_dirtyfb_ioctl` behavior and prevents silent misinterpretation of
clip lists.

---


## 70. LOW-16: kernel test modules remain unbuildable

The kernel's own `#[cfg(test)]` modules require a host harness to build and
run; only the `drm-uapi` subset is currently buildable. This is tracked
separately from the graphics-specific work.

---

## 71. Host Texture Probe Breakthrough: `res 105` is Clean in Host GL Memory

### What was measured

To definitively isolate whether the wallpaper shearing originates before or
during host-side sampling/scanout presentation, a host-side OpenGL probe
(`scratch/trace_tex105.c` -> `scratch/libtrace_tex105.so`) was preloaded into
QEMU via `LD_PRELOAD`.

The probe attached an FBO to the host texture associated with `res 105`
(`tex_id=4`) and read back the raw RGBA pixels directly via `glReadPixels`.

### Findings

1. **Host OpenGL Texture (`res 105`) is Clean**:
   - Comparison against the decoded guest wallpaper (`scratch/compare_tex.py`):
     Mean absolute error Y-flipped = **0.45 / 255** (~0.17%), with >53% exact pixel match.
   - Autocorrelation analysis shows **zero periodic peaks** at lag 25, 32, or 64.
   - **Conclusion**: Guest RAM upload, staging buffer (`res 106`), execbuffer `COPY_TRANSFER3D`,
     and the host GL texture itself are completely clean and unsheared.
2. **Host GBM Resource Trace**:
   - Traced `virgl_renderer_resource_create` via `scratch/libtrace_gbm.so`.
   - Confirmed `res 105` has `tex_id=4, stride=5120, drm_fourcc=0x0, fd=-1` (standard GL texture, no GBM bo).
   - Confirmed scanout buffers (`res 102` and `111`) are created with `fmt=1, bind=0x14000a` (`tex_id=2` and `9`).
   - The shear occurs entirely during presentation / sampling into the scanout!

---

## 72. Transition to Atomic KMS Plane Updates (`DRM_IOCTL_MODE_ATOMIC`)

### What was changed

1. **UAPI Definitions (`crates/drm-uapi/src/lib.rs`)**:
   - Defined `struct DrmModeAtomic` (56 bytes, matching upstream Linux `<drm/drm_mode.h>`).
   - Pinned `DRM_IOCTL_MODE_ATOMIC = drm_ioctl_t::<DrmModeAtomic>(IOC_RW, 0xBC) == 0xC03864BC`.
   - Defined atomic commit flags (`DRM_MODE_ATOMIC_TEST_ONLY`, `DRM_MODE_ATOMIC_NONBLOCK`, `DRM_MODE_ATOMIC_ALLOW_MODESET`, `DRM_MODE_PAGE_FLIP_EVENT`, `DRM_MODE_PAGE_FLIP_ASYNC`).
   - Defined standard property flags and types (`DRM_MODE_PROP_RANGE`, `DRM_MODE_PROP_OBJECT`, `DRM_MODE_PROP_SIGNED_RANGE`, etc.).
   - Exposed `DRM_CAP_CRTC_IN_VBLANK_EVENT = 0x12`.

2. **Atomic Properties Exposure (`crates/kernel/src/drivers/drm/mod.rs`)**:
   - Assigned standard property IDs for Planes (`type`, `FB_ID`, `CRTC_ID`, `CRTC_X`, `CRTC_Y`, `CRTC_W`, `CRTC_H`, `SRC_X`, `SRC_Y`, `SRC_W`, `SRC_H`, `IN_FENCE_FD`).
   - Assigned property IDs for CRTCs (`ACTIVE`, `MODE_ID`).
   - Assigned property IDs for Connectors (`CRTC_ID`).
   - Updated `handle_obj_get_properties` and `handle_get_property` to return all atomic properties.
   - Enabled client capability `DRM_CLIENT_CAP_ATOMIC` in `handle_set_client_cap`.

3. **Unified Plane Presentation (`crates/kernel/src/drivers/drm/virtgpu_plane.rs`)**:
   - Generalized `primary_plane_update` with `PlaneBo`, supporting 3D textures, host blobs, and 2D dumb buffers.
   - Integrated source rect coordinates (`src_x`, `src_y`) into scanout offsets.
   - Routed `handle_page_flip`, `handle_set_plane`, `handle_set_crtc`, and atomic commits through `primary_plane_update`.

---

## 73. Blank Screen Defect in Atomic Commits & Shear Status

### 73.1 The Cursor Plane Blank Screen Bug
- **Deviation**: During atomic commits with multiple planes, Plane 5 (cursor plane) was committed with `fb_id = None`. In the original commit loop, every plane was passed to `primary_plane_update`.
- **Failure**: When `primary_plane_update` saw `fb_id = None` on Plane 5, it called `vgdev.set_scanout(0, 0, rect)`, passing `resource_id = 0`. In QEMU virgl (`ui/console.c:qemu_console_set_surface`), this deactivated the console surface and displayed the string `"Display output is not active."` (`tex9_crop_flipped.png`).
- **Fix**: Plane commit iteration now filters for `plane_type == DRM_PLANE_TYPE_PRIMARY`. Non-primary planes without their own backing hardware plane are skipped.

### 73.2 False "Clean Screen" Discovery
- A previous measurement reported that `/tmp/dragos_screen.png` showed zero autocorrelation peaks after atomic commits were enabled.
- Subsequent analysis showed that `/tmp/dragos_screen.png` was an artifact anomaly rather than a live render capture.
- Live captures from QEMU (`fixed_qemu_live.png`) verify that the compositor is running and fully visible, but the horizontal wallpaper shear (~32px vertical columns) persists.
- The root cause is isolated to the sampling / drawing of texture `res 105` into scanout buffers `res 102`/`111` in `vrend_draw_vbo` / Mesa Gallium shaders.