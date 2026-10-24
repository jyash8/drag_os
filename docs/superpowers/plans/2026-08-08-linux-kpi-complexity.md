# Linux KPI — Complexity Assessment

> **Status:** Analysis only. No implementation in this document.
> Part of the `2026-08-08-roadmap.md` Phase 7 (capstone).

**Goal:** Estimate the effort and risk of building a **Kernel Programming Interface** (KPI) for DragOS — the in-kernel API surface that Linux device drivers and modules are written against (the contract a driver author relies on: memory allocation, locks, timers, interrupt registration, workqueues, PCI/bus helpers, and the exported-symbol table that binds a module to the core).

## What "Linux KPI" means here

Two different things travel under the name; this assessment covers the one you selected (Kernel Programming Interface for modules/drivers), and notes where the other interpretation would change the answer.

1. **In-kernel module/driver API** (the target): stable `pub` functions/types a loadable module compiles against and links to at load time — the mirror of Linux's `include/linux/*.h` + `EXPORT_SYMBOL`.
2. Userspace ABI compatibility (run Linux binaries) — *not* this scope; different project (elf-loader + syscall-ABI matching, see roadmap Phase 4).

## What DragOS has today vs. what a KPI needs

| KPI subsystem | Linux equivalent | DragOS today | Gap |
|---|---|---|---|
| Memory allocation | `kmalloc/kfree`, `GFP_*` | `alloc::vec!`, global `FixedSizeBlockAllocator` | No C-like allocator API, no `GFP` flags, no alignment control |
| Locks | `spinlock_t`, `mutex`, `rwlock`, RCU | `SpinLock<T>`, all interrupt-disabling | No IRQ-save/restore spinlocks, no sleepable mutex, no per-CPU lock state (SMP changes semantics) |
| Timers / deferral | `timer_list`, `delayed_work`, `hrtimer` | PIT tick, no timer API | None |
| Interrupt registration | `request_irq`, `irq_handler_t` | Handlers hard-wired in `IDT` Lazy (interrupts.rs) | No dynamic `request_irq` registry |
| Sleep / wait | `wait_event`, `completion`, `workqueue` | `block_current_thread(PendingBlock)` enum + 3 hardcoded wait queues | Not generic; `PendingBlock` is an enum, not a queue object |
| Device model | `struct device`, `struct bus`, probe/remove | none | Nothing |
| Driver registration | `module_init`, `platform_driver`, PCI driver table | none | Nothing |
| Symbol export | `EXPORT_SYMBOL` + kallsyms | `pub` items, no symbol table | No ELF symbol table service |
| Module loading | `finit_module`/`init_module` syscalls + ELF relocs | goblin already parses ELF for exec | Relocation against kernel base not implemented |
| Reference counting | `kref`, `get_device/put_device` | `Arc`/`Arc::strong_count` ad hoc | No `kref`-style API |
| Per-CPU | `DEFINE_PER_CPU` | nothing | Nothing (SMP is Phase 6) |
| Bus helpers | `pci_get_device`, `iomem` accessors | no PCI driver | Phase 5 prerequisite |

## Effort estimate

Rough sizing in "subsystem-weeks" for a single developer who already owns the kernel codebase (this codebase, not Linux):

| Subsystem | Effort | Complexity | Blocked by |
|---|---|---|---|
| Basic allocator + `kmalloc`-style API | 0.5–1 wk | Low | — |
| IRQ-save spinlock + `spin_lock_irqsave` | 0.5 wk | Low | Phase 6 (SMP correctness) |
| Timer/workqueue API | 1–1.5 wk | Medium | — |
| `request_irq` registry | 0.5–1 wk | Medium | — |
| Generic wait queues + `wait_event` | 1–1.5 wk | Medium | — |
| Symbol table + ELF relocations for modules | 2–3 wk | High | — |
| Module loader (in-kernel) | 1.5–2.5 wk | High | Symbol table |
| Driver/device/bus model | 2–3 wk | Medium-High | All above |
| PCI bus driver + helpers | 1–2 wk | Medium | Phase 5 |
| Per-CPU | 0.5–1 wk | Medium | Phase 6 |
| **Total (core KPI, loadable modules)** | **~11–17 wk** | | |

**Shortcut options if a loadable-module *loader* is not the goal:**
- **KPI as a static driver API only** (compile drivers into the kernel, no `init_module` syscall): drop the symbol table + ELF relocs + module loader → **~7–10 wk**. The API contract (allocators, locks, irq, timers, device model) is identical; only the load-time machinery is deferred. This is the recommended first cut — the API surface and the drivers it serves are what matter.
- **KPI as a compatibility *header* layer only** (re-export existing `pub` items under Linux-like names, e.g. `kmalloc` → current allocator): ~1 wk, but it's cosmetic — no real driver can be ported onto a thin alias layer because the semantics (GFP, sleepability, IRQ context) differ.

## Real blockers (why it's Phase 7, not Phase 2)

1. **SMP (Phase 6) changes lock semantics.** Every `SpinLock` today saves/restores IF and assumes it owns the CPU. A KPI lock contract (`spin_lock_irqsave`) must be correct under SMP — writing the KPI now and reworking it after SMP is pure throwaway.
2. **No device/bus substrate.** A KPI without PCI/storage drivers is a header file, not an interface. Phase 5 gives it concrete consumers to shape the API against.
3. **Symbol table requires kernel image metadata.** The ELF symbol table service needs the kernel's own symbols addressable at runtime; cheap to add once, but it's a prerequisite the current single-crate build doesn't produce.

## Risk ranking
1. **Module loader / ELF relocations** — highest technical risk (relocatable `.o` → linked against kernel base, GOT/PLT handling, symbol resolution failures). Mitigate by making it the *last* sub-step and keeping modules out of scope initially.
2. **SMP interaction** — second highest; mitigated by ordering after Phase 6.
3. **Device model design** — risk of over-modeling (Linux's is huge). Mitigate by YAGNI: only `platform_device` + a single bus (PCI) at first.

## Recommendation
Do **not** front-load a full module loader. Sequence as:
1. (Phase 5) PCI + one real driver (virtio-blk) — gives the KPI a consumer.
2. (Phase 6) SMP — fixes lock semantics the KPI depends on.
3. (Phase 7a) **Static KPI**: allocator, IRQ-save locks, timers, wait queues, `request_irq`, minimal device model — ~7–10 wk.
4. (Phase 7b) **Module loader** — symbol table, relocations, `init_module` — +4–7 wk, only if out-of-tree drivers are actually needed.

The static KPI (7a) delivers 90% of the value (portable driver code) for ~60% of the cost. The loader (7b) is the expensive last mile that most hobby/unikernel OSes never need.
