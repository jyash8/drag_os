use core::{arch::naked_asm, sync::atomic::{AtomicI64, AtomicUsize}};

use crate::drivers::rtc::DateTime;

// This will increment 1000 times a second
pub static TICKS: AtomicUsize = AtomicUsize::new(0);

// ponytail: TEMP-DIAG tick-cost census accumulators (rdtsc cycles), only
// compiled with `dsp_verbose` feature to keep the hot path lean.
#[cfg(feature = "dsp_verbose")]
static TICK_NS_SUM: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
#[cfg(feature = "dsp_verbose")]
static TICK_COUNT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
#[cfg(feature = "dsp_verbose")]
static TICK_MAX: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Nanoseconds since boot (1 tick = 1 ms = 1_000_000 ns).
pub static BOOT_REALTIME_NS: AtomicI64 = AtomicI64::new(0);
/// TICKS value captured when `BOOT_REALTIME_NS` was set, so realtime can be
/// advanced by `TICKS - BOOT_TICKS` ticks.
pub static BOOT_TICKS: AtomicUsize = AtomicUsize::new(0);

/// Initializes the realtime clock base from the CMOS RTC. Call once during
/// kernel init, before any user program can call `clock_gettime`.
pub fn init_realtime_clock() {
    let dt = crate::drivers::rtc::read_datetime();
    let secs = datetime_to_unix_seconds(&dt);
    BOOT_REALTIME_NS.store(secs.saturating_mul(1_000_000_000), core::sync::atomic::Ordering::Relaxed);
    BOOT_TICKS.store(TICKS.load(core::sync::atomic::Ordering::Relaxed), core::sync::atomic::Ordering::Relaxed);
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Converts a `DateTime` (UTC) to seconds since the Unix epoch (1970-01-01).
pub fn datetime_to_unix_seconds(dt: &DateTime) -> i64 {
    let y = dt.year as i64;
    let m = dt.month as i64;
    let d = dt.day as i64;
    let h = dt.hours as i64;
    let mi = dt.minutes as i64;
    let s = dt.seconds as i64;

    let mut days: i64 = 0;
    for yy in 1970..y {
        days += if is_leap(yy) { 366 } else { 365 };
    }
    let month_days = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    for mm in 1..m {
        days += month_days[(mm - 1) as usize];
        if mm == 2 && is_leap(y) {
            days += 1;
        }
    }
    days += d - 1;
    days * 86_400 + h * 3_600 + mi * 60 + s
}

/// Returns the current monotonic time in nanoseconds since boot.
pub fn monotonic_nanos(ticks: usize) -> u128 {
    (ticks as u128) * 1_000_000
}

/// Busy-waits for `ms` milliseconds using a CPU spin loop.
pub fn busy_wait_ms(ms: u32) {
    for _ in 0..(ms as u64 * 5_000) {
        core::hint::spin_loop();
    }
}

/// Returns the current realtime (wall-clock) in nanoseconds since the epoch.
pub fn realtime_nanos() -> i64 {
    let base = BOOT_REALTIME_NS.load(core::sync::atomic::Ordering::Relaxed);
    let boot = BOOT_TICKS.load(core::sync::atomic::Ordering::Relaxed);
    let now = TICKS.load(core::sync::atomic::Ordering::Relaxed);
    let elapsed_ms = now.saturating_sub(boot) as i64;
    base.saturating_add(elapsed_ms.saturating_mul(1_000_000))
}

/// Packs a `struct timespec` (16 bytes: tv_sec i64, tv_nsec i64) into `buf`.
pub fn pack_timespec(buf: &mut [u8], sec: i64, nsec: i64) {
    buf[0..8].copy_from_slice(&sec.to_ne_bytes());
    buf[8..16].copy_from_slice(&nsec.to_ne_bytes());
}

/// Packs a `struct timeval` (16 bytes: tv_sec i64, tv_usec i64) into `buf`.
pub fn pack_timeval(buf: &mut [u8], sec: i64, usec: i64) {
    buf[0..8].copy_from_slice(&sec.to_ne_bytes());
    buf[8..16].copy_from_slice(&usec.to_ne_bytes());
}

#[unsafe(naked)]
pub extern "C" fn timer_interrupt_handler() {
    naked_asm!(
        // On entry, stack looks like:
        // RSP+0: RIP
        // RSP+8: CS
        // ...
        // We need to check if CS is User mode (CS & 3 == 3).
        "test qword ptr [rsp + 8], 3",
        "jz 1f",
        "swapgs",
        "1:",
        "push r15",
        "push r14",
        "push r13",
        "push r12",
        "push r11",
        "push r10",
        "push r9",
        "push r8",
        "push rbp",
        "push rdi",
        "push rsi",
        "push rdx",
        "push rcx",
        "push rbx",
        "push rax",
        "mov rdi, rsp",
        "call timer_rust_handler",
        "mov rsp, rax",
        "pop rax",
        "pop rbx",
        "pop rcx",
        "pop rdx",
        "pop rsi",
        "pop rdi",
        "pop rbp",
        "pop r8",
        "pop r9",
        "pop r10",
        "pop r11",
        "pop r12",
        "pop r13",
        "pop r14",
        "pop r15",
        // Check if we are returning to User mode
        "test qword ptr [rsp + 8], 3",
        "jz 2f",
        "swapgs",
        "2:",
        "iretq",
    );
}

#[unsafe(naked)]
pub extern "C" fn yield_interrupt_handler() {
    naked_asm!(
        "test qword ptr [rsp + 8], 3",
        "jz 1f",
        "swapgs",
        "1:",
        "push r15",
        "push r14",
        "push r13",
        "push r12",
        "push r11",
        "push r10",
        "push r9",
        "push r8",
        "push rbp",
        "push rdi",
        "push rsi",
        "push rdx",
        "push rcx",
        "push rbx",
        "push rax",
        "mov rdi, rsp",
        "call yield_rust_handler",
        "mov rsp, rax",
        "pop rax",
        "pop rbx",
        "pop rcx",
        "pop rdx",
        "pop rsi",
        "pop rdi",
        "pop rbp",
        "pop r8",
        "pop r9",
        "pop r10",
        "pop r11",
        "pop r12",
        "pop r13",
        "pop r14",
        "pop r15",
        "test qword ptr [rsp + 8], 3",
        "jz 2f",
        "swapgs",
        "2:",
        "iretq",
    );
}

#[unsafe(no_mangle)]
pub extern "C" fn yield_rust_handler(current_stack_ptr: u64) -> u64 {
    // Dedicated software yield: does NOT increment TICKS or send APIC EOI
    let r = crate::smp::timer_tick_on_core(current_stack_ptr);
    verify_locks_clear("yield");
    r
}

#[unsafe(no_mangle)]
pub extern "C" fn timer_rust_handler(current_stack_ptr: u64) -> u64 {
    let tick_start = unsafe { core::arch::x86_64::_rdtsc() };
    if crate::smp::current_cpu_id() == 0 {
        TICKS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        // Expire timed futex/epoll waits (parked, never spinning).
        crate::tasks::wake_expired_waiters(TICKS.load(core::sync::atomic::Ordering::Relaxed) as u64);
        crate::drivers::audio::pcspeaker::timer_tick();
        // Reap completed virtio-gpu commands and raise their fences.
        crate::drivers::drm::virtgpu::reap_gpu_fences();
        crate::drivers::drm::virtgpu::drain_deferred_submits();
        // ponytail: periodic_flush for wallpaper/scanout refresh
        if TICKS.load(core::sync::atomic::Ordering::Relaxed) % 16 == 0 {
            crate::drivers::drm::periodic_flush();
        }
        // ponytail: diagnostic stats, only compiled with dsp_verbose.
        #[cfg(feature = "dsp_verbose")]
        if TICKS.load(core::sync::atomic::Ordering::Relaxed) % 8000 == 0 {
            use core::sync::atomic::Ordering::Relaxed;
            let pushed = crate::drivers::keyboard::INPUT_PUSHED.load(Relaxed);
            let dropped = crate::drivers::keyboard::INPUT_DROPPED.load(Relaxed);
            let consumed = crate::drivers::keyboard::INPUT_CONSUMED.load(Relaxed);
            let ps2k = crate::drivers::keyboard::PS2K_IRQ.load(Relaxed);
            let krep = crate::drivers::usb::xhci::KBD_REPORT_COUNT.load(Relaxed);
            let kdrop = crate::drivers::usb::xhci::KBD_REPORT_DROP.load(Relaxed);
            let mirq = crate::drivers::ps2_mouse::IRQ_COUNT.load(Relaxed);
            let mpkt = crate::drivers::ps2_mouse::PACKET_COUNT.load(Relaxed);
            let mrep = crate::drivers::usb::xhci::MOUSE_REPORT_COUNT.load(Relaxed);
            let trep = crate::drivers::usb::xhci::TABLET_REPORT_COUNT.load(Relaxed);
            let mut snap = [(usize::MAX, 0usize, 0usize); 8];
            let mut nn = 0usize;
            {
                let map = crate::drivers::keyboard::INPUT_DEV.lock();
                for (dev, d) in map.iter() {
                    if nn >= snap.len() {
                        break;
                    }
                    snap[nn] = (*dev, d.readers.values().map(|q| q.len()).sum::<usize>(), d.readers.len());
                    nn += 1;
                }
            }
            crate::dprintln!("[input-stats] pushed={} dropped={} consumed={} nodes={} ps2k={} kbdrep={}/{} ps2m={}/{} mrep={} trep={}", pushed, dropped, consumed, nn, ps2k, krep, kdrop, mirq, mpkt, mrep, trep);
            for (dev, d, r) in snap.iter().take(nn) {
                crate::dprintln!("[input-ev{dev}] depth={d} readers={r}");
            }
            // ponytail: periodic thread snapshot every other report.
            static N: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
            let n = N.fetch_add(1, Relaxed);
            if n % 2 == 0 {
                crate::tasks::dump_thread_states("periodic");
            }
            // ponytail: stall detector.
            static STALL: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);
            static STALL_PUSHED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
            static STALL_CONSUMED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
            let backlog: u64 = snap.iter().take(nn).map(|(_, d, _)| *d as u64).sum();
            let advancing = pushed > STALL_PUSHED.load(Relaxed);
            STALL_PUSHED.store(pushed, Relaxed);
            match STALL.load(Relaxed) {
                0 if advancing && consumed == STALL_CONSUMED.load(Relaxed) && backlog > 0 => {
                    STALL.store(1, Relaxed);
                    STALL_CONSUMED.store(consumed, Relaxed);
                }
                1 if !advancing || consumed != STALL_CONSUMED.load(Relaxed) => {
                    STALL.store(0, Relaxed);
                    STALL_CONSUMED.store(consumed, Relaxed);
                }
                1 => {
                    STALL.store(2, Relaxed);
                    STALL_CONSUMED.store(consumed, Relaxed);
                    crate::dprintln!(
                        "[stall] consumer frozen: pushed={} consumed={} backlog={} dumping threads",
                        pushed, consumed, backlog
                    );
                    crate::tasks::dump_thread_states("input consumer stalled");
                }
                _ => {}
            }
        }
    }
    // Drain USB HID reports (polled xHCI event ring).
    if TICKS.load(core::sync::atomic::Ordering::Relaxed) % 8 == 0 {
        crate::drivers::usb::xhci::poll();
    }
    crate::arch::x86_64::apic::lapic_eoi();
    crate::arch::x86_64::apic::lapic_timer_rearm(crate::arch::x86_64::apic::lapic_period_us());
    crate::net::poll();
    let r = crate::smp::timer_tick_on_core(current_stack_ptr);
    verify_locks_clear("timer");
    // ponytail: TEMP-DIAG tick-cost census, only with dsp_verbose.
    #[cfg(feature = "dsp_verbose")]
    {
        let dt = unsafe { core::arch::x86_64::_rdtsc() }.saturating_sub(tick_start);
        TICK_NS_SUM.fetch_add(dt, core::sync::atomic::Ordering::Relaxed);
        TICK_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let mut max = TICK_MAX.load(core::sync::atomic::Ordering::Relaxed);
        while dt > max {
            match TICK_MAX.compare_exchange_weak(
                max,
                dt,
                core::sync::atomic::Ordering::Relaxed,
                core::sync::atomic::Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(v) => max = v,
            }
        }
    }
    r
}

#[unsafe(no_mangle)]
fn verify_locks_clear(which: &str) {
    crate::smp::assert_no_cpus_leak(which);
}
