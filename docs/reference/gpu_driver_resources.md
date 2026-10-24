# GPU Driver Integration Resources

## Linux DRM / KMS Subsystem
- **DRM Core Documentation**: https://www.kernel.org/doc/html/latest/gpu/drm-kms.html
- **GEM (Graphics Execution Manager)**: https://www.kernel.org/doc/html/latest/gpu/drm-mm.html
- **DRM UAPI Header**: https://github.com/torvalds/linux/blob/master/include/uapi/drm/drm_mode.h

## Intel GPU (i915 / Xe) Resources
- **Intel OpenSource Graphics Programmer's Reference Manuals (PRMs)**: https://01.org/linuxgraphics/documentation/hardware-specs-0
- **Intel GFX GitLab (Documentation)**: https://gitlab.freedesktop.org/drm/intel
- **OSDev Intel Graphics**: https://wiki.osdev.org/Intel_Graphics

## AMD GPU (AMDGPU) Resources
- **AMD Developer Central (GPU ISA and Docs)**: https://developer.amd.com/resources/developer-guides-manuals/
- **AMDGPU Linux Driver Source**: https://github.com/torvalds/linux/tree/master/drivers/gpu/drm/amd
- **OSDev AMD Graphics**: https://wiki.osdev.org/AMD_Graphics

## VirtIO-GPU
- **VirtIO-GPU Spec**: https://docs.oasis-open.org/virtio/virtio/v1.1/csprd01/virtio-v1.1-csprd01.html#x1-3220007
- **VirtIO-GPU Architecture**: https://www.kraxel.org/blog/2014/08/virtio-gpu-guest-driver/

## Local DragOS Files
- `kernel/src/drivers/drm/kms.rs`: KMS backend trait and GOP/Virtio bridge.
- `kernel/src/drivers/virtio/gpu.rs`: VirtIO-GPU driver implementation (queue commands).
- `kernel/src/drivers/drm/uapi.rs`: DRM UAPI structs (ioctls, mode objects).

## Implementation Tips
- **Phase 1**: Enumerate PCI ID `0x8086` (Intel) or `0x1002` (AMD). Map BAR0 to memory for MMIO register access.
- **Phase 2**: Use EDID reading via I2C (GMBUS for Intel). Implement PLL calculations for modesetting (clock routing).
- **Phase 3/4**: Implement GEM objects mapping page frames to GPU GTT (Graphics Translation Table).
