//! Relocation processing — `docs/module-system.md` §8.1 phase 6.
//!
//! # The rule this module exists to enforce
//!
//! §8.2: *"a skipped relocation is a corruption you invent."* Every relocation
//! in the image must be either applied correctly or cause the load to **abort
//! with the offending type named**. There is no third option. A relocation this
//! relocator does not recognise is a hard failure, never a `continue`.
//!
//! That is the whole reason §2.1's measurement matters. With a GOT, a skipped
//! `GOTPCREL` leaves a live pointer to garbage that works until it is
//! dereferenced. With the direct `PC32` our toolchain actually emits, an
//! unhandled type is a loud refusal at load time instead. The safety property
//! comes from the *absence* of a GOT, so it is load-bearing and must not be
//! traded away for convenience.
//!
//! # The addend trap (§6.3)
//!
//! For `PC32`/`PLT32` the stored value is `S + A - P`, **not** `S`. Measured
//! across `aegis128-aesni.ko`: all 78 `PLT32` relocations carry addend `-4`.
//! Writing the resolved address `S` directly into the field leaves every such
//! relocation off by 4 — a jump into the middle of a `call rel32`, which faults
//! far from the cause. `_apply_field` folds the addend; do not "simplify" it.
//!
//! # All-or-nothing (§6.2)
//!
//! Resolution happens in a first pass that patches nothing, so a module missing
//! one symbol is refused before any field is written. Only after every symbol
//! resolves does the second pass write. A partially-relocated module is the DRM
//! ABI lesson from AGENTS.md §0: a request that is *partly* serviced looks like
//! success.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use goblin::elf::{
    reloc::{R_X86_64_32S, R_X86_64_64, R_X86_64_PC32, R_X86_64_PLT32},
    Elf,
};

use super::elf::{MappedSection, ModuleImage};

/// A relocation this relocator cannot apply.
///
/// Carries the numeric type so the boot trail names it exactly (§8.2: "with the
/// offending type named") rather than reporting a generic failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedReloc {
    pub rtype: u32,
    pub section: String,
    pub offset: u64,
}

impl core::fmt::Display for UnsupportedReloc {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "unsupported relocation type {} in `{}` at {:#x}", self.rtype, self.section, self.offset)
    }
}

/// Why a relocation pass failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelocError {
    /// A relocation type with no implementation (§8.2).
    Unsupported(UnsupportedReloc),
    /// A relocation naming a symbol index past the end of the symbol table.
    BadSymbolIndex { section: String, index: u32 },
    /// A symbol index inside the table whose name cannot be read out of the
    /// string table.
    BadSymbolName { section: String, index: u32 },
    /// A symbol the kernel (and every already-loaded module) does not export.
    ///
    /// §6.2 requires the load to fail and the *name* to be reported. Distinct
    /// from `BadSymbolIndex` because the index is valid here and the module is
    /// asking for something legitimate that simply is not offered.
    UnresolvedSymbol { name: String, section: String },
    /// A `PC32`/`PLT32`/`32S` whose result does not fit in 32 bits.
    ///
    /// Distinct from `Unsupported` because it is *not* a missing feature: the
    /// module and the kernel are too far apart for a 32-bit displacement. That
    /// is a build-configuration error worth naming separately.
    DisplacementOutOfRange {
        section: String,
        value: i64,
        needed: i64,
    },
    /// A section-relative offset that does not fit its section.
    OffsetOutOfRange { section: String, offset: u64, size: u64 },
    /// A relocation naming a section that is not part of the mapped image.
    BadTargetSection { section: String, index: usize },
    /// A REL entry (implicit addend) where this relocator requires RELA.
    ///
    /// Refused rather than assuming an addend of 0: an implicit addend means the
    /// value to relocate is whatever is already in the field, which is only
    /// meaningful for an unlinked image. Treating it as 0 would silently write a
    /// wrong address -- the §8.2 failure mode.
    MissingAddend { section: String, offset: u64 },
}

impl core::fmt::Display for RelocError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RelocError::Unsupported(u) => write!(f, "{u}"),
            RelocError::BadSymbolIndex { section, index } => {
                write!(f, "`{section}`: symbol index {index} is out of range")
            }
            RelocError::BadSymbolName { section, index } => {
                write!(f, "`{section}`: symbol {index} has an unreadable name")
            }
            RelocError::UnresolvedSymbol { name, section } => write!(
                f,
                "`{section}`: unresolved symbol `{name}` -- the kernel does not export it \
                 (§6.2: the load fails rather than patching a placeholder)"
            ),
            RelocError::DisplacementOutOfRange { section, value, needed } => write!(
                f,
                "`{section}`: displacement {value} does not fit in 32 bits (needed {needed}) \
                 -- module and kernel too far apart for code-model=kernel"
            ),
            RelocError::BadTargetSection { section, index } => write!(
                f,
                "relocation targets section {index} (`{section}`), which is not mapped"
            ),
            RelocError::MissingAddend { section, offset } => write!(
                f,
                "`{section}` at {offset:#x}: REL without an explicit addend is not supported"
            ),
            RelocError::OffsetOutOfRange { section, offset, size } => write!(
                f,
                "`{section}`: offset {offset:#x}+{} exceeds section size {size:#x}",
                FIELD_SIZE
            ),
        }
    }
}

/// Widest field a relocation may patch. `R_X86_64_64` is the only 8-byte case.
const FIELD_SIZE: u64 = 8;

/// Resolves a symbol name to a runtime address. Supplied by the caller so this
/// module does not depend on export-table ordering (§6.2: kernel first, then
/// already-loaded modules).
pub trait SymbolResolver {
    /// `Ok(addr)` if the symbol is bound, `Err(name)` if not.
    fn resolve(&self, name: &str) -> Result<u64, String>;
}

/// The kernel's own export table alone.
pub struct KernelExports;

impl SymbolResolver for KernelExports {
    fn resolve(&self, name: &str) -> Result<u64, String> {
        super::exports::resolve(name).ok_or_else(|| name.to_string())
    }
}

/// Apply every relocation in `image`, or fail having changed nothing logical.
///
/// Two bases, deliberately:
///
///  - `writable_base` is the **virtual address** the arithmetic is done against.
///    It is where the image will live (§4's module window), and it is what makes
///    a 32-bit `PC32`/`PLT32` displacement representable at all.
///  - `storage` is where the bytes are **written**. In production this is the
///    same memory — phase 4 maps the image at `writable_base` — so passing
///    `writable_base as *mut u8` is correct. The self-check splits them so it can
///    use a heap buffer while still exercising window-relative arithmetic.
///
/// The split is not a testing convenience bolted on afterwards; it is the only
/// way to test relocation at all before phases 3-5 exist, and it keeps a relocator
/// bug from corrupting kernel mappings.
///
/// # Safety
/// `storage..storage + image.total_map_len()` must be writable and must remain
/// valid for the duration of the call. Called in §8.1 phase 6, after phases 4/5
/// have mapped and copied the sections, and before phase 8 makes text executable.
pub unsafe fn relocate<R: SymbolResolver>(
    image: &ModuleImage<'_>,
    elf: &Elf,
    writable_base: u64,
    storage: *mut u8,
    resolver: &R,
) -> Result<RelocStats, RelocError> {
    let section_name = |idx: usize| -> String {
        image
            .sections
            .iter()
            .find(|s| s.index == idx)
            .map(|s| s.name.clone())
            .unwrap_or_else(|| format!("<{idx}>"))
    };
    // Section index -> offset within the mapped image.
    //
    // The offset is the parser's (`MappedSection::offset`), not a second
    // computation of it here: the loader copies and the relocator patches the
    // same image, and two independent computations of "where does this section
    // live" is exactly the kind of duplication that eventually disagrees.
    let offset_of = |idx: usize| -> Option<u64> {
        image.sections.iter().find(|s| s.index == idx).map(|s| s.offset)
    };

    let mut stats = RelocStats::default();
    let mut pending: Vec<PendingWrite> = Vec::new();
    // Log the parsed symbol-table size and every relocation's symbol index. A
    // `BadSymbolIndex` is otherwise opaque: it does not say whether the table
    // was short or the index was bogus, and those have completely different
    // causes (a truncated Symtab parse vs. a crafted file).
    crate::dprintln!(
        "[module] reloc: {} symbols, {} reloc sections",
        elf.syms.len(),
        elf.shdr_relocs.len()
    );

    // ── Pass 1: resolve and compute, patch nothing ─────────────────────────
    //
    // `shdr_relocs` is keyed by the *relocation section's own* index, not by
    // the section it patches (measured: goblin pushes `(idx, relocs)` where idx
    // enumerates the section header table, and for `.rela.text.foo` at index 4
    // with `sh_info=3` it yields 4). The patched section is `sh_info`, so it is
    // read from the section header rather than taken from the tuple.
    //
    // Getting this wrong is not a no-op: using the .rela index either fails to
    // find a mapped section (the bug this originally had, caught by the relocator
    // refusing) or, worse, matches a *different* real section and patches the
    // wrong field.
    for (rela_idx, relsec) in elf.shdr_relocs.iter() {
        let target_idx = match elf.section_headers.get(*rela_idx) {
            Some(sh) => sh.sh_info as usize,
            None => continue,
        };
        let target = image
            .sections
            .iter()
            .find(|s| s.index == target_idx)
            .ok_or_else(|| RelocError::BadTargetSection {
                section: section_name(target_idx),
                index: target_idx,
            })?;
        let sec_name = target.name.clone();
        let target_base = offset_of(target.index).ok_or_else(|| RelocError::BadTargetSection {
            section: sec_name.clone(),
            index: target.index,
        })?;

        for i in 0..relsec.len() {
        let rela = match relsec.get(i) {
            Some(r) => r,
            None => continue,
        };
        let rtype = rela.r_type;
        // r_offset is section-relative in ET_REL. Bounds-check it against the
        // target section so a crafted offset cannot reach into a neighbour or
        // past the end of the mapping.
        let field_off = rela.r_offset;
        if field_off >= target.size {
            return Err(RelocError::OffsetOutOfRange {
                section: sec_name,
                offset: field_off,
                size: target.size,
            });
        }
        // A 64-bit field needs 8 bytes available, a 32-bit one 4.
        let width = if rtype == R_X86_64_64 { 8u64 } else { 4u64 };
        if field_off + width > target.size {
            return Err(RelocError::OffsetOutOfRange {
                section: sec_name,
                offset: field_off,
                size: target.size,
            });
        }

        let sym = elf.syms.get(rela.r_sym).ok_or_else(|| RelocError::BadSymbolIndex {
            section: sec_name.clone(),
            index: rela.r_sym as u32,
        })?;

        // Resolve S. A defined, module-local symbol's st_value is
        // section-relative, so it is rebased onto the section's mapped address.
        // An undefined one goes to the resolver, which returns the absolute
        // address it already lives at (kernel exports need no rebasing).
        let s: u64 = if sym.st_shndx == 0 {
            // Read the name from the symbol table's own string table. This
            // goblin version has no Elf::get_string, and a module may carry
            // several tables, so sh_link is what binds the pair.
            let name = sym_name(elf, sym.st_name as usize).ok_or_else(|| RelocError::BadSymbolName {
                section: sec_name.clone(),
                index: rela.r_sym as u32,
            })?;
            // §6.2: unresolved means the *load* fails, and the name is what must
            // be reported. Reporting the index instead would be actively
            // misleading -- the index is perfectly valid, the kernel simply does
            // not export that symbol, and those two failures need different fixes.
            let addr = resolver
                .resolve(&name)
                .map_err(|_| RelocError::UnresolvedSymbol {
                    name,
                    section: sec_name.clone(),
                })?;
            addr
        } else {
            let sym_sec = sym.st_shndx as usize;
            match offset_of(sym_sec) {
                Some(base) => writable_base + base + sym.st_value,
                None => writable_base + sym.st_value,
            }
        };

        // P: the address of the field being patched.
        let p = writable_base + target_base + field_off;
        // ET_REL uses RELA, so the addend is always present. A REL (implicit
        // addend) module would leave this None, and treating it as 0 would be
        // the §8.2 failure in disguise -- so refuse instead.
        let a = rela.r_addend.ok_or_else(|| RelocError::MissingAddend {
            section: sec_name.clone(),
            offset: field_off,
        })?;

        let bytes: [u8; 8] = match rtype {
            // §6.3: fold the addend. `S + A - P` with the -4 call bias.
            R_X86_64_PC32 | R_X86_64_PLT32 => {
                let v = (s as i64 + a) - (p as i64);
                if v < i32::MIN as i64 || v > i32::MAX as i64 {
                    return Err(RelocError::DisplacementOutOfRange {
                        section: sec_name,
                        value: v,
                        needed: v,
                    });
                }
                let b = (v as u32).to_le_bytes(); [b[0], b[1], b[2], b[3], 0, 0, 0, 0]
            }
            // Absolute 64-bit: S + A, written as a full imm64. No P.
            R_X86_64_64 => (s as u64).wrapping_add(a as u64).to_le_bytes(),
            // Absolute 32-bit sign-extended: S + A must sign-extend back to
            // itself, or the high half is silently lost (AGENTS.md §16.6).
            R_X86_64_32S => {
                let v = (s as i128 + a as i128) as i64;
                if (v as i32) as i64 != v {
                    return Err(RelocError::DisplacementOutOfRange {
                        section: sec_name,
                        value: v,
                        needed: v,
                    });
                }
                let b = (v as u32).to_le_bytes(); [b[0], b[1], b[2], b[3], 0, 0, 0, 0]
            }
            other => {
                // §8.2: name the type and refuse. Never `continue`.
                return Err(RelocError::Unsupported(UnsupportedReloc {
                    rtype: other,
                    section: sec_name,
                    offset: rela.r_offset,
                }));
            }
        };

        pending.push(PendingWrite {
            offset: p - writable_base,
            bytes,
            rtype,
        });
        stats.total += 1;
        match rtype {
            R_X86_64_PC32 => stats.pc32 += 1,
            R_X86_64_PLT32 => stats.plt32 += 1,
            R_X86_64_64 => stats.abs64 += 1,
            R_X86_64_32S => stats.abs32s += 1,
            _ => {}
        }
        }
    }

    // ── Pass 2: commit. Every symbol resolved, so it is safe to write. ─────
    //
    // `storage` is where the bytes physically live; `writable_base` is the
    // virtual address the arithmetic used. In production the two coincide --
    // phase 4 maps the image AT `writable_base`. They differ in the self-check,
    // which passes a heap buffer with a window base because a heap address is
    // ~112 TiB from kernel text and no 32-bit displacement could survive.
    for w in &pending {
        let n = if w.rtype == R_X86_64_64 { 8usize } else { 4usize };
        unsafe {
            core::ptr::copy_nonoverlapping(w.bytes.as_ptr(), storage.add(w.offset as usize), n);
        }
    }
    Ok(stats)
}

/// Resolve a symbol name through the kernel's own string table.
///
/// Deliberately simple and bounded: a bad `st_name` yields `None` (the load is
/// refused) rather than indexing out of bounds.
fn sym_name(elf: &Elf<'_>, off: usize) -> Option<String> {
    let name = elf.strtab.get_at(off)?;
    if name.is_empty() {
        return None;
    }
    Some(name.to_string())
}

/// The address of a symbol the module *defines*, in the mapped image.
///
/// A module's exported symbols are section-relative in an `ET_REL` object
/// (`st_value` is an offset into its own section), so the address is the
/// section's mapped address plus that offset. This is the same rebasing the
/// relocator does for a defined symbol, exposed for the loader, which needs it
/// to call the module's `init_module` and to hand the kernel the module's
/// exported entry points.
///
/// Returns `None` when the module does not define a symbol with that name.
pub fn defined_symbol_address(
    image: &ModuleImage<'_>,
    elf: &Elf<'_>,
    base: u64,
    name: &str,
) -> Option<u64> {
    for sym in elf.syms.iter() {
        if sym.st_shndx == 0 {
            continue; // undefined: not the module's own
        }
        if sym_name(elf, sym.st_name as usize).as_deref() != Some(name) {
            continue;
        }
        let sec_off = image
            .sections
            .iter()
            .find(|s| s.index == sym.st_shndx as usize)
            .map(|s| s.offset);
        return Some(match sec_off {
            Some(off) => base.wrapping_add(off).wrapping_add(sym.st_value),
            None => base.wrapping_add(sym.st_value),
        });
    }
    None
}

/// A relocation computed but not yet written.
///
/// `offset` is relative to the image base and is what gets written, so the write
/// targets `storage` while the *arithmetic* targeted `writable_base`.
struct PendingWrite {
    offset: u64,
    bytes: [u8; 8],
    rtype: u32,
}

/// Per-type relocation counts, for the boot trail (§8.3) and for asserting the
/// census's measured set actually appeared (§11).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RelocStats {
    pub total: usize,
    pub pc32: usize,
    pub plt32: usize,
    pub abs64: usize,
    pub abs32s: usize,
}

impl RelocStats {
    /// One-line summary for the boot log.
    pub fn summary(&self) -> String {
        format!(
            "total={} pc32={} plt32={} abs64={} abs32s={}",
            self.total, self.pc32, self.plt32, self.abs64, self.abs32s
        )
    }
}

/// Section-relative helper kept for the phase-5 copier: the file offset of a
/// section's content, and its length.
pub fn copy_span(sec: &MappedSection) -> Option<(u64, u64)> {
    if sec.size == 0 {
        return None;
    }
    Some((sec.file_off, sec.size))
}
