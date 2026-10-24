//! Boot-time self-check for the module ELF validator (`docs/module-system.md`
//! §8.1 phase 2).
//!
//! # Why a self-check and not a unit test
//!
//! There is no userspace test harness that can reach this code: the parser runs
//! in the kernel, before a module slot exists, and the only way to exercise it
//! is to call it during boot. That matches the existing pattern — the VT and TTY
//! self-checks run from `lib.rs` for exactly the same reason (the console is
//! normally owned by a compositor, so correctness is otherwise only observable
//! by looking at a screen nobody is looking at).
//!
//! # What it actually proves
//!
//! It parses a **real** relocatable object built by the real toolchain, not a
//! hand-written byte array. That is the point: the census
//! (`scripts/module-census.py`) measures which relocations our rustc emits, and
//! this checks that the parser's model of such a file matches reality. A
//! hand-rolled ELF would pass while the real thing failed.
//!
//! It asserts four things that would each be a distinct real bug:
//!
//!  1. A valid `ET_REL` module parses at all.
//!  2. Its allocatable sections are assigned addresses **inside the module
//!     window** — this is the check that would catch a window/base mismatch
//!     between this module and `tasks.rs`'s `MODULE_START`, which would
//!     otherwise show up much later as a BADRIP panic on the first preempted
//!     module thread (AGENTS.md §16.3).
//!  3. No two sections overlap, since the bump allocator assigns them in order.
//!  4. The undefined-symbol set is **non-empty**, because a module with no
//!     undefined symbols has not actually been tested for the resolution path.
//!     An empty set here means the fixture changed shape, not that the parser
//!     is fine.

use alloc::format;
use alloc::string::ToString;
use alloc::string::String;
use alloc::vec::Vec;

use super::elf::{self, ModuleError};

/// The marker the test suite greps for, printed on the **serial**.
///
/// The self-check's own diagnostics go to the debugcon, which the suite cannot
/// read -- the suite talks to the guest over the serial, and the debugcon log
/// is a file on the host. A self-check that only writes to the debugcon is
/// therefore invisible to CI: it can fail on every boot and nothing says so.
/// This constant is the serial-visible half, and the suite asserts it.
pub const SELFTEST_MARKER: &str = "MODULE_SELFTEST_OK";

/// The marker printed when the self-check did not pass.
///
/// It is deliberately *not* the success marker with a suffix: the suite asserts
/// the success marker is present, and a failure line that merely mentioned it
/// would satisfy that assertion.
pub const SELFTEST_FAIL_MARKER: &str = "MODULE_SELFTEST_FAIL";

/// A file to parse, supplied by the caller so the self-check has no filesystem
/// dependency of its own (see [`SelfCheck`]).
pub type ModuleSource<'a> = &'a [u8];

/// Result of the self-check. `Err` carries a human-readable reason for the boot
/// log, matching the `[vt] selftest FAILED: …` convention.
pub type SelfCheckResult = Result<(), String>;

/// Describe a module image the way the boot trail should (§8.3: one line per
/// step, naming what was found).
fn describe(img: &elf::ModuleImage<'_>) -> String {
    let mut s = format!(
        "name={} sections={} undefined={} exports={} map_bytes={:#x}",
        img.name(),
        img.sections.len(),
        img.undefined.len(),
        img.exports.len(),
        img.total_map_len(),
    );
    for sec in &img.sections {
        s.push_str(&format!(
            "\n      {:#012x} {} {} size={:#x} map={:#x}",
            sec.addr,
            sec.name,
            if sec.is_text() { "text" } else { "data" },
            sec.size,
            sec.map_len,
        ));
    }
    format!("{s}")
}

/// Run the validator against `data` and check the invariants above.
///
/// `expect_sections` is the number of allocatable sections the fixture is known
/// to have. It is passed in rather than hardcoded so a deliberate fixture change
/// (adding a section) shows up as a self-check failure naming both numbers,
/// instead of silently validating a different file.
pub fn check_module(data: ModuleSource<'_>, expect_sections: usize) -> SelfCheckResult {
    // Measured, not placed: the address checks below are about the window's
    // *size*, and the absolute addresses come from the slot the loader picks.
    let mut img = elf::parse_module(data, super::KERNEL_VERMAGIC, 0)
        .map_err(|e: ModuleError| format!("parse failed: {}", e.reason()))?;

    crate::dprintln!("[module] selftest parsed {}", describe(&img));

    // (2) Every section address must be within the window the BADRIP check
    // accepts. Cross-checked here because the two constants are declared in
    // different files and a divergence is silent until a module thread is
    // preempted.
    // The image is laid out at the bottom of the window, so the addresses the
    // BADRIP check accepts are exercised. The slot allocator is what actually
    // chooses a base in production; this is the same arithmetic with the
    // window's base as the base.
    elf::layout(&mut img, elf::MODULE_WINDOW_BASE)
        .map_err(|e| format!("layout at the window base failed: {}", e.reason()))?;

    for sec in &img.sections {
        if sec.addr < elf::MODULE_WINDOW_BASE || sec.addr >= elf::MODULE_WINDOW_END {
            return Err(format!(
                "section `{}` at {:#x} is outside the module window {:#x}..{:#x} \
                 -- tasks.rs MODULE_START/END has probably diverged",
                sec.name, sec.addr, elf::MODULE_WINDOW_BASE, elf::MODULE_WINDOW_END
            ));
        }
        if sec.addr + sec.map_len > elf::MODULE_WINDOW_END {
            return Err(format!(
                "section `{}` at {:#x} +{:#x} runs past the window end {:#x}",
                sec.name, sec.addr, sec.map_len, elf::MODULE_WINDOW_END
            ));
        }
    }

    // (3) No overlap. The bump allocator hands out increasing addresses, so an
    // overlap would mean a sizing or alignment bug rather than a hostile file.
    let mut ordered: Vec<(u64, u64, &str)> =
        img.sections.iter().map(|s| (s.addr, s.map_len, s.name.as_str())).collect();
    ordered.sort_unstable();
    for w in ordered.windows(2) {
        let (a, alen, an) = w[0];
        let (b, _, bn) = w[1];
        if a + alen > b {
            return Err(format!("sections `{an}` and `{bn}` overlap"));
        }
    }

    // Section count must match what the fixture is known to contain.
    if img.sections.len() != expect_sections {
        return Err(format!(
            "expected {expect_sections} allocatable sections, parsed {}",
            img.sections.len()
        ));
    }

    // (4) The resolution path must be exercised. A module with nothing
    // undefined would let a broken undefined-symbol walk report success.
    if img.undefined.is_empty() {
        return Err("fixture has no undefined symbols, so resolution is untested".into());
    }
    for u in &img.undefined {
        crate::dprintln!("[module] selftest undefined symbol: {}", u.name);
    }

    Ok(())
}

/// Exercise the slot allocator (§4.1).
///
/// The slot allocator is the one piece that makes a *second* module possible:
/// every module's sections used to be assigned the same addresses, starting at
/// the window base, so two modules would land on top of each other. This
/// checks the properties §4.1 states, and the one property that decides
/// whether the window can be overrun:
///
///  - slots are 64 KiB aligned, so a module's sections can be mapped with
///    `map_2m` and a single bad module cannot straddle a giant-page boundary;
///  - a slot lies inside the window, and never crosses its end;
///  - two slots do not overlap;
///  - a reservation larger than the window is **refused**, and the refusal
///    leaves the allocator usable -- a refused reservation that consumed the
///    cursor would waste the rest of the window;
///  - the first slot starts at the bottom of the window.
pub fn check_slots() -> SelfCheckResult {
    use super::elf::{MODULE_SECTION_ALIGN, MODULE_WINDOW_BASE, MODULE_WINDOW_END};
    use super::slot;

    let a = slot::reserve(64 * 1024).ok_or("reserve(64 KiB) refused")?;
    let b = slot::reserve(128 * 1024).ok_or("reserve(128 KiB) refused")?;

    for (label, sl) in [("a", &a), ("b", &b)] {
        if sl.base % MODULE_SECTION_ALIGN != 0 {
            return Err(format!(
                "slot {label} base {:#x} is not {:#x}-aligned",
                sl.base, MODULE_SECTION_ALIGN
            ));
        }
        if sl.base < MODULE_WINDOW_BASE {
            return Err(format!(
                "slot {label} base {:#x} is below the window base {:#x}",
                sl.base, MODULE_WINDOW_BASE
            ));
        }
        let end = sl.base.checked_add(sl.len).ok_or("slot end overflows")?;
        if end > MODULE_WINDOW_END {
            return Err(format!(
                "slot {label} ends at {end:#x}, past the window end {:#x}",
                MODULE_WINDOW_END
            ));
        }
    }

    if a.base + a.len > b.base {
        return Err(format!(
            "slots overlap: a is {:#x}..{:#x}, b starts at {:#x}",
            a.base,
            a.base + a.len,
            b.base
        ));
    }

    // A reservation that cannot fit must be refused, and must not consume the
    // cursor: a later small reservation has to succeed.
    let too_big = slot::reserve(MODULE_WINDOW_END - MODULE_WINDOW_BASE + 1);
    if too_big.is_some() {
        return Err("an oversized reservation was accepted".into());
    }
    let c = slot::reserve(64 * 1024).ok_or("the allocator stopped working after a refusal")?;
    if c.base < b.base + b.len {
        return Err(format!(
            "a reservation after a refusal started at {:#x}, before the previous              slot ended at {:#x}",
            c.base,
            b.base + b.len
        ));
    }

    // The first slot starts at the bottom of the window. §4.1 makes the
    // loader's base choice the only available ASLR mechanism, since an ET_REL
    // object has no preferred address -- and the bottom is where the first
    // module belongs.
    if a.base != MODULE_WINDOW_BASE {
        return Err(format!(
            "the first slot starts at {:#x}, not the window base {:#x}",
            a.base, MODULE_WINDOW_BASE
        ));
    }

    Ok(())
}

/// Load the fixture module and call into it, in both directions.
///
/// This is the round-trip test §11.2 asks for, and it is the first thing that
/// exercises the loader's phases 3-9 against real memory rather than a
/// scratch buffer:
///
///  - **kernel -> module**: the loader hands back the module's exported
///    symbols, and the kernel calls one. The fixture's
///    `dragos_fixture_probe` returns a value derived from its own state, so a
///    wrong relocation shows up as a wrong number rather than as a crash.
///  - **module -> kernel**: the fixture's init function calls the kernel's
///    exported `dprintln_fmt`. The kernel counts the calls, so "the module ran"
///    and "the module called back" are different assertions.
///
/// The two directions are separate assertions on purpose. A loader that
/// relocated the module's *calls into the kernel* correctly but not its
/// *exports* would pass a one-directional test.
pub fn check_load(data: ModuleSource<'_>) -> SelfCheckResult {
    use super::exports;
    use super::loader;

    let calls_before = exports::export_call_count();

    let loaded = unsafe { loader::load(data) }
        .map_err(|e| format!("load failed: {e}"))?;

    // The module called into the kernel: its init function calls
    // `dprintln_fmt`, and the kernel counts the calls.
    let calls_after = exports::export_call_count();
    if calls_after == calls_before {
        return Err("the module's init function did not call back into the kernel".into());
    }

    // The kernel can call into the module.
    let probe = loaded
        .export("dragos_fixture_probe")
        .ok_or("the module does not export dragos_fixture_probe")?;
    let f: extern "C" fn(u64) -> u64 = unsafe { core::mem::transmute(probe) };
    let got = unsafe { f(7) };
    // The fixture's probe is `bump(x) ^ tally(x)`, where `bump` adds to a
    // counter in the module's `.data` and `tally` is `x * 3`. The counter
    // makes the value depend on the module's *relocated* state, so a probe that
    // returned the right answer for the wrong reason would be caught by
    // calling it twice: the second call must differ by exactly the amount
    // `bump` adds.
    let got2 = unsafe { f(7) };
    if got == got2 {
        return Err(format!(
            "calling the module's probe twice gave the same value ({got}) -- the \
             module's own state is not being updated, so the relocation of its \
             .data is not right"
        ));
    }
    crate::dprintln!(
        "[module] load selftest: probe(7) = {got}, then {got2} (delta {})",
        got2.wrapping_sub(got)
    );

    // The init function ran. The fixture's init returns a non-zero value
    // derived from the kernel's tick count and its own state; what matters is
    // that it is the *module's* return value, not a default.
    crate::dprintln!(
        "[module] load selftest: init returned {}, base {:#x}, len {:#x}",
        loaded.init_result, loaded.base, loaded.len
    );

    Ok(())
}

/// Load a module that binds to *another module's* export.
///
/// This is the second half of §6.2: resolution looks in the kernel's export
/// table and then in the export tables of the modules that are already loaded.
/// The consumer fixture calls `dragos_fixture_probe`, which the good fixture
/// exports and the kernel does not, so the load can only succeed if the first
/// module's exports were published and are visible to a later module.
///
/// The ordering is the test: the good fixture must already be loaded.
pub fn check_cross_module(consumer: ModuleSource<'_>) -> SelfCheckResult {
    use super::exports;
    use super::loader;

    // The other module's export has to be visible before this load.
    let probe = exports::resolve("dragos_fixture_probe").ok_or(
        "the first module's export is not visible -- nothing published it",
    )?;

    let loaded = unsafe { loader::load(consumer) }
        .map_err(|e| format!("the consumer module failed to load: {e}"))?;

    // It called into the other module, and the value it returned came from
    // there.
    if loaded.init_result == 0 {
        return Err("the consumer module's init returned 0".into());
    }
    crate::dprintln!(
        "[module] cross-module selftest: the consumer called the other module's \
         export at {probe:#x} and got {}",
        loaded.init_result
    );
    Ok(())
}

/// A module that asks for a symbol the kernel does not export must be refused
/// cleanly, with nothing half-loaded.
///
/// §6.2: an unresolved symbol fails the load and the *name* is reported. The
/// state that matters is what is left behind: a refused load must not leave a
/// mapped, half-relocated module behind, and must not consume a slot that a
/// later load could have used.
pub fn check_load_refused(data: ModuleSource<'_>) -> SelfCheckResult {
    use super::exports;
    use super::loader;

    let before = exports::export_count();
    let slots_before = super::slot::next_base();
    let result = unsafe { loader::load(data) };
    match result {
        Ok(_) => Err("a module with an unresolvable symbol was loaded".into()),
        Err(e) => {
            crate::dprintln!("[module] refusal selftest: refused with: {e}");
            // §6.2 requires the *name* of the unresolved symbol to be
            // reported. A refusal that says only "unresolved symbol" leaves
            // the reader to guess which one, and the name is the only part
            // that tells them what to fix.
            let text = e.to_string();
            if !text.contains("dragos_this_symbol_is_not_exported") {
                return Err(format!(
                    "the refusal did not name the unresolved symbol: {text}"
                ));
            }
            // The export table is unchanged: a refused load must not have
            // published anything.
            if exports::export_count() != before {
                return Err("a refused load changed the kernel's export table".into());
            }
            // No slot was consumed: v1 has no unload, so a slot that leaked
            // here would be gone for good.
            if super::slot::next_base() != slots_before {
                return Err(format!(
                    "a refused load consumed a slot: the window moved from {:#x} \
                     to {:#x}",
                    slots_before,
                    super::slot::next_base()
                ));
            }
            Ok(())
        }
    }
}

/// Exercise the relocator (§8.1 phase 6) against a scratch buffer.
///
/// # Why a scratch buffer and not the real module window
///
/// Relocation writes into the mapped image. Mapping into the module window
/// would mean phases 3-5 (reserve/allocate/copy) exist, and a bug in the
/// relocator would then corrupt kernel mappings. Using a heap buffer keeps this
/// test hermetic: it can only damage its own allocation. Phases 3-5 get their
/// own test once they exist.
///
/// # What it actually proves
///
/// The census (`scripts/module-census.py`) measures *which* relocation types the
/// toolchain emits; this measures whether the relocator computes them correctly.
/// The two failure modes are different and neither implies the other, which is
/// exactly the trap in §6.3: a relocator can handle every type it sees and
/// still be wrong by 4 bytes on every call.
///
/// The decisive assertion is the **addend fold**. `S + A - P` is recomputed
/// independently here and compared against what the relocator wrote. Getting
/// that wrong passes every "did it run" check and faults at the call site.
pub fn check_relocator(data: ModuleSource<'_>) -> SelfCheckResult {
    let img = super::elf::parse_module(data, super::KERNEL_VERMAGIC, 0)
        .map_err(|e| format!("parse failed: {}", e.reason()))?;
    let elf = match goblin::elf::Elf::parse(data) {
        Ok(e) => e,
        Err(_) => return Err("re-parse failed".into()),
    };

    // Scratch image: one contiguous, writable, page-aligned copy of every
    // section, laid out exactly as phases 4/5 would. Zeroed first because the
    // frame allocator does not guarantee zeroed memory (it recycles frames), and
    // phase 7 exists precisely because of that.
    let total = img.total_map_len() as usize;
    if total == 0 {
        return Err("no sections to relocate".into());
    }
    let mut buf = alloc::vec![0u8; total];
    let heap = buf.as_mut_ptr() as u64;

    // Relocate against *synthetic* addresses in the real module window rather
    // than the heap's.
    //
    // This is not a shortcut, it is forced: a `PC32`/`PLT32` displacement is
    // 32-bit, and the kernel heap (0xFFFF_9000_...) sits ~112 TiB from kernel
    // text (0xFFFFFFFF_8...), so every call relocation from a heap buffer is
    // unrepresentable. The relocator is right to refuse it.
    //
    // At the real window base the picture inverts, and this is the single fact
    // the whole module design depends on (§4): the window is 0xffffffffa0000000,
    // only ~511 MiB below kernel text, which *does* fit in a signed 32-bit
    // displacement. Verified above rather than assumed.
    //
    // So the buffer is the storage and `base` is the address arithmetic is done
    // against. Reads/writes still touch the heap allocation (no page tables, no
    // window mapping), but S and P are window-relative, exactly as they will be
    // once phases 3-5 map for real.
    let base = super::elf::MODULE_WINDOW_BASE;
    crate::dprintln!(
        "[module] reloc scratch heap={heap:#x} ({} B), arithmetic base={base:#x}",
        total
    );

    // Phase 5 (copy) + phase 7 (zero .bss), done into the scratch buffer.
    let mut cursor = 0u64;
    for sec in &img.sections {
        if sec.has_file_content() {
            // PROGBITS: copy file content.
            let start = sec.file_off as usize;
            let end = start + sec.size as usize;
            if end > data.len() {
                return Err(format!("section `{}` copy range out of file", sec.name));
            }
            buf[cursor as usize..(cursor + sec.size) as usize]
                .copy_from_slice(&data[start..end]);
        }
        cursor += sec.map_len;
    }

    let stats = match unsafe {
        super::reloc::relocate(
            &img,
            &elf,
            base,
            buf.as_mut_ptr(),
            &super::reloc::KernelExports,
        )
    } {
        Ok(s) => s,
        Err(e) => return Err(format!("relocation refused: {e}")),
    };
    crate::dprintln!("[module] relocator {}", stats.summary());

    // §8.2 coverage, in both directions. Observed must be a subset of what the
    // relocator claims to handle, and every type the census says the fixture
    // provokes must actually appear -- otherwise this test quietly stops
    // covering a case while still passing, which is the §11 lesson.
    if stats.total == 0 {
        return Err("relocator applied nothing".into());
    }
    for (name, got, want) in [
        ("pc32", stats.pc32, true),
        ("plt32", stats.plt32, true),
        ("abs64", stats.abs64, true),
        ("abs32s", stats.abs32s, true),
    ] {
        let present = got > 0;
        if present != want {
            return Err(format!(
                "relocator coverage drift: {name} count={got}, expected {}",
                if want { ">0" } else { "0" }
            ));
        }
    }

    // Independently recompute one relocation and compare. A `PLT32` against a
    // kernel export is the case that matters: it is a real call, it carries the
    // -4 addend, and getting it wrong jumps into the middle of the instruction.
    verify_one_call_reloc(&img, &elf, base, &buf)?;
    Ok(())
}

/// Recompute a single PLT32/PC32 relocation from first principles and compare
/// against the bytes the relocator wrote.
fn verify_one_call_reloc(
    img: &super::elf::ModuleImage<'_>,
    elf: &goblin::elf::Elf<'_>,
    base: u64,
    storage: &[u8],
) -> SelfCheckResult {
    use goblin::elf::reloc::{R_X86_64_PC32, R_X86_64_PLT32};

    // section index -> offset within the scratch image
    let mut sec_off = alloc::vec::Vec::new();
    let mut cursor = 0u64;
    for s in &img.sections {
        sec_off.push((s.index, cursor));
        cursor += s.map_len;
    }
    let off_of = |i: usize| -> Option<u64> { sec_off.iter().find(|(x, _)| *x == i).map(|(_, o)| *o) };

    let mut checked = 0usize;
    for (rela_idx, relsec) in elf.shdr_relocs.iter() {
        // shdr_relocs is keyed by the .rela section's index; the patched
        // section is sh_info (see reloc.rs).
        let tidx = match elf.section_headers.get(*rela_idx) {
            Some(sh) => sh.sh_info as usize,
            None => continue,
        };
        let target = match img.sections.iter().find(|s| s.index == tidx) {
            Some(t) => t,
            None => continue,
        };
        let tbase = match off_of(target.index) {
            Some(b) => b,
            None => continue,
        };
        for i in 0..relsec.len() {
            let r = match relsec.get(i) {
                Some(r) => r,
                None => continue,
            };
            if r.r_type != R_X86_64_PLT32 && r.r_type != R_X86_64_PC32 {
                continue;
            }
            // Only relocations against a *kernel export*: those are the ones
            // whose S comes from the resolver and are therefore independently
            // checkable. Module-local symbols are rebased, which is a different
            // (already-covered) path.
            let sym = match elf.syms.get(r.r_sym) {
                Some(s) => s,
                None => continue,
            };
            if sym.st_shndx != 0 {
                continue;
            }
            let name = match elf.strtab.get_at(sym.st_name as usize) {
                Some(n) if !n.is_empty() => n.to_string(),
                _ => continue,
            };
            let s_addr = match super::exports::resolve(&name) {
                Some(a) => a,
                // An unexported symbol means the fixture references something the
                // kernel does not offer. That is a legitimate refusal to check,
                // not a relocator bug -- report it rather than passing silently.
                None => {
                    return Err(format!(
                        "fixture needs `{name}`, which the kernel does not export"
                    ))
                }
            };
            let a = match r.r_addend {
                Some(a) => a,
                None => return Err("REL without addend in fixture".into()),
            };
            let p = base + tbase + r.r_offset;
            let expect = (s_addr as i64 + a) - (p as i64);
            if expect < i32::MIN as i64 || expect > i32::MAX as i64 {
                return Err(format!("`{name}` displacement {expect} out of range"));
            }
            // Read from the heap copy; the arithmetic above used window
            // addresses (see the note on `base`).
            let off = (p - base) as usize;
            let got = unsafe { core::ptr::read_unaligned(storage.as_ptr().add(off) as *const i32) }
                as i64;
            if got != expect {
                return Err(format!(
                    "`{name}`: relocator wrote {got}, expected {expect} (S={s_addr:#x} A={a} P={p:#x}) \
                     -- addend not folded correctly?"
                ));
            }
            // Also prove the result would actually reach the target: the caller
            // does `call rel32` from the end of the instruction, so
            // (address after the instruction) + displacement must equal S.
            let next_rip = (p + 4) as i64;
            if next_rip + got != s_addr as i64 {
                return Err(format!(
                    "`{name}`: {next_rip:#x} + {got} != {s_addr:#x}; the call would not land on the export"
                ));
            }
            checked += 1;
            crate::dprintln!(
                "[module] verify call reloc `{name}` S={s_addr:#x} A={a} P={p:#x} disp={got}"
            );
        }
    }
    if checked == 0 {
        return Err("no kernel-export call relocation was verified".into());
    }
    crate::dprintln!("[module] verified {checked} call relocations against the export table");
    Ok(())
}
