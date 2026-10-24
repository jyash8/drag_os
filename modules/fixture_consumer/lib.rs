//! A module that binds to another module's export.
//!
//! §6.2 says symbol resolution looks in the kernel's export table and *then*
//! in the export tables of the modules that are already loaded. This fixture
//! is the test for the second half: it calls `dragos_fixture_probe`, which
//! the *good* fixture exports and the kernel does not, so the only way the
//! load can succeed is if the first module's exports were published and are
//! visible to a later module.
//!
//! It is loaded after the good fixture, so the ordering is the test.
//!
//! Built with the same flags as the other fixtures (docs/module-system.md
//! §5.1).

#![no_std]

// ── Undefined here, and not a kernel export ─────────────────────────────────
//
// It is defined by the *other* module, which is loaded first. If the loader
// only consulted the kernel's table, this would be unresolvable and the load
// would be refused.
unsafe extern "C" {
    /// Exported by the good fixture.
    pub fn dragos_fixture_probe(x: u64) -> u64;

    /// A kernel export, so this module also exercises the kernel path.
    pub fn dprintln_fmt(fmt: *const u8) -> isize;
}

/// The module's init entry, which the loader calls last (§8.1 phase 9).
///
/// It calls into the other module, so a successful load proves the binding
/// worked and not merely that the relocation was accepted. The value it
/// returns is the other module's, and the loader logs it, so the boot trail
/// shows a value that crossed the module boundary.
///
/// # Safety
/// Called by the loader with interrupts disabled. Touches only its own state
/// and the other module's exported function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn init_module() -> isize {
    let v = unsafe { dragos_fixture_probe(123) };
    // SAFETY: a NUL-terminated literal. The kernel's printer does not do
    // printf formatting, so this is a plain string.
    unsafe {
        dprintln_fmt(b"[module] consumer called the other module's probe\0".as_ptr() as *const u8);
    }
    v as isize
}
