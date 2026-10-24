//! SMP scaffold (design: docs/superpowers/specs/2026-08-12-smp-scheduler-design.md)
//!
//! All function bodies are `todo!()` placeholders; signatures/structs are the
//! contract to implement.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use shared::memory::address::PhysAddr;

use crate::arch::{
    sync::spinlock::SpinLock,
    x86_64::{
        gdt::Gdt,
        syscalls::CpuState,
        tss::TaskStateSegment,
    },
};
use crate::tasks::{Thread, STOPPED};

pub const MAX_CPUS: usize = 4;

/// Number of CPUs detected at boot (BSP always id 0). Set during init.
pub static CPU_COUNT: AtomicUsize = AtomicUsize::new(1);

/// Maps kernel CPU index → hardware LAPIC APIC ID.
/// Populated by `detect_cpu_count()` from the MADT; index 0 is always the BSP.
pub static CPU_APIC_IDS: [core::sync::atomic::AtomicU32; MAX_CPUS] = [
    core::sync::atomic::AtomicU32::new(0),
    core::sync::atomic::AtomicU32::new(0),
    core::sync::atomic::AtomicU32::new(0),
    core::sync::atomic::AtomicU32::new(0),
];

/// Reverse map: APIC ID → kernel CPU index.  Initialised to 0xFF (= invalid).
/// Only 8-bit APIC IDs are supported (xAPIC).
pub static APIC_ID_TO_CPU_INDEX: [core::sync::atomic::AtomicU8; 256] = {
    // const-initialise all 256 entries to 0xFF
    const INVALID: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0xFF);
    [INVALID; 256]
};

/// Per-CPU syscall register save area. Only accessed by the current CPU.
///
/// Wrapped in `UnsafeCell` inside a local newtype (instead of `static mut`)
/// to make the per-CPU exclusive-access invariant explicit and avoid the
/// technically-UB `&mut` from `static mut` under Rust's aliasing model.
/// Safety: each CPU only ever accesses `CPU_STATES[my_cpu_id()]`.
pub struct CpuStateArray([core::cell::UnsafeCell<CpuState>; MAX_CPUS]);

// Safety: each CPU only ever accesses CPU_STATES[my_cpu_id()]. The indices are
// disjoint across CPUs, so concurrent reads/writes through UnsafeCell are sound.
unsafe impl Sync for CpuStateArray {}

pub static CPU_STATES: CpuStateArray = CpuStateArray([
    core::cell::UnsafeCell::new(CpuState { kernel_stack: 0, user_stack: 0, user_rip: 0, user_rflags: 0, fpu_scratch: [0; 512] }),
    core::cell::UnsafeCell::new(CpuState { kernel_stack: 0, user_stack: 0, user_rip: 0, user_rflags: 0, fpu_scratch: [0; 512] }),
    core::cell::UnsafeCell::new(CpuState { kernel_stack: 0, user_stack: 0, user_rip: 0, user_rflags: 0, fpu_scratch: [0; 512] }),
    core::cell::UnsafeCell::new(CpuState { kernel_stack: 0, user_stack: 0, user_rip: 0, user_rflags: 0, fpu_scratch: [0; 512] }),
]);

/// Flipped to `false` once `detect_cpu_count()` has finished populating
/// `APIC_ID_TO_CPU_INDEX`.  Before that point the only valid APIC ID is 0
/// (the BSP), so a 0xFF lookup result is expected and maps to CPU index 0.
/// After that point a 0xFF result means an unregistered APIC ID — a bug,
/// not an early-boot condition — and we must not silently return 0.
static BSP_EARLY_BOOT: AtomicBool = AtomicBool::new(true);

/// Per-CPU structure. Each core mutates only `CPUS[my_id]` except for reads of
/// other cores' `active`/wake state. Holds everything that is physically
/// per-CPU: TSS (rsp0), GDT (per-CPU TSS entry), and this core's
/// currently running thread.
pub struct CpuLocal {
    pub id: u32,
    pub tss: TaskStateSegment,
    pub gdt: Gdt,
    /// This core's running thread (replaces the global `CURRENT_THREAD`).
    pub current: Option<Thread>,
    /// Stack used by an AP while parked (wake condition wait).
    pub idle_stack_top: u64,
    /// Dedicated IST stack for double faults on this core.
    pub df_stack_top: u64,
    /// Set once this core's GDT/TSS/IDT/timer are installed.
    pub active: AtomicBool,
}

/// All-None until each core fills its slot in `ap_entry`.
pub static CPUS: [SpinLock<Option<CpuLocal>>; MAX_CPUS] = [
    SpinLock::named("CPUS[0]", None),
    SpinLock::named("CPUS[1]", None),
    SpinLock::named("CPUS[2]", None),
    SpinLock::named("CPUS[3]", None),
];

pub static IDLE_STACK_TOPS: [core::sync::atomic::AtomicU64; MAX_CPUS] =
    [const { core::sync::atomic::AtomicU64::new(0) }; MAX_CPUS];

/// Bumps when a runnable thread is pushed to the shared queue and an AP is
/// parked. APs park on this condition rather than busy-polling the queue.
pub static RUNNABLE_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Detects the CPU count via MADT/APIC (acpi.rs). Stores into `CPU_COUNT`.
/// Also populates `CPU_APIC_IDS` (index → apic_id) and `APIC_ID_TO_CPU_INDEX`
/// (apic_id → index) so that hardware IDs and kernel indices are never conflated.
pub fn detect_cpu_count() -> usize {
    use acpi::sdt::madt::{Madt, MadtEntry};

    let Some(tables) = (unsafe { &*core::ptr::addr_of!(crate::arch::x86_64::acpi::ACPI_TABLES) })
        .as_ref()
    else {
        // Single-CPU fallback: BSP has APIC ID 0.
        CPU_APIC_IDS[0].store(0, Ordering::Relaxed);
        APIC_ID_TO_CPU_INDEX[0].store(0, Ordering::Relaxed);
        CPU_COUNT.store(1, Ordering::Relaxed);
        return 1;
    };

    let Some(madt) = tables.find_table::<Madt>() else {
        CPU_APIC_IDS[0].store(0, Ordering::Relaxed);
        APIC_ID_TO_CPU_INDEX[0].store(0, Ordering::Relaxed);
        CPU_COUNT.store(1, Ordering::Relaxed);
        return 1;
    };

    let mut count = 0usize;
    let mut raw_count = 0usize;
    for entry in madt.get().entries() {
        if let MadtEntry::LocalApic(e) = entry {
            if { e.flags } & 1 != 0 {
                if count < MAX_CPUS {
                    let apic_id = e.apic_id as u32;
                    // `APIC_ID_TO_CPU_INDEX` is indexed by the *raw* APIC ID, so
                    // an ID >= 256 would index past the array. Only 8-bit (xAPIC)
                    // IDs are representable, so skip anything larger rather than
                    // writing out of bounds: the CPU still gets a `CPUS` slot,
                    // but its ID cannot be reverse-mapped, and a later lookup for
                    // it reports "unregistered" rather than corrupting memory.
                    if (apic_id as usize) < APIC_ID_TO_CPU_INDEX.len() {
                        CPU_APIC_IDS[count].store(apic_id, Ordering::Relaxed);
                        APIC_ID_TO_CPU_INDEX[apic_id as usize].store(count as u8, Ordering::Relaxed);
                    } else {
                        crate::dprintln!(
                            "[SMP] MADT LocalApic id={} is not 8-bit; cannot be \
                             reverse-mapped, skipping registration",
                            apic_id
                        );
                    }
                }
                raw_count += 1;
                count += 1;
            }
        }
    }
    // `raw_count` is the number of *enabled* LocalApic entries in the MADT,
    // which can exceed MAX_CPUS. Only the first MAX_CPUS got a `CPUS` slot
    // above, so the published count must not claim more CPUs than exist --
    // every other CPU_COUNT reader indexes CPUS[]. Clamping here means the
    // published value is a statement about the array, not about the firmware.
    if raw_count > MAX_CPUS {
        crate::dprintln!(
            "[SMP] MADT reports {raw_count} LocalApic entries but MAX_CPUS is {MAX_CPUS}; \
             using the first {MAX_CPUS}"
        );
    }
    let count = count.clamp(1, MAX_CPUS);
    CPU_COUNT.store(count, Ordering::Relaxed);
    // Signal that APIC ID → CPU index table is now complete. Any 0xFF lookup
    // after this point is a genuine error, not an expected early-boot state.
    BSP_EARLY_BOOT.store(false, Ordering::Release);
    count
}

/// Returns the kernel CPU index (0 = BSP, 1.. = APs) for the core running this code.
///
/// This looks up the hardware LAPIC ID in `APIC_ID_TO_CPU_INDEX` so that the
/// kernel's `CPUS[]` array is always indexed by a stable software index, never
/// directly by the hardware APIC ID (which is not guaranteed to be 0..N-1).
pub fn current_cpu_id() -> u32 {
    let lapic_id = crate::arch::x86_64::apic::lapic_id();
    let idx = APIC_ID_TO_CPU_INDEX[lapic_id as usize].load(Ordering::Relaxed);
    if idx == 0xFF || idx == 255 {
        // Two legitimate causes of 0xFF:
        // 1. Very early BSP boot — APIC_ID_TO_CPU_INDEX not yet populated.
        //    Safe to return 0: the BSP always has kernel index 0.
        // 2. An unregistered APIC ID *after* detect_cpu_count() ran — this is
        //    a bug (hardware APIC ID not in MADT, or called from a rogue context).
        //    Returning 0 here would silently corrupt CPU 0's thread/TSS/state.
        if !BSP_EARLY_BOOT.load(Ordering::Acquire) {
            crate::tprintln!("WARNING: current_cpu_id() called with lapic_id={} but idx=0xFF", lapic_id);
        }
        if BSP_EARLY_BOOT.load(Ordering::Acquire) {
            return 0;
        }
        panic!("current_cpu_id: APIC ID {} not in lookup table — called after early boot on unknown core", lapic_id);
    }
    idx as u32
}

/// Calls the closure with a mutable reference to this CPU's `CpuState`.
/// Uses the lock-free `CPU_STATES` array since CpuState is per-CPU.
pub fn with_cpu_state<F, R>(f: F) -> R
where
    F: FnOnce(&mut CpuState) -> R,
{
    let cpu_id = current_cpu_id() as usize;
    // Safety: CPU_STATES is per-CPU, only accessed by the current CPU.
    // The `UnsafeCell::get()` deref gives `&mut CpuState` from the raw pointer.
    f(unsafe { &mut *CPU_STATES.0[cpu_id].get() })
}

/// Calls the closure with a mutable reference to this CPU's TSS.
/// BSP phase targets `CPUS[0].tss`. Uses the same closure pattern as
/// `with_cpu_state`; only the current CPU touches its own TSS.
pub fn with_cpu_tss<F, R>(f: F) -> R
where
    F: FnOnce(&mut TaskStateSegment) -> R,
{
    let mut slot = CPUS[current_cpu_id() as usize].lock();
    f(&mut slot.as_mut().unwrap().tss)
}

/// Builds the per-CPU GDT (identical selectors, this core's own TSS entry).
fn build_per_cpu_gdt(tss: &'static TaskStateSegment) -> Gdt {
    crate::arch::x86_64::gdt::build_gdt(tss)
}

/// Wakes an AP parked in the idle loop. Called after a thread is pushed to
/// `SCHEDULER`. With a condition-on-queue-lock wake this may be a no-op beyond
/// bumping `RUNNABLE_COUNT`; see spec's pinned wake model.
pub fn signal_runnable() {
    RUNNABLE_COUNT.fetch_add(1, Ordering::Release);
}

/// Initializes `CPUS[0]` as the BSP. Called once during kernel init before
/// the first thread runs. This core is always id 0.
pub fn init_bsp() {
    let mut slot = CPUS[0].lock();
    let idle_stack = crate::tasks::alloc_kernel_stack();
    let df_stack = crate::tasks::alloc_kernel_stack();
    IDLE_STACK_TOPS[0].store(idle_stack, Ordering::Release);

    let mut tss = TaskStateSegment::new();
    tss.interrupt_stack_table[crate::arch::x86_64::tss::DOUBLE_FAULT_IST_INDEX as usize] = df_stack;
    tss.previlege_stack_table[0] = idle_stack;

    let cpu = slot.get_or_insert(CpuLocal {
        id: 0,
        tss,
        // ponytail: real per-CPU GDT built below once the TSS has a stable
        // address inside the static CPUS slot.
        gdt: unsafe { core::mem::zeroed() },
        current: None,
        // Private idle stack (heap, high-mapped): cpu_idle_loop switches to
        // it on entry so idlers never borrow parked threads' stacks (which
        // get resumed on another CPU mid-borrow → mutual smashing).
        idle_stack_top: idle_stack,
        df_stack_top: df_stack,
        active: AtomicBool::new(true),
    });

    // The TSS now has a stable address (lives in the static CPUS array).
    // Safety: `CPUS` is a static, never moves; the field address is 'static.
    let tss_static: &'static TaskStateSegment = unsafe {
        &*(&cpu.tss as *const TaskStateSegment)
    };
    cpu.gdt = build_per_cpu_gdt(tss_static);

    // Make this core's TR point at its own TSS so `set_rsp0`(CPUS[0].tss)
    // actually updates the TSS the CPU uses on ring3->ring0 transitions.
    unsafe {
        crate::arch::x86_64::gdt::load_per_cpu(&cpu.gdt);
    }
}

/// Builds `CPUS[id]` (GDT/TSS, idle stack) for a non-BSP core. Called by the
/// BSP before SIPI; `ap_entry` then loads the prebuilt GDT/TSS live.
pub fn prepare_cpu(id: u32) {
    let mut slot = CPUS[id as usize].lock();
    let idle_stack = crate::tasks::alloc_kernel_stack();
    let df_stack = crate::tasks::alloc_kernel_stack();
    IDLE_STACK_TOPS[id as usize].store(idle_stack, Ordering::Release);

    let mut tss = TaskStateSegment::new();
    tss.interrupt_stack_table[crate::arch::x86_64::tss::DOUBLE_FAULT_IST_INDEX as usize] = df_stack;
    tss.previlege_stack_table[0] = idle_stack;

    let cpu = slot.get_or_insert(CpuLocal {
        id,
        tss,
        gdt: unsafe { core::mem::zeroed() },
        current: None,
        // AP idle stack: heap-allocated (high-mapped kernel memory, shared by every
        // address space) so the AP keeps a valid stack across write_cr3 into user
        // CR3s on first dispatch. Raw low frames with a kernel-CR3-only identity
        // map go unmapped the moment the AP switches CR3, and the next push/call
        // triple-faults with zero output (silent reset on first fork).
        idle_stack_top: idle_stack,
        df_stack_top: df_stack,
        active: AtomicBool::new(false),
    });

    let tss_static: &'static TaskStateSegment =
        unsafe { &*(&cpu.tss as *const TaskStateSegment) };
    cpu.gdt = build_per_cpu_gdt(tss_static);
}

/// Wakes all detected APs via the trampoline. Returns the count woken.
pub fn boot_aps() -> usize {
    let count = CPU_COUNT.load(Ordering::Relaxed).min(MAX_CPUS);
    if count <= 1 {
        return 0;
    }

    // Low kernel CR3 must identity-map the trampoline page before we patch.
    let cr3 = crate::arch::x86_64::cpu::active_pml4_phys();
    crate::arch::x86_64::trampoline::identity_map_trampoline(PhysAddr(cr3));
    crate::arch::x86_64::trampoline::install();

    let mut woken = 0;
    for id in 1..count {
        prepare_cpu(id as u32);
        // Use the real LAPIC APIC ID from the MADT table, not the kernel CPU
        // index. On most QEMU configs they happen to match, but on real
        // hardware or non-default QEMU configs APIC IDs can be non-contiguous.
        let lapic_id = CPU_APIC_IDS[id].load(Ordering::Relaxed);
        let entry = ap_entry as *const () as usize as u64;
        let stack = IDLE_STACK_TOPS[id].load(Ordering::Relaxed);
        // Safety: CPUS[id] is a static, never moves; ap_entry outlives its boot.
        crate::arch::x86_64::trampoline::patch(entry, cr3, stack);
        crate::arch::x86_64::trampoline::wake_ap(lapic_id);
        crate::println!("SMP: sent INIT-SIPI to AP{}", id);
        // Wait for the AP to mark itself active.
        let mut tries = 0;
        while !CPUS[id].lock().as_ref().unwrap().active.load(Ordering::Acquire) {
            crate::time::busy_wait_ms(1);
            tries += 1;
            if tries > 3000 {
                break;
            }
        }
        // Only count this AP as live if it actually set active=true.
        // A timed-out AP must NOT be counted: the scheduler must never
        // dispatch to a core index that never came online.
        if CPUS[id].lock().as_ref().unwrap().active.load(Ordering::Acquire) {
            woken += 1;
            crate::println!("SMP: {} AP(s) online", woken);
        } else {
            crate::tprintln!("SMP: AP{} did not boot — marking unusable", id);
        }
    }
    woken
}

/// AP entry point reached from the low-memory trampoline (real→32→64-bit).
/// Installs per-CPU GDT/TSS/IDT, sets rsp0/fs/gs, starts this core's timer,
/// marks `active`, then enters the idle wake loop. Never returns.
pub extern "C" fn ap_entry(_id: u32) -> ! {
    let cpu_id = current_cpu_id();
    let slot = CPUS[cpu_id as usize].lock();
    let cpu = slot.as_ref().unwrap();

    // Install the per-CPU GDT/TSS that the BSP built into CPUS[id].
    unsafe {
        crate::arch::x86_64::gdt::load_per_cpu(&cpu.gdt);
    }

    // ponytail: AP comes out of the trampoline with CS=0x18 (the trampoline's
    // 64-bit code segment). The kernel GDT's slot 3 is user_data, so the first
    // iretq that tries to restore CS=0x18 GPFs. Far-return now to load CS
    // from the fresh GDT (selector 0x08 = kernel_code).
    unsafe {
        crate::arch::x86_64::gdt::reload_cs_after_lgdt();
    }

    // Load the shared IDT (kernel-wide handlers; per-CPU IST is out of scope).
    unsafe {
        crate::interrupts::IDT.load();
        crate::arch::x86_64::cpu::init_fpu_sse();
    }

    // Per-CPU syscall MSRs: enable SCE, NXE, STAR, LSTAR and point KERNEL_GS_BASE at CPU_STATES[id]
    crate::arch::x86_64::syscalls::init_syscalls();
    crate::smp::with_cpu_state(|st| {
        unsafe {
            crate::arch::x86_64::msr::wrmsr(
                crate::arch::x86_64::msr::IA32_KERNEL_GS_BASE,
                st as *mut _ as u64,
            );
        }
    });

    // Explicitly initialize Local APIC on AP core before timer start (Invariant 7).
    unsafe { crate::arch::x86_64::apic::init_lapic(); }

    // Start this core's LAPIC timer (calibrated period, ~1ms).
    crate::arch::x86_64::apic::lapic_timer_start(
        crate::interrupts::TIMER_INTERRUPT_ID,
        crate::arch::x86_64::apic::lapic_period_us(),
    );

    cpu.active.store(true, Ordering::Release);
    drop(slot);

    crate::arch::x86_64::cpu::sti();

    crate::tprintln!("SMP: AP{} online", cpu_id);
    ap_idle_loop()
}

/// Core's timer tick. Per-core version of the single-CPU handler in time.rs:
/// 1. take CPUS[k].current, save stack_ptr
/// 2. pending block + runnable alternative? -> park to wait queue
/// 3. else round-robin via shared SCHEDULER
/// 4. set current, switch cr3/fs_base/tss.rsp0/syscall_stack
/// 5. return new stack_ptr for the naked asm to iretq into
pub fn timer_tick_on_core(mut current_stack_ptr: u64) -> u64 {
    let mut queue = crate::tasks::SCHEDULER.lock();
    let cpu_id = current_cpu_id();
    let mut cpu_slot = CPUS[cpu_id as usize].lock();

    if let Some(mut current) = cpu_slot.as_mut().unwrap().current.take() {
        current.stack_ptr = current_stack_ptr;
        current.fpu_save();

        // Account CPU time used by the current thread
        crate::sched::sched_account(&mut current, 1);

        // Check if the current thread requested to be blocked
        let pending = current.pending_block;

        if let Some(target) = pending {
            crate::tasks::check_no_double_owner(&current, "park", &queue);
            match target {
                crate::tasks::PendingBlock::Keyboard => {
                    // SMP lost-wakeup closure: a keystroke ISR on another core
                    // can push into STDIN_BUFFER in the window between the
                    // reader seeing it empty and this park. Its wake finds an
                    // empty KEYBOARD_WAIT_QUEUE, so nothing reschedules the
                    // reader and the char is stuck forever. Register first,
                    // then re-check the buffer. try_lock (never block) so we
                    // don't deadlock against the ISR's STDIN -> SCHEDULER
                    // order while holding SCHEDULER -> CPUS[k] here. If the
                    // ISR holds the buffer lock, it is mid-push and will wake
                    // us (we are already in the queue).
                    let mut wq = crate::tasks::KEYBOARD_WAIT_QUEUE.lock();
                    wq.push_back(current);
                    // Re-check the per-VT tty queues rather than the old
                    // global STDIN_BUFFER, which no longer receives
                    // keystrokes. Every VT is probed, not just the active one:
                    // a reader parked on /dev/tty2 must not sleep through input
                    // that arrived while VT 2 was on screen.
                    //
                    // `has_input` try_locks internally and never blocks, so
                    // this cannot deadlock against the ISR's TTYS -> SCHEDULER
                    // order while holding SCHEDULER -> CPUS[k] here. A failed
                    // try_lock reports "no data", which is safe: the holder is
                    // mid-push and its wake-up finds this thread in the queue.
                    let has_data = (1..=crate::graphics::vt::MAX_VTS as u16)
                        .any(|n| crate::tty::has_input(n));
                    if has_data {
                        let mut resumed = wq.pop_back().unwrap();
                        drop(wq);
                        resumed.pending_block = None;
                        cpu_slot.as_mut().unwrap().current = Some(resumed);
                        return current_stack_ptr;
                    }
                    drop(wq);
                }
                crate::tasks::PendingBlock::ChildWait => {
                    // Lost-wakeup fix: child may have exited before this park.
                    // If a zombie already exists, don't park — let wait4 reap it.
                    let has_zombie = {
                        let p = current.process.lock();
                        p.children.iter().any(|c| c.lock().status == crate::tasks::ProcessStatus::Zombie)
                    };
                    if has_zombie {
                        let mut cur = current;
                        cur.pending_block = None;
                        cpu_slot.as_mut().unwrap().current = Some(cur);
                        return current_stack_ptr;
                    }
                    crate::tasks::CHILD_WAIT_QUEUE.lock().push_back(current);
                }
                crate::tasks::PendingBlock::PipeWait => {
                    // SMP lost-wakeup closure, same class as Keyboard/Input but
                    // keyed on a global sequence instead of a probe-able buffer
                    // (the buffer lives in whichever subsystem parked us: pty,
                    // pipe, socket, eventfd). A producer can push and call
                    // wake_pipe_waiters() *after* this thread saw the buffer
                    // empty but *before* it parks; that wake finds
                    // PIPE_WAIT_QUEUE empty and is otherwise lost forever, so
                    // the reader sleeps with data available. If the sequence
                    // advanced since this thread last checked, resume it so its
                    // loop re-checks; it will park again if still empty. We hold
                    // SCHEDULER, which wake_pipe_waiters() also takes, so the
                    // compare and the enqueue cannot interleave with a wake.
                    let seq = crate::tasks::PIPE_WAKE_SEQ
                        .load(core::sync::atomic::Ordering::Relaxed);
                    if seq != current.pipe_seq_baseline {
                        current.pipe_seq_baseline = seq;
                        current.pending_block = None;
                        cpu_slot.as_mut().unwrap().current = Some(current);
                        return current_stack_ptr;
                    }
                    crate::tasks::PIPE_WAIT_QUEUE.lock().push_back(current);
                }
                // ponytail: epoll parks on the pipe queue (broad wakes) with
                // deadline expiry via the tick sweep; see PendingBlock docs.
                crate::tasks::PendingBlock::EpollWait { .. } => {
                    crate::tasks::PIPE_WAIT_QUEUE.lock().push_back(current);
                }
                crate::tasks::PendingBlock::Futex { uaddr, expected, .. } => {
                    let mut wq = crate::tasks::FUTEX_WAIT_QUEUE.lock();
                    wq.push_back(current);
                    let changed = unsafe {
                        if uaddr < 0x8000_0000_0000 && uaddr & 3 == 0 {
                            core::ptr::read_volatile(uaddr as *const u32) != expected
                        } else {
                            false
                        }
                    };
                    if changed {
                        let mut resumed = wq.pop_back().unwrap();
                        drop(wq);
                        resumed.pending_block = None;
                        cpu_slot.as_mut().unwrap().current = Some(resumed);
                        return current_stack_ptr;
                    }
                }
                crate::tasks::PendingBlock::Input { reader, dev } => {
                    // Same lost-wakeup shape as Keyboard: register first,
                    // then re-check our own stream via try_lock.
                    let mut wq = crate::tasks::INPUT_WAIT_QUEUE.lock();
                    wq.push_back(current);
                    let has_event = crate::drivers::keyboard::input_pending_try(reader, dev)
                        .unwrap_or(false);
                    if has_event {
                        let mut resumed = wq.pop_back().unwrap();
                        drop(wq);
                        resumed.pending_block = None;
                        cpu_slot.as_mut().unwrap().current = Some(resumed);
                        return current_stack_ptr;
                    }
                    drop(wq);
                }
                // Sleep: park on FUTEX_WAIT_QUEUE; the per-tick sweep
                // (wake_expired_waiters) wakes us when now >= deadline.
                // No lost-wakeup check needed — the tick always fires.
                crate::tasks::PendingBlock::Sleep { .. } => {
                    crate::tasks::FUTEX_WAIT_QUEUE.lock().push_back(current);
                }
                // Stopped by terminal job control (SIGTTIN/SIGTTOU). Parks on
                // PIPE_WAIT_QUEUE with the other blocked readers and is
                // released by `wake_parked_for_pid` on SIGCONT. No lost-wakeup
                // re-check here: `park_until_stopped_cleared` re-tests the
                // condition after the wake, so a resume that raced this park
                // is caught rather than slept through.
                crate::tasks::PendingBlock::JobControl => {
                    crate::tasks::PIPE_WAIT_QUEUE.lock().push_back(current);
                }
                // Syncobj: park on SYNCOBJ_WAIT_QUEUE with a lost-wakeup
                // re-check (signal may have landed between the ioctl's
                // fast-path check and this park). Signal path wakes via
                // wake_syncoff_waiters; deadline expiry via the tick sweep.
                crate::tasks::PendingBlock::Syncobj { handle, .. } => {
                    let mut wq = crate::tasks::SYNCOBJ_WAIT_QUEUE.lock();
                    wq.push_back(current);
                    let signaled = crate::drivers::drm::syncobj::SYNCOBJS
                        .try_lock()
                        .map(|m| m.get(&handle).map(|s| s.lock().is_signaled()).unwrap_or(true))
                        .unwrap_or(false);
                    if signaled {
                        let mut resumed = wq.pop_back().unwrap();
                        drop(wq);
                        resumed.pending_block = None;
                        cpu_slot.as_mut().unwrap().current = Some(resumed);
                        return current_stack_ptr;
                    }
                    drop(wq);
                }
            }

            if let Some(mut next) = queue.pop_front() {
                crate::tasks::check_thread(&next, "block-pop");
                if next.stack_ptr == 0 {
                    queue.push_front(next);
                    cpu_slot.as_mut().unwrap().current = None;
                    drop(queue);
                    drop(cpu_slot);
                    unsafe {
                        if crate::arch::x86_64::cpu::read_cr3() != crate::arch::x86_64::cpu::KERNEL_CR3 {
                            crate::arch::x86_64::cpu::write_cr3(crate::arch::x86_64::cpu::KERNEL_CR3);
                        }
                        crate::arch::x86_64::cpu::sti();
                    }
                    cpu_idle_loop(cpu_id as usize);
                }

                // [DSP] block-pop dispatch (gated: fires per wakeup at desktop scale)
                if cfg!(feature = "dsp_verbose") {
                    let npid = next.process.lock().pid;
                    let fctx = unsafe { &*(next.stack_ptr as *const crate::tasks::CpuContext) };
                    let bad = next.stack_ptr >= 0xffff_ff00_0000_0000;
                    crate::dprintln!(
                        "[DSP] block-pop tid={} pid={} sp={:#x} rip={:#x}{}",
                        next.id, npid, next.stack_ptr, fctx.rip,
                        if bad { " !!BADSP" } else { "" }
                    );
                }

                // Switch to next thread
                let next_cr3 = next.process.lock().cr3;
                unsafe {
                    if crate::arch::x86_64::cpu::read_cr3() != next_cr3 {
                        crate::arch::x86_64::cpu::write_cr3(next_cr3);
                    }
                    crate::arch::x86_64::msr::wrmsr(crate::arch::x86_64::msr::IA32_FS_BASE, next.fs_base);
                    crate::arch::x86_64::tss::set_rsp0(
                        &mut cpu_slot.as_mut().unwrap().tss,
                        next.kernel_stack_top,
                    );
                    with_cpu_state(|state| state.kernel_stack = next.syscall_stack_top);
                }
                // Deliver pending signals before returning to user mode.
                // Stash the process for fault-path demand paging: resume may
                // fault on user memory while SCHEDULER/CPUS are held, where
                // current_thread() would self-deadlock (see fault_process).
                crate::tasks::fault_stash_set(&next.process);
                // `None` = nothing left to dispatch (every remaining thread
                // belongs to a stopped process). It must NOT become sp=0:
                // the caller returns this straight to the naked handler, which
                // iretqs to it, so a zero RSP double-faults at address -8.
                let sp = loop {
                    match crate::tasks::resume_with_signals(&mut next) {
                        Ok(sp) => {
                            crate::tasks::fault_stash_clear();
                            break Some(sp);
                        }
                        Err(code) => {
                            if code == STOPPED {
                                // Stopped (SIGTSTP/SIGSTOP). Put it back so it
                                // keeps its saved kernel-stack frame, and try
                                // the next candidate. The per-thread cr3 /
                                // fs_base / rsp0 / syscall_stack setup below
                                // is redone for each candidate, because the
                                // loop can pick a thread from a different
                                // address space than the one just switched to.
                                //
                                // Clone rather than move: `next` is still live
                                // after the loop breaks, and `Thread::clone`
                                // shares the process/FPU Arcs, so the copy in
                                // the run queue and the local are the same
                                // thread.
                                crate::tasks::fault_stash_clear();
                                queue.enqueue(next.clone());
                                match queue.pop_front() {
                                    Some(mut n) => {
                                        if n.stack_ptr == 0 {
                                            queue.push_front(n);
                                            break None; // will idle
                                        }
                                        let cr3 = n.process.lock().cr3;
                                        unsafe {
                                            if crate::arch::x86_64::cpu::read_cr3() != cr3 {
                                                crate::arch::x86_64::cpu::write_cr3(cr3);
                                            }
                                            crate::arch::x86_64::msr::wrmsr(
                                                crate::arch::x86_64::msr::IA32_FS_BASE,
                                                n.fs_base,
                                            );
                                            crate::arch::x86_64::tss::set_rsp0(
                                                &mut cpu_slot.as_mut().unwrap().tss,
                                                n.kernel_stack_top,
                                            );
                                            with_cpu_state(|state| {
                                                state.kernel_stack = n.syscall_stack_top
                                            });
                                        }
                                        next = n;
                                        crate::tasks::fault_stash_set(&next.process);
                                        continue;
                                    }
                                    None => {
                                        crate::tasks::fault_stash_clear();
                                        break None; // will idle
                                    }
                                }
                            }
                            // SIG_DFL termination: drop locks, then exit this thread group.
                            let t = next;
                            drop(queue);
                            drop(cpu_slot);
                            crate::tasks::set_current_thread(Some(t));
                            crate::tasks::exit_group(code);
                        }
                    }
                };
                match sp {
                    Some(new_rsp) => {
                        next.fpu_restore();
                        cpu_slot.as_mut().unwrap().current = Some(next);
                        return new_rsp;
                    }
                    None => {
                        // Nothing runnable left on this core. The stopped
                        // thread is back on the run queue, so parking here is
                        // safe: SIGCONT clears the stopped flag and the idle
                        // loop picks it up again.
                        cpu_slot.as_mut().unwrap().current = None;
                        drop(queue);
                        drop(cpu_slot);
                        unsafe {
                            if crate::arch::x86_64::cpu::read_cr3()
                                != crate::arch::x86_64::cpu::KERNEL_CR3
                            {
                                crate::arch::x86_64::cpu::write_cr3(
                                    crate::arch::x86_64::cpu::KERNEL_CR3,
                                );
                            }
                            crate::arch::x86_64::cpu::sti();
                        }
                        cpu_idle_loop(cpu_id as usize);
                    }
                }
            } else {
                // Nobody else to run — park this CPU into cpu_idle_loop
                cpu_slot.as_mut().unwrap().current = None;
                drop(queue);
                drop(cpu_slot);
                unsafe {
                    if crate::arch::x86_64::cpu::read_cr3() != crate::arch::x86_64::cpu::KERNEL_CR3 {
                        crate::arch::x86_64::cpu::write_cr3(crate::arch::x86_64::cpu::KERNEL_CR3);
                    }
                    crate::arch::x86_64::cpu::sti();
                }
                cpu_idle_loop(cpu_id as usize);
            }
        } else {
            // Normal timer tick — CFS preemption
            let proc = current.process.lock();
            let is_zombie = proc.status == crate::tasks::ProcessStatus::Zombie;
            let curr_id = current.id;
            drop(proc);
            if !is_zombie {
                queue.enqueue(current);
            }

            // Pick-and-dispatch loop.
            //
            // A candidate can turn out to be stopped (SIGTSTP/SIGSTOP) at
            // resume_with_signals time. That must NOT re-enter this function:
            // `queue` (SCHEDULER) and `cpu_slot` (CPUS[k]) are live across the
            // body, so a recursive call re-locks both non-reentrant spinlocks
            // on the same CPU -- an instant deadlock. This used to be exactly
            // that recursive call, and it fired on *every* same-thread tick
            // (not just stopped ones), which is why boot never reached
            // userspace. Loop here instead: put the stopped thread back and
            // pick the next candidate while the locks are already held.
            loop {
                let mut next = match queue.pick_next() {
                    Some(n) => n,
                    None => {
                        // Nothing runnable. `current` was enqueued above
                        // unless it is a zombie, so `None` means either the
                        // process died or every queued thread belongs to a
                        // stopped process (pick_next skips those). Either way
                        // there is nothing to iretq into -- returning
                        // `current_stack_ptr` here would resume a thread the
                        // scheduler just refused to hand out. Park instead.
                        cpu_slot.as_mut().unwrap().current = None;
                        drop(queue);
                        drop(cpu_slot);
                        unsafe {
                            if crate::arch::x86_64::cpu::read_cr3()
                                != crate::arch::x86_64::cpu::KERNEL_CR3
                            {
                                crate::arch::x86_64::cpu::write_cr3(
                                    crate::arch::x86_64::cpu::KERNEL_CR3,
                                );
                            }
                            crate::arch::x86_64::cpu::sti();
                        }
                        cpu_idle_loop(cpu_id as usize);
                    }
                };
                crate::tasks::check_thread(&next, "cfs-pop");
                if next.stack_ptr == 0 {
                    // Invalid/uninitialized stack pointer — put back and keep running
                    queue.push_front(next);
                    return current_stack_ptr;
                }

                // If the scheduler picked the exact same thread, avoid context switch overhead
                if next.id == curr_id && !is_zombie {
                    // Deliver pending signals on the current thread's own frame.
                    // `next` IS that thread (it was just popped from the queue we
                    // enqueued `current` into, and `current.stack_ptr` was set to
                    // `current_stack_ptr` above), so resume it directly.
                    //
                    // Must NOT call current_thread() here — cpu_slot is already held.
                    crate::tasks::fault_stash_set(&next.process);
                    let r = crate::tasks::resume_with_signals(&mut next);
                    crate::tasks::fault_stash_clear();
                    match r {
                        Ok(sp) => {
                            next.fpu_restore();
                            cpu_slot.as_mut().unwrap().current = Some(next);
                            return sp;
                        }
                        // Stopped: it stays in the run queue but pick_next now
                        // skips stopped processes, so it will not be dispatched
                        // again until SIGCONT clears the flag.
                        Err(STOPPED) => {
                            queue.enqueue(next);
                            continue;
                        }
                        // SIG_DFL termination: drop locks, then exit this thread group.
                        Err(code) => {
                            let t = next;
                            drop(queue);
                            drop(cpu_slot);
                            crate::tasks::set_current_thread(Some(t));
                            crate::tasks::exit_group(code);
                        }
                    }
                }

                // [DSP] cfs-switch dispatch diagnostics (dsp_verbose only).
                #[cfg(feature = "dsp_verbose")]
                {
                    let npid = next.process.lock().pid;
                    let fctx = unsafe { &*(next.stack_ptr as *const crate::tasks::CpuContext) };
                    let bad = next.stack_ptr >= 0xffff_ff00_0000_0000;
                    crate::dprintln!(
                        "[DSP] cfs tid={} pid={} sp={:#x} rip={:#x} rax={:#x} rbp={:#x} rsp={:#x} rcx={:#x} rflags={:#x}{}",
                        next.id, npid, next.stack_ptr, fctx.rip, fctx.rax, fctx.rbp, fctx.rsp, fctx.rcx, fctx.rflags,
                        if bad { " !!BADSP" } else { "" }
                    );
                }

                let next_cr3 = next.process.lock().cr3;
                unsafe {
                    if crate::arch::x86_64::cpu::read_cr3() != next_cr3 {
                        crate::arch::x86_64::cpu::write_cr3(next_cr3);
                    }
                    crate::arch::x86_64::msr::wrmsr(crate::arch::x86_64::msr::IA32_FS_BASE, next.fs_base);
                    crate::arch::x86_64::tss::set_rsp0(
                        &mut cpu_slot.as_mut().unwrap().tss,
                        next.kernel_stack_top,
                    );
                    let cur_rsp0 = core::ptr::addr_of!(cpu_slot.as_ref().unwrap().tss.previlege_stack_table[0]).read_unaligned();
                    let tss_ptr = core::ptr::addr_of!(cpu_slot.as_ref().unwrap().tss) as u64;
                    if cfg!(feature = "dsp_verbose") {
                        crate::dprintln!(
                            "[RSP0W] cfs tid={} wrote={:#x} readback={:#x} tss_addr={:#x}",
                            next.id,
                            next.kernel_stack_top,
                            cur_rsp0,
                            tss_ptr
                        );
                    }
                    with_cpu_state(|state| state.kernel_stack = next.syscall_stack_top);
                }

                // Deliver pending signals before returning to user mode.
                // Stash for fault-path demand paging (see fault_process).
                crate::tasks::fault_stash_set(&next.process);
                let sp = match crate::tasks::resume_with_signals(&mut next) {
                    Ok(sp) => {
                        crate::tasks::fault_stash_clear();
                        sp
                    }
                    // Stopped before it ever ran again. Put it back so it keeps
                    // its saved frame, and try the next candidate.
                    Err(STOPPED) => {
                        crate::tasks::fault_stash_clear();
                        queue.enqueue(next);
                        continue;
                    }
                    Err(code) => {
                        // Stash stays set for exit's clear_child_tid write;
                        // exit_group clears it afterwards.
                        let t = next;
                        drop(queue);
                        drop(cpu_slot);
                        crate::tasks::set_current_thread(Some(t));
                        crate::tasks::exit_group(code);
                    }
                };

                let new_rsp = sp;
                next.fpu_restore();
                cpu_slot.as_mut().unwrap().current = Some(next);
                return new_rsp;
            }
        }
    } else {
        // No thread on this core. This is normal when a CPU is idle waiting for IRQs.
    }

    current_stack_ptr
}

/// Shared idle body used by both APs and the BSP when they have no current
/// thread.  Parks the core with `hlt` until the LAPIC timer (or any other
/// interrupt) wakes it, then re-checks the scheduler queue.
///
/// ## Lost-wakeup fix
///
/// The old design snapped `RUNNABLE_COUNT` outside the scheduler lock:
///
/// ```text
/// let last = RUNNABLE_COUNT.load();          // (1) snapshot
/// let next = SCHEDULER.lock().pop_front();   // (2) pop attempt
/// // --- thread pushed here, count bumped ---
/// while RUNNABLE_COUNT.load() == last { hlt; }  // (3) races (2)→(3) window
/// ```
///
/// A thread pushed between (2) and (3) sets `RUNNABLE_COUNT = last+1`.  The
/// `while` comparison breaks on the *first poll after hlt*, so the wakeup is
/// not permanently lost — but there is still a window where the core sleeps
/// one timer tick unnecessarily and only wakes on the next interrupt.
///
/// The robust fix: take the lock, check empty, and only *then* record the
/// count baseline used to detect the next push.  This way:
///
/// * If the queue is non-empty → dispatch immediately (no sleep at all).
/// * If the queue is empty → baseline is recorded under the lock, so any
///   push that happens *after* we release the lock will either:
///   (a) increment RUNNABLE_COUNT above baseline → `hlt` is retired by the
///       next interrupt and the while-condition breaks, or
///   (b) happen before we enter `hlt` → the while-check fires and we skip hlt.
///
/// Either way the core picks up the work within one interrupt latency (~1 ms
/// LAPIC timer) rather than potentially sleeping forever.
///
/// Naked entry: switches RSP to this CPU's private idle stack before running
/// any Rust. Parking paths arrive on a parked thread's kernel stack; idling
/// there while another CPU resumes that thread shares one stack between two
/// live owners (mutual frame smashing → garbage RIPs, skipped swapgs, wild
/// returns). The private stack makes the idler disjoint by construction.
/// Established: smp.rs cpu_idle_loop (SMP mywima-era corruption).
#[unsafe(naked)]
pub extern "C" fn cpu_idle_loop(_my_id: usize) -> ! {
    core::arch::naked_asm!(
        "call {cpu_id}",
        "mov rdi, rax",
        "call {idle_top}",
        "mov rsp, rax",
        "call {cpu_id2}",
        "mov rdi, rax",
        "call {inner}",
        cpu_id = sym current_cpu_id_for_idle,
        idle_top = sym idle_stack_top_for,
        cpu_id2 = sym current_cpu_id_for_idle,
        inner = sym cpu_idle_loop_inner,
    );
}

/// current_cpu_id callable from the naked idle entry (newtype wrapper keeps
/// the naked `call` target a plain SysV symbol).
extern "C" fn current_cpu_id_for_idle() -> u64 {
    current_cpu_id() as u64
}

/// idle_stack_top for `cpu`. Lock-free: called with a live (borrowed) stack before switching RSP.
extern "C" fn idle_stack_top_for(cpu: u64) -> u64 {
    IDLE_STACK_TOPS[cpu as usize].load(Ordering::Relaxed)
}

pub fn cpu_idle_loop_inner(my_id: usize) -> ! {
    loop {
        // IRQs off for the whole body: with IF=1 a timer tick nesting inside
        // (e.g. while SCHEDULER/XHCI/WORK locks are held below) self-deadlocks
        // this core on the non-reentrant locks. sti() again before hlt/worker
        // continue further down; dispatch paths diverge with IF=0 as required.
        crate::arch::x86_64::cpu::cli();
        // Take the lock first so that the empty-queue check and the baseline
        // snapshot are atomic with respect to producers.
        let baseline = loop {
            let mut q = crate::tasks::SCHEDULER.lock();
            let t = match q.pop_front() {
                Some(t) => t,
                None => break RUNNABLE_COUNT.load(Ordering::Acquire),
            };
            if t.stack_ptr == 0 {
                q.push_front(t);
                break RUNNABLE_COUNT.load(Ordering::Acquire);
            }
            // Unlock before dispatching.
            drop(q);
            crate::tasks::check_thread(&t, "idle-pop");
            if run_thread_on_core(my_id, t) {
                // iretq'd into the thread; returns here only after a timer
                // tick parks it back in SCHEDULER. Re-scan from the top.
                continue;
            }
            // The thread turned out to be stopped and has been re-queued by
            // run_thread_on_core. Keep scanning: the queue may still hold
            // runnable threads, and `baseline` is only meaningful once we
            // settle on one to park against.
        };

        // Check if there are background worker tasks to process
        if crate::worker::has_pending_work() {
            crate::worker::run_worker_tasks();
            crate::arch::x86_64::cpu::sti();
            continue;
        }

        // ponytail: deferred USB hotplug — scan_hotplug (timer IRQ) only sets
        // HOTPLUG_DIRTY; the alloc/broadcast/MMIO teardown runs here, process
        // context with no locks held (AGENTS.md §1-2). The flag check is one
        // atomic; the LAPIC tick wakes this loop every ~1ms anyway.
        crate::drivers::usb::xhci::service_hotplug();
        crate::drivers::drm::kms::check_hotplug();

        // Enable interrupts so that hlt can retire, then park.
        // The LAPIC timer fires every ~1 ms, guaranteeing we re-check the
        // queue even if no other wakeup IPI is delivered.
        crate::arch::x86_64::cpu::sti();
        while RUNNABLE_COUNT.load(Ordering::Acquire) == baseline {
            unsafe { core::arch::asm!("hlt"); }
        }
        // An interrupt changed RUNNABLE_COUNT; loop to re-check the queue.
    }
}

/// AP-specific entry point.  Delegates to the shared `cpu_idle_loop`.
fn ap_idle_loop() -> ! {
    let my_id = current_cpu_id() as usize;
    cpu_idle_loop(my_id)
}

/// Switches AP `my_id` into thread `t` (mirror of the BSP timer switch body for
/// the runnable path). Returns when `t` is preempted (its timer tick parks it
/// back into `SCHEDULER`).
///
/// `t` was already removed from `SCHEDULER` by the caller under the lock, so it
/// is exclusively owned here; installing it into `current` keeps it owned by
/// exactly this core until a later tick moves it back to the queue.
///
/// Returns `false` without dispatching when `t` turned out to be stopped (a
/// stop signal was already pending when the idle loop handed it out). The
/// caller must then re-enter the idle loop. `t` has been put back on the run
/// queue by this function, so it keeps its saved kernel-stack frame.
fn run_thread_on_core(my_id: usize, t: Thread) -> bool {
    // IRQs off BEFORE any lock: callers arrive from the IF=1 idle loop, and a
    // timer tick nesting between CPUS-lock and the switch self-deadlocks this
    // core on the non-reentrant slot lock (nested tick spins forever).
    crate::arch::x86_64::cpu::cli();
    let mut cpu_slot = CPUS[my_id].lock();
    let mut t = t;

    // [DSP] idle-pop dispatch (gated: fires per dispatch at desktop scale)
    if cfg!(feature = "dsp_verbose") {
        let tpid = t.process.lock().pid;
        let fctx = unsafe { &*(t.stack_ptr as *const crate::tasks::CpuContext) };
        let bad = t.stack_ptr >= 0xffff_ff00_0000_0000;
        crate::dprintln!(
            "[DSP] idle-pop tid={} pid={} sp={:#x} rip={:#x}{}",
            t.id, tpid, t.stack_ptr, fctx.rip,
            if bad { " !!BADSP" } else { "" }
        );
    }

    // Switch cr3 / fs-base / rsp0 / syscall stack, then iretq into t.
    // IRQs already off (see fn top); they stay off across the whole switch so
    // no tick can observe a half-installed thread. Matches the IF=0 dispatch
    // paths in timer_tick_on_core.
    // Normalize GS bases for the iretq below. switch_to_stack_and_iretq swaps
    // exactly once for user frames, which pairs a (K,0) arrival (tick-park /
    // syscall / exit paths that bypassed their entry's exit-swap). A fresh
    // idle arrival is (0,K) and must be swapped once to (K,0) first — but a
    // post-park (K,0) arrival (kgs transiently 0) must NOT be swapped again,
    // or the thread's first syscall dies at `mov gs:[8]` (cr2=0x8).
    // Rule: swap iff active == 0 (user leftover). Established: smp.rs
    // run_thread_on_core (the `active != kgs` form misfires on (K,0)).
    unsafe {
        let active = crate::arch::x86_64::msr::rdmsr(crate::arch::x86_64::msr::IA32_GS_BASE);
        if active == 0 {
            core::arch::asm!("swapgs", options(nostack, preserves_flags));
        }
    }
    let t_cr3 = t.process.lock().cr3;
    unsafe {
        if crate::arch::x86_64::cpu::read_cr3() != t_cr3 {
            crate::arch::x86_64::cpu::write_cr3(t_cr3);
        }
        crate::arch::x86_64::msr::wrmsr(
            crate::arch::x86_64::msr::IA32_FS_BASE,
            t.fs_base,
        );
        crate::arch::x86_64::tss::set_rsp0(
            &mut cpu_slot.as_mut().unwrap().tss,
            t.kernel_stack_top,
        );
        if cfg!(feature = "dsp_verbose") {
            let cur_rsp0 = core::ptr::addr_of!(cpu_slot.as_ref().unwrap().tss.previlege_stack_table[0]).read_unaligned();
            let tss_ptr = core::ptr::addr_of!(cpu_slot.as_ref().unwrap().tss) as u64;
            crate::dprintln!(
                "[RSP0W] runc tid={} wrote={:#x} readback={:#x} tss_addr={:#x}",
                t.id,
                t.kernel_stack_top,
                cur_rsp0,
                tss_ptr
            );
        }
        with_cpu_state(|st| st.kernel_stack = t.syscall_stack_top);
    }
    // Stash for fault-path demand paging (see fault_process).
    crate::tasks::fault_stash_set(&t.process);
    let sp = match crate::tasks::resume_with_signals(&mut t) {
        Ok(sp) => {
            crate::tasks::fault_stash_clear();
            sp
        }
        // Stopped before it ever ran again (a stop signal was already pending
        // when the idle loop handed it out). Put it back — it keeps its saved
        // frame and `pick_next` will skip it while stopped — and let the idle
        // loop find the next runnable thread.
        //
        // This must NOT fall through to exit_group: STOPPED is -1, and treating
        // it as a fatal signal killed the process outright instead of stopping
        // it, which surfaced as a double fault once the doomed thread's exit
        // path ran on a process the parent still considered stopped.
        Err(STOPPED) => {
            crate::tasks::fault_stash_clear();
            {
                let mut q = crate::tasks::SCHEDULER.lock();
                q.enqueue(t);
            }
            drop(cpu_slot);
            return false;
        }
        Err(code) => {
            // Stash stays set for exit's clear_child_tid write;
            // exit_current_thread clears it afterwards.
            cpu_slot.as_mut().unwrap().current = Some(t);
            drop(cpu_slot);
            crate::tasks::exit_group(code);
        }
    };
    t.fpu_restore();
    cpu_slot.as_mut().unwrap().current = Some(t);
    drop(cpu_slot);
    // NOTE: do NOT sti() here. IF stays 0 through switch_to_stack_and_iretq;
    // the iretq re-enables interrupts atomically with entering user mode. An
    // sti before iretq opens a kernel-context IF=1 window with (0,K) GS state
    // in which a timer tick can redispatch a parked-in-kernel thread that
    // later faults at `mov rsp, gs:[8]` on its syscall return path.

    assert_no_cpus_leak("run_thread_on_core");
    unsafe {
        crate::tasks::switch_to_stack_and_iretq(sp);
    }
    // Unreachable: switch_to_stack_and_iretq diverges. Present only so the
    // `bool` return type type-checks on the false path above.
}

pub fn assert_no_cpus_leak(at: &str) {
    let cpu_id = current_cpu_id() as u8;
    for (i, slot) in CPUS.iter().enumerate() {
        if slot.is_locked_by_current_cpu() {
            let line = slot.holder_line();
            let rip = slot.holder_rip();
            panic!(
                "LEAKED CPUS[{i}] by CPU {cpu_id} (acquired at line {line}, rip {:#x}) before {at}",
                rip
            );
        }
    }
}
