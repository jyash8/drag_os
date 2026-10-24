//! Fixture module for the relocation census (docs/module-system.md §11.1).
//!
//! This is not a working driver. It exists so the census test can read back
//! every relocation our toolchain emits for module-shaped code and assert the
//! set is one the relocator handles.
//!
//! Milestone 1 is deliberately this cheap. The design in §2.1 measured Linux
//! modules and found exactly `PC32`, `PLT32`, `64` and `32S`, with no GOT
//! relocations at all. That is an assumption about *our* rustc too, and if it
//! is wrong we want to know before writing a relocator rather than after.
//!
//! The four shapes below are chosen to provoke one of each expected type.
//! **The mapping in this table is measured, not predicted** — the first version
//! of this file guessed that reading an undefined static's value would give
//! `32S`, and the census showed otherwise (see the notes on each item and
//! `scripts/module-census.sh`, which prints the per-type target breakdown):
//!
//! | relocation        | count | what actually produces it here            |
//! |-------------------|-------|-------------------------------------------|
//! | `R_X86_64_PC32`   |   6   | local calls; loads of undefined statics    |
//! | `R_X86_64_PLT32`  |   4   | calls to undefined (kernel) functions       |
//! | `R_X86_64_64`     |   1   | the function pointer in `.data`            |
//! | `R_X86_64_32S`    |   1   | the address of a string in `.rodata`       |
//!
//! Note there are **no** `GOTPCREL` / `REX_GOTPCRELX` / `R_X86_64_ROR`
//! relocations, which is the load-bearing half of the §2.1 assumption: with a
//! GOT, a relocation the relocator skipped would leave a live pointer to
//! garbage (§8.2). With direct `PC32`, an unhandled type is a loud refusal.
//!
//! Plus `.data`/`.bss` and exported symbols, so the census covers the section
//! and export shapes the loader will also have to handle (§2.4, §5.3).

#![no_std]

// ── Undefined: what the loader must resolve against the kernel ──────────────
//
// These names stand in for real kernel exports. The census only cares that they
// are *undefined* in the module, so the loader has to resolve them; which kernel
// symbol they eventually name is decided when the `__ksymtab` allow-list exists
// (§6.1), not here.

unsafe extern "C" {
    /// An exported kernel function. Calling it must be a direct `call rel32`
    /// with no PLT and no GOT.
    pub fn dprintln_fmt(fmt: *const u8, ...) -> isize;

    /// An exported kernel static, read by value. Measured: this is a `PC32`,
    /// not a `32S` — reading a static's *value* is a RIP-relative load, so the
    /// displacement is relative to the instruction, not an absolute address.
    pub static TICKS: u64;

    /// An exported kernel table. Also a `PC32` here, because the fixture
    /// dereferences it; only a *taken address* held in a 64-bit slot needs a
    /// `64`, and that case is covered by `HOOK` below.
    pub static KEYBOARD_DEV: u64;
}

// ── Defined here: PC32, plus the data a real module would carry ────────────

/// Mutable state with a **non-zero** initialiser, so the module has a real
/// `.data` section. `= 0` would place this in `.bss` instead — a zero
/// initialiser is, by definition, zero bits — and the `.data` case would go
/// untested.
static mut COUNTER: u64 = 0x1234_5678_9abc_def0;

/// Zero-initialised state, so the module has a `.bss` section. The loader must
/// zero this itself: it is NOBITS and has no file content (§8.1 phase 7).
static mut SCRATCH: [u8; 64] = [0; 64];

/// A function pointer in `.data` with a relocatable initialiser.
///
/// This is what provokes `R_X86_64_64`: a 64-bit slot holding an address
/// cannot be `32S`.
///
/// It needs two defences, both learned the hard way here. `#[used]` stops the
/// section being dropped as unreferenced, and `init_module` reads it through
/// `read_volatile` -- an ordinary load of a never-written `static mut` is
/// constant-folded and the call devirtualised, which deletes the relocation
/// along with the code. A census that quietly loses a case is worse than no
/// census, because it still passes; that is why the census asserts the expected
/// types are *present* and not merely that the observed ones are handled.
#[used]
static mut HOOK: extern "C" fn(u64) -> u64 = tally;

/// A local function. Calling it is a direct relative call, and taking its
/// address for `HOOK` is a `64`-bit absolute relocation.
#[inline(never)]
fn bump(by: u64) -> u64 {
    // SAFETY: single-threaded fixture; nothing else touches these.
    unsafe {
        COUNTER = COUNTER.wrapping_add(by);
        SCRATCH[0] = SCRATCH[0].wrapping_add(1);
        COUNTER
    }
}

#[inline(never)]
extern "C" fn tally(x: u64) -> u64 {
    x.wrapping_mul(3)
}

/// The module's init entry, which the loader calls last (§8.1 phase 9).
///
/// # Safety
/// Called by the loader with interrupts disabled. Touches only its own state.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn init_module() -> isize {
    let msg = b"fixture: init\0";
    // Undefined call -> PLT32.
    unsafe { dprintln_fmt(msg.as_ptr() as *const u8) };
    // Undefined static, by value -> 32S under the kernel code model.
    let t = unsafe { TICKS };
    // Undefined static, by address -> 64.
    let kd = core::ptr::addr_of!(KEYBOARD_DEV);
    // Read the function pointer out of `.data` and call through it, which is
    // what keeps the `R_X86_64_64` initialiser alive.
    let hook = unsafe { core::ptr::read_volatile(&raw const HOOK) };
    // Local call -> PC32.
    bump(unsafe { hook(t) ^ t ^ *kd }) as isize
}

/// An exported symbol, so the census covers the `__ksymtab` shape.
#[unsafe(no_mangle)]
pub extern "C" fn dragos_fixture_probe(x: u64) -> u64 {
    bump(x) ^ tally(x)
}
