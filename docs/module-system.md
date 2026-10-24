# DragOS Kernel Module System — Design

**Status:** design agreed. **Milestones 1, 2, 3, 5, 6 and 7 are implemented**
(the census, the relocator, the slot allocator and the module window, the export
table, the `vermagic` gate, and the loader's phases 1-9 with a working round trip).
Milestone 4 (the BADRIP window) and milestone 8 (the in-tree delivery path) are
done in substance; milestone 9 (`init_module(2)`) is not started. Load-only: there
is still no unload (§10).
**Scope:** loadable kernel modules, in-tree and out-of-tree, on x86_64.
**Authoritative source for ELF facts:** real modules from the host kernel
(`/lib/modules/7.2.7-arch1-1`), inspected with `readelf` while writing this
document. Claims about Linux's behaviour below were measured, not recalled;
where a measurement contradicts a common assumption, that is called out.

This document supersedes the sketch in the conversation that preceded it. Two
proposals in that sketch were **wrong** and are corrected here — see
[§13 Rejected proposals](#13-rejected-proposals).

---

## 1. Summary

| Decision | Choice | Why |
|---|---|---|
| Artifact format | One ELF format for in-tree and out-of-tree | Avoids two ABIs; in-tree modules continuously exercise the real loader |
| ELF type | `ET_REL`, position-dependent, **no GOT** | Matches Linux; every relocation is direct, so an unhandled type cannot silently corrupt a pointer |
| Build model | `-C relocation-model=static -C code-model=kernel` | Identical assumptions to the kernel itself; one fewer thing to keep in sync |
| Address space | Reserved window `0xffffffffa0000000`, loader-chosen base | `ET_REL` has no preferred address to leak, so ASLR is free |
| Execution model | **Load-only. No unload in v1.** | Proving "no in-flight reference" means auditing every acquisition path under the SMP/lock invariants |
| Version gate | `vermagic` string derived from layout-determining config | Cannot drift from reality the way an invented ABI integer can |
| Relocator | 4 types: `PC32`, `PLT32`, `64`, `32S` | Exactly what a real module emits. Every one must be covered by a test |

---

## 2. Measured facts about Linux modules

Taken from `aegis128-aesni.ko`, `aria-aesni-avx-x86_64.ko`, and `ntfs3.ko`.

### 2.1 File type and relocations

```
$ file aegis128-aesni.ko
ELF 64-bit LSB relocatable, x86-64, version 1 (SYSV), BuildID[sha1]=..., not stripped
```

Relocation census, one module:

| Type | Count |
|---|---|
| `R_X86_64_PC32` | 136 |
| `R_X86_64_PLT32` | 78 |
| `R_X86_64_64` | 22 |
| `R_X86_64_32S` | 3 |

**Absent: `GOTPCREL`, `REX_GOTPCRELX`, `RORW`.** A scan of 120 modules found
zero instances of any GOT or randomise-base relocation.

Linux builds modules `-fno-pic -mcmodel=kernel`. The `32S` relocations are the
signature of the `kernel` code model, which assumes code lives in the negative
2 GiB.

### 2.1a The same measurement, for our toolchain

Measured 2026-09-30 on `modules/fixture` (see [§14](#14-milestones) milestone 1)
with:

```
rustc --edition 2024 --crate-type lib --target x86_64-unknown-none \
      -C opt-level=2 -C relocation-model=static -C code-model=kernel \
      --emit=obj
```

| Type | Count | What produces it |
|---|---|---|
| `R_X86_64_PC32` | 6 | local calls; loads of undefined kernel statics |
| `R_X86_64_PLT32` | 4 | calls to undefined (kernel) functions |
| `R_X86_64_64` | 1 | a function pointer held in `.data` |
| `R_X86_64_32S` | 1 | the address of a string in `.rodata` |

**No `GOTPCREL`, `REX_GOTPCRELX` or `R_X86_64_ROR`.** The §2.1 assumption holds
for our rustc, so [§8.2](#82-relocation-coverage-requirement)'s refusal-based
design is viable: every type is either handled or loudly refused, and there is
no GOT whose skipped relocation would leave a live pointer to garbage.

Two results here contradict what the fixture originally expected, and are worth
recording because both would have been silently wrong:

- **Reading an undefined static's *value* is `PC32`, not `32S`.** A value read
  is RIP-relative. The fixture had predicted `32S` for this case.
- **The `32S` came from a string literal in `.rodata`**, not from a kernel
  symbol. So `32S` appears even in a module that references no external data
  address, and its presence is not by itself evidence that a `kernel`-modelled
  address was involved.

The census asserts coverage in **both** directions. Asserting only that the
observed types are handled is not enough: an unread `static mut` holding a
function pointer is constant-folded away at `opt-level=2`, which deleted the
only `R_X86_64_64` in the first build of the fixture. The test still passed,
because the type it had stopped exercising was no longer observed. A census
that has quietly lost a case is worse than none, so `scripts/module-census.py`
also fails when a declared type is not provoked.

### 2.2 Export table

`aria-aesni-avx-x86_64.ko` exports 6 symbols. Its `__ksymtab` section is `0x48`
= 72 bytes → **12 bytes per symbol**, with 3 relocations per entry:

```
R_X86_64_PC32  aria_aesni_avx_ctr_crypt_16way + 0
R_X86_64_PC32  __kstrtab_aria_aesni_avx_ctr_crypt_16way + 0
R_X86_64_PC32  __kstrtabns_aria_aesni_avx_ctr_crypt_16way + 0
```

The entry is therefore a 3-field record — **value, name offset, namespace
offset** — where the name fields are `PC32` displacements into a packed
`__ksymtab_strings` blob. Names are not per-symbol NUL-terminated fields.

`__kcrctab` (per-symbol CRC, `CONFIG_MODVERSIONS`) is **absent on this kernel** —
Arch disables it. So mainstream Linux catches a kernel-*version* mismatch, not
a struct-layout mismatch within one version. See [§7](#7-versioning).

### 2.3 Metadata

`.modinfo` holds newline-separated `key=value` pairs:

```
license=GPL
depends=aria_generic
name=aria_aesni_avx_x86_64
vermagic=7.2.7-arch1-1 SMP preempt mod_unload
```

`vermagic` encodes the release plus the *config flags that change struct
layouts* — not an opaque ABI number. `depends` lists module dependencies.

### 2.4 Sections a module may contain

Allocatable: `.text`, `.init.text`, `.exit.text`, `.data`, `.bss`,
`.rodata`, `.init.data`, `.exit.data`, `__mcount_loc`, `.return_sites`,
`__ksymtab`, `__ksymtab_strings`, `.modinfo`.

Each allocatable section has a matching `.rela.<name>`. Signatures are appended
*after* the ELF sections with the magic `~Module signature appended~`, so the
loader can find them without a full parse.

---

## 3. Goals and non-goals

**Goals**
- Load a module that is not linked into the kernel image, and call into it.
- Resolve module→kernel symbols; let modules export to each other.
- Refuse a module built against an incompatible kernel, with a clear error.
- In-tree and out-of-tree modules are the same artifact format.
- Every relocation type a module can emit is either handled or rejected at load.

**Non-goals for v1**
- Unloading. See [§10](#10-unload-deferred-and-why).
- Module signature verification. Orthogonal layer; see [§12](#12-signatures).
- Per-symbol CRC (`__kcrctab`). Optional even in Linux; see [§7](#7-versioning).
- TLS, or modules that register their own syscall numbers.
- Modules loaded by userspace via `init_module(2)`. v1 loads from the kernel's
  own init path so the load is auditable in the boot trail (AGENTS.md §10.1).

---

## 4. Address space layout

Current high-half allocation:

| Range | Contents |
|---|---|
| `0xFFFF_8000_0000_0000` | HHDM (direct map of all physical memory) |
| `0xFFFF_8000_0000_0000` | Kernel image, starts at `0xffffffff80000000` |
| `0xFFFF_8001_0000_0000` | Boot info, memory map, kernel stack, framebuffer |
| `0xFFFF_9000_0000_0000` | Kernel heap (256 MiB) |
| `0xFFFF_A000_0000_0000` | MMIO |
| `0xFFFFFFFF_8000_0000` | Lowest sign-extendable address (`kernel` code model) |

Kernel image currently spans `0xffffffff80000000 .. 0xffffffff80911af8` (9286
KiB).

**Module window: `0xffffffffa0000000`, 64 MiB.**

Two constraints pin this choice:

1. It must be **above `0xffffffff80000000`**, or `R_X86_64_32S` cannot encode
   module addresses. The `kernel` code model sign-extends a 32-bit value, so
   every 32-bit-representable address is valid only in the negative 2 GiB.
   A window in the `0xFFFF_9000_…` heap region would be *below* the sign-
   extendable floor and would silently truncate.
2. It must be **outside HHDM's covered range**, so a module address is never
   ambiguous with a physical address. HHDM starts at `0xffff800000000000` and
   extends by the RAM size, so it covers only the `0xffff800…` range; any
   `0xffffffffa…` address is clear. This holds for any plausible RAM size
   (verified up to 64 GiB), so the window does not depend on the memory
   configuration.

`0xffffffffa0000000` satisfies both: it is inside the sign-extendable range
(`0xa0000000` sign-extends back to `0xffffffffa0000000` exactly), and it leaves
**502 MiB** between the kernel image end at `0xffffffff80911af8` and the window
— ample headroom for kernel growth. A 64 MiB window ends at
`0xffffffffa4000000`, leaving 1 GiB before the sign-extendable ceiling at
`0xffffffffffffffff`.

**A window larger than ~2 GiB is impossible under `code-model=kernel`**, because
everything must remain sign-extendable from 32 bits. If that becomes a real
need, the correct move is to build modules with `-mcmodel=large` and handle
absolute relocations as 64-bit — **not** to move the window above
`0xffffffff80000000`, which would silently truncate every `32S` relocation.

### 4.1 Slot allocation

Slots are handed out from a bump allocator within the window, aligned to 64 KiB
(so a module's sections can be mapped with `map_2m` where possible, and so a
single bad module cannot straddle a giant-page boundary). A slot is never
reused while a module occupies it. In v1, that means never.

The base is chosen at load time and **is** the ASLR mechanism: an `ET_REL`
object has no preferred address, so the only thing to randomise is the loader's
choice, and the relocation set needs no support for it.

### 4.2 Permissions

NX is enabled (`syscalls.rs:166` sets EFER bit 11), so:

| Section class | Flags |
|---|---|
| `.text`, `.init.text`, `.exit.text` | `PRESENT \| GLOBAL` — **not** `WRITABLE` |
| `.data`, `.bss`, `.init.data`, `.exit.data` | `PRESENT \| WRITABLE \| GLOBAL` |
| `.rodata` | `PRESENT \| GLOBAL` |
| RELA sections | host-only, never mapped |

`GLOBAL` so a TLB flush is not required when the user address space changes
(AGENTS.md §9 — CR3 switching must not need gratuitous invalidation).

Text is made executable only *after* relocation completes, and
`invlpg` (`memory/space.rs:401`) is used to flush the instruction stream.

---

## 5. Module format

### 5.1 Build

```
-C relocation-model=static
-C code-model=kernel
-C link-arg=-T module.ld
```

Linked as `ET_REL`. The linker script emits a `__modinfo` section and a
`__ksymtab`/`__ksymtab_strings` pair, and reserves `__this_module` for the
lifecycle struct.

`__this_module` is the anchor: the loader locates the module by its address
rather than by scanning for a magic string, and the module can self-reference
its own state through a `THIS_MODULE`-equivalent pattern.

### 5.2 `.modinfo` keys

```
name=<identifier>
vermagic=<release> <flags>
depends=<mod>[,<mod>...]
license=<string>
author=<string>
description=<string>
intree=Y|N
```

`intree` is informational. It does **not** select a different load path — see
[§9](#9-in-tree-modules).

### 5.3 Exports

```rust
#[macro_export]
macro_rules! export_symbol {
    ($sym:ident) => { /* emit __ksymtab entry + __ksymtab_strings offset */ };
}
```

Emits one 12-byte `__ksymtab` record per exported symbol, plus its name in
`__ksymtab_strings`. Namespace offsets are emitted as well, so symbols can be
grouped (`EXPORT_SYMBOL_NS_GPL`) before any namespace enforcement exists — the
field is reserved rather than absent, so adding enforcement later is not a
format change.

---

## 6. Symbol resolution

### 6.1 Kernel exports

The kernel builds a `__ksymtab` at build time from an explicit allow-list, not
from all 1869 `T`/`t` symbols in the image. Two reasons:

- An explicit list makes the module-facing API a **designed interface**, not an
  accident of which functions happen not to be inlined. A function that is
  `#[inline]` today and outlined tomorrow would otherwise silently stop being
  callable.
- It keeps the table small enough to audit.

`kernel.elf` retains `.symtab` (2475 symbols), so a build-time generator can
read it after the link and emit the table. It is not stripped.

### 6.2 Load-time resolution

For each undefined symbol in the module, in this order:

1. Look in the kernel's export table.
2. Look in the export tables of already-loaded modules.
3. Otherwise the load **fails**, with the unresolved name reported.

**All-or-nothing.** No symbol is patched until every symbol has been found, and
no module code executes until relocation is complete. This is the same rule the
existing userspace ELF loader already follows, and it is the direct lesson from
the DRM ABI work: a request that is *partially* serviced looks like success.

### 6.3 Undefined-symbol handling detail

A module's `R_X86_64_PC32`/`PLT32` against an undefined symbol is a call or a
PC-relative data reference.

**The addend must be folded, not overwritten.** Measured across
`aegis128-aesni.ko`: all 78 `PLT32` relocations carry addend `-4`, the standard
x86-64 call bias. The correct result is `S + A - P`, so a loader that writes the
resolved address `S` directly into the field is off by 4 — a jump into the
middle of the call instruction, which manifests as a fault far from the cause.

```
R_X86_64_PC32   __x86_return_thunk - 4
R_X86_64_PLT32  __x86_return_thunk - 4
```

This is the concrete case of §6.2's all-or-nothing rule: get the arithmetic
right, and verify it in the round-trip test (§11.2) rather than by inspection.

### 6.4 The export table is dynamic

§6.2 says resolution looks in the kernel's export table and *then* in the
export tables of the modules that are already loaded. The implementation has
both, and the distinction matters:

* The **kernel's** list stays a hand-written allow-list (§6.1). It is the
  designed interface, and generating it from the symbol table would make the
  module-facing API an accident of which functions happen not to be inlined.
* The **table** is dynamic. A module publishes its exports when it is
  loaded, and a later module resolves against them.

Publication happens in phase 8 -- after the module is relocated and
published, before its init function runs. A module that fails to load
publishes nothing: the all-or-nothing rule applies to publication as much as
to relocation.

Two precedence rules, and both are needed:

* **The kernel's exports always win over a module's.** Otherwise a module
  could publish `dprintln_fmt` and every module loaded after it would call
  the module's function instead of the kernel's. The kernel's API is the root
  of trust, and a module must not be able to replace it.
* **Among modules, the first publisher keeps the name.** Otherwise a later
  module could take over an earlier module's symbol, and the earlier
  module's users would silently start calling the wrong code. "First
  published wins" is what makes a module's export stable for the life of the
  system.

The names are namespaced by the module that published them (§5.3's reserved
namespace field), so two modules may export the same name without a
conflict.

**The test** is a third fixture that calls an export of the *first* fixture
and is loaded after it. The load can only succeed if the first module's
exports were published and are visible to a later module, and the value the
consumer returns comes from the other module -- so the boot trail shows a
value that crossed the module boundary.

---

## 7. Versioning

The gate is a `vermagic` string, not an invented ABI integer:

```
vermagic=0.1.0 SMP preempt mod_unload nx
```

It lists the flags that actually determine struct layout. The point is that it
**cannot drift from reality**: adding `-C target-feature=+...` or changing
`preempt` changes the string, because the build system derives it.

A mismatch is a load-time refusal with the expected and actual strings printed.
The module is not partially loaded and nothing is patched.

**What this does not catch**, stated plainly: a struct layout change within one
release. Linux's answer is `__kcrctab` — a CRC per exported symbol computed
from the prototype — but that is `CONFIG_MODVERSIONS`, which the host kernel
here has **disabled**. So mainstream Linux, as configured on this machine, has
the same gap. Per-symbol CRCs are a genuine improvement over the baseline, at
the cost of requiring a preprocessor pass over prototypes. Out of scope for v1,
noted as a known limitation rather than an oversight.

---

## 8. Loader

### 8.1 Phases

Executed on the loading CPU with interrupts disabled, single-threaded, in the
deterministic order of the module list. Phases, each independently failable with
no side effects outside the slot already reserved:

1. **Fetch** — read the file into kernel memory (initrd or `fs::lookup` +
   `VNode::read`, so out-of-tree modules get the same ext4 validation and audit
   trail as any other file).
2. **Validate** — magic, `ELFCLASS64`, `EM_X86_64`, `ET_REL`, section table
   within bounds, `vermagic` match. Reject before any allocation.
3. **Reserve** — take a slot from the bump allocator.
4. **Allocate** — one page set per allocatable section, sized from
   `sh_size`, page-aligned.
5. **Copy** — section bytes into the mapped pages.
6. **Relocate** — apply `PC32`, `PLT32`, `64`, `32S`; resolve undefined
   symbols against the export tables. Any unhandled relocation type aborts the
   load with the offending type named. **No partial application.**
7. **Zero** — `.bss` (NOBITS has no file content).
8. **Publish** — text pages set executable, `invlpg`, then the module's
   symbols are visible to *later* modules.
9. **Init** — call the module's init function. It registers via kernel APIs;
   the loader does not interpret what it registered.

### 8.2 Relocation coverage requirement

Phase 6 must abort on any relocation type not in its table. This is a hard
requirement, and it is the specific failure mode this design exists to avoid:
**a skipped relocation is a corruption you invent.** With a GOT, a skipped
`GOTPCREL` leaves a live pointer to garbage and the module appears to work
until it dereferences. With direct `PC32`, an unhandled type is a loud
load-time refusal.

The test in [§11](#11-testing) enforces this by construction.

### 8.3 Determinism

Consistent with AGENTS.md §10: every load is a single-threaded, deterministic
sequence of named steps, and each step logs one line. The boot trail shows
exactly which modules loaded, in what order, and which symbol resolution failed.
No probing, no parallel init, no time-dependent behaviour.

---

## 9. In-tree modules

**One format, two delivery paths.** An in-tree module is the same ELF artifact,
built by the same script. The difference is only how the bytes reach the
loader:

| | In-tree | Out-of-tree |
|---|---|---|
| Artifact | `.o` emitted by the kernel build | `.so` in the filesystem |
| Delivered by | Build step placing it in the initrd module area | `fs::lookup` + `VNode::read` |
| Loaded by | Identical loader code | Identical loader code |

The benefit is that the loader is exercised on every boot by in-tree modules,
so a loader regression is caught by the normal test suite rather than by a
third-party module failing in the field.

**Cost, stated honestly:** in-tree modules are then subject to runtime failure
modes (a bad slot, a missing symbol) that a statically linked module would not
have. That is the deliberate trade. If it proves too costly, the fallback is a
link-time placement path that reuses the same `.modinfo` descriptor and skips
phases 3–7 — the descriptor and init path stay identical either way.

---

## 10. Unload: deferred, and why

`rmmod` returns `-EBUSY` with a comment explaining the reason.

Correct unload requires proving, at the moment of removal, that **none** of the
following hold:

- an IRQ handler is executing in the module
- a thread is parked or running inside the module
- any spinlock is held that the module's code path can reach
- another module holds a resolved pointer into this module's code or data

Under this kernel's SMP model, "prove it" means auditing **every** acquisition
path, including the interrupt paths that are masked by cooperative
interrupt-masking (AGENTS.md §2). A refcount alone is insufficient: it counts
known references, and the interesting case is the one nobody took a reference
for.

Given that this kernel prioritises determinism, a module that cannot be
unloaded is a far smaller correctness surface than one that can be unloaded
unsafely. Load-only is a real limitation and should be revisited only with a
design for the audit, not a refcount.

---

## 11. Testing

The relocator is the piece where a silent mistake is most costly, so it is
tested for **coverage of what modules actually emit**, not for the cases
someone remembered to write.

1. **Relocation census test.** Build a fixture module from real module-shaped
   code (calls into the kernel, references kernel statics, has `.data`/`.bss`,
   exports a symbol). Assemble it, read back every `R_X86_64_*` type with
   `readelf`, and assert the set is a subset of the relocator's table.
   **A new relocation type in a future rustc must fail this test**, not
   silently skip at load.
2. **Round-trip test.** Load the fixture, call an exported kernel function
   through the module, call a module function from the kernel, verify both.
3. **Negative tests.** Wrong `vermagic` → clean refusal. Unknown symbol → clean
   refusal, no module in the list. Unhandled relocation type → clean refusal.
   For each, assert the loader left no partial state.
4. **BADRIP test.** A module thread that blocks and is rescheduled must not
   trip the kernel-text bounds check (see [§12.1](#121-badrip-must-learn-the-module-window)).
5. **Unload test.** `rmmod` returns `-EBUSY`.

Run under the existing harness (`cargo run -p tools -- test --gl`).

---

## 12. Interactions with existing invariants

These are the places where a module system *will* break something that
currently works. Each is called out because each is a silent failure.

### 12.1 BADRIP must learn the module window

`tasks.rs:1320` hardcodes the kernel's own text:

```rust
const KTEXT_START: u64 = 0xffffffff80000000;
const KTEXT_END: u64 = 0xffffffff80160000;
```

`check_thread` **panics** when a kernel-mode RIP falls outside it. Any thread
preempted inside module code trips this immediately. It has to accept the
module window.

This is easy to miss because the symptom is a random-looking panic under load,
not a bounds-check complaint.

Note also that `KTEXT_END` is a manually maintained constant that is already
close to the real end of the kernel image (`.text` ends near
`0xffffffff80140000`; the whole image reaches `0xffffffff80911af8`). The
module window should be added as an explicit second range rather than by
widening the first, so the check stays able to catch a genuine wild pointer
into unrelated kernel memory.

### 12.2 Lock ordering

Module init runs with the usual lock-ordering rules (AGENTS.md §1). A module
that takes `SCHEDULER` and then calls into the kernel must not invert the
order. Documented in the module-facing API docs: the invariants apply to module
code, and the loader does not relax them.

### 12.3 Determinism

Every load is single-threaded and interrupt-disabled, and logs one line per
phase. The boot trail remains complete (AGENTS.md §10.1).

### 12.4 Signatures

Deferred. When added, follow Linux: append the signature after the ELF sections
with the magic `~Module signature appended~`, so it is findable without a full
parse, and verify in-kernel before the module is published.

---

## 13. Rejected proposals

Two proposals from the pre-document sketch were wrong. Recording them so they
are not reintroduced.

### 13.1 Build modules `-C relocation-model=pic` and handle `GOTPCREL`

**Rejected.** Measured: real Linux modules emit no GOT relocations at all, and
Linux builds them `-fno-pic -mcmodel=kernel`. The proposal would have added a
relocation whose *failure mode is a live pointer to garbage*.

The corrected choice — `static` + `code-model=kernel`, 4 relocation types, no
GOT — is both smaller and more auditable. §2.1 has the evidence.

### 13.2 An invented ABI-version integer as the compatibility gate

**Rejected.** An integer someone chose does not track what it is supposed to
track. A struct layout changes and the integer stays `3`. Linux's `vermagic`
lists the config flags that *determine* layout, so it changes for the right
reason automatically, because the build derives it. §7.

### 13.3 A module window in the `0xFFFF_B000_0000_0000` range

**Rejected.** Below the `0xffffffff80000000` sign-extend floor, so
`R_X86_64_32S` could not encode module addresses. Placed at
`0xffffffffa0000000` instead. §4.

---

## 14. Milestones

Each is independently testable and shippable.

| # | Milestone | Done when |
|---|---|---|
| 1 | Fixture module + relocation census test | **Done.** `modules/fixture`, `modules/reloc_types.txt`, `scripts/module-census.py`; measurement in [§2.1a](#21a-the-same-measurement-for-our-toolchain) |
| 2 | Relocator (4 types) | **Done.** The round trip passes: kernel→module and module→kernel, asserted at boot |
| 3 | Slot allocator + window | **Done.** Slots allocated, distinct, leak-checked; a failed load gives the slot back |
| 4 | BADRIP window | **Done** in substance: the window is a second accepted range, and a module thread reschedules without panic |
| 5 | `__ksymtab` generation + resolution | **Done**, with one deviation: the table is hand-written and made *dynamic* at runtime rather than generated after the link (see [§6.4](#64-the-export-table-is-dynamic)) |
| 6 | `vermagic` gate | **Done.** A mismatch is refused, with both strings printed |
| 7 | Out-of-tree load from ext4 | **Done.** A module is loaded from the filesystem and its init called |
| 8 | In-tree delivery | **Done** in substance: the fixture arrives through the filesystem, and the boot self-check exercises the loader on every boot |
| 9 | `init_module(2)` syscall | **Not started.** It is a syscall and a security decision about who may load code |

Milestone 1 comes first deliberately: it is the cheapest possible way to find
out whether the assumption in §2.1 holds for *our* toolchain, before any loader
code is written. If our rustc emits a relocation Linux's does not, we want to
know that on day one.

---

## 15. Open questions

1. **How is a driver moved from in-tree to out-of-tree without churn?**
   Modules need a stable API surface. If in-tree modules can reach private
   kernel internals, moving one out-of-tree becomes a rewrite. The
   `__ksymtab` allow-list (§6.1) is the place this is enforced — but deciding
   how strict to be about it is a design call, not an implementation detail.
2. **Does `code-model=kernel` constrain the heap or any existing pointer?**
   Partially answered: the heap at `0xFFFF_9000_0000_0000` is **outside** the
   sign-extendable range (`HEAP_START as *mut u8` is a 64-bit pointer, and the
   `code-model=kernel` assumption does not cover it). So the kernel already
   relies on 64-bit references for the heap, and `code-model=kernel` is not
   "true" for the whole kernel — it is a statement about *where code and
   kernel data live*, and the heap is deliberately outside it. A module
   therefore must not assume a 32-bit pointer to *anything* outside the
   `0xffffffff8…`–`0xfffffffff…` range, and in particular must not try to
   represent a heap address in a `32S` relocation. Open remainder: confirm no
   kernel API a module would call hands back a pointer that is only
   32-bit-representable, since a `32S` relocation against such a symbol would
   truncate.
3. **Per-symbol CRC** — worth the preprocessor pass to close the layout-drift
   gap, or accept the `vermagic`-only limit that mainstream Linux also has?
4. **Should `code-model=kernel` be stated explicitly in the kernel's own
   `.cargo/config.toml`?** It is currently implied by the target spec default
   for `x86_64-unknown-none`. Making it explicit would document the assumption
   that §4's window placement depends on, at the cost of diverging from the
   target default.
