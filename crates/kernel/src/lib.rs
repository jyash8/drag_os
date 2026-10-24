#![no_std]
#![cfg_attr(test, no_main)]
#![feature(abi_x86_interrupt)]
#![feature(custom_test_frameworks)]
#![test_runner(crate::test_runner::test_runner)]
#![reexport_test_harness_main = "test_main"]
#![allow(unsafe_op_in_unsafe_fn)]

extern crate alloc;

pub mod arch;
pub mod debug;
pub mod debugcon;
pub mod drivers;
pub mod elf;
pub mod exec;
pub mod fs;
pub mod graphics;
pub mod interrupts;
pub mod memory;
pub mod module;
pub mod panic;
pub mod sched;
pub mod serial_macro;
pub mod smp;
pub mod net;
pub mod tasks;
pub mod time;
pub mod users;
pub mod worker;

pub mod test_runner;
pub mod tty;

use crate::{
    arch::x86_64::{
        cpu::{KERNEL_CR3, read_cr3, sti},
        gdt,
        pic::PICS,
        syscalls::init_syscalls,
    },
    graphics::{Display, terminal::WRITER},
    memory::{
        FRAME_ALLOCATOR, heap::init_heap, space::{init_cow_table, mapper_for},
    },
};
use core::sync::atomic::Ordering;
use shared::boot::boot_info::BootInfo;

static mut BOOT_RNG_SEED: u64 = 0;
pub static BOOT_TEST_MODE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn init(boot_info: *const BootInfo) {
    let boot_info = unsafe { &*boot_info };

    if boot_info.reserved & 1 != 0 {
        BOOT_TEST_MODE.store(true, Ordering::Relaxed);
    }

    // RNG seed for ASLR, provided by the bootloader.
    unsafe { BOOT_RNG_SEED = boot_info.rng_seed; }

    // Frame allocator from the boot memory map.
    unsafe { FRAME_ALLOCATOR.lock().init(boot_info.memory_map); }
    // Print memory map for diagnostics.
    {
        let alloc = FRAME_ALLOCATOR.lock();
        let (total, usable) = alloc.memory_stats();
        crate::println!("[mm] memory map: total={} usable={} regions={}", total, usable, alloc.memory_map().len());
        for r in alloc.memory_map().iter() {
            crate::println!("[mm]   region {:#x}+{:#x} pages={} kind={:?}", r.start.as_u64(), r.length, r.page_count(), r.kind);
        }
    }

    // Kernel heap (16 MiB) — must come before any heap allocations.
    let mut mapper = mapper_for(read_cr3());
    init_heap(&mut mapper, &mut *FRAME_ALLOCATOR.lock()).expect("heap init failed");

    // COW table sized to the highest frame the frame allocator can actually
    // hand out.
    //
    // This used to take the maximum `start + length` over *every* region in the
    // memory map, including MMIO and reserved ones. On a machine whose map
    // contains a high MMIO region (this host does -- the map reaches ~1 TiB),
    // that produced `vec![0u32; 268_435_456]` and the kernel died at boot:
    //
    //   [heap-big] alloc 0x40000000 bytes
    //   KERNEL PANIC: memory allocation of 1073741824 bytes failed
    //
    // MMIO is not RAM and can never be COW-tracked, so it must not size the
    // table. This is the hazard AGENTS.md 16.10 records -- "max_frame comes from
    // the bootloader memory map" -- with the consequence spelled out: one
    // unclamped region turns a 4 MB table into a 1 GiB allocation.
    //
    // Two guards, because either alone is insufficient:
    //   1. only count frames the allocator can actually produce (usable RAM);
    //   2. clamp the result to that same count, so the table can never exceed
    //      the allocator's capacity even if the map is wrong in some new way.
    let usable_frames: usize = boot_info
        .memory_map
        .iter()
        .filter(|r| matches!(r.kind, shared::boot::MemoryRegionKind::Usable))
        .map(|r| ((r.start.as_u64() + r.length) / 4096) as usize)
        .max()
        .unwrap_or(0);
    let highest_any: usize = boot_info
        .memory_map
        .iter()
        .map(|r| ((r.start.as_u64() + r.length) / 4096) as usize)
        .max()
        .unwrap_or(0);
    let max_frame = usable_frames;
    crate::dprintln!(
        "[cow] table frames={} (highest usable; highest region end was {})",
        max_frame, highest_any
    );
    init_cow_table(max_frame);

    // Framebuffer console.
    // Establish the kernel's own .text bounds from linker-provided symbols, before
    // anything can be dispatched. The dispatch path panics on a kernel-mode RIP
    // outside .text (BADRIP), and that bound previously lived in a hand-written
    // Rust constant which silently rotted: .text grew from 0x1425a4 to 0x1653cf
    // when the module loader landed, overran the constant by ~21 KiB, and every
    // thread preempted in the new code panicked with
    //   BADRIP[enter]: tid=10 would iretq to data 0xffffffff80165186
    // -- a valid kernel text address. kernel.ld now emits __kernel_text_start /
    // __kernel_text_end, so the bound is measured by the linker and cannot rot.
    {
        let (s0, e0) = unsafe { crate::tasks::init_ktext_bounds() };
        crate::dprintln!(
            "[ktext] .text {s0:#x}..{e0:#x} (size {:#x})",
            e0.wrapping_sub(s0)
        );
    }

    if let Some(fb) = boot_info.framebuffer {
        crate::dprintln!("[fb] bootloader framebuffer: {}x{} stride={} size={} ({:#x} B) addr={:x} fmt={:?}", fb.width, fb.height, fb.stride, fb.size, fb.size, fb.address.0, fb.format);
        WRITER.lock().init(Display::new(fb));
        // Boot-time console self-check. The console's correctness is otherwise
        // only observable by looking at the screen, and the screen is normally
        // owned by a compositor -- so a broken text console is invisible. The
        // outcome goes to the debug log on every boot.
        match crate::graphics::vt::selftest() {
            Ok(()) => crate::dprintln!("[vt] selftest OK"),
            Err(e) => crate::dprintln!("[vt] selftest FAILED: {e}"),
        }
        // Render-path self-check. Both defects it guards were invisible from
        // userspace and would have been reported as "the terminal is broken"
        // with nothing to point at.
        match crate::graphics::vt::render_selftest() {
            Ok(()) => crate::dprintln!("[vt] render selftest OK"),
            Err(e) => crate::dprintln!("[vt] render selftest FAILED: {e}"),
        }
        // Per-VT input queues and line-discipline state, one per VT.
        crate::tty::init_ttys();
        match crate::tty::selftest() {
            Ok(()) => crate::dprintln!("[tty] selftest OK"),
            Err(e) => crate::dprintln!("[tty] selftest FAILED: {e}"),
        }
        // Module ELF validator (docs/module-system.md §8.1 phase 2) runs from
        // main.rs, after mount_root — it needs a filesystem, which does not
        // exist yet at this point in the boot trail.
    }

    // CPU setup: GDT, PIC, IDT, syscalls.
    unsafe {
        gdt::init();
        PICS.lock().init();
        // Mask all PIC IRQs first, then unmask only IRQ1 (keyboard), IRQ2 (cascade), IRQ4 (COM1 serial).
        PICS.lock().disable();
        PICS.lock().unmask(1);
        PICS.lock().unmask(2);
        PICS.lock().unmask(4);
        PICS.lock().unmask(12);
        // Initialize COM1 serial port to generate interrupts on received data
        crate::serial_macro::COM1_PORT.lock().init();
        // Dynamic evdev table: PS/2 init below allocates minors 0 (kbd) and
        // 1 (mouse) as their own nodes, USB/tablet take 2+ at attach.
        // Caps live with each node (kbd_caps/mouse_caps); fsio.rs serves
        // EVIOCGBIT straight from the table, never from globals.
        // Sysfs device registry: seed the four non-input static nodes.
        // (PS/2 kbd/mouse register their own event0/event1 in their init.)
        crate::fs::sysfs::sysfs_init();
        // Initialize PS/2 keyboard controller.
        crate::arch::x86_64::cpu::ps2_keyboard_init();
        // Initialize PS/2 mouse (8042 aux port, IRQ12). Failure is non-fatal.
        crate::drivers::ps2_mouse::init();
    }
    // MUST come after gdt::init(): gdt::init() ltr's the GLOBAL TSS
    // (rsp0 = static PREVILEGE_STACK). init_bsp() re-lgdt/ltr's this core's
    // per-CPU TSS; if it ran first, gdt::init() would silently point TR back
    // at the global TSS and every ring3->ring0 transition would push frames
    // onto one shared .bss stack, corrupting all thread contexts.
    crate::smp::init_bsp();
    crate::interrupts::init();
    init_syscalls();

    // Initialize FPU, MMX, and SSE (CR0, CR4, MXCSR)
    unsafe { crate::arch::x86_64::cpu::init_fpu_sse(); }

    // Save the kernel address space for idle / task switching.
    unsafe { KERNEL_CR3 = read_cr3(); }

    // ACPI (MADT/IOAPIC) for SMP.
    if let Some(rdsp) = boot_info.rdsp {
        crate::arch::x86_64::acpi::init(rdsp as usize);
    }

    crate::smp::detect_cpu_count();

    // Scan PCI bus to discover VirtIO-GPU, block devices, audio, xHCI
    crate::arch::x86_64::pci::init();

    crate::net::init();

    // Initialise the DRM subsystem (creates /dev/dri/card0 and /dev/dri/renderD128).
    crate::drivers::drm::drm_init();

    // Initialize wall-clock realtime from CMOS RTC
    crate::time::init_realtime_clock();

    sti();
}

#[cfg(test)]
#[unsafe(no_mangle)]
pub extern "sysv64" fn kernel_main(boot_info: *const shared::boot::boot_info::BootInfo) -> ! {
    init(boot_info);
    test_main();
    test_runner::exit_qemu(test_runner::QemuExitCode::Success);
}
