# Host3D blob command payload Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `DRM_IOCTL_VIRTGPU_RESOURCE_CREATE_BLOB` accept Mesa's HOST3D blob requests *with a command payload*, submit that command as a 3D command on the context, and then create the blob — so `VIRGL_CAP_ARB_BUFFER_STORAGE` can stay advertised.

**Architecture:** Follow upstream Linux `virtio_gpu_resource_create_blob_ioctl`: the blob `cmd` is submitted as an ordinary `VIRTIO_GPU_CMD_SUBMIT_3D` on the client's context *before* the blob object is created. Validation mirrors `verify_blob()`. The context id is the smallest non-Venus context (the existing plain-submit rule). No new ioctls, no new device commands.

**Tech Stack:** Rust, `no_std` kernel (`x86_64-unknown-none`), guest-run unit tests, QEMU + virglrenderer.

**Spec:** `docs/superpowers/specs/2026-10-18-virtgpu-host3d-blob-command.md`
**Evidence:** `scratch/EXPERIMENTS.md` §16.

## Outcome (2026-10-18)

Implemented. `blob_create_validate` added to `virtgpu_ioctl.rs`;
`handle_virtgpu_resource_create_blob` now submits the HOST3D command payload as a
SUBMIT_3D before creating the blob, on the plain (non-Venus) context; the bit31
mask was removed (default `clr1 == 0`) and the spike scaffolding deleted.

Validated end-to-end (see `scratch/EXPERIMENTS.md` §16.5): with bit31 advertised
(E29/E31) and in the shipped default (E30), the blob create succeeds, draws
return (14× `cmd=6` + 14× `cmd=8`, matching the cap-cleared baseline) and the
99 `EINVAL` rejections of E28 are gone.

**Caveat:** the documented unit-test command
(`cargo test -p kernel --lib --target x86_64-unknown-none`) does not build on
this toolchain — the `test` crate needs `std`, which cannot be built for the
bare target (`-Z build-std`); the host build fails on a duplicate `panic_impl`.
Validation therefore relied on the guest end-to-end runs.

## Global Constraints

- Request struct/field order fixed by `crates/drm-uapi` (`DrmVirtgpuResourceCreateBlob`); do not change it.
- Lock order AGENTS.md §1: `GLOBAL_VIRTIO_GPU → VIRTGPU_MANAGER`. Never hold `VIRTGPU_MANAGER` while taking `GLOBAL_VIRTIO_GPU` (matters for the new submit in Task 2).
- Blob mem constants: `VIRTGPU_BLOB_MEM_GUEST = 0x0001`, `VIRTGPU_BLOB_MEM_HOST3D = 0x0002`, `VIRTGPU_BLOB_MEM_HOST3D_GUEST = 0x0003`. Mesa's upload-buffer command is `cmd_size = 48`.
- Unit tests: `cargo test -p kernel --lib --target x86_64-unknown-none` (boots QEMU via `scripts/test-kernel.sh`; expect `Tests passed!`).
- Kernel deploy: `cargo run -p tools -- build-kernel` (release; copies to `esp/efi/boot/kernel.elf`).
- Never `pkill qemu`; only the runner's own QEMU is terminated.

## Review Focus

- HOST3D blob with `cmd_size` **not** a multiple of 4 → `EINVAL`, nothing submitted.
- HOST3D blob with `cmd_size == 0` (plain blob) → must still succeed; do not require a payload.
- GUEST blob with `blob_id != 0` or `cmd_size != 0` → `EINVAL`.
- Unreadable `cmd` pointer → `EFAULT`, no submit, no blob created.
- `blob_id` passed through verbatim so the host binds the submitted `PIPE_RESOURCE_CREATE` to the blob it creates.

---

### Task 1: Blob-create validation (pure function)

**Files:**
- Modify: `crates/kernel/src/drivers/drm/virtgpu_ioctl.rs` (add function near `resource_create_validate`; add tests in the existing `mod tests`)
- Test: same file, `#[cfg(test)] mod tests`

**Interfaces:**
- Produces: `pub fn blob_create_validate(blob_mem: u32, blob_id: u64, cmd_size: u32) -> Result<(), ()>`

- [ ] **Step 1: Write the failing tests** (in `mod tests`)

```rust
#[test]
fn host3d_blob_allows_a_dword_aligned_command() {
    assert_eq!(blob_create_validate(0x0002, 7, 48), Ok(()));   // HOST3D + Mesa payload
    assert_eq!(blob_create_validate(0x0003, 7, 0), Ok(()));    // HOST3D_GUEST, no payload
}

#[test]
fn host3d_blob_rejects_unaligned_command() {
    assert!(blob_create_validate(0x0002, 7, 46).is_err());     // not a multiple of 4
}

#[test]
fn guest_blob_forbids_command_and_blob_id() {
    assert_eq!(blob_create_validate(0x0001, 0, 0), Ok(()));
    assert!(blob_create_validate(0x0001, 0, 48).is_err());
    assert!(blob_create_validate(0x0001, 9, 0).is_err());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p kernel --lib --target x86_64-unknown-none`
Expected: FAIL to compile — `blob_create_validate` not found.

- [ ] **Step 3: Implement the validator**

Add to `virtgpu_ioctl.rs` (near `resource_create_validate`), using the `drm_uapi` constants:

```rust
/// `verify_blob` (virtgpu_ioctl.c:487-547), the per-mem-type rules.
///
/// host3d blobs may carry a dword-aligned command payload (the virgl
/// PIPE_RESOURCE_CREATE submitted just before the blob is created). Guest
/// blobs may not: they carry no command and no blob_id.
pub fn blob_create_validate(blob_mem: u32, blob_id: u64, cmd_size: u32) -> Result<(), ()> {
    use drm_uapi::{VIRTGPU_BLOB_MEM_GUEST, VIRTGPU_BLOB_MEM_HOST3D, VIRTGPU_BLOB_MEM_HOST3D_GUEST};
    if blob_mem == VIRTGPU_BLOB_MEM_GUEST {
        if blob_id != 0 || cmd_size != 0 {
            return Err(());
        }
    } else if blob_mem == VIRTGPU_BLOB_MEM_HOST3D || blob_mem == VIRTGPU_BLOB_MEM_HOST3D_GUEST {
        if cmd_size % 4 != 0 {
            return Err(());
        }
    } else {
        return Err(());
    }
    Ok(())
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p kernel --lib --target x86_64-unknown-none`
Expected: `Tests passed!`

- [ ] **Step 5: Commit** (the repo has no git tracking for `crates/`; commit only if the tree is tracked)

```bash
git add crates/kernel/src/drivers/drm/virtgpu_ioctl.rs
git commit -m "virtgpu: verify_blob rules for blob creation"
```

---

### Task 2: Handler accepts and submits the blob command

**Files:**
- Modify: `crates/kernel/src/drivers/drm/virtgpu.rs` — `handle_virtgpu_resource_create_blob` (≈2160-2366)
- Modify: `crates/kernel/src/drivers/drm/virtio/gpu.rs` — none expected; `submit_3d` already exists

**Interfaces:**
- Consumes: `blob_create_validate(blob_mem, blob_id, cmd_size) -> Result<(), ()>` (Task 1); `GpuDriver::submit_3d(ctx_id: u32, ring_idx: u8, cmd_ptr: u64, cmd_len: usize, fence_id: Option<u64>) -> (bool, Option<u64>)`.

- [ ] **Step 1: Replace the blanket rejection**

Delete the `if req.cmd_size != 0 { … return EINVAL; }` block and the spike's diagnostic log. After the existing blob_mem/flag/alignment checks, add:

```rust
if blob_create_validate(req.blob_mem, req.blob_id, req.cmd_size).is_err() {
    return EINVAL;
}
```

- [ ] **Step 2: Read and submit the command before creating the blob**

After the `size`/alignment checks and before `create_resource_blob`, insert (note: no `VIRTGPU_MANAGER` lock held here):

```rust
// The blob `cmd` is not part of CREATE_BLOB: upstream submits it as an
// ordinary SUBMIT_3D on the client's context first, then creates the blob.
// The blob_id in that command is what the host binds the blob to.
if req.cmd_size != 0 {
    let cmd_len = req.cmd_size as usize;
    if !vr_read(req.cmd, cmd_len) {
        return EFAULT;
    }
    let ok = {
        let mut gpu = GLOBAL_VIRTIO_GPU.lock();
        match gpu.as_mut() {
            Some(g) => g.submit_3d(ctx_id, 0, req.cmd, cmd_len, None).0,
            None => false,
        }
    };
    if !ok {
        crate::dprintln!("[VIRTGPU] create_blob: cmd submit failed for rid={}", res_id);
        return EINVAL;
    }
}
```

- [ ] **Step 3: Use one context id for submit and blob create**

Replace the existing Venus-preferring selection (`filter(|(_, c)| c.capset_id == VIRTIO_GPU_CAPSET_VENUS)…`) with the plain-submit rule already used by `handle_virtgpu_execbuffer` (smallest non-Venus context id):

```rust
let ctx_id = {
    let mgr = VIRTGPU_MANAGER.lock();
    mgr.contexts
        .iter()
        .filter(|(_, c)| c.capset_id != VIRTIO_GPU_CAPSET_VENUS)
        .map(|(id, _)| *id)
        .min()
        .unwrap_or(0)
};
```

Move this computation **above** the Step 2 submit so both use `ctx_id`. Keep the `GLOBAL_VIRTIO_GPU` lock scope in Step 2 separate from any `VIRTGPU_MANAGER` lock.

- [ ] **Step 4: Build the kernel**

Run: `cargo run -p tools -- build-kernel`
Expected: builds and deploys; no errors.

- [ ] **Step 5: End-to-end validation (bit31 advertised)**

Run: `DRAGOS_CAPSET_KEEP=1 cargo run -p tools -- build-kernel && python3 scratch/run_exp.py E29_blobcmd --traced`

Then inspect `/tmp/exp/E29_blobcmd/uefi_debug.log`:
- `capset FIX clr1=0x00000000 … cb1 0xdff7ffff->0xdff7ffff`
- `resource_create_blob bo_handle=… res_id=… size=…` successes **> 0** (≥ the 99 attempts E28 rejected)
- command-walker histogram contains **`cmd=6` and `cmd=8`** (SET_VERTEX_BUFFERS / DRAW_VBO)

Expected: all three hold. If any fails, stop and report before Task 3.

- [ ] **Step 6: Commit**

```bash
git add crates/kernel/src/drivers/drm/virtgpu.rs
git commit -m "virtgpu: submit HOST3D blob command payload before blob create"
```

---

### Task 3: Ship — stop masking bit31

**Files:**
- Modify: `crates/kernel/src/drivers/drm/virtgpu.rs` — `handle_virtgpu_get_caps` capset block (≈2095-2136)
- Modify: `scratch/EXPERIMENTS.md` (§15 status note + §16.5)

**Interfaces:**
- Consumes: the working HOST3D path from Task 2.

- [ ] **Step 1: Advertise the host capset verbatim**

Set the base mask to 0 and remove the `DRAGOS_CAPSET_KEEP` switch added for the spike, so `clr1` starts at `0` and only the `DRAGOS_CAPSET_CLR1` experiment hook can clear bits:

```rust
let mut clr1 = 0u32;              // host capset forwarded verbatim (§16 fix)
let mut set2 = 0u32;
```

Keep the `DRAGOS_CAPSET_EGLHEADLESS`/`CLR1`/`SET2` hooks.

- [ ] **Step 2: Build and validate the shipped default**

Run: `cargo run -p tools -- build-kernel && python3 scratch/run_exp.py E30_default --traced`
Expected: `capset FIX clr1=0x00000000 … cb1 0xdff7ffff->0xdff7ffff`; blob-create successes > 0; `cmd=6`/`cmd=8` present; screen shows the correct, unsheared composite (compare `frame_t*.png`).

- [ ] **Step 3: Regression check (CPU / no-virgl path)**

Run: `cargo run -p tools -- run --no-gl` (boot only) and confirm the guest still reaches the shell/desktop with no panics in the kernel log.

- [ ] **Step 4: Update the docs**

In `scratch/EXPERIMENTS.md`: mark §15.3 as *superseded by §16* and record the E29/E30 results. In `SHEAR-HANDOFF.md`, update the root-cause/fix section to point at §16.

- [ ] **Step 5: Commit**

```bash
git add crates/kernel/src/drivers/drm/virtgpu.rs scratch/EXPERIMENTS.md SHEAR-HANDOFF.md docs/
git commit -m "virtgpu: back ARB_buffer_storage via HOST3D blobs; keep bit31"
```
