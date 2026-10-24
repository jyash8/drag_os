//! The module loader — `docs/module-system.md` §8.1.
//!
//! # The phases, and why they are in this order
//!
//! §8.1 lists nine phases. The order is the safety property:
//!
//!  1. **Fetch** — read the file through the normal VNode path, so a staged
//!     module gets the same ext4 validation and audit trail as any other file.
//!  2. **Validate** — magic, class, machine, `ET_REL`, section bounds,
//!     `vermagic`. Nothing is allocated yet, so a malformed module costs one
//!     parse.
//!  3. **Reserve** — take a slot from the module window (§4.1).
//!  4. **Allocate** — one page set per allocatable section.
//!  5. **Copy** — the section's bytes into its pages.
//!  6. **Relocate** — apply the relocations, resolving undefined symbols
//!     against the export table. All-or-nothing (§6.2).
//!  7. **Zero** — `.bss`, which has no file content.
//!  8. **Publish** — make text executable.
//!  9. **Init** — call the module's init function.
//!
//! Every phase logs one line, so the boot trail shows exactly which phase a
//! load reached and where it stopped (§8.3).
//!
//! # Text is not executable until phase 8
//!
//! The image is mapped `NO_EXECUTE` and only the text sections lose that flag
//! in phase 8, after every relocation has been applied. A module that is
//! halfway through relocation therefore cannot be executing, even by accident.
//!
//! # A failed load leaves nothing behind
//!
//! v1 has no unload (§10), so a load that fails after reserving a slot would
//! waste window space that never comes back. The loader therefore *releases*
//! the slot it reserved when a later phase fails. A release is only sound
//! because the slot was never published: nothing can be running from it, and
//! its pages are unmapped again before the slot goes back.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use goblin::elf::Elf;
use shared::memory::address::{PAGE_SIZE, PhysAddr, VirtAddr};
use shared::memory::allocator::FrameAllocator;
use shared::memory::flags::PageTableFlags;
use shared::memory::page::{PhysPage, VirtPage};

use crate::memory::FRAME_ALLOCATOR;

use super::elf::{self, MappedSection, ModuleError};
use super::reloc::{self, KernelExports, RelocError};
use super::slot;

/// The name of a module's init function.
///
/// Linux's convention, and the fixture's: a module that has no init function
/// is still a valid module, and loads with a zero return.
pub const INIT_SYMBOL: &str = "init_module";

/// Why a load failed.
#[derive(Debug, Clone)]
pub enum LoadError {
    /// The image did not validate (§8.1 phase 2).
    Invalid(ModuleError),
    /// The ELF could not be parsed a second time, for the relocations.
    NotElf,
    /// No slot could be reserved for the module (§8.1 phase 3).
    NoSlot,
    /// The image could not be placed at the slot's base.
    Layout(ModuleError),
    /// A frame could not be allocated for the image (§8.1 phase 4).
    OutOfMemory,
    /// The image could not be mapped into the kernel's address space.
    MapFailed,
    /// A relocation could not be applied (§8.1 phase 6).
    Relocation(RelocError),
    /// The module's init function returned, and the loader was told to treat
    /// that as a failure.
    InitFailed(isize),
}

impl core::fmt::Display for LoadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            LoadError::Invalid(e) => write!(f, "invalid module: {}", e.reason()),
            LoadError::NotElf => write!(f, "not an ELF file"),
            LoadError::NoSlot => write!(f, "no module slot available"),
            LoadError::Layout(e) => write!(f, "cannot be placed: {}", e.reason()),
            LoadError::OutOfMemory => write!(f, "out of memory"),
            LoadError::MapFailed => write!(f, "cannot be mapped into the kernel address space"),
            LoadError::Relocation(e) => write!(f, "relocation failed: {e}"),
            LoadError::InitFailed(r) => write!(f, "the module's init function returned {r}"),
        }
    }
}

/// A module that is loaded and published.
pub struct Loaded {
    /// Base address of the module in the window.
    pub base: u64,
    /// Bytes the module occupies.
    pub len: u64,
    /// What the module's init function returned. Zero when the module has no
    /// init function.
    pub init_result: isize,
    /// The module's exported symbols and where they live, so the kernel can
    /// call into the module (§5.3).
    pub exports: Vec<(String, u64)>,
}

impl Loaded {
    /// The address of one of the module's exported symbols.
    pub fn export(&self, name: &str) -> Option<u64> {
        self.exports.iter().find(|(n, _)| n == name).map(|(_, a)| *a)
    }
}

/// Load a module image: validate, place, map, relocate, publish, init.
///
/// # Safety
///
/// The module's code runs with the kernel's privileges once `init` is called.
/// The caller is responsible for the same reasons it is responsible for any
/// other code it runs: the module is ordinary input, and the loader validates
/// what it can (§8.1 phase 2) but cannot vouch for what the module *does*.
pub unsafe fn load(data: &[u8]) -> Result<Loaded, LoadError> {
    // ── Phase 2: validate ──────────────────────────────────────────────────
    //
    // Measured, not placed: the slot's size is only known after the module has
    // been measured, and the slot has to exist before the module can be put in
    // it.
    let mut image = elf::parse_module(data, super::KERNEL_VERMAGIC, 0)
        .map_err(LoadError::Invalid)?;
    let parsed = Elf::parse(data).map_err(|_| LoadError::NotElf)?;
    crate::dprintln!(
        "[module] load: validated {} sections, {} undefined, {} exports",
        image.sections.len(),
        image.undefined.len(),
        image.exports.len()
    );

    // ── Phase 3: reserve ───────────────────────────────────────────────────
    let total = image.total_map_len();
    let slot = slot::reserve(total).ok_or_else(|| {
        let free = slot::free_bytes();
        crate::dprintln!(
            "[module] load: no slot for {total:#x} bytes ({free:#x} free)"
        );
        LoadError::NoSlot
    })?;
    crate::dprintln!("[module] load: slot at {:#x} ({:#x} bytes)", slot.base, slot.len);

    // ── Phase 4: allocate and map ──────────────────────────────────────────
    //
    // The whole image is one contiguous range: the parser gives every section
    // an offset that is a multiple of the section alignment, so the sections
    // tile the range without gaps and the relocator's arithmetic on
    // `base + offset` is the same arithmetic the copy uses.
    let pages = (total / PAGE_SIZE) as usize;
    let phys = {
        let mut alloc = FRAME_ALLOCATOR.lock();
        alloc
            .alloc_pages(pages, PAGE_SIZE)
            .map_err(|_| LoadError::OutOfMemory)?
    };

    // Mapped writable and not executable: the copy and the relocation both
    // write, and text must not be executable until phase 8.
    let map_flags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::GLOBAL
        | PageTableFlags::NO_EXECUTE;
    {
        let mut mapper = crate::memory::space::mapper_for(kernel_cr3());
        let mut alloc = FRAME_ALLOCATOR.lock();
        if mapper
            .map_pages(
                &mut *alloc,
                VirtPage::containing(VirtAddr::new(slot.base)),
                PhysPage::containing(phys),
                pages,
                map_flags,
            )
            .is_err()
        {
            // The mapping failed, so the frames were never reachable; give
            // them back and the slot with them.
            let mut alloc = FRAME_ALLOCATOR.lock();
            alloc.dealloc_pages(phys, pages);
            slot::release(slot);
            return Err(LoadError::MapFailed);
        }
    }
    crate::dprintln!("[module] load: mapped {pages} pages at {:#x}", slot.base);

    // The image is now placed: re-base it onto the slot.
    if let Err(e) = elf::layout(&mut image, slot.base) {
        cleanup(slot, phys, pages);
        return Err(LoadError::Layout(e));
    }

    // ── Phases 5 to 9 ───────────────────────────────────────────────────────
    //
    // Everything from here on can fail, and a failure has to leave nothing
    // behind: the pages unmapped, the frames returned, the slot given back.
    // v1 has no unload, so a slot that leaked here would be gone for good.
    match place_and_publish(&mut image, &parsed, slot, phys, pages) {
        Ok((init_result, exports)) => Ok(Loaded {
            base: slot.base,
            len: slot.len,
            init_result,
            exports,
        }),
        Err(e) => {
            cleanup(slot, phys, pages);
            Err(e)
        }
    }
}

/// Unmap a failed module's pages, free its frames, and give the slot back.
///
/// The order matters: the pages go first, so nothing can reach the frames
/// after they are freed.
fn cleanup(slot: slot::Slot, phys: PhysAddr, pages: usize) {
    {
        let mut mapper = crate::memory::space::mapper_for(kernel_cr3());
        let _ = mapper.unmap_range(VirtAddr::new(slot.base), pages * PAGE_SIZE as usize);
    }
    {
        let mut alloc = FRAME_ALLOCATOR.lock();
        alloc.dealloc_pages(phys, pages);
    }
    if !slot::release(slot) {
        crate::dprintln!(
            "[module] load: slot at {:#x} was not the most recent one; it is leaked",
            slot.base
        );
    }
}

/// Copy, relocate, zero, publish and init: §8.1 phases 5 to 9.
///
/// Split out of [`load`] so that every failure in it runs the same cleanup.
fn place_and_publish(
    image: &mut elf::ModuleImage<'_>,
    parsed: &Elf<'_>,
    slot: slot::Slot,
    _phys: PhysAddr,
    _pages: usize,
) -> Result<(isize, Vec<(String, u64)>), LoadError> {
    // ── Phase 5: copy, and phase 7: zero ───────────────────────────────────
    //
    // The copy writes through the mapping, so the bytes the relocator patches
    // are the bytes that execute.
    let base_ptr = slot.base as *mut u8;
    for sec in &image.sections {
        let dst = unsafe { base_ptr.add(sec.offset as usize) };
        if sec.has_file_content() {
            let start = sec.file_off as usize;
            let end = start + sec.size as usize;
            if end > image_source_len(image) {
                // The parser checked this; re-checked here because this is the
                // code that would read out of bounds.
                crate::dprintln!("[module] load: section `{}` is out of bounds", sec.name);
                return Err(LoadError::Invalid(ModuleError::SectionOutOfBounds {
                    name: sec.name.clone(),
                }));
            }
            unsafe {
                core::ptr::copy_nonoverlapping(
                    image_source(image, start, sec.size as usize).as_ptr(),
                    dst,
                    sec.size as usize,
                );
            }
        } else if sec.needs_zeroing() {
            // NOBITS: no file content, so the pages must start out zero.
            unsafe {
                core::ptr::write_bytes(dst, 0, sec.size as usize);
            }
        }
    }
    crate::dprintln!("[module] load: copied {} sections", image.sections.len());

    // ── Phase 6: relocate ──────────────────────────────────────────────────
    //
    // All-or-nothing: the relocator resolves every symbol before it writes
    // anything, so a module missing one symbol is refused with nothing
    // patched (§6.2).
    let stats = unsafe {
        reloc::relocate(image, parsed, slot.base, base_ptr, &KernelExports)
    }
    .map_err(|e| {
        crate::dprintln!("[module] load: relocation failed: {e}");
        LoadError::Relocation(e)
    })?;
    crate::dprintln!(
        "[module] load: relocated {} relocations (pc32={} plt32={} abs64={} abs32s={})",
        stats.total, stats.pc32, stats.plt32, stats.abs64, stats.abs32s
    );

    // ── Phase 8: publish ───────────────────────────────────────────────────
    //
    // The permissions §4.2 states: text is present and global but not
    // writable, and executable; data is writable; rodata is neither writable
    // nor executable.
    for sec in &image.sections {
        publish_section(sec, slot.base)?;
    }
    crate::dprintln!("[module] load: published");

    // ── Phase 8b: publish the module's exports ─────────────────────────────
    //
    // §8.1 phase 8: "the module's symbols are visible to *later* modules".
    // This is the point at which they become visible: the module is fully
    // relocated and published, and nothing about it can still fail.
    let mut exports = Vec::new();
    for name in &image.exports {
        if let Some(addr) = reloc::defined_symbol_address(image, parsed, slot.base, name) {
            exports.push((name.clone(), addr));
        }
    }
    let module_name: &'static str = Box::leak(String::from(image.name()).into_boxed_str());
    super::exports::publish_module_exports(module_name, &exports);

    // ── Phase 9: init ──────────────────────────────────────────────────────
    let init_result = match reloc::defined_symbol_address(image, parsed, slot.base, INIT_SYMBOL) {
        Some(addr) => {
            let f: extern "C" fn() -> isize = unsafe { core::mem::transmute(addr) };
            let r = unsafe { f() };
            crate::dprintln!("[module] load: init returned {r}");
            r
        }
        None => {
            crate::dprintln!("[module] load: no {INIT_SYMBOL}, nothing to call");
            0
        }
    };

    Ok((init_result, exports))
}

/// The length of the module image the loader was given.
///
/// The copy needs the *file* to read the section bytes from, and the image
/// does not carry it: the loader is given the bytes and hands the parser a
/// view of them. This keeps the file's length next to the image, rather than
/// threading it through every function.
fn image_source_len(image: &elf::ModuleImage<'_>) -> usize {
    image.data.len()
}

/// A slice of the module file.
fn image_source<'a>(image: &'a elf::ModuleImage<'a>, start: usize, len: usize) -> &'a [u8] {
    &image.data[start..start + len]
}

/// Apply one section's final permissions, and invalidate its translations.
///
/// §4.2: text is `PRESENT | GLOBAL` and not writable; data is writable;
/// rodata is neither. The `NO_EXECUTE` flag is dropped only for text, and
/// only here -- phase 8, after every relocation has been applied.
fn publish_section(sec: &MappedSection, base: u64) -> Result<(), LoadError> {
    let mut flags = PageTableFlags::PRESENT | PageTableFlags::GLOBAL;
    if sec.is_writable() {
        flags |= PageTableFlags::WRITABLE;
    }
    if !sec.is_text() {
        flags |= PageTableFlags::NO_EXECUTE;
    }

    let start = base + sec.offset;
    let end = start + sec.map_len;
    let mut addr = start;
    while addr < end {
        let mut mapper = crate::memory::space::mapper_for(kernel_cr3());
        if mapper
            .update_prot_4k(VirtAddr::new(addr), flags)
            .is_err()
        {
            return Err(LoadError::MapFailed);
        }
        // The permission change has to be visible to the instruction stream:
        // the stale entry would still say "not executable" and the first call
        // into the module would fault.
        crate::memory::space::invlpg(addr);
        addr += PAGE_SIZE;
    }
    Ok(())
}

/// The kernel's own root table.
///
/// The module lives in the *kernel's* address space, not in whatever process
/// happens to be running: a module is called by the kernel, and the kernel's
/// code is mapped in the high half. `KERNEL_CR3` is a mutable static because
/// it is written once during boot; reading it is unsafe for that reason
/// alone.
fn kernel_cr3() -> PhysAddr {
    unsafe { crate::arch::x86_64::cpu::KERNEL_CR3 }
}

/// The address of the module window's base, for the boot trail.
pub fn window_base() -> u64 {
    elf::MODULE_WINDOW_BASE
}

/// The physical address a module's image was placed at, for the boot trail.
pub fn image_phys(base: u64) -> Option<PhysAddr> {
    crate::memory::space::frame_at(base).map(|f| PhysAddr(f << 12))
}
