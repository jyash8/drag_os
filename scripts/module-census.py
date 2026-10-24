#!/usr/bin/env python3
"""Relocation census: does our toolchain emit only what the relocator handles?

Implements docs/module-system.md §11.1 (Milestone 1). The point is to find out
*before* writing a relocator whether the §2.1 assumption -- that a module built
with `-C relocation-model=static -C code-model=kernel` emits only PC32, PLT32,
64 and 32S, with no GOT relocations -- holds for our rustc.

Two assertions, both required:

  observed ⊆ handled   a relocation type the relocator does not list is a hard
                        failure. §8.2: "a skipped relocation is a corruption you
                        invent", so an unlisted type must never load.

  expected ⊆ observed  every type the fixture claims to exercise must actually
                        appear. Without this the test is vacuous: it passes
                        happily when optimisation deletes a construct, and a
                        census that has quietly stopped covering a case is worse
                        than no census because it still reports success.

That second assertion is not hypothetical. Building this fixture initially
produced three of the four types, because an unread `static mut` holding a
function pointer was constant-folded away at `opt-level=2` and took the only
`R_X86_64_64` with it.

Exit status 0 on pass, 1 on failure, so it can gate a build.
"""

import re
import subprocess
import sys
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
FIXTURE = ROOT / "modules" / "fixture" / "lib.rs"
TABLE = ROOT / "modules" / "reloc_types.txt"
OUT = Path("/tmp/dragos-module-census")

# The flags §5.1 specifies for a module build. Deliberately passed on the
# rustc command line rather than inherited from .cargo/config.toml: the kernel's
# config would otherwise apply, and the fixture must be built exactly as a
# module would be, independently of how the kernel itself is built.
RUSTFLAGS = [
    "--edition", "2024",
    "--crate-type", "lib",
    "--target", "x86_64-unknown-none",
    "-C", "opt-level=2",
    "-C", "relocation-model=static",
    "-C", "code-model=kernel",
]

# Types the fixture is built to provoke. Kept next to the fixture rather than
# derived from it: this is the coverage claim being tested, so it has to be
# written down independently to be worth anything.
EXPECTED = {
    "R_X86_64_PC32",
    "R_X86_64_PLT32",
    "R_X86_64_64",
    "R_X86_64_32S",
}

# Relocations that would mean the no-GOT assumption is false. Any of these
# appearing is a failure even though the file would still parse: a skipped
# GOTPCREL leaves a live pointer to garbage that only faults on dereference.
FORBIDDEN = re.compile(r"GOTPCREL|REX_GOTPCRELX|R_X86_64_ROR|R_X86_64_GOTPCRELX")


def load_table():
    """The relocator's declared coverage, from the single source of truth."""
    names = []
    for line in TABLE.read_text().splitlines():
        line = line.split("#", 1)[0].strip()
        if line:
            names.append(line)
    return set(names)


def build_fixture():
    OUT.mkdir(parents=True, exist_ok=True)
    obj = OUT / "fixture.o"
    cmd = ["rustc", *RUSTFLAGS, "--emit=obj", "-o", str(obj), str(FIXTURE)]
    r = subprocess.run(cmd, capture_output=True, text=True)
    if r.returncode != 0:
        print("FAIL: fixture did not build")
        print(r.stderr[:4000])
        return None
    return obj


def relocations(obj):
    r = subprocess.run(["readelf", "-r", str(obj)], capture_output=True, text=True)
    if r.returncode != 0:
        print("FAIL: readelf -r failed")
        print(r.stderr[:2000])
        return None
    return r.stdout


def main():
    handled = load_table()
    print(f"relocator table ({TABLE.relative_to(ROOT)}): {len(handled)} types")
    for n in sorted(handled):
        print(f"    {n}")

    obj = build_fixture()
    if obj is None:
        return 1
    print(f"\nfixture: {obj} ({obj.stat().st_size} bytes)")

    txt = relocations(obj)
    if txt is None:
        return 1

    found = Counter(re.findall(r"R_X86_64_[A-Z0-9_]+", txt))
    # Drop GOT relocations from the census so they are reported by the
    # forbidden check below rather than silently inflating the count.
    got = [t for t in found if FORBIDDEN.search(t)]
    found = Counter({t: c for t, c in found.items() if not FORBIDDEN.search(t)})

    print("\nobserved relocations:")
    for t, c in sorted(found.items(), key=lambda kv: -kv[1]):
        mark = "handled" if t in handled else "NOT HANDLED"
        print(f"    {t:<24} {c:>4}   {mark}")

    ok = True

    if got:
        ok = False
        print(f"\nFAIL: GOT-family relocations present: {sorted(got)}")
        print("      §2.1 measured none. A GOT means a skipped relocation leaves a")
        print("      live pointer to garbage (§8.2), so this invalidates the")
        print("      whole no-GOT design and must be resolved before the relocator.")

    unhandled = sorted(set(found) - handled)
    if unhandled:
        ok = False
        print(f"\nFAIL: relocator does not handle: {unhandled}")
        print("      Either implement them or refuse modules that need them. Do not")
        print("      widen the table to make this pass: an entry in")
        print(f"      {TABLE.relative_to(ROOT)} is a claim the relocator makes.")

    unexercised = sorted(handled - set(found))
    if unexercised:
        ok = False
        print(f"\nFAIL: declared but not exercised: {unexercised}")
        print("      The fixture stopped provoking a type, so the census is not")
        print("      covering it. Most likely an optimisation deleted the construct")
        print("      (an unread static, a folded call). This is a coverage bug in")
        print("      the fixture, not a relocator bug -- but it makes the test lie.")

    missing_expected = sorted(EXPECTED - set(found))
    if missing_expected:
        ok = False
        print(f"\nFAIL: fixture failed to provoke: {missing_expected}")

    unused_table = sorted(handled - EXPECTED)
    if unused_table:
        ok = False
        print(f"\nFAIL: table lists types the fixture never provokes: {unused_table}")
        print("      Either the table over-claims coverage, or the fixture lost a")
        print("      case. Both make the census unreliable.")

    if ok:
        print(f"\nOK: {len(found)} types observed, all handled and all exercised")
        print("    §2.1's assumption holds for our toolchain. Milestone 1 satisfied.")
        return 0
    print("\nMilestone 1 NOT satisfied.")
    return 1


if __name__ == "__main__":
    sys.exit(main())
