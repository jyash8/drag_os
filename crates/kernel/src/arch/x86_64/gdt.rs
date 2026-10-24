use core::arch::asm;

use crate::arch::{
    sync::lazy::Lazy,
    x86_64::tss::{TSS, TaskStateSegment},
};

const PRESENT: u64 = 1 << 47;
const USER_DPL: u64 = 3 << 45;
const CODE_DATA: u64 = 1 << 44; // Descriptor Type (1 for code/data)
const EXECUTABLE: u64 = 1 << 43;
const WRITABLE: u64 = 1 << 41;
const ACCESSED: u64 = 1 << 40;

const LONG_MODE: u64 = 1 << 53;

#[derive(Debug, Clone, Copy)]
#[repr(transparent)]
pub struct GdtEntry(u64);

impl GdtEntry {
    pub const fn null() -> Self {
        Self(0)
    }

    pub const fn kernel_code() -> Self {
        // Flags:
        // 43: Executable (1)
        // 44: Descriptor Type (1 for code/data)
        // 47: Present (1)
        // 53: Long Mode (1)
        Self(ACCESSED | EXECUTABLE | CODE_DATA | PRESENT | LONG_MODE)
    }

    pub const fn kernel_data() -> Self {
        // Flags:
        // 41: Writable (1)
        // 44: Descriptor Type (1)
        // 47: Present (1)
        Self(ACCESSED | WRITABLE | CODE_DATA | PRESENT)
    }

    pub const fn user_code() -> Self {
        // Same as kernel_code, but with USER_DPL
        Self(ACCESSED | EXECUTABLE | CODE_DATA | PRESENT | LONG_MODE | USER_DPL)
    }

    pub const fn user_data() -> Self {
        // Same as kernel_data, but with USER_DPL
        Self(ACCESSED | WRITABLE | CODE_DATA | PRESENT | USER_DPL)
    }

    pub fn tss_descriptor(tss: &'static TaskStateSegment) -> (Self, Self) {
        let ptr = tss as *const _ as u64;
//         crate::println!("tss_descriptor: TSS address is {:#x}", ptr);
        let limit = (core::mem::size_of::<TaskStateSegment>() - 1) as u64;

        let low = (limit & 0xFFFF)
            | ((ptr & 0xFFFFFF) << 16)
            | (0x89 << 40) // Access: Present, Ring 0, TSS (0x89)
            | (((limit >> 16) & 0xF) << 48)
            | ((ptr & 0xFF000000) << 32);

        let high = ptr >> 32;
        (Self(low), Self(high))
    }
}

#[repr(C, align(16))]
pub struct Gdt {
    entries: [GdtEntry; 7],
}

impl Gdt {}

#[repr(C, packed)]
struct GdtPtr {
    limit: u16,
    base: u64,
}

pub fn build_gdt(tss: &'static TaskStateSegment) -> Gdt {
    let mut gdt = Gdt {
        entries: [GdtEntry::null(); 7],
    };

    gdt.entries[1] = GdtEntry::kernel_code();
    gdt.entries[2] = GdtEntry::kernel_data();

    gdt.entries[3] = GdtEntry::user_data();
    gdt.entries[4] = GdtEntry::user_code();

    let (tss_low, tss_high) = GdtEntry::tss_descriptor(tss);
    gdt.entries[5] = tss_low;
    gdt.entries[6] = tss_high;

    gdt
}

pub static GDT: Lazy<Gdt> = Lazy::new(|| build_gdt(&TSS));

pub const KERNEL_CODE_SELECTOR: u16 = 8;
pub const KERNEL_DATA_SELECTOR: u16 = 16;

// RPL 3 (Requested Previlege Level)
pub const USER_DATA_SELECTOR: u16 = 24 | 3;
pub const USER_CODE_SELECTOR: u16 = 32 | 3;

pub const TSS_SELECTOR: u16 = 40;

pub unsafe fn init() {
    let ptr = GdtPtr {
        limit: (core::mem::size_of::<Gdt>() - 1) as u16,
        base: &*GDT as *const _ as u64,
    };

    unsafe {
        // Load GDT
        asm!("lgdt [{}]", in(reg) &ptr, options(readonly, nostack, preserves_flags));
        // Load the TSS into the Task Register
        asm!("ltr ax", in("ax") TSS_SELECTOR, options(nomem, nostack, preserves_flags));
        asm!(
            "mov ds, {0:x}",
            "mov es, {0:x}",
            "mov fs, {0:x}",
            "mov gs, {0:x}",
            "mov ss, {0:x}",
            in(reg) KERNEL_DATA_SELECTOR,
            options(nomem, nostack, preserves_flags)
        );
        asm!(
            "push {selector}",
            "lea {tmp}, [rip + 2f]",
            "push {tmp}",
            "retfq",
            "2:",
            selector = in(reg) KERNEL_CODE_SELECTOR as u64,
            tmp = lateout(reg) _,
        );
    }
}

/// Loads a per-CPU GDT (built from `gdt`) and points the task register at its
/// TSS entry. Segment selectors are identical to the boot GDT, so the data
/// segment + far-return reload from `init()` is skipped; only lgdt + ltr change.
///
/// # Safety
/// `gdt` must outlive use (live in a kernel static, e.g. `CPUS[id].gdt`), and
/// its TSS descriptor must reference this core's TSS.
pub unsafe fn load_per_cpu(gdt: &Gdt) {
    let ptr = GdtPtr {
        limit: (core::mem::size_of::<Gdt>() - 1) as u16,
        base: gdt as *const _ as u64,
    };
    unsafe {
        asm!("lgdt [{}]", in(reg) &ptr, options(readonly, nostack, preserves_flags));
        asm!("ltr ax", in("ax") TSS_SELECTOR, options(nomem, nostack, preserves_flags));
    }
}

/// Reloads CS to KERNEL_CODE_SELECTOR via far return, fixing the CS-cache
/// after `lgdt`. Call after `load_per_cpu` on a core whose CS still refers to
/// a stale GDT entry (e.g. the AP coming out of the trampoline where CS=0x18
/// was the trampoline's 64-bit code; the new kernel GDT[3] is user_data, so
/// the first iretq post-`lgdt` would fault trying to reload CS=0x18).
///
/// # Safety
/// Must be called from kernel mode with a valid stack.
pub unsafe fn reload_cs_after_lgdt() {
    unsafe {
        asm!(
            "push {sel}",
            "lea rax, [rip + 2f]",
            "push rax",
            "retfq",
            "2:",
            sel = in(reg) KERNEL_CODE_SELECTOR as u64,
            options(nostack, preserves_flags)
        );
    }
}
