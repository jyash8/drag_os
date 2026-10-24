//! Module slot allocation — `docs/module-system.md` §4.1.
//!
//! # Why a slot allocator at all
//!
//! An `ET_REL` object has no preferred load address, so the loader chooses
//! one. Every module needs a *different* choice, or two modules would occupy
//! the same addresses and the second would overwrite the first's code while it
//! was running.
//!
//! # The rules §4.1 states
//!
//!  - Slots come from a bump allocator inside the 64 MiB module window
//!    (`elf::MODULE_WINDOW_BASE`).
//!  - A slot is 64 KiB aligned, so a module's sections can be mapped with
//!    `map_2m` where possible, and so a single bad module cannot straddle a
//!    giant-page boundary.
//!  - A slot is never reused while a module occupies it. In v1 that means
//!    never: there is no unload (§10).
//!  - The base is chosen at load time, and *is* the ASLR mechanism. There is
//!    nothing else to randomise, because the object carries no address.
//!
//! # Why a refusal is not a clamp
//!
//! A reservation that would cross the window's end is **refused**. Clamping it
//! would put part of a module outside the window, where `R_X86_64_32S` cannot
//! reach it (§4) -- a silent truncation that only shows up as a wrong
//! relocation, much later.
//!
//! A refused reservation does not consume the cursor either. A refusal that
//! advanced it would waste the remainder of the window on a module that never
//! loads.

use crate::arch::sync::spinlock::SpinLock;

use super::elf::{MODULE_SECTION_ALIGN, MODULE_WINDOW_BASE, MODULE_WINDOW_END};

/// A reserved region of the module window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot {
    /// First address of the slot. 64 KiB aligned.
    pub base: u64,
    /// Length of the reservation in bytes.
    pub len: u64,
}

impl Slot {
    /// One past the last byte of the slot.
    pub fn end(&self) -> u64 {
        self.base + self.len
    }
}

/// The bump cursor: the next address a slot may start at.
///
/// Never rewound. A slot is never reused (§4.1), and v1 has no unload to
/// release one (§10), so there is nothing to rewind it for.
static NEXT: SpinLock<u64> = SpinLock::new(MODULE_WINDOW_BASE);

/// Reserve `len` bytes from the module window.
///
/// Returns `None` when the reservation cannot be satisfied: a zero length, a
/// length that does not fit in what is left of the window, or an address
/// computation that would overflow. In every case the cursor is left alone, so
/// the allocator stays usable.
pub fn reserve(len: u64) -> Option<Slot> {
    if len == 0 {
        // A module with no allocatable sections needs no slot. Returning one
        // would hand the loader a zero-length region whose "base" is the
        // window base, and two such modules would share it.
        return None;
    }
    let mut cursor = NEXT.lock();
    let base = align_up(*cursor, MODULE_SECTION_ALIGN)?;
    let end = base.checked_add(len)?;
    if end > MODULE_WINDOW_END {
        return None;
    }
    *cursor = end;
    Some(Slot { base, len })
}

/// Round `v` up to a multiple of `align`.
///
/// `align` is a power of two, so this is a mask, not a division. Returns
/// `None` on overflow, which for a base near the top of the address space is
/// the difference between "refused" and "a slot that wraps".
fn align_up(v: u64, align: u64) -> Option<u64> {
    debug_assert!(align.is_power_of_two());
    v.checked_add(align - 1).map(|x| x & !(align - 1))
}

/// Give a slot back.
///
/// Only the *most recent* slot can be released: the allocator is a bump
/// allocator, so rewinding it past a slot that is still in use would hand the
/// same addresses to two modules. The loader is single-threaded and
/// interrupt-disabled (§8.1), so the slot it is failing on is always the most
/// recent one.
///
/// A slot that is not the most recent is **not** released. That is a leak, and
/// it is the safe direction: two modules sharing an address range is not.
pub fn release(slot: Slot) -> bool {
    let mut cursor = NEXT.lock();
    if *cursor == slot.end() {
        *cursor = slot.base;
        true
    } else {
        false
    }
}

/// Bytes still available in the window.
///
/// For the boot trail: a load that fails because the window is full is a
/// different failure from one that fails because the module is malformed, and
/// the trail should say which.
pub fn free_bytes() -> u64 {
    let cursor = *NEXT.lock();
    MODULE_WINDOW_END.saturating_sub(cursor)
}

/// The address the next slot would start at, without reserving it.
pub fn next_base() -> u64 {
    *NEXT.lock()
}
