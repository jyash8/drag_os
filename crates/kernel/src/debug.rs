use core::arch::asm;
pub unsafe fn set_hw_breakpoint(addr: u64) {
    unsafe {
        asm!(
            "mov dr0, {0}",
            "mov rax, 0x00010001", // local exact breakpoint enable (bit 0), write-only (bit 16)
            "mov dr7, rax",
            in(reg) addr,
            out("rax") _,
        );
    }
}
