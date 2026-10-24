# VMA + MMIO Mapping Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the ad-hoc user-memory tracking (`mmap_base`/`heap_brk` + blind PTE walks) with a real per-process VMA (virtual memory area) layer, and add a kernel-side MMIO mapping facility (`map_device_memory`) with correct uncached semantics and device-page handling in fork/destroy — the substrate PCI drivers will need to map device BARs.

**Architecture:** Two dependent halves.
- **Part A (VMA):** a sorted `Vec<Vma>` on `Process` recording every user virtual region (start/end/prot/flags/kind). `mmap`/`munmap`/`mprotect`/`brk` create, trim, and query these records instead of blindly walking PTEs. `fork` clones the list; `execve` resets it. `munmap` gains frame reclamation by reusing the existing COW-aware `free_leaf` logic.
- **Part B (MMIO):** a new software `MMIO` page-table flag marks device pages so `fork_address_space` copies them verbatim (never COW) and `destroy_address_space`/`munmap` never free their physical frames. A new `kernel/src/memory/mmio.rs` maps device physical ranges into a reserved kernel-higher-half region at `0xFFFF_A000_0000_0000` with `CACHE_DISABLE` (UC).

**Tech Stack:** Rust nightly (`no_std`, `x86_64-unknown-none`), 4-level paging (`shared::memory::mapper::Mapper`), QEMU KVM + OVMF, isa-debug-exit test runner. Not a git repo — no commit steps; each task ends with build + QEMU verification.

**Verification protocol (every task):**
1. `./scripts/build-kernel.sh` — must end with `==> Kernel copied to:` and zero errors.
2. `cargo test -p kernel --lib --target x86_64-unknown-none` — must end with `Tests passed!`.
3. `./scripts/smoke.sh` — must exit 0 (only once the boot regression in `syscalls.rs`/`tasks.rs` from the pre-syscall-stack work is confirmed fixed; if the boot is still red, use (1)+(2) as the gate and skip (3) until it's green).

---

## Part A — VMA subsystem

### Task A1: VMA types and `VmaList` (pure data structure, no syscall wiring)

**Files:**
- Create: `kernel/src/memory/vma.rs`
- Modify: `kernel/src/memory/mod.rs` (add `pub mod vma;`)
- Test: `kernel/src/lib.rs` (add `#[test_case]`)

- [ ] **Step 1: Write the failing test**

Append to `kernel/src/lib.rs`:

```rust
#[test_case]
fn vma_list_insert_find_remove() {
    use kernel::memory::vma::{Vma, VmaKind, VmaList};

    let mut list = VmaList::default();
    assert!(list.insert(Vma { start: 0x1000, end: 0x3000, prot: 3, flags: 0x20, kind: VmaKind::Anonymous }).is_ok());
    assert!(list.insert(Vma { start: 0x5000, end: 0x6000, prot: 1, flags: 0x20, kind: VmaKind::Anonymous }).is_ok());
    // overlap -> error
    assert!(list.insert(Vma { start: 0x2000, end: 0x4000, prot: 1, flags: 0x20, kind: VmaKind::Anonymous }).is_err());

    assert_eq!(list.find(0x2000).map(|v| v.end), Some(0x3000));
    assert!(list.find(0x4000).is_none());

    // middle remove splits the 0x1000..0x3000 vma
    let removed = list.remove_range(0x1500, 0x2500);
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].start, 0x1500);
    assert_eq!(removed[0].end, 0x2500);
    assert_eq!(list.vmas.len(), 3); // 0x1000..0x1500, 0x2500..0x3000, 0x5000..0x6000
    assert_eq!(list.find(0x1000).map(|v| v.end), Some(0x1500));
    assert_eq!(list.find(0x2500).map(|v| v.end), Some(0x3000));

    // full-range remove
    let removed = list.remove_range(0x5000, 0x6000);
    assert_eq!(removed.len(), 1);
    assert!(list.find(0x5500).is_none());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p kernel --lib --target x86_64-unknown-none`
Expected: compile error, `module memory::vma` not found.

- [ ] **Step 3: Create `kernel/src/memory/vma.rs`**

```rust
use alloc::vec::Vec;

// Linux MAP_* / PROT_* bit values, as passed through the syscalls.
pub const PROT_READ: u64 = 1;
pub const PROT_WRITE: u64 = 2;
pub const PROT_EXEC: u64 = 4;
pub const MAP_SHARED: u64 = 0x01;
pub const MAP_PRIVATE: u64 = 0x02;
pub const MAP_FIXED: u64 = 0x10;
pub const MAP_ANONYMOUS: u64 = 0x20;

/// What backs a virtual region. `Device` pages are never COW'd and their
/// physical frames are never freed by the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmaKind {
    Anonymous,
    Heap,
    Device { phys: u64 },
}

#[derive(Debug, Clone, Copy)]
pub struct Vma {
    pub start: u64,
    pub end: u64, // exclusive
    pub prot: u64,
    pub flags: u64,
    pub kind: VmaKind,
}

impl Vma {
    pub fn contains(&self, addr: u64) -> bool {
        self.start <= addr && addr < self.end
    }
}

/// Sorted, non-overlapping VMAs for one process.
#[derive(Debug, Default, Clone)]
pub struct VmaList {
    pub vmas: Vec<Vma>,
}

impl VmaList {
    pub fn find(&self, addr: u64) -> Option<&Vma> {
        self.vmas.iter().find(|v| v.contains(addr))
    }

    pub fn insert(&mut self, vma: Vma) -> Result<(), ()> {
        if vma.start >= vma.end {
            return Err(());
        }
        if self.vmas.iter().any(|v| v.start < vma.end && vma.start < v.end) {
            return Err(());
        }
        let pos = self
            .vmas
            .iter()
            .position(|v| v.start > vma.start)
            .unwrap_or(self.vmas.len());
        self.vmas.insert(pos, vma);
        Ok(())
    }

    /// Removes `[start, end)` from the map, splitting overlapped VMAs.
    /// Returns the removed sub-ranges so the caller can free their frames.
    pub fn remove_range(&mut self, start: u64, end: u64) -> Vec<Vma> {
        let mut removed = Vec::new();
        let mut keep = Vec::with_capacity(self.vmas.len());
        for v in self.vmas.drain(..) {
            if v.end <= start || v.start >= end {
                keep.push(v);
            } else if start <= v.start && v.end <= end {
                removed.push(v);
            } else {
                if v.start < start {
                    keep.push(Vma { end: start, ..v });
                }
                if v.end > end {
                    keep.push(Vma { start: end, ..v });
                }
                removed.push(Vma {
                    start: core::cmp::max(v.start, start),
                    end: core::cmp::min(v.end, end),
                    ..v
                });
            }
        }
        self.vmas = keep;
        removed
    }
}
```

- [ ] **Step 4: Register the module**

`kernel/src/memory/mod.rs`, add `pub mod vma;` to the existing module list.

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p kernel --lib --target x86_64-unknown-none`
Expected: `Tests passed!` (includes `vma_list_insert_find_remove`).

- [ ] **Step 6: Verify**

```bash
./scripts/build-kernel.sh
cargo test -p kernel --lib --target x86_64-unknown-none
```

Expected: build clean, `Tests passed!`.

---

### Task A2: COW-aware `unmap_user_range` + free frames on munmap

**Files:**
- Modify: `kernel/src/memory/space.rs`
- Test: `kernel/src/lib.rs`

`munmap` must reclaim demand-paged/allocated frames and refcount-dec COW pages — reusing the existing `free_leaf` (space.rs:137). Add a helper that unmaps a user range and frees every non-device frame.

- [ ] **Step 1: Add `unmap_user_range` to `space.rs`**

After `demand_paging_fault` (space.rs:337):

```rust
/// Unmaps `[start, end)` in the current address space. Non-present demand
/// entries are cleared; present entries are freed via `free_leaf` (COW-aware).
/// `MMIO` device pages are unmapped but their frames are never freed.
pub fn unmap_user_range(start: u64, end: u64, allocator: &mut impl FrameAllocator, phys_offset: u64) {
    let mut page = start;
    while page < end {
        let entry_copy = {
            let pml4_phys = read_cr3();
            let pml4 = unsafe { &mut *(pml4_phys + phys_offset).as_mut_ptr::<PageTable>() };
            match leaf_entry_mut(pml4, VirtAddr(page), phys_offset) {
                Some(e) => Some(*e),
                None => None,
            }
        };
        let should_unmap = entry_copy.is_some();
        if let Some(e) = entry_copy {
            if e.is_present() && !e.flags().contains(PageTableFlags::MMIO) {
                let mut counts = COW_COUNTS.lock();
                free_leaf(e, allocator, counts.as_mut().expect("COW table not init"));
            }
        }
        if should_unmap {
            let pml4_phys = read_cr3();
            let pml4 = unsafe { &mut *(pml4_phys + phys_offset).as_mut_ptr::<PageTable>() };
            if let Some(entry) = leaf_entry_mut(pml4, VirtAddr(page), phys_offset) {
                entry.set_flags(PageTableFlags::NONE);
                entry.set_address(PhysAddr(0));
            }
            invlpg(page);
        }
        page += PAGE_SIZE;
    }
}
```

Note: `leaf_entry_mut` (space.rs:204) is already private and in the same file — no import needed.

- [ ] **Step 2: Build**

Run: `cargo b -p kernel --target x86_64-unknown-none`
Expected: compiles (warning about unused `unmap_user_range` is fine — wired up in A3).

---

### Task A3: Wire VMA into `mmap`/`munmap`/`mprotect`/`brk`

**Files:**
- Modify: `kernel/src/arch/x86_64/syscalls.rs` (`sys_mmap` 487, `sys_mprotect` 532, `sys_munmap` 556, `sys_brk` 1258)
- Modify: `kernel/src/tasks.rs` (`Process` struct + `new`/`fork_from`)
- Modify: `kernel/src/arch/x86_64/syscalls.rs` (`fork` arm ~809, `execve` arm ~928)
- Test: `kernel/src/lib.rs` + boot smoke

- [ ] **Step 1: Add `vmas` to `Process`**

`kernel/src/tasks.rs`:

```rust
use crate::memory::vma::{VmaKind, VmaList};

pub struct Process {
    pub pid: u64,
    pub ppid: u64,
    pub cr3: PhysAddr,
    pub heap_brk: VirtAddr,
    pub mmap_base: VirtAddr,
    pub vmas: VmaList,
    // ... existing fields unchanged
}
```

In `Process::new` and `fork_from` add `vmas: VmaList::default(),` / `vmas: self.vmas.clone(),`.

- [ ] **Step 2: Rewrite `sys_mmap`**

Replace `sys_mmap` (syscalls.rs:487) with a VMA-recording version (anonymous only, same allocation policy as today):

```rust
fn sys_mmap(addr: u64, len: usize, prot: u64, flags: u64) -> u64 {
    use crate::memory::vma::{Vma, VmaKind};
    if (flags & MAP_ANONYMOUS) == 0 || len == 0 {
        return EINVAL; // file-backed mappings are out of scope
    }
    let aligned_len = (len as u64 + PAGE_MASK) & !PAGE_MASK;

    let mut cur = CURRENT_THREAD.lock();
    let t = cur.as_mut().unwrap();
    let mut p = t.process.lock();

    let vaddr = if (flags & MAP_FIXED) != 0 {
        if addr & PAGE_MASK != 0 {
            return EINVAL;
        }
        addr
    } else {
        let base = p.mmap_base.0;
        p.mmap_base = VirtAddr(base + aligned_len);
        base
    };

    let vma = Vma {
        start: vaddr,
        end: vaddr + aligned_len,
        prot,
        flags,
        kind: VmaKind::Anonymous,
    };
    if p.vmas.insert(vma).is_err() {
        return ENOMEM;
    }
    let cr3 = p.cr3;
    drop(p);
    drop(cur);

    let phys_offset = HHDM_BASE.0;
    let pml4 = unsafe { &mut *(cr3 + phys_offset).as_mut_ptr::<PageTable>() };
    let mut mapper = Mapper::new_with_offset(pml4, phys_offset);
    let mut pt_flags = PageTableFlags::USER;
    if (prot & 2) != 0 {
        pt_flags |= PageTableFlags::WRITABLE;
    }
    if (prot & 4) == 0 {
        pt_flags |= PageTableFlags::NO_EXECUTE;
    }
    let mut alloc = FRAME_ALLOCATOR.lock();
    for page in (vaddr..vaddr + aligned_len).step_by(PAGE_SIZE as usize) {
        let _ = mapper.map_demand_4k(&mut *alloc, VirtAddr(page), pt_flags);
    }
    vaddr
}
```

Add the `MAP_*` constants import at the top of `syscalls.rs`:
`use crate::memory::vma::{MAP_ANONYMOUS, MAP_FIXED};`

- [ ] **Step 3: Rewrite `sys_munmap`**

```rust
fn sys_munmap(addr: u64, len: usize) -> u64 {
    if len == 0 {
        return 0;
    }
    let start = addr & !PAGE_MASK;
    let end = ((addr + len as u64) + PAGE_MASK) & !PAGE_MASK;

    let removed;
    let cr3;
    {
        let mut cur = CURRENT_THREAD.lock();
        let t = cur.as_mut().unwrap();
        let mut p = t.process.lock();
        removed = p.vmas.remove_range(start, end);
        cr3 = p.cr3;
    }
    if removed.is_empty() {
        return 0;
    }

    let phys_offset = HHDM_BASE.0;
    let mut alloc = FRAME_ALLOCATOR.lock();
    for v in &removed {
        if matches!(v.kind, crate::memory::vma::VmaKind::Device { .. }) {
            continue; // device frames belong to the hardware, never freed
        }
        crate::memory::space::unmap_user_range(v.start, v.end, &mut *alloc, phys_offset);
    }
    0
}
```

- [ ] **Step 4: Rewrite `sys_mprotect`**

```rust
fn sys_mprotect(addr: u64, len: usize, prot: u64) -> u64 {
    let start = addr & !PAGE_MASK;
    let end = ((addr + len as u64) + PAGE_MASK) & !PAGE_MASK;
    if start >= end {
        return 0;
    }

    let mut pt_flags = PageTableFlags::USER;
    if (prot & 2) != 0 {
        pt_flags |= PageTableFlags::WRITABLE;
    }
    if (prot & 4) == 0 {
        pt_flags |= PageTableFlags::NO_EXECUTE;
    }

    let mut cur = CURRENT_THREAD.lock();
    let t = cur.as_mut().unwrap();
    let mut p = t.process.lock();
    let cr3 = p.cr3;
    let mut changed = false;
    for v in p.vmas.vmas.iter_mut() {
        let o_start = core::cmp::max(v.start, start);
        let o_end = core::cmp::min(v.end, end);
        if o_start < o_end {
            v.prot = prot;
            changed = true;
        }
    }
    if !changed {
        return 0;
    }
    drop(p);
    drop(cur);

    let phys_offset = HHDM_BASE.0;
    let pml4 = unsafe { &mut *(cr3 + phys_offset).as_mut_ptr::<PageTable>() };
    let mut mapper = Mapper::new_with_offset(pml4, phys_offset);
    for page in (start..end).step_by(PAGE_SIZE as usize) {
        let _ = mapper.update_prot_4k(VirtAddr(page), pt_flags);
        invlpg(page);
    }
    0
}
```

- [ ] **Step 5: Rewrite `sys_brk`**

Maintain the heap as one `Heap` VMA spanning `0x4000_0000_0000 .. heap_brk`; on grow, map demand pages and extend the VMA; on shrink, `unmap_user_range` and trim:

```rust
fn sys_brk(addr: u64) -> u64 {
    use crate::memory::vma::{Vma, VmaKind};

    let mut cur = CURRENT_THREAD.lock();
    if let Some(t) = cur.as_mut() {
        let mut p = t.process.lock();
        if p.heap_brk.0 == 0 {
            p.heap_brk = VirtAddr(0x4000_0000_0000); // 64TB
            // seed the heap VMA
            let _ = p.vmas.insert(Vma {
                start: p.heap_brk.0,
                end: p.heap_brk.0,
                prot: 3,
                flags: 0,
                kind: VmaKind::Heap,
            });
        }
        if addr == 0 {
            return p.heap_brk.0;
        }

        let current_brk = p.heap_brk.0;
        let start_page = (core::cmp::min(current_brk, addr) + PAGE_MASK) & !PAGE_MASK;
        let end_page = (core::cmp::max(current_brk, addr) + PAGE_MASK) & !PAGE_MASK;

        if addr > current_brk {
            // grow: map demand pages, extend the heap VMA
            let mut alloc = FRAME_ALLOCATOR.lock();
            let pml4_ptr = (p.cr3 + HHDM_BASE.0).as_mut_ptr::<PageTable>();
            let mut mapper = Mapper::new_with_offset(unsafe { &mut *pml4_ptr }, HHDM_BASE.0);
            let flags = PageTableFlags::USER | PageTableFlags::WRITABLE;
            for page in (current_brk..end_page).step_by(PAGE_SIZE as usize) {
                if page < end_page {
                    let _ = mapper.map_demand_4k(&mut *alloc, VirtAddr(page), flags);
                }
            }
            drop(alloc);
        } else if addr < current_brk {
            // shrink: unmap + free the removed top pages, trim the VMA
            let removed = p.vmas.remove_range(start_page, end_page);
            drop(p);
            drop(cur);
            let mut alloc = FRAME_ALLOCATOR.lock();
            for v in &removed {
                crate::memory::space::unmap_user_range(v.start, v.end, &mut *alloc, HHDM_BASE.0);
            }
            drop(alloc);
            let mut cur = CURRENT_THREAD.lock();
            let t = cur.as_mut().unwrap();
            let mut p = t.process.lock();
        }
        p.heap_brk = VirtAddr(addr);
        // keep the heap VMA end in sync with heap_brk
        if let Some(v) = p.vmas.vmas.iter_mut().find(|v| v.kind == VmaKind::Heap) {
            v.end = addr;
        }
        return addr;
    }
    ENOMEM
}
```

> ponytail: `heap_brk`/`mmap_base` remain the source of truth for allocation position (only used as bumps); the VMAs are the accounting record. If the grow/shrink edge cases prove finicky during implementation, simplify to: always grow, never free on shrink (Linux `brk` can legally leave the shrunken pages mapped until exit) — set the VMA end to `addr` and skip the unmap on shrink entirely.

- [ ] **Step 6: Reset VMAs in `execve`**

In `sys_execve` (syscalls.rs ~991), alongside `old_p.heap_brk = VirtAddr(0);` add `old_p.vmas = crate::memory::vma::VmaList::default();` (import `VmaList`).

- [ ] **Step 7: Build + verify**

```bash
./scripts/build-kernel.sh
cargo test -p kernel --lib --target x86_64-unknown-none
```

Expected: build clean, `Tests passed!`. (Boot smoke in A4.)

---

### Task A4: Boot smoke for the VMA-backed mmap path

**Files:**
- Modify: `initrd/` + `scripts/build-initrd.sh` usage (add a tiny test program)
- Test: QEMU boot smoke

- [ ] **Step 1: Add an mmap smoke program `artifacts/mmap_test.asm`**

```asm
; mmap_test.asm — anonymous mmap, write, mprotect, munmap, mmap again.
; Exits 0 if all steps succeed.
global _start
section .text
_start:
    ; mmap(NULL, 0x3000, PROT_READ|PROT_WRITE=3, MAP_PRIVATE|MAP_ANON=0x22, -1, 0)
    mov rax, 9
    xor rdi, rdi
    mov rsi, 0x3000
    mov rdx, 3
    mov r10, 0x22
    mov r8, -1
    xor r9, r9
    syscall
    test rax, rax
    js .fail
    mov r12, rax                 ; base

    ; write a pattern across all 3 pages
    lea rsi, [pattern]
    mov rdi, r12
    mov rdx, 16
    mov rax, 1                   ; write() to the mapping region is wrong;
    ;  actually do a raw store:
    mov rax, 0
.store:
    mov [r12+rax], rax
    add rax, 8
    cmp rax, 0x3000
    jl .store

    ; verify readback
    mov rax, [r12+0x1000]
    cmp rax, 0x1000
    jne .fail

    ; mprotect(base, 0x1000, PROT_READ=1)
    mov rax, 10
    mov rdi, r12
    mov rsi, 0x1000
    mov rdx, 1
    syscall
    test rax, rax
    js .fail

    ; munmap(base, 0x3000)
    mov rax, 11
    mov rdi, r12
    mov rsi, 0x3000
    syscall
    test rax, rax
    js .fail

    ; second anonymous mmap must still work (list integrity after munmap)
    mov rax, 9
    xor rdi, rdi
    mov rsi, 0x1000
    mov rdx, 3
    mov r10, 0x22
    mov r8, -1
    xor r9, r9
    syscall
    test rax, rax
    js .fail

    mov rax, 60
    xor rdi, rdi
    syscall
.fail:
    mov rax, 60
    mov rdi, 1
    syscall
section .data
pattern: db "mmap ok", 10, 0
```

Build and install:

```bash
nasm -f elf64 -o /tmp/mmap_test.o artifacts/mmap_test.asm
ld -o /tmp/mmap_test /tmp/mmap_test.o
cp /tmp/mmap_test initrd/mmap_test
./scripts/build-initrd.sh
```

- [ ] **Step 2: Boot and run it**

Boot via `./scripts/smoke.sh` (or a manual QEMU run). From the shell, `exec mmap_test`.

Expected: process exits with code 0. A panic, SIGSEGV, or exit code 1 means a VMA path is broken.

- [ ] **Step 3: Verify existing behavior unchanged**

From the shell, `exec fork_test` and `exec ls` still behave as before (fork COW, initrd reads).

---

## Part B — MMIO mapping

### Task B1: `MMIO` page-table flag + device-page exclusion in fork/destroy

**Files:**
- Modify: `shared/src/memory/flags.rs`
- Modify: `kernel/src/memory/space.rs`
- Test: `kernel/src/lib.rs` (flag assertion) + boot smoke

Device pages must never be COW-copied at fork nor freed at teardown/unmap. A software flag on the leaf PTE is the marker both paths check.

- [ ] **Step 1: Add the software flag**

`shared/src/memory/flags.rs`, next to `COW`/`DEMAND_PAGING`:

```rust
/// Software flag: leaf maps a fixed physical device range (MMIO). The frame
/// is owned by the hardware — never COW'd, never freed.
pub const MMIO: Self = Self(1 << 12);
```

- [ ] **Step 2: Add a unit test for the flag**

`kernel/src/lib.rs`:

```rust
#[test_case]
fn mmio_flag_bits_are_software_only() {
    use shared::memory::flags::PageTableFlags;
    let f = PageTableFlags::MMIO | PageTableFlags::CACHE_DISABLE;
    assert!(f.contains(PageTableFlags::MMIO));
    assert!(f.contains(PageTableFlags::CACHE_DISABLE));
    assert!(!f.contains(PageTableFlags::PRESENT));
    assert!(!f.contains(PageTableFlags::HUGE_PAGE));
}
```

- [ ] **Step 3: Exclude MMIO leaves in `fork_address_space`**

In `kernel/src/memory/space.rs` `fork_address_space` (the pte loop, line 107):

```rust
for pti in 0..512 {
    let pte = *pt.entry(pti);
    if pte.flags().contains(PageTableFlags::MMIO) {
        // Device pages are shared verbatim; never COW or refcount.
        *child_pt.entry_mut(pti) = pte;
        continue;
    }
    // ... existing present / demand logic unchanged
}
```

- [ ] **Step 4: Exclude MMIO leaves in `free_leaf`**

`kernel/src/memory/space.rs` `free_leaf` (line 137):

```rust
fn free_leaf(entry: PageTableEntry, allocator: &mut impl FrameAllocator, counts: &mut Vec<u32>) {
    if !entry.is_present() || entry.flags().contains(PageTableFlags::MMIO) {
        return;
    }
    // ... existing COW/non-COW logic unchanged
}
```

- [ ] **Step 5: Verify**

```bash
./scripts/build-kernel.sh
cargo test -p kernel --lib --target x86_64-unknown-none
```

Expected: build clean, `Tests passed!`, boot unaffected (no device pages exist yet to hit the new branches).

---

### Task B2: Kernel MMIO mapping facility (`mmio.rs`)

**Files:**
- Create: `kernel/src/memory/mmio.rs`
- Modify: `kernel/src/memory/mod.rs` (add `pub mod mmio;`)
- Test: `kernel/src/lib.rs` (`#[test_case]`)

Provides `map_device_memory(phys, len)` / `unmap_device_memory(virt, len)` mapping a physical range into a reserved kernel-higher-half region with uncached semantics.

- [ ] **Step 1: Write the failing test**

`kernel/src/lib.rs`:

```rust
#[test_case]
fn mmio_map_unmap_roundtrip() {
    use shared::boot::boot_info::HHDM_BASE;
    use shared::memory::address::{PAGE_SIZE, PhysAddr};
    use crate::memory::mmio::{map_device_memory, unmap_device_memory};

    // Borrow a real phys page (RAM), map it UC, poke it, unmap it.
    let mut alloc = crate::memory::FRAME_ALLOCATOR.lock();
    let phys = alloc.alloc_pages(1, PAGE_SIZE).unwrap();
    drop(alloc);

    let virt = map_device_memory(phys, PAGE_SIZE as usize).expect("map_device_memory");
    unsafe {
        *virt.as_mut_ptr::<u64>() = 0x1122_3344_5566_7788;
        assert_eq!(*virt.as_ptr::<u64>(), 0x1122_3344_5566_7788);
    }
    unmap_device_memory(virt, PAGE_SIZE as usize);

    // frame must still be allocator-owned (device map never frees it)
    let mut alloc = crate::memory::FRAME_ALLOCATOR.lock();
    alloc.dealloc_pages(phys, 1);
    drop(alloc);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p kernel --lib --target x86_64-unknown-none`
Expected: compile error, `module memory::mmio` not found.

- [ ] **Step 3: Create `kernel/src/memory/mmio.rs`**

```rust
use core::sync::atomic::{AtomicU64, Ordering};

use shared::{
    boot::boot_info::HHDM_BASE,
    memory::{
        address::{PAGE_SIZE, PhysAddr, VirtAddr},
        allocator::FrameAllocator,
        error::AllocError,
        flags::PageTableFlags,
        mapper::Mapper,
        page::PageTable,
    },
};

use crate::{
    arch::x86_64::cpu::read_cr3,
    memory::{FRAME_ALLOCATOR, space},
};

/// Kernel-only virtual region for device mappings, well above the heap
/// (`0xFFFF_9000_0000_0000`) and below the top of the kernel half.
pub const KERNEL_MMIO_BASE: u64 = 0xFFFF_A000_0000_0000;

/// Bump cursor for the kernel MMIO region. Never freed (drivers map once).
static MMIO_NEXT: AtomicU64 = AtomicU64::new(0);

/// Maps a physical device range (e.g. a PCI BAR) into the kernel MMIO region
/// with uncached semantics. The physical frames are owned by the hardware and
/// are never freed by `unmap_device_memory`.
pub fn map_device_memory(phys: PhysAddr, len: usize) -> Result<VirtAddr, AllocError> {
    let len = (len as u64 + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    if len == 0 {
        return Err(AllocError::InvalidAlignment);
    }
    let virt = KERNEL_MMIO_BASE + MMIO_NEXT.fetch_add(len, Ordering::SeqCst);

    let phys_offset = HHDM_BASE.0;
    let cr3 = read_cr3();
    let pml4 = unsafe { &mut *(cr3 + phys_offset).as_mut_ptr::<PageTable>() };
    let mut mapper = Mapper::new_with_offset(pml4, phys_offset);

    let flags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::NO_EXECUTE
        | PageTableFlags::MMIO
        | PageTableFlags::CACHE_DISABLE; // UC: PAT index 0 with CD=1

    mapper.map_range(
        &mut *FRAME_ALLOCATOR.lock(),
        VirtAddr(virt),
        phys,
        len as usize,
        flags,
    )?;
    Ok(VirtAddr(virt))
}

/// Unmaps a kernel MMIO mapping. Frees no physical frames.
pub fn unmap_device_memory(virt: VirtAddr, len: usize) {
    let len = (len as u64 + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let phys_offset = HHDM_BASE.0;
    let cr3 = read_cr3();
    let pml4 = unsafe { &mut *(cr3 + phys_offset).as_mut_ptr::<PageTable>() };
    let mut mapper = Mapper::new_with_offset(pml4, phys_offset);
    let _ = mapper.unmap_range(virt, len as usize);
}
```

> ponytail: bump-allocated region, no reuse of holes — device mappings are long-lived (drivers map a BAR once at probe). Add a free-list only when a driver maps/unmaps frequently.
>
> `CACHE_DISABLE` (PTE bit 4) gives uncached access with PAT index 0. Write-combining (needed later for framebuffers) requires a PAT MSR setup + the PTE PAT bit — explicitly deferred, PCI register access only needs UC.

- [ ] **Step 4: Register the module**

`kernel/src/memory/mod.rs`: add `pub mod mmio;`.

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p kernel --lib --target x86_64-unknown-none`
Expected: `Tests passed!` (includes `mmio_map_unmap_roundtrip`).

- [ ] **Step 6: Verify**

```bash
./scripts/build-kernel.sh
cargo test -p kernel --lib --target x86_64-unknown-none
./scripts/smoke.sh
```

Expected: build clean, `Tests passed!`, `SMOKE PASS`.

---

### Task B3: Expose device mappings via `mmap` (user-space device window)

**Files:**
- Modify: `kernel/src/arch/x86_64/syscalls.rs` (`sys_mmap`)
- Test: QEMU boot (framebuffer identity check via an asm program) — optional

Now that device pages are first-class (`VmaKind::Device`, `MMIO` flag), extend `mmap` so a fixed physical range can be mapped into user space. This is the path a future user-space driver (and today's framebuffer consumer) needs.

> **Scope note (roadmap `2026-08-08-roadmap.md`):** "user-space drivers / hardware passthrough" is explicitly deferred. This task is included because it's ~20 lines and completes the device-mapping story; it can be skipped without blocking PCI. If you skip it, delete the `VmaKind::Device` variant too.

- [ ] **Step 1: Recognize a device mmap request**

In `sys_mmap`, before the anonymous check, accept a range flag using an unused `MAP_` bit the loader reserves (e.g. `MAP_DEVICE = 0x40`). On device maps, `addr` is treated as the physical address and `mmap_base` is untouched:

```rust
const MAP_DEVICE: u64 = 0x40;

fn sys_mmap(addr: u64, len: usize, prot: u64, flags: u64) -> u64 {
    use crate::memory::vma::{Vma, VmaKind};
    if len == 0 {
        return EINVAL;
    }
    if (flags & MAP_DEVICE) != 0 {
        return mmap_device(addr, len, prot, flags);
    }
    if (flags & MAP_ANONYMOUS) == 0 {
        return EINVAL;
    }
    // ... existing anonymous body unchanged
}

fn mmap_device(phys: u64, len: usize, prot: u64, flags: u64) -> u64 {
    use crate::memory::vma::{Vma, VmaKind};
    let start = phys & !PAGE_MASK;
    let aligned_len = (len as u64 + PAGE_MASK) & !PAGE_MASK;

    let mut cur = CURRENT_THREAD.lock();
    let t = cur.as_mut().unwrap();
    let mut p = t.process.lock();
    let vaddr = p.mmap_base.0;
    p.mmap_base = VirtAddr(vaddr + aligned_len);
    let vma = Vma {
        start: vaddr,
        end: vaddr + aligned_len,
        prot,
        flags,
        kind: VmaKind::Device { phys: start },
    };
    if p.vmas.insert(vma).is_err() {
        return ENOMEM;
    }
    let cr3 = p.cr3;
    drop(p);
    drop(cur);

    let mut pt_flags = PageTableFlags::USER
        | PageTableFlags::MMIO
        | PageTableFlags::CACHE_DISABLE;
    if (prot & 2) != 0 {
        pt_flags |= PageTableFlags::WRITABLE;
    }
    if (prot & 4) == 0 {
        pt_flags |= PageTableFlags::NO_EXECUTE;
    }

    let phys_offset = HHDM_BASE.0;
    let pml4 = unsafe { &mut *(cr3 + phys_offset).as_mut_ptr::<PageTable>() };
    let mut mapper = Mapper::new_with_offset(pml4, phys_offset);
    mapper
        .map_range(
            &mut *FRAME_ALLOCATOR.lock(),
            VirtAddr(vaddr),
            PhysAddr::new(start),
            aligned_len as usize,
            pt_flags,
        )
        .map(|_| vaddr)
        .map_err(|_| ENOMEM)
}
```

- [ ] **Step 2: Add a framebuffer identity smoke test**

Boot QEMU, from the shell run an asm program that `mmap`s the framebuffer phys (from `boot_info.framebuffer.address`, print it to serial first) with `MAP_DEVICE`, writes a pixel, and checks the framebuffer virtual-address readback matches — the bootloader maps the same phys into HHDM as writeback RAM, so both views must agree.

> If the QEMU/asm plumbing is too fiddly, fall back to: `exec mmap_test`, then `exec ls`, `exec fork_test`, and confirm no regression.

- [ ] **Step 3: Verify**

```bash
./scripts/build-kernel.sh
cargo test -p kernel --lib --target x86_64-unknown-none
./scripts/smoke.sh
```

Expected: build clean, `Tests passed!`, `SMOKE PASS`.

---

## Task B4: Full regression + leak check

**Files:** none (verification only)

- [ ] **Step 1: Full suite**

```bash
./scripts/build-kernel.sh && cargo test -p kernel --lib --target x86_64-unknown-none && ./scripts/smoke.sh
```

- [ ] **Step 2: Repeat smoke 3 times**

All must pass.

- [ ] **Step 3: Free-frames stability under mmap churn**

Boot, then from the shell run a loop of `mmap`/`munmap` (an asm variant of `mmap_test.asm` repeating 100×). Check the kernel `meminfo` (or a TEMP serial print of `FRAME_ALLOCATOR.lock().free_frames()`) is stable across the loop — proves `munmap` actually reclaims frames and device pages aren't leaking.

- [ ] **Step 4: Confirm no leftover smell**

`grep -rn "mmap_base.0" kernel/src/` — only the two bump sites in `sys_mmap`/`mmap_device` remain (all other mmap logic is VMA-driven).

---

## Deferred (explicitly out of scope)

- **PAT / write-combining:** only UC is implemented (`CACHE_DISABLE`). WC needs PAT MSR programming + the 4K PTE PAT bit; needed for framebuffer performance, not for PCI register access. Note: `PageTableFlags::HUGE_PAGE` (bit 7) currently collides with the 4K PTE PAT bit — resolve before adding WC.
- **File-backed mmap** (`MAP_SHARED` on a VNode fd, demand-loading from the initrd TAR): needs the VFS to expose page-in callbacks; defer.
- **VMA-driven `destroy_address_space`:** teardown still walks page tables; the `MMIO`-flag skip (B1) makes that walk device-safe. Rewriting teardown over the VMA list is cleanup, not correctness.
- **TLB shootdown / SMP concerns:** single-core; `invlpg` is sufficient. Revisit when the SMP phase lands.
- **`MAP_FIXED` overlap handling:** currently rejects overlaps; Linux unmaps the old range first. Only matters if a program relies on remap semantics — none do today.
