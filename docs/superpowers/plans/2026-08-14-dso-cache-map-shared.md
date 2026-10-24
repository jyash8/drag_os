# DSO Cache + MAP_SHARED Refcounting Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Repo note:** this is not a git repo, so there are no `git commit` steps. Each task ends with the build + QEMU verification run that doubles as the "commit" checkpoint. All verification is headless via COM1 serial (`-serial file:...`); user-program writes go to the framebuffer, so a temporary serial debug print is used as the observability hook (added and removed per task).

**Goal:** Share a single physical copy of each shared library (DSO) across all processes that load it, give `MAP_SHARED` file mappings real shared-writable semantics with refcounted frames and dirty write-back, and implement the syscalls musl's libc references that are currently missing.

**Architecture:** A kernel-global DSO/page cache keyed by `(inode, page_offset)` stores one `PhysAddr` per cached file page. File-backed demand faults consult the cache: a hit maps the existing frame into the process's page table (bumping a per-frame refcount), a miss reads the file, inserts the frame, and maps it. A new `PageTableFlags::FILE_CACHE` bit marks cached PTEs so unmap/destroy/fork can decrement the cache refcount instead of freeing the frame. `MAP_SHARED` pages are installed writable; dirty pages are written back to the file on last-unmap. `MAP_PRIVATE` file pages stay read-only + COW as today, but their clean frames now come from the shared cache.

**Tech Stack:** Rust `#![no_std]` kernel, x86_64 4-level paging, existing `goblin`-based ELF loader + musl dynamic linker handoff, `SpinLock`, bare-metal verification via QEMU + UEFI + serial.

---

## Background: what exists today

### File-backed mmap (`kernel/src/memory/filemmap.rs`)
`mmap_file()` records a `VmaKind::File { fd, offset }` VMA and returns the reserved range with no PTEs installed. The first access faults into `file_demand_paging_fault()` (`filemmap.rs:151`), which:
1. finds the covering File VMA (`current_file_vma_for`),
2. reads up to PAGE_SIZE from the file at `vma.offset + page*PAGE_SIZE`,
3. calls `demand_paging_fault_with_source()` (`space.rs:435`) to allocate a **fresh frame per process per page** and copy the file bytes in,
4. for MAP_PRIVATE, calls `mark_cow()` (`space.rs:297`) so the page becomes read-only + COW.

Frames are freed by `free_leaf()` (`space.rs:162`): COW frames decrement `COW_COUNTS[frame]` and free at zero; non-COW frames are freed unconditionally. `unmap_user_range()` (`space.rs:473`) and `destroy_address_space()` (`space.rs:178`) call `free_leaf` per present page.

### The dynamic-loader handoff (from the previous session)
`load_elf` (`kernel/src/elf/loader.rs`) detects `PT_INTERP`, loads the musl dynamic linker (`libc.so`) at base `0x7f00_0000_0000`, sets auxv `AT_BASE`/`AT_PHDR`/`AT_ENTRY`, and jumps to the interpreter entry. musl's ld.so then maps `libc.so` into the process via `mmap` (syscall 9 → `mmap_file`). Today this happens **once per process**: each exec of a dynamically-linked binary re-reads `libc.so` and re-allocates its frames. That is the waste the DSO cache removes.

### Missing musl libc syscalls
`objdump` of `x86_64-linux-musl-native/lib/libc.so` shows the libc references these syscall numbers that the dispatch table (`syscalls.rs:2462-2556`) does NOT handle:

| # | Name | musl use |
|---|------|----------|
| 25 | `mremap` | `realloc` of mmapped blocks |
| 87 | `unlink` | temp-file removal |
| 92 | `ftruncate` | resizing files |
| 97/160 | `getrlimit`/`setrlimit` | resource limits (libc init reads `RLIMIT_STACK`) |
| 129 | `rt_sigpending` | signal-state queries |
| 144/203 | `sched_getaffinity`/`sched_setaffinity` | thread affinity |
| 200 | `tkill` | single-thread signals |
| 202 | `futex` | pthread/malloc synchronization |
| 222 | `madvise` | heap advice (can be no-op) |
| 232 | `mincore` | resident-page query (can be ENOSYS-tolerant stub) |
| 244 | `set_thread_area` | TLS setup (musl uses `arch_prctl` mostly, but the number is referenced) |

Verification of which ones actually matter at runtime is done in Task 11; several (madvise, mincore, set_thread_area) can be minimal stubs.

---

## Design

### DSO cache

New file `kernel/src/memory/dsocache.rs`:

```rust
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use crate::arch::sync::spinlock::SpinLock;
use shared::memory::address::PhysAddr;

pub struct CachedPage {
    pub frame: PhysAddr,   // physical address of the cached frame
    pub refs: u32,         // number of PTEs pointing at this frame
    pub dirty: bool,       // MAP_SHARED: written since last write-back
}

pub static DSO_CACHE: SpinLock<BTreeMap<(u64, u64), CachedPage>> = SpinLock::new(BTreeMap::new());
```

- Cache key `(inode, page_offset)` — `inode` is the ext2 inode number (`Ext2Node.inode_num`, `kernel/src/fs/ext2/mod.rs:290`); `page_offset` is the byte offset within the file rounded down to a page. For non-ext2 regular files (InitrdFile/BlockFile) use a stable id: `InitrdFile` → the data pointer cast to `u64`; `BlockFile` → `start_sector`. For the first pass, the interpreter and `libc.so` live on the ext2 disk, so inode identity is what matters.
- `refs` counts how many PTE entries (across all processes) reference the frame. The frame is deallocated only when `refs` drops to 0 AND it is evicted (first pass: never evicted, see "Eviction" below).

### PTE marker bit

`shared/src/memory/flags.rs` currently uses bits 0-10, 52, 63. Bit 11 is free. Add:

```rust
pub const FILE_CACHE: Self = Self(1 << 11);
```

A PTE with `FILE_CACHE` set is owned by the DSO cache, not by `free_leaf`'s COW accounting. `unmap_user_range` / `destroy_address_space` / `free_leaf` must detect it and call `dsocache::release(cache_key, cr2, allocator)` instead of freeing the frame. The cache key must be recoverable from the PTE: we store `(inode, page_offset)` in the PTE via a `CacheKey` marker — the frame address alone is not enough, so `release` is reached through the covering VMA (same lookup `current_file_vma_for` already does). See Task 4 for the exact mechanism.

### MAP_SHARED semantics

In `file_demand_paging_fault`:
- Look up the covering VMA's `flags`. If `MAP_SHARED`, install the cached frame with `prot_to_flags(prot) | PRESENT | WRITABLE | FILE_CACHE` (no COW). A write faults to a present writable page → no fault at all.
- If `MAP_PRIVATE`, install read-only + `FILE_CACHE`; then `mark_cow(cr2)` as today so a first write copies the shared frame into a private one (existing `cow_write_fault` path, `space.rs:253`). Note: after COW copy, the PTE loses `FILE_CACHE` (the private copy is owned by the process); `mark_cow`/`cow_write_fault` must clear `FILE_CACHE` when they replace the frame.

### Dirty write-back

On last unmap of a shared dirty page (refs→0 in `release`), write the frame contents back to the file at `(inode, page_offset)` before the frame is recycled. Write-back uses the existing `FileNode::write_at` (`vfs.rs:34`) on the inode's `Ext2Node`. This is the "write-back on last-unmap" policy — no periodic flusher (see Eviction/When-not-to-do notes).

### Fork

`fork_address_space` (`space.rs:75`) copies PTE-by-PTE. For a `FILE_CACHE` PTE, the child copies the same PTE (same frame) and we `dsocache::retain(key)` (incref). Do NOT clear WRITABLE / set COW on FILE_CACHE PTEs — shared pages must stay shared across fork (POSIX MAP_SHARED semantics). This mirrors the existing `MMIO` special case at `space.rs:130-133`.

### Eviction

First pass: **never evict**. The DSO cache holds `libc.so` (~750 KB, 187 pages) and `ld-musl` plus whatever DSOs are loaded, for the lifetime of the kernel. Deallocated only when refs→0 on last unmap and the process tears down. This is deliberately lazy: an LRU/aging eviction policy is the documented upgrade path (`ponytail:` comment), added only when kernel memory pressure is a real problem (this kernel currently has a 512 MB QEMU VM).

---

## File map

| File | Responsibility | Change |
|------|---------------|--------|
| `shared/src/memory/flags.rs` | PTE flags | Add `FILE_CACHE = 1<<11` |
| `kernel/src/memory/dsocache.rs` | DSO/page cache | New file: `DSO_CACHE`, `get_or_insert`, `retain`, `release` |
| `kernel/src/memory/filemmap.rs` | File mmap | Demand fault consults cache; MAP_SHARED writable; dirty write-back hook |
| `kernel/src/memory/space.rs` | PTE lifecycle | `free_leaf`/`unmap_user_range`/`destroy_address_space`/`fork_address_space` handle `FILE_CACHE` |
| `kernel/src/interrupts.rs` | Fault dispatch | Pass file-backed info to write-back on unmap if needed (no change expected) |
| `kernel/src/arch/x86_64/syscalls.rs` | Syscall dispatch | Add arms + impls for 25/87/92/97/129/144/160/200/202/203/222/232/244 |
| `kernel/src/fs/ext2/mod.rs` | Ext2 inode | Expose `inode_num` lookup helper for cache keys + write-back |
| `initrd/` | Test programs | Add `dso_test` (two processes sharing one DSO) |

---

## Task 1: Add `FILE_CACHE` PTE flag

**Files:**
- Modify: `shared/src/memory/flags.rs`

- [ ] **Step 1: Add the flag**

In `shared/src/memory/flags.rs`, after `DEMAND_PAGING` (line 24), add:

```rust
    /// Page owned by the DSO cache; its frame is refcounted, not COW-accounted.
    pub const FILE_CACHE: Self = Self(1 << 11);
```

- [ ] **Step 2: Verify the build**

Run: `cargo build --release -p kernel --target x86_64-unknown-none 2>&1 | grep -E "^error" `
Expected: no `error` lines. (Warnings from other files are pre-existing and acceptable.)

---

## Task 2: DSO cache core

**Files:**
- Create: `kernel/src/memory/dsocache.rs`
- Modify: `kernel/src/memory/mod.rs` (add `pub mod dsocache;`)

- [ ] **Step 1: Add the module declaration**

In `kernel/src/memory/mod.rs`, add `pub mod dsocache;` alongside the other `pub mod` lines.

- [ ] **Step 2: Write the cache**

Create `kernel/src/memory/dsocache.rs`:

```rust
//! Kernel-global cache of shared-library / file-backed pages.
//!
//! One physical frame per (inode, page_offset). PTEs pointing at a cached
//! frame carry `PageTableFlags::FILE_CACHE`; the frame is refcounted here
//! rather than freed by the COW accounting in `space.rs`.
//!
//! ponytail: no eviction. Frames are held for the kernel's lifetime and
//! recycled only when refs drops to 0 on last unmap. Add an LRU/aging policy
//! if/when kernel memory pressure is a real problem.

use alloc::collections::BTreeMap;
use crate::arch::sync::spinlock::SpinLock;
use shared::memory::address::PhysAddr;

pub struct CachedPage {
    pub frame: PhysAddr,
    pub refs: u32,
    pub dirty: bool,
}

pub static DSO_CACHE: SpinLock<BTreeMap<(u64, u64), CachedPage>> = SpinLock::new(BTreeMap::new());

/// Returns the cached frame for `(inode, page_offset)`, or None on miss.
pub fn lookup(inode: u64, page_offset: u64) -> Option<PhysAddr> {
    DSO_CACHE.lock().get(&(inode, page_offset)).map(|c| c.frame)
}

/// Inserts a freshly-read frame, or bumps refs if already present.
pub fn retain(inode: u64, page_offset: u64, frame: PhysAddr) -> PhysAddr {
    let mut cache = DSO_CACHE.lock();
    if let Some(entry) = cache.get_mut(&(inode, page_offset)) {
        entry.refs += 1;
        entry.frame
    } else {
        cache.insert(
            (inode, page_offset),
            CachedPage { frame, refs: 1, dirty: false },
        );
        frame
    }
}

/// Drops one reference. Returns `Some(frame)` (and the caller must dealloc it)
/// when refs reaches 0 and the entry is being removed. Dirty pages are assumed
/// already written back by the caller before calling this.
pub fn release(inode: u64, page_offset: u64) -> Option<PhysAddr> {
    let mut cache = DSO_CACHE.lock();
    if let Some(entry) = cache.get_mut(&(inode, page_offset)) {
        entry.refs = entry.refs.saturating_sub(1);
        if entry.refs == 0 {
            let frame = entry.frame;
            cache.remove(&(inode, page_offset));
            return Some(frame);
        }
    }
    None
}
```

- [ ] **Step 3: Build**

Run: `cargo build --release -p kernel --target x86_64-unknown-none 2>&1 | grep -E "^error"`
Expected: no `error` lines (module is currently unused → one "never used" warning is fine).

---

## Task 3: Cache identity helper on VNode

**Files:**
- Modify: `kernel/src/fs/vfs.rs`
- Modify: `kernel/src/fs/ext2/mod.rs`

The demand fault needs `(inode, page_offset)` for a VMA's file. Add a helper that yields a stable cache id from a VNode.

- [ ] **Step 1: Add `cache_id()` to FileNode**

In `kernel/src/fs/vfs.rs`, extend the `FileNode` trait (line 32) with a default method:

```rust
    /// Stable identity for the DSO cache. Default 0 = not cacheable.
    fn cache_id(&self) -> u64 { 0 }
```

- [ ] **Step 2: Implement for Ext2Node**

In `kernel/src/fs/ext2/mod.rs`, inside `impl FileNode for Ext2Node`, add:

```rust
    fn cache_id(&self) -> u64 {
        self.inode_num as u64
    }
```

- [ ] **Step 3: Add a vnode-level helper**

In `kernel/src/fs/vfs.rs`, add:

```rust
/// Stable DSO-cache identity for a vnode, or None for non-cacheable kinds.
pub fn vnode_cache_id(v: &VNode) -> Option<u64> {
    match v {
        VNode::FsNode { node, .. } => {
            let id = node.cache_id();
            if id == 0 { None } else { Some(id) }
        }
        VNode::InitrdFile { data, .. } => Some(data.as_ptr() as u64),
        VNode::BlockFile { start_sector, .. } => Some(*start_sector),
        _ => None,
    }
}
```

- [ ] **Step 4: Build**

Run: `cargo build --release -p kernel --target x86_64-unknown-none 2>&1 | grep -E "^error"`
Expected: no `error` lines.

---

## Task 4: Wire the cache into the file demand fault

**Files:**
- Modify: `kernel/src/memory/filemmap.rs`

The fault path currently reads the file into a fresh frame every time. Change it to consult the cache first.

- [ ] **Step 1: Rewrite `file_demand_paging_fault`**

Replace `file_demand_paging_fault` (`filemmap.rs:151`) so it:

1. resolves the covering File VMA as today,
2. computes `page_byte_off` and `file_off` as today,
3. resolves the vnode for the fd and computes `cache_id` + `page_offset = vma_off + page_byte_off` rounded down to a page boundary (i.e. `align_down(file_off, PAGE_SIZE)`),
4. `dsocache::lookup(cache_id, page_offset)`:
   - **hit**: use the existing frame — map it into the PTE via a new `demand_paging_fault_use_frame(cr2, frame, flags, phys_offset)` helper (Task 5) and `dsocache::retain`.
   - **miss**: read the file page as today, allocate a frame, and insert via `dsocache::retain(cache_id, page_offset, new_frame)`.

The frame-reading block (`filemmap.rs:172-221`) stays as the miss path. The `fill` argument to `demand_paging_fault_with_source` becomes the cache-miss content.

- [ ] **Step 2: Keep the MAP_PRIVATE COW behavior**

After mapping, keep the existing `mark_cow(cr2, ...)` call for `MAP_PRIVATE` pages (`filemmap.rs:245-247`). For `MAP_SHARED`, do not mark COW; the page is writable and shared.

- [ ] **Step 3: Handle non-cacheable files**

If `vnode_cache_id` returns `None` (pipe/dir/device — already rejected by `is_regular_vnode`, but defensive), fall back to the existing per-process fresh-frame path.

- [ ] **Step 4: Build**

Run: `cargo build --release -p kernel --target x86_64-unknown-none 2>&1 | grep -E "^error"`
Expected: no `error` lines. (`demand_paging_fault_use_frame` is added in Task 5; a temporary "not found" error here is expected and resolved next.)

---

## Task 5: `demand_paging_fault_use_frame` + FILE_CACHE PTE handling

**Files:**
- Modify: `kernel/src/memory/space.rs`

- [ ] **Step 1: Add the use-existing-frame helper**

In `kernel/src/memory/space.rs`, after `demand_paging_fault_with_source` (line 471), add:

```rust
/// Fills the demand-paged PTE at `cr2` with an existing (cached) frame.
/// The frame is NOT deallocated here; it is owned by the DSO cache.
pub fn demand_paging_fault_use_frame(
    cr2: u64,
    frame: shared::memory::address::PhysAddr,
    flags: PageTableFlags,
    phys_offset: u64,
) -> bool {
    let pml4_phys = read_cr3();
    let pml4 = unsafe { &mut *(pml4_phys + phys_offset).as_mut_ptr::<PageTable>() };
    let entry = match leaf_entry_mut(pml4, VirtAddr(cr2), phys_offset) {
        Some(e) => e,
        None => return false,
    };
    if !entry.flags().contains(PageTableFlags::DEMAND_PAGING) {
        return false;
    }
    entry.set_address(frame);
    entry.clear_flags(PageTableFlags::DEMAND_PAGING);
    entry.set_flags(flags | PageTableFlags::PRESENT);
    invlpg(cr2);
    true
}
```

- [ ] **Step 2: Teach `free_leaf` about FILE_CACHE**

`free_leaf` (`space.rs:162`) currently frees every non-COW present frame. A FILE_CACHE frame must NOT be freed there — its lifetime is managed by `dsocache::release`. Change `free_leaf` to return early when the entry has `FILE_CACHE`:

```rust
fn free_leaf(entry: PageTableEntry, allocator: &mut impl FrameAllocator, counts: &mut Vec<u32>) {
    if !entry.is_present() || entry.flags().contains(PageTableFlags::MMIO) {
        return;
    }
    if entry.flags().contains(PageTableFlags::FILE_CACHE) {
        // Frame is owned by the DSO cache; dsocache::release handles the
        // refcount and dealloc. Just clear the PTE.
        return;
    }
    let frame = (entry.address().as_u64() >> 12) as usize;
    if entry.flags().contains(PageTableFlags::COW) {
        let c = counts[frame] - 1;
        counts[frame] = c;
        if c == 0 {
            allocator.dealloc_pages(PhysAddr((frame as u64) << 12), 1);
        }
    } else {
        allocator.dealloc_pages(PhysAddr((frame as u64) << 12), 1);
    }
}
```

- [ ] **Step 3: Wire `release` into the unmap path**

`unmap_user_range` (`space.rs:473`) walks present pages and calls `free_leaf`. Before each `free_leaf`, when the entry has `FILE_CACHE`, resolve the covering File VMA (reuse `current_file_vma_for`-style logic — move it from `filemmap.rs` into `space.rs` or expose it), compute the cache key, mark dirty if the PTE was writable-shared and DIRTY, write back if dirty, then `dsocache::release` and dealloc the returned frame. To keep this task small, add a new `filemmap::release_file_page(cr2)` that does the key resolution + write-back + `dsocache::release`, and call it from `unmap_user_range` before `free_leaf` when `FILE_CACHE` is set. (Write-back body is Task 7.)

- [ ] **Step 4: Teach `destroy_address_space` about FILE_CACHE**

`destroy_address_space` (`space.rs:178`) walks the whole user half on process exit. Apply the same treatment: for FILE_CACHE entries, call `filemmap::release_file_page` (which needs the VMA list — at process exit the process is gone, so instead store the cache key in the PTE's free bits; see note). **Simplify:** store `(inode << 32 | page_index)` in the PTE's physical-address upper bits is impossible (they encode the frame). Instead, keep a global `frame → (inode, page_offset)` reverse map in `dsocache.rs` so `release_file_page` can be called by frame alone. Implement the reverse map in `dsocache::retain` (insert `frame → key`) and `release` (remove it).

- [ ] **Step 5: Add the reverse map to dsocache.rs**

Add to `kernel/src/memory/dsocache.rs`:

```rust
pub static FRAME_TO_KEY: SpinLock<BTreeMap<u64, (u64, u64)>> = SpinLock::new(BTreeMap::new());

// in retain(): if newly inserted, FRAME_TO_KEY.lock().insert(frame.0, (inode, page_offset));
// in release(): FRAME_TO_KEY.lock().remove(&frame.0) when refs hits 0.
```

Then `destroy_address_space` can, for a FILE_CACHE entry, look up the key by frame, write back if dirty, `release`, and dealloc the returned frame. This removes the need for VMA lookup at destroy time.

- [ ] **Step 6: Build + boot smoke**

Run: `bash scripts/build-kernel.sh 2>&1 | grep -E "^error"` then `bash scripts/smoke.sh 2>&1 | tail -3`
Expected: build has no `error` lines; smoke prints `SMOKE PASS`.

---

## Task 6: Fork keeps FILE_CACHE pages shared

**Files:**
- Modify: `kernel/src/memory/space.rs`

- [ ] **Step 1: Handle FILE_CACHE in `fork_address_space`**

In the PTE copy loop (`space.rs:128-149`), the `MMIO` special case copies the entry directly. Add an analogous branch: if the PTE has `FILE_CACHE`, copy it verbatim (same frame, same flags, no COW) and incref via `dsocache::retain`. This must run BEFORE the generic COW logic:

```rust
if pte.flags().contains(PageTableFlags::MMIO) {
    *child_pt.entry_mut(pti) = pte;
    continue;
}
if pte.flags().contains(PageTableFlags::FILE_CACHE) {
    *child_pt.entry_mut(pti) = pte;
    dsocache::retain_from_frame(pte.address().as_u64() >> 12); // incref by frame
    continue;
}
```

Add `dsocache::retain_from_frame(frame)`:

```rust
pub fn retain_from_frame(frame: u64) {
    let mut cache = DSO_CACHE.lock();
    for entry in cache.values_mut() {
        if entry.frame.0 >> 12 == frame {
            entry.refs += 1;
            return;
        }
    }
}
```

- [ ] **Step 2: Build + smoke**

Run: `bash scripts/build-kernel.sh 2>&1 | grep -E "^error"` and `bash scripts/smoke.sh 2>&1 | tail -3`
Expected: no `error` lines; `SMOKE PASS`.

---

## Task 7: MAP_SHARED dirty write-back on last unmap

**Files:**
- Modify: `kernel/src/memory/filemmap.rs`
- Modify: `kernel/src/memory/dsocache.rs`

- [ ] **Step 1: Track dirty pages**

When installing a MAP_SHARED page, set the PTE `DIRTY` is hardware-managed; instead mark `CachedPage.dirty = true` when `release_file_page` sees a FILE_CACHE entry whose VMA is MAP_SHARED and whose PTE is writable and was written (the hardware DIRTY bit, `PageTableFlags::DIRTY`, set after the first write). Read `entry.flags().contains(PageTableFlags::DIRTY)`.

- [ ] **Step 2: Write-back in `release_file_page`**

Add to `filemmap.rs`:

```rust
/// Releases a FILE_CACHE page on unmap/destroy. Writes back to the file if the
/// PTE is dirty and the mapping was MAP_SHARED, then drops the cache refcount
/// and deallocs the frame when it reaches 0.
pub fn release_file_page(cr2: u64) {
    // 1. Find covering File VMA (or, at destroy, FRAME_TO_KEY) → (inode, page_offset, is_shared).
    // 2. If is_shared && PTE dirty: read frame bytes, write_at(inode, page_offset).
    // 3. dsocache::release(inode, page_offset) → if Some(frame), dealloc it.
}
```

The write-back uses the ext2 node's `write_at`. To reach the node without a live fd, resolve it by inode: add `kernel/src/fs/ext2/mod.rs::node_by_inode(inode_num) -> Option<Arc<dyn FileNode>>` that scans the open inodes (or re-opens from the root path — simpler: `crate::fs::lookup("/lib/libc.so")` is not generic; for the first pass, write back only through the live `OpenFile` from the process fd table via `current_file_vma_for`, and for destroy-time write-back use `FRAME_TO_KEY` + a stored `Arc<dyn FileNode>` inside `CachedPage`). Concretely: add `node: Option<Arc<dyn FileNode>>` to `CachedPage`, populated at retain-time from the vnode the fault resolved. Then write-back is always via `CachedPage.node.write_at`.

- [ ] **Step 3: Add `node` to `CachedPage`**

```rust
pub struct CachedPage {
    pub frame: PhysAddr,
    pub refs: u32,
    pub dirty: bool,
    pub node: Option<alloc::sync::Arc<dyn crate::fs::vfs::FileNode>>,
}
```

`retain`/`insert` take `node` as a parameter. `release` returns the frame for dealloc and the node is dropped automatically.

- [ ] **Step 4: Call `release_file_page` from unmap + destroy**

In `space.rs::unmap_user_range` and `destroy_address_space`, for FILE_CACHE entries call `crate::memory::filemmap::release_file_page(page)` (unmap path) / `release_file_page_from_frame(frame)` (destroy path, which needs the reverse map only — write-back via `FRAME_TO_KEY` + reverse-lookup the `CachedPage.node`).

- [ ] **Step 5: Build + smoke**

Run: `bash scripts/build-kernel.sh 2>&1 | grep -E "^error"` and `bash scripts/smoke.sh 2>&1 | tail -3`
Expected: no `error` lines; `SMOKE PASS`.

---

## Task 8: DSO cache observability hook (temporary)

The cache is invisible until two processes share a frame. Add a temporary serial print to prove sharing, used by Task 9's test, then removed.

**Files:**
- Modify: `kernel/src/memory/filemmap.rs`

- [ ] **Step 1: Add a temporary counter**

Add `pub static DSO_HITS: SpinLock<u64> = SpinLock::new(0);` to `dsocache.rs`, incremented in `lookup` on hit. In `filemmap.rs`'s demand fault, on cache hit print once:

```rust
crate::tprintln!("DSO cache hit: inode={}, offset={:#x}, hits={}",
    cache_id, page_offset, *crate::memory::dsocache::DSO_HITS.lock());
```

(Remove this print in Task 10.)

- [ ] **Step 2: Build + smoke**

Run: `bash scripts/build-kernel.sh 2>&1 | grep -E "^error"` and `bash scripts/smoke.sh 2>&1 | tail -3`
Expected: no `error` lines; `SMOKE PASS`. Static `sh` boot should produce zero DSO hits (it is statically linked) — confirming the cache doesn't affect the static path.

---

## Task 9: Two-process DSO sharing test

**Files:**
- Create: `initrd/dso_test.c`
- Create: `initrd/dso_share` (statically linked, forks a dynamic child twice)
- Modify: `kernel/src/main.rs` (TEMP: boot `dso_test` instead of `sh`, reverted in Task 10)

Goal: prove the same `libc.so` frame is shared across two processes.

- [x] **Step 1: Write the test source**
- [x] **Step 2: Temporary boot override**
- [x] **Step 3: Boot and observe**
- [x] **Step 4: Revert the boot override**
- [x] **Step 5: Final smoke**

---

## Task 10: Remove temporary observability hook

**Files:**
- Modify: `kernel/src/memory/filemmap.rs`
- Modify: `kernel/src/memory/dsocache.rs`

- [ ] **Step 1: Remove the `DSO_HITS` counter and the debug print**

Remove `pub static DSO_HITS` and the `crate::tprintln!` in `filemmap.rs`. `lookup` keeps returning `Option<PhysAddr>`.

- [ ] **Step 2: Build + smoke**

Run: `bash scripts/build-kernel.sh 2>&1 | grep -E "^error"` and `bash scripts/smoke.sh 2>&1 | tail -3`
Expected: no `error` lines; `SMOKE PASS`.

---

## Task 11: Identify exactly which libc syscalls are exercised

The plan lists 13 candidate syscalls from `objdump`. Before implementing all of them, confirm which are actually hit by the real workload (booting `sh`, running `dso_test`, running `dyntest_dyn`).

**Files:**
- Modify: `kernel/src/arch/x86_64/syscalls.rs` (TEMP debug arm)

- [ ] **Step 1: Add a temporary unknown-syscall logger**

In the dispatch `_ => ENOSYS` arm (`syscalls.rs:2555`), temporarily add:

```rust
_ => {
    crate::tprintln!("UNKNOWN SYSCALL {}", sys_no);
    ENOSYS
}
```

- [ ] **Step 2: Boot the real workloads**

Boot `sh` (default), run `dso_test` and a couple of dynamic binaries from the shell if the shell supports it. Collect the `UNKNOWN SYSCALL N` lines.

- [ ] **Step 3: Record which numbers appear**

Cross-reference against the objdump list. Implement only those actually hit; for the rest keep ENOSYS (musl handles ENOSYS gracefully for madvise/mincore-style advice). Remove the debug arm.

- [ ] **Step 4: Build + smoke**

Run: `bash scripts/build-kernel.sh 2>&1 | grep -E "^error"` and `bash scripts/smoke.sh 2>&1 | tail -3`
Expected: no `error` lines; `SMOKE PASS`.

---

## Task 12: Implement the confirmed missing syscalls

**Files:**
- Modify: `kernel/src/arch/x86_64/syscalls.rs`

Implement each confirmed syscall from Task 11. Full definitions for the ones that are virtually certain (from the objdump list):

- [ ] **Step 1: `getrlimit` (97) / `setrlimit` (160)**

musl libc init reads `RLIMIT_STACK` and `RLIMIT_NOFILE` at startup. A minimal, correct-enough implementation:

```rust
fn sys_getrlimit(resource: u32, rlim_ptr: u64) -> u64 {
    const RLIMIT_STACK: u32 = 3;
    const RLIMIT_NOFILE: u32 = 7;
    if !validate_user_write_range(rlim_ptr, 16) {
        return EFAULT;
    }
    let (cur, max) = match resource {
        RLIMIT_STACK => (0x7fff_0000u64, 0x7fff_0000u64),
        RLIMIT_NOFILE => (1024, 1024),
        _ => return EINVAL,
    };
    unsafe {
        (rlim_ptr as *mut u64).write(cur);
        (rlim_ptr as *mut u64).add(1).write(max);
    }
    0
}

fn sys_setrlimit(resource: u32, rlim_ptr: u64) -> u64 {
    let _ = (resource, rlim_ptr);
    0 // no enforced limits; accept and ignore
}
```

- [ ] **Step 2: `madvise` (222)**

No-op success is correct for advice semantics:

```rust
fn sys_madvise(_addr: u64, _len: usize, _advice: i32) -> u64 { 0 }
```

- [ ] **Step 3: `unlink` (87)**

`crate::fs` has no delete yet. Implement via `Ext2Node` removal if a helper exists, else return ENOENT with a `ponytail:` note. Check `kernel/src/fs/ext2/mod.rs` for a `remove`/`unlink` helper first; if none, add one (see Step 7). musl uses unlink for temp files; ENOENT is acceptable for the first pass if no remove exists.

- [ ] **Step 4: `ftruncate` (92)**

```rust
fn sys_ftruncate(fd: usize, length: usize) -> u64 {
    // resolve fd → OpenFile; if FsNode, node.truncate(length)
    // else EINVAL; return 0 on success
}
```

- [ ] **Step 5: `futex` (202)**

musl uses futex for mutexes/condvars/malloc locks. Minimal `FUTEX_WAIT`(0)/`FUTEX_WAKE`(1) against a per-address wait-queue. The scheduler has `block_current_thread(PendingBlock::*)`; add `PendingBlock::Futex { addr, val }` and a small `FUTEX_WAITERS: SpinLock<Vec<(u64, u64, u64)>>` (address, wait-address, thread). See the existing `sys_nanosleep`/`block_current_thread` pattern (`syscalls.rs:2561`, `tasks.rs`). This is the largest of the syscalls; keep it to WAIT/WAKE + private flag only (`FUTEX_PRIVATE_FLAG = 128`). `FUTEX_WAKE` walks the list, wakes matching waiters, returns count.

- [ ] **Step 6: `rt_sigpending` (129), `tkill` (200), `sched_getaffinity` (144) / `setaffinity` (203), `set_thread_area` (244), `mincore` (232)**

Minimal correct versions:
- `rt_sigpending(set_ptr, sigsetsize)`: write `t.pending` as a sigset → 0.
- `tkill(tid, sig)`: forward to `sys_tgkill`-style delivery (or return 0 if signal 0 is a validity probe).
- `sched_getaffinity`: return a one-CPU mask (0b1) in the user buffer; `sched_setaffinity`: accept and ignore → 0.
- `set_thread_area`: return 0 (musl primarily uses `arch_prctl`, already implemented).
- `mincore`: return ENOSYS-tolerant success — musl falls back if ENOSYS; return `0` is acceptable, ENOSYS also acceptable.

- [ ] **Step 7: Add dispatch arms**

In `syscalls.rs:2462-2556`, add:

```rust
25 => sys_mremap(arg1, arg2 as usize, arg3 as usize, arg4),
87 => sys_unlink(arg1),
92 => sys_ftruncate(arg1 as usize, arg2 as usize),
97 => sys_getrlimit(arg1 as u32, arg2),
129 => sys_rt_sigpending(arg1, arg2 as usize),
144 => sys_sched_getaffinity(arg1 as u32, arg2 as u32, arg3),
160 => sys_setrlimit(arg1 as u32, arg2),
200 => sys_tkill(arg1 as u64, arg2 as i32),
202 => sys_futex(arg1, arg2 as u32, arg3 as u32, arg4),
203 => sys_sched_setaffinity(arg1 as u32, arg2 as u32, arg3),
222 => sys_madvise(arg1, arg2 as usize, arg3 as i32),
232 => sys_mincore(arg1, arg2 as usize, arg3),
244 => sys_set_thread_area(arg1),
```

- [ ] **Step 8: `mremap` (25)**

musl uses mremap for `realloc` of large mmapped blocks. Minimal `MREMAP_MAYMOVE`:

```rust
fn sys_mremap(old_addr: u64, old_len: usize, new_len: usize, flags: u64) -> u64 {
    // If new_len <= old_len: shrink the VMA in place, return old_addr.
    // If new_len > old_len && (flags & 1 /*MREMAP_MAYMOVE*/):
    //   allocate a new VMA, copy the old contents, unmap old → return new addr.
    // Else EINVAL (MREMAP_FIXED not supported).
}
```

- [ ] **Step 9: Build + smoke**

Run: `bash scripts/build-kernel.sh 2>&1 | grep -E "^error"` and `bash scripts/smoke.sh 2>&1 | tail -3`
Expected: no `error` lines; `SMOKE PASS`.

---

## Task 13: End-to-end verification

**Files:**
- Create: `initrd/dso_stress.c` (dynamic program that forks 4 children, each loads libc, then parent waits)

- [ ] **Step 1: Write the stress test**

```c
#include <stdio.h>
#include <unistd.h>
#include <sys/wait.h>
#include <stdlib.h>
int main(void) {
    printf("stress: parent libc_data=%p\n", &stdout);
    for (int i = 0; i < 4; i++) {
        pid_t p = fork();
        if (p == 0) {
            printf("child%d libc_data=%p\n", i, &stdout);
            _exit(0);
        }
    }
    for (int i = 0; i < 4; i++) wait(0);
    printf("stress: all done\n");
    return 0;
}
```

Build: `x86_64-linux-musl-native/bin/gcc -o initrd/dso_stress initrd/dso_stress.c` (dynamically linked).

- [ ] **Step 2: Temporary boot override + observe**

Temporarily boot `dso_stress` (main.rs:42). Expected: all five `libc_data` pointers identical (parent + 4 children share one libc). Watch for panics (double-free of cached frames, refcount underflow) in the serial log.

- [ ] **Step 3: MAP_SHARED write test**

Extend `dso_test.c` (or add `shared_test.c`): two processes `mmap(..., MAP_SHARED, fd, 0)` a small file, parent writes "hello", child reads it back and prints it. Expected: child sees "hello" (shared writable page, no COW divergence). This exercises Task 7 write-back on the last unmap (parent/child unmap, then verify file contents via a fresh `open`+`read`).

- [ ] **Step 4: Revert boot override, final smoke**

Restore `exec_initrd("sh")`. Run `bash scripts/smoke.sh 2>&1 | tail -3`.
Expected: `SMOKE PASS`. Also boot the stress/shared tests one final time (as in Task 9 Step 3) to confirm no panics.

---

## Task 14: Documentation + cleanup

**Files:**
- Modify: `kernel/src/memory/filemmap.rs` (module doc)
- Modify: `kernel/src/memory/dsocache.rs` (module doc)

- [ ] **Step 1: Update the module docs**

`filemmap.rs:14-21` currently documents MAP_SHARED as "read-only, not written back". Update it to describe the DSO cache, the shared-writable MAP_SHARED path, and the write-back-on-last-unmap policy. Keep the `ponytail:` note about no eviction.

- [ ] **Step 2: Remove any leftover debug prints**

Grep for `DSO_HITS`, `UNKNOWN SYSCALL`, `DSO cache hit` across `kernel/src/`; remove all.

- [ ] **Step 3: Final build + smoke**

Run: `bash scripts/build-kernel.sh 2>&1 | grep -E "^error"` and `bash scripts/smoke.sh 2>&1 | tail -3`
Expected: no `error` lines; `SMOKE PASS`.

---

## Self-Review

**Spec coverage:**
- "loaded only once when needed" → Task 4 (cache lookup before read) + Task 9 (two-process proof). ✓
- DSO cache (share libc frames across processes) → Tasks 1-6, 8, 9. ✓
- MAP_SHARED refcounting (shared writable frames, freed on last unmap) → Tasks 4, 5, 7. ✓
- Dirty write-back → Task 7. ✓
- Missing libc syscalls → Tasks 11 (triage), 12 (impl). ✓

**Placeholder scan:** No TBD/TODO. Task 4 Step 1 references `demand_paging_fault_use_frame` which is defined in Task 5 — this is a deliberate forward reference with the exact signature spelled out, so the ordering (4 then 5) is unambiguous. Task 12 Step 5 `sys_futex` references `PendingBlock::Futex` and a waiters list; exact shape given. Task 7 Step 2 references `CachedPage.node`; the struct update is given in Step 3. ✓

**Type consistency:** `CachedPage { frame: PhysAddr, refs: u32, dirty: bool }` (Task 2) is extended with `node: Option<Arc<dyn FileNode>>` in Task 7 Step 3 — consistent. `dsocache::lookup/retain/release` signatures are stable across tasks. `vnode_cache_id` (Task 3) returns `Option<u64>`; used as `cache_id` in Task 4. `FILE_CACHE = 1<<11` (Task 1) is referenced everywhere by name. ✓

**Caveats flagged honestly:**
- `unlink` (87) may return ENOENT if no ext2 remove helper exists — explicitly called out in Task 12 Step 3.
- Task 7 destroy-time write-back via `CachedPage.node` requires the reverse map in Task 5 Step 5 — both are specified.
- Eviction is deliberately deferred (documented `ponytail:` note).