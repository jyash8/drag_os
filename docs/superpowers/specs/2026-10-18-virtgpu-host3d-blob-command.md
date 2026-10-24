# Host3D blob command payload — spec

**Status:** approved for implementation (2026-10-18)

## Problem

Under `gtk,gl=on` the host advertises `VIRGL_CAP_ARB_BUFFER_STORAGE` (v1 bit31).
DragOS reports `HOST_VISIBLE = 1` and `RESOURCE_BLOB = 1`, so guest Mesa's
`virgl` driver enables `caps.buffer_map_persistent_coherent`. Mesa's upload
manager then creates its staging buffers with
`PIPE_RESOURCE_FLAG_MAP_PERSISTENT | MAP_COHERENT`, which routes resource
creation through `virgl_drm_winsys_resource_create_blob` →
`DRM_IOCTL_VIRTGPU_RESOURCE_CREATE_BLOB` with a **command payload**
(`blob_mem = HOST3D`, `blob_flags = USE_MAPPABLE`, `cmd_size = 48`).

DragOS rejects every such request (`cmd_size != 0 → EINVAL`), so `u_upload`
fails and the compositor's draws are dropped — the render is black/unrendered.
Measured: E28, scratch/EXPERIMENTS.md §16.

## Goal

Accept and correctly service Mesa's HOST3D blob creation *with* a command
payload, so the persistent/coherent upload path works and bit31 can stay
advertised. The §15.3 bit31 mask then becomes unnecessary.

## Design

Mirror upstream `virtio_gpu_resource_create_blob_ioctl`
(`drivers/gpu/drm/virtio/virtgpu_ioctl.c`):

1. **Validation** follows `verify_blob()`:
   - host3d blobs (`HOST3D` = 0x0002, `HOST3D_GUEST` = 0x0003): `cmd_size % 4 == 0`.
   - guest blobs (`GUEST` = 0x0001): `blob_id == 0` and `cmd_size == 0`.
2. The `cmd` payload is **not** part of the CREATE_BLOB wire command. It is
   submitted first as an ordinary `VIRTIO_GPU_CMD_SUBMIT_3D` on the context
   (`virtio_gpu_cmd_submit(..., vfpriv->ctx_id, ...)`), then the blob is
   created. DragOS already has `GpuDriver::submit_3d(ctx_id, ring_idx, cmd_ptr,
   cmd_len, fence_id)`; it reads the command straight from the guest pointer.
3. The same context id must be used for the command submit and the blob create.
   Use the plain-submit rule already in `handle_virtgpu_execbuffer`: the
   smallest non-Venus context id.
4. Everything else already exists: `create_resource_blob` with no guest pages,
   `hostmem_map` → `RESOURCE_MAP_BLOB` (PCI BAR 4, `hostmem=256M`), and
   `handle_virtgpu_map` returning the hostmem-window offset.

## Constraints

- AGENTS.md §0 (ioctl ABI): the request struct and its field order are fixed by
  `crates/drm-uapi`; no new ioctl numbers.
- AGENTS.md §1 (lock order): `GLOBAL_VIRTIO_GPU → VIRTGPU_MANAGER`. The handler
  must not hold `VIRTGPU_MANAGER` while locking `GLOBAL_VIRTIO_GPU`. New
  validation is a pure function with no locks.

## Validation

- Unit: the new pure validator, run in the guest test harness
  (`cargo test -p kernel --lib --target x86_64-unknown-none`).
- End to end: rebuild with `DRAGOS_CAPSET_KEEP=1` (bit31 advertised) and run
  `scratch/run_exp.py --traced` under `gtk,gl=on`. Success = the blob creates
  succeed, `resource_create_blob` successes > 0, and the guest command stream
  regains `cmd=6` (SET_VERTEX_BUFFERS) and `cmd=8` (DRAW_VBO).
- Ship: flip the default so bit31 is no longer masked, re-run, confirm draws
  and a correct, unsheared composite.

## Out of scope

- RESOURCE_UNMAP_BLOB / hostmem-window reclamation on blob destroy.
- Broader "GPU-support roadmap" items (fences, transfers, performance).
