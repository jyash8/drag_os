# Module load pipeline — the loader's remaining phases, and the KPI's export list

**Goal:** take the module system from "parses and relocates in the abstract" to
"loads a real module, runs it, and calls into it in both directions", and close
the open item in the module-facing API (the export allow-list is hand-written and
unchecked against the linked kernel).

**Design authority:** `docs/module-system.md`. **KPI authority:**
`docs/superpowers/plans/2026-08-08-linux-kpi-complexity.md`.

## Ruling: the order of work, where the two documents disagree

The module system design (milestones 3-9) and the KPI analysis
("do not front-load a full module loader; build the static KPI first") point at
different orders.

**Ruling: continue the module system; treat the KPI's contribution as the export
table, which already exists.** The KPI analysis assumed the module loader did not
exist yet. It does: `crates/kernel/src/module/` already has the ELF parser and
validator (§8.1 phases 1-2), the relocator (phase 6's arithmetic), the export
table, the `vermagic` gate, and a boot self-check for each. The remaining loader
work is the payoff, not a front-loaded risk. The rest of the static KPI
(allocators, IRQ-save locks, timers, wait queues, `request_irq`, the device
model) has no consumer yet and is a separate slice.

**What this costs if wrong:** if the device model turns out to be needed before
the loader is finished, the KPI slice comes back to the front. Nothing built
here is wasted either way — the export table is the bridge in both directions.

## Pre-flight: shared interfaces between tasks

| Producer | Consumer | What | Found |
|---|---|---|---|
| Task 1 (serial marker) | Task 2, 3 (new self-checks) | the marker string the test suite greps | consistent — one constant, `module::selftest::SELFTEST_MARKER` |
| Task 2 (slot allocator) | Task 3 (loader) | `slot::reserve(len) -> Slot` and `Slot { base, len }` | consistent — the loader takes the slot's base as the layout base |
| Task 2 (`layout(base)`) | Task 3 (loader) | section addresses | consistent — the loader assigns addresses from the slot base, then allocates pages for them |

`Pre-flight: no shared interfaces` does not apply: the three rows above are the
interfaces, and each is checked when its producing task is read.

## Global constraints

- The kernel is `no_std` and cannot run host unit tests (its panic handler
  conflicts with `std`). **Every test in this plan is a boot self-check**, run by
  booting the guest and reading the serial log — the pattern the existing
  `[module] selftest` already uses.
- The self-check's result must reach the **serial**, not only the debugcon, or
  the test suite cannot assert it. `println!` goes to the serial;
  `dprintln!` goes to the debugcon.
- AGENTS.md §12.2: module code obeys the kernel's lock ordering and interrupt
  discipline exactly as in-tree code does. The loader does not relax them.
- AGENTS.md §8.2: a relocation type the relocator does not know aborts the load
  with the type named. There is no "skip it" path.
- AGENTS.md §6.2: all-or-nothing symbol resolution. Nothing is patched until
  every symbol resolves.
- The module window is `0xffffffffa0000000`, 64 MiB (`elf::MODULE_WINDOW_BASE`).
  It is above `0xffffffff80000000` so `R_X86_64_32S` can encode module
  addresses, and outside HHDM so a module address is never ambiguous with a
  physical address.

## Test command for every task

```
cargo run -p tools --release -- build-kernel && \
cargo run -p tools --release -- build-fs && \
cargo run -p tools --release -- test
```

The new self-check is asserted by the suite (Task 1 makes that possible).

---

## Task 1: the module self-check, assertable from the test suite

Today the module self-check prints to the debugcon (`dprintln!`), so the test
suite — which reads the serial — cannot see it and cannot catch a regression in
the module code. This task makes the self-check's result reach the serial with a
stable marker, and adds the suite case that asserts it.

**Files:** `crates/kernel/src/module/selftest.rs`, `crates/kernel/src/main.rs`,
`crates/tools/src/test_runner.rs`.

### Steps

1. **Add the suite case first (RED).** In `crates/tools/src/test_runner.rs` add
   a test case:

   ```rust
   TestCase {
       name: "module_selftest",
       command: "/bin/true",
       expected: &["MODULE_SELFTEST_OK"],
       timeout_secs: 60,
       gl_only: false,
   },
   ```

   The marker is expected to be printed by the kernel during boot, not by the
   command — the command is only there to give the harness something to run
   after the boot output has been produced.

2. **Run the suite — expect FAIL.** The kernel does not print the marker yet.

   `Expected:` `[FAIL] Test 'module_selftest' - timed out after 60s` (the marker
   never appears, so the case cannot pass).

3. **Print the marker to the serial.** In `crates/kernel/src/module/selftest.rs`
   add a constant and a helper:

   ```rust
   /// The marker the test suite greps for. On the serial, not the debugcon:
   /// the suite reads the serial, so a self-check that only writes to the
   /// debugcon is invisible to it -- which is the state this task fixes.
   pub const SELFTEST_MARKER: &str = "MODULE_SELFTEST_OK";
   ```

   and report the outcome of the module self-checks with `println!` (serial) in
   addition to the existing `dprintln!` (debugcon), e.g. in `main.rs`:

   ```rust
   match kernel::module::selftest::check_module(&bytes, 8) {
       Ok(()) => { kernel::dprintln!("[module] selftest OK");
                   kernel::println!("{}", kernel::module::selftest::SELFTEST_MARKER); }
       Err(e) => { kernel::dprintln!("[module] selftest FAILED: {e}");
                   kernel::println!("MODULE_SELFTEST_FAIL: {e}"); }
   }
   ```

   The relocator self-check's result goes the same way, so one marker covers
   both checks: the marker is printed only when **both** pass.

4. **Run the suite — expect PASS.** `Expected:` `[PASS] Test 'module_selftest'`.

5. **Commit.**

   ```
   test(module): make the module self-check assertable from the suite
   ```

---

## Task 2: the slot allocator and the module window (design milestone 3)

`docs/module-system.md` §4.1: slots come from a bump allocator inside the
64 MiB window, 64 KiB aligned, never reused while a module occupies them (in v1:
never). Today the parser assigns every module's sections the *same* addresses
(`elf.rs`: `let mut cursor = MODULE_WINDOW_BASE;`), so a second module would
land on top of the first.

**Files:** `crates/kernel/src/module/slot.rs` (new),
`crates/kernel/src/module/elf.rs`, `crates/kernel/src/module/selftest.rs`.

### Steps

1. **Write the failing self-check (RED).** In `selftest.rs` add
   `check_slots()`:

   - reserve two slots of 64 KiB and 128 KiB;
   - assert both are 64 KiB aligned;
   - assert they are inside the window (`base >= MODULE_WINDOW_BASE`,
     `base + len <= MODULE_WINDOW_END`);
   - assert they do not overlap;
   - assert a reservation larger than the window is **refused**, and that the
     refusal leaves the allocator usable (a later small reservation still
     succeeds);
   - assert the first slot's base is the window base (the design makes the
     loader's base choice the ASLR mechanism, and the first module gets the
     bottom of the window).

2. **Run the guest — expect the check to fail.**

   `Expected:` the boot log contains `[module] slot selftest FAILED: ...`, and
   the `module_selftest` suite case fails.

3. **Implement `slot.rs`.**

   ```rust
   pub struct Slot { pub base: u64, pub len: u64 }

   /// Reserve `len` bytes from the module window.
   ///
   /// §4.1: 64 KiB aligned, never reused (v1 has no unload), and a
   /// reservation that would cross the window's end is refused rather than
   /// clamped -- a clamped reservation would map a module partly outside the
   /// window, where `R_X86_64_32S` cannot reach it.
   pub fn reserve(len: u64) -> Option<Slot>;
   ```

   The allocator is a global `SpinLock<u64>` cursor starting at
   `elf::MODULE_WINDOW_BASE`. It is never reset: a slot is never reused.

4. **Give the parser a base.** Split the address assignment out of
   `parse_module`:

   - `parse_module(data, vermagic)` validates and returns the sections with
     *offsets* only;
   - `layout(image, base)` assigns the absolute addresses, starting at `base`.

   This is §8.1's phase 2/3 split: validation must not need a slot, and the
   slot's size is only known after validation.

5. **Update the existing self-check** (`check_module`) for the new signature,
   and make it assert that the sections land inside the slot it was given.

6. **Run the guest — expect the check to pass.**

   `Expected:` `[module] slot selftest OK` in the boot log, and the
   `module_selftest` suite case passes.

7. **Commit.**

   ```
   feat(module): allocate module slots from the module window
   ```

---

## Task 3: the load pipeline, and the round trip (design milestones 2 and 7)

The loader's phases 3-9, in §8.1's order: reserve, allocate, copy, relocate,
zero, publish, init. This is the task that makes a module *run*, and it is
where the round-trip test finally becomes possible.

**Files:** `crates/kernel/src/module/loader.rs` (new),
`crates/kernel/src/module/reloc.rs`, `crates/kernel/src/module/selftest.rs`,
`crates/kernel/src/main.rs`.

### Steps

1. **Write the failing self-check (RED).** `check_load()`:

   - load `/modules/fixture.o` through `module::read_whole` (the §8.1 phase 1
     path) and the new loader;
   - assert the load **succeeds**;
   - assert the module's `init` ran (the fixture's init returns a value; the
     loader records it);
   - assert the **kernel→module** direction: calling the module's exported
     `dragos_fixture_probe` from the kernel returns the value the fixture
     computes;
   - assert the **module→kernel** direction: the fixture's init calls the
     kernel's exported `dprintln_fmt`, so a successful init *is* that
     direction; the self-check asserts the kernel's export was reached (the
     kernel counts the calls).

2. **Run the guest — expect the check to fail** (the loader does not exist yet).

   `Expected:` `[module] load selftest FAILED: ...`.

3. **Implement `loader.rs`.** One function per phase, each logging one line
   (§8.3), run with interrupts disabled and single-threaded:

   ```rust
   pub fn load(data: &[u8]) -> Result<Loaded, LoadError>;
   ```

   - **reserve** — `slot::reserve(img.total_map_len())`;
   - **allocate** — one page set per allocatable section, sized from `sh_size`
     and page-aligned; map with the §4.2 permissions (`PRESENT | GLOBAL` for
     text and rodata, `PRESENT | WRITABLE | GLOBAL` for data, `NO_EXECUTE`
     until phase 8);
   - **copy** — the section's file bytes into its pages, skipping `NOBITS`;
   - **relocate** — the existing relocator, against the export table,
     **all-or-nothing**: resolve every undefined symbol first, and refuse the
     load with the unresolved name if any is missing;
   - **zero** — `.bss`;
   - **publish** — clear `NO_EXECUTE` on the text pages and `invlpg`;
   - **init** — call the module's `init_module` and record its return value.

   The text pages are mapped non-executable until phase 8, so a partially
   relocated module can never be executing (§4.2).

4. **Run the guest — expect the check to pass.**

   `Expected:` `[module] load selftest OK`, and the suite's `module_selftest`
   case passes.

5. **Add the negative test.** A module with an unknown symbol must be refused
   cleanly, with no partial state: the self-check builds a module image whose
   undefined symbol is not in the export table, asserts the load is refused and
   names the symbol, and asserts the slot allocator still works afterwards
   (the refusal consumed no slot).

6. **Run the guest — expect the negative test to pass.**

   `Expected:` `[module] negative selftest OK`.

7. **Commit.**

   ```
   feat(module): load a module, relocate it, and call into it
   ```

---

## Task 4: the export allow-list becomes data, and is checked against the kernel

`docs/module-system.md` §6.1 wants the export table generated from the linked
kernel's symbol table. `exports.rs` records why that cannot be done from
`build.rs` — the kernel is not linked yet when `build.rs` runs — and leaves the
table hand-written. The consequence is that a typo in an export name is
invisible: the symbol is exported, no module can ever resolve it, and nothing
says so.

This task does the part that does not need a post-link pass: the **names** move
into a data file, exactly as `modules/reloc_types.txt` does for the relocation
types, and the names are checked against the linked kernel.

**Files:** `modules/kernel_exports.txt` (new),
`crates/kernel/src/module/exports.rs`, `crates/kernel/src/module/selftest.rs`,
`crates/tools/build.rs` or a post-link step in the tools crate.

### Steps

1. **Write the failing check (RED).** A self-check that reads the names from
   `modules/kernel_exports.txt` (embedded with `include_str!`) and compares them
   with the table's names, and asserts the table is not empty. With the file
   absent, the check fails.

2. **Move the names into `modules/kernel_exports.txt`**, one per line, with a
   comment per name saying what it is for. The same pattern as
   `modules/reloc_types.txt`, including the rule that the file is the single
   source of truth for the *names*.

3. **Make `exports.rs` read the file** with `include_str!`, and assert at boot
   that the file's names and the table's names are the same set. A mismatch is
   a boot-time failure with both lists printed — the same shape as
   `reloc_types.txt`'s "adding a type here without implementing it fails the
   census".

4. **Check the names against the linked kernel.** A post-link step in the tools
   crate reads `kernel.elf`'s `.symtab` and asserts every name in the file
   exists there. A typo becomes a build failure instead of an export that can
   never be resolved.

   `Expected:` the tools build fails with the offending name when a name in
   the file is not in the kernel's symbol table.

5. **Run the guest and the suite — expect both green.**

6. **Commit.**

   ```
   feat(module): the export allow-list is data, and is checked against the kernel
   ```

---

## Not in this plan

- **Unload** (§10) — deferred by the design, and deferred here.
- **`init_module(2)`** (milestone 9) — a syscall, and a security decision
  about who may load code.
- **The rest of the static KPI** — timers, `request_irq`, generic wait queues,
  the device model. No consumer yet; a separate slice.
- **The in-tree delivery path** (milestone 8) — the fixture already arrives
  through the filesystem, so the *delivery* difference is small, but it is a
  separate milestone and this plan does not claim it.
