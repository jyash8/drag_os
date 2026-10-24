# Design: Pointer, Wallpaper Image, Wayland Terminal, Virgl — DragOS mywima

Date: 2026-09-02
Status: Approved for planning

## 1. Context
- DragOS mywima (Smithay 0.7, GLES, GBM, libinput via seatd, DRM/KMS on virtio-vga 1280x800) now renders solid Mocha (99.98% RGB 30,30,46) after bypassing large JPEG wallpaper (3000x2000) which produced black via `import_memory`.
- `screen.ppm` via `screendump` is the display ground truth; `uefi_debug.log` is the instrumentation channel.
- Tools: `cargo run -p tools -- build-kernel / build-fs / run`, `capture_ppm.py` (60s + screendump).

## 2. Goals & Success Criteria
1. **Pointer moves**: QEMU usb-tablet drag moves cursor from center; `screen.ppm` cursor position changes; `uefi_debug.log` shows `POINTER_MOTION_ABSOLUTE` and `handle_mode_cursor MOVE ok`.
2. **Wallpaper image**: `screen.ppm` shows photographic image (not solid), >10 distinct colors, no black. Fallback remains solid mocha if image missing.
3. **Wayland terminal**: `foot`/`weston-terminal` autostarts, `space_elements >0`, window visible in `screen.ppm`.
4. **Virgl**: QEMU `virtio-vga-gl` + Mesa `virpipe` renders via host GPU; `MESA_LOADER` shows `virgl` not `llvmpipe`; `glxinfo`/`epoxy` reports virgl.

## 3. Architecture & Components

### Pointer (phase 1)
- **Path**: QEMU `qemu-xhci` + `usb-tablet` (absolute) / `usb-mouse` (relative) → `/dev/input/event0` (evdev) → seatd fd passing → libinput (quirks at `/usr/share/libinput`) → Smithay `LibinputInputBackend` → `state.pointer_location` → `PointerElement` (xcursor `left_ptr` 24px, Abgr8888) → `DRM_IOCTL_MODE_CURSOR` → `virtio-gpu update_cursor`/`move_cursor` (64x64) + page flip damage.
- **Files**: `mywima/src/input.rs`, `mywima/src/state.rs`, `mywima/src/element.rs`, `kernel/src/drivers/drm/mod.rs:1114`, `kernel/src/drivers/virtio/gpu.rs`, `scripts/build-fs.sh` (cursor icons), `tools/src/qemu.rs`.

### Wallpaper (phase 2)
- **Current bypass**: `mywima/src/backends/kms.rs:351` `wallpaper_buffer = None` → clear color only.
- **Fix**: `mywima/src/element.rs:209` keep solid fallback but add downscale: `image::load_from_memory(DEFAULT_WALLPAPER_BYTES) → imageops::resize(1280,800, Triangle) → import_memory(Abgr8888, 1280,800)`. Keeps proven 1280x800 import path.
- **Files**: `element.rs`, `assets/default_wallpaper.jpg`, `backends/kms.rs:351` (restore call).

### Terminal (phase 3)
- **Stage**: `scripts/build-fs.sh` copy `foot`/`weston-terminal` + deps (`libwayland-client`, `libxkbcommon`, `libpixman`, `fontconfig`) to `/usr/bin` and `/usr/share`; `mywima/config.toml autostart = ["foot"]` (or `weston-terminal`).
- **Verify**: `WAYLAND_DISPLAY=wayland-1` client connects, `space_elements` count, `screen.ppm` window chrome.

### Virgl (phase 4)
- **QEMU**: `tools/src/qemu.rs` add `--gl` variant: `-device virtio-vga-gl -display gtk,gl=on` (fallback `virtio-gpu-gl + sdl,gl=on`), require host `virglrenderer`.
- **Mesa**: `MESA_LOADER_DRIVER_OVERRIDE=virpipe` or auto via `virtio_gpu` caps; kernel `virtio-gpu` already does `create_resource_2d/blob`; need to expose `VIRTIO_GPU_CAPSET_VIRGL` if not.
- **Files**: `tools/src/qemu.rs`, `kernel/src/drivers/virtio/gpu.rs`, `scripts/build-fs.sh` (libEGL_mesa, virpipe).

## 4. Data Flow & Error Handling
- Pointer: libinput → compositor event loop (`calloop`) → cursor plane → page flip → GOP blit. On `SessionEvent::PauseSession`, libinput suspend; on `Activate`, resume. Log `POINTER_MOTION` via `eprintln!` to `uefi_debug.log` (stderr).
- Wallpaper: load → resize → import → `TextureBuffer` → `create_wallpaper_element` → `MyRenderElement::Texture` → `render_frame` clear + wallpaper. On import failure, return `None` → clear-only mocha (never black).
- Terminal: autostart via `Command::spawn`; on missing binary, log `Failed to autostart` and continue (already `autostart=[]` guard).
- Virgl: QEMU gl init may fail if host lacks `virglrenderer` → fallback to `llvmpipe` with log.

## 5. Testing Plan
- Each phase: `cargo run -p tools -- build-fs && build-kernel` → `capture_ppm.py` (60s) → check `screen.ppm` distinct colors and `uefi_debug.log` grep.
- Pointer: manual drag in QEMU window, compare two `screen.ppm` before/after.
- Wallpaper: after phase 2, `screen.ppm` distinct colors >10 and photographic content.
- Terminal: `screen.ppm` shows terminal window, `uefi_debug.log` `space_elements >0`.
- Virgl: `uefi_debug.log` `MESA_LOADER` shows `virpipe`, `glxinfo` in guest reports `virgl`.

## 6. Risks & Mitigations
- **Input absolute vs relative**: usb-tablet sends `ABS_X/Y`, libinput may map to `POINTER_MOTION_ABSOLUTE`. Mitig: test both `usb-tablet` and `usb-mouse`.
- **Texture size**: 3000x2000 exceeds some limits; mit: resize to output size first.
- **Virgl host dependency**: host may lack `virglrenderer`/GL; mit: keep `llvmpipe` fallback, feature-flag QEMU `gl` variant.
- **Disk sync**: `debugfs write` without `rm` leaves stale `mywima` (seen 8.7M vs 11M); mit: `rm` before `write` already fixed.

## 7. Out of Scope
- Full desktop environment, multiple outputs, HiDPI scaling.
- Hardware Vulkan, AMDGPU/i915 firmware (already staged).
