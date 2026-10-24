#!/usr/bin/env python3
"""Audit the kernel's virtio-gpu command structs against the Linux uapi.

These layouts are a wire ABI shared with QEMU. A wrong field order, a missing
pad or a different size is a *silent* misparse: the host still replies
OK_NODATA, so no log distinguishes "the command arrived" from "the host
understood it". That is precisely the failure being chased here, so the layouts
are checked rather than trusted.

Written after this file's author claimed an off-by-4 in virtio_gpu_cmd_submit
from memory. The claim was wrong: `virtio_gpu_ctrl_hdr_3d` (hdr + size, 28B) is
the legacy DRM_IOCTL_EXECBUFFER struct, while `virtio_gpu_cmd_submit`
(hdr + size + padding, 32B) is the virtio transport command this driver sends.
Guessing produced a confident wrong hypothesis; measuring is cheap.

Method:
  * read the struct bodies out of crates/kernel/src/drivers/virtio/gpu.rs with a
    brace matcher, so the real source is what gets checked (drift is caught);
  * compile them standalone and print size_of;
  * compile a C program against <linux/virtio_gpu.h> printing sizeof;
  * diff.
Structs the uapi header does not carry are reported as unchecked, not as OK.
"""
import re
import subprocess
import sys
import textwrap

GPU_RS = "/home/jyash8/Projects/drag_os/crates/kernel/src/drivers/virtio/gpu.rs"
OUT = "/tmp/opencode/vgpu_abi"

# kernel Rust name -> Linux uapi name. Only structs the driver transmits.
PAIRS = [
    ("VirtioGpuCtrlHdr", "virtio_gpu_ctrl_hdr"),
    ("VirtioGpuRect", "virtio_gpu_rect"),
    ("VirtioGpuResourceCreate2d", "virtio_gpu_resource_create_2d"),
    ("VirtioGpuResourceUnref", "virtio_gpu_resource_unref"),
    ("VirtioGpuSetScanout", "virtio_gpu_set_scanout"),
    ("VirtioGpuResourceFlush", "virtio_gpu_resource_flush"),
    ("VirtioGpuTransferToHost2d", "virtio_gpu_transfer_to_host_2d"),
    ("VirtioGpuResourceAttachBacking", "virtio_gpu_resource_attach_backing"),
    ("VirtioGpuMemEntry", "virtio_gpu_mem_entry"),
    ("VirtioGpuResourceCreateBlob", "virtio_gpu_resource_create_blob"),
    ("VirtioGpuResourceMapBlob", "virtio_gpu_resource_map_blob"),
    ("VirtioGpuSetScanoutBlob", "virtio_gpu_set_scanout_blob"),
    ("VirtioGpuUpdateCursor", "virtio_gpu_update_cursor"),
    ("VirtioGpuGetCapsetInfo", "virtio_gpu_get_capset_info"),
    ("VirtioGpuGetCapset", "virtio_gpu_get_capset"),
    ("VirtioGpuBox", "virtio_gpu_box"),
    ("VirtioGpuCtxCreate", "virtio_gpu_ctx_create"),
    ("VirtioGpuCtxDestroy", "virtio_gpu_ctx_destroy"),
    ("VirtioGpuCtxResource", "virtio_gpu_ctx_resource"),
    ("VirtioGpuResourceCreate3d", "virtio_gpu_resource_create_3d"),
    ("VirtioGpuCmdSubmit3d", "virtio_gpu_cmd_submit"),
]

# Present in the driver's headers but not exported by the uapi header; their
# layout is fixed by the virtio-gpu specification, not by <linux/virtio_gpu.h>.
NOT_IN_UAPI = {
    "VirtioGpuTransferHost3d": "virtio_gpu_3d_transfer_from_host",
    "VirtioGpuTransferToHost3d": "virtio_gpu_3d_transfer_to_host",
    "VirtioGpuPosition": "virtio_gpu_position (spec-only name)",
    "VirtioGpuGetEdid": "virtio_gpu_get_edid (spec-only name)",
}


def extract_structs(src):
    """Pull out `#[repr(C)] ... struct Name { ... }` blocks, brace-matched."""
    out = {}
    for m in re.finditer(r"#\[repr\(C\)\]", src):
        st = m.start()
        j = src.find("{", m.end())
        if j < 0:
            continue
        depth, k = 0, j
        while k < len(src):
            if src[k] == "{":
                depth += 1
            elif src[k] == "}":
                depth -= 1
                if depth == 0:
                    break
            k += 1
        body = src[st:k + 1]
        nm = re.search(r"struct\s+(\w+)", body)
        if nm:
            out[nm.group(1)] = body
    return out


def kernel_sizes():
    structs = extract_structs(open(GPU_RS).read())
    # The structs reference each other and nothing else once we drop derives.
    chunks = []
    for name, _ in PAIRS + [(k, None) for k in NOT_IN_UAPI]:
        body = structs.get(name)
        if body:
            chunks.append(body)
    if not chunks:
        return {}
    # Some drivers are generic over the allocator; not needed for these structs.
    lib = "\n\n".join(chunks)
    main = "fn main() {\n" + "\n".join(
        f'    println!("struct {n} size={{}}", std::mem::size_of::<{n}>());'
        for n, _ in PAIRS + [(k, None) for k in NOT_IN_UAPI]
        if n in structs
    ) + "\n}\n"
    open(f"{OUT}_lib.rs", "w").write(lib)
    open(f"{OUT}_main.rs", "w").write(f'#![allow(dead_code)]\ninclude!("{OUT}_lib.rs");\n{main}')
    r = subprocess.run(["rustc", "-O", "--edition", "2021", "-o", f"{OUT}_rs",
                        f"{OUT}_main.rs"], capture_output=True, text=True)
    if r.returncode != 0:
        print("rust probe failed:\n" + "\n".join(r.stderr.splitlines()[:20]))
        return {}
    res = subprocess.run([f"{OUT}_rs"], capture_output=True, text=True)
    sizes = {}
    for line in res.stdout.splitlines():
        m = re.match(r"struct (\w+) size=(\d+)", line)
        if m:
            sizes[m.group(1)] = int(m.group(2))
    return sizes


def uapi_sizes():
    lines = ["#include <stdio.h>", "#include <linux/virtio_gpu.h>",
             "int main(void) {"]
    for _, c in PAIRS:
        lines.append(f'    printf("struct {c} size=%zu\\n", sizeof(struct {c}));')
    lines.append("    return 0;\n}")
    open(f"{OUT}_c.c", "w").write("\n".join(lines))
    r = subprocess.run(["gcc", "-o", f"{OUT}_c", f"{OUT}_c.c"],
                       capture_output=True, text=True)
    if r.returncode != 0:
        print("c probe failed:\n" + "\n".join(r.stderr.splitlines()[:20]))
        return {}
    res = subprocess.run([f"{OUT}_c"], capture_output=True, text=True)
    sizes = {}
    for line in res.stdout.splitlines():
        m = re.match(r"struct (\w+) size=(\d+)", line)
        if m:
            sizes[m.group(1)] = int(m.group(2))
    return sizes


def main():
    k = kernel_sizes()
    u = uapi_sizes()
    if not k or not u:
        print("probe incomplete; refusing to report a verdict")
        return 2
    print(f"{'kernel struct':<32} {'uapi struct':<34} {'kernel':>6} {'uapi':>6}  verdict")
    print("-" * 96)
    bad = 0
    checked = 0
    for kr, cn in PAIRS:
        if kr not in k:
            print(f"{kr:<32} {cn:<34} {'-':>6} {u.get(cn,'?'):>6}  KERNEL STRUCT NOT FOUND")
            bad += 1
            continue
        if cn not in u:
            continue
        checked += 1
        ok = k[kr] == u[cn]
        bad += 0 if ok else 1
        note = "ok" if ok else f"MISMATCH (kernel is {k[kr]-u[cn]:+d})"
        print(f"{kr:<32} {cn:<34} {k[kr]:>6} {u[cn]:>6}  {note}")
    print()
    if NOT_IN_UAPI:
        print("not in the uapi header (spec-only), size for reference:")
        for kr, cn in NOT_IN_UAPI.items():
            if kr in k:
                print(f"  {kr:<32} {cn:<40} {k[kr]:>4}")
    print()
    print(f"{bad} mismatches across {checked} comparable structs")
    return 0 if bad == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
