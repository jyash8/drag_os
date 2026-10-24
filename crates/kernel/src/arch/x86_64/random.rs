//! Kernel entropy source for `getrandom`. Uses hardware RDRAND when available
//! and falls back to a per-call xorshift64* seeded from volatile counters.

use core::sync::atomic::{AtomicU64, Ordering};

/// A xorshift64* PRNG with a rolling seed, so two calls do not return the same
/// stream. RDRAND is preferred; the PRNG is only a best-effort fallback on
/// hosts where `rdrand` faults (e.g. QEMU without `-cpu host`).
static SEED: AtomicU64 = AtomicU64::new(0x9E3779B97F4A7C15);

fn rdrand_u64() -> Option<u64> {
    let cpuid = core::arch::x86_64::__cpuid(1);
    if (cpuid.ecx & (1 << 30)) == 0 {
        return None;
    }

    let mut val: u64 = 0;
    let mut ok: u8 = 0;
    // `setc` reads the carry flag set by RDRAND (1 = valid value).
    unsafe {
        core::arch::asm!(
            "rdrand {0}",
            "setc {1}",
            out(reg) val,
            out(reg_byte) ok,
            options(nomem, nostack)
        );
    }
    if ok != 0 {
        Some(val)
    } else {
        None
    }
}

fn fallback_u64() -> u64 {
    let prev = SEED.load(Ordering::Relaxed);
    let mut x = prev ^ (crate::time::TICKS.load(Ordering::Relaxed) as u64);
    x ^= (crate::smp::current_cpu_id() as u64) << 32;
    x = x.wrapping_mul(0x2545F4914F6CDD1D);
    let next = x ^ (x >> 32);
    SEED.store(next, Ordering::Relaxed);
    next
}

/// Fills `buf` with pseudo-random bytes.
pub fn fill_random(buf: &mut [u8]) {
    for chunk in buf.chunks_mut(8) {
        let r = rdrand_u64().unwrap_or_else(fallback_u64);
        for (i, b) in chunk.iter_mut().enumerate() {
            *b = (r >> (i * 8)) as u8;
        }
    }
}