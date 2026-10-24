use core::{
    fmt::{self, Write},
    sync::atomic::{AtomicBool, AtomicU8, Ordering},
};

use crate::arch::x86_64::io::outb;

// QEMU isa-debugcon at 0x402 -> file:uefi_debug.log (see scripts/qemu.sh)
const DEBUGCON_PORT: u16 = 0x402;

// ponytail: serialize debugcon bytes across CPUs so concurrent dprintln!
// calls (e.g. an AP fault handler racing BSP syscall traces) don't interleave
// mid-line into unreadable garbage. Leaf lock (port writes only, never held
// across acquiring another lock); spinners always run with IF=0 so the holder
// on the other CPU always makes progress. Same-CPU nesting (fault handler
// firing mid-print) bypasses instead of deadlocking — rare garble beats a
// lockup in the diagnostic path.
static DEBUGCON_HELD: AtomicBool = AtomicBool::new(false);
static DEBUGCON_HOLDER: AtomicU8 = AtomicU8::new(u8::MAX);

pub struct DebugCon;

impl Write for DebugCon {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            unsafe { outb(DEBUGCON_PORT, b) };
        }
        Ok(())
    }
}

#[doc(hidden)]
pub fn _dprint(args: fmt::Arguments) {
    let my_cpu = crate::smp::current_cpu_id() as u8;
    // Same-CPU nesting (e.g. #PF handler printing while this CPU already
    // holds the port): write through, don't spin forever.
    if DEBUGCON_HOLDER.load(Ordering::Relaxed) == my_cpu
        && DEBUGCON_HELD.load(Ordering::Relaxed)
    {
        let mut c = DebugCon;
        let _ = c.write_fmt(args);
        return;
    }
    while DEBUGCON_HELD
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    DEBUGCON_HOLDER.store(my_cpu, Ordering::Relaxed);
    let mut c = DebugCon;
    let _ = c.write_fmt(args);
    DEBUGCON_HOLDER.store(u8::MAX, Ordering::Relaxed);
    DEBUGCON_HELD.store(false, Ordering::Release);
}

#[macro_export]
macro_rules! dprint {
    ($($arg:tt)*) => ($crate::debugcon::_dprint(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! dprintln {
    () => ($crate::dprint!("\n"));
    ($($arg:tt)*) => ($crate::dprint!("{}\n", format_args!($($arg)*)));
}
