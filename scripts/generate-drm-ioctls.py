#!/usr/bin/env python3
"""Emit the guest-side DRM ioctl numbers straight from the kernel's uapi crate.

The guest test programs used to carry hand-copied hex literals, which is how
initrd/drm_cursor_test.c ended up sending 0xC01864A3 for MODE_CURSOR while the
kernel expected 0xC01C64A3: the test passed because the *kernel* was wrong in
the same way, and both broke together. Generating the numbers from the single
source of truth makes that class of bug impossible.
"""
import re
import subprocess
import sys
import pathlib

ROOT = pathlib.Path(__file__).resolve().parents[1]
OUT = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else "initrd/drm_ioctls.h")

src = (ROOT / "crates/drm-uapi/src/lib.rs").read_text()
# Resolve the _IOC encoding ourselves rather than parsing the DSL, so the header
# always reflects the *computed* value.
SRC = src.replace("#![no_std]", "").replace("#![allow(unused)]", "")
SRC = re.sub(r"^#!\[allow\(non_upper_case_globals\)\]\n", "", SRC, flags=re.M)
TMP = pathlib.Path("/tmp/opencode/_drm_uapi_gen.rs")
TMP.write_text(SRC)

rs = r'''
include!("/tmp/opencode/_drm_uapi_gen.rs");
fn main() {
    macro_rules! p { ($($n:ident),*) => { $( println!("{} 0x{:08X}", stringify!($n), $n); )* } }
    p!(DRM_IOCTL_VERSION, DRM_IOCTL_GET_CAP, DRM_IOCTL_SET_CLIENT_CAP,
       DRM_IOCTL_GEM_CLOSE, DRM_IOCTL_AUTH_MAGIC, DRM_IOCTL_SET_MASTER,
       DRM_IOCTL_DROP_MASTER, DRM_IOCTL_MODE_GETRESOURCES, DRM_IOCTL_MODE_GETCRTC,
       DRM_IOCTL_MODE_SETCRTC, DRM_IOCTL_MODE_CURSOR, DRM_IOCTL_MODE_GETGAMMA,
       DRM_IOCTL_MODE_SETGAMMA, DRM_IOCTL_MODE_GETENCODER, DRM_IOCTL_MODE_GETCONNECTOR,
       DRM_IOCTL_MODE_GETPROPERTY, DRM_IOCTL_MODE_GETPROPBLOB, DRM_IOCTL_MODE_ADDFB,
       DRM_IOCTL_MODE_RMFB, DRM_IOCTL_MODE_PAGE_FLIP, DRM_IOCTL_MODE_DIRTYFB,
       DRM_IOCTL_MODE_CREATE_DUMB, DRM_IOCTL_MODE_MAP_DUMB, DRM_IOCTL_MODE_DESTROY_DUMB,
       DRM_IOCTL_MODE_GETPLANERESOURCES, DRM_IOCTL_MODE_GETPLANE, DRM_IOCTL_MODE_SETPLANE,
       DRM_IOCTL_MODE_ADDFB2, DRM_IOCTL_MODE_OBJ_GETPROPERTIES, DRM_IOCTL_MODE_ATOMIC,
       DRM_IOCTL_PRIME_HANDLE_TO_FD, DRM_IOCTL_PRIME_FD_TO_HANDLE,
       DRM_IOCTL_SYNCOBJ_CREATE, DRM_IOCTL_SYNCOBJ_DESTROY, DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD,
       DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE, DRM_IOCTL_SYNCOBJ_WAIT, DRM_IOCTL_SYNCOBJ_RESET,
       DRM_IOCTL_SYNCOBJ_SIGNAL, DRM_IOCTL_VIRTGPU_MAP, DRM_IOCTL_VIRTGPU_EXECBUFFER,
       DRM_IOCTL_VIRTGPU_GETPARAM, DRM_IOCTL_VIRTGPU_RESOURCE_CREATE,
       DRM_IOCTL_VIRTGPU_RESOURCE_INFO, DRM_IOCTL_VIRTGPU_TRANSFER_FROM_HOST,
       DRM_IOCTL_VIRTGPU_TRANSFER_TO_HOST, DRM_IOCTL_VIRTGPU_WAIT,
       DRM_IOCTL_VIRTGPU_GET_CAPS, DRM_IOCTL_VIRTGPU_RESOURCE_CREATE_BLOB,
       DRM_IOCTL_VIRTGPU_CONTEXT_INIT);
}
'''
TMP2 = pathlib.Path("/tmp/opencode/_drm_uapi_gen_main.rs")
TMP2.write_text(rs)
binary = pathlib.Path("/tmp/opencode/_drm_uapi_gen_bin")
subprocess.run(["rustc", "-O", "-o", str(binary), str(TMP2)], check=True)
out = subprocess.run([str(binary)], check=True, capture_output=True, text=True).stdout
rows = []
for line in out.splitlines():
    name, val = line.split()
    assert val.startswith("0x"), val
    rows.append((name, int(val, 16)))

body = "\n".join(f"#define {n:38s} 0x{v:08X}u" for n, v in rows)
OUT.write_text(
    f"""/* GENERATED FILE - do not edit.
 *
 * DRM ioctl numbers, emitted from crates/drm-uapi (see
 * scripts/generate-drm-ioctls.sh). Do not hand-copy these into a test
 * program: the guest and the kernel must agree on the exact value userspace
 * sends, and the kernel's own constants are the only place that is checked
 * against upstream Linux's headers.
 */
#pragma once

{body}
""")
print(f"wrote {OUT} ({len(rows)} numbers)")
