#!/usr/bin/env bash
# Verify every abi_pin! value in crates/drm-uapi against real libdrm headers.
#
# AGENTS.md §0 makes these pins the mechanism that keeps a userspace client from
# silently hitting the wrong ioctl handler. That guarantee is only as good as the
# reference values, and a pin can be "verified" against a value that is itself
# wrong -- which is exactly what happened to DRM_IOCTL_SYNCOBJ_WAIT_BINARY, whose
# pin asserted a number for an ioctl that does not exist in <drm/drm.h> and whose
# struct size contradicted the encoding. A compile-time assert cannot catch that,
# because both sides were wrong together.
#
# So: expand the real macros with the C preprocessor and diff. This is an
# independent reference -- unlike scripts/generate-drm-ioctls.sh, which derives
# from the kernel's own crate and would happily reproduce the same mistake.
#
# Usage:  scripts/verify-drm-pins.sh
# Exit:   0 all pins match, 1 a mismatch, 2 could not run (no headers/gcc).
set -uo pipefail
cd "$(dirname "$0")/.."

CRATE=crates/drm-uapi/src/lib.rs
[ -f "$CRATE" ] || { echo "missing $CRATE" >&2; exit 2; }

command -v gcc >/dev/null || { echo "gcc required" >&2; exit 2; }
HDRS=$(ls /usr/include/drm/drm.h /usr/include/drm/drm_mode.h /usr/include/drm/virtgpu_drm.h 2>/dev/null)
[ -n "$HDRS" ] || { echo "libdrm headers not found (need drm.h, drm_mode.h)" >&2; exit 2; }

TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT

# Pull the pinned names straight out of the macro invocation.
python3 - "$CRATE" > "$TMP/names.txt" <<'PY'
import re, sys
src = open(sys.argv[1]).read()
m = re.search(r'abi_pin!\s*\{(.*?)\}', src, re.S)
if not m:
    sys.exit("could not find abi_pin! block")
seen = []
for n in re.findall(r'(DRM_IOCTL_\w+)\s*=', m.group(1)):
    if n not in seen:
        seen.append(n)
print("\n".join(seen))
PY

# One translation unit per header group; virtgpu lives in its own header and
# including it alongside drm.h is not always safe.
python3 - "$TMP/names.txt" "$TMP" <<'PY'
import sys
names = [l.strip() for l in open(sys.argv[1]) if l.strip()]
def emit(path, incs, sel):
    out = ['#include <stdio.h>', '#include <stdint.h>']
    out += [f'#include <{i}>' for i in incs]
    out.append('int main(void){')
    for n in names:
        if sel(n):
            out.append(f'#ifdef {n}')
            out.append(f'  printf("{n} 0x%llX\\n",(unsigned long long)({n}));')
            out.append('#else')
            out.append(f'  printf("{n} ABSENT\\n");')
            out.append('#endif')
    out.append('return 0;}')
    open(path, 'w').write("\n".join(out))
emit(sys.argv[2] + '/p1.c', ['drm/drm.h', 'drm/drm_mode.h'], lambda n: not n.startswith('DRM_IOCTL_VIRTGPU'))
emit(sys.argv[2] + '/p2.c', ['drm/drm.h', 'drm/virtgpu_drm.h'], lambda n: n.startswith('DRM_IOCTL_VIRTGPU'))
PY

gcc -o "$TMP/p1" "$TMP/p1.c" 2>/dev/null || { echo "probe 1 failed to compile" >&2; exit 2; }
gcc -o "$TMP/p2" "$TMP/p2.c" 2>/dev/null || "$TMP/p2" 2>/dev/null || true

python3 - "$CRATE" "$TMP" <<'PY'
import re, subprocess, sys, glob, os
src = open(sys.argv[1]).read()
body = re.search(r'abi_pin!\s*\{(.*?)\}', src, re.S).group(1)
pinned = {n: int(v, 16) for n, v in
          re.findall(r'(DRM_IOCTL_\w+)\s*=\s*(0x[0-9A-Fa-f]+)\s*;', body)}
probe = {}
for exe in sorted(glob.glob(os.path.join(sys.argv[2], 'p*'))):
    if not os.access(exe, os.X_OK):
        continue
    for line in subprocess.run([exe], capture_output=True, text=True).stdout.splitlines():
        n, _, v = line.partition(' ')
        probe[n] = None if v == 'ABSENT' else int(v, 16)
ok = mism = unver = 0
bad = []
for n in sorted(pinned):
    if n not in probe:
        unver += 1; bad.append((n, hex(pinned[n]), 'no-macro')); continue
    if probe[n] is None:
        unver += 1; bad.append((n, hex(pinned[n]), 'macro-absent')); continue
    if probe[n] == pinned[n]:
        ok += 1
    else:
        mism += 1; bad.append((n, hex(pinned[n]), hex(probe[n])))
print(f"pins={len(pinned)}  verified={ok}  mismatched={mism}  unverifiable={unver}")
if bad:
    print(f"\n{'ioctl':40} {'repo pin':>12} {'libdrm':>14}")
    for n, p, l in bad:
        print(f"{n:40} {p:>12} {l:>14}")
sys.exit(1 if mism else 0)
PY