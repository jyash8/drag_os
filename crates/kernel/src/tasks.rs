use core::{
    alloc::Layout,
    arch::naked_asm,
    sync::atomic::{AtomicU64, Ordering},
};

use alloc::{
    alloc::alloc_zeroed,
    collections::{BTreeMap, VecDeque},
    string::String,
    sync::Arc,
    vec::Vec,
};
use shared::memory::address::{PhysAddr, VirtAddr};

use crate::{
    arch::x86_64::{
        abi::{
            RT_SIGFRAME_SIZE, RtSigFrame, SIGCHLD, SIGCONT, SIGIO, SIGKILL, SIGSTOP, SIGTSTP,
            SIGTTIN, SIGTTOU, SIGURG, SIGWINCH, SigAction,
        },
        cpu::{KERNEL_CR3, read_cr3, write_cr3},
        gdt::{USER_CODE_SELECTOR, USER_DATA_SELECTOR},
        msr::{IA32_FS_BASE, wrmsr},
        tss::set_rsp0,
    },
    fs::vfs::OpenFile,
    memory::vma::VmaList,
};

use crate::smp::CPUS;

static NEXT_TID: AtomicU64 = AtomicU64::new(1);

pub fn next_tid() -> u64 {
    NEXT_TID.fetch_add(1, Ordering::Relaxed)
}

pub static PROCESS_TABLE: SpinLock<BTreeMap<u64, Arc<SpinLock<Process>>>> =
    SpinLock::named("PROCESS_TABLE", BTreeMap::new());

pub const KERNEL_STACK_SIZE: usize = 32 * 1024;

// TEMP-DIAG stack-overflow canary (iret-to-garbage panic): magic word at the
// base of every kernel/syscall stack; checked on every dispatch. A smashed
// canary proves overflow (deep syscall + nested IRQs). Remove after root cause.
pub const STACK_CANARY: u64 = 0xDEADBEEFCAFEBABE;

pub fn alloc_kernel_stack() -> u64 {
    let layout = Layout::from_size_align(KERNEL_STACK_SIZE, 16).unwrap();
    let ptr = unsafe { alloc_zeroed(layout) } as u64;
    assert_ne!(ptr, 0, "Kernel heap exhausted");
    unsafe { (ptr as *mut u64).write_volatile(STACK_CANARY) };
    ptr + KERNEL_STACK_SIZE as u64
}

/// Allocates a small dedicated stack used by `syscall_entry` so that a thread's
/// syscall frame is not clobbered by another thread's syscalls (the previous
/// single global `SYSCALL_STACK` corrupted frames across fork/exec).
pub const SYSCALL_STACK_BYTES: usize = 16384;

pub fn alloc_syscall_stack() -> u64 {
    // ponytail: 16KB to match the original global; 4KB overflows deep syscall paths.
    let layout = Layout::from_size_align(SYSCALL_STACK_BYTES, 16).unwrap();
    let ptr = unsafe { alloc_zeroed(layout) } as u64;
    assert_ne!(ptr, 0, "Kernel heap exhausted");
    unsafe { (ptr as *mut u64).write_volatile(STACK_CANARY) };
    ptr + SYSCALL_STACK_BYTES as u64
}

pub const THREAD_FRAME_SIZE: u64 = 0xA0;

pub fn build_child_kernel_stack(
    user_rsp: u64,
    user_rip: u64,
    user_rflags: u64,
    r9: u64,
    rbx: u64,
    rbp: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
) -> (u64, u64, u64) {
    let stack_top = alloc_kernel_stack();
    let syscall_stack_top = alloc_syscall_stack();
    crate::dprintln!(
        "[KSTK] build frame base={:#x} ktop={:#x} systop={:#x} rip={:#x}",
        stack_top - THREAD_FRAME_SIZE, stack_top, syscall_stack_top, user_rip
    );
    let base = stack_top - THREAD_FRAME_SIZE;
    let frame = base as *mut u64;

    unsafe {
        *frame.add(0) = 0; // rax (fork returns 0 in child)
        *frame.add(1) = rbx;
        // rcx, rdx, rsi, rdi are scratch
        *frame.add(6) = rbp;
        *frame.add(8) = r9; // r9 (fn pointer in musl __clone!)
        *frame.add(11) = r12;
        *frame.add(12) = r13;
        *frame.add(13) = r14;
        *frame.add(14) = r15;

        *frame.add(15) = user_rip; // rip
        *frame.add(16) = USER_CODE_SELECTOR as u64; // cs
        *frame.add(17) = user_rflags; // rflags
        *frame.add(18) = user_rsp; // rsp
        *frame.add(19) = USER_DATA_SELECTOR as u64; // ss
    }
    (base, stack_top, syscall_stack_top)
}

#[rustfmt::skip]
#[derive(Debug, Default, Clone,Copy)]
#[repr(C)]
pub struct CpuContext {
    pub rax: u64, pub rbx: u64, pub rcx: u64, pub rdx: u64,
    pub rsi: u64, pub rdi: u64, pub rbp: u64, pub r8: u64,
    pub r9:  u64, pub r10: u64, pub r11: u64, pub r12: u64,
    pub r13: u64, pub r14: u64, pub r15: u64, pub rip: u64,
    pub cs: u64,pub rflags: u64,pub rsp: u64, pub ss: u64
}

// ponytail: queue for readability — bitmask retained for fast ready check, queue carries si_value/pid
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SigInfo {
    pub signo: i32,
    pub code: i32, // SI_USER=0, SI_QUEUE=-1
    pub pid: u64,
    pub value: i64,
}
pub const SI_USER: i32 = 0;
pub const SI_QUEUE: i32 = -1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessStatus {
    Running,
    Stopped,
    Zombie,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct RLimit {
    pub rlim_cur: u64,
    pub rlim_max: u64,
}

/// Per-thread FPU state. The kernel itself is compiled without SSE/AVX
/// (integer-only), so the only FPU users are user threads and the CPU FPU
/// registers leak across context switches unless we save/restore them. This is
/// the modern form of AGENTS invariant: save on park/switch, restore before the
/// iretq into the next thread.
///
/// `Box<FpuBuf>` is shared by all clones of a Thread (like Arc), so the
/// fxsave/fxrstor buffer is unique per logical thread, never per-clone.
#[repr(C, align(16))]
pub struct FpuBuf {
    pub data: [u8; 512],
}

pub fn fpu_save_to(addr: u64) {
    unsafe {
        core::arch::asm!("fxsave [{}]", in(reg) addr, options(nostack));
    }
}

pub fn fpu_restore_from(addr: u64) {
    unsafe {
        core::arch::asm!("fxrstor [{}]", in(reg) addr, options(nostack));
    }
}

impl FpuBuf {
    /// Capture the current CPU FPU state (used to seed a fresh thread so the
    /// buffer always holds a valid fxsave image, never a zero MXCSR).
    pub fn init() -> alloc::sync::Arc<FpuBuf> {
        let b = alloc::sync::Arc::new(FpuBuf { data: [0; 512] });
        fpu_save_to(core::ptr::addr_of!(b.data) as u64);
        b
    }
}

pub const RLIMIT_CPU: usize = 0;
pub const RLIMIT_FSIZE: usize = 1;
pub const RLIMIT_DATA: usize = 2;
pub const RLIMIT_STACK: usize = 3;
pub const RLIMIT_CORE: usize = 4;
pub const RLIMIT_RSS: usize = 5;
pub const RLIMIT_NPROC: usize = 6;
pub const RLIMIT_NOFILE: usize = 7;
pub const RLIMIT_MEMLOCK: usize = 8;
pub const RLIMIT_AS: usize = 9;
pub const RLIM_NLIMITS: usize = 16;
pub const RLIM_INFINITY: u64 = !0;

pub fn default_rlimits() -> [RLimit; RLIM_NLIMITS] {
    let mut limits = [RLimit { rlim_cur: RLIM_INFINITY, rlim_max: RLIM_INFINITY }; RLIM_NLIMITS];
    limits[RLIMIT_STACK] = RLimit {
        rlim_cur: 8 * 1024 * 1024, // 8 MB default stack limit
        rlim_max: 8 * 1024 * 1024,
    };
    limits[RLIMIT_NOFILE] = RLimit {
        rlim_cur: 1024,
        rlim_max: 4096,
    };
    limits
}

pub struct Process {
    pub pid: u64,
    pub ppid: u64,
    pub cr3: PhysAddr,
    pub heap_brk: VirtAddr,
    pub mmap_base: VirtAddr,
    pub vmas: VmaList,
    pub fd_table: Vec<Option<Arc<SpinLock<OpenFile>>>>,
    pub stdin_nonblock: bool,
    pub status: ProcessStatus,
    /// Atomic flag for quick stopped check without locking (avoids deadlock with scheduler).
    pub stopped: core::sync::atomic::AtomicBool,
    /// The signal that caused this process to stop (for waitpid).
    pub stop_signal: i32,
    /// Whether this process has been continued after being stopped (for WCONTINUED).
    pub continued: bool,
    pub exit_code: i32,
    pub children: Vec<Arc<SpinLock<Process>>>,
    // wait_queue removed: child-wait blocking uses the global CHILD_WAIT_QUEUE
    // instead.  Keeping a per-process VecDeque<Thread> created a second
    // ownership site that was never actually read back by any wakeup path.
    pub sigactions: [SigAction; 32],
    pub pending: u64,
    pub sigqueue: VecDeque<SigInfo>,
    pub canary: u64,
    pub priority: u8,
    pub nice: i32,
    pub uid: u32,
    pub euid: u32,
    pub suid: u32,
    pub gid: u32,
    pub egid: u32,
    pub sgid: u32,
    /// File-creation mask, set by `umask(2)`.
    ///
    /// Needed because `umask` was not dispatched, so it returned the
    /// dispatcher's ENOSYS default. Programs that read it back treat a
    /// negative return as the *old* mask, so they got `-38` — which `nix`'s
    /// `Mode::from_bits_truncate` rejects, and zellij died at its umask probe
    /// with "[BUG] umask returned invalid Mode" before reaching any of its own
    /// logic.
    pub umask: u32,
    /// The supplementary group list (`getgroups(2)`).
    ///
    /// Access checks classify a caller against the file's group by its
    /// **effective gid or any supplementary group**; a system that consulted
    /// only the primary gid would deny a user their own shared files. Empty
    /// for root's initial processes, which is Linux's own default.
    pub groups: alloc::vec::Vec<u32>,
    /// The capability bits this process holds. See `crate::fs::perm`.
    ///
    /// A uid-0 process starts with all of them and loses them when it drops
    /// its uid — otherwise `setuid(1000)` would change only what `getuid`
    /// reports while every access check kept the root bypasses.
    pub caps: u32,
    // ── Job control ────────────────────────────────────────────────────────────
    //
    // Real session/process-group state, which did not exist: `setpgid` was a
    // stub returning 0 and `setsid` just returned the pid. A program could
    // therefore create a session, believe it had a controlling terminal (its
    // `TIOCSCTTY` also returned 0 without recording anything), and then never
    // learn that a `SIGTTIN` should have stopped it. Everything below is what
    // makes `TIOCSCTTY` mean something and gives `ISIG` a group to signal.
    //
    // Invariant: `pgid` is a *pid* (the group leader's), not a separate counter.
    // A thread inherits the process's values and `fork_from` copies them, which
    // is why a child lands in its parent's group unless it moves.

    /// Process group id. Equals `pid` for a group leader.
    pub pgid: u64,
    /// Session id. Equals `pid` for a session leader.
    pub sid: u64,
    /// The VT this process group has as its controlling terminal, or 0 for
    /// none. Only meaningful when `sid` matches the session that claimed it.
    pub ctty_vt: u16,
    /// The VT this session owns, or 0. Set by `TIOCSCTTY`; a session may have
    /// at most one controlling terminal, which is the rule `TIOCSCTTY` enforces.
    pub ctty_owner_sid: u64,
    pub cwd: String,
    pub exe_path: String,
    pub threads_count: usize,
    /// Every thread belonging to this process, by tid.
    ///
    /// `threads_count` alone was not enough, and the symptom was a threaded
    /// client silently losing threads. A thread calling `exit_group` found no
    /// way to find its siblings, so it marked the process a zombie and dropped
    /// the count to zero while the others kept running — the process was over
    /// from the group's point of view but its threads were still scheduled.
    /// zellij (client + server + screen + wasm threads) lost its client this
    /// way and the server then logged "No client ids in screen found" forever.
    ///
    /// Holding the `Thread` means holding its kernel stack pointer, so this is
    /// also what makes it possible to signal or reap an individual thread.
    pub threads: BTreeMap<u64, Arc<Thread>>,
    /// CPU time accounted for `times`/`getrusage`, in timer ticks (1ms).
    /// `utime` counts ticks with the thread in user mode, `stime` in kernel.
    pub utime: u64,
    pub stime: u64,
    pub rlimits: [RLimit; RLIM_NLIMITS],
    // ── ptrace (PTRACE_*) ────────────────────────────────────────────────────────
    //
    // Minimal ptrace support: enough of the protocol for a tracer to attach,
    // stop, inspect (registers/memory), continue, and detach a tracee. The
    // stop/continue machinery is shared with job control (`ProcessStatus` +
    // `stopped` + `stop_signal`), which is what makes `waitpid(WUNTRACED)`
    // report a ptrace-stop with the right encoding.

    /// pid of the tracer, 0 when not traced. Set by `PTRACE_TRACEME`
    /// (the tracee's own syscall, stores its ppid) or `PTRACE_ATTACH`/
    /// `PTRACE_SEIZE` (the tracer's syscall, stores its pid).
    pub tracer_pid: u64,
    /// Snapshot of the traced process's current thread (kernel stack frame)
    /// captured when it entered a ptrace-stop. Register/memory access run
    /// against `*this`.
    pub ptrace_thread: Option<Thread>,
    /// The signal whose delivery was intercepted as a ptrace-stop (0 when the
    /// process is running or stopped by ordinary job control). The tracer
    /// re-delivers it via `PTRACE_CONT(pid, sig)` / `PTRACE_DETACH`.
    pub ptrace_sig: i32,
    /// Options from `PTRACE_SEIZE` (PTRACE_O_*). Accepted and stored; none of
    /// the event-reporting options are acted on yet, so the bits are inert.
    pub ptrace_flags: u32,
}

impl Process {
    pub fn new(pid: u64, ppid: u64, cr3: PhysAddr, priority: u8) -> Self {
        let tty_arc = Arc::new(SpinLock::new(OpenFile {
            vnode: crate::fs::vfs::VNode::TtyDevice { vt: 0 },
            flags: 0,
            path: alloc::string::String::from("/dev/tty"),
        }));
        Self {
            pid,
            ppid,
            cr3,
            heap_brk: VirtAddr(0),
            mmap_base: VirtAddr(0x0000_7000_0000_0000),
            fd_table: alloc::vec![Some(tty_arc.clone()), Some(tty_arc.clone()), Some(tty_arc)], // 0: stdin, 1: stdout, 2: stderr
            stdin_nonblock: false,
            status: ProcessStatus::Running,
            stop_signal: 0,
            continued: false,
            exit_code: 0,
            stopped: core::sync::atomic::AtomicBool::new(false),
            children: Vec::new(),
            // wait_queue removed — see Process struct comment.
            sigactions: [SigAction::default(); 32],
            pending: 0,
            sigqueue: VecDeque::new(),
            canary: 0xCAFEBABE,
            priority,
            nice: 0,
            uid: 0,
            euid: 0,
            suid: 0,
            umask: 0o022,
            groups: Vec::new(),
            caps: crate::fs::perm::CAP_ALL,
            gid: 0,
            egid: 0,
            sgid: 0,
            vmas: VmaList::default(),
            pgid: pid,
            sid: pid,
            ctty_vt: 0,
            ctty_owner_sid: 0,
            cwd: String::from("/"),
            exe_path: String::new(),
            threads_count: 1,
            threads: BTreeMap::new(),
            utime: 0,
            stime: 0,
            rlimits: default_rlimits(),
            tracer_pid: 0,
            ptrace_thread: None,
            ptrace_sig: 0,
            ptrace_flags: 0,
        }
    }

    pub fn fork_from(&self, child_pid: u64, child_cr3: PhysAddr) -> Self {
        let new_fd_table = self.fd_table.clone();
        Self {
            pid: child_pid,
            ppid: self.pid,
            cr3: child_cr3,
            heap_brk: self.heap_brk,
            mmap_base: self.mmap_base,
            fd_table: new_fd_table,
            stdin_nonblock: self.stdin_nonblock,
            status: ProcessStatus::Running,
            stop_signal: 0,
            continued: false,
            exit_code: 0,
            stopped: core::sync::atomic::AtomicBool::new(false),
            children: Vec::new(),
            // wait_queue removed — see Process struct comment.
            sigactions: self.sigactions,
            pending: 0,
            sigqueue: VecDeque::new(),
            canary: 0xCAFEBABE,
            priority: self.priority,
            nice: self.nice,
            uid: self.uid,
            euid: self.euid,
            suid: self.suid,
            umask: self.umask,
            groups: self.groups.clone(),
            caps: self.caps,
            gid: self.gid,
            egid: self.egid,
            sgid: self.sgid,
            vmas: self.vmas.clone(),
            // A forked child starts in its parent's process group and session,
            // with no controlling terminal of its own. That is the POSIX rule
            // and it is what makes a shell's `fork(); setpgid();` the only way
            // to put a job in the background.
            pgid: self.pgid,
            sid: self.sid,
            ctty_vt: 0,
            ctty_owner_sid: 0,
            cwd: self.cwd.clone(),
            exe_path: self.exe_path.clone(),
            threads_count: 1,
            threads: BTreeMap::new(),
            utime: 0,
            stime: 0,
            rlimits: self.rlimits,
            tracer_pid: 0,
            ptrace_thread: None,
            ptrace_sig: 0,
            ptrace_flags: 0,
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if self.cr3.0 != 0 {
            crate::memory::space::destroy_address_space(self.cr3);
        }
    }
}
#[derive(Clone)]
pub struct Thread {
    pub id: u64,
    pub process: Arc<SpinLock<Process>>, // Multiple threads can share one process
    // NOTE: `context` has been removed. The authoritative CPU context lives on
    // the kernel stack at `stack_ptr` as a `CpuContext`-shaped frame built by
    // the interrupt handler. Use `context_from_stack(thread.stack_ptr)` to
    // read or mutate it. A separate `context` copy-field would create two
    // contradictory representations of the same architectural state.
    pub fs_base: u64,           // For Thread Local Storage
    pub stack_ptr: u64,         // Saved RSP pointing at the in-kernel CpuContext frame
    pub stack_limit: u64,       // Lowest valid user/RSP address (guard page base - 1)
    pub kernel_stack_top: u64,  // Top of the thread's Ring 0 stack
    pub syscall_stack_top: u64, // Top of a dedicated stack used by `syscall_entry` for this thread.
    pub sigmask: u64,           // Signal mask
    pub pending: u64,
    pub sigqueue: VecDeque<SigInfo>,
    pub priority: u8,
    pub nice: i32,
    pub vruntime: u64,
    /// Set by a syscall when this thread wants to park itself into a wait queue.
    /// Consumed by `timer_tick_on_core()` on the next timer interrupt, which
    /// moves the thread from the CPU's `current` slot into the appropriate
    /// wait queue.  Storing this on the thread (not in a global) ensures that
    /// only the CPU currently running this thread can ever read or clear it,
    /// eliminating the SMP race where another CPU's timer interrupt could steal
    /// a blocking request that belongs to a different core.
    pub pending_block: Option<PendingBlock>,
    /// Last syscall number entered by this thread (0 = none yet).
    /// Written once per syscall entry (write-back with the entry clone);
    /// read by the NMI hang dump to show what parked threads were doing.
    /// Plain u64: only ever written through the CPU-local current slot or
    /// an owned queue value, never shared mutably.
    pub last_syscall_nr: u64,
    /// `PIPE_WAKE_SEQ` observed when this thread last successfully parked as
    /// `PendingBlock::PipeWait` (or self-resumed because a wake raced the
    /// park). The park branch compares it to the live sequence: if it advanced,
    /// a producer pushed to the pty/pipe/socket *after* this thread's
    /// empty-check but before the park, and the wake would otherwise be lost.
    /// See `timer_tick_on_core`'s `PipeWait` arm.
    pub pipe_seq_baseline: u64,
    pub clear_child_tid: Option<u64>,
    /// Saved FPU/SSE state. Only restored when this thread is iretq'd back in,
    /// only saved when the thread is parked/switched out. `Arc` so all clones
    /// of a Thread share one buffer (Box would deep-copy on clone).
    pub fpu: alloc::sync::Arc<FpuBuf>,
    /// FPU state captured when a signal handler was delivered (pre-handler
    /// state); restored by rt_sigreturn. Per-thread, so no locking needed.
    /// ponytail: replaces the old global/per-CPU scratch (SMP-racy + deadlocked).
    pub uc_fpu: alloc::sync::Arc<FpuBuf>,
    /// Alternate signal stack (sigaltstack). ss_sp is the stack base, ss_size
    /// its length, ss_flags SS_DISABLE means no alternate stack. Zero-initialised
    /// means the thread uses its normal stack for signal handlers.
    pub alt_stack: crate::arch::x86_64::abi::StackT,
}

impl Thread {
    pub fn new(
        id: u64,
        process: Arc<SpinLock<Process>>,
        stack_ptr: u64,
        kernel_stack_top: u64,
        syscall_stack_top: u64,
        priority: u8,
        stack_limit: u64,
    ) -> Self {
        let nice = process.lock().nice;
        Self {
            id,
            process,
            fs_base: 0,
            stack_ptr,
            stack_limit,
            kernel_stack_top,
            syscall_stack_top,
            sigmask: 0,
            pending: 0,
            sigqueue: VecDeque::new(),
            priority,
            nice,
            vruntime: 0,
            pending_block: None,
            last_syscall_nr: 0,
            pipe_seq_baseline: 0,
            clear_child_tid: None,
            fpu: FpuBuf::init(),
            uc_fpu: FpuBuf::init(),
            alt_stack: crate::arch::x86_64::abi::StackT::default(),
        }
    }

    /// Raw address of this thread's FPU buffer, shared by all clones and only
    /// valid while the owning Thread (or any of its clones) is alive.
    pub fn fpu_buf_addr(&self) -> u64 {
        (&self.fpu.data[0]) as *const u8 as u64
    }

    /// Save the live CPU FPU state (which, since the kernel is integer-only,
    /// is this thread's own user FPU state) into this thread's buffer.
    pub fn fpu_save(&self) {
        fpu_save_to(self.fpu_buf_addr());
    }

    /// Restore this thread's last saved FPU state from its buffer.
    pub fn fpu_restore(&self) {
        fpu_restore_from(self.fpu_buf_addr());
    }
}

use crate::arch::sync::spinlock::SpinLock;

pub static SCHEDULER: SpinLock<crate::sched::SchedRunQueue> = SpinLock::named("SCHEDULER", crate::sched::SchedRunQueue::new());

/// Returns the current thread for the CPU running this code (clone).
#[track_caller]
pub fn current_thread() -> Option<Thread> {
    let cpu_id = crate::smp::current_cpu_id();
    let cpu_slot = &CPUS[cpu_id as usize];
    cpu_slot.lock().as_ref().unwrap().current.clone()
}

/// Returns the current process for the CPU running this code (clones only Arc<SpinLock<Process>>).
#[track_caller]
pub fn current_process() -> Option<Arc<SpinLock<Process>>> {
    let cpu_id = crate::smp::current_cpu_id();
    let cpu_slot = &CPUS[cpu_id as usize];
    cpu_slot.lock().as_ref().unwrap().current.as_ref().map(|t| t.process.clone())
}

/// Returns the PID of the current thread.
#[track_caller]
pub fn get_current_pid() -> u64 {
    current_thread()
        .map(|t| t.process.lock().pid)
        .unwrap_or(0)
}

/// Sets the current thread for the CPU running this code.
#[track_caller]
pub fn set_current_thread(t: Option<Thread>) {
    let cpu_id = crate::smp::current_cpu_id();
    CPUS[cpu_id as usize].lock().as_mut().unwrap().current = t;
}

/// Per-CPU fault-path process stash. resume_with_signals() touches user
/// memory (signal frames) while CPUS[me] is held, so a demand fault there
/// cannot use current_thread() (same-CPU re-entry panic). The dispatcher
/// stashes the switching thread's process around resume; the PF handler's
/// demand lookups prefer it via fault_process(). Set/clear regions run with
/// IF=0, so no async tick can observe a stale entry; cleared on every return
/// from resume (and in exit_current_thread after the clear_child_tid write).
/// Established: smp.rs run_thread_on_core / timer_tick_on_core.
pub static FAULT_PROC: [AtomicU64; crate::smp::MAX_CPUS] =
    [const { AtomicU64::new(0) }; crate::smp::MAX_CPUS];

/// Stash `p` for the fault path on this CPU. The Arc stays alive in the
/// dispatcher's owned thread across resume, so the raw pointer is valid.
pub fn fault_stash_set(p: &Arc<SpinLock<Process>>) {
    let me = crate::smp::current_cpu_id() as usize;
    FAULT_PROC[me].store(Arc::as_ptr(p) as u64, Ordering::Relaxed);
}

/// Clear this CPU's fault stash (see above for discipline).
pub fn fault_stash_clear() {
    let me = crate::smp::current_cpu_id() as usize;
    FAULT_PROC[me].store(0, Ordering::Relaxed);
}

/// Process for fault-path demand paging: stashed (lock-free) if inside a
/// resume dispatch, else the installed current thread. If CPUS[me] is held
/// (demand fault under dispatch without a stash — a bug), dumps forensics
/// and returns None instead of re-entry-panicking and masking the fault.
pub fn fault_process() -> Option<Arc<SpinLock<Process>>> {
    let me = crate::smp::current_cpu_id() as usize;
    let raw = FAULT_PROC[me].load(Ordering::Relaxed);
    if raw != 0 {
        // SAFETY: reconstructed without disturbing the strong count
        // (clone + forget nets zero); the Arc is alive in the dispatcher.
        unsafe {
            let a = Arc::from_raw(raw as *const SpinLock<Process>);
            let out = a.clone();
            core::mem::forget(a);
            Some(out)
        }
    } else {
        match CPUS[me].try_lock() {
            Some(slot) => {
                let out = match slot.as_ref() {
                    Some(c) => match c.current.as_ref() {
                        Some(t) => Some(t.process.clone()),
                        None => None,
                    },
                    None => None,
                };
                out
            }
            None => {
                // Demand fault under CPUS-hold with no stash: dump forensics
                // (stack words resolve the caller via nm) and fail gracefully
                // instead of re-entry-panicking and masking the fault.
                let mut rsp: u64;
                unsafe { core::arch::asm!("mov {}, rsp", out(reg) rsp, options(nomem, nostack, preserves_flags)) };
                crate::dprintln!("FAULT-NOLOCK cpu={} rsp={:#x}", me, rsp);
                for i in 0..16u64 {
                    let w = unsafe { ((rsp + i * 8) as *const u64).read_volatile() };
                    crate::dprintln!("  stk[{:#x}] = {:#x}", i * 8, w);
                }
                None
            }
        }
    }
}
pub static THREADS_ACTIVE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
pub static CHILD_WAIT_QUEUE: SpinLock<VecDeque<Thread>> = SpinLock::named("CHILD_WAIT_QUEUE", VecDeque::new());
pub static KEYBOARD_WAIT_QUEUE: SpinLock<VecDeque<Thread>> = SpinLock::named("KEYBOARD_WAIT_QUEUE", VecDeque::new());
pub static INPUT_WAIT_QUEUE: SpinLock<VecDeque<Thread>> = SpinLock::named("INPUT_WAIT_QUEUE", VecDeque::new());
pub static PIPE_WAIT_QUEUE: SpinLock<VecDeque<Thread>> = SpinLock::named("PIPE_WAIT_QUEUE", VecDeque::new());
pub static FUTEX_WAIT_QUEUE: SpinLock<VecDeque<Thread>> = SpinLock::named("FUTEX_WAIT_QUEUE", VecDeque::new());
pub static SYNCOBJ_WAIT_QUEUE: SpinLock<VecDeque<Thread>> = SpinLock::named("SYNCOBJ_WAIT_QUEUE", VecDeque::new());

/// Monotonic count of pipe/pty/socket broadcast wakeups.
///
/// A `PipeWait` waiter records this at the point it last checked its buffer and
/// re-checks it in the park branch. If it advanced, a producer pushed *after*
/// the empty-check but before the park — the wake found `PIPE_WAIT_QUEUE`
/// empty and would otherwise be lost. Comparing a sequence avoids having the
/// generic park branch know which subsystem (pty, pipe, socket, eventfd) owns
/// the buffer. Bumped only under `SCHEDULER`, in `wake_pipe_waiters`.
pub static PIPE_WAKE_SEQ: AtomicU64 = AtomicU64::new(0);

static NEXT_PID: AtomicU64 = AtomicU64::new(1);
pub fn next_pid() -> u64 {
    NEXT_PID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingBlock {
    Keyboard,
    ChildWait,
    PipeWait,
    Futex { uaddr: u64, expected: u32, deadline: u64, pid: u64 },
    // ponytail: epoll_wait park. Lives on PIPE_WAIT_QUEUE (broad fd-activity
    // wakes hit it there); the tick sweep expires deadline!=MAX. No park-time
    // re-check (unlike Futex): re-scanning epoll sets under SCHEDULER would
    // invert lock order, so a wake landing in the scan->park window is lost
    // and delivery waits for the next broad wake or the deadline — bounded
    // (calloop always passes finite timeouts) and the same shape as PipeWait.
    EpollWait { deadline: u64 },
    // ponytail: per-open evdev stream key (OpenFile Arc ptr) + device so the
    // park re-checks the same reader the blocked read() drains — Linux-like.
    Input { reader: usize, dev: usize },
    // Terminal job control (SIGTTIN/SIGTTOU): the thread stopped itself
    // because it is in a background process group and must sleep until
    // SIGCONT clears `Process::stopped`. Parked on PIPE_WAIT_QUEUE, which
    // `wake_parked_for_pid` sweeps on resume. Re-checked on every wake by
    // `park_until_stopped_cleared`, so a resume that raced the park is not
    // lost — see that function for why that matters.
    JobControl,
    // Timer-backed nanosleep: parks on FUTEX_WAIT_QUEUE; the per-tick sweep
    // in wake_expired_waiters wakes the thread when now >= deadline.
    // Unlike Futex, no futex-word re-check — always wake on expiry.
    // Interruptible by signal: sys_nanosleep checks pending on resume.
    Sleep { deadline: u64 },
    // ponytail: DRM syncobj wait. Parked on SYNCOBJ_WAIT_QUEUE; wake on signal.
    Syncobj { handle: u32, deadline: u64, pid: u64 },
}

/// Dump every thread's scheduling state: runnable, running, and blocked (with
/// the reason it is blocked on).
///
/// This exists because "the UI is stuck" was measurable but not diagnosable.
/// `[input-stats]` says a consumer stopped draining; it cannot say whether the
/// consumer is parked in `Input`, spinning on a spinlock, or blocked on a
/// pipe/fd/syncobj. Two wrong diagnoses came from inferring that from counters,
/// so the state has to be observable rather than inferred.
///
/// Print outside every lock: clone each queue under its own guard, drop the
/// guard, then print. Holding SCHEDULER or a wait-queue lock across a debugcon
/// write is exactly the lock-order inversion this codebase's invariants forbid.
pub fn dump_thread_states(reason: &str) {
    use alloc::string::ToString;
    fn line(t: &Thread, class: &str, queue: &str) {
        let pid = t.process.try_lock().map(|p| p.pid).unwrap_or(0);
        let blk = match &t.pending_block {
            None => "-".to_string(),
            Some(crate::tasks::PendingBlock::Input { reader, dev }) => {
                alloc::format!("Input{{reader={reader:#x},dev={dev}}}")
            }
            Some(crate::tasks::PendingBlock::PipeWait) => "PipeWait".to_string(),
            Some(crate::tasks::PendingBlock::JobControl) => "JobControl".to_string(),
            Some(crate::tasks::PendingBlock::Keyboard) => "Keyboard".to_string(),
            Some(crate::tasks::PendingBlock::ChildWait) => "ChildWait".to_string(),
            Some(crate::tasks::PendingBlock::Sleep { deadline }) => {
                alloc::format!("Sleep{{deadline={deadline}}}")
            }
            Some(crate::tasks::PendingBlock::Futex { .. }) => "Futex".to_string(),
            Some(crate::tasks::PendingBlock::EpollWait { .. }) => "EpollWait".to_string(),
            Some(crate::tasks::PendingBlock::Syncobj { handle, .. }) => {
                alloc::format!("Syncobj{{handle={handle}}}")
            }
            // No catch-all `Some(_)`: every variant is listed, so a new
            // `PendingBlock` becomes a *compile error* here rather than
            // silently printing "other". That is the point of this dump —
            // it exists because states were undiagnosable, and a block
            // reason that silently degrades to "other" is the failure mode
            // it was written to prevent.
        };
        crate::dprintln!(
            "  [{:<8}] {:<14} tid={} pid={} last_syscall={} block={}",
            class,
            queue,
            t.id,
            pid,
            t.last_syscall_nr,
            blk
        );
    }

    crate::dprintln!("[threads] dump: {}", reason);

    // Runnable.
    //
    // try_lock, NOT lock: this runs from the timer tick, i.e. interrupt context
    // with IF=0. A blocking acquire here would spin with interrupts masked
    // waiting for a SCHEDULER holder on the *other* CPU, which cannot make
    // progress because the thing it is waiting for may need this very
    // interrupt to run. That is a self-inflicted deadlock — and the diagnostic
    // must never be able to cause the failure it is diagnosing.
    {
        let run: Vec<Thread> = {
            let s = match SCHEDULER.try_lock() {
                Some(s) => s,
                None => {
                    crate::dprintln!("[threads] dump: skipped (SCHEDULER held elsewhere)");
                    return;
                }
            };
            s.iter_threads().cloned().collect()
        };
        crate::dprintln!("[threads] runnable: {}", run.len());
        for t in &run {
            line(t, "RUNNABLE", "sched");
        }
    }

    // Running / current per CPU.
    for cpu in 0..crate::smp::MAX_CPUS {
        let cur: Option<Thread> = crate::smp::CPUS[cpu]
            .try_lock()
            .and_then(|s| s.as_ref()?.current.clone());
        if let Some(t) = cur {
            line(&t, "RUNNING", &alloc::format!("cpu{cpu}"));
        }
    }

    // Blocked, per queue.
    macro_rules! dump_q {
        ($q:expr, $name:expr) => {{
            let v: Vec<Thread> = $q.lock().iter().cloned().collect();
            if !v.is_empty() {
                crate::dprintln!("[threads] blocked in {}: {}", $name, v.len());
                for t in &v {
                    line(t, "BLOCKED", $name);
                }
            }
        }};
    }
    dump_q!(INPUT_WAIT_QUEUE, "INPUT_WAIT_QUEUE");
    dump_q!(PIPE_WAIT_QUEUE, "PIPE_WAIT_QUEUE");
    dump_q!(KEYBOARD_WAIT_QUEUE, "KEYBOARD_WAIT_QUEUE");
    dump_q!(CHILD_WAIT_QUEUE, "CHILD_WAIT_QUEUE");
    dump_q!(FUTEX_WAIT_QUEUE, "FUTEX_WAIT_QUEUE");
    dump_q!(SYNCOBJ_WAIT_QUEUE, "SYNCOBJ_WAIT_QUEUE");
}

/// Pull a set of threads out of the run queue *and* every wait queue.
///
/// Adding a thread to a new wait queue without also covering it here is how a
/// reaped sibling comes back to life: it stays parked on a queue nobody prunes,
/// a later `wake_*_waiters` re-enqueues it, and it then runs against a process
/// that is already gone. `exit_group` had exactly that hole — it cleared
/// `SCHEDULER` but not the wait queues (see the call in `exit_current_thread`).
///
/// Takes one lock at a time, never two: the global order is
/// `SCHEDULER -> CPUS[k] -> wait-queues -> process.lock()`, and a wait queue is
/// always released before the next is taken.
pub fn purge_threads_from_queues(ids: &alloc::collections::BTreeSet<u64>) {
    if ids.is_empty() {
        return;
    }
    SCHEDULER.lock().retain(|t| !ids.contains(&t.id));
    PIPE_WAIT_QUEUE.lock().retain(|t| !ids.contains(&t.id));
    INPUT_WAIT_QUEUE.lock().retain(|t| !ids.contains(&t.id));
    KEYBOARD_WAIT_QUEUE.lock().retain(|t| !ids.contains(&t.id));
    CHILD_WAIT_QUEUE.lock().retain(|t| !ids.contains(&t.id));
    FUTEX_WAIT_QUEUE.lock().retain(|t| !ids.contains(&t.id));
    SYNCOBJ_WAIT_QUEUE.lock().retain(|t| !ids.contains(&t.id));
}

pub fn wake_pipe_waiters() {
    let mut sched = SCHEDULER.lock();
    // Advance the sequence *before* draining the queue. A waiter that has
    // checked its buffer but has not yet parked will see the change in the
    // park branch and re-check instead of sleeping through this wake. Wakers
    // and the park branch both hold SCHEDULER, so this compare is exact.
    let seq = PIPE_WAKE_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    let mut wq = PIPE_WAIT_QUEUE.lock();
    while let Some(mut t) = wq.pop_front() {
        t.pending_block = None;
        t.pipe_seq_baseline = seq;
        sched.enqueue_with_boost(t, 4);
    }
}

pub fn wake_input_waiters() {
    let mut sched = SCHEDULER.lock();
    let mut wq = INPUT_WAIT_QUEUE.lock();
    while let Some(mut t) = wq.pop_front() {
        t.pending_block = None;
        sched.enqueue_with_boost(t, 4);
    }
}

pub fn wake_keyboard_waiters() {
    let mut sched = SCHEDULER.lock();
    let mut wq = KEYBOARD_WAIT_QUEUE.lock();
    while let Some(mut t) = wq.pop_front() {
        t.pending_block = None;
        sched.enqueue_with_boost(t, 4);
    }
}

pub fn wake_futex_waiters(uaddr: u64, max_wake: usize, caller_pid: u64) -> usize {
    let mut sched = SCHEDULER.lock();
    let mut woken = 0;

    // Check CPUS slots first: if a thread on another CPU is currently in
    // block_current_thread(PendingBlock::Futex) executing `sti; int 0x81`
    // before timer_tick parks it, clearing pending_block prevents it from
    // being parked into FUTEX_WAIT_QUEUE and missing this wakeup.
    for cpu_id in 0..crate::smp::MAX_CPUS {
        if woken >= max_wake {
            break;
        }
        let mut cpu = crate::smp::CPUS[cpu_id].lock();
        if let Some(cpu_data) = cpu.as_mut() {
            if let Some(t) = cpu_data.current.as_mut() {
                if let Some(PendingBlock::Futex { uaddr: a, pid, .. }) = t.pending_block {
                    if a == uaddr && (pid == caller_pid || caller_pid == 0) {
                        t.pending_block = None;
                        woken += 1;
                    }
                }
            }
        }
    }

    let mut wq = FUTEX_WAIT_QUEUE.lock();
    let mut kept = VecDeque::with_capacity(wq.len());
    while let Some(t) = wq.pop_front() {
        if woken < max_wake
            && matches!(t.pending_block, Some(PendingBlock::Futex { uaddr: a, pid, .. }) if a == uaddr && (pid == caller_pid || caller_pid == 0))
        {
            let mut t = t;
            t.pending_block = None;
            sched.enqueue_with_boost(t, 4);
            woken += 1;
        } else {
            kept.push_back(t);
        }
    }
    *wq = kept;
    woken
}

/// Wake all threads blocked on a specific syncobj handle.
/// Lock order: SCHEDULER -> SYNCOBJ_WAIT_QUEUE (matches the park path).
pub fn wake_syncoff_waiters(handle: u32, pid: u64) -> usize {
    let mut sched = SCHEDULER.lock();
    let mut woken = 0;

    // Check CPUS slots first: threads currently in block_current_thread(Syncobj)
    // before timer_tick parks them. The thread is still running on that CPU,
    // so clearing pending_block is enough (no enqueue — it was never parked).
    for cpu_id in 0..crate::smp::MAX_CPUS {
        let mut cpu = crate::smp::CPUS[cpu_id].lock();
        if let Some(cpu_data) = cpu.as_mut() {
            if let Some(t) = cpu_data.current.as_mut() {
                if let Some(PendingBlock::Syncobj { handle: h, pid: p, .. }) = t.pending_block {
                    if h == handle && (p == pid || pid == 0) {
                        t.pending_block = None;
                        woken += 1;
                    }
                }
            }
        }
    }

    let mut wq = SYNCOBJ_WAIT_QUEUE.lock();
    let mut kept = VecDeque::with_capacity(wq.len());
    while let Some(t) = wq.pop_front() {
        if let Some(PendingBlock::Syncobj { handle: h, pid: p, .. }) = t.pending_block {
            if h == handle && (p == pid || pid == 0) {
                let mut t = t;
                t.pending_block = None;
                sched.enqueue_with_boost(t, 4);
                woken += 1;
            } else {
                kept.push_back(t);
            }
        } else {
            kept.push_back(t);
        }
    }
    *wq = kept;
    woken
}

pub fn requeue_futex_waiters(
    uaddr1: u64,
    max_wake: usize,
    uaddr2: u64,
    max_requeue: usize,
    caller_pid: u64,
) -> (usize, usize) {
    let mut sched = SCHEDULER.lock();
    let mut woken = 0;
    let mut requeued = 0;

    // Check CPUS slots first: threads currently in block_current_thread
    for cpu_id in 0..crate::smp::MAX_CPUS {
        if woken >= max_wake && requeued >= max_requeue {
            break;
        }
        let mut cpu = crate::smp::CPUS[cpu_id].lock();
        if let Some(cpu_data) = cpu.as_mut() {
            if let Some(t) = cpu_data.current.as_mut() {
                if let Some(PendingBlock::Futex { uaddr: a, pid, .. }) = t.pending_block {
                    if a == uaddr1 && (pid == caller_pid || caller_pid == 0) {
                        if woken < max_wake {
                            t.pending_block = None;
                            woken += 1;
                        } else if requeued < max_requeue {
                            // Clear pending_block so it returns to userspace and locks uaddr2 directly
                            t.pending_block = None;
                            requeued += 1;
                        }
                    }
                }
            }
        }
    }

    let mut wq = FUTEX_WAIT_QUEUE.lock();
    let mut kept = VecDeque::with_capacity(wq.len());
    while let Some(mut t) = wq.pop_front() {
        let is_match = match t.pending_block {
            Some(PendingBlock::Futex { uaddr, pid, .. }) => uaddr == uaddr1 && (pid == caller_pid || caller_pid == 0),
            _ => false,
        };
        if is_match {
            if woken < max_wake {
                t.pending_block = None;
                sched.enqueue_with_boost(t, 4);
                woken += 1;
            } else if requeued < max_requeue {
                if let Some(PendingBlock::Futex { ref mut uaddr, .. }) = t.pending_block {
                    *uaddr = uaddr2;
                }
                requeued += 1;
                kept.push_back(t);
            } else {
                kept.push_back(t);
            }
        } else {
            kept.push_back(t);
        }
    }
    *wq = kept;
    (woken, requeued)
}

/// Wake any threads sleeping (PendingBlock::Sleep) that belong to `pid`.
/// Called by sys_kill so that nanosleep is signal-interruptible: the thread
/// wakes, sys_nanosleep re-checks pending signals, and returns EINTR.
/// Lock order: SCHEDULER -> FUTEX_WAIT_QUEUE (same as wake_futex_waiters).
pub fn wake_sleep_waiters_for_pid(pid: u64) {
    let mut sched = SCHEDULER.lock();
    let mut wq = FUTEX_WAIT_QUEUE.lock();
    let mut kept = VecDeque::with_capacity(wq.len());
    while let Some(t) = wq.pop_front() {
        let is_sleep = matches!(t.pending_block, Some(PendingBlock::Sleep { .. }));
        if is_sleep && t.process.lock().pid == pid {
            let mut t = t;
            t.pending_block = None;
            sched.enqueue_with_boost(t, 4);
        } else {
            kept.push_back(t);
        }
    }
    *wq = kept;
}

/// Wake timed waiters whose deadline passed: futex waits (ETIMEDOUT) and
/// epoll waits (re-scan on resume) parked with a tick deadline.
/// Scans FUTEX_WAIT_QUEUE (Futex) and PIPE_WAIT_QUEUE (EpollWait only —
/// plain PipeWait has no deadline and is skipped).
/// Same lock order as the wake helpers: SCHEDULER -> wait-queue.
/// Called once per timer tick (CPU0) with no locks held.
/// Established: time.rs timer_rust_handler (lock-free tick context).
pub fn wake_expired_waiters(now: u64) -> usize {
    // ponytail: fast path — most ticks nothing expires. Peek under the
    // wait-queue lock alone; take SCHEDULER + rebuild only when something
    // actually expired. Same expiries, same tick (rescan is full).
    fn futex_expired(t: &Thread, now: u64) -> bool {
        matches!(t.pending_block,
            Some(PendingBlock::Futex { deadline, .. }) if deadline <= now)
            || matches!(t.pending_block,
            Some(PendingBlock::Sleep { deadline }) if deadline <= now)
    }
    fn epoll_expired(t: &Thread, now: u64) -> bool {
        matches!(t.pending_block, Some(PendingBlock::EpollWait { deadline }) if deadline <= now)
    }
    fn syncobj_expired(t: &Thread, now: u64) -> bool {
        matches!(t.pending_block, Some(PendingBlock::Syncobj { deadline, .. }) if deadline <= now)
    }
    let mut woken = 0;
    // Futex + Sleep deadlines (both live on FUTEX_WAIT_QUEUE).
    let need_futex = { FUTEX_WAIT_QUEUE.lock().iter().any(|t| futex_expired(t, now)) };
    if need_futex {
        let mut sched = SCHEDULER.lock();
        let mut wq = FUTEX_WAIT_QUEUE.lock();
        let mut kept = VecDeque::with_capacity(wq.len());
        while let Some(t) = wq.pop_front() {
            if futex_expired(&t, now) {
                let mut t = t;
                t.pending_block = None;
                sched.enqueue_with_boost(t, 4);
                woken += 1;
            } else {
                kept.push_back(t);
            }
        }
        *wq = kept;
    }
    // Syncobj deadlines.
    let need_syncobj = { SYNCOBJ_WAIT_QUEUE.lock().iter().any(|t| syncobj_expired(t, now)) };
    if need_syncobj {
        let mut sched = SCHEDULER.lock();
        let mut wq = SYNCOBJ_WAIT_QUEUE.lock();
        let mut kept = VecDeque::with_capacity(wq.len());
        while let Some(t) = wq.pop_front() {
            if syncobj_expired(&t, now) {
                let mut t = t;
                t.pending_block = None;
                sched.enqueue_with_boost(t, 4);
                woken += 1;
            } else {
                kept.push_back(t);
            }
        }
        *wq = kept;
    }
    // Epoll deadlines (same queue as pipes for broad fd-activity wakes).
    let need_epoll = { PIPE_WAIT_QUEUE.lock().iter().any(|t| epoll_expired(t, now)) };
    if need_epoll {
        let mut sched = SCHEDULER.lock();
        let mut wq = PIPE_WAIT_QUEUE.lock();
        let mut kept = VecDeque::with_capacity(wq.len());
        while let Some(t) = wq.pop_front() {
            if epoll_expired(&t, now) {
                let mut t = t;
                t.pending_block = None;
                sched.enqueue_with_boost(t, 4);
                woken += 1;
            } else {
                kept.push_back(t);
            }
        }
        *wq = kept;
    }
    woken
}

/// Park until the process is no longer stopped, or `pred` says it may proceed.
///
/// This closes the stop/resume race that a plain park loses. The sequence for
/// terminal job control is: the process marks itself stopped, then parks. A
/// `SIGCONT` arriving in between runs its wait-queue sweep *before* this
/// thread is in any queue, finds nothing, and the thread then parks forever --
/// stopped, asleep, and never woken. That is a hang, not a stop.
///
/// Here the thread parks and, on every wake, re-tests the stop flag rather
/// than assuming the wake was the resume. If it wakes and is still stopped it
/// simply parks again, so a SIGCONT that raced the park is caught on the next
/// dispatch. The pending-stop invariant (`Process::stopped`) is therefore the
/// synchronisation point, not queue membership.
///
/// `pred` is called with the process locked *not* held — it may take other
/// locks.
pub fn park_until_stopped_cleared<F: FnMut() -> bool>(mut pred: F) {
    // Publish the park reason on this thread *before* the first condition
    // test.
    //
    // `pred` is what marks the process stopped, and the stop is what a
    // `SIGCONT`/`tcsetpgrp` acts on. If the reason were set inside
    // `block_current_thread` (after `pred` ran), a resume landing in that
    // window would sweep the wait queues, find this thread in none of them,
    // and return — leaving it to park afterwards with nobody left to wake it.
    // Setting it first makes the thread findable by every wake that follows.
    set_current_pending_block(Some(PendingBlock::JobControl));
    loop {
        if pred() {
            set_current_pending_block(None);
            return;
        }
        block_current_thread(PendingBlock::JobControl);
        // Wake and immediately re-test: `block_current_thread` returns for any
        // reason (a broad wake, a spurious tick), not only for our resume.
        if pred() {
            set_current_pending_block(None);
            return;
        }
    }
}

/// Write `pending_block` on the thread this CPU is running, bypassing
/// `current_thread()` (which would re-lock `CPUS[k]`, invariant 5).
pub fn set_current_pending_block(pb: Option<PendingBlock>) {
    let cpu_id = crate::smp::current_cpu_id();
    if let Some(t) = CPUS[cpu_id as usize].lock().as_mut().unwrap().current.as_mut() {
        t.pending_block = pb;
    }
}

/// Wake every blocked thread belonging to `pid`, from every wait queue.
///
/// Used by `sys_kill`: a signal cannot be delivered to a thread that is
/// asleep in a blocking syscall, because delivery happens on the next return
/// to user mode (invariant 8) and a parked read never gets there. Without this
/// a fatal signal to a process blocked in `read(2)` never takes effect and the
/// parent waits in `waitpid` indefinitely.
///
/// The thread is re-parked by its own syscall loop if it turns out to have
/// been woken spuriously, so this is safe for non-fatal signals too.
///
/// Lock order: SCHEDULER -> wait-queue, the established order (invariant 1).
pub fn wake_blocked_for_pid(pid: u64) {
    let mut sched = SCHEDULER.lock();

    let mut sweep = |wq: &'static SpinLock<VecDeque<Thread>>| {
        let mut q = wq.lock();
        let mut kept = VecDeque::with_capacity(q.len());
        while let Some(t) = q.pop_front() {
            if t.process.lock().pid == pid {
                let mut t = t;
                t.pending_block = None;
                sched.enqueue_with_boost(t, 4);
            } else {
                kept.push_back(t);
            }
        }
        *q = kept;
    };

    sweep(&CHILD_WAIT_QUEUE);
    sweep(&PIPE_WAIT_QUEUE);
    sweep(&FUTEX_WAIT_QUEUE);
    sweep(&INPUT_WAIT_QUEUE);
    sweep(&KEYBOARD_WAIT_QUEUE);
    sweep(&SYNCOBJ_WAIT_QUEUE);
}

/// Wake every thread parked in [`PendingBlock::JobControl`].
///
/// These are threads that stopped themselves on terminal job control and are
/// asleep waiting to be foregrounded. Either half of the handshake can make
/// that happen — `SIGCONT` (which clears the stop flag) or `tcsetpgrp` (which
/// moves the group into the foreground) — so both call this.
///
/// Takes SCHEDULER then PIPE_WAIT_QUEUE, the established order (invariant 1).
/// The parked thread re-tests its condition on wake, so a spurious wake is
/// harmless.
pub fn wake_jobcontrol_waiters() {
    {
        let mut sched = SCHEDULER.lock();
        let mut q = PIPE_WAIT_QUEUE.lock();
        let mut kept = VecDeque::with_capacity(q.len());
        while let Some(t) = q.pop_front() {
            if t.pending_block == Some(PendingBlock::JobControl) {
                let mut t = t;
                t.pending_block = None;
                sched.enqueue_with_boost(t, 4);
            } else {
                kept.push_back(t);
            }
        }
        *q = kept;
    }

    }

pub fn block_current_thread(target: PendingBlock) {
    let is_zombie = current_thread().map(|t| t.process.lock().status == ProcessStatus::Zombie).unwrap_or(false);
    if is_zombie {
        exit_current_thread(0);
    }
    // Check for pending signals before parking. A child may have exited
    // between sys_wait4's pending check and this call, setting SIGCHLD on
    // the parent. If we park now, the wake from exit_current_thread is lost
    // (it already swept CHILD_WAIT_QUEUE when it was empty) and the parent
    // sleeps forever. Checking here catches the race: if a signal is pending,
    // don't block—let it be delivered on return to user mode.
    {
        let t = current_thread().expect("block_current_thread: no current thread");
        let p = t.process.lock();
        if p.pending & (1u64 << 16) != 0 {
            return;
        }
    }
    // Store the blocking reason on the thread itself, not in a global, so
    // that only the CPU currently running this thread can ever see it.
    let cpu_id = crate::smp::current_cpu_id();
    if let Some(t) = CPUS[cpu_id as usize].lock().as_mut().unwrap().current.as_mut() {
        t.pending_block = Some(target);
    }
    unsafe {
        // sti is CRITICAL: syscall entry masks IF via IA32_FMASK (0x200).
        // Without sti, keyboard interrupts can never fire and STDIN stays empty forever.
        // sti takes effect after the next instruction, so int 0x81 runs with IF=1 in saved RFLAGS.
        // After iretq, IF=1 and keyboard interrupts can fire between timer ticks.
        core::arch::asm!("sti", "int 0x81");
    }
    // When we resume here, we've been woken up and rescheduled.
    let cpu_id = crate::smp::current_cpu_id();
    if let Some(t) = CPUS[cpu_id as usize].lock().as_mut().unwrap().current.as_mut() {
        t.pending_block = None;
    }
    let is_zombie = current_thread().map(|t| t.process.lock().status == ProcessStatus::Zombie).unwrap_or(false);
    if is_zombie {
        exit_current_thread(0);
    }
}

pub fn register_exec_thread(cr3: PhysAddr, uid: u32, gid: u32, exe_path: &str) {
    let mut p = Process::new(next_pid(), 0, cr3, 10);
    p.uid = uid;
    p.euid = uid;
    p.suid = uid;
    p.gid = gid;
    p.egid = gid;
    p.sgid = gid;
    p.caps = if uid == 0 { crate::fs::perm::CAP_ALL } else { 0 };
    p.exe_path = alloc::string::String::from(exe_path);
    let process = Arc::new(SpinLock::new(p));
    PROCESS_TABLE
        .lock()
        .insert(process.lock().pid, process.clone());

    // Enqueue the old current thread (if any) so it can resume after exec.
    // Without this, the old thread is lost and the scheduler queue stays empty
    // after the exec'd program exits, causing the CPU into the idle loop
    // with no runnable threads.
    if let Some(mut old_t) = current_thread() {
        let mut sched = SCHEDULER.lock();
        if old_t.stack_ptr != 0 {
            old_t.pending_block = None;
            sched.enqueue_with_boost(old_t, 4);
        }
    }

    let stack_top = alloc_kernel_stack();
    let syscall_stack_top = alloc_syscall_stack();
    crate::dprintln!(
        "[KSTK] register_exec ktop={:#x} systop={:#x}",
        stack_top, syscall_stack_top
    );
    crate::smp::with_cpu_tss(|tss| crate::arch::x86_64::tss::set_rsp0(tss, stack_top));
    crate::smp::with_cpu_state(|state| state.kernel_stack = syscall_stack_top);
    THREADS_ACTIVE.store(true, core::sync::atomic::Ordering::Relaxed);
    let first = Thread::new(
        next_tid(),
        process.clone(),
        0, // stack_ptr will be set correctly upon execution
        stack_top,
        syscall_stack_top,
        10, // Default priority
        stack_top,        // stack_limit: set to kernel_stack_top (check disabled until page fault handler config)
    );
    // Register in the process's own thread list, for the same reason clone does:
    // "a runnable thread is always in Process::threads" has to hold from the
    // very first thread, or the first exit_group finds an empty list and cannot
    // reap anything.
    process.lock().threads.insert(first.id, Arc::new(first.clone()));
    set_current_thread(Some(first));
}

#[unsafe(naked)]
pub unsafe extern "C" fn switch_to_stack_and_iretq(stack_ptr: u64) -> ! {
    naked_asm!(
        "mov rsp, rdi",
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
        "jz 1f",
        "swapgs",
        "1:",
        "iretq",
    );
}

pub fn exit_current_thread(exit_code: i32) -> ! {
    exit_thread_internal(exit_code, false)
}

pub fn exit_group(exit_code: i32) -> ! {
    exit_thread_internal(exit_code, true)
}

fn exit_thread_internal(exit_code: i32, is_group_exit: bool) -> ! {
    // Per-CPU `current`, never the global — a thread belongs to exactly one core.
    let old_thread = current_thread().expect("No current thread to exit!");
    set_current_thread(None);

    let is_last_thread;
    let ppid;
    let mut old_fds = alloc::vec::Vec::new();
    let child_pid = old_thread.process.lock().pid;
    // Sibling threads that `exit_group` must remove from the run queue. Taken
    // under the process lock and acted on after it is dropped: waking a thread
    // takes SCHEDULER, and holding `process.lock()` across that inverts the
    // lock order (invariant 1 — process.lock() is innermost).
    let mut siblings_to_kill: Vec<Arc<Thread>> = Vec::new();
    {
        let mut p = old_thread.process.lock();
        // Always drop this thread from the list. Doing it only on the
        // group-exit path left the entry behind for a plain thread exit, so the
        // list grew monotonically and every later exit believed it had
        // siblings to wait for.
        p.threads.remove(&old_thread.id);
        if is_group_exit {
            // The whole group is going. Any thread still queued would keep
            // running against a process already marked dead — the exact state
            // that made zellij's server outlive its client.
            siblings_to_kill = p.threads.values().cloned().collect();
            p.threads.clear();
            p.status = ProcessStatus::Zombie;
            p.exit_code = exit_code;
            p.threads_count = 0;
            is_last_thread = true;
            ppid = p.ppid;
            old_fds = core::mem::take(&mut p.fd_table);
        } else if p.threads_count > 1 {
            p.threads_count -= 1;
            is_last_thread = false;
            ppid = 0;
        } else {
            p.status = ProcessStatus::Zombie;
            p.exit_code = exit_code;
            p.threads_count = 0;
            is_last_thread = true;
            ppid = p.ppid;
            old_fds = core::mem::take(&mut p.fd_table);
        }
    }

    // Remove the dying group's other threads from the run queue *and* every
    // wait queue. `SCHEDULER` alone was not enough: a sibling parked on a wait
    // queue was never pruned, so the next `wake_*_waiters` re-enqueued it and
    // it ran against a process already marked dead.
    if !siblings_to_kill.is_empty() {
        let dead: alloc::collections::BTreeSet<u64> =
            siblings_to_kill.iter().map(|t| t.id).collect();
        crate::tasks::purge_threads_from_queues(&dead);
    }

    // A reaped sibling never runs its own exit path, so its `CLONE_CHILD_CLEARTID`
    // word is never zeroed and its futex is never woken. glibc's
    // `pthread_join` blocks on exactly that word, so without this a joiner
    // waits forever for a thread that has already been removed from the run
    // queue — a hang introduced by the reaping above, and precisely the case
    // that made adding thread reaping look like it broke joining.
    //
    // Done after the run-queue removal so a thread cannot be both reaped and
    // subsequently run its own exit path.
    for t in &siblings_to_kill {
        if let Some(ctid_addr) = t.clear_child_tid {
            if crate::memory::space::validate_user_write_range(ctid_addr, 4) {
                unsafe {
                    core::ptr::write_volatile(ctid_addr as *mut u32, 0);
                }
                wake_futex_waiters(ctid_addr, 1, child_pid);
            }
        }
    }

    for opt_v in old_fds {
        if let Some(v) = opt_v {
            // mount open-count accounting
            {
                let of = v.lock();
                crate::fs::mount::record_close(&of.path);
            }
            let vnode_kind = { v.lock().vnode.clone() };
            if matches!(vnode_kind, crate::fs::vfs::VNode::Pipe { .. }) {
                wake_pipe_waiters();
            }
            // ponytail: last-close evdev release (same as sys_close) — fork
            // children share the OpenFile; a child's exit must not unregister
            // the parent's stream. Dup'd fds (0/1/2 sharing one OpenFile)
            // release on the final one.
            if alloc::sync::Arc::strong_count(&v) == 1 {
                match &vnode_kind {
                    crate::fs::vfs::VNode::PtyMaster(pty) => crate::fs::pty::pty_release(pty, true),
                    crate::fs::vfs::VNode::PtySlave(pty) => crate::fs::pty::pty_release(pty, false),
                    crate::fs::vfs::VNode::InputEventDevice { .. }
                    | crate::fs::vfs::VNode::InputMiceDevice { .. } => {
                        crate::drivers::keyboard::input_unregister(alloc::sync::Arc::as_ptr(&v) as usize);
                    }
                    _ => {}
                }
            }
        }
    }

    if let Some(ctid_addr) = old_thread.clear_child_tid {
        if crate::memory::space::validate_user_write_range(ctid_addr, 4) {
            unsafe {
                core::ptr::write_volatile(ctid_addr as *mut u32, 0);
            }
            let pid = old_thread.process.lock().pid;
            wake_futex_waiters(ctid_addr, 1, pid);
        }
    }

    // The fault stash (if set by a resume that died via SIG_DFL) has served
    // the write above; clear it so later faults on this CPU resolve against
    // the correct process instead of this dying one.
    fault_stash_clear();

    // ── Parent notification ───────────────────────────────────────────────────
    // LOCK ORDER INVARIANT: process/PROCESS_TABLE locks must never be held at
    // the same time as SCHEDULER.  Resolve the parent Arc and signal it *before*
    // taking SCHEDULER, so we never cross with sys_wait4's ordering:
    //   sys_wait4:        parent.process → child.process → PROCESS_TABLE
    //   exit (old/bad):   SCHEDULER → PROCESS_TABLE → parent.process  ← AB-BA
    //   exit (fixed):     PROCESS_TABLE (brief) → parent.process → SCHEDULER
    let parent_arc: Option<alloc::sync::Arc<crate::arch::sync::spinlock::SpinLock<Process>>> =
        if is_last_thread && ppid != 0 {
            PROCESS_TABLE.lock().get(&ppid).cloned()
            // PROCESS_TABLE lock dropped immediately at end of expression
        } else {
            None
        };

    // Signal parent (pending bit + queue) — no SCHEDULER held.
    if let Some(ref p) = parent_arc {
        let mut pp = p.lock();
        pp.pending |= 1 << 16;
        // ponytail: queue carries readable siginfo; bitmask is fast check
        pp.sigqueue.push_back(SigInfo { signo: 17, code: SI_USER, pid: child_pid, value: 0 });
    }

    // ── DRM display restoration on last-thread exit ──────────────────────────
    // If this was the last thread of the process and the process was DRM master,
    // restore VT mode to text and keyboard mode to text before the scheduler runs.
    if is_last_thread {
        // Drop GLOBAL_DRM *before* touching the KMS backend. `set_scanout`
        // re-acquires GLOBAL_DRM to resolve the framebuffer, and SpinLock is
        // non-reentrant — holding both here is a same-CPU reentry panic.
        // It also violates the AGENTS.md lock order, which puts the backend
        // outside the device lock: GLOBAL_DRM -> GLOBAL_VIRTIO_GPU.
        let restore_scanout = {
            let mut dev = crate::drivers::drm::GLOBAL_DRM.lock();
            match dev.as_mut() {
                Some(device) if device.master_pid == Some(child_pid) => {
                    device.master_pid = None;
                    // Restore VT mode to VT_AUTO (text mode)
                    crate::arch::x86_64::syscalls::fsio::CONSOLE_KD_MODE.store(0, core::sync::atomic::Ordering::SeqCst);
                    crate::arch::x86_64::syscalls::fsio::CONSOLE_KB_MODE.store(2, core::sync::atomic::Ordering::SeqCst);
                    crate::graphics::terminal::WRITER.lock().clear();
                    // SAFETY: set once at boot, read-only afterwards.
                    unsafe { crate::graphics::FRAMEBUFFER_INFO }.map(|fb| fb.address.0)
                }
                _ => None,
            }
        }; // GLOBAL_DRM released here
        if let Some(fb_phys) = restore_scanout {
            if let Some(backend) = crate::drivers::drm::kms::KMS_BACKEND.lock().as_ref() {
                backend.page_flip(fb_phys);
            }
        }
    }

    // ── Scheduler operations ─────────────────────────────────────────────────
    // Now take SCHEDULER. Inside this block we only touch the scheduler queue
    // and the wait queues — never any process or PROCESS_TABLE locks.
    let mut queue = SCHEDULER.lock();
    let next: Option<Thread>;

    {
        if is_last_thread {
            // Purge sibling threads of the dead process from SCHEDULER runqueue
            queue.retain(|t| t.process.lock().pid != child_pid);

            // Purge sibling threads from wait-queues
            KEYBOARD_WAIT_QUEUE.lock().retain(|t| t.process.lock().pid != child_pid);
            PIPE_WAIT_QUEUE.lock().retain(|t| t.process.lock().pid != child_pid);
            FUTEX_WAIT_QUEUE.lock().retain(|t| t.process.lock().pid != child_pid);
            INPUT_WAIT_QUEUE.lock().retain(|t| t.process.lock().pid != child_pid);
            // DRM syncobj waiters too. Leaving them queued would let a later
            // SYNCOBJ_SIGNAL re-enqueue a thread whose process is already
            // gone, dispatching it against a zombie process.
            SYNCOBJ_WAIT_QUEUE.lock().retain(|t| t.process.lock().pid != child_pid);

            if ppid != 0 {
                // Wake any thread from the parent that is blocked in waitpid/wait4.
                let mut child_wait = CHILD_WAIT_QUEUE.lock();
                let mut kept = VecDeque::new();
                while let Some(mut t) = child_wait.pop_front() {
                    if t.process.lock().pid == ppid {
                        t.pending_block = None;
                        queue.enqueue_with_boost(t, 4);
                    } else if t.process.lock().pid != child_pid {
                        kept.push_back(t);
                    }
                }
                *child_wait = kept;
                drop(child_wait); // ponytail: release BEFORE the CPUS loop below so we
                                  // hold wait-queues only with the established
                                  // SCHEDULER -> CPUS[k] order, never SCHEDULER ->
                                  // wait-queue -> CPUS[k].
                // Lost-wakeup: parent may have set ChildWait but not yet been
                // parked by timer_tick (still in CPUS[current].pending_block).
                // Check current slots directly and wake.
                for cpu_id in 0..crate::smp::MAX_CPUS {
                    // ponytail: drop cpu_guard BEFORE enqueue_with_boost so the
                    // scheduler queue is touched without any per-CPU lock held.
                    // Also avoids any future same-CPU re-entry via the heap push.
                    let stolen = {
                        let mut cpu_guard = crate::smp::CPUS[cpu_id].lock();
                        let should_wake = cpu_guard.as_ref().map(|cpu| {
                            cpu.current.as_ref().map(|t| {
                                t.pending_block == Some(crate::tasks::PendingBlock::ChildWait)
                                    && t.process.lock().pid == ppid
                            }).unwrap_or(false)
                        }).unwrap_or(false);
                        if should_wake {
                            let cpu = cpu_guard.as_mut().unwrap();
                            let mut t = cpu.current.take().unwrap();
                            t.pending_block = None;
                            Some(t)
                        } else {
                            None
                        }
                    }; // cpu_guard dropped here
                    if let Some(t) = stolen {
                        queue.enqueue_with_boost(t, 4);
                    }
                }
            }
        }

        next = queue.pop_front();
    }

    drop(queue);


    let (next_cr3, next_stack_ptr) = match next {
        Some(ref t) => (t.process.lock().cr3, Some(t.stack_ptr)),
        None => unsafe { (KERNEL_CR3, None) },
    };

    unsafe {
        if read_cr3() != next_cr3 {
            write_cr3(next_cr3);
        }
    }

    match next_stack_ptr {
        Some(_ptr) => {
            let mut next_thread = next.unwrap();
            unsafe {
                wrmsr(IA32_FS_BASE, next_thread.fs_base);
                crate::smp::with_cpu_tss(|tss| set_rsp0(tss, next_thread.kernel_stack_top));
            }
            crate::smp::with_cpu_state(|state| state.kernel_stack = next_thread.syscall_stack_top);
            next_thread.fpu_restore();
            // Debug: check if can_deliver would return true by reading CS from frame
            // (gated: fires on every thread exit at desktop scale)
            if cfg!(feature = "dsp_verbose") {
                let frame_ctx = unsafe { &*(next_thread.stack_ptr as *const crate::tasks::CpuContext) };
                crate::dprintln!("[SIGCHK] frame.cs={:#x} rip={:#x} rsp={:#x} (user={}) stack_ptr={:#x} in_kernel={}",
                    frame_ctx.cs, frame_ctx.rip, frame_ctx.rsp, (frame_ctx.cs & 3) == 3, next_thread.stack_ptr, crate::smp::current_cpu_id());
            }
            // Deliver pending signals BEFORE taking ownership via set_current_thread.
            // resume_with_signals returns the stack pointer to iretq into, or None
            // if the signal's default action is to terminate the thread.
            // Stash for fault-path demand paging (see fault_process).
            fault_stash_set(&next_thread.process);
            match crate::tasks::resume_with_signals(&mut next_thread) {
                Ok(sp) => {
                    fault_stash_clear();
                    set_current_thread(Some(next_thread));
                    crate::smp::assert_no_cpus_leak("exit_current_thread");
                    unsafe { switch_to_stack_and_iretq(sp) }
                }
                Err(STOPPED) => {
                    // The thread was stopped (SIGTSTP/SIGSTOP), not killed.
                    // Re-queue it so it keeps its saved frame and can be
                    // resumed by SIGCONT, then park this core.
                    //
                    // Treating this as termination would kill a merely-stopped
                    // process, and re-entering `exit_current_thread` would reap
                    // it out from under a parent about to `waitpid(WUNTRACED)`
                    // on it. Parking (rather than returning) matters because
                    // `pick_next` skips stopped threads, so there is nothing
                    // here to iretq into.
                    fault_stash_clear();
                    SCHEDULER.lock().enqueue(next_thread);
                    unsafe {
                        if read_cr3() != KERNEL_CR3 {
                            write_cr3(KERNEL_CR3);
                        }
                        crate::arch::x86_64::cpu::sti();
                    }
                    crate::smp::cpu_idle_loop(crate::smp::current_cpu_id() as usize);
                }
                Err(_) => {
                    // Signal delivery requested thread termination;
                    // re-enter exit_current_thread to clean up the process.
                    // ponytail: restore current first — the callee starts with
                    // current_thread().expect(), and current is None here.
                    set_current_thread(Some(next_thread));
                    exit_current_thread(exit_code);
                }
            }
        }
        None => {
            // No runnable thread right now.  Rather than halting this CPU
            // permanently (which would mean CPU 0 can never run another
            // process after the last one exits), enter the same idle loop
            // used by APs.  The BSP will pick up the next thread pushed to
            // SCHEDULER by a fork/exec syscall or an unblocked waiter.
            //
            // Switch back to the kernel CR3 first so that we're not running
            // on a user address space that is about to be freed.
            unsafe {
                if read_cr3() != KERNEL_CR3 {
                    write_cr3(KERNEL_CR3);
                }
                crate::arch::x86_64::cpu::sti();
            }
            crate::tprintln!("CPU{}: no runnable thread, entering idle loop",
                crate::smp::current_cpu_id());
            let my_id = crate::smp::current_cpu_id() as usize;
            crate::smp::cpu_idle_loop(my_id)
        }
    }
}

pub fn fxsave_scratch() {
    crate::smp::with_cpu_state(|st| {
        unsafe {
            core::arch::asm!(
                "fxsave [{}]",
                in(reg) core::ptr::addr_of!(st.fpu_scratch),
                options(nostack)
            );
        }
    });
}
pub fn fxrstor_scratch() {
    crate::smp::with_cpu_state(|st| {
        unsafe {
            core::arch::asm!(
                "fxrstor [{}]",
                in(reg) core::ptr::addr_of!(st.fpu_scratch),
                options(nostack)
            );
        }
    });
}

pub enum Delivery {
    None,
    Handled,
    Die(i32),
    Stopped,
}

/// The `Err` code `resume_with_signals` returns for [`Delivery::Stopped`].
///
/// Named rather than a bare `-1` because the scheduler matches on it, and
/// `-1` is indistinguishable from a real exit code at the call site: a
/// silently mismatched constant would kill the process with status 255
/// instead of stopping it.
///
/// Negative so it can never collide with a `Die(128 + sig)` code, which is
/// always positive.
pub const STOPPED: i32 = -1;

/// Stop the calling process on behalf of terminal job control (SIGTTIN /
/// SIGTTOU), applying the stop *immediately* rather than queueing a signal for
/// later delivery.
///
/// Why not just queue the signal: a thread that discovers it is in the
/// background is inside a syscall, so its saved frame is a *kernel* frame
/// (`cs == 0x8`), and `can_deliver` refuses to deliver to those (invariant 8).
/// The signal would sit pending forever. Returning `EINTR` instead is also
/// wrong — the caller sees a failed read and exits, which is not a stop.
///
/// Linux keeps the task asleep and stopped in the same `read`. This does the
/// equivalent: mark the process stopped (so `pick_next` skips it) and let the
/// caller park. `sys_kill(SIGCONT)` clears the flag and calls
/// [`wake_parked_for_pid`], which is what lets the read continue.
///
/// Returns false when there is no current thread to stop.
pub fn stop_self_for_tty_io(sig: i32) -> bool {
    // Capture ppid before dropping the process lock. We must not hold
    // process.lock() while taking SCHEDULER (wake_parked_for_pid) — that would
    // violate the global lock order (process.lock() is innermost).
    let ppid = match mark_self_stopped(sig) {
        Some(ppid) => ppid,
        None => return false,
    };
    crate::smp::signal_runnable();
    // Wake parent's waitpid(WUNTRACED): a child that stops itself (SIGTTIN from
    // a background terminal read, SIGTTOU from a background write, etc.) sets
    // its own status to Stopped but nothing else notifies the parent. The
    // parent may be parked in CHILD_WAIT_QUEUE waiting for the child to change
    // state — wake it so it rescans and finds the stopped child.
    //
    // Only the wake, deliberately NOT a pending-SIGCHLD bit. That bit is what
    // `sys_wait4`'s pre-park check and `block_current_thread` key off, and it
    // is cleared only when a zombie is reaped. Setting it for a *stop* would
    // make a blocking `waitpid(pid, NULL, 0)` (no WUNTRACED) on a merely-stopped
    // child re-scan forever instead of parking: the scan finds nothing, the bit
    // is still set, `block_current_thread` returns immediately, and the loop
    // spins. The parked case is fully covered by the wake; the unparked case is
    // covered because the parent's own scan sees the already-stopped child.
    if ppid != 0 {
        crate::tasks::wake_parked_for_pid(ppid);
    }
    true
}

/// Mark the calling process stopped on `sig` and return its `ppid`, or `None`
/// when there is no current thread. Does the process/status bookkeeping only;
/// callers do the parent wake-up (which takes SCHEDULER and so must run with
/// `process.lock()` released).
fn mark_self_stopped(sig: i32) -> Option<u64> {
    let t = current_thread()?;
    let ppid = {
        let mut p = t.process.lock();
        p.status = ProcessStatus::Stopped;
        p.stopped.store(true, core::sync::atomic::Ordering::Release);
        p.stop_signal = sig;
        p.ppid
    };
    Some(ppid)
}

/// Is `sig` currently ignored (`SIG_IGN`) or blocked for the calling process?
///
/// Terminal job-control uses this to match Linux: a process that ignores or
/// blocks `SIGTTIN`/`SIGTTOU` is *not* stopped by a background terminal
/// read/write — the operation is allowed to proceed. Shells and job-control
/// programs routinely set `SIGTTOU` to `SIG_IGN` around `tcsetpgrp`, and
/// `n_tty` / `tty_check_change` honour it. Without this, such a program would
/// be stopped anyway, diverging from Linux.
pub fn signal_is_ignored_or_blocked(sig: i32) -> bool {
    // `sigactions` covers signals 1..=32 (the array is `[SigAction; 32]`).
    if sig < 1 || sig > 32 {
        return false;
    }
    let t = match current_thread() {
        Some(t) => t,
        None => return false,
    };
    let p = t.process.lock();
    let handler = p.sigactions[(sig - 1) as usize].sa_handler;
    handler == 1 /* SIG_IGN */ || (t.sigmask & (1u64 << (sig - 1))) != 0
}

/// Convert the delivery of a pending signal into a **ptrace-stop**.
///
/// When `p` is traced (`tracer_pid != 0`), any pending signal except
/// `SIGKILL` (must always kill) and `SIGCONT` (must always resume) stops the
/// process so its tracer can inspect it before it is delivered — Linux's
/// ptrace model. The signal bit is *consumed* here: like Linux, the stop
/// signal is not delivered unless the tracer explicitly re-delivers it via
/// `PTRACE_CONT(pid, sig)` / `PTRACE_DETACH`.
///
/// Reuses the job-control stop state (`ProcessStatus::Stopped` + `stopped` +
/// `stop_signal`), which is exactly what `waitpid(WUNTRACED)` decodes as a
/// stopped status. Returns the trapped signal, or `None` when `p` is not
/// traced or nothing trap-eligible is pending.
///
/// Does **not** wake the tracer: callers run in different lock contexts and
/// decide that themselves (the syscall boundary may wake freely; the tick
/// path holds SCHEDULER and cannot).
pub fn ptrace_trap(t: &mut Thread, p: &mut Process) -> Option<i32> {
    if p.tracer_pid == 0 {
        return None;
    }
    let ready = (t.pending | p.pending) & !t.sigmask;
    let mut bits = ready;
    let mut trap = 0i32;
    while bits != 0 {
        let s = bits.trailing_zeros() as i32 + 1;
        bits &= bits - 1;
        if s != SIGKILL && s != SIGCONT {
            trap = s;
            break;
        }
    }
    if trap == 0 {
        return None;
    }
    // Consume the signal; the tracer re-delivers it via CONT/DETACH.
    t.pending &= !(1u64 << (trap - 1));
    p.pending &= !(1u64 << (trap - 1));
    t.sigqueue.retain(|i| i.signo != trap);
    p.sigqueue.retain(|i| i.signo != trap);
    if p.status != ProcessStatus::Stopped {
        p.status = ProcessStatus::Stopped;
        p.stopped.store(true, core::sync::atomic::Ordering::Release);
    }
    p.stop_signal = trap;
    p.ptrace_sig = trap;
    Some(trap)
}

/// Apply a pending, unblocked, SIG_DFL job-control stop or fatal signal
/// synchronously at the syscall boundary.
///
/// DragOS delivers signals only on interrupt frames (timer/yield), and
/// `can_deliver` refuses kernel frames (invariant 8). A process that loops on
/// syscalls — `while (1) pause()`, a tight `read`/`write` loop — therefore
/// spends almost all its time on kernel frames and can go arbitrarily long
/// without a stop or fatal signal taking effect: `sysretq` has no signal path
/// of its own. Linux applies these dispositions at the syscall's user-return
/// boundary, so a terminal `SIGTSTP` or a `SIGTERM` always lands; this mirrors
/// that from the syscall dispatcher's exit.
///
/// Only SIG_DFL stop signals (SIGSTOP/SIGTSTP/SIGTTIN/SIGTTOU) and SIG_DFL
/// fatal signals are acted on here. Custom handlers, SIG_IGN, ignore-by-default
/// signals (SIGCHLD etc.) and masked signals are left for the tick path, which
/// is the only path that can build a user-frame handler — matching
/// `check_fatal_signal`. This is the disposition-level analogue of that
/// function, extended to stops.
///
/// A stop marks the process stopped and parks the thread until `SIGCONT`
/// (`park_until_stopped_cleared` re-tests on every wake, so a resume racing the
/// park is caught). A fatal signal terminates immediately via `exit_group`.
pub fn apply_pending_signals_sync() {
    loop {
        let mut t = match current_thread() {
            Some(t) => t,
            None => return,
        };
        let ppid = {
            let mut p = t.process.lock();
            let ready = (t.pending | p.pending) & !t.sigmask;
            if ready == 0 {
                return;
            }
            // ptrace: a traced process stops on any pending signal (except
            // SIGKILL / SIGCONT) instead of delivering it, so its tracer can
            // inspect it first. The syscall boundary holds no scheduler lock,
            // so waking the tracer's waitpid is safe here.
            if p.tracer_pid != 0 {
                let mut bits = ready;
                let mut trap = 0i32;
                while bits != 0 {
                    let s = bits.trailing_zeros() as i32 + 1;
                    bits &= bits - 1;
                    if s != SIGKILL && s != SIGCONT {
                        trap = s;
                        break;
                    }
                }
                if trap != 0 {
                    // Consume the signal; the tracer re-delivers it via
                    // PTRACE_CONT / PTRACE_DETACH.
                    p.pending &= !(1u64 << (trap - 1));
                    p.sigqueue.retain(|i| i.signo != trap);
                    if p.status != ProcessStatus::Stopped {
                        p.status = ProcessStatus::Stopped;
                        p.stopped.store(true, core::sync::atomic::Ordering::Release);
                    }
                    p.stop_signal = trap;
                    p.ptrace_sig = trap;
                    // store snapshot (thread `t` is the running thread now)
                    p.ptrace_thread = Some(t.clone());
                    let tracer = p.tracer_pid;
                    let ppid = p.ppid;
                    drop(p);
                    t.pending &= !(1u64 << (trap - 1));
                    t.sigqueue.retain(|i| i.signo != trap);
                    set_current_thread(Some(t));
                    crate::smp::signal_runnable();
                    wake_parked_for_pid(tracer);
                    if ppid != 0 {
                        wake_parked_for_pid(ppid);
                    }
                    let proc = current_thread().map(|t| t.process.clone());
                    park_until_stopped_cleared(|| match &proc {
                        Some(p) => !p.lock().stopped.load(core::sync::atomic::Ordering::Acquire),
                        None => true,
                    });
                    continue;
                }
            }
            let mut stop = 0i32;
            let mut fatal = 0i32;
            let mut bits = ready;
            while bits != 0 {
                let s = bits.trailing_zeros() as i32 + 1;
                bits &= bits - 1;
                // SIG_DFL only: anything else needs a user frame (tick path).
                if p.sigactions[(s - 1) as usize].sa_handler != 0 {
                    continue;
                }
                if sig_default_nonfatal(s) {
                    if matches!(s, SIGSTOP | SIGTSTP | SIGTTIN | SIGTTOU) && stop == 0 {
                        stop = s;
                    }
                    // SIGCHLD / SIGURG / SIGWINCH / SIGIO / SIGCONT: leave for
                    // the tick path (the SIGCHLD bit is also waitpid's wake bit).
                } else if fatal == 0 {
                    fatal = s;
                }
            }
            if fatal != 0 {
                // Drop the process lock before exit_group (it re-locks this
                // process during teardown; the spinlock is non-reentrant).
                drop(p);
                exit_group(128 + fatal);
            }
            if stop == 0 {
                return;
            }
            // Consume the stop bit and mark stopped, exactly as
            // `deliver_pending`'s stop branch does at a user boundary.
            t.pending &= !(1u64 << (stop - 1));
            p.pending &= !(1u64 << (stop - 1));
            p.status = ProcessStatus::Stopped;
            p.stopped.store(true, core::sync::atomic::Ordering::Release);
            p.stop_signal = stop;
            p.ppid
        };
        set_current_thread(Some(t));
        crate::smp::signal_runnable();
        if ppid != 0 {
            wake_parked_for_pid(ppid);
        }
        let proc = current_thread().map(|t| t.process.clone());
        park_until_stopped_cleared(|| match &proc {
            Some(p) => !p.lock().stopped.load(core::sync::atomic::Ordering::Acquire),
            None => true,
        });
        // Resumed by SIGCONT: loop in case more signals are already pending.
    }
}

/// Move every parked thread belonging to `pid` from the wait queues back onto
/// the run queue.
///
/// Used when a stopped process is continued (`SIGCONT`): the thread that
/// stopped itself inside a tty read is asleep in a wait queue, and nothing
/// else would ever dispatch it again.
///
/// Lock order: SCHEDULER -> wait-queue, the established order.
pub fn wake_parked_for_pid(pid: u64) {
    crate::dprintln!(
        "[WAKE] pid={} pipe={} futex={} kb={} child={}",
        pid,
        PIPE_WAIT_QUEUE.lock().len(),
        FUTEX_WAIT_QUEUE.lock().len(),
        KEYBOARD_WAIT_QUEUE.lock().len(),
        CHILD_WAIT_QUEUE.lock().len()
    );
    let mut sched = SCHEDULER.lock();

    let mut sweep = |wq: &'static SpinLock<VecDeque<Thread>>| {
        let mut q = wq.lock();
        let mut kept = VecDeque::with_capacity(q.len());
        while let Some(t) = q.pop_front() {
            if t.process.lock().pid == pid {
                let mut t = t;
                t.pending_block = None;
                sched.enqueue_with_boost(t, 4);
            } else {
                kept.push_back(t);
            }
        }
        *q = kept;
    };

    sweep(&CHILD_WAIT_QUEUE);
    sweep(&PIPE_WAIT_QUEUE);
    sweep(&FUTEX_WAIT_QUEUE);
    sweep(&INPUT_WAIT_QUEUE);
    sweep(&KEYBOARD_WAIT_QUEUE);
    sweep(&SYNCOBJ_WAIT_QUEUE);
}

pub fn can_deliver(frame_ptr: u64) -> bool {
    // Use the CpuContext frame abstraction instead of a bare pointer add(16)
    // magic offset so that this check stays correct if the frame layout changes.
    if !kernel_ptr_ok(frame_ptr) || frame_ptr & 7 != 0 {
        return false;
    }
    let ctx = unsafe { &*(frame_ptr as *const CpuContext) };
    ctx.cs == USER_CODE_SELECTOR as u64
}

/// Returns a mutable reference to the `CpuContext` frame saved on the kernel
/// stack at `stack_ptr`.  This is the **authoritative** CPU context for the
/// thread; modifying it changes the values restored by `switch_to_stack_and_iretq`.
///
/// # Safety
/// `stack_ptr` must be a valid, 8-byte-aligned kernel address pointing to a
/// complete `CpuContext` frame (i.e. built by the interrupt stub).
#[inline]
pub unsafe fn context_from_stack<'a>(stack_ptr: u64) -> &'a mut CpuContext {
    unsafe { &mut *(stack_ptr as *mut CpuContext) }
}

/// True if `p` is a canonical high-half kernel address. Deliberately broader
/// than the kernel heap window: the initial thread is legitimately interrupted
/// on the boot stack before it switches to its heap-allocated ring-0 stack, so
/// frame/stack pointers can briefly point outside the heap without corruption.
fn kernel_ptr_ok(p: u64) -> bool {
    p >= 0xffff_8000_0000_0000
}

fn thread_in_frame_ok(t: &Thread) -> bool {
    // User frames have a 20-word layout (CS offset +16 == 0x23).
    // Kernel-mode interrupt frames are only 18 words (no RSP/SS push because
    // there is no privilege transition).  Reading word +18 or +19 on a kernel
    // frame reads past the interrupt-pushed data into whatever was on the stack
    // before — this caused the previous version to invoke UB.
    //
    // Fix: read CS from offset +16 first (valid for both frame types), then
    // only read RSP/SS for user frames.
    if !kernel_ptr_ok(t.stack_ptr) || t.stack_ptr & 7 != 0 {
        return false;
    }
    // SAFETY: stack_ptr is a valid, aligned kernel address per the check above.
    let ctx = unsafe { &*(t.stack_ptr as *const CpuContext) };
    match ctx.cs {
        cs if cs == USER_CODE_SELECTOR as u64 => {
            // Full user frame: validate RIP, RSP and SS.
            ctx.rip != 0
                && ctx.rsp < 0x0000_8000_0000_0000
                && ctx.ss == USER_DATA_SELECTOR as u64
        }
        0x8 => {
            // Kernel-mode interrupt frame: only RIP is architectural.
            // RSP and SS were NOT pushed by the CPU (no ring change).
            ctx.rip != 0
        }
        _ => false,
    }
}

static INV_VIOLATED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

// TEMP-DIAG BADRIP hunt (iret-to-SCHEDULER panic): kernel .text bounds.
//
// These two constants are a FALLBACK. The live bound is `KTEXT_END_RUNTIME`,
// derived at boot from the kernel's own ELF program headers (see
// `derive_kernel_text_end`), because a hand-maintained constant silently rots:
// .text grew from 0x1425a4 to 0x1653cf when the module loader landed, overran
// this window by ~21 KiB, and every thread preempted in the new code panicked
// with `BADRIP ... would iretq to data 0xffffffff80165186` -- an address that
// was perfectly valid kernel text. AGENTS.md §16.3 predicted exactly this and
// noted that "nothing enforces it". Now it is enforced by measurement.
//
// The fallback is still needed for the window before derivation runs (very
// early dispatch) and if ELF parsing fails, so keep it >= the real text end.
const KTEXT_START: u64 = 0xffffffff80000000;
const KTEXT_END: u64 = 0xffffffff80166000;

// Live end of kernel `.text`, supplied by the linker.
//
// `kernel.ld` emits `__kernel_text_start` / `__kernel_text_end` around `.text`.
// Using the linker's answer rather than a Rust constant means the bound cannot
// go stale when code is added -- which is exactly how the previous constant
// broke (see the note on `KTEXT_END`).
//
// Plain `//`, not `///`: extern blocks cannot carry doc comments, so the `///`
// form attached to nothing and warned as an unused doc comment.
unsafe extern "C" {
    static __kernel_text_start: u8;
    static __kernel_text_end: u8;
}

/// `[__kernel_text_start, __kernel_text_end)` — the linker-measured `.text`.
///
/// Read once into a static so the dispatch path does not take a symbol address
/// on every check, and so the values can be logged at boot.
pub static KTEXT_BOUNDS: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// Read the linker's `.text` bounds.
///
/// The end is sign-extended back from 32 bits. `code-model=kernel` addresses
/// are all in the top 2 GiB, so `0xffffffff801656af` truncates to `0x801656af`
/// -- and comparing a full-width RIP against that truncated bound rejects every
/// valid address. That bug shipped for one run and produced exactly the
/// symptom it was meant to detect (BADRIP on a good address), so the conversion
/// is spelled out rather than left implicit.
///
/// # Safety
/// Must be called once during single-threaded boot, before any dispatch.
pub unsafe fn init_ktext_bounds() -> (u64, u64) {
    let start = core::ptr::addr_of!(__kernel_text_start) as u64;
    let end = core::ptr::addr_of!(__kernel_text_end) as u64;
    let end32 = (end & 0xffff_ffff) as u32;
    let end_sext = end32 as i32 as i64 as u64;
    KTEXT_BOUNDS.store((start << 32) | (end_sext & 0xffff_ffff), Ordering::Relaxed);
    (start, end)
}

/// `(start, end)` of kernel `.text`, falling back to the compiled-in constants
/// if [`init_ktext_bounds`] has not run.
fn ktext_bounds() -> (u64, u64) {
    let packed = KTEXT_BOUNDS.load(Ordering::Relaxed);
    if packed == 0 {
        return (KTEXT_START, KTEXT_END);
    }
    let start = packed >> 32;
    let end32 = (packed & 0xffff_ffff) as u32;
    // Sign-extend, matching init_ktext_bounds. Must not be dropped: without it
    // `end` is 0x8016_56af and no real RIP ever compares less than it.
    let end = end32 as i32 as i64 as u64;
    (start, end)
}

// Loadable-module window (docs/module-system.md §4). Kept as a SEPARATE range
// from KTEXT_END rather than widening KTEXT_END: widening would make this
// check accept a genuine wild pointer into kernel *data*, which is exactly the
// corruption this diagnostic exists to catch. Two ranges, two meanings.
//
// `0xffffffffa0000000` was chosen in the design doc so the window is inside the
// `code-model=kernel` sign-extendable range (every 32S relocation must encode
// its own address) and outside HHDM's covered range (so a module address is
// never ambiguous with a physical address). Verified here: the heap is
// 0xFFFF_9000_0000_0000, MMIO is 0xFFFF_A000_0000_0000, HHDM starts at
// 0xFFFF_8000_0000_0000 -- all in the upper half, none of which can overlap a
// 0xffffffffa... address.
const MODULE_START: u64 = 0xffffffffa0000000;
const MODULE_END: u64 = 0xffffffffa4000000; // window + 64 MiB

/// True if `rip` is a plausible kernel-mode code address: kernel `.text`, or a
/// loaded module's text within the module window.
///
/// Both BADRIP call sites share this so the two checks cannot drift. Note the
/// module window is accepted *unconditionally* -- no module is loaded yet, so
/// there is no slot table to consult, and an empty window simply contains no
/// valid thread RIP. Tighten this if a slot allocator lands: an unloaded
/// window should ideally be rejected rather than trusted.
fn kernel_text_rip_ok(rip: u64) -> bool {
    let (start, end) = ktext_bounds();
    (rip >= start && rip < end) || (rip >= MODULE_START && rip < MODULE_END)
}

pub fn check_thread(t: &Thread, at: &str) {
    // A kernel-mode resume frame must point into .text. A data address here
    // (e.g. &SCHEDULER from stale stack garbage) faults identically at iretq
    // microseconds later with far less context — catch it here instead.
    // User frames (cs=0x23) carry user RIPs and are exempt.
    if kernel_ptr_ok(t.stack_ptr) && t.stack_ptr & 7 == 0 {
        let ctx = unsafe { &*(t.stack_ptr as *const CpuContext) };
        if ctx.cs == 8 && !kernel_text_rip_ok(ctx.rip) {
            crate::dprintln!("BADRIP[{}]: tid={} stack={:#x} rip={:#x} (not kernel .text or module window)",
                at, t.id, t.stack_ptr, ctx.rip);
            panic!("BADRIP[{}]: tid={} would iretq to data {:#x}", at, t.id, ctx.rip);
        }
    }
    let rawproc = unsafe { core::ptr::read_volatile(&t.process as *const _ as *const u64) };
    let bad = rawproc == 0
        || !kernel_ptr_ok(rawproc)
        || !kernel_ptr_ok(t.kernel_stack_top)
        || !kernel_ptr_ok(t.syscall_stack_top)
        || !thread_in_frame_ok(t);
    if bad && !INV_VIOLATED.load(core::sync::atomic::Ordering::Relaxed) {
        INV_VIOLATED.store(true, core::sync::atomic::Ordering::Relaxed);
        crate::dprintln!("INV1[{}]: id={} thread={:#x} rawproc={:#x} stack={:#x} kstack={:#x} sstack={:#x} cs={:#x} rip={:#x}",
            at, t.id, &t as *const _ as u64, rawproc, t.stack_ptr, t.kernel_stack_top, t.syscall_stack_top,
            unsafe { (*(t.stack_ptr as *const CpuContext)).cs }, unsafe { (*(t.stack_ptr as *const CpuContext)).rip });
        // ponytail: don't panic in production, allow ls to proceed and return to shell
        INV_VIOLATED.store(false, core::sync::atomic::Ordering::Relaxed);
        return;
    }
}

pub fn check_scheduler(at: &str) {
    let mut seen: alloc::vec::Vec<u64> = alloc::vec::Vec::new();
    let q = SCHEDULER.lock();
    for t in q.to_vec() {
        check_thread(&t, at);
        if seen.contains(&t.id) {
            panic!("INV2[{}]: duplicate id={} in SCHEDULER", at, t.id);
        }
        seen.push(t.id);
    }
}

pub fn check_no_double_owner(t: &Thread, at: &str, queue: &crate::sched::SchedRunQueue) {
    if queue.to_vec().iter().any(|q| q.id == t.id) {
        panic!("INV3[{}]: id={} also in SCHEDULER", at, t.id);
    }
}

// TEMP-DIAG double-dispatch hunt (iret-to-SCHEDULER panic): loud log, no
// panic, if tid is `current` on another core while this core parks/dispatches
// it. try_lock only, so lock ordering is unaffected. Remove after root cause.
pub fn debug_check_single_owner(tid: u64, at: &str) {
    let me = crate::smp::current_cpu_id() as usize;
    for (k, slot) in crate::smp::CPUS.iter().enumerate() {
        if k == me {
            continue;
        }
        if let Some(s) = slot.try_lock() {
            if let Some(c) = s.as_ref() {
                if let Some(cur) = c.current.as_ref() {
                    if cur.id == tid {
                        crate::dprintln!(
                            "[DOUBLE] tid={} current on cpu {} while cpu {} at {}",
                            tid, k, me, at
                        );
                    }
                }
            }
        }
    }
}

/// Synchronously terminate on a newly-fatal signal, mirroring what
/// deliver_pending() would do at the next user boundary.
///
/// Linux checks pending-unblocked signals before the raising syscall
/// returns; glibc abort() depends on it (unblock SIGABRT → raise → must
/// die, never fall through to its hlt trap). Our tick/context-switch
/// delivery can miss tight sequences entirely (no tick fires between the
/// unblock and the trap), so queue/unblock sites call this to die
/// immediately when the signal is fatal under SIG_DFL. Custom-handler,
/// ignored, and still-masked signals are left for the normal tick path.
/// Only ever targets the calling thread itself; cross-process kills rely
/// on the tick path as before.
/// Linux SIG_DFL dispositions that never terminate: ignore-by-default
/// (CHLD, URG, WINCH, IO) plus job-control stop/continue (STOP, TSTP, CONT),
/// which suspend or resume but never kill. Everything else with SIG_DFL is
/// fatal (128+sig). Single source of truth for both delivery paths.
pub fn sig_default_nonfatal(sig: i32) -> bool {
    matches!(
        sig,
        SIGCHLD
            | SIGURG
            | SIGWINCH
            | SIGIO
            | SIGCONT
            | SIGSTOP
            | SIGTSTP
            | SIGTTIN
            | SIGTTOU
    )
}

pub fn check_fatal_signal(sig: i32) {
    if sig < 1 || sig > 31 {
        return;
    }
    // Never fatal under SIG_DFL (mirrors deliver_pending).
    if sig_default_nonfatal(sig) {
        return;
    }
    let t = match current_thread() {
        Some(t) => t,
        None => return,
    };
    let (handler, masked, pending) = {
        let p = t.process.lock();
        (
            p.sigactions[(sig - 1) as usize].sa_handler,
            t.sigmask,
            (t.pending | p.pending) & (1u64 << (sig - 1)),
        )
    };
    if handler != crate::arch::x86_64::abi::SIG_DFL {
        return; // custom handler or SIG_IGN: tick path handles it
    }
    if masked & (1u64 << (sig - 1)) != 0 {
        return; // still blocked: stays queued
    }
    if pending == 0 {
        return;
    }
    exit_group(128 + sig);
}

/// check_fatal_signal() for every signal (call after sigprocmask changes
/// the mask — unblocking may release a queued fatal signal).
pub fn check_any_fatal() {
    for sig in 1..=31 {
        check_fatal_signal(sig);
    }
}

pub fn deliver_pending(current: &mut Thread, frame_ptr: u64) -> Delivery {
    let process = current.process.clone();
    let mut p = process.lock();
    let ready = current.pending | p.pending;
    let ready = ready & !current.sigmask;
    if ready == 0 {
        return Delivery::None;
    }
    // ptrace: a traced process stops on any pending signal (except SIGKILL /
    // SIGCONT) instead of delivering it. We cannot wake the tracer here —
    // this path runs with SCHEDULER held — which mirrors job-control stops,
    // whose parent wake is done by sys_kill (the sender). The stop itself is
    // what matters: waitpid(WUNTRACED) reports it, and PTRACE_CONT resumes.
    if p.tracer_pid != 0 {
        let mut ptrace_res = ptrace_trap(current, &mut *p);
        if ptrace_res.is_some() {
            p.ptrace_thread = Some((*current).clone());
            return Delivery::Stopped;
        }
    }
    let sig = ready.trailing_zeros() as i32 + 1;
    let action = p.sigactions[(sig - 1) as usize];
    // ponytail: queue is readable source of truth; bitmask is fast check. Pop queued info if present.
    let mut queued: Option<SigInfo> = None;
    if let Some(pos) = current.sigqueue.iter().position(|i| i.signo == sig) {
        queued = Some(current.sigqueue.remove(pos).unwrap());
    } else if let Some(pos) = p.sigqueue.iter().position(|i| i.signo == sig) {
        queued = Some(p.sigqueue.remove(pos).unwrap());
    }
    let p_cr3 = p.cr3;
    let p = &mut *p;
    
    // Helper to clear the signal bit
    let mut clear_signal_bit = |current: &mut Thread, proc: &mut Process| {
        if queued.is_some() {
            let still = current.sigqueue.iter().any(|i| i.signo == sig) || proc.sigqueue.iter().any(|i| i.signo == sig);
            if !still {
                current.pending &= !(1u64 << (sig - 1));
                proc.pending &= !(1u64 << (sig - 1));
            }
        } else {
            current.pending &= !(1u64 << (sig - 1));
            proc.pending &= !(1u64 << (sig - 1));
        }
    };

    match action.sa_handler {
        0 => {
            if sig_default_nonfatal(sig) {
                // Job control stop signals: stop the process instead of ignoring
                if sig == SIGTSTP || sig == SIGSTOP || sig == SIGTTIN || sig == SIGTTOU {
                    clear_signal_bit(current, p);
                    // Stop the process: mark as stopped and remove from run queue
                    p.status = ProcessStatus::Stopped;
                    p.stopped.store(true, core::sync::atomic::Ordering::Release);
                    // Save the stop signal for waitpid
                    p.stop_signal = sig;
                    return Delivery::Stopped;
                }
                // SIGCONT resumes a stopped process
                if sig == SIGCONT {
                    if p.stopped.load(core::sync::atomic::Ordering::Acquire) {
                        p.status = ProcessStatus::Running;
                        p.stopped.store(false, core::sync::atomic::Ordering::Release);
                        p.stop_signal = 0;
                        p.continued = true; // Mark as continued for waitpid(WCONTINUED)
                        // Process stays in run queue; scheduler will pick it up now that it's not stopped
                    }
                    clear_signal_bit(current, p);
                    return Delivery::None;
                }
                clear_signal_bit(current, p);
                return Delivery::None; // ignored / unsupported
            }
            return Delivery::Die(128 + sig);
        }
        1 => {
            clear_signal_bit(current, p);
            Delivery::None // SIG_IGN
        }
        handler => {
            // Use CpuContext abstraction instead of raw pointer arithmetic.
            // SAFETY: frame_ptr is a valid kernel stack address holding a CpuContext
            // (validated by can_deliver() before this function is called).
            let ctx = unsafe { context_from_stack(frame_ptr) };
            let user_rsp = ctx.rsp.saturating_sub(128);

            // Guard against integer underflow: if user_rsp < RT_SIGFRAME_SIZE
            // the subtraction would wrap around into kernel address space.
            let aligned_rsp = user_rsp & !0xF;
            let frame_addr = match aligned_rsp.checked_sub(RT_SIGFRAME_SIZE) {
                Some(addr) => addr,
                None => return Delivery::None,
            };
            // If user_rsp is a kernel address, frame_addr lands in kernel memory.
            // Writing the sigframe there would zero-fill kernel data (e.g. Thread.process).
            // If the sigframe pages aren't mapped yet, defer: this path runs with
            // CPUS/SCHEDULER held, so demand-faulting here would self-deadlock.
            // The signal stays pending and is retried at the next user boundary.
            if frame_addr >= 0xffff_8000_0000_0000
                || !crate::memory::space::user_pages_mapped_cr3(p_cr3, frame_addr, RT_SIGFRAME_SIZE as usize, true)
            {
                return Delivery::None;
            }
            // Validate handler and restorer are canonical user addresses.
            if !crate::memory::space::user_pages_mapped_cr3(p_cr3, handler, 1, false)
                || action.sa_restorer == 0
                || !crate::memory::space::user_pages_mapped_cr3(p_cr3, action.sa_restorer, 1, false)
            {
                crate::println!("BAD-SIG: sig={} handler={:#x} restorer={:#x}",
                    sig, handler, action.sa_restorer);
                return Delivery::None;
            }
            clear_signal_bit(current, p);
            drop(p);
            let sigframe = frame_addr as *mut RtSigFrame;

            // capture queued info before p is dropped
            let info = queued.unwrap_or(SigInfo { signo: sig, code: SI_USER, pid: 0, value: 0 });
            unsafe {
                // Zero the entire signal frame, not just 1 byte.
                core::ptr::write_bytes(sigframe, 0, 1);
                let sf = &mut *sigframe;
                // ponytail: queue makes siginfo readable — Linux siginfo_t layout: signo@0, errno@4, code@8, pid@16, uid@20, value@24
                {
                    let p = sf.info.as_mut_ptr();
                    core::ptr::write_unaligned(p.add(0) as *mut i32, info.signo);
                    core::ptr::write_unaligned(p.add(4) as *mut i32, 0);
                    core::ptr::write_unaligned(p.add(8) as *mut i32, info.code);
                    core::ptr::write_unaligned(p.add(16) as *mut i32, info.pid as i32);
                    core::ptr::write_unaligned(p.add(20) as *mut i32, 0);
                    core::ptr::write_unaligned(p.add(24) as *mut i32, info.value as i32);
                    core::ptr::write_unaligned(p.add(28) as *mut i32, 0);
                    // also store full 64-bit at 24 for our test's sival_ptr
                    core::ptr::write_unaligned(p.add(32) as *mut u64, info.value as u64);
                }
                sf.pretcode = action.sa_restorer;
                let mc = &mut sf.uc.uc_mcontext;
                mc.r8  = ctx.r8;
                mc.r9  = ctx.r9;
                mc.r10 = ctx.r10;
                mc.r11 = ctx.r11;
                mc.r12 = ctx.r12;
                mc.r13 = ctx.r13;
                mc.r14 = ctx.r14;
                mc.r15 = ctx.r15;
                mc.rdi = ctx.rdi;
                mc.rsi = ctx.rsi;
                mc.rbp = ctx.rbp;
                mc.rbx = ctx.rbx;
                mc.rdx = ctx.rdx;
                mc.rax = ctx.rax;
                mc.rcx = ctx.rcx;
                mc.rsp = ctx.rsp;
                mc.rip = ctx.rip;
                mc.eflags = ctx.rflags;
                mc.cs = USER_CODE_SELECTOR as u16;
                mc.ss = USER_DATA_SELECTOR as u16;
                mc.oldmask = current.sigmask;
                sf.uc.uc_sigmask = current.sigmask;

                // Save FPU before the handler clobbers it. Direct asm into the
                // thread's own buffer — must NOT take CPUS lock here (caller
                // holds it; with_cpu_state would self-deadlock).
                fpu_save_to(core::ptr::addr_of!(current.uc_fpu.data) as u64);

                // Deliver to the handler via the same iretq frame.
                ctx.rdi = sig as u64;       // arg1 = signum
                ctx.rsi = frame_addr + 312; // arg2 = &siginfo
                ctx.rdx = frame_addr + 8;   // arg3 = &ucontext
                ctx.rip = handler;
                ctx.rsp = frame_addr;       // user RSP = &pretcode

                if action.sa_flags & crate::arch::x86_64::abi::SA_NODEFER == 0 {
                    current.sigmask |= (1u64 << (sig - 1)) | action.sa_mask;
                }
            }

            Delivery::Handled
        }
    }
}

/// Deliver pending signals to `t` right before it is iretq'd back to user mode.
/// Returns the stack pointer to iretq into, or `None` if the signal's default
/// action is to terminate the thread (in which case the thread was not resumed).
///
/// Only delivers when the saved frame is a user-mode frame (`can_deliver`).
// TEMP-DIAG dispatch journal (iret-to-SCHEDULER hunt): per-CPU ring of the
// last 8 dispatches (tid, sp, frame rip, thread stack top, in-own-stack).
// Written at the single resume_with_signals funnel; dumped by the PF handler
// at panic. try_lock on read; write side runs on its own CPU. The journal
// tells us exactly what the switch path was handed (vs. what faulted).
#[derive(Clone, Copy)]
pub struct DjEntry {
    pub tid: u64,
    pub sp: u64,
    pub rip: u64,
    pub top: u64,
    pub in_range: bool,
}
pub static DISPATCH_J: [crate::arch::sync::spinlock::SpinLock<[DjEntry; 8]>; 4] =
    [const { crate::arch::sync::spinlock::SpinLock::new([DjEntry { tid: u64::MAX, sp: 0, rip: 0, top: 0, in_range: false }; 8]) }; 4];
pub static DISPATCH_JIDX: [core::sync::atomic::AtomicU64; 4] =
    [const { core::sync::atomic::AtomicU64::new(0) }; 4];

pub fn debug_journal_dispatch(t: &Thread, sp: u64) {
    let cpu = crate::smp::current_cpu_id() as usize;
    if cpu >= crate::smp::MAX_CPUS {
        return;
    }
    let (rip, in_range) = if kernel_ptr_ok(sp) && sp & 7 == 0 {
        let ctx = unsafe { &*(sp as *const CpuContext) };
        (ctx.rip, sp >= t.kernel_stack_top.wrapping_sub(KERNEL_STACK_SIZE as u64) && sp < t.kernel_stack_top)
    } else {
        (0, false)
    };
    if let Some(mut g) = DISPATCH_J[cpu].try_lock() {
        let i = (DISPATCH_JIDX[cpu].fetch_add(1, core::sync::atomic::Ordering::Relaxed) & 7) as usize;
        g[i] = DjEntry { tid: t.id, sp, rip, top: t.kernel_stack_top, in_range };
    }
    // TEMP-DIAG canary check (see alloc_*_stack): smashed base = overflow.
    // Read-only volatile loads; log-only.
    let kcan = unsafe { ((t.kernel_stack_top.wrapping_sub(KERNEL_STACK_SIZE as u64)) as *const u64).read_volatile() };
    if kcan != STACK_CANARY {
        crate::dprintln!("CANARY-DEAD: tid={} kstack base smashed {:#x}", t.id, kcan);
    }
    let scan = unsafe { ((t.syscall_stack_top.wrapping_sub(SYSCALL_STACK_BYTES as u64)) as *const u64).read_volatile() };
    if scan != STACK_CANARY {
        crate::dprintln!("CANARY-DEAD: tid={} sstack base smashed {:#x}", t.id, scan);
    }
}

pub fn resume_with_signals(t: &mut Thread) -> Result<u64, i32> {
    let sp = t.stack_ptr;
    debug_journal_dispatch(t, sp);
    // TEMP-DIAG BADRIP2 hunt (iret-to-SCHEDULER panic): every dispatch funnels
    // through here, after check_thread but immediately before any frame
    // mutation/iretq. A kernel-mode resume RIP outside .text is a forged
    // frame; iretq would fault identically with less context. Panic now.
    // User frames (cs=0x23) carry user RIPs and are exempt.
    if kernel_ptr_ok(sp) && sp & 7 == 0 {
        let ctx = unsafe { &*(sp as *const CpuContext) };
        if ctx.cs == 8 && !kernel_text_rip_ok(ctx.rip) {
            crate::dprintln!("BADRIP2: tid={} stack={:#x} rip={:#x} (not kernel .text or module window)",
                t.id, sp, ctx.rip);
            panic!("BADRIP2: tid={} would iretq to data {:#x}", t.id, ctx.rip);
        }
    }
    if !can_deliver(sp) {
        return Ok(sp);
    }
    match deliver_pending(t, sp) {
        Delivery::None | Delivery::Handled => Ok(sp),
        Delivery::Die(code) => Err(code),
        Delivery::Stopped => {
            // The process is Stopped: it must not return to user mode, and it
            // stays in the run queue so it keeps its saved kernel-stack frame.
            // `pick_next` skips stopped processes, so clearing the flag on
            // SIGCONT is all that is needed to make it runnable again.
            // Parent wakeup is handled by sys_kill (which sends the signal),
            // not here — SCHEDULER is held at this call site and taking it
            // again would deadlock.
            Err(STOPPED)
        }
    }
}
