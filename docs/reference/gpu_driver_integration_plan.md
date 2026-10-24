# DragOS GPU Driver Integration Plan

## Overview
This plan outlines the integration of Intel/AMD GPU drivers into the DragOS kernel, building on the existing DRM/KMS infrastructure. The plan follows a phased approach from initial hardware bring-up to full modesetting support.

## Phased Implementation Plan

### Phase 1: Hardware Bring-Up (Weeks 1-2)
**Objective**: Bring up basic GPU hardware, enable MMIO access, and establish basic display output.

**Tasks**:
- PCI enumeration for Intel/AMD GPU devices
- MMIO region mapping and basic register access
- Interrupt setup (MSI/MSI-X or legacy)
- Power management initialization
- Basic GPU reset sequence

**Deliverables**:
- PCI can enumerate GPU devices
- MMIO reads/writes work without stalls
- Interrupts can be enabled/acknowledged
- GPU reset sequence documented and functional

### Phase 2: Basic Framebuffer (Weeks 3-4)
**Objective**: Get basic framebuffer visible on connected display.

**Tasks**:
- EDID reading via I2C/PCI
- Parse detailed timing descriptors
- Calculate pixel clocks and setup PLLs
- Allocate dumb buffers (2D resources)
- Program scanout registers
- Set up basic VESA/VESA-compatible modes

**Deliverables**:
- GPU boots with basic VESA 1024x768 mode
- Console appears on connected display
- Framebuffer is writeable from kernel space

### Phase 2b: Simple DRM ioctls (Weeks 5-6)
**Objective**: Basic DRM ioctl support for user-space.

**Tasks**:
- DRM_IOCTL_MODE_GETRESOURCES
- DRM_IOCTL_MODE_GETRESOURCE
- DRM_IOCTL_MODE_GETCONNECTORS
- DRM_IOCTL_MODE_GETENCODER
- DRM_IOCTL_MODE_GETCRTC
- DRM_IOCTL_MODE_GETPROPBLOB

**Deliverables**:
- Basic DRM ioctls work from user space
- Resources enumerated and found
- Connectors, CRTCs, encoders visible

### Phase 3: Full KMS Mode-Setting (Weeks 5-8)
**Objective**: Full KMS mode-setting pipeline.

**Tasks**:
- create_resource_2d() resource creation
- attach_backing() with page allocation
- set_scanout() with scanout ID tracking
- flush_resource() via virtqueue commands
- Pixel clock calculations and PLL setup
- refresh rate calculation from dot clock
- drm_mode_set_crtc full modesetting
- drm_mode_set_base_address setup
- drm_mode_config structure with CRTCs
- drm_plane structures for cursor/compositing

**Deliverables**:
- Full DRM/KMS backend fully functional
- Mode setting works: set_crtc succeeds
- Framebuffer objects created and mapped
- Scanout visible on display with proper resolution/refresh
- Buffer sharing works between processes

### Phase 4: Memory Management (Weeks 6-8)
**Objective**: GEM and buffer sharing infrastructure.

**4.1 GEM Object Lifetime**:
- gem_flink/gem_flink for name-based lookup
- gem_open/gem_flink for reference counting
- gem_vmap/gem_vunmap for CPU access

**4.2 Buffer Sharing & Sync**:
- dma_buf export/import
- Sync file fences (drm_fence operations)
- Waiting/ioctl synchronization

**Deliverables**:
- GEM objects survive context flushes
- dma_buf sharing works between processes
- Fences properly synchronize GPU/CPU operations

### Phase 5: Vendor-Specific Features (Weeks 9-10)
**Objective**: Vendor-specific feature implementation.

**5.1 Intel Specific**:
- i915-style command submission
- Enable display pipelines (pipe A/B/C)
- Enable transcoders (HDMI/DP/eDP)
- Enable ACP (Audio Codec Processor) for sound

**5.2 AMD Specific**:
- AMD display core initialization
- Set up DC (Display Controller) pipes
- Enable audio over HDMI/DP

**Deliverables**:
- Full modesetting pipeline functional
- Multi-monitor support (if hardware allows)
- Audio over HDMI/DP (if codec present)

### Phase 6: Testing & Robustness (Weeks 9-10)
**Objective**: Test suite integration and edge cases.

**6.1 Test Suite Integration**:
- GPU tests added to test harness
- Verify no regressions in existing tests
- Memory leak testing under load

**6.2 Edge Cases**:
- Hot-plug detection (GPU inserted/removed)
- Resume from suspend (DRM early/suspend)
- Concurrent access from multiple processes

**Deliverables**:
- Full test suite passes with GPU present
- No kernel panics during modeset changes
- Memory remains consistent under stress

## Final Integration Checklist

### Kernel Subsystem Integration
- drivers/gpu/ directory structure
- drivers/drm/ integration (already partially done)
- drivers/i2c/ for EDID/confidence EEPROMs
- drivers/pci/ GPU resource detection
- include/uapi/drm/ uapi headers (if distributing)

### Build System
- Kbuild rules for GPU driver compilation
- Makefile targets for make menuconfig
- Dependency declarations (drm, i2c, mfd)

### Documentation
- Documentation/gpu/ directory
- i915/amd specific notes
- API reference for driver writers

## Success Criteria

| Metric | Target |
|--------|--------|
| GPU boots on test hardware | Yes |
| Console visible on display | Yes |
| Mode set succeeds at least once | Yes |
| GEM objects survive flush | Yes |
| No kernel panics during modeset | Yes |
| Existing test suite passes | 6/6, 2/2 runs |

## Next Steps After This Plan

1. **Start with Phase 1** - Hardware bring-up is the foundation
2. **Daily milestones** - Track progress against the week-by-week schedule
3. **Weekly review** - Assess what's working, what needs adjustment
4. **Integration testing** - Ensure all subsystems work together before moving to next phase