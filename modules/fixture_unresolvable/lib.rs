//! A module that asks for a symbol the kernel does not export.
//!
//! The point is §6.2: a module whose undefined symbol cannot be resolved must
//! be **refused**, with the name reported, and with nothing left behind. This
//! fixture is the input for that test: it names a symbol no kernel exports,
//! so every load of it must fail.
//!
//! It is deliberately as small as possible. The refusal happens in the
//! relocation pass, before any of the module's code could run, so the module
//! needs no code beyond the call that forces the symbol to be undefined.
//!
//! Built with the same flags as the good fixture (docs/module-system.md §5.1),
//! because a refusal that only works for one shape of module would not be a
//! refusal at all.

#![no_std]

// ── Undefined, and not exported by the kernel ────────────────────────────────
//
// The name is deliberately unlike anything the kernel exports, so the test
// cannot pass by accident because the symbol happened to exist.
unsafe extern "C" {
    /// A kernel function that does not exist.
    pub fn dragos_this_symbol_is_not_exported(x: u64) -> u64;
}

/// The module's init entry, which the loader would call last (§8.1 phase 9).
///
/// The load must be refused long before this runs.
///
/// # Safety
/// Never called: the load is refused during relocation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn init_module() -> isize {
    unsafe { dragos_this_symbol_is_not_exported(1) as isize }
}
