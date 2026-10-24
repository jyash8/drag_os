use std::path::PathBuf;
use std::process::Command;

/// Split off the next whitespace-delimited token, returning it with the
/// remainder (leading whitespace already trimmed).
fn next_token(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start();
    if s.is_empty() {
        return None;
    }
    match s.find(char::is_whitespace) {
        Some(i) => Some((&s[..i], s[i..].trim_start())),
        None => Some((s, "")),
    }
}

fn is_hex_token(t: &str) -> bool {
    !t.is_empty() && t.chars().all(|c| c.is_ascii_hexdigit())
}

/// Find the byte offset of a `label:` definition in a nasm listing.
///
/// nasm's `-l` listing is columnar:
///
/// ```text
///      99 000000A8 0000000000000000        entry_slot: dq 0
///     ^^^  ^^^^^^  ^^^^^^^^^^^^^^^^        ^^^^^^^^^^^ source column
///     line   offset   encoded bytes
/// ```
///
/// The offset column is only trustworthy for a *definition*, so the label has
/// to begin the source column. A line that merely *references* the symbol
/// (`mov eax, [cr3_slot]`) also contains the name, and matching on a bare
/// substring would silently return the wrong offset.
fn listing_offset(listing: &str, label: &str) -> Option<usize> {
    let want = format!("{label}:");
    for line in listing.lines() {
        // Comment and blank lines have no offset column at all, and they are
        // the *first* lines of a nasm listing. So a line that cannot be parsed
        // is skipped, never propagated: `?` here would abandon the search
        // before reaching the definition.
        let Some((lineno, mut rest)) = next_token(line) else {
            continue;
        };
        if lineno.parse::<u32>().is_err() {
            continue;
        }
        // Column 2: offset, always 8 hex digits.
        let Some((offset, after)) = next_token(rest) else {
            continue;
        };
        if offset.len() != 8 || !is_hex_token(offset) {
            continue;
        }
        rest = after;
        // Column 3: encoded bytes, in whitespace-separated hex groups. Consume
        // them all, then the source column begins.
        while let Some((token, tail)) = next_token(rest) {
            if is_hex_token(token) {
                rest = tail;
            } else {
                break;
            }
        }
        if rest.starts_with(&want) {
            return usize::from_str_radix(offset, 16).ok();
        }
    }
    None
}

fn main() {
    println!("cargo:rerun-if-changed=src/arch/x86_64/trampoline.asm");

    let src = "src/arch/x86_64/trampoline.asm";
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let bin = out.join("trampoline.bin");
    let lst = out.join("trampoline.lst");

    // `-l` emits the listing that carries the symbol offsets. Without it the
    // kernel would have to hard-code them, and the Rust constants and the
    // assembly layout would be two hand-maintained copies of one fact.
    let status = Command::new("nasm")
        .args(["-f", "bin", "-l"])
        .arg(&lst)
        .args(["-o"])
        .arg(&bin)
        .arg(src)
        .status()
        .expect("failed to run nasm; is it installed?");
    assert!(status.success(), "nasm failed");

    println!("cargo:rustc-env=TRAMPOLINE_BIN={}", bin.display());

    let listing = std::fs::read_to_string(&lst)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", lst.display()));
    let blob_len = std::fs::metadata(&bin)
        .unwrap_or_else(|e| panic!("failed to stat {}: {e}", bin.display()))
        .len() as usize;

    let emit = |label: &str| {
        let off = listing_offset(&listing, label).unwrap_or_else(|| {
            panic!(
                "trampoline.asm: no definition of `{label}` found in the nasm \
                 listing. The kernel patches this slot at a fixed offset, and \
                 a missing or renamed label means the offsets in \
                 `trampoline.rs` no longer describe the blob."
            )
        });
        // The slots are u64 and patched in place, so each must be 8-aligned and
        // wholly inside the blob. Catching it here turns a silent
        // "APs jump to garbage" into a build failure.
        assert_eq!(off % 8, 0, "`{label}` is not 8-aligned at {off:#x}");
        assert!(
            off + 8 <= blob_len,
            "`{label}` at {off:#x} runs past the {blob_len:#x}-byte trampoline blob"
        );
        off
    };

    let entry = emit("entry_slot");
    let cr3 = emit("cr3_slot");
    let stack = emit("stack_slot");

    // The three slots are written independently, so overlap would corrupt one
    // with another. They are adjacent by construction; assert it rather than
    // assume it.
    for (a, b, na, nb) in [
        (entry, cr3, "entry_slot", "cr3_slot"),
        (cr3, stack, "cr3_slot", "stack_slot"),
    ] {
        assert_eq!(
            a + 8,
            b,
            "`{na}` ({a:#x}) and `{nb}` ({b:#x}) are not adjacent 8-byte slots"
        );
    }

    // Emitted as Rust integer literals rather than env vars: `str::parse` is
    // not a const fn, so `const { env!(..).parse() }` does not compile. A
    // generated file keeps the value a literal the optimiser can see, and
    // leaves a single place to look for where an offset came from.
    std::fs::write(
        out.join("trampoline_offsets.rs"),
        format!(
            "// GENERATED by crates/kernel/build.rs from trampoline.asm.\n\
             // Do not edit: these are the byte offsets of `entry_slot`,\n\
             // `cr3_slot` and `stack_slot` in the assembled blob.\n\
             pub const ENTRY: usize = {entry:#x};\n\
             pub const CR3: usize = {cr3:#x};\n\
             pub const STACK: usize = {stack:#x};\n"
        ),
    )
    .expect("failed to write trampoline_offsets.rs");
}

