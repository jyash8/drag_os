use core::{
    arch::asm,
    ptr::{addr_of, addr_of_mut},
};

use shared::memory::address::PhysAddr;

use crate::arch::x86_64::{
    gdt::{USER_CODE_SELECTOR, USER_DATA_SELECTOR},
    syscalls::SHELL_CTX,
};

pub static mut KERNEL_CR3: PhysAddr = PhysAddr(0);

/// Halts the CPU until the next interrupt arrives.
#[inline(always)]
pub fn hlt() {
    unsafe {
        asm!("hlt", options(nomem, nostack, preserves_flags));
    }
}

/// Disables hardware interrupts (Clears the Interrupt Flag).
#[inline(always)]
pub fn cli() {
    unsafe {
        asm!("cli", options(nomem, nostack));
    }
}

/// Enables hardware interrupts (Sets the Interrupt Flag).
#[inline(always)]
pub fn sti() {
    unsafe {
        asm!("sti", options(nomem, nostack));
    }
}

/// Waits for PS/2 controller to not be busy (bit 1 of port 0x64).
/// Returns false on timeout — with QEMU `-machine i8042=off` (or hardware
/// without an 8042) port 0x64 reads 0xFF forever; callers must skip init.
unsafe fn ps2_wait_ready() -> bool {
    let mut n = 0usize;
    loop {
        let c = crate::arch::x86_64::io::inb(0x64);
        if c & 0x02 == 0 {
            return true;
        }
        n += 1;
        if n >= 100_000 {
            return false;
        }
    }
}

/// Configures PS/2 keyboard controller to send interrupt requests.
/// Writes to PS/2 controller at I/O port 0x64 to enable keyboard operation.
// ponytail: claims this keyboard's own evdev node (first alloc at boot, so
// minor 0) + sysfs/uevent registration; pushes target it, never an aggregate.
pub unsafe fn ps2_keyboard_init() {
    // Enable keyboard port (0xAE) via controller command port (0x64)
    if !ps2_wait_ready() {
        crate::dprintln!("[PS2KBD] no 8042 controller, skipping");
        return;
    }
    crate::arch::x86_64::io::outb(0x64, 0xAE);
    let minor = crate::drivers::keyboard::input_alloc_node(
        crate::drivers::keyboard::kbd_caps("dragos-ps2kbd"),
    );
    crate::drivers::keyboard::PS2_KBD_DEV
        .store(minor, core::sync::atomic::Ordering::Relaxed);
    crate::fs::sysfs::sysfs_register_input(minor, crate::fs::sysfs::EVDEV_CAPS_KBD);
    crate::dprintln!("[PS2KBD] evdev node event{minor} (minor {minor})");
}

#[inline(always)]
pub unsafe fn out32(port: u16, value: u32) {
    unsafe {
        asm!("out dx, eax", in("dx") port, in("eax") value, options(nomem, nostack, preserves_flags));
    }
}

#[inline(always)]
pub fn active_pml4_phys() -> u64 {
    let cr3: u64;
    unsafe {
        core::arch::asm!(
            "mov {}, cr3",
            out(reg) cr3,
            options(nomem, nostack, preserves_flags)
        );
    }
    // The lower 12 bits of CR3 are used for configuration flags.
    // We mask them out to get the pure, 4KB-aligned physical address.
    cr3 & 0x000F_FFFF_FFFF_F000
}

/// Reads the physical address of the active PML4 table from the CR3 register
pub fn read_cr3() -> PhysAddr {
    let cr3: u64;
    unsafe {
        asm!(
            "mov {}, cr3",
            out(reg) cr3,
            options(nomem, nostack, preserves_flags)
        );
    }
    PhysAddr::new(cr3 & 0x000F_FFFF_FFFF_F000)
}

pub unsafe fn write_cr3(addr: PhysAddr) {
    let cr3_val = addr.as_u64();

    // No need to mask lower bits, hardware ensures that
    unsafe {
        asm!(
            "mov cr3, {}",
            in(reg) cr3_val,
            options(nostack, preserves_flags)
        )
    }
}

pub unsafe extern "C" fn jump_to_user_mode(entry_point: u64, user_stack: u64) -> ! {
    // RFLAGS: 0x200 is the Interrupt Enable flag (IF).
    // 0x2 is a reserved bit that the x86 manual mandates must ALWAYS be 1.
    let rflags: u64 = 0x202;

    let user_data = USER_DATA_SELECTOR as u64;
    let user_code = USER_CODE_SELECTOR as u64;

    unsafe {
        asm!(
            "mov ds, {0:e}",
            "mov es, {0:e}",
            "mov fs, {0:e}",
            "mov gs, {0:e}",
            "push {0}", // SS (Stack segment)
            "push {1}", // RSP (Stack Pointer)
            "push {2}", // RFLAGS
            "push {3}", // CS (Code segment)
            "push {4}", // RIP (Instruction Pointer)
            // Clear out general purpose registers to prevent
            // leaking kernel data to user space
            "xor rax, rax",
            "xor rbx, rbx",
            "xor rcx, rcx",
            "xor rdx, rdx",
            "xor r8, r8",
            "xor r9, r9",
            "xor r10, r10",
            "xor r11, r11",
            "xor r12, r12",
            "xor r13, r13",
            "xor r14, r14",
            "xor r15, r15",

            "iretq",

            in(reg) user_data,
            in(reg) user_stack,
            in(reg) rflags,
            in(reg) user_code,
            in(reg) entry_point,
            options(noreturn)
        );
    }
}

pub unsafe fn execute_and_resume(entry: u64, stack: u64) {
    let ctx_ptr = addr_of_mut!(SHELL_CTX);

    unsafe {
        asm!(
            // Save all the calle-preserved registers
            "mov [r8+0x00], rbx",
            "mov [r8+0x08], rbp",
            "mov [r8+0x10], r12",
            "mov [r8+0x18], r13",
            "mov [r8+0x20], r14",
            "mov [r8+0x28], r15",

            // Save the shell's stack pointer
            "mov [r8 + 0x30], rsp",

            // Safe the instruction pointer (rip) of the local label '2'
            "lea r9, [rip + 2f]",
            "mov [r8 + 0x38], r9",

            "call {jump_func}",

            "2:",

            jump_func = sym jump_to_user_mode,

            in("r8") ctx_ptr,
            in("rdi") entry,
            in("rsi") stack,

            clobber_abi("C")
        );
    }
}

pub fn read_cr0() -> u64 {
    let out: u64;
    unsafe {
        asm!("mov {}, cr0", out(reg) out, options(nomem, preserves_flags));
    }
    out
}

pub unsafe fn write_cr0(val: u64) {
    asm!("mov cr0, {}", in(reg) val, options(nostack, preserves_flags));
}

pub fn read_cr4() -> u64 {
    let out: u64;
    unsafe {
        asm!("mov {}, cr4", out(reg) out, options(nomem, preserves_flags));
    }
    out
}

pub unsafe fn write_cr4(val: u64) {
    asm!("mov cr4, {}", in(reg) val, options(nostack, preserves_flags));
}

/// Initialize FPU, MMX, and SSE capabilities on the current CPU core.
/// Enables hardware FPU instructions, FXSAVE/FXRSTOR, and SSE exception handling.
pub unsafe fn init_fpu_sse() {
    // CR0: Set MP (bit 1), NE (bit 5), clear EM (bit 2), clear TS (bit 3)
    let mut cr0 = read_cr0();
    cr0 |= (1 << 1) | (1 << 5); // MP, NE
    cr0 &= !((1 << 2) | (1 << 3)); // Clear EM, TS
    write_cr0(cr0);

    // CR4: Set OSFXSR (bit 9), OSXMMEXCPT (bit 10), PGE (bit 7)
    let mut cr4 = read_cr4();
    cr4 |= (1 << 7) | (1 << 9) | (1 << 10);
    write_cr4(cr4);

    // Initialize FPU state with fninit and standard MXCSR (0x1f80)
    asm!("fninit", options(nomem, nostack));
    let mxcsr: u32 = 0x1f80; // Default mask all exceptions
    asm!("ldmxcsr [{}]", in(reg) &mxcsr, options(nostack));
}

pub unsafe fn return_to_shell() -> ! {
    let ctx_ptr = addr_of!(SHELL_CTX);

    unsafe {
        asm!(
            "mov rbx, [r8 + 0x00]",
            "mov rbp, [r8 + 0x08]",
            "mov r12, [r8 + 0x10]",
            "mov r13, [r8 + 0x18]",
            "mov r14, [r8 + 0x20]",
            "mov r15, [r8 + 0x28]",

            "mov rsp, [r8 + 0x30]",

            "mov r9, [r8 + 0x38]",
            "jmp r9",

            in("r8") ctx_ptr,
            options(noreturn)
        )
    }
}
