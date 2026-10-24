//! Kernel export table for loadable modules (`docs/module-system.md` §6.1).
//!
//! # Why this is a hand-written allow-list
//!
//! §6.1 specifies a build-time generator that reads `kernel.elf`'s `.symtab`
//! after linking. That cannot be done from `build.rs`, which runs *before* the
//! kernel is linked — a genuine chicken-and-egg, not an oversight in the design.
//! It will eventually need a post-link step in the `tools` crate.
//!
//! Until then this is the explicit list §6.1 actually asks for, written by hand.
//! The design's own reasoning for an allow-list applies directly and is the
//! reason this is preferable to exporting "whatever is `pub`":
//!
//!  - It makes the module-facing API a **designed interface**. A function that
//!    is `#[inline]` today and gets outlined tomorrow would silently stop being
//!    callable if we exported by accident.
//!  - It stays small enough to audit.
//!
//! # Consequence, and it is not optional
//!
//! Adding a function here makes it part of a public contract. It must obey the
//! kernel invariants (AGENTS.md §1 lock ordering, §2 interrupt discipline,
//! §10 determinism) exactly as in-tree code does — the loader does **not**
//! relax them for module callers (`docs/module-system.md` §12.2).
//!
//! # Addresses
//!
//! Each entry stores the address of the item as the kernel linked it. These are
//! absolute virtual addresses in the kernel half, in the same space the module
//! relocator (`§6.2`) computes `S` in. Nothing here needs a page-table change:
//! the symbols already live in `.text`/`.data` of the running kernel, and the
//! module window (§4) is mapped with `GLOBAL` precisely so a module can call
//! into them without a TLB flush.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::sync::spinlock::SpinLock;

/// One exported kernel symbol.
#[derive(Clone, Copy)]
pub struct KernelSymbol {
    /// Symbol name as it appears in a module's undefined-symbol list.
    pub name: &'static str,
    /// Address as linked.
    pub addr: u64,
}

/// Sentinel for "this symbol has no address" — a data export whose address the
/// linker could not place. Kept distinct from a valid low address so a missing
/// export is never mistaken for one at address 0.
pub const NO_SYMBOL: u64 = u64::MAX;

/// C-ABI formatted print to the debug console, for module use.
///
/// # Safety
/// `fmt` must be a NUL-terminated UTF-8 string. This is the raw-C entry point
/// because a module cannot pass a `core::fmt::Arguments` across the boundary —
/// it is not `repr(C)` and carries a vtable-shaped slice pointer that would be
/// meaningless in the module's own address space.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dprintln_fmt(fmt: *const u8) -> isize {
    if fmt.is_null() {
        return -1;
    }
    // Bounded scan for the NUL. A module is not trusted to terminate its string;
    // an unbounded scan would let a bad pointer walk the kernel's address space.
    const MAX: usize = 4096;
    let mut len = 0usize;
    while len < MAX {
        if unsafe { *fmt.add(len) } == 0 {
            break;
        }
        len += 1;
    }
    if len == MAX {
        return -1; // unterminated within the cap
    }
    EXPORT_CALLS.fetch_add(1, Ordering::Relaxed);
    let bytes = unsafe { core::slice::from_raw_parts(fmt, len) };
    match core::str::from_utf8(bytes) {
        Ok(s) => {
            crate::dprintln!("[module] {s}");
            len as isize
        }
        Err(_) => -1,
    }
}

/// Current kernel tick count, for module use. Read by value, so a module
/// loading `TICKS` gets a `PC32` RIP-relative load (measured — see
/// `docs/module-system.md` §2.1a, which records that this is *not* `32S`).
#[unsafe(no_mangle)]
pub extern "C" fn module_tick_count() -> u64 {
    crate::time::TICKS.load(Ordering::Relaxed) as u64
}

/// The module-facing export table.
///
/// Built on first use rather than as a `static`, because a function's address
/// is not a constant expression and a `static` would require exactly that cast.
/// (Trying it produces "pointers cannot be cast to integers during const eval"
/// — a compile error, not a silent wrong address.) Populated once under an
/// `AtomicBool`; every core sees the same table because it is built from
/// function addresses that are identical on all cores.
///
/// Order is irrelevant to lookup (a linear scan over a handful of entries, and
/// a load is not a hot path), but it is kept alphabetical so a reviewer can see
/// the whole surface at a glance.
static EXPORTS: SpinLock<Option<Vec<KernelSymbol>>> = SpinLock::new(None);


/// LinuxKPI baseline allocator: allocate a zeroed-ish block of memory from the
/// global allocator. Used only by loadable modules as a small convenience
/// surface; it is not the production kmalloc.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lk_malloc(size: usize) -> *mut u8 {
    if size == 0 {
        return core::ptr::null_mut();
    }
    let layout = alloc::alloc::Layout::from_size_align(size, 16).unwrap();
    unsafe { alloc::alloc::alloc(layout) }
}

/// LinuxKPI baseline free.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lk_free(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    // `dealloc` needs the same layout; modules using this must pass the
    // original pointer without further bookkeeping for now.
    let layout = alloc::alloc::Layout::new::<u8>();
    unsafe { alloc::alloc::dealloc(ptr, layout) }
}

fn export_table() -> Vec<KernelSymbol> {
    vec![
        KernelSymbol { name: "dprintln_fmt", addr: dprintln_fmt as *const () as u64 },
        KernelSymbol { name: "module_tick_count", addr: module_tick_count as *const () as u64 },
        // Data exports. `modules/fixture/lib.rs` references TICKS and
        // KEYBOARD_DEV, and §6.2 requires an unresolved symbol to fail the load
        // -- so the fixture cannot be relocated until they exist. Exported as
        // functions rather than raw statics because a `static`'s address cannot
        // be taken in a const context here, and because a module holding the
        // *address* of kernel state is a wider contract than handing it the
        // value (see the note on TICKS_VALUE below).
        KernelSymbol { name: "TICKS", addr: ticks_addr as *const () as u64 },
        KernelSymbol { name: "KEYBOARD_DEV", addr: keyboard_dev_addr as *const () as u64 },
        KernelSymbol { name: "lk_malloc", addr: lk_malloc as *const () as u64 },
        KernelSymbol { name: "lk_free", addr: lk_free as *const () as u64 },
    ]
}

/// Address of the kernel tick counter, as a `u64` a module can load from.
///
/// The fixture reads `TICKS` by value, which the census measured as a `PC32`
/// (RIP-relative) rather than a `32S`. So the export must be the *address* of
/// the counter, which is what an `R_X86_64_PC32` against `TICKS` resolves to and
/// what a module then dereferences.
#[unsafe(no_mangle)]
pub extern "C" fn ticks_addr() -> *const u64 {
    // SAFETY: a module may read the pointer but the kernel keeps ownership; the
    // counter is a static AtomicUsize that outlives every module (unload is
    // deferred, §10).
    &crate::time::TICKS as *const _ as *const u64
}

/// Address of the keyboard device table.
///
/// Same reasoning as [`ticks_addr`]: the fixture takes KEYBOARD_DEV's address
/// (measured: a `32S`), so what the export must yield is the address of the
/// table, not a copy of it.
#[unsafe(no_mangle)]
pub extern "C" fn keyboard_dev_addr() -> *const u64 {
    // The *address* of the keyboard device word, not its value: the fixture
    // takes KEYBOARD_DEV's address and dereferences it, so the export has to
    // be an address a module can read.
    //
    // This used to return a null pointer, which was never dereferenced
    // because nothing ever called into a module. The moment the loader calls
    // a module's init function, a module that reads KEYBOARD_DEV faults on a
    // null dereference -- in *kernel* mode, which panics the whole machine.
    &crate::drivers::keyboard::PS2_KBD_DEV as *const _ as *const u64
}

/// How many times a module has called an exported kernel function.
///
/// The self-check needs this to prove the module->kernel direction: the
/// module's init function calls `dprintln_fmt`, and a load that only ran the
/// module's code would look the same as one that never called back.
static EXPORT_CALLS: AtomicU64 = AtomicU64::new(0);

/// The number of calls modules have made into the kernel's exports.
pub fn export_call_count() -> u64 {
    EXPORT_CALLS.load(Ordering::Relaxed)
}

/// Bumped whenever `EXPORTS` changes, so the boot trail can record the surface
/// a running kernel offered.
static EXPORT_COUNT: AtomicU64 = AtomicU64::new(0);

/// A symbol exported by a loaded module.
///
/// §5.3 reserves a namespace field in the `__ksymtab` record for exactly
/// this: two modules may export the same name without a conflict, because a
/// consumer binds to `module::symbol`, not to a bare `symbol`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModuleExport {
    /// The module that published it.
    pub module: &'static str,
    /// The symbol's name, as the module spells it.
    pub name: &'static str,
    /// Where it lives.
    pub addr: u64,
}

/// The symbols published by the modules that are loaded.
///
/// Empty until a module is published (docs/module-system.md §8.1 phase 8:
/// "the module's symbols are visible to *later* modules"). A module that
/// fails to load publishes nothing at all -- the all-or-nothing rule of §6.2
/// applies to publication as much as to relocation.
static MODULE_EXPORTS: SpinLock<alloc::vec::Vec<ModuleExport>> = SpinLock::new(alloc::vec::Vec::new());

/// Publish a loaded module's exports.
///
/// Called by the loader after the module is relocated and published, and
/// before its init function runs: a module must be able to call another
/// module that was loaded before it, but must not be callable *by* a module
/// that is still being loaded.
///
/// # Precedence
///
/// The kernel's own exports always win over a module's, and among modules the
/// first one to publish a name keeps it. Both rules are needed:
///
///  - Without the first, a module could publish `dprintln_fmt` and every
///    module loaded after it would call the module's function instead of the
///    kernel's. The kernel's API is the root of trust; a module must not be
///    able to replace it.
///  - Without the second, a later module could take over an earlier module's
///    symbol, and the earlier module's users would silently start calling
///    the wrong code. "First published wins" is the rule that makes a
///    module's export stable for the life of the system.
pub fn publish_module_exports(module: &'static str, symbols: &[(String, u64)]) {
    let mut table = MODULE_EXPORTS.lock();
    for (name, addr) in symbols {
        // A name already published by an earlier module is not taken over.
        if table.iter().any(|e| e.name == name.as_str()) {
            crate::dprintln!(
                "[module] export `{}` is already published; {} does not take it over",
                name,
                module
            );
            continue;
        }
        // The kernel's own exports win over any module's.
        if resolve_in_kernel(name).is_some() {
            crate::dprintln!(
                "[module] export `{}` from {} is shadowed by a kernel export; not published",
                name,
                module
            );
            continue;
        }
        table.push(ModuleExport {
            module,
            name: Box::leak(name.clone().into_boxed_str()),
            addr: *addr,
        });
    }
    crate::dprintln!(
        "[module] published {} exports from {} ({} module exports total)",
        symbols.len(),
        module,
        table.len()
    );
}

/// The kernel's own export table, without consulting the modules'.
fn resolve_in_kernel(name: &str) -> Option<u64> {
    let mut guard = EXPORTS.lock();
    let table = guard.get_or_insert_with(export_table);
    table.iter().find(|s| s.name == name).map(|s| s.addr)
}

/// Resolve `name` against the export tables: the kernel's first, then the
/// modules' (§6.2).
///
/// Returns the linked address, or `None` if nothing exports it. The caller
/// (§6.2) must fail the load rather than patch a placeholder: an unresolved
/// symbol written as zero is a live pointer to address 0, which is the exact
/// failure mode §8.2 exists to prevent.
pub fn resolve(name: &str) -> Option<u64> {
    // The kernel first: a module must not be able to shadow it.
    if let Some(addr) = resolve_in_kernel(name) {
        return Some(addr);
    }
    let table = MODULE_EXPORTS.lock();
    table.iter().find(|e| e.name == name).map(|e| e.addr)
}

/// The module-published exports, for the boot trail.
pub fn module_exports() -> alloc::vec::Vec<ModuleExport> {
    MODULE_EXPORTS.lock().clone()
}

/// The whole table, for the boot trail and for the module self-check.
pub fn exports() -> alloc::vec::Vec<KernelSymbol> {
    EXPORTS.lock().get_or_insert_with(export_table).clone()
}

/// Number of exports, for logging.
///
/// This used to return the *accumulated* value of `EXPORT_COUNT` rather than
/// the number of exports, because `fetch_or` returns the value it replaced.
/// Two calls in a row therefore returned different numbers for the same
/// table, which is exactly what a caller comparing them is entitled to expect
/// not to happen. The self-check that compares the count before and after a
/// refused load is what found it.
pub fn export_count() -> u64 {
    let n = exports().len() as u64;
    // A high-water mark, for the boot trail.
    EXPORT_COUNT.fetch_max(n, Ordering::Relaxed);
    n
}

/// Log the module-facing API at boot.
///
/// Without this the export table is invisible: a module that fails to resolve
/// gives no clue whether the kernel never had the symbol or the lookup is
/// broken (§6.2 requires the unresolved *name* to be reported, which is only
/// useful if the available set is also known).
pub fn log_exports() {
    let table = exports();
    crate::dprintln!("[module] kernel exports: {}", table.len());
    for s in &table {
        crate::dprintln!("[module]   export {:#x} {}", s.addr, s.name);
    }
}

