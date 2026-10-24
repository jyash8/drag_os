//! Local APIC driver (xAPIC mode, memory-mapped at 0xFEE0_0000).
//!
//! The bootloader maps the local APIC MMIO region into the HHDM, so registers
//! are reached at `HHDM_BASE + phys`. The LAPIC is the substrate for both the
//! per-CPU timer and the INIT-SIPI wake sequence used to boot APs.

use shared::boot::boot_info::HHDM_BASE;
use shared::memory::address::VirtAddr;

use crate::arch::x86_64::msr::{rdmsr, wrmsr};

const LAPIC_BASE_PHYS: u64 = 0xFEE0_0000;

// Register offsets (4-byte, 16-byte stride).
const LAPIC_ID: u64 = 0x020;
const LAPIC_TPR: u64 = 0x080;
const LAPIC_EOI: u64 = 0x0B0;
const LAPIC_SVR: u64 = 0x0F0;
const LAPIC_ICR_LOW: u64 = 0x300;
const LAPIC_ICR_HIGH: u64 = 0x310;
const LAPIC_LVT_TIMER: u64 = 0x320;
const LAPIC_LVT_LINT0: u64 = 0x350;
const LAPIC_LVT_LINT1: u64 = 0x360;
const LAPIC_TIMER_INIT: u64 = 0x380;
const LAPIC_TIMER_CUR: u64 = 0x3E0;
const LAPIC_TIMER_DIV: u64 = 0x3F0;

// MSR APIC base.
const MSR_APIC_BASE: u32 = 0x1B;
const APIC_BASE_ENABLE: u64 = 1 << 11;

const SVR_APIC_ENABLE: u32 = 1 << 8;

pub fn lapic_base_virt() -> VirtAddr {
    VirtAddr(HHDM_BASE.0 + LAPIC_BASE_PHYS)
}

#[inline]
fn lapic_read(offset: u64) -> u32 {
    unsafe {
        core::ptr::read_volatile(
            (lapic_base_virt().0 as *mut u8).add(offset as usize) as *mut u32,
        )
    }
}

#[inline]
fn lapic_write(offset: u64, value: u32) {
    unsafe {
        core::ptr::write_volatile(
            (lapic_base_virt().0 as *mut u8).add(offset as usize) as *mut u32,
            value,
        );
    }
}

/// Enables the local APIC in xAPIC mode and clears TPR.
///
/// LINT0/LINT1 are deliberately NOT masked: until the IO-APIC routes device
/// IRQs, the legacy PIC delivers its interrupts (keyboard/serial/timer) through
/// LINT0, and masking it would kill all input/output.
pub unsafe fn init_lapic() {
    let base = rdmsr(MSR_APIC_BASE);
    wrmsr(MSR_APIC_BASE, base | APIC_BASE_ENABLE);
    lapic_write(LAPIC_SVR, 0xFF | SVR_APIC_ENABLE);
    lapic_write(LAPIC_TPR, 0);

    // Route legacy PIC interrupts through LINT0 as ExtINT (delivery mode 0b111 = 0x700) on BSP.
    if lapic_id() == 0 {
        lapic_write(LAPIC_LVT_LINT0, 0x700);
    } else {
        lapic_write(LAPIC_LVT_LINT0, 0x10000);
    }
    lapic_write(LAPIC_LVT_LINT1, 0x10000);
}

/// Returns this CPU's LAPIC APIC ID.
pub fn lapic_id() -> u32 {
    lapic_read(LAPIC_ID) >> 24
}

/// Sends an end-of-interrupt to the local APIC.
pub fn lapic_eoi() {
    lapic_write(LAPIC_EOI, 0);
}

/// Waits until the ICR delivery-status bit (bit 12) clears.
/// Logs a warning if the IPI is not acknowledged within ~100 000 spins;
/// the caller continues anyway (same behaviour as before) but the message
/// makes delivery failures visible instead of silently turning into
/// "AP randomly didn't boot" symptoms.
fn icr_ready() {
    let mut spins = 0u32;
    while lapic_read(LAPIC_ICR_LOW) & (1 << 12) != 0 {
        core::hint::spin_loop();
        spins += 1;
        if spins > 100_000 {
            crate::tprintln!("APIC: ICR delivery-status bit stuck after 100k spins — IPI may not have been delivered");
            break;
        }
    }
}

/// Sends an IPI to the target local APIC ID.
///
/// `delivery` is the delivery-mode field (e.g. 0b100 for INIT, 0b110 for SIPI
/// or ordinary fixed 0b000). For a targeted INIT/SIPI the `vector` is folded
/// into the low word; deliverers pass the complete low word via `low_word`.
#[allow(dead_code)]
pub fn lapic_send_ipi(target_lapic_id: u32, low_word: u32) {
    icr_ready();
    lapic_write(LAPIC_ICR_HIGH, target_lapic_id << 24);
    lapic_write(LAPIC_ICR_LOW, low_word);
    icr_ready();
}

/// Delivers an NMI to the calling CPU via ICR self shorthand.
/// ponytail: host-triggerable hang-dump path — QMP `send-key` Ctrl+Alt+F10
/// reaches input_push (and this) even with userspace dead, unlike QEMU's
/// inject-nmi which is silently a no-op on this QEMU/KVM combo. Vector is
/// ignored for NMI delivery; level must be assert, trigger edge.
pub fn lapic_self_nmi() {
    icr_ready();
    // low = 0x44402: shorthand=self(0b01<<18), level=assert(1<<14),
    // delivery=NMI(0b100<<8), vector=2 (ignored).
    lapic_write(LAPIC_ICR_HIGH, 0);
    lapic_write(LAPIC_ICR_LOW, 0x44402);
}

/// Sends an INIT assert followed by an INIT deassert to the target APIC ID.
///
/// The Intel SDM startup sequence is:
///   1. INIT assert  (delivery=INIT, level=assert,   trigger=level)
///   2. 10 ms delay
///   3. INIT deassert (delivery=INIT, level=deassert, trigger=level)
///
/// This resets the AP to real mode and leaves it ready for two SIPIs.
pub fn send_init_ipi(target_lapic_id: u32) {
    // Step 1: INIT assert — delivery=INIT(0b100), level=1, trigger=level(1).
    // ICR low = 0x4500: DM=INIT, DS=assert, TM=level.
    icr_ready();
    lapic_write(LAPIC_ICR_HIGH, target_lapic_id << 24);
    lapic_write(LAPIC_ICR_LOW, 0x4500);
    crate::time::busy_wait_ms(10);
    icr_ready();

    // Step 2: INIT deassert — same delivery mode, level=0 (bit14 clear), trigger=level(bit15 set).
    // ICR low = 0x8500: DM=INIT, DS=deassert(bit14=0), TM=level(bit15=1).
    lapic_write(LAPIC_ICR_HIGH, target_lapic_id << 24);
    lapic_write(LAPIC_ICR_LOW, 0x8500);
    icr_ready();
}

/// Sends a STARTUP IPI to `target`, vectoring the AP to `trampoline >> 12`.
pub fn send_sipi_ipi(target_lapic_id: u32, trampoline_page: u64) {
    let vec = (trampoline_page >> 12) & 0xFF;
    icr_ready();
    lapic_write(LAPIC_ICR_HIGH, target_lapic_id << 24);
    lapic_write(LAPIC_ICR_LOW, 0x4600 | (vec as u32));
    icr_ready();
}

/// Programs the LAPIC timer to fire `vector` periodically every `period_us`
/// microseconds (approx). Requires the LAPIC timer to be calibrated first via
/// `calibrate_lapic()` (else the period is a wild guess).
pub fn lapic_timer_start(vector: u8, period_us: u32) {
    lapic_write(LAPIC_TIMER_DIV, 0b1011);
    // One-shot mode (no periodic bit), unmasked
    lapic_write(LAPIC_LVT_TIMER, vector as u32);
    let count = lapic_count_for_us(period_us);
    lapic_write(LAPIC_TIMER_INIT, count);
}

pub fn lapic_timer_rearm(period_us: u32) {
    let count = lapic_count_for_us(period_us);
    lapic_write(LAPIC_TIMER_INIT, count);
}

static LAPIC_TICKS_PER_MS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(1_000_000);

pub fn calibrate_lapic() {
    // ponytail: real calibration needs PIT/HPET reference; fixed value works for QEMU
    LAPIC_TICKS_PER_MS.store(1_000_000, core::sync::atomic::Ordering::Relaxed);
}

/// Returns the timer initial-count value for `us` microseconds, using the
/// calibrated ticks-per-ms. Falls back to a fixed guess if uncalibrated.
pub fn lapic_count_for_us(us: u32) -> u32 {
    let per_ms = LAPIC_TICKS_PER_MS.load(core::sync::atomic::Ordering::Relaxed);
    if per_ms == 0 {
        return us * 1000;
    }
    (per_ms * us) / 1000
}

/// The LAPIC timer period, in microseconds.
///
/// Defaults to 1 ms (1000 us), which is what QEMU's 1 GHz LAPIC bus and the
/// `LAPIC_TICKS_PER_MS = 1_000_000` calibration above assume (invariant 7).
///
/// Overridable at build time for scheduler stress-testing:
///
/// ```sh
/// DRAGOS_TICK_US=100 cargo run -p tools -- build-kernel
/// ```
///
/// A shorter period preempts more often, which is the interesting axis when
/// asking how the scheduler behaves as process count grows — at 1 ms a short
/// wave of processes may finish entirely between two ticks and never contend
/// at all. The value is clamped so a typo cannot ask for a period the LAPIC
/// count register cannot express (the divisor is fixed at /1 above).
///
/// `option_env!` is compile-time, so changing this requires a rebuild; it is
/// not a runtime knob and makes no attempt to be one.
pub fn lapic_period_us() -> u32 {
    let default = 1000u32;
    let raw = match option_env!("DRAGOS_TICK_US") {
        Some(v) => v,
        None => return default,
    };
    match raw.trim().parse::<u32>() {
        // Floor of 50us: below that the re-arm cost dominates and the guest
        // spends its time in the timer path rather than doing work, which
        // measures the harness, not the scheduler.
        Ok(us) if us >= 50 => us.min(default),
        _ => default,
    }
}

pub fn lapic_timer_cur() -> u32 {
    lapic_read(LAPIC_TIMER_CUR)
}