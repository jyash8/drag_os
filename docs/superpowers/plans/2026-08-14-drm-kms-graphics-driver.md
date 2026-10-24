# DRM / KMS Graphics Driver & Intel/AMD Porting Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a complete Direct Rendering Manager (DRM) and Kernel Mode Setting (KMS) graphics subsystem in DragOS, enabling `/dev/dri/card0` and `/dev/dri/renderD128` character devices, dumb buffer allocation, mode-setting, page flipping, VirtIO-GPU 2D/3D hardware acceleration, and the LinuxKPI firmware/DMA foundation for Intel (`i915`/`xe`) and AMD (`amdgpu`) graphics drivers.

**Architecture:** 
1. **DRM/KMS Core (`kernel/src/drivers/drm/`)**: Implements standard Linux DRM ioctls (`DRM_IOCTL_MODE_*`, `DRM_IOCTL_VERSION`, dumb buffers, `mmap` mapping, connector/CRTC enumeration, and page flipping).
2. **Generic Framebuffer DRM Bridge**: Wraps UEFI GOP and linear MMIO framebuffers as standard DRM connectors/CRTCs so all DRM userspace applications run immediately on any system.
3. **VirtIO-GPU Driver (`kernel/src/drivers/virtio/gpu.rs`)**: Provides 2D and VirGL 3D host-accelerated command stream submission over VirtIO PCI queues in QEMU/KVM.
4. **Firmware & DMA Subsystem (`kernel/src/fs/firmware.rs`, `kernel/src/memory/dma.rs`)**: Provides binary microcode loading (`request_firmware`) and scatter-gather physical page mapping (`sg_table`) required by modern Intel and AMD GPUs.
5. **LinuxKPI DRM Shim (`kernel/src/drivers/linuxkpi/`)**: Implements Linux kernel DRM abstractions (`dma_buf`, `dma_fence`, `struct drm_device`, `GEM`, `TTM`) for porting Linux GPU drivers.

**Tech Stack:** Rust (`no_std`), x86_64 MMIO, PCI Bus, Linux DRM/KMS ABI (Major 226), VirtIO 1.0 GPU specification, Userspace `libdrm` & `kmscube`.

---

## Global Constraints

- Kernel code is strictly `no_std` Rust.
- DRM ABI must precisely match Linux `include/uapi/drm/drm.h` and `drm_mode.h` (ioctl numbers, struct layouts, bitflags).
- Memory allocations for GPU buffers must be physical page-aligned (4096-byte boundaries) and properly tracked in process VMA.
- All lock acquisitions must follow hierarchy to prevent deadlocks across SMP cores.

---

## Tasks

### Task 1: DRM Core & Linux UAPI Definitions

**Files:**
- Create: `kernel/src/drivers/drm/mod.rs`
- Create: `kernel/src/drivers/drm/uapi.rs`
- Create: `kernel/src/drivers/drm/types.rs`
- Modify: `kernel/src/drivers/mod.rs`
- Modify: `kernel/src/fs/vfs.rs:30-80`

**Interfaces:**
- Consumes: `kernel/src/memory/space.rs` (VMA, mmap), `kernel/src/fs/vfs.rs` (Device node registration)
- Produces: `drm::drm_open()`, `drm::drm_ioctl()`, `drm::drm_mmap()`, `/dev/dri/card0`, `/dev/dri/renderD128`

- [ ] **Step 1: Write UAPI constants and struct definitions**
  - Implement `DRM_IOCTL_VERSION` (0xC0406400), `DRM_IOCTL_GET_CAP` (0xC010640C).
  - Implement `DRM_IOCTL_MODE_GETRESOURCES` (0xC04064A0), `DRM_IOCTL_MODE_GETCRTC` (0xC06864A1), `DRM_IOCTL_MODE_SETCRTC` (0xC06864A2).
  - Implement `DRM_IOCTL_MODE_GETCONNECTOR` (0xC05064A7), `DRM_IOCTL_MODE_ADDFB` (0xC01C64AE), `DRM_IOCTL_MODE_ADDFB2` (0xC04464B8).
  - Implement `DRM_IOCTL_MODE_RMFB` (0xC00464AF), `DRM_IOCTL_MODE_PAGE_FLIP` (0xC01864B0).
  - Implement `DRM_IOCTL_MODE_CREATE_DUMB` (0xC02064B2), `DRM_IOCTL_MODE_MAP_DUMB` (0xC01064B3), `DRM_IOCTL_MODE_DESTROY_DUMB` (0xC00464B4).

- [ ] **Step 2: Create DRM device node registration in VFS**
  - Register `/dev/dri` directory in VFS root.
  - Register `/dev/dri/card0` (major 226, minor 0) character device.
  - Register `/dev/dri/renderD128` (major 226, minor 128) character device.

- [x] **Step 3: Implement DRM Dumb Buffer Allocator**
  - Implement `DumbBuffer` backed by physically contiguous/scattered frames via `FRAME_ALLOCATOR`.
  - Implement `DRM_IOCTL_MODE_CREATE_DUMB`: assigns handle ID, calculates pitch and size.
  - Implement `DRM_IOCTL_MODE_MAP_DUMB`: returns mmap fake offset (`0x1000_0000 + handle * 0x100_0000`).
  - Implement `DRM_IOCTL_MODE_DESTROY_DUMB`: frees backing physical frames.

- [x] **Step 4: Connect DRM VMA mmap handler**
  - In `sys_mmap` and page fault handler, recognize DRM mmap offsets and map buffer physical frames to user virtual address space with `PRESENT | WRITABLE | USER` flags.

- [x] **Step 5: Verify with userspace test**
  - Write a standalone test program `initrd/drm_test.c` that opens `/dev/dri/card0`, queries version, allocates a dumb buffer, maps it with `mmap`, writes RGB test pixels, and frees it.

---

### Task 2: Generic KMS Framebuffer Bridge & Mode-Setting

**Files:**
- Create: `kernel/src/drivers/drm/kms.rs`
- Create: `kernel/src/drivers/drm/gop_bridge.rs`
- Modify: `kernel/src/drivers/drm/mod.rs`
- Modify: `kernel/src/graphics/framebuffer.rs`

**Interfaces:**
- Consumes: `kernel/src/graphics/framebuffer.rs` (Boot framebuffer info: width, height, stride, phys_addr)
- Produces: KMS Objects: 1 Connector (eDP-1/HDMI-A-1), 1 Encoder, 1 CRTC, Dumb scanout planes

- [x] **Step 1: Define KMS Object Hierarchy in Rust**
  - `struct DrmConnector`: ID, type (HDMI/eDP/Virtual), connection status (Connected), modes list (e.g. 1280x800@60Hz, 1920x1080@60Hz).
  - `struct DrmEncoder`: ID, supported CRTCs bitmask.
  - `struct DrmCrtc`: ID, current framebuffer ID, active mode, gamma size.
  - `struct DrmFramebuffer`: ID, width, height, pitch, bpp, handle.

- [x] **Step 2: Implement KMS Discovery IOCTLs**
  - `DRM_IOCTL_MODE_GETRESOURCES`: returns count and array of CRTC IDs, Connector IDs, Encoder IDs, and Framebuffer IDs.
  - `DRM_IOCTL_MODE_GETCONNECTOR`: returns display resolution modes (width, height, vrefresh, clock), connection status, encoder ID.
  - `DRM_IOCTL_MODE_GETENCODER`: returns encoder type and CRTC mask.
  - `DRM_IOCTL_MODE_GETCRTC`: returns current X, Y, width, height, mode, and FB ID.

- [x] **Step 3: Implement Mode-Setting & Double-Buffer Page Flipping**
  - `DRM_IOCTL_MODE_ADDFB2`: registers dumb buffer handle as a displayable framebuffer ID.
  - `DRM_IOCTL_MODE_SETCRTC`: points CRTC scanout to requested framebuffer ID.
  - `DRM_IOCTL_MODE_PAGE_FLIP`: performs vsync/timer-synchronized buffer flip and sends `DRM_EVENT_VBLANK` event back to waiting userspace via `/dev/dri/card0` read.

- [x] **Step 4: Verify KMS with `drm_info` or userspace test**
  - Build `initrd/drm_test.c`: opens `/dev/dri/card0`, queries resources, selects primary mode, creates dumb framebuffer, maps it, and performs mode setting.

---

### Task 3: VirtIO-GPU 2D Hardware Driver

**Files:**
- Create: `kernel/src/drivers/virtio/gpu.rs`
- Modify: `kernel/src/drivers/virtio/mod.rs`
- Modify: `kernel/src/drivers/pci.rs:50-100`

**Interfaces:**
- Consumes: `kernel/src/drivers/virtio/mod.rs` (Virtqueue management, PCI capability navigation)
- Produces: `VirtioGpuDriver`, Hardware-accelerated 2D host presentation

- [x] **Step 1: Define VirtIO-GPU Header Protocols**
  - Header types: `VIRTIO_GPU_CMD_GET_DISPLAY_INFO`, `VIRTIO_GPU_CMD_RESOURCE_CREATE_2D`, `VIRTIO_GPU_CMD_RESOURCE_UNREF`, `VIRTIO_GPU_CMD_SET_SCANOUT`, `VIRTIO_GPU_CMD_RESOURCE_FLUSH`, `VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D`, `VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING`.
  - Format enums: `VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM`, `VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM`.

- [x] **Step 2: Implement VirtIO-GPU PCI Device Probe**
  - Detect PCI Vendor `0x1af4` and Device `0x1050` (VirtIO GPU).
  - Setup control virtqueue (Queue 0) and cursor virtqueue (Queue 1).
  - Send `VIRTIO_GPU_CMD_GET_DISPLAY_INFO` to retrieve host screen geometry and EDID.

- [x] **Step 3: Implement VirtIO-GPU 2D Commands**
  - `create_2d_resource(res_id, width, height, format)`: sends `RESOURCE_CREATE_2D`.
  - `attach_backing(res_id, pages)`: binds physical guest RAM frames to the host resource.
  - `set_scanout(scanout_id, res_id, rect)`: maps host display output to the GPU resource.
  - `flush_resource(res_id, rect)`: transfers guest pixel updates directly to host GPU window/VRAM.

- [x] **Step 4: Integrate VirtIO-GPU into DRM Core**
  - When VirtIO-GPU is detected, register it as the primary backend for `/dev/dri/card0`.
  - Route `PAGE_FLIP` and `ADDFB2` through VirtIO-GPU command queues.

- [ ] **Step 5: Verify in QEMU (`-device virtio-gpu-pci`)**
  - Run QEMU with `-device virtio-gpu-pci` and verify hardware scanout and fluent page flipping.

---

### Task 4: Firmware Loading Subsystem (`request_firmware`)

**Files:**
- Create: `kernel/src/fs/firmware.rs`
- Modify: `kernel/src/fs/mod.rs`
- Modify: `kernel/src/lib.rs`

**Interfaces:**
- Consumes: `kernel/src/fs/vfs.rs` (File reading from `/lib/firmware/`)
- Produces: `pub fn request_firmware(path: &str) -> Result<Vec<u8>, FirmwareError>`

- [x] **Step 1: Implement kernel firmware loader**
  - Lookup path `/lib/firmware/<path>` in root filesystem.
  - Read binary microcode into kernel memory.
  - Cache loaded firmware in memory to prevent repeated disk I/O.

- [x] **Step 2: Populate standard test firmware**
  - Create directory `initrd/lib/firmware/`.
  - Verify `request_firmware("amdgpu/navi10_pfp.bin")` and `request_firmware("i915/kbl_dmc_ver1_04.bin")` locate and read binary files correctly.

---

### Task 5: Scatter-Gather DMA Memory Manager (`sg_table` / `dma_buf`)

**Files:**
- Create: `kernel/src/memory/dma.rs`
- Modify: `kernel/src/memory/mod.rs`
- Modify: `kernel/src/drivers/drm/types.rs`

**Interfaces:**
- Consumes: `kernel/src/memory/mod.rs` (`FRAME_ALLOCATOR`)
- Produces: `struct SgTable`, `struct DmaBuf`, `dma_alloc_coherent()`, `dma_map_sg()`

- [x] **Step 1: Implement Scatter-Gather Memory Table**
  - `struct SgEntry`: physical address (`PhysAddr`), length, DMA bus address.
  - `struct SgTable`: vector of `SgEntry` representing non-contiguous physical pages allocated for GPU buffer objects (textures, vertices, shaders).

- [x] **Step 2: Implement DMA-BUF Handle Sharing**
  - Implement `DRM_IOCTL_PRIME_HANDLE_TO_FD` (0xC00C642D) and `DRM_IOCTL_PRIME_FD_TO_HANDLE` (0xC00C642E).
  - Allows zero-copy GPU buffer sharing between userspace processes (Wayland compositor, Mesa 3D clients, video decoders).

---

### Task 6: LinuxKPI DRM Compatibility Subsystem for Intel/AMD

**Files:**
- Create: `kernel/src/drivers/linuxkpi/mod.rs`
- Create: `kernel/src/drivers/linuxkpi/mutex.rs`
- Create: `kernel/src/drivers/linuxkpi/workqueue.rs`
- Create: `kernel/src/drivers/linuxkpi/fence.rs`
- Create: `kernel/src/drivers/linuxkpi/gem.rs`
- Modify: `kernel/src/drivers/mod.rs`

**Interfaces:**
- Consumes: `kernel/src/tasks.rs` (Thread blocking/waking), `kernel/src/smp.rs` (SMP locks)
- Produces: Linux C/Rust API bindings for `i915` and `amdgpu` drivers

- [x] **Step 1: Implement Asynchronous GPU Fences (`dma_fence`)**
  - `struct DmaFence`: tracks GPU command completion, signals callbacks when GPU interrupt fires.
  - Implement `sync_file` file descriptor export.

- [x] **Step 2: Implement Linux Workqueues & Deferred Tasks**
  - `struct WorkQueue`: kernel worker threads that execute GPU bottom-half routines, interrupt completion, and display hotplug detection.

- [x] **Step 3: Implement GEM Memory Management**
  - Object lifecycle: `drm_gem_object_init`, `drm_gem_object_lookup`, `drm_gem_object_release`.
  - Physical memory pinning for GPU translation tables (PPGTT / GART / GPUVM).

---

### Task 7: Userspace `libdrm` & 3D Benchmark (`kmscube`)

**Files:**
- Create: `tools/build-libdrm.sh`
- Create: `tools/build-kmscube.sh`
- Modify: `scripts/build-fs.sh`

**Interfaces:**
- Consumes: `/dev/dri/card0`, `/dev/dri/renderD128`
- Produces: `/usr/lib/libdrm.so`, `/usr/bin/drm_info`, `/usr/bin/kmscube`

- [ ] **Step 1: Cross-compile `libdrm` for DragOS**
  - Compile `libdrm` (version 2.4.120) with musl-cross toolchain targeting DragOS.
  - Install `libdrm.so.2` and `libdrm_radeon.so`, `libdrm_intel.so` to `initrd/usr/lib/`.

- [ ] **Step 2: Cross-compile `kmscube` & `drm_info`**
  - Compile `drm_info` to inspect and dump connector/CRTC modes.
  - Compile `kmscube` to demonstrate 60 FPS hardware mode-setting and page flipping.

- [ ] **Step 3: Run Full Validation in QEMU**
  - Launch DragOS in QEMU.
  - Execute `drm_info` and verify accurate device detection.
  - Execute `kmscube` and verify smooth animation and tear-free double-buffering.

---

## Verification Plan

### Automated Regression Testing
- Run `./scripts/smoke.sh` to ensure all existing kernel subsystems, SMP, VFS, and audio tests pass.
- Run `cargo test --package kernel` for unit tests.

### DRM & KMS Hardware Verification
- Run `drm_test` in QEMU to verify dumb buffer creation, mapping, and deletion.
- Run `kms_test` in QEMU to verify resolution enumeration, CRTC mode setting, and 60 FPS page flipping.
- Test under both UEFI GOP (`-vga std`) and VirtIO GPU (`-device virtio-gpu-pci`).
