# P2 — Dynamic USB HID Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace boot-protocol guessing with real HID report-descriptor parsing, per-device report lengths/layouts, and interrupt-driven (MSI-X) delivery instead of timer-tick polling.

**Architecture:** Parse the HID report descriptor at enumeration into a compact report layout (field list: usage page/usage, bit offset/size, count). `deliver_report` walks the layout instead of calling hardcoded `decode_keyboard/mouse/tablet`. MSI-X comes last — polling stays until interrupts prove reliable, gated by a per-controller flag.

**Tech Stack:** Existing xHCI driver `kernel/src/drivers/usb/xhci.rs` (1375 lines), `DmaBuf` rings, `decode_*` functions at lines ~1075-1210. Depends on P1 (per-device evdev nodes to attach layouts to).

---

### File map

- Modify: `kernel/src/drivers/usb/xhci.rs` — descriptor fetch, report-map parser, layout-driven decode, MSI-X table setup, interrupt handler.
- Modify: `kernel/src/arch/x86_64/pci.rs` (check exact path: `kernel/src/arch/x86_64/pci.rs` per xhci.rs imports) — MSI-X capability walk + table mapping.
- Modify: `kernel/src/interrupts.rs` — vector allocation for xHCI MSI-X.

---

### Task 1: Fetch + parse HID report descriptors

**Files:**
- Modify: `kernel/src/drivers/usb/xhci.rs` (near `enumerate_port`, ~line 834-882)

- [x] **Step 1: Fetch the report descriptor.** After the config-descriptor walk, issue `GET_DESCRIPTOR(HID_REPORT)` = `ctrl_xfer(slot, 0x81, 0x06, 0x2200, iface_num, len, &mut buf)`. Length comes from the HID class descriptor (parse `wDescriptorLength` at offset 7 of the HID descriptor found during the walk — extend the walk to record it). Cap at 512 bytes.

- [x] **Step 2: Minimal report-map parser.** New `fn parse_report_map(desc: &[u8]) -> ReportLayout` handling short items only: Usage Page (0x04), Usage (0x08), Logical Min/Max (0x14/0x24), Report Size (0x74), Report Count (0x94), Input (0x80), Collection/End (0xA0/0xC0, depth-tracked, ignored). Output:

```rust
struct ReportField { usage_page: u16, usage: u16, bit_off: u16, bit_size: u8 }
struct ReportLayout { fields: Vec<ReportField, 64>, total_bits: u16 }
```

Stop parsing on: long items (0xFE — skip), unknown prefix with size>4 (abort, fall back to boot layout), or 64 fields. This covers QEMU usb-kbd/mouse/tablet and 95% of real hardware.

- [x] **Step 3: Fallback.** If fetch or parse fails, keep today's classification (`(iface_sub, iface_proto)` match at line ~877) and hardcoded `report_len`. Log `[XHCI] slot N: report-desc parse failed, boot fallback`.
- [x] **Step 4: Build**

Run: `cargo run -p tools -- build-kernel 2>&1 | tail -1`
Expected: success, zero behavior change (parser runs, result only logged: `dprintln!("[XHCI] slot {}: report layout {} fields {} bits")`).

- [x] **Step 5: Commit**

```bash
git add kernel/src/drivers/usb/xhci.rs
git commit -m "feat(usb): HID report descriptor fetch and parse with boot fallback"
```

---

### Task 2: Layout-driven decode, retire hardcoded decoders

**Files:**
- Modify: `kernel/src/drivers/usb/xhci.rs` (`deliver_report` ~1010, `decode_keyboard/mouse/tablet`, `hid_to_linux`, `press_key`)

- [x] **Step 1: Generic usage→evdev mapping.** Replace the three `decode_*` bodies with one walk:

```rust
fn decode_layout(dev: &mut UsbDevice, report: &[u8]) {
    // buttons: usages 0x90001..0x90005 → BTN_LEFT/RIGHT/MIDDLE (+ side/extra)
    // keys: usage page 0x07 (keyboard) → hid_to_linux(usage) edges vs dev.last_keys
    // motion: usages 0x10030 (X), 0x10031 (Y) → signed deltas or absolute (see step 2)
    // wheel: usage 0x10038 → REL_WHEEL
}
```

Reuse `hid_to_linux` (now positional-correct) and the edge-detection pattern from `decode_keyboard`. Keep `push_event/push_syn` unchanged.

- [x] **Step 2: Absolute vs relative.** Usage Min/Max (Logical Min/Max from the layout) decides: max > 255 on X/Y → absolute device, scale by `(max-min)` instead of the hardcoded `HID_MAX=32767`, keep the remainder accumulator pattern from `decode_tablet:1163-1169`. Max ≤ 255 → relative signed deltas.
- [x] **Step 3: Delete `decode_keyboard/mouse/tablet`** once `deliver_report` routes everything through `decode_layout`. Keep `DevKind` only for the boot-fallback path.
- [x] **Step 4: Verify byte-identical behavior on QEMU devices.** Boot headless, type nothing, move nothing: `[input-stats]` counters must match pre-change baselines (`kbdrep`, `mrep` grow only from idle traffic as before). Interactive: letters/digits/arrows/mouse/tablet all still produce correct keysyms (regression check for the T→N class of bug: press T, expect `keysym raw=116`).

Run: `cargo run -p tools -- build-kernel 2>&1 | tail -1 && cargo run -p tools -- build-fs 2>&1 | tail -1`

- [x] **Step 5: Commit**

```bash
git add kernel/src/drivers/usb/xhci.rs
git commit -m "feat(usb): layout-driven HID decode, generic absolute/relative"
```

---

### Task 3: MSI-X interrupt delivery (replaces timer polling)

**Files:**
- Modify: PCI MSI-X setup, `kernel/src/interrupts.rs`, `kernel/src/drivers/usb/xhci.rs` (`drain_events`, `poll`)

- [x] **Step 1: MSI-X table setup at xHCI init.** Walk PCI caps for `0x11` (MSI-X), map the table BAR, allocate one vector: Message Address = LAPIC base `0xFEE00000 | (cpu_id << 12)`, Data = free IDT vector (pick from the currently unused range — audit `interrupts.rs` IDT assignments first), unmask via function mask + vector control. Store `msi_x: bool` on the controller.
- [x] **Step 2: Interrupt handler.** New `xhci_interrupt_handler`: `drain_events()` + LAPIC EOI. Same body as today's `poll()` drain, minus `scan_hotplug` (keep hotplug on a 1Hz timer — PORTSC polling is cheap and race-free).
- [x] **Step 3: Gate and compare.** `poll()` drains only `if !hc.msi_x`. Boot with MSI-X forced on: idle `mrep/kbdrep` counters must still climb, key/mouse latency subjectively better, no `libinput ... lagging behind` warnings. If anything regresses, one-line revert to polling (keep both paths, flag-selected).
- [x] **Step 4: Commit**

```bash
git add kernel/src/drivers/usb/xhci.rs kernel/src/interrupts.rs kernel/src/arch/x86_64/pci.rs
git commit -m "feat(usb): MSI-X interrupt delivery for xHCI event ring"
```

---

## Self-review

- Spec coverage: descriptor parsing (Task 1), generic decode (Task 2), interrupts (Task 3). Hotplug enumeration already exists (`scan_hotplug`); P1 wires it to node lifecycle.
- No placeholders: exact control-request bytes, exact struct shapes, exact verify commands.
- Type consistency: `ReportField/ReportLayout` defined once in Task 1, consumed in Task 2.
