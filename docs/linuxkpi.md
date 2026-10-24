# LinuxKPI Base Plan

## 1. Goal
Port a small, loadable **Linux-compatible KPI** layer so Linux-exported symbol
families (`pci register`, `request_irq`, `ioremap`, `kmalloc`, etc.) can
expose enough surface that a selected driver or out-of-tree module can
compile with `-DLINUXKPI` (kernel-only built) and load via
`init_module(2)/finit_module(2)` already defined in `module::mod`.

## 2. Top-level design
- **KPI expects Linux semantics as ABI baseline**.  
  e.g. `module_init`/`module_exit`, interrupt handlers, `remap_pio`, `probe`, etc.
- **PSCI chain**: Linux identifiers take the `linuxkpi_*` namespace, but behave
  like a Linux export table augmentation.
- **Single owner**. The KPI layer is statically compiled into the kernel or
  loaded as `linuxkpi.ko`. The export table publishes:
  `malloc_kmem_cache`, `kmalloc`, `kfree`, `ioremap`, `ioreremap`, `phys_to_virt`,
  `printk`, `request_irq`, `request_threaded_irq`, `sync`, `s schedule_names`, etc.
- **Memory / DMA rules preserved** (`AGENTS.md` 16.5/16.6).
- **No compile support for modules until loader + symbol resolution proves**.

## 3. Symbol resolution & linkage
- Modules in v1 are **built as ET_REL objects**, relocation model static,
  code-model kernel. No module-specific exports unproven.
- Because the source exports `EXPORT_SYMBOL()`, `EXPORT_SYMBOL_GPL`, they must
  match in Rust: every export includes association bit and index; the KPI picks
  non-GPL exported symbols by default.
- Symbol versioning (`MODULE_VERSION` / `VERITY`) is long-term absent.

## 4. Tier 1 deliverables (v1)
1. `LinuxKPI types.h` emulating `linux/types.h`, `linux/list.h`, `linux/errno.h`,
   `linux/scatterlist.h`, `linux/mutex.h`, `linux/spinlock.h`, `linux/workqueue.h`.
2. Minimal `alloc` entrypoints `malloc`, `free`.
3. `ioremap`, `io_unmap`. Must use the `0xffff_8000_...` HHDM window and respect
   §16.5 (offset).
4. `request_irq`, `disable_/enable_irq`.
5. DMA remapping (`dma_map_single` against the `0xFFFF_8000` persistent HHDM region).
6. `pci_register_driver`, `pci_unregister_driver` with key ID, simple BAR request
   and PCI disable device stub.

## 5. Tier 2 deliverables (later)
- `MKTOOL` stub
- `struct` class/filesystem ghosts
- `kobject`/`sysfs` stubs
- `workqueue` integration with the kernel `scheduler`

## 6. Testing strategy
- Use existing mechanism with the fixed `modules/fixture` flows.
- Add `modules/bsd/linuxkpi.ko` built with both Linuxkpi + ELF Buchner `_GPL`.
- Walk through `request_irq` without hardware to verify linker relocations hit.
- Convert a known-useful item (e.g. `usb/usbhid` module) into LinuxKPI module soon;
  elide their assertions through the fixture threshold.

## 7. Build/embed notes
- `kernel::module::sys_init_module` can run LinuxKPI modules as raw images already.
- Build tool uses `-C relocation-model=static -C code-model=kernel` with
  freestanding `extern C` entrypoints.
- Runtime loader rejects any module carrying `__ksymtab_gpl` that is not back
  by a valid rule set.

## 8. Security notes
- LinuxKPI modules cannot call into random export; define a curated Lua-like KPI table.
- Only `root/CAP_SYS_MODULE` may load the module.
- Fix SHM `0x50` bug in kernel + drivers so all module descriptors not destabilize memory.
