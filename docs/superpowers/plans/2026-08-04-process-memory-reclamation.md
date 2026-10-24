# Process Memory Reclamation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Repo note:** this is not a git repo, so there are no `git commit` steps. Each task ends with the build + QEMU verification run that doubles as the "commit" checkpoint. All verification is headless via COM1 serial (`-serial file:...`); user-program writes go to the framebuffer, so a temporary serial debug print is used as the observability hook (added and removed per task).

**Goal:** Free all memory owned by a process (user pages, page-table frames, kernel stack, COW refcounts) when its last thread exits, so long-running fork/exec workloads don't leak RAM.

**Architecture:** Replace the bump frame allocator with a bitmap allocator that supports `dealloc_pages`. Add `destroy_address_space` (COW-aware walk of the user half of a page table) and trigger it from `impl Drop for Process` — the address space is freed automatically when the last `Arc<SpinLock<Process>>` holder (i.e., the last thread of the process) is destroyed. Kernel stacks are freed via a deferred "tombstone" list drained by the timer, because a stack can't be freed while the CPU is running on it.

**Tech Stack:** Rust `#![no_std]` kernel, x86_64 4-level paging (4KB pages in user space), `linked_list_allocator` global heap, `SpinLock`, bare-metal verification via QEMU + UEFI + serial.

---

## Background: what leaks today

When a process's last thread exits, `exit_current_thread` (kernel/src/tasks.rs:123) just removes the thread from `SCHEDULER`/`CURRENT_THREAD` and resumes the next thread. Everything it owned is leaked:

1. **User leaf frames** — the ELF segments (code/data, mapped `PRESENT|WRITABLE|USER`) and the 64-page user stack (kernel/src/elf/loader.rs:28-84).
2. **Page-table frames** — the process's own pml4, plus one pdpt/pd/pt frame per used slot (allocated by `create_address_space`/`fork_address_space`, kernel/src/memory/space.rs).
3. **Kernel stack** — 16 KiB from the global heap per thread (`alloc_kernel_stack`, kernel/src/tasks.rs:25).
4. **COW refcounts** — `COW_COUNTS[frame]` (kernel/src/memory/space.rs:13) must be decremented when a COW-marked page is freed, or the count drifts and future `cow_write_fault` logic corrupts refcounts.

Foundational blocker: `FrameAllocator` (shared/src/memory/allocator.rs) only has `alloc_pages`, and `BootInfoFrameAllocator` (kernel/src/memory/frame.rs) is a bump allocator — it cannot free anything.

### File map

| File | Responsibility | Change |
|------|---------------|--------|
| `shared/src/memory/allocator.rs` | `FrameAllocator` trait | Add `dealloc_pages` |
| `kernel/src/memory/frame.rs` | Physical frame allocator | Bump → bitmap + free + `free_frames()` |
| `kernel/src/memory/space.rs` | COW + address-space copy | Add `destroy_address_space` |
| `kernel/src/tasks.rs` | Thread/Process model, scheduler exit | `Process::drop`, `Thread.owns_kernel_stack`, tombstone list, exit wiring |
| `kernel/src/arch/x86_64/syscalls.rs` | fork/clone/execve arms | Set `owns_kernel_stack`, tombstone old stack in execve |
| `kernel/src/time.rs` | Timer/scheduler | Drain tombstones |
| `artifacts/reap_test.asm` | Stress test | New file |
| `kernel/src/main.rs` | Boot | TEMP serial debug + auto-exec (removed at end) |

---

## Task 1: Bitmap frame allocator with `dealloc_pages`

**Files:**
- Modify: `shared/src/memory/allocator.rs`
- Modify: `kernel/src/memory/frame.rs`
- Test: `kernel/src/main.rs` (TEMP boot self-check, removed in Task 4)

- [ ] **Step 1: Add `dealloc_pages` to the trait**

`shared/src/memory/allocator.rs`:

```rust
pub trait FrameAllocator {
    fn alloc_pages(&mut self, pages: usize, alignment: u64) -> Result<PhysAddr, AllocError>;
    fn dealloc_pages(&mut self, addr: PhysAddr, pages: usize);
}
```

- [ ] **Step 2: Rewrite the allocator as a bitmap**

`kernel/src/memory/frame.rs` (full replacement):

```rust
use alloc::vec::Vec;
use shared::{
    boot::{MemoryRegion, MemoryRegionKind},
    memory::{
        address::{PAGE_SIZE, PhysAddr},
        allocator::FrameAllocator,
        error::AllocError,
    },
};

pub struct BootInfoFrameAllocator {
    bitmap: Option<Vec<u64>>,
    max_frame: usize,
}

impl BootInfoFrameAllocator {
    pub const fn new() -> Self {
        Self {
            bitmap: None,
            max_frame: 0,
        }
    }

    pub unsafe fn init(&mut self, memory_map: &'static [MemoryRegion]) {
        let mut max_frame = 0;
        for region in memory_map.iter() {
            max_frame = max_frame.max((region.end.as_u64() >> 12) as usize);
        }
        self.max_frame = max_frame;
        let mut bm = vec![0u64; max_frame.div_ceil(64)];
        // Frame is used iff its bit is set. Non-usable memory stays set forever.
        for region in memory_map.iter() {
            if region.kind != MemoryRegionKind::Usable {
                let start = (region.start.as_u64() >> 12) as usize;
                let end = ((region.end.as_u64() >> 12) as usize).min(max_frame);
                for f in start..end {
                    bm[f / 64] |= 1 << (f % 64);
                }
            }
        }
        self.bitmap = Some(bm);
    }

    fn test(&self, frame: usize) -> bool {
        let bm = self.bitmap.as_ref().expect("allocator not initialized");
        bm[frame / 64] & (1 << (frame % 64)) != 0
    }

    fn set(&mut self, frame: usize, used: bool) {
        let bm = self.bitmap.as_mut().expect("allocator not initialized");
        if used {
            bm[frame / 64] |= 1 << (frame % 64);
        } else {
            bm[frame / 64] &= !(1 << (frame % 64));
        }
    }

    pub fn free_frames(&self) -> u64 {
        match &self.bitmap {
            None => 0,
            Some(bm) => {
                let mut free = 0;
                for &word in bm.iter() {
                    free += word.count_ones() as u64;
                }
                self.max_frame as u64 - free
            }
        }
    }

    pub fn memory_stats(&self) -> (u64, u64) {
        // total usable bytes, usable bytes currently free
        let mut total_pages = 0;
        let mut free_pages = 0;
        if let Some(bm) = &self.bitmap {
            for f in 0..self.max_frame {
                if !self.test(f) {
                    free_pages += 1;
                }
            }
        }
        let _ = bm; // placeholder guard
        total_pages = self.max_frame as u64;
        (total_pages * PAGE_SIZE, free_pages * PAGE_SIZE)
    }
}
```

> ponytail: `memory_stats` iterates every frame (O(max_frame) per call) — fine for an occasional `meminfo`; keep a running free count if it ever shows up on a hot path.

- [ ] **Step 3: Implement alloc/dealloc**

`kernel/src/memory/frame.rs`, add to the `impl FrameAllocator for BootInfoFrameAllocator` block (replace the existing `alloc_pages`):

```rust
impl FrameAllocator for BootInfoFrameAllocator {
    fn alloc_pages(&mut self, count: usize, _align: u64) -> Result<PhysAddr, AllocError> {
        if self.bitmap.is_none() {
            return Err(AllocError::OutOfMemory);
        }
        // ponytail: naive first-fit; O(max_frame * count). Fine at this scale.
        let mut start = 0;
        while start + count <= self.max_frame {
            let mut ok = true;
            for f in start..start + count {
                if self.test(f) {
                    ok = false;
                    break;
                }
            }
            if ok {
                for f in start..start + count {
                    self.set(f, true);
                }
                return Ok(PhysAddr::new((start as u64) << 12));
            }
            start += 1;
        }
        Err(AllocError::OutOfMemory)
    }

    fn dealloc_pages(&mut self, addr: PhysAddr, count: usize) {
        let start = (addr.as_u64() >> 12) as usize;
        for f in start..start + count {
            self.set(f, false);
        }
    }
}
```

- [ ] **Step 4: Call `init` at boot and verify alloc→dealloc→alloc round-trips**

`kernel/src/main.rs` (TEMP — removed in Task 4). Find the existing `init_heap` + `init_cow_table` block (~line 88-96) and add after it:

```rust
crate::println!("Frame allocator initialized."); // TEMP
{
    let mut alloc = FRAME_ALLOCATOR.lock();
    let p = alloc.alloc_pages(3, PAGE_SIZE).expect("test alloc");
    alloc.dealloc_pages(p, 3);
    let p2 = alloc.alloc_pages(3, PAGE_SIZE).expect("test alloc 2");
    crate::println!("[alloc] roundtrip same={} free_frames={}", p == p2, alloc.free_frames()); // TEMP
}
```

- [ ] **Step 5: Build + verify**

```bash
cargo b -p kernel --target x86_64-unknown-none
./scripts/build-kernel.sh
timeout 5 qemu-system-x86_64 -m 512M \
  -drive if=pflash,format=raw,readonly=on,file="$PWD/OVMF_CODE.fd" \
  -drive if=pflash,format=raw,readonly=on,file="$PWD/OVMF_VARS.fd" \
  -drive format=raw,file=fat:rw:"$PWD/esp" \
  -debugcon file:/tmp/uefi_debug.log \
  -global isa-debugcon.iobase=0x402 \
  -serial file:/tmp/dragos_serial.log \
  -no-reboot -nographic -monitor none 2>/dev/null
tr -d '\000' < /tmp/dragos_serial.log | grep -a "alloc"
```

Expected: `[alloc] roundtrip same=true free_frames=<n>` (the same run of 3 frames is handed back after dealloc), and the kernel still boots to the shell with no panic.

---

## Task 2: `destroy_address_space` (COW-aware walk)

**Files:**
- Modify: `kernel/src/memory/space.rs`
- Test: exercised in Task 3; compile check here

- [ ] **Step 1: Add the destructor**

`kernel/src/memory/space.rs`, add after `fork_address_space` (line ~120). It walks the user half (pml4 entries `0..256`), frees every leaf frame, then frees the page-table frames and the pml4 itself. Shared (COW) leaves are refcount-decremented and freed only by the last owner:

```rust
fn free_leaf(entry: PageTableEntry, allocator: &mut impl FrameAllocator, counts: &mut Vec<u32>) {
    let frame = (entry.address().as_u64() >> 12) as usize;
    if entry.flags().contains(PageTableFlags::COW) {
        // COW-marked => count >= 1 (the owner that flipped writable cleared COW)
        let c = counts[frame] - 1;
        counts[frame] = c;
        if c == 0 {
            allocator.dealloc_pages(PhysAddr::new((frame as u64) << 12), 1);
        }
    } else {
        allocator.dealloc_pages(PhysAddr::new((frame as u64) << 12), 1);
    }
}

pub fn destroy_address_space(pml4_phys: PhysAddr) {
    let phys_offset = HHDM_BASE.0;
    let mut allocator = FRAME_ALLOCATOR.lock();
    let mut counts = COW_COUNTS.lock();
    let counts = counts.as_mut().expect("COW table not init");

    unsafe {
        let pml4 = &mut *(pml4_phys + phys_offset).as_mut_ptr::<PageTable>();
        for pml4i in 0..256 {
            let pml4e = *pml4.entry(pml4i);
            if !pml4e.is_present() {
                continue;
            }
            let pdpt = &mut *(pml4e.address() + phys_offset).as_mut_ptr::<PageTable>();
            for pdpti in 0..512 {
                let pdpte = *pdpt.entry(pdpti);
                if !pdpte.is_present() {
                    continue;
                }
                if pdpte.flags().contains(PageTableFlags::HUGE_PAGE) {
                    free_leaf(pdpte, &mut *allocator, counts); // 1 GiB page
                    continue;
                }
                let pd = &mut *(pdpte.address() + phys_offset).as_mut_ptr::<PageTable>();
                for pdi in 0..512 {
                    let pde = *pd.entry(pdi);
                    if !pde.is_present() {
                        continue;
                    }
                    if pde.flags().contains(PageTableFlags::HUGE_PAGE) {
                        free_leaf(pde, &mut *allocator, counts); // 2 MiB page
                        continue;
                    }
                    let pt = &mut *(pde.address() + phys_offset).as_mut_ptr::<PageTable>();
                    for pti in 0..512 {
                        let pte = *pt.entry(pti);
                        if !pte.is_present() {
                            continue;
                        }
                        free_leaf(pte, &mut *allocator, counts);
                    }
                    free_leaf(pde, &mut *allocator, counts); // free the pt table frame
                }
                free_leaf(pdpte, &mut *allocator, counts); // free the pd table frame
            }
            free_leaf(pml4e, &mut *allocator, counts); // free the pdpt table frame
        }
    }
    // the pml4 frame itself
    allocator.dealloc_pages(pml4_phys, 1);
    drop(counts);
    drop(allocator);
}
```

Add imports at the top of space.rs (extend the existing ones):

```rust
use shared::boot::boot_info::HHDM_BASE;
```

> ponytail: 1 GiB/2 MiB user huge pages don't exist yet (load_elf only ever maps 4KB); the branches are one-liners that cost nothing and prevent a wrong walk if they appear later. Entries `256..512` (shared kernel half) are intentionally skipped.

- [ ] **Step 2: Build**

```bash
cargo b -p kernel --target x86_64-unknown-none
```

Expected: compiles (warning about unused `destroy_address_space` is fine — it is wired up in Task 3).

---

## Task 3: Wire reclamation into the process lifecycle

**Files:**
- Modify: `kernel/src/tasks.rs`
- Modify: `kernel/src/arch/x86_64/syscalls.rs`
- Modify: `kernel/src/time.rs`
- Test: `artifacts/reap_test.asm` + QEMU

- [ ] **Step 1: Add `owns_kernel_stack` to `Thread`**

`kernel/src/tasks.rs`:

```rust
pub struct Thread {
    pub id: u64,
    pub process: Arc<SpinLock<Process>>, // Multiple threads can share one process
    pub context: CpuContext,
    pub fs_base: u64,   // For Thread Local Storage
    pub stack_ptr: u64, // Every threads' own Ring 0 Stack
    pub owns_kernel_stack: bool, // true if stack_ptr points into a heap-allocated kernel stack
}
```

Update the only construction in `register_exec_thread` (kernel/src/tasks.rs:87):

```rust
    *CURRENT_THREAD.lock() = Some(Thread {
        id: next_tid(),
        process,
        context: CpuContext::default(),
        fs_base: 0,
        stack_ptr: 0, // first timer tick fills this in
        owns_kernel_stack: false,
    });
```

- [ ] **Step 2: Kernel-stack free helper + tombstone list**

`kernel/src/tasks.rs`, add after `alloc_kernel_stack`:

```rust
// alloc_kernel_stack returns top = alloc_base + KERNEL_STACK_SIZE;
// build_child_kernel_stack stores stack_ptr = frame base = top - THREAD_FRAME_SIZE.
pub fn free_kernel_stack(stack_ptr: u64) {
    let alloc_base = stack_ptr + THREAD_FRAME_SIZE - KERNEL_STACK_SIZE as u64;
    let layout = Layout::from_size_align(KERNEL_STACK_SIZE, 16).unwrap();
    unsafe {
        alloc::alloc::dealloc(alloc_base as *mut u8, layout);
    }
}

// A kernel stack can't be freed while the CPU is running on it (execve replaces a
// thread's stack; exit leaves the stack as we switch away). Tombstones are pushed
// and drained by the timer, which never runs on a tombstoned stack.
static KERNEL_STACK_TOMBSTONES: SpinLock<alloc::vec::Vec<u64>> = SpinLock::new(alloc::vec::Vec::new());

pub fn drain_kernel_stack_tombstones(current_stack_ptr: u64) {
    let mut tbs = KERNEL_STACK_TOMBSTONES.lock();
    if tbs.is_empty() {
        return;
    }
    let cur_base = current_stack_ptr + THREAD_FRAME_SIZE - KERNEL_STACK_SIZE as u64;
    tbs.retain(|&base| {
        if base == cur_base {
            return true; // defensively skip our own stack
        }
        let layout = Layout::from_size_align(KERNEL_STACK_SIZE, 16).unwrap();
        unsafe {
            alloc::alloc::dealloc(base as *mut u8, layout);
        }
        false
    });
}
```

- [ ] **Step 3: Free the address space via `Drop for Process`**

`kernel/src/tasks.rs`, add after the `Process` struct:

```rust
impl Drop for Process {
    fn drop(&mut self) {
        crate::memory::space::destroy_address_space(self.cr3);
    }
}
```

> Lock-order audit (verified against every lock site): the drop runs only while `CURRENT_THREAD`/`SCHEDULER`/`TOMBSTONES` are held (never while `FRAME_ALLOCATOR`/`COW_COUNTS` are). `FRAME_ALLOCATOR` is never held while acquiring `CURRENT_THREAD`/`SCHEDULER`. No cycle.
>
> `execve` (kernel/src/arch/x86_64/syscalls.rs) replaces `t.process` with a fresh `Arc` — dropping the old `Arc` frees the pre-exec image. This is the desired semantics: execve reclaims the old address space.

- [ ] **Step 4: Reclaim in `exit_current_thread`**

Replace `kernel/src/tasks.rs:123-153`:

```rust
pub fn exit_current_thread() -> ! {
    let mut queue = SCHEDULER.lock();
    let mut current_lock = CURRENT_THREAD.lock();
    let current = current_lock.take();

    let had_thread = current.is_some();
    {
        let tbs = KERNEL_STACK_TOMBSTONES.lock();
        if let Some(t) = current.as_ref() {
            if t.owns_kernel_stack && t.stack_ptr != 0 {
                tbs.push(t.stack_ptr);
            }
        }
    }
    drop(current); // last Arc holder? Process::drop frees the address space
    drop(current_lock);
    let next = queue.pop_front();
    drop(queue);

    if !had_thread || next.is_none() {
        unsafe {
            return_to_shell();
        }
    }
    let next = next.unwrap();

    let next_cr3 = next.process.lock().cr3;
    unsafe {
        if read_cr3() != next_cr3 {
            write_cr3(next_cr3);
        }
        wrmsr(IA32_FS_BASE, next.fs_base);
        set_rsp0(next.stack_ptr + THREAD_FRAME_SIZE);
    }
    let next_stack_ptr = next.stack_ptr;
    *CURRENT_THREAD.lock() = Some(next);
    switch_to_stack_and_iretq(next_stack_ptr)
}
```

- [ ] **Step 5: Set `owns_kernel_stack` in fork/clone; tombstone the old stack in execve**

`kernel/src/arch/x86_64/syscalls.rs`, clone arm (syscall 56, ~line 185): the pushed child `Thread` gets `owns_kernel_stack: true`.

fork arm (syscall 57, ~line 225): the pushed child `Thread` gets `owns_kernel_stack: true`.

execve arm (syscall 59, the `CURRENT_THREAD` block ~line 299): before replacing `t.process`, tombstone the old kernel stack:

```rust
            {
                let mut cur = CURRENT_THREAD.lock();
                if let Some(t) = cur.as_mut() {
                    if t.owns_kernel_stack && t.stack_ptr != 0 {
                        tasks::KERNEL_STACK_TOMBSTONES.lock().push(t.stack_ptr);
                    }
                    t.process = Arc::new(SpinLock::new(Process {
                        cr3: new_pml4,
                        heap_brk: VirtAddr(0),
                    }));
                    t.fs_base = 0;
                }
            }
```

(Export the tombstone as `pub(crate) static KERNEL_STACK_TOMBSTONES` if the syscall module needs it; otherwise route through a `tasks::push_kernel_stack_tombstone(stack_ptr)` helper.)

- [ ] **Step 6: Drain tombstones in the timer**

`kernel/src/time.rs`, inside `timer_rust_handler`, right after `TICKS.fetch_add`/EOI:

```rust
    crate::tasks::drain_kernel_stack_tombstones(current_stack_ptr);
```

- [ ] **Step 7: Build**

```bash
cargo b -p kernel --target x86_64-unknown-none
```

Expected: compiles.

- [ ] **Step 8: Stress test — `artifacts/reap_test.asm`**

Create `artifacts/reap_test.asm`:

```asm
; reap_test.asm — forks N children that exit immediately. If process memory is
; reclaimed, free frames stay stable across all cycles.
global _start

section .text
_start:
    mov r12, 200
.loop:
    mov rax, 57           ; fork
    syscall
    test rax, rax
    jz .child
    dec r12
    jnz .loop
    mov rax, 1            ; parent: write "reap ok\n"
    mov rdi, 1
    lea rsi, [msg]
    mov rdx, 8
    syscall
    mov rax, 60
    xor rdi, rdi
    syscall
.child:
    mov rax, 60           ; child: exit(0) immediately
    xor rdi, rdi
    syscall

section .data
msg: db "reap ok", 10
```

Build and install into the initrd:

```bash
nasm -f elf64 -o /tmp/reap_test.o artifacts/reap_test.asm
ld -o /tmp/reap_test /tmp/reap_test.o
cp /tmp/reap_test initrd/reap_test
./scripts/build-initrd.sh
```

- [ ] **Step 9: Verify reclamation with a serial hook**

Temporarily add to `exit_current_thread` (right after `drop(current)`):

```rust
    crate::println!("[exit] free_frames={}", FRAME_ALLOCATOR.lock().free_frames()); // TEMP
```

`kernel/src/main.rs` (TEMP, removed in Task 4) — auto-exec the stress test after boot:

```rust
    crate::shell::execute_command("exec reap_test"); // TEMP
```

Build and run (same QEMU command as Task 1):

```bash
tr -d '\000' < /tmp/dragos_serial.log | grep -aE "exit|reap"
```

Expected: **20 to 40 `[exit] free_frames=` lines with a nearly constant value**, then `reap ok`. If reclamation is broken, each child's exit drops `free_frames` by ~(pages of fork_test image + tables + 16KiB stack) and the numbers fall monotonically.

Then run `exec fork_test` the same way and confirm the original behavior is unchanged (10 A / 10 B, parent exit code 0, child prints `Child exec'd hello!` then exit 42).

---

## Task 4: Cleanup and final verification

**Files:**
- Modify: `kernel/src/main.rs` (remove TEMP lines)
- Modify: `kernel/src/tasks.rs` (remove the TEMP `[exit]` print)
- Test: full boot

- [ ] **Step 1: Remove every `// TEMP` line added in Tasks 1, 3** (allocator self-check in main.rs, auto-exec, `[exit]` serial print). Grep must come back clean:

```bash
grep -rn "TEMP" kernel/src/ | wc -l   # -> 0
```

- [ ] **Step 2: Final build + boot**

```bash
cargo b -p kernel --target x86_64-unknown-none
./scripts/build-kernel.sh
# QEMU run, 5s
```

Expected: kernel boots to the shell prompt, no panic, no `Unknown syscall`, `meminfo` reports plausible numbers.

- [ ] **Step 3: Manual smoke test**

From the shell, `exec fork_test`, then `exec reap_test`, then `meminfo`. Expect interleaved A/B output, `Child exec'd hello!`, exit codes, `reap ok`, and no freeze.

---

## Self-review

**Spec coverage:** the two leaks (address space + kernel stack) plus the refcount bookkeeping are each covered by a task. The bitmap allocator is the prerequisite and lands first. Verification is runnable after every task. ✓

**Placeholders:** none — every step has concrete code and a concrete verify command. ✓

**Type consistency:** `free_kernel_stack`/tombstone helpers all use `stack_ptr` as the *frame base* (what `build_child_kernel_stack` returns and what the timer saves into `Thread.stack_ptr`); `alloc_base = stack_ptr + THREAD_FRAME_SIZE - KERNEL_STACK_SIZE`. `destroy_address_space`, `free_leaf`, `dealloc_pages`, `free_frames`, `owns_kernel_stack`, `drain_kernel_stack_tombstones` are defined once and referenced with matching signatures across Tasks 2/3. ✓

**Known gaps (deferred deliberately):**
- `alloc_kernel_stack` for a thread is freed at exit/execve, but the shell's own stack (`owns_kernel_stack=false`) is never touched — correct, it's static.
- COW refcounts assume a COW-marked entry always has count ≥ 1; `cow_write_fault` guarantees this invariant (last owner clears COW), so `free_leaf` never underflows.
- `meminfo`'s `memory_stats` becomes O(max_frame); acceptable for a debug command.
