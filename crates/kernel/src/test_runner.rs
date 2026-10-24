use crate::{arch::x86_64::cpu::out32, tprint, tprintln};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum QemuExitCode {
    Success = 0x10,
    Failed = 0x11,
}

pub fn exit_qemu(exit_code: QemuExitCode) -> ! {
    unsafe {
        out32(0xf4, exit_code as u32);
    }
    loop {
        crate::arch::x86_64::cpu::hlt();
    }
}

pub trait Testable {
    fn run(&self) -> ();
}

impl<T> Testable for T
where
    T: Fn(),
{
    fn run(&self) {
        tprint!("{}...\t", core::any::type_name::<T>());
        crate::println!("RUN: {}", core::any::type_name::<T>()); // TEMP serial mirror
        self();
        tprintln!("[ok]");
        crate::println!("OK: {}", core::any::type_name::<T>()); // TEMP serial mirror
    }
}

pub fn test_runner(tests: &[&dyn Testable]) {
    tprintln!("Running {} tests", tests.len());
    for test in tests {
        test.run();
    }
    exit_qemu(QemuExitCode::Success);
}
