use alloc::collections::VecDeque;
use shared::boot::boot_info::HHDM_BASE;

use crate::{
    arch::{
        sync::{lazy::Lazy, spinlock::SpinLock},
        x86_64::{
            cpu::{cli, sti},
            gdt,
            idt::{Idt, InterruptStackFrame},
            io::{inb, outb},
            pic::PICS,
            tss,
        },
    },
    drivers::keyboard::Keyboard,
    memory::{
        FRAME_ALLOCATOR,
        space::cow_write_fault,
    },
    println,
};

#[inline]
pub fn interrupts_enabled() -> bool {
    let rflags: u64;
    unsafe {
        // pushfq pushes the RFLAGS register to the stack,
        // pop pulls it into our variable.
        core::arch::asm!(
            "pushfq",
            "pop {}",
            out(reg) rflags,
            options(nomem, preserves_flags)
        );
    }
    // Check if bit 9 is set (0x200)
    (rflags & (1 << 9)) != 0
}

#[inline]
pub fn disable_interrupts() {
    cli();
}

#[inline]
pub fn enable_interrupts() {
    sti();
}

pub static STDIN_BUFFER: Lazy<SpinLock<VecDeque<u8>>> =
    Lazy::new(|| SpinLock::new(VecDeque::new()));

pub const PIC_1_OFFSET: u8 = 32;
pub const TIMER_INTERRUPT_ID: u8 = PIC_1_OFFSET;
pub const KEYBOARD_INTERRUPT_ID: u8 = PIC_1_OFFSET + 1;
pub const COM1_INTERRUPT_ID: u8 = PIC_1_OFFSET + 4;
pub const MOUSE_INTERRUPT_ID: u8 = PIC_1_OFFSET + 12;
pub const XHCI_INTERRUPT_ID: u8 = 0x40;
pub const YIELD_INTERRUPT_ID: u8 = 0x81;

pub static IDT: Lazy<Idt> = Lazy::new(|| {
    let mut idt = Idt::new();
    let cs = gdt::KERNEL_CODE_SELECTOR;

    idt.entries[3].set_handler(breakpoint_handler as *const () as u64, cs);
    idt.entries[6].set_handler(invalid_opcode_handler as *const () as u64, cs);
    idt.entries[13].set_handler(gpf_handler as *const () as u64, cs);
    idt.entries[14].set_handler(page_fault_handler as *const () as u64, cs);
    idt.entries[0].set_handler(divide_by_zero_handler as *const () as u64, cs);
    // NMI (vector 2): hang dump (see nmi_handler). Always installed;
    // triggered by Ctrl+Alt+F10 self-IPI, never by QEMU inject-nmi (broken).
    idt.entries[2].set_handler(nmi_handler as *const () as u64, cs);

    // Setup Double Fault handler to use our new safe stack
    idt.entries[8].set_handler(double_fault_handler as *const () as u64, cs);
    // This is the new line: tell the IDT to use the IST for this specific exception!
    idt.entries[8].set_ist(tss::DOUBLE_FAULT_IST_INDEX as u8 + 1); // CPU expects 1-based index (1-7)

    // Timer, keyboard, serial, and yield interrupt handlers
    idt.entries[TIMER_INTERRUPT_ID as usize]
        .set_handler(crate::time::timer_interrupt_handler as *const () as u64, cs);
    idt.entries[YIELD_INTERRUPT_ID as usize]
        .set_handler(crate::time::yield_interrupt_handler as *const () as u64, cs);
    idt.entries[KEYBOARD_INTERRUPT_ID as usize]
        .set_handler(keyboard_interrupt_handler as *const () as u64, cs);
    idt.entries[COM1_INTERRUPT_ID as usize]
        .set_handler(com1_interrupt_handler as *const () as u64, cs);
    idt.entries[MOUSE_INTERRUPT_ID as usize]
        .set_handler(mouse_interrupt_handler as *const () as u64, cs);
    idt.entries[XHCI_INTERRUPT_ID as usize]
        .set_handler(xhci_interrupt_handler as *const () as u64, cs);

    idt
});

// Global thread-safe Keyboard state
pub static KEYBOARD: Lazy<SpinLock<Keyboard>> = Lazy::new(|| SpinLock::new(Keyboard::new()));

pub fn init() {
    unsafe { IDT.load() }
}

extern "x86-interrupt" fn breakpoint_handler(stack_frame: InterruptStackFrame) {
    println!("EXCEPTION: BREAKPOINT\n{:#?}", stack_frame);
}

extern "x86-interrupt" fn divide_by_zero_handler(stack_frame: InterruptStackFrame) {
    panic!("EXCEPTION: DIVIDE BY ZERO\n{:#?}", stack_frame);
}

// ── NMI hang dump ────────────────────────────────────────────────────────────
// ponytail: hang observability (per-thread last-syscall). Trigger is
// Ctrl+Alt+F10 via input_push → lapic_self_nmi (QMP `send-key` reaches it
// even with userspace dead; QEMU 11 inject-nmi is accepted but never
// delivered, so don't rely on it). Prints every CPU current + runqueue +
// wait queues as pid/tid, current-or-parked syscall, and park reason.
// try_lock + direct COM1 port writes only: a wedged holder shows as
// <busy>, never deadlocks the dump itself (safe mid-anything, including
// mid-print — nested NMIs bail via the guard).
static IN_NMI_DUMP: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

const NMI_COM1_DATA: u16 = 0x3F8;
const NMI_COM1_LSR: u16 = 0x3FD;
const NMI_LSR_EMPTY: u8 = 0x20;

fn nmi_putc(b: u8) {
    // Bounded LSR poll: never hang the NMI on a wedged UART.
    for _ in 0..1_000_000 {
        let lsr = unsafe { inb(NMI_COM1_LSR) };
        if lsr & NMI_LSR_EMPTY != 0 {
            break;
        }
        core::hint::spin_loop();
    }
    unsafe { outb(NMI_COM1_DATA, b) };
}

struct NmiOut {
    buf: [u8; 224],
    len: usize,
}

impl NmiOut {
    const fn new() -> Self {
        Self { buf: [0; 224], len: 0 }
    }
    fn push(&mut self, b: u8) {
        if self.len >= self.buf.len() {
            self.flush();
        }
        if self.len < self.buf.len() {
            self.buf[self.len] = b;
            self.len += 1;
        }
    }
    fn flush(&mut self) {
        for i in 0..self.len {
            nmi_putc(self.buf[i]);
        }
        self.len = 0;
    }
}

impl core::fmt::Write for NmiOut {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for b in s.bytes() {
            if b == b'\n' {
                self.push(b'\r');
            }
            self.push(b);
        }
        Ok(())
    }
}

fn nmi_emit(args: core::fmt::Arguments) {
    use core::fmt::Write;
    let mut o = NmiOut::new();
    let _ = o.write_fmt(args);
    o.flush();
}

fn nmi_block_tag(pb: &Option<crate::tasks::PendingBlock>) -> &'static str {
    match pb {
        None => "run",
        Some(crate::tasks::PendingBlock::Keyboard) => "key",
        Some(crate::tasks::PendingBlock::ChildWait) => "child",
        Some(crate::tasks::PendingBlock::PipeWait) => "pipe",
        Some(crate::tasks::PendingBlock::Futex { .. }) => "futex",
        Some(crate::tasks::PendingBlock::EpollWait { .. }) => "epoll",
        Some(crate::tasks::PendingBlock::Input { .. }) => "input",
        Some(crate::tasks::PendingBlock::Sleep { .. }) => "sleep",
        Some(crate::tasks::PendingBlock::Syncobj { .. }) => "syncobj",
        Some(crate::tasks::PendingBlock::JobControl) => "jobctl",
    }
}

fn nmi_thread(kind: &str, idx: usize, t: &crate::tasks::Thread) {
    let pid = t.process.try_lock().map(|p| p.pid as i64).unwrap_or(-1);
    let user_rip = if t.stack_ptr != 0 {
        unsafe { (*(t.stack_ptr as *const crate::tasks::CpuContext)).rip }
    } else {
        0
    };
    let extra = match &t.pending_block {
        Some(crate::tasks::PendingBlock::Futex { uaddr, expected, deadline, pid: fpid }) => {
            alloc::format!(" uaddr={:#x} exp={} dl={} fpid={}", uaddr, expected, deadline, fpid)
        }
        _ => alloc::string::String::new(),
    };
    nmi_emit(format_args!(
        "[nmi] {}:{} tid={} pid={} nr={} rip={:#x} blk={}{}\n",
        kind,
        idx,
        t.id,
        pid,
        t.last_syscall_nr,
        user_rip,
        nmi_block_tag(&t.pending_block),
        extra
    ));
}

fn nmi_threads<'a>(
    tag: &str,
    it: impl Iterator<Item = &'a crate::tasks::Thread>,
    total: usize,
) {
    let mut n = 0usize;
    for t in it {
        if n >= 12 {
            break;
        }
        nmi_thread(tag, n, t);
        n += 1;
    }
    if total > n {
        nmi_emit(format_args!("[nmi] {}: ...+{} more\n", tag, total - n));
    }
}

extern "x86-interrupt" fn nmi_handler(stack_frame: InterruptStackFrame) {
    use core::sync::atomic::Ordering;
    if IN_NMI_DUMP.swap(true, Ordering::SeqCst) {
        return; // nested NMI: never re-enter the dump
    }
    // x86-interrupt ABI doesn't swapgs (see page_fault_handler); balance it.
    let from_user = (stack_frame.code_segment & 3) == 3;
    if from_user {
        unsafe { core::arch::asm!("swapgs", options(nostack, preserves_flags)) };
    }
    nmi_emit(format_args!("[nmi] task dump\n"));
    // CPU currents.
    for (k, slot) in crate::smp::CPUS.iter().enumerate() {
        match slot.try_lock() {
            Some(g) => match g.as_ref().and_then(|c| c.current.as_ref()) {
                Some(t) => nmi_thread("cpu", k, t),
                None => nmi_emit(format_args!("[nmi] cpu{}: idle\n", k)),
            },
            None => nmi_emit(format_args!("[nmi] cpu{}: <busy>\n", k)),
        }
    }
    // Runqueue (CFS heap, arbitrary order — diagnostic only).
    match crate::tasks::SCHEDULER.try_lock() {
        Some(q) => nmi_threads("run", q.iter_threads(), q.len()),
        None => nmi_emit(format_args!("[nmi] run: <busy>\n")),
    }
    // Wait queues.
    match crate::tasks::KEYBOARD_WAIT_QUEUE.try_lock() {
        Some(q) => nmi_threads("keybd", q.iter(), q.len()),
        None => nmi_emit(format_args!("[nmi] keybd: <busy>\n")),
    }
    match crate::tasks::CHILD_WAIT_QUEUE.try_lock() {
        Some(q) => nmi_threads("child", q.iter(), q.len()),
        None => nmi_emit(format_args!("[nmi] child: <busy>\n")),
    }
    match crate::tasks::PIPE_WAIT_QUEUE.try_lock() {
        Some(q) => nmi_threads("pipe", q.iter(), q.len()),
        None => nmi_emit(format_args!("[nmi] pipe: <busy>\n")),
    }
    match crate::tasks::FUTEX_WAIT_QUEUE.try_lock() {
        Some(q) => nmi_threads("futex", q.iter(), q.len()),
        None => nmi_emit(format_args!("[nmi] futex: <busy>\n")),
    }
    match crate::tasks::INPUT_WAIT_QUEUE.try_lock() {
        Some(q) => nmi_threads("input", q.iter(), q.len()),
        None => nmi_emit(format_args!("[nmi] input: <busy>\n")),
    }
    if from_user {
        unsafe { core::arch::asm!("swapgs", options(nostack, preserves_flags)) };
    }
    IN_NMI_DUMP.store(false, Ordering::SeqCst);
}

// Note: Page faults push an extra 'error_code' to the stack.
extern "x86-interrupt" fn page_fault_handler(stack_frame: InterruptStackFrame, error_code: u64) {
    // ponytail: x86-interrupt ABI doesn't swapgs; we must do it ourselves on
    // user-mode entry/exit, else KERNEL_GS_BASE gets clobbered on the next syscall.
    let from_user = (stack_frame.code_segment & 3) == 3;
    if from_user {
        unsafe { core::arch::asm!("swapgs", options(nostack, preserves_flags)) };
    }

    let cr2: u64;
    unsafe {
        core::arch::asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack, preserves_flags));
    }
    // ponytail: no per-fault trace (22K+ debugcon writes/boot); unresolvable faults dump below.

    // Check for Not Present (bit 0 is 0)
    if (error_code & 1) == 0 {
        let is_write = (error_code & (1 << 1)) != 0;
        // Try file-backed demand paging first (mmap of a regular file).
        if crate::memory::filemmap::dispatch_demand_fault(cr2, is_write) {
            if from_user {
                unsafe { core::arch::asm!("swapgs", options(nostack, preserves_flags)) };
            }
            return;
        }
        if crate::memory::space::stack_demand_fault(cr2, HHDM_BASE.0) {
            if from_user {
                unsafe { core::arch::asm!("swapgs", options(nostack, preserves_flags)) };
            }
            return; // Dynamically allocated new stack page
        }
        let mut alloc = FRAME_ALLOCATOR.lock();
        if crate::memory::space::demand_paging_fault_ext(cr2, is_write, &mut *alloc, HHDM_BASE.0) {
            if from_user {
                unsafe { core::arch::asm!("swapgs", options(nostack, preserves_flags)) };
            }
            return; // Added the demanded page
        }
    }

    // Write fault (error bit 1) to a present page? Try COW first.
    if (error_code & (1 << 1)) != 0 {
        if cow_write_fault(cr2, HHDM_BASE.0) {
            if from_user {
                unsafe { core::arch::asm!("swapgs", options(nostack, preserves_flags)) };
            }
            return;
        }
    }

    // Unresolvable fault - deliver SIGSEGV to user mode
    if from_user {
        let (vmas_dump, cur_cr3, pid, sigactions, sigmask) = crate::tasks::fault_process().map(|p| {
            let proc = p.lock();
            // Try to get thread's sigmask via CPUS try_lock
            let sigmask = crate::smp::CPUS[crate::smp::current_cpu_id() as usize].try_lock()
                .and_then(|slot| slot.as_ref()?.current.as_ref().map(|t| t.sigmask))
                .unwrap_or(0);
            (
                proc.vmas.vmas.clone(),
                proc.cr3,
                proc.pid,
                proc.sigactions,
                sigmask,
            )
        }).unwrap_or((alloc::vec::Vec::new(), shared::memory::address::PhysAddr(0), 0, [crate::arch::x86_64::abi::SigAction::default(); 32], 0));
        // For non-sh null derefs (e.g., ls), don't spam verbose VMA dump; just exit quietly
        if cr2 == 0x0 {
            crate::println!("[NULL-DEREF] pid={} rip={:#x} -> SIGSEGV", pid, stack_frame.instruction_pointer);
            crate::tasks::exit_current_thread(139);
        }
        crate::println!(
            "UNRESOLVABLE USER PAGE FAULT: pid={} cr2={:#x}, rip={:#x}, err={:b}, cr3={:#x}, sigmask={:#x}",
            pid, cr2, stack_frame.instruction_pointer, error_code, cur_cr3.0, sigmask
        );
        for (i, a) in sigactions.iter().enumerate() {
            if a.sa_handler != 0 && a.sa_handler != 1 {
                crate::dprintln!("  sigaction[{}] handler={:#x} restorer={:#x}", i + 1, a.sa_handler, a.sa_restorer);
            }
        }
        crate::dprintln!(
            "  FAULT err={:#x} (NP={} WR={} US={}) cr2={:#x} rip={:#x} pid={}",
            error_code,
            error_code & 1,
            (error_code >> 1) & 1,
            (error_code >> 2) & 1,
            cr2,
            stack_frame.instruction_pointer,
            pid
        );
        // Everything below reads the page tables through `cur_cr3`, which comes
        // from the *process record*, not from the CPU. Under SMP those can
        // differ (the fault may have been taken on an AP partway through a
        // dispatch), in which case every PTE, VMA and walk printed here
        // describes a different address space than the one that actually
        // faulted -- and would look perfectly healthy while explaining nothing.
        // So compare them explicitly.
        {
            let cpu_cr3 = crate::arch::x86_64::cpu::read_cr3();
            crate::dprintln!(
                "  cr3: cpu={:#x} proc={:#x} {}",
                cpu_cr3.0,
                cur_cr3.0,
                if cpu_cr3 == cur_cr3 {
                    "MATCH"
                } else {
                    "*** MISMATCH: dump below describes the WRONG address space ***"
                }
            );
        }
        // Raw PTE at cr2. The error code alone cannot distinguish "not present"
        // from "present but not writable" (bit0==1 means the latter), and the
        // two demand completely different fixes. Read the leaf and print every
        // flag bit that is set.
        if let Some((f, phys)) = crate::memory::space::probe_leaf_pte(cur_cr3, cr2) {
            crate::dprintln!(
                "  pte[{:#x}] = {:#x}  P={} W={} U={} NX={} COW={} DEMAND={} FILECACHE={} MMIO={} HUGE={} phys={:#x}",
                cr2 & !0xfff,
                f,
                f & 1 != 0,
                f & 2 != 0,
                f & 4 != 0,
                f & (1 << 63) != 0,
                f & (1 << 9) != 0,
                f & (1 << 10) != 0,
                f & (1 << 11) != 0,
                f & (1 << 52) != 0,
                f & (1 << 7) != 0,
                phys
            );
        } else {
            crate::dprintln!("  pte[{:#x}] = <no leaf: walk failed>", cr2 & !0xfff);
        }
        for v in &vmas_dump {
            if cr2 >= v.start && cr2 < v.end {
                crate::dprintln!(
                    "  covering vma: {:#x}..{:#x} prot={:#x} flags={:#x} kind={:?}  (prot bits R={} W={})",
                    v.start, v.end, v.prot, v.flags, v.kind,
                    v.prot & 1 != 0,
                    v.prot & 2 != 0
                );
            }
        }
        // Is cr2 a *hole punched inside* an otherwise-mapped range, or a page
        // that was never mapped at all? The neighbours decide: if cr2-4K and
        // cr2+4K both translate while cr2 does not, something unmapped exactly
        // this page after mmap. If the neighbours are missing too, the range
        // was never populated (or was unmapped wholesale).
        {
            let m = crate::memory::space::mapper_for(cur_cr3);
            for (label, a) in [
                ("cr2-8K", cr2.wrapping_sub(0x2000)),
                ("cr2-4K", cr2.wrapping_sub(0x1000)),
                ("cr2    ", cr2),
                ("cr2+4K", cr2.wrapping_add(0x1000)),
                ("cr2+8K", cr2.wrapping_add(0x2000)),
            ] {
                match m.translate(shared::memory::address::VirtAddr(a & !0xfffu64)) {
                    Some(p) => crate::dprintln!(
                        "  walk {} {:#x} -> phys {:#x}",
                        label,
                        a & !0xfffu64,
                        p.as_u64()
                    ),
                    None => crate::dprintln!("  walk {} {:#x} -> ABSENT", label, a & !0xfffu64),
                }
            }
        }
        for v in &vmas_dump {
            crate::dprintln!("  vma: {:#x}..{:#x} prot={:#x} flags={:#x} kind={:?}", v.start, v.end, v.prot, v.flags, v.kind);
        }
        let dump = |addr: u64| unsafe {
            if !crate::memory::space::user_pages_mapped(addr, 24, false) {
                crate::dprintln!("  mem[{:#x}] = <unmapped>", addr);
                return;
            }
            let mut b = [0u8; 24];
            for i in 0..24 { b[i] = *((addr + i as u64) as *const u8); }
            crate::dprintln!("  mem[{:#x}] = {:02x?}", addr, b);
        };
        dump(stack_frame.instruction_pointer & !0xfu64);
        dump(cr2 & !0xfu64);
        let base = 0x5555_5555_4000u64;
        if crate::memory::space::user_pages_mapped(base + 0x3d8fc0, 0x300, false) {
            for off in [0x3d8fd0, 0x3d9000, 0x3da6c0, 0x3da6d0] {
                crate::dprintln!("  data[{:#x}] = {:#x}", base + off, unsafe { *((base + off) as *const u64) });
            }
        }
        let usp = stack_frame.stack_pointer;
        crate::dprintln!("  user rsp={:#x}", usp);
        if crate::memory::space::user_pages_mapped(usp, 128, false) {
            unsafe {
                let mut b = [0u8; 128];
                for i in 0..128 { b[i] = *((usp + i as u64) as *const u8); }
                for (i, chunk) in b.chunks(8).enumerate() {
                    crate::dprintln!("    [{:#x}] = {:#x}", usp + (i * 8) as u64, u64::from_le_bytes(chunk.try_into().unwrap()));
                }
            }
        }
        // GPR dump from the CpuContext frame on the kernel stack (layout: rax..r15 then iret frame)
        let _kframe = crate::smp::with_cpu_state(|st| st.kernel_stack);
        // The interrupt frame is at the top of the kernel stack; walk back to find the
        // saved GPRs pushed by the PF handler's caller is unreliable — instead read the
        // iret frame via stack_frame and print what we can.
        crate::dprintln!(
            "  iret: rip={:#x} cs={:#x} rflags={:#x} rsp={:#x} ss={:#x}",
            stack_frame.instruction_pointer, stack_frame.code_segment,
            stack_frame.cpu_flags, stack_frame.stack_pointer, stack_frame.stack_segment
        );
        // exit_current_thread -> switch_to_stack_and_iretq does its own swapgs,
        // so we must NOT swapgs here. KERNEL_GS_BASE state will be left as it
        // was on entry (user's gs in KERNEL_GS_BASE, kernel's in active) — wait,
        // we already did swapgs at entry, so active=kernel gs, KERNEL=user gs.
        // switch_to_stack_and_iretq sees user CS on its own frame and swapgs's
        // again, restoring the correct state for return to user.
        crate::tasks::exit_current_thread(139);
    }

    // Mirror the fault to debugcon (locked) so SMP-interleaved serial output
    // and post-panic resets can't hide the faulting CPU/thread/RIP. try_lock:
    // a kernel fault under CPUS-hold must not re-entry-panic here and mask
    // the fault it was meant to diagnose.
    let cpu = crate::smp::current_cpu_id();
    let diag_t = match crate::smp::CPUS[cpu as usize].try_lock() {
        Some(slot) => match slot.as_ref() {
            Some(c) => match c.current.as_ref() {
                Some(t) => Some((t.id, t.process.lock().pid, t.stack_ptr)),
                None => None,
            },
            None => None,
        },
        None => Some((u64::MAX, u64::MAX, u64::MAX)),
    };
    crate::dprintln!(
        "PF-DIAG cpu={} thr={:?} cr2={:#x} err={:b} rip={:#x} cs={:#x} rsp={:#x}",
        cpu, diag_t, cr2, error_code,
        stack_frame.instruction_pointer, stack_frame.code_segment,
        stack_frame.stack_pointer
    );
    // TEMP-DIAG call-chain dump (iret-to-garbage hunt): 16 words below the
    // pre-fault RSP hold the return-address chain naming the faulting caller.
    // Read-only, panic path only.
    {
        let base = stack_frame.stack_pointer.wrapping_sub(64);
        for i in 0..16u64 {
            let a = base + i * 8;
            let v = unsafe { (a as *const u64).read_volatile() };
            crate::dprintln!("  PF-STK [{:#x}] = {:#x}", a, v);
        }
    }
    // TEMP-DIAG ownership snapshot (iret-to-SCHEDULER hunt): list every
    // core's current thread + tid inventory of all queues. A tid appearing
    // twice (two currents, or current+queued) is double ownership.
    // try_lock only: never blocks, never deadlocks the fault path.
    for (k, slot) in crate::smp::CPUS.iter().enumerate() {
        if let Some(s) = slot.try_lock() {
            match s.as_ref() {
                Some(c) => {
                    // TEMP-DIAG TSS check: ring3 IRQs land on TSS.rsp0. If it
                    // disagrees with the installed thread's kernel_stack_top,
                    // IRQs build frames on the wrong stack (forged frames).
                    let rsp0 = c.tss.previlege_stack_table[0];
                    match c.current.as_ref() {
                        Some(t) => crate::dprintln!("  OWN cpu{}: tid={} sp={:#x} rsp0={:#x} kstack={:#x}",
                            k, t.id, t.stack_ptr, rsp0, t.kernel_stack_top),
                        None => crate::dprintln!("  OWN cpu{}: idle rsp0={:#x}", k, rsp0),
                    }
                }
                None => crate::dprintln!("  OWN cpu{}: <uninit>", k),
            }
        } else {
            crate::dprintln!("  OWN cpu{}: <busy>", k);
        }
    }
    {
        use crate::tasks::{
            CHILD_WAIT_QUEUE, FUTEX_WAIT_QUEUE, INPUT_WAIT_QUEUE,
            KEYBOARD_WAIT_QUEUE, PIPE_WAIT_QUEUE, SCHEDULER,
        };
        // TEMP-DIAG duplicate-tid hunt: collect every live tid across all
        // cores and queues; any tid seen twice is double ownership.
        let mut seen: [(u64, u8); 128] = [(u64::MAX, 0); 128];
        let mut seen_n = 0usize;
        let mut note = |tid: u64, where_: u8| {
            for i in 0..seen_n {
                if seen[i].0 == tid {
                    seen[i].1 += 1;
                    if seen[i].1 == 2 {
                        crate::dprintln!("  DUP tid={} also at {}", tid, where_);
                    }
                    return;
                }
            }
            if seen_n < seen.len() {
                seen[seen_n] = (tid, 1);
                seen_n += 1;
            }
        };
        for (k, slot) in crate::smp::CPUS.iter().enumerate() {
            if let Some(s) = slot.try_lock() {
                if let Some(c) = s.as_ref().and_then(|c| c.current.as_ref()) {
                    note(c.id, 10 + k as u8);
                }
            }
        }
        if let Some(g) = SCHEDULER.try_lock() {
            let v = g.to_vec();
            let mut s = alloc::string::String::new();
            
            for t in v.iter().take(10) {
                let _ = core::fmt::write(&mut s, format_args!("{} ", t.id));
            }
            crate::dprintln!("  OWNQ SCHED: len={} [{}]", v.len(), s);
            for t in v.iter() {
                note(t.id, 20);
            }
        } else {
            crate::dprintln!("  OWNQ SCHED: <busy>");
        }
        let mut dump = |name: &str, code: u8, q: &crate::arch::sync::spinlock::SpinLock<alloc::collections::VecDeque<crate::tasks::Thread>>| {
            if let Some(g) = q.try_lock() {
                let mut s = alloc::string::String::new();
                
                for t in g.iter().take(10) {
                    let _ = core::fmt::write(&mut s, format_args!("{} ", t.id));
                }
                crate::dprintln!("  OWNQ {}: len={} [{}]", name, g.len(), s);
                for t in g.iter() {
                    note(t.id, code);
                }
            } else {
                crate::dprintln!("  OWNQ {}: <busy>", name);
            }
        };
        dump("KEYBD", 21, &KEYBOARD_WAIT_QUEUE);
        dump("CHILD", 22, &CHILD_WAIT_QUEUE);
        dump("PIPE", 23, &PIPE_WAIT_QUEUE);
        dump("FUTEX", 24, &FUTEX_WAIT_QUEUE);
        dump("INPUT", 25, &INPUT_WAIT_QUEUE);
    }
    // TEMP-DIAG dispatch journal dump (iret-to-SCHEDULER hunt): what the
    // switch path was last handed on the faulting CPU. try_lock only.
    if (cpu as usize) < crate::smp::MAX_CPUS {
        if let Some(g) = crate::tasks::DISPATCH_J[cpu as usize].try_lock() {
            for (i, e) in g.iter().enumerate() {
                if e.tid != u64::MAX {
                    crate::dprintln!("  DJ[{}]: tid={} sp={:#x} rip={:#x} top={:#x} in_range={}",
                        i, e.tid, e.sp, e.rip, e.top, e.in_range as u8);
                }
            }
        } else {
            crate::dprintln!("  DJ: <busy>");
        }
    }
    panic!(
        "EXCEPTION: PAGE FAULT\nAccessed Address: {:#x}\nError Code: {:b}\n{:#?}",
        cr2, error_code, stack_frame
    );
}

extern "x86-interrupt" fn double_fault_handler(
    stack_frame: crate::arch::x86_64::idt::InterruptStackFrame,
    _error_code: u64,
) -> ! {
    let cr2: u64;
    unsafe {
        core::arch::asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack, preserves_flags));
    }
    panic!(
        "EXCEPTION: DOUBLE FAULT\nCR2 = {:#x}\n{:#?}",
        cr2, stack_frame
    );
}

extern "x86-interrupt" fn keyboard_interrupt_handler(stack_frame: InterruptStackFrame) {
    let from_user = (stack_frame.code_segment & 3) == 3;
    if from_user {
        unsafe { core::arch::asm!("swapgs", options(nostack, preserves_flags)) };
    }

    let scancode: u8 = unsafe { inb(0x60) };
    crate::dprintln!("[irq1-ps2k] inb={:#02x}", scancode);
    crate::drivers::keyboard::PS2K_IRQ.fetch_add(1, core::sync::atomic::Ordering::Relaxed);

    let mut keyboard = KEYBOARD.lock();
    if let Some(character) = keyboard.process_scancode(scancode) {
        // Route through the line discipline, into the *active* VT's own input
        // queue. This replaces pushing into the single global STDIN_BUFFER:
        // with per-VT queues, VT 2 keeps its half-typed line while VT 1 is
        // displayed, and the discipline does echo, canonical editing and
        // signal characters.
        //
        // Lock order: the tty lock is a leaf here. It is taken, released, and
        // only then is SCHEDULER taken -- the producer pattern the park branch
        // in smp.rs depends on. Any signal the discipline asks for is
        // dispatched after every lock is dropped.
        let action = crate::tty::input_active(character as u8);
        let vt = crate::graphics::vt::active_vt_number();

        {
            let mut sched = crate::tasks::SCHEDULER.lock();
            let mut wait_queue = crate::tasks::KEYBOARD_WAIT_QUEUE.lock();
            while let Some(mut t) = wait_queue.pop_front() {
                t.pending_block = None;
                sched.enqueue_with_boost(t, 4);
            }
            drop(wait_queue);
            drop(sched);
            crate::tasks::wake_pipe_waiters();
        }
        // All locks are dropped; only now may a signal reach PROCESS_TABLE.
        crate::tty::dispatch(action, vt);
    }

    unsafe {
        PICS.lock().notify_end_of_interrupt(KEYBOARD_INTERRUPT_ID);
    }
    crate::arch::x86_64::apic::lapic_eoi();

    if from_user {
        unsafe { core::arch::asm!("swapgs", options(nostack, preserves_flags)) };
    }
}

extern "x86-interrupt" fn mouse_interrupt_handler(stack_frame: InterruptStackFrame) {
    let from_user = (stack_frame.code_segment & 3) == 3;
    if from_user {
        unsafe { core::arch::asm!("swapgs", options(nostack, preserves_flags)) };
    }

    let _got_packet = crate::drivers::ps2_mouse::handle_irq();

    unsafe {
        PICS.lock().notify_end_of_interrupt(MOUSE_INTERRUPT_ID);
    }
    crate::arch::x86_64::apic::lapic_eoi();

    if from_user {
        unsafe { core::arch::asm!("swapgs", options(nostack, preserves_flags)) };
    }
}

extern "x86-interrupt" fn xhci_interrupt_handler(stack_frame: InterruptStackFrame) {
    let from_user = (stack_frame.code_segment & 3) == 3;
    if from_user {
        unsafe { core::arch::asm!("swapgs", options(nostack, preserves_flags)) };
    }

    crate::drivers::usb::xhci::handle_interrupt();

    crate::arch::x86_64::apic::lapic_eoi();

    if from_user {
        unsafe { core::arch::asm!("swapgs", options(nostack, preserves_flags)) };
    }
}

extern "x86-interrupt" fn com1_interrupt_handler(stack_frame: InterruptStackFrame) {
    let from_user = (stack_frame.code_segment & 3) == 3;
    if from_user {
        unsafe { core::arch::asm!("swapgs", options(nostack, preserves_flags)) };
    }

    // Signals the discipline asked for, dispatched after every lock is dropped.
    let mut com1_actions: alloc::vec::Vec<crate::tty::Action> = alloc::vec::Vec::new();

    // Drain the UART FIFO
    while (unsafe { inb(0x3F8 + 5) } & 1) != 0 {
        let mut byte: u8 = unsafe { inb(0x3F8) };
        if byte == b'\r' {
            byte = b'\n';
        }
        crate::dprint!("[com1] byte={:x}\n", byte);

        // Through the line discipline like the keyboard, so serial input
        // behaves identically (echo, canonical editing, ^C). COM1 is VT 1 on
        // this configuration -- the serial console is the first virtual
        // terminal, matching the kernel console's own numbering.
        com1_actions.push(crate::tty::input_byte(1, byte));
    }

    let mut sched = crate::tasks::SCHEDULER.lock();
    let mut wait_queue = crate::tasks::KEYBOARD_WAIT_QUEUE.lock();
    crate::dprint!("[com1] wq={}\n", wait_queue.len());
    while let Some(mut t) = wait_queue.pop_front() {
        crate::dprint!("[com1] waking {}\n", t.id);
        t.pending_block = None;
        sched.enqueue_with_boost(t, 4);
    }
    drop(wait_queue);
    drop(sched);
    crate::tasks::wake_pipe_waiters();
    for a in com1_actions {
        crate::tty::dispatch(a, 1);
    }

    unsafe {
        PICS.lock().notify_end_of_interrupt(COM1_INTERRUPT_ID);
    }
    crate::arch::x86_64::apic::lapic_eoi();

    if from_user {
        unsafe { core::arch::asm!("swapgs", options(nostack, preserves_flags)) };
    }
}

extern "x86-interrupt" fn gpf_handler(
    stack_frame: crate::arch::x86_64::idt::InterruptStackFrame,
    error_code: u64,
) {
    let from_user = (stack_frame.code_segment & 3) == 3;
    if from_user {
        unsafe { core::arch::asm!("swapgs", options(nostack, preserves_flags)) };
        crate::println!(
            "USER GPF: rip={:#x}, cs={:#x}, rflags={:#x}, rsp={:#x}, ss={:#x}, err={:#x}",
            stack_frame.instruction_pointer, stack_frame.code_segment, stack_frame.cpu_flags,
            stack_frame.stack_pointer, stack_frame.stack_segment, error_code
        );
        crate::tasks::exit_current_thread(128 + 11);
    }
    // ponytail: never current_thread() here — CPUS[cpu] may already be held
    // (e.g. GPF inside timer_tick_on_core); that re-entry-panics and masks
    // the real fault. try_lock degrades to pid=0 instead of dying.
    let cpu = crate::smp::current_cpu_id();
    let pid = match crate::smp::CPUS[cpu as usize].try_lock() {
        Some(slot) => {
            let pid_opt = match slot.as_ref() {
                Some(c) => match c.current.as_ref() {
                    Some(t) => Some(t.process.lock().pid),
                    None => None,
                },
                None => None,
            };
            pid_opt.unwrap_or(0)
        }
        None => 0,
    };
    let cr3 = crate::arch::x86_64::cpu::read_cr3();
    crate::dprintln!(
        "EXCEPTION: GENERAL PROTECTION FAULT cpu={} pid={} cr3={:#x}\nError Code: {:#x}\n{:#?}",
        cpu, pid, cr3.0, error_code, stack_frame
    );
    panic!("GPF halted");
}

extern "x86-interrupt" fn invalid_opcode_handler(
    stack_frame: crate::arch::x86_64::idt::InterruptStackFrame,
) {
    panic!("EXCEPTION: INVALID OPCODE\n{:#?}", stack_frame);
}

/// Called from the PS/2 keyboard driver when Ctrl+C is pressed. Sets SIGINT
/// pending on the current thread's process so it is delivered when the thread
/// next returns to user mode.
pub fn signal_ctrl_c() {
    let mut t = match crate::tasks::current_thread() {
        Some(t) => t,
        None => return,
    };
    t.pending |= 1u64 << (2 - 1); // SIGINT = signal 2
    crate::tasks::set_current_thread(Some(t));

    // Wake the thread if it is blocked reading the keyboard, so the signal can
    // be delivered rather than waiting indefinitely on input.
    crate::tasks::wake_keyboard_waiters();
}

pub fn signal_ctrl_c_from_isr() {
    // This ISR may be called with CPUS[cpu] already held (e.g., timer tick).
    // Must never call current_thread() here, as it would re-lock CPUS[cpu] and deadlock.
    // Instead, take the local slot without re-locking.
    let cpu_id = crate::smp::current_cpu_id() as usize;
    let slot = &crate::smp::CPUS[cpu_id];
    let mut slot_guard = slot.lock();
    if let Some(cpu_local) = slot_guard.as_mut() {
        if let Some(current) = cpu_local.current.as_mut() {
            current.pending |= 1u64 << (2 - 1);
            crate::tasks::set_current_thread(Some(current.clone()));
            drop(slot_guard);
            // Wake the thread if it is blocked reading the keyboard
            crate::tasks::wake_keyboard_waiters();
        }
    }
}
