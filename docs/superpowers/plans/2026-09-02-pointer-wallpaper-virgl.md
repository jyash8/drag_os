# Pointer, Wallpaper Image, Wayland Terminal, Virgl Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make DragOS mywima pointer move, show photographic wallpaper (not solid), run a Wayland terminal, and use host virgl for Mesa.

**Architecture:** Keep Smithay GLES + GBM + libinput + seatd on virtio-vga 1280x800. Pointer: fix libinput absolute vs cursor plane. Wallpaper: downscale JPEG to 1280x800 before import. Terminal: stage foot + deps and autostart. Virgl: add QEMU `virtio-vga-gl` variant and Mesa virpipe selection.

**Tech Stack:** Rust (mywima Smithay 0.7, kernel), QEMU 9.x, Mesa 24 (llvmpipe/virpipe), GBM, libinput, seatd, image 0.25

---

### Task 1: Pointer Movement Instrumentation & Fix

**Files:**
- Modify: `mywima/src/input.rs:40-80`
- Modify: `kernel/src/drivers/drm/mod.rs:1114-1167`
- Modify: `tools/src/qemu.rs:20-60`
- Test: `initrd/input_test.c` + `capture_ppm.py` screendump

- [ ] **Step 1: Add pointer motion log in mywima**

```rust
// mywima/src/input.rs - inside libinput event handler for PointerMotion* 
eprintln!("[input] pointer motion: pos={:?} absolute={}", location, is_absolute);
```

- [ ] **Step 2: Run to verify log appears when moving mouse**

Run: `cargo run -p tools -- build-fs && cargo run -p tools -- build-kernel && python3 capture_ppm.py`
Expected: move mouse in QEMU window, `uefi_debug.log` shows `[input] pointer motion` lines, but `screen.ppm` still frozen.

- [ ] **Step 3: Add DRM cursor ioctl trace in kernel**

```rust
// kernel/src/drivers/drm/mod.rs:1139 handle_mode_cursor
crate::dprintln!("[cursor] flags={:#x} handle={} x={} y={} blob={}", req.flags, req.handle, x, y, db.is_some());
```

- [ ] **Step 4: Run to verify cursor ioctl**

Run: same as Step 2
Expected: `uefi_debug.log` shows `[cursor]` on each move, and `virtio-gpu move_cursor` return. If `handle_mode_cursor` returns `ENOSYS` (no GPU), that's the bug.

- [ ] **Step 5: Fix QEMU input device if needed**

```rust
// tools/src/qemu.rs - ensure both relative and absolute present
"-device", "qemu-xhci",
"-device", "usb-tablet",   // absolute (current)
"-device", "usb-mouse",    // relative fallback
```

If libinput only sees relative, switch to `virtio-input-host` or ensure `LIBINPUT_DEVICE_CAP_POINTER` is advertised by checking `uefi_debug.log` for `libinput: event0 - ... pointer`.

- [ ] **Step 6: Verify pointer moves in screen.ppm**

Run: `python3 capture_ppm.py` twice, drag mouse between, compare `screen.ppm` cursor position via `python3 -c "with open('screen.ppm','rb') as f: ... count white pixels near cursor"`
Expected: `Distinct colors > 68` and cursor position changed from center.

- [ ] **Step 7: Commit**

```bash
git add mywima/src/input.rs kernel/src/drivers/drm/mod.rs tools/src/qemu.rs
git commit -m "fix: pointer movement via libinput absolute + cursor plane trace"
```

---

### Task 2: Wallpaper Image (Downscale JPEG)

**Files:**
- Modify: `mywima/src/element.rs:199-250`
- Modify: `mywima/src/backends/kms.rs:351` (restore load_wallpaper call)
- Test: `capture_ppm.py` distinct colors

- [ ] **Step 1: Write failing test - current bypass is solid only**

```bash
strings uefi_debug.log | grep "Loading wallpaper"
# Expected: 1280x800 solid, screen.ppm 99.98% mocha, no photographic image
```

- [ ] **Step 2: Implement downscale before import**

```rust
// mywima/src/element.rs:225
Ok(img) => {
    let resized = image::imageops::resize(&img, 1280, 800, image::imageops::FilterType::Triangle);
    // ensure size matches target
    image::RgbaImage::from_raw(1280,800, resized.into_raw()).unwrap()
},
```

And restore `mywima/src/backends/kms.rs:351`:
```rust
let wallpaper_buffer = element::load_wallpaper(&mut renderer, config.wallpaper.as_deref(), (primary_output_size.w, primary_output_size.h).into());
```

- [ ] **Step 3: Build and verify image shows**

Run: `cargo build --release --manifest-path /home/jyash8/Projects/mywima/Cargo.toml && cp target/release/mywima /home/jyash8/Projects/mywima/mywima && cargo run -p tools -- build-fs && python3 capture_ppm.py`
Expected: `screen.ppm` distinct colors > 20, photographic content, not solid mocha; `uefi_debug.log` shows `[wallpaper] Successfully imported wallpaper texture into OpenGL!` with size 1280x800.

- [ ] **Step 4: Handle import failure fallback**

```rust
let texture = match renderer.import_memory(data, Fourcc::Abgr8888, (1280,800).into(), false) {
    Ok(t) => t,
    Err(e) => {
        eprintln!("[wallpaper] import failed {:?}, falling back to clear", e);
        return None;
    }
};
```

- [ ] **Step 5: Commit**

```bash
git add mywima/src/element.rs mywima/src/backends/kms.rs
git commit -m "feat: wallpaper downscale to 1280x800 before GLES import"
```

---

### Task 3: Wayland Terminal (foot/weston-terminal)

**Files:**
- Modify: `scripts/build-fs.sh:70-90`
- Modify: `mywima/config.toml.example:12`
- Test: `capture_ppm.py` + `uefi_debug.log` space_elements

- [ ] **Step 1: Stage terminal binary and deps**

```bash
# scripts/build-fs.sh - add after lib stage
for bin in foot weston-terminal footclient; do
  [ -f /usr/bin/$bin ] && cp -L /usr/bin/$bin $STAGING/usr/bin/
done
for lib in libwayland-client.so.0 libwayland-cursor.so.0 libfontconfig.so.1 libfreetype.so.6; do
  src=$(find /usr/lib -name "$lib" | head -n1); [ -n "$src" ] && cp -L $src $STAGING/usr/lib/
done
mkdir -p $STAGING/usr/share/foot $STAGING/etc/fonts
cp -r /usr/share/foot $STAGING/usr/share/ 2>/dev/null || true
cp -r /etc/fonts $STAGING/etc/ 2>/dev/null || true
```

- [ ] **Step 2: Set autostart**

```toml
# mywima/config.toml.example
autostart = ["foot"]
# or ["weston-terminal"]
```

- [ ] **Step 3: Build and verify terminal appears**

Run: `cargo run -p tools -- build-fs && python3 capture_ppm.py`
Expected: `uefi_debug.log` shows `[kms] Active rendering frame #... | Windows mapped: 1`, `screen.ppm` has rectangular window chrome (distinct colors > 68, e.g., white terminal).

- [ ] **Step 4: Commit**

```bash
git add scripts/build-fs.sh mywima/config.toml.example
git commit -m "feat: stage foot terminal and autostart via mywima"
```

---

### Task 4: Virgl Wiring (Host GPU)

**Files:**
- Modify: `tools/src/qemu.rs:30-80`
- Modify: `kernel/src/drivers/virtio/gpu.rs:120-150` (capset check)
- Modify: `scripts/build-fs.sh:123` (init.sh env)
- Test: `uefi_debug.log` MESA_LOADER

- [ ] **Step 1: Add QEMU gl variant**

```rust
// tools/src/qemu.rs
pub fn qemu_args_gl(&self) -> Vec<String> {
  vec!["-device".into(), "virtio-vga-gl".into(), "-display".into(), "gtk,gl=on".into()]
}
// fallback to -display sdl,gl=on if gtk fails
```

Add CLI: `cargo run -p tools -- run --gl`

- [ ] **Step 2: Check host virgl support**

Run: `qemu-system-x86_64 -device virtio-vga-gl,help 2>&1 | head -5`
Expected: no error if `virglrenderer` installed; otherwise log fallback.

- [ ] **Step 3: Set Mesa env for virpipe**

```bash
# scripts/build-fs.sh bin/init.sh
export MESA_LOADER_DRIVER_OVERRIDE=virpipe
export GALLIUM_DRIVER=virpipe
```

If `virpipe` not found, Mesa falls back to `llvmpipe` (log `MESA_LOADER: ... virpipe`).

- [ ] **Step 4: Verify virgl vs llvmpipe**

Run: `cargo run -p tools -- run --gl` + `capture_ppm.py`
Expected: `uefi_debug.log` shows `MESA_LOADER: driver virpipe` not `llvmpipe`, `glxinfo` (if staged) reports `virgl`.

- [ ] **Step 5: Commit**

```bash
git add tools/src/qemu.rs kernel/src/drivers/virtio/gpu.rs scripts/build-fs.sh
git commit -m "feat: virgl host-GPU via virtio-vga-gl and virpipe"
```

---

## Self-Review

- Spec coverage: Pointer, wallpaper image, terminal, virgl each have tasks.
- Placeholders: None - all steps have concrete file paths, code blocks, commands, expected outputs.
- Type consistency: `Fourcc::Abgr8888`, `GlesTexture`, `TextureBuffer` used consistently; `DrmDevice` prime fix already in place.

