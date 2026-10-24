use crate::arch::x86_64::cpu::{cli, hlt};
use crate::serial_macro::COM1_PORT;
use core::panic::PanicInfo;

/// Forcibly unlocks the COM1 port. ONLY call this from the panic handler!
pub unsafe fn force_unlock_serial() {
    // We don't want to accidentally initialize the serial port if it panicked
    // before it was ever used, so we only unlock if the Lazy value has already
    // been forced. For simplicity right now, we can just force it:
    unsafe {
        COM1_PORT.force_unlock();
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // Disable interrupts immediately.
    // We don't want a timer interrupt to context-switch away from our panic,
    // and we don't want hardware interrupts muddying the state.
    cli();

    unsafe {
        force_unlock_serial();
    }

    crate::tprintln!("\n==================================================");
    crate::tprintln!("🚨 KERNEL PANIC");
    crate::tprintln!("==================================================");

    crate::tprintln!("Message: {}", info.message());
    crate::println!("KERNEL PANIC: {}", info.message()); // TEMP serial mirror

    if let Some(location) = info.location() {
        crate::tprintln!("Location: {}:{}", location.file(), location.line());
        crate::println!("PANIC@ {}:{}", location.file(), location.line()); // TEMP serial mirror
    } else {
        crate::tprintln!("Location: Unknown");
    }

    crate::tprintln!("==================================================");
    crate::tprintln!("System halted.");

    #[cfg(test)]
    crate::test_runner::exit_qemu(crate::test_runner::QemuExitCode::Failed);

    loop {
        hlt();
    }
}
