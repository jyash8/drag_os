# Storage Drivers — PCI, Virtio-blk, AHCI, NVMe Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Repo note:** this is not a git repo, so there are no `git commit` steps. Each task ends with the build + QEMU verification run (the same command is the "checkpoint").

**Goal:** Add block-device support in four stages — a minimal PCI bus driver, then virtio-blk (QEMU dev/test), then AHCI (real SATA), then NVMe (modern SSD) — exposed through a small block layer that future filesystems can read.

**Architecture:** Stage A is a PCI config-space enumeration + BAR mapping service (the substrate every device driver needs). Stage B uses it for virtio-blk in QEMU (fast iteration, no real hardware). Stage C is AHCI via its HBA BAR (a real SATA spec implementation). Stage D is NVMe via its BAR + doorbells. All four stages end in one common `BlockDevice` trait so the VFS/initrd work is unchanged until a real FS wants them.

**Tech Stack:** Rust `#![no_std]` kernel, x86_64 MMIO (all BARs land inside the HHDM device region already reserved by `MMIO_DEVICE_REGIONS`), `0xCF8`/`0xCFC` PCI config ports, QEMU with `-drive` attaching virtio-blk / SATA / NVMe disks, `-serial file:` verification.

---

## Current state (what this builds on)

- No PCI code anywhere. `MMIO_DEVICE_REGIONS` (shared/src/boot/mod.rs:18-25) reserves `0xFEE0_0000`, `0xFEC0_0000`, `0xFED0_0000`, `0xFF00_0000` for APIC/HPET/device-hole — **note:** real device BARs under 4GB land in the `0xFF00_0000`-ish region the last entry covers only partially; the plan maps BARs explicitly on demand.
- `HHDM_BASE` maps all physical memory, so `phys` → `HHDM_BASE + phys` reaches any BAR.
- `kernel/src/fs/vfs.rs` has `VNode::Pipe`, `VNode::InitrdFile`, `VNode::Directory`, `VNode::Char` — no block node yet.
- `initrd/` is a tar the bootloader loads; storage is entirely absent.

## Stage A — PCI enumeration

### Task A1: PCI config-space access

**Files:**
- Create: `kernel/src/drivers/pci.rs`
- Modify: `kernel/src/dri- [x] **Step 1: Create `kernel/src/drivers/pci.rs`**
- [x] **Step 2: Read BARs**
- [x] **Step 3: Full enumeration**
- [x] **Step 4: Register module + smoke test**
- [x] **Step 5: BAR mapping helper**

---

## Stage B — virtio-blk

### Task B1: Virtio queue basics
- [x] **Step 1: virtio header access**
- [x] **Step 2: Implement the split virtqueue**
- [x] **Step 3: Device reset + feature negotiation**
- [x] **Step 4: Verify virtio device is found**

---

### Task B2: virtio-blk reads
- [x] **Step 1: Block request format**
- [x] **Step 2: `read_sector(&self, sector: u64, buf: &mut [u8])`**
- [x] **Step 3: The `BlockDevice` trait**
- [x] **Step 4: Verify a real read**

---

## Stage C — AHCI

### Task C1: AHCI HBA probe + HBA memory
- [x] **Step 1: Find the AHCI device**
- [x] **Step 2: HBA register offsets**
- [x] **Step 3: Bring the HBA up**
- [x] **Step 4: ATA IDENTIFY + read**
- [x] **Step 5: Verify**

---

## Stage D — NVMe

### Task D1: NVMe controller init + admin identify
- [x] **Step 1: Find the NVMe device**
- [x] **Step 2: Controller memory map**
- [x] **Step 3: Init sequence**
- [x] **Step 4: Admin Identify (CNS 0x01)**
- [x] **Step 5: Verify**

---

### Task D2: NVMe I/O read
- [x] **Step 1: Create I/O queues**
- [x] **Step 2: I/O read command**
- [x] **Step 3: Implement `BlockDevice`**
- [x] **Step 4: Verify a real read**

## Final task: unify behind `BlockDevice`
- [x] **Step 1: Register all discovered block devices**
- [x] **Step 2: Smoke test reads through the trait**

---

## Deferred (explicitly out of scope)
- A real filesystem (ext2/vfat) — this plan stops at block devices.
- Writing (all drivers here are read-only; write paths are symmetric and left for the FS plan).
- DMA interrupts (polling only — matches the current PIC/APIC state; wire interrupts after the APIC plan lands).
- virtio legacy / MMIO transport, PCIe ECAM config space, MSI-X.
