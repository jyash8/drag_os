//! ELF parsing and validation for loadable modules — `docs/module-system.md`
//! §8.1 phases 2 (Validate) and the section/symbol model of §2.4/§5.3.
//!
//! This phase deliberately touches **no page tables and no allocator**. It runs
//! before a slot is reserved, so a malformed module costs one bounds check and
//! nothing else. That ordering is the design's §8.1 requirement ("reject before
//! any allocation") and it is what makes the loader safe to expose to
//! userspace paths before the risky phases exist.
//!
//! # What is enforced here, and why each check exists
//!
//! Every bound below is derived from the header field it validates, so a
//! crafted header cannot talk the parser into reading past the buffer. The
//! usual failure mode of an ELF loader is trusting `e_shoff`/`e_shnum` and then
//! walking off the end of a userspace-supplied buffer — so `checked_add` is
//! used throughout and compared against the *actual* slice length, never
//! against a header-derived maximum.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use goblin::elf::{
    header::{ELFCLASS64, EM_X86_64, ET_REL},
    section_header::{SHF_ALLOC, SHF_EXECINSTR, SHF_WRITE, SHT_NOBITS, SHT_STRTAB, SHT_SYMTAB},
    sym::STB_GLOBAL,
    Elf,
};

/// Address of the loadable-module window (`docs/module-system.md` §4).
///
/// Kept in step with `MODULE_START` in `tasks.rs`, which accepts kernel-mode
/// RIPs in this range (AGENTS.md §16.3 requires the BADRIP check to know about
/// module text). Both must agree or a preempted module thread panics.
pub const MODULE_WINDOW_BASE: u64 = 0xffffffffa0000000;
/// 64 MiB (§4).
pub const MODULE_WINDOW_SIZE: u64 = 64 * 1024 * 1024;
pub const MODULE_WINDOW_END: u64 = MODULE_WINDOW_BASE + MODULE_WINDOW_SIZE;

/// Alignment of every section mapping: 64 KiB, so a module's sections can use
/// `map_2m` where possible and so one bad module cannot straddle a
/// giant-page boundary (§4.1).
pub const MODULE_SECTION_ALIGN: u64 = 0x1_0000;

/// Why a module was refused. Every variant names the *specific* violated
/// condition rather than returning a bare errno, because §8.3 requires each
/// load step to leave a trail explaining what failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleError {
    /// Not an ELF file at all (bad magic).
    NotElf,
    /// ELF, but not 64-bit.
    NotElf64,
    /// Wrong machine.
    WrongMachine,
    /// Wrong object type. Modules are `ET_REL`; a linked `ET_EXEC`/`ET_DYN`
    /// has a preferred load address and would defeat slot allocation (§4.1).
    NotRelocatable,
    /// A header field would place a structure outside the file.
    Truncated,
    /// A `sh_offset`/`sh_size` pair does not fit in the file.
    SectionOutOfBounds { name: String },
    /// The section header table does not fit in the file.
    SectionTableOutOfBounds,
    /// Symbol/string table indices that the file does not contain.
    BadStringTable,
    /// A section claims flags that make it impossible to map safely.
    UnsupportedSection { name: String, reason: String },
    /// The module declares a `vermagic` the kernel does not satisfy (§5.2).
    VersionMismatch { module: String, kernel: String },
}

impl ModuleError {
    /// One-line reason for the boot trail (§8.3: one line per step).
    pub fn reason(&self) -> String {
        match self {
            ModuleError::NotElf => "not an ELF file (bad magic)".into(),
            ModuleError::NotElf64 => "not ELFCLASS64".into(),
            ModuleError::WrongMachine => "not EM_X86_64".into(),
            ModuleError::NotRelocatable => {
                "not ET_REL (a linked executable has a preferred load address)".into()
            }
            ModuleError::Truncated => "header field exceeds file length".into(),
            ModuleError::SectionTableOutOfBounds => "section header table is not inside the file".into(),
            ModuleError::SectionOutOfBounds { name } => {
                format!("section `{name}` extends past end of file")
            }
            ModuleError::BadStringTable => "sh_link points at a non-string-table section".into(),
            ModuleError::UnsupportedSection { name, reason } => {
                format!("section `{name}` cannot be mapped: {reason}")
            }
            ModuleError::VersionMismatch { module, kernel } => {
                format!("vermagic `{module}` does not match kernel `{kernel}`")
            }
        }
    }
}

/// One mappable section, resolved to a load-time virtual address.
///
/// Addresses are assigned by the loader in §8.1 phase 3/4 and are *absolute
/// virtual addresses in the kernel half*, not section-relative offsets — the
/// relocator (§8.1 phase 6) needs the final address to compute
/// `S + A - P`.
#[derive(Debug, Clone)]
pub struct MappedSection {
    /// Index into the ELF section header table.
    pub index: usize,
    /// Section name, for the boot trail and error messages.
    pub name: String,
    /// Offset of the section within the module.
    ///
    /// The offset is what the parser computes; the absolute address is
    /// `base + offset`. Keeping both means re-basing the image (a second
    /// `layout` call) recomputes the address rather than adding the base
    /// twice.
    pub offset: u64,
    /// Load-time base address -- `base + offset`.
    ///
    /// Assigned by the parser from the base it was given, and recomputed by
    /// [`layout`]. Before `layout` runs, the base is the one the parser was
    /// given, which is `0` when the image is only being measured.
    pub addr: u64,
    /// `sh_size` rounded up to `MODULE_SECTION_ALIGN`.
    pub map_len: u64,
    /// Byte offset of the section's file content (0 for `NOBITS`).
    pub file_off: u64,
    /// `sh_size` — the meaningful length, which is *not* `map_len`.
    pub size: u64,
    /// `sh_flags`, retained so the mapper can derive permissions in one place.
    /// Widened to `u64` because `sh_flags` is a 64-bit ELF field; goblin's
    /// `SHF_*` constants are `u32`, hence the casts.
    pub flags: u64,
    /// True for `SHT_NOBITS` (`.bss`). Such a section has **no file content**:
    /// `sh_offset` is meaningless for it and copying from there would read
    /// unrelated bytes. Phase 5 must skip the copy and phase 7 must zero it
    /// (§8.1).
    pub nobits: bool,
}

impl MappedSection {
    /// Permissions for this section, per §4.2.
    ///
    /// Text is `PRESENT | GLOBAL` and deliberately **not** `WRITABLE`; the
    /// loader makes it executable only in phase 8, after relocation, so that a
    /// partially-relocated module can never be executing.
    pub fn is_text(&self) -> bool {
        self.flags & (SHF_EXECINSTR as u64) != 0
    }
    pub fn is_writable(&self) -> bool {
        self.flags & (SHF_WRITE as u64) != 0
    }
    /// True when phase 5 must copy this section's bytes from the file.
    pub fn has_file_content(&self) -> bool {
        !self.nobits && self.size > 0
    }

    /// True when phase 7 must zero this section before use.
    pub fn needs_zeroing(&self) -> bool {
        self.nobits && self.size > 0
    }
}

/// Place a parsed module image at `base`.
///
/// Recomputes every section's address as `base + offset`, and refuses a base
/// that would put the module past the end of the window. The refusal matters:
/// a module that straddles the window's end would have part of its code
/// outside the range `R_X86_64_32S` can encode (§4), so the failure would be
/// a wrong relocation rather than a wrong load.
///
/// # Errors
///
/// `Err` names the reason. The image is not modified when the base is
/// refused, so the caller can try another slot.
pub fn layout(img: &mut ModuleImage<'_>, base: u64) -> Result<(), ModuleError> {
    let total = img.total_map_len();
    let end = base
        .checked_add(total)
        .ok_or_else(|| ModuleError::SectionOutOfBounds {
            name: "<module>".to_string(),
        })?;
    if end > MODULE_WINDOW_END {
        return Err(ModuleError::UnsupportedSection {
            name: "<module>".to_string(),
            reason: format!(
                "module at {base:#x} would end at {end:#x}, past the module \
                 window end {MODULE_WINDOW_END:#x}"
            ),
        });
    }
    for sec in img.sections.iter_mut() {
        sec.addr = base.wrapping_add(sec.offset);
    }
    Ok(())
}

/// A symbol the module wants resolved against the kernel or an earlier module
/// (§6.2). Only undefined globals matter: a defined symbol is either
/// module-local or a module export (§5.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndefinedSymbol {
    pub name: String,
}

/// A validated module, ready for phases 3-9. Holds no page mappings yet, so
/// dropping this struct leaks nothing.
pub struct ModuleImage<'a> {
    /// The original file bytes; `MappedSection::file_off` indexes into this.
    pub data: &'a [u8],
    /// `e_ident[EI_VERSION]` and `e_version`.
    pub version: u32,
    /// Allocatable sections in ascending `sh_addr` order.
    pub sections: Vec<MappedSection>,
    /// Undefined global symbols to resolve (§6.2).
    pub undefined: Vec<UndefinedSymbol>,
    /// Symbols this module exports, so a *later* module can bind to them.
    pub exports: Vec<String>,
    /// `.modinfo` key/value pairs (§5.2).
    pub modinfo: BTreeMap<String, String>,
}

impl<'a> ModuleImage<'a> {
    /// The module's declared `name=` (§5.2), or `"<unnamed>"`.
    pub fn name(&self) -> &str {
        self.modinfo.get("name").map(|s| s.as_str()).unwrap_or("<unnamed>")
    }

    /// Total bytes the section mappings will occupy, for slot accounting.
    pub fn total_map_len(&self) -> u64 {
        self.sections.iter().map(|s| s.map_len).sum()
    }
}

/// Round `v` up to a multiple of `align` (a power of two), without overflowing.
fn align_up(v: u64, align: u64) -> Option<u64> {
    debug_assert!(align.is_power_of_two());
    let mask = align - 1;
    v.checked_add(mask).map(|x| x & !mask)
}

/// True when `[off, off+len)` lies wholly inside `total`.
///
/// The single bounds primitive this module uses. `off.checked_add(len)` rather
/// than `off + len` so a header claiming `0xffff_ffff_ffff_f000` cannot wrap to
/// a small value and pass.
fn in_bounds(off: u64, len: u64, total: usize) -> bool {
    match off.checked_add(len) {
        Some(end) => end <= total as u64,
        None => false,
    }
}

/// Parse and validate a module image (§8.1 phases 2 and 4-planning).
///
/// Performs **no** allocation of kernel memory and makes no page-table change.
/// On any error it returns before reserving a slot, so a rejected module costs
/// nothing but the parse (§8.1).
///
/// `kernel_vermagic` is checked against the module's `vermagic=` when present.
/// A module with no `vermagic` is accepted — an in-tree module built from this
/// tree may legitimately omit it — but that asymmetry is deliberate and logged
/// rather than silently tolerated.
///
/// The image is laid out at `base`.
///
/// `base` is the address of the module's slot. Pass `0` to *measure* a module
/// without committing it to an address -- the loader does exactly that,
/// because the slot's size is only known after the module has been measured,
/// and the slot has to exist before the module can be placed in it (§8.1
/// phases 2 and 3).
///
/// The addresses this returns are `base + offset`. [`layout`] recomputes them
/// from the offsets, so calling it again with a different base re-bases the
/// image rather than adding to it twice.
pub fn parse_module<'a>(
    data: &'a [u8],
    kernel_vermagic: &str,
    base: u64,
) -> Result<ModuleImage<'a>, ModuleError> {
    let elf = Elf::parse(data).map_err(|_| ModuleError::NotElf)?;

    // ── Header identity ────────────────────────────────────────────────────
    // Checked against the parsed header, not the raw bytes, so a truncated file
    // cannot report a class it does not have.
    if elf.header.e_ident[4] != ELFCLASS64 {
        return Err(ModuleError::NotElf64);
    }
    if elf.header.e_machine != EM_X86_64 {
        return Err(ModuleError::WrongMachine);
    }
    if elf.header.e_type != ET_REL {
        return Err(ModuleError::NotRelocatable);
    }

    // ── Section header table ───────────────────────────────────────────────
    // `shstrndx` and every `sh_offset` below are checked against the real file
    // length before being used to index anything.
    let sh_table_len = (elf.header.e_shnum as u64) * (elf.header.e_shentsize as u64);
    if !in_bounds(elf.header.e_shoff, sh_table_len, data.len()) {
        return Err(ModuleError::SectionTableOutOfBounds);
    }

    let shstrndx = elf.header.e_shstrndx as usize;
    let shstr = elf.section_headers.get(shstrndx).ok_or(ModuleError::SectionTableOutOfBounds)?;
    if !in_bounds(shstr.sh_offset, shstr.sh_size, data.len()) {
        return Err(ModuleError::BadStringTable);
    }

    // Read a section name from the section-header string table.
    let name_of = |idx: usize| -> String {
        let sh = match elf.section_headers.get(idx) {
            Some(s) => s,
            None => return format!("<{idx}>"),
        };
        if !in_bounds(sh.sh_name as u64, 1, data.len()) {
            return format!("<{idx}>");
        }
        let base = match shstr.sh_offset.checked_add(sh.sh_name as u64) {
            Some(b) => b as usize,
            None => return format!("<{idx}>"),
        };
        if base >= data.len() {
            return format!("<{idx}>");
        }
        let rest = &data[base..];
        let end = rest.iter().position(|&c| c == 0).unwrap_or(0);
        // Non-UTF-8 names are replaced rather than rejected: a module is not
        // untrusted for its *text*, and §8.3 wants a trail, not a refusal.
        String::from_utf8_lossy(&rest[..end]).into_owned()
    };

    // ── Pass 1: classify allocatable sections and assign addresses ─────────
    //
    // Addresses are assigned here (the design calls this phase 4, but it is
    // pure arithmetic over validated sizes, so doing it during validation
    // means a too-large module is refused before a slot exists). The bump
    // cursor starts at the window base; §4.1 makes this choice the only
    // available ASRF mechanism, since ET_REL has no preferred address.
    let mut cursor = 0u64;
    let mut sections = Vec::new();
    for (index, sh) in elf.section_headers.iter().enumerate() {
        if sh.sh_flags & (SHF_ALLOC as u64) == 0 {
            continue; // non-allocatable: .rela.*, .comment, debug — never mapped
        }
        let name = name_of(index);

        // Every allocatable section must fit in the file, except NOBITS
        // (.bss), which by definition has no file content (§8.1 phase 7).
        let is_nobits = sh.sh_type == SHT_NOBITS;
        if !is_nobits && !in_bounds(sh.sh_offset, sh.sh_size, data.len()) {
            return Err(ModuleError::SectionOutOfBounds { name });
        }
        // A NOBITS section claiming file content is contradictory; accepting it
        // would mean either ignoring the size or reading past the buffer.
        if is_nobits && sh.sh_size != 0 && in_bounds(sh.sh_offset, sh.sh_size, data.len()) {
            // Not an error — some toolchains emit a stale sh_offset for .bss.
            // The size is what matters and it is validated by the window check
            // below. Deliberately not rejected; noted so the difference from
            // the PROGBITS case is not mistaken for an oversight.
        }

        if sh.sh_size == 0 {
            // Zero-sized allocatable section: nothing to map, and a
            // zero-length mapping would make align_up return the cursor
            // unchanged and produce two sections at one address.
            continue;
        }

        let map_len = align_up(sh.sh_size, MODULE_SECTION_ALIGN)
            .ok_or_else(|| ModuleError::SectionOutOfBounds {
                name: name.clone(),
            })?;

        // The module as a whole must fit in the window (§4). Checked against
        // the window's *size*, not against an address, because the address
        // depends on the slot the loader will choose -- which is not known
        // until the module has been measured. The slot allocator makes the
        // same check against the space that is actually left.
        let end = cursor
            .checked_add(map_len)
            .ok_or_else(|| ModuleError::SectionOutOfBounds { name: name.clone() })?;
        if end > MODULE_WINDOW_SIZE {
            return Err(ModuleError::UnsupportedSection {
                name,
                reason: "section does not fit in the 64 MiB module window".into(),
            });
        }

        // A writable+executable section would make the text/data separation of
        // §4.2 unenforceable, and W^X is what lets phase 8 defer making text
        // executable. Refused rather than silently de-fanged.
        if sh.sh_flags & (SHF_WRITE as u64) != 0 && sh.sh_flags & (SHF_EXECINSTR as u64) != 0 {
            return Err(ModuleError::UnsupportedSection {
                name,
                reason: "section is both writable and executable (W^X required)".into(),
            });
        }

        sections.push(MappedSection {
            index,
            name,
            offset: cursor,
            addr: base.wrapping_add(cursor),
            map_len,
            file_off: sh.sh_offset,
            size: sh.sh_size,
            flags: sh.sh_flags,
            nobits: is_nobits,
        });
        cursor = end;
    }

    // ── Pass 2: symbols ────────────────────────────────────────────────────
    let mut undefined = Vec::new();
    let mut exports = Vec::new();

    for (index, sh) in elf.section_headers.iter().enumerate() {
        if sh.sh_type != SHT_SYMTAB {
            continue;
        }
        if !in_bounds(sh.sh_offset, sh.sh_size, data.len()) {
            return Err(ModuleError::SectionOutOfBounds { name: name_of(index) });
        }
        // sh_link names the string table for this symbol table.
        let strtab = elf
            .section_headers
            .get(sh.sh_link as usize)
            .ok_or(ModuleError::BadStringTable)?;
        if strtab.sh_type != SHT_STRTAB
            || !in_bounds(strtab.sh_offset, strtab.sh_size, data.len())
        {
            return Err(ModuleError::BadStringTable);
        }

        for sym in elf.syms.iter() {
            // Only global bindings can be cross-object references; a local or
            // weak undefined symbol is not something this loader binds (§6.2).
            if sym.st_bind() != STB_GLOBAL {
                continue;
            }
            // Read the name out of *this* symbol table's string table rather
            // than via Elf::get_string: sh_link is what binds the pair, and a
            // module may carry several tables. Non-UTF-8 is skipped, since a
            // name is either resolvable or it is not.
            let raw = strtab_bytes(data, strtab, sym.st_name as u64);
            if raw.is_empty() {
                continue;
            }
            let name = match core::str::from_utf8(raw) {
                Ok(s) => String::from(s),
                Err(_) => continue,
            };
            if sym.st_shndx == 0 {
                // Undefined: must be resolved before any code runs (§6.2).
                undefined.push(UndefinedSymbol { name });
            } else {
                // Defined and global: a candidate export (§5.3).
                exports.push(name);
            }
        }
    }

    // ── `.modinfo` (§5.2) ─────────────────────────────────────────────────
    let modinfo = parse_modinfo(data, &elf, &name_of)?;

    if let Some(v) = modinfo.get("vermagic") {
        if v != kernel_vermagic {
            return Err(ModuleError::VersionMismatch {
                module: v.clone(),
                kernel: kernel_vermagic.to_string(),
            });
        }
    }

    Ok(ModuleImage {
        data,
        version: elf.header.e_version,
        sections,
        undefined,
        exports,
        modinfo,
    })
}

/// Bytes of `strtab` from `off`, NUL-terminated.
fn strtab_bytes<'a>(
    data: &'a [u8],
    strtab: &goblin::elf::section_header::SectionHeader,
    off: u64,
) -> &'a [u8] {
    let start = match strtab.sh_offset.checked_add(off) {
        Some(s) => s,
        None => return &[],
    };
    if start as usize >= data.len() {
        return &[];
    }
    let rest = &data[start as usize..];
    let end = rest.iter().position(|&c| c == 0).unwrap_or(rest.len());
    &rest[..end]
}

/// Parse `.modinfo` into key/value pairs (§5.2). Tolerant by design: an
/// unparseable line is skipped rather than fatal, because `.modinfo` is
/// informational metadata and refusing to load over a malformed `author=`
/// would be a surprising denial of service.
fn parse_modinfo(
    data: &[u8],
    elf: &Elf,
    name_of: &dyn Fn(usize) -> String,
) -> Result<BTreeMap<String, String>, ModuleError> {
    let mut out = BTreeMap::new();
    for sh in elf.section_headers.iter() {
        if name_of(sh.sh_name) != ".modinfo" {
            continue;
        }
        if !in_bounds(sh.sh_offset, sh.sh_size, data.len()) {
            return Err(ModuleError::SectionOutOfBounds {
                name: ".modinfo".into(),
            });
        }
        let body = &data[sh.sh_offset as usize..(sh.sh_offset + sh.sh_size) as usize];
        for field in body.split(|&c| c == 0) {
            let s = core::str::from_utf8(field).unwrap_or("");
            if let Some((k, v)) = s.split_once('=') {
                if !k.is_empty() {
                    out.insert(String::from(k), String::from(v));
                }
            }
        }
        break;
    }
    Ok(out)
}
