use core::{arch::naked_asm, ptr::addr_of};

use alloc::{sync::Arc, vec::Vec};


use crate::smp::with_cpu_tss;
use shared::{
    boot::boot_info::*,
    memory::{
        address::*,
        flags::*,
    },
};

use crate::arch::sync::spinlock::SpinLock;
use crate::arch::x86_64::abi::*;
use crate::arch::x86_64::cpu::*;
use crate::arch::x86_64::msr::*;
use crate::arch::x86_64::tss::set_rsp0;
use crate::fs::*;
use crate::module;
use crate::fs::vfs::*;
use crate::interrupts::STDIN_BUFFER;
use crate::memory::FRAME_ALLOCATOR;
use crate::memory::space::*;
use crate::memory::vma::*;
use crate::tasks;
use crate::tasks::*;
use crate::tprint;

// Standard POSIX error codes

// --------------------------------------------------------------------
// Submodules, each holding a domain's syscall handlers.
// --------------------------------------------------------------------
pub mod fsio;
mod mem;
mod misc;
pub(crate) mod proc;
mod sig;
mod time;
pub mod epoll;
pub mod io_uring;
pub mod net;

pub(crate) use proc::sys_kill;

pub use fsio::{CONSOLE_KD_MODE, CONSOLE_KB_MODE};

// --------------------------------------------------------------------
// Handler import scope for the dispatcher below.
// --------------------------------------------------------------------
use self::epoll::{sys_epoll_create1, sys_epoll_ctl, sys_epoll_wait, sys_eventfd, sys_timerfd_create, sys_timerfd_settime, sys_inotify_init1, sys_inotify_add_watch, sys_inotify_rm_watch};
use self::io_uring::{sys_io_uring_enter, sys_io_uring_register, sys_io_uring_setup};
use self::fsio::{sys_access, sys_chdir, sys_chroot, sys_close, sys_close_range, sys_dup, sys_dup2, sys_faccessat, sys_fcntl, sys_fallocate, sys_flock, sys_fstat_fd, sys_fstatat, sys_fstatfs, sys_fsync, sys_ftruncate, sys_get_fb_info, sys_getcwd, sys_getdents64, sys_ioctl, sys_lseek, sys_memfd_create, sys_mkdir, sys_mkdirat, sys_mount, sys_fchmodat, sys_umask,
        sys_posix_openpt, sys_grantpt, sys_unlockpt, sys_ptsname_r, sys_open, sys_openat, sys_pipe, sys_poll, sys_ppoll, sys_pread64, sys_pwrite64, sys_read, sys_readlink, sys_readlinkat, sys_readv, sys_rmdir, sys_stat_path, sys_statfs, sys_statx, sys_sync, sys_truncate, sys_unlink, sys_unlinkat, sys_utimensat, sys_renameat, sys_renameat2, sys_umount2, sys_write, sys_writev, Statfs};
use self::proc::{sys_arch_prctl, sys_clone, sys_execve, sys_fork, sys_getegid, sys_geteuid, sys_getgid, sys_getgroups, sys_getpgid, sys_getpgrp, sys_getpid, sys_getppid, sys_getpriority, sys_getresgid, sys_getresuid, sys_getrlimit, sys_getrusage, sys_getsid, sys_gettid, sys_getuid, sys_prlimit64, sys_reboot, sys_rt_sigqueueinfo, sys_rt_tgsigqueueinfo, sys_set_tid_address, sys_setgid, sys_setgroups, sys_setpgid, sys_setpriority, sys_setregid, sys_setresgid, sys_setresuid, sys_setreuid, sys_setrlimit, sys_setsid, sys_setuid, sys_sigqueue, sys_tgkill, sys_times, sys_uname, sys_wait4};
use self::mem::{mmap_device, sys_brk, sys_madvise, sys_mmap, sys_mprotect, sys_munmap};
use self::sig::{sys_rt_sigaction, sys_rt_sigprocmask, sys_rt_sigreturn, sys_sigaltstack};
use self::time::{sys_clock_getres, sys_clock_gettime, sys_gettimeofday};
use self::misc::{sys_beep, sys_futex, sys_get_robust_list, sys_getrandom, sys_membarrier, sys_rseq, sys_set_robust_list, sys_sysinfo};

pub const EPERM: u64 = (-1i64) as u64;
pub const ENOENT: u64 = (-2i64) as u64;
pub const ESRCH: u64 = (-3i64) as u64;
pub const EINTR: u64 = (-4i64) as u64;
pub const EIO: u64 = (-5i64) as u64;
pub const ENOEXEC: u64 = (-8i64) as u64;
pub const EBADF: u64 = (-9i64) as u64;
pub const ECHILD: u64 = (-10i64) as u64;
pub const EAGAIN: u64 = (-11i64) as u64;
pub const ENOMEM: u64 = (-12i64) as u64;
pub const EACCES: u64 = (-13i64) as u64;
pub const EFAULT: u64 = (-14i64) as u64;
pub const EBUSY: u64 = (-16i64) as u64;
pub const EEXIST: u64 = (-17i64) as u64;
pub const ENOTBLK: u64 = (-15i64) as u64;
pub const ENODEV: u64 = (-19i64) as u64;
pub const ENOTDIR: u64 = (-20i64) as u64;
pub const EISDIR: u64 = (-21i64) as u64;
pub const EINVAL: u64 = -22i64 as u64;
pub const EROFS: u64 = (-30i64) as u64;
pub const ENOTTY: u64 = (-25i64) as u64;
pub const EPIPE: u64 = (-32i64) as u64;
pub const ERANGE: u64 = (-34i64) as u64;
pub const ENOSYS: u64 = (-38i64) as u64;
pub const ENOTEMPTY: u64 = (-39i64) as u64;
pub const ECONNREFUSED: u64 = (-111i64) as u64;
pub const ENOTSOCK: u64 = (-88i64) as u64;
pub const EOPNOTSUPP: u64 = (-95i64) as u64;
pub const EPROTONOSUPPORT: u64 = (-93i64) as u64;
pub const EADDRINUSE: u64 = (-98i64) as u64;
pub const ENOTCONN: u64 = (-107i64) as u64;
pub const EINPROGRESS: u64 = (-115i64) as u64;
pub const EALREADY: u64 = (-114i64) as u64;
pub const EDESTADDRREQ: u64 = (-89i64) as u64;
pub const EAFNOSUPPORT: u64 = (-97i64) as u64;
pub const ENETUNREACH: u64 = (-101i64) as u64;
pub const ETIMEDOUT: u64 = (-110i64) as u64;

pub const SYS_READ: u64 = 0;
pub const SYS_WRITE: u64 = 1;
pub const SYS_OPEN: u64 = 2;
pub const SYS_CLOSE: u64 = 3;

pub const AT_FDCWD: i32 = -100;

#[repr(align(16))]


#[repr(C)]
#[derive(Default)]
pub struct Stat {
    pub st_dev: u64,
    pub st_ino: u64,
    pub st_nlink: u64,
    pub st_mode: u32,
    pub st_uid: u32,
    pub st_gid: u32,
    pub __pad0: u32,
    pub st_rdev: u64,
    pub st_size: i64,
    pub st_blksize: i64,
    pub st_blocks: i64,
    pub st_atime: u64,
    pub st_atime_nsec: u64,
    pub st_mtime: u64,
    pub st_mtime_nsec: u64,
    pub st_ctime: u64,
    pub st_ctime_nsec: u64,
    pub __unused: [i64; 3],
}

#[repr(C)]
#[derive(Default)]
pub struct PollFd {
    pub fd: i32,
    pub events: i16,
    pub revents: i16,
}

/// ponytail: TEMP-DIAG syscall census (see dispatcher). Reported + reset in
/// the 8s stats dump; remove after the video-CPU investigation.
pub static SYSCALL_COUNTS: [core::sync::atomic::AtomicU64; 512] =
    [const { core::sync::atomic::AtomicU64::new(0) }; 512];

pub fn init_syscalls() {
    unsafe {
        // Enable System Call Extensions (SCE, bit 0) and No-Execute (NXE, bit 11) in EFER
        let efer = rdmsr(IA32_EFER);
        wrmsr(IA32_EFER, efer | 1 | (1 << 11));

        // Configure the STAR register (Segment Selectors)
        // Bits 32-47: Kernel Base (0x08 - KERNEL_CODE_SELECTOR)
        // Bits 48-63: User Base (0x10, so sysret loads SS=0x18|3=0x1B and CS=0x20|3=0x23)
        let star = (0x10 << 48) | (0x08 << 32);
        wrmsr(IA32_STAR, star);

        // Configure LSTAR (The kernel function to jump to)
        let lstar = syscall_entry as *const () as u64;
        wrmsr(IA32_LSTAR, lstar);

        // Configure SFMASK (Flags to clear upon syscall)
        // We mask out bit 9 (Interrupt Enable) so hardware interrupts
        // don't fire while we are in the middle of our syscall logic!
        wrmsr(IA32_FMASK, 0x200);

        // Boot-time default syscall stack. This is NOT shared per-core: it is
        // only valid until the first thread is dispatched, at which point every
        // CPU's CpuState.kernel_stack is overwritten with that thread's own
        // per-thread `syscall_stack_top` (see smp::timer_tick_on_core /
        // run_thread_on_core). Two CPUs never run syscalls on this stack.
        let stack_top = addr_of!(SYSCALL_STACK) as u64 + SYSCALL_STACK_SIZE as u64;
        crate::smp::with_cpu_state(|state| state.kernel_stack = stack_top);

        let gs_base = crate::smp::with_cpu_state(|state| state as *mut _ as u64);
        wrmsr(IA32_KERNEL_GS_BASE, gs_base);
    }
}

#[unsafe(naked)]
pub extern "C" fn syscall_entry() {
    naked_asm!(
        "swapgs",
        "mov gs:[8], rsp",
        "mov rsp, gs:[0]",
        "mov gs:[16], rcx",
        "mov gs:[24], r11",
        "push qword ptr gs:[8]", // User RSP
        "push 0",                // Alignment padding
        "push rcx",              // User RIP
        "push r11",              // User RFLAGS
        // Save caller-saved registers that Linux sysv64 ABI expects to be preserved
        "push rdi",
        "push rsi",
        "push rdx",
        "push r8",
        "push r9",
        "push r10",
        "push 0", // padding
        "push r15",
        "push r14",
        "push r13",
        "push r12",
        "push rbp",
        "push rbx",
        "push r9", // arg6 (on stack for syscall_rust_handler 7th argument)
        // --- THE SYSCALL SHUFFLE ---
        "mov r9, r8",   // arg5 (was in r8)
        "mov r8, r10",  // arg4 (syscall uses r10, we move it to r8)
        "mov rcx, rdx", // arg3 (was in rdx)
        "mov rdx, rsi", // arg2 (was in rsi)
        "mov rsi, rdi", // arg1 (was in rdi)
        "mov rdi, rax", // sys_no (was in rax, now it's arg 1!)
        "call syscall_rust_handler",
        "add rsp, 8",   // pop arg6
        // ===========================
        // normal_return:
        // Restore callee-saved registers
        "pop rbx",
        "pop rbp",
        "pop r12",
        "pop r13",
        "pop r14",
        "pop r15",
        "add rsp, 8",   // pop padding
        // Restore caller-saved registers
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdx",
        "pop rsi",
        "pop rdi",
        "pop r11",
        "pop rcx",
        "add rsp, 8",   // Remove Alignment padding
        "pop qword ptr gs:[8]", // Restore User RSP
        "mov rsp, gs:[8]",
        "swapgs",
        "sysretq",
    );
}

#[repr(C)]
pub struct CpuState {
    pub kernel_stack: u64, // Offset 0
    pub user_stack: u64,   // Offset 8
    pub user_rip: u64,     // Offset 16
    pub user_rflags: u64,  // Offste 24
    pub fpu_scratch: [u8; 512],
}

pub const SYSCALL_STACK_SIZE: usize = 16384;
pub static mut SYSCALL_STACK: [u8; SYSCALL_STACK_SIZE] = [0; SYSCALL_STACK_SIZE];

pub static mut CPU_STATE: CpuState = CpuState {
    kernel_stack: 0,
    user_stack: 0,
    user_rip: 0,
    user_rflags: 0,
    fpu_scratch: [0; 512],
};

// The shell's register state while a user program runs (saved/restored by
// cpu.rs execute_and_resume/return_to_shell). Offsets match the asm.
#[repr(C)]
pub struct ShellCtx {
    pub rbx: u64, // Offset 0x00
    pub rbp: u64, // Offset 0x08
    pub r12: u64, // Offset 0x10
    pub r13: u64, // Offset 0x18
    pub r14: u64, // Offset 0x20
    pub r15: u64, // Offset 0x28
    pub rsp: u64, // Offset 0x30
    pub rip: u64, // Offset 0x38
}

pub static mut SHELL_CTX: ShellCtx = ShellCtx {
    rbx: 0,
    rbp: 0,
    r12: 0,
    r13: 0,
    r14: 0,
    r15: 0,
    rsp: 0,
    rip: 0,
};

// -----------------------------------------------------------------------------
// Per-syscall handlers. Each handler takes only the arguments it needs and
// returns the raw syscall return value (positive count, 0, or negated errno).
// Handlers that never return to the dispatcher use `-> !`.
// -----------------------------------------------------------------------------

/// Decide whether the calling process may read its controlling terminal, and
/// stop it with `SIGTTIN` if not.
///
/// Linux: a process in a *background* process group that reads its controlling
/// terminal gets `SIGTTIN` (or `EIO` if it is blocked/ignored/orphaned). This
/// is what keeps a background job from stealing the terminal's input, and tmux
/// and zellij both rely on it to implement "stop me when I try to read".
///
/// Returns `true` if the read may proceed. `false` means a `SIGTTIN` was
/// queued for the process group and the caller must abandon this read — it
/// must *not* return an error, because the process is about to be stopped and
/// the syscall will not resume until `SIGCONT`.
///
/// A process in the foreground group, or one with no controlling terminal, is
/// always allowed: there is nothing to arbitrate against.
pub fn tty_read_permitted(vt: u16) -> bool {
    let t = match crate::tasks::current_thread() {
        Some(t) => t,
        None => return true,
    };
    let (ctty_vt, pgid) = {
        let p = t.process.lock();
        (p.ctty_vt, p.pgid as i32)
    };
    // No controlling terminal => no foreground group to be in the background of.
    if ctty_vt == 0 {
        return true;
    }
    let target = if vt == 0 { ctty_vt } else { vt };
    // `get_fg_pgid` falls back to `open_pgid` when no `tcsetpgrp` has run yet,
    // so a lone shell on a fresh VT is still considered foreground.
    let fg = crate::tty::get_fg_pgid(target);
    if fg == 0 || fg == pgid {
        return true;
    }
    // Orphans are allowed to read (Linux EIO case collapsed to "allow"):
    // a stopped-then-continued job whose group leader is gone would otherwise
    // deadlock against a terminal nobody owns.
    crate::tasks::stop_self_for_tty_io(crate::arch::x86_64::abi::SIGTTIN);
    false
}

/// Write-side counterpart of [`tty_read_permitted`]: `SIGTTOU` for a background
/// process group writing its controlling terminal.
///
/// The kernel-emitted case (`TOSTOP` set) is Linux's rule, but the tcsetpgrp
/// path raises `SIGTTOU` *unconditionally* of `TOSTOP`, so this is applied to
/// terminal writes generally — a background job that paints the terminal
/// steals the foreground's screen.
pub fn tty_write_permitted(vt: u16) -> bool {
    let t = match crate::tasks::current_thread() {
        Some(t) => t,
        None => return true,
    };
    let (ctty_vt, pgid) = {
        let p = t.process.lock();
        (p.ctty_vt, p.pgid as i32)
    };
    if ctty_vt == 0 {
        return true;
    }
    let target = if vt == 0 { ctty_vt } else { vt };
    let fg = crate::tty::get_fg_pgid(target);
    if fg == 0 || fg == pgid {
        return true;
    }
    crate::tasks::stop_self_for_tty_io(crate::arch::x86_64::abi::SIGTTOU);
    false
}

/// Read from the VT named by `vt` (0 = the active one) through the line
/// discipline.
///
/// This replaces the old single global queue. Each VT has its own input queue,
/// so a program holding /dev/tty3 drains VT 3's keystrokes and no one else's.
pub fn read_tty_vt(vt: u16, slice: &mut [u8]) -> Result<usize, u64> {
    let nonblock = match crate::tasks::current_thread() {
        Some(t) => t.process.lock().stdin_nonblock,
        None => false,
    };
    // Background read attempt: the process is marked stopped above. Park, so the
    // read behaves as if there were no input until SIGCONT releases it (see
    // `stop_self_for_tty_io` for why the stop is applied inline rather than
    // delivered as a queued signal).
    if !tty_read_permitted(vt) {
        crate::tasks::block_current_thread(crate::tasks::PendingBlock::Keyboard);
    }
    loop {
        match crate::tty::read(vt, slice) {
            // Data, or EOF. `read_tty` distinguishes the two; a 0 with no data
            // is only possible for EOF, because an empty queue returns `Empty`.
            crate::tty::ReadOutcome::Data(n) => return Ok(n),
            crate::tty::ReadOutcome::Eof => return Ok(0),
            crate::tty::ReadOutcome::Empty => {
                if nonblock {
                    return Err(EAGAIN);
                }
                crate::tasks::block_current_thread(crate::tasks::PendingBlock::Keyboard);
            }
        }
    }
}

/// Kept for callers that do not care which VT (fd 0 fallback paths).
pub fn read_stdin_internal(slice: &mut [u8]) -> Result<usize, u64> {
    read_tty_vt(0, slice)
}



fn with_current_process<T>(f: impl FnOnce(&mut Process) -> T) -> T {
    let t = crate::tasks::current_thread().expect("syscall with no current thread");
    let result = f(&mut t.process.lock());
    crate::tasks::set_current_thread(Some(t));
    result
}

pub fn sys_sched_yield() -> u64 {
    unsafe {
        core::arch::asm!("sti", "int 0x81");
    }
    0
}

/// Callee-saved registers passed through from `syscall_entry`, for handlers
/// that build a child kernel stack (clone/fork/execve).
#[derive(Clone, Copy)]
struct SyscallRegs {
    rbx: u64,
    rbp: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
}

#[unsafe(no_mangle)]
pub extern "C" fn syscall_rust_handler(
    sys_no: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
    arg5: u64,
    arg6: u64,
    rbx: u64,
    rbp: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
) -> u64 {
    let regs = SyscallRegs {
        rbx,
        rbp,
        r12,
        r13,
        r14,
        r15,
    };

    // ponytail: single current_thread() per entry (was two clones plus up to
    // three process locks): zombie check, last-syscall record (NMI hang
    // dumps), and the verbose pid all ride one clone + one write-back.
    // IF=0 here (no preemption) and CPU-local current is exclusively ours,
    // so the write-back cannot clobber anything.
    //
    // `pending_seen` is captured here so the exit path can decide whether to
    // run `apply_pending_signals_sync` without a second `current_thread()`
    // clone: a stop or fatal signal that was already queued when the syscall
    // was entered must take effect before we return to user mode (see the
    // function for why the tick path alone is insufficient).
    let mut pending_seen = false;
    if let Some(mut t) = crate::tasks::current_thread() {
        let (is_zombie, pid, pending) = {
            let p = t.process.lock();
            (
                p.status == ProcessStatus::Zombie,
                p.pid,
                (t.pending | p.pending) & !t.sigmask,
            )
        };
        pending_seen = pending != 0;
        if sys_no != 60 && sys_no != 231 && is_zombie {
            crate::tasks::exit_current_thread(0);
        }
        t.last_syscall_nr = sys_no;
        crate::tasks::set_current_thread(Some(t));
        // ponytail: per-syscall tracing gated — at desktop syscall rates the
        // debugcon port (a KVM VMEXIT per byte) otherwise saturates both CPUs
        // and starves input/rendering. Re-enable with --features dsp_verbose.
        if cfg!(feature = "dsp_verbose") {
            if pid == 2 {
                crate::dprintln!("[sc-entry] {} args: {:#x}, {:#x}, {:#x}, {:#x}", sys_no, arg1, arg2, arg3, arg4);
            }
        }
    }

    let result = match sys_no {
        82 => fsio::sys_rename(arg1, arg2),
        86 => fsio::sys_link(arg1, arg2),
        88 => fsio::sys_symlink(arg1, arg2),
        90 => fsio::sys_chmod(arg1, arg2 as u32),
        91 => fsio::sys_fchmod(arg1 as usize, arg2 as u32),
        92 => fsio::sys_chown(arg1, arg2 as u32, arg3 as u32),
        93 => fsio::sys_fchown(arg1 as usize, arg2 as u32, arg3 as u32),
        0 => {
            if let Some(res) = epoll::try_read(arg1 as usize, arg2 as *mut u8, arg3 as usize) {
                res
            } else {
                sys_read(arg1 as usize, arg2 as *mut u8, arg3 as usize)
            }
        }
        1 => {
            if let Some(res) = epoll::try_write(arg1 as usize, arg2 as *const u8, arg3 as usize) {
                res
            } else {
                sys_write(arg1 as usize, arg2 as *const u8, arg3 as usize)
            }
        }
        2 => sys_open(arg1, arg2, arg3),
        3 => {
            epoll::on_close(arg1 as usize);
            sys_close(arg1 as usize)
        }
        4 => sys_stat_path(arg1, arg2 as *mut Stat),
        5 => sys_fstat_fd(arg1 as usize, arg2 as *mut Stat),
        6 => sys_stat_path(arg1, arg2 as *mut Stat),
        7 => sys_poll(arg1 as *mut PollFd, arg2 as usize, arg3 as i32),
        23 => fsio::sys_select(arg1 as usize, arg2, arg3, arg4, arg5),
        270 => fsio::sys_pselect6(arg1 as usize, arg2, arg3, arg4, arg5, arg6),
        271 => sys_ppoll(arg1 as *mut PollFd, arg2 as usize, arg3, arg4),
        8 => sys_lseek(arg1 as usize, arg2 as isize, arg3 as usize),
         9 => {
            let fd = arg5 as u32;
            let off = arg6 as u64;
            // MAP_ANONYMOUS should only create anonymous mappings when no real fd is provided.
            // POSIX: providing a valid fd means file-backed mapping regardless of MAP_ANONYMOUS.
            // glibc passes fd=-1 for anonymous reservations and fd=<real> for file-backed.
            let is_anonymous_map = (arg4 & MAP_ANONYMOUS) != 0 && (fd == (AT_FDCWD as u32) || (fd as i32) == -1);
            if is_anonymous_map || (arg4 & MAP_DEVICE) != 0 {
                sys_mmap(arg1, arg2 as usize, arg3, arg4)
            } else {
                // Check if fd is a DrmDevice or FbDevice, mapping device memory directly
                let is_dev_node = {
                    let t = crate::tasks::current_thread();
                    if let Some(t) = t {
                        let p = t.process.lock();
                        p.fd_table.get(fd as usize).cloned().flatten().map(|of| {
                            let node = of.lock();
                            matches!(node.vnode, crate::fs::vfs::VNode::DrmDevice { .. } | crate::fs::vfs::VNode::FbDevice { .. })
                        }).unwrap_or(false)
                    } else {
                        false
                    }
                };

                // An io_uring ring fd is mmap'd at three distinct file
                // offsets (SQ ring, CQ ring, SQE array) to publish the shared
                // ring buffers. It is not a regular file, so it must be
                // recognised before the file-backed path rejects it.
                if io_uring::is_ring_fd(fd as usize) {
                    return io_uring::io_uring_mmap(
                        fd as usize,
                        arg1,
                        arg2 as usize,
                        arg3,
                        arg4,
                        off,
                    );
                }

                if is_dev_node {
                    // Two namespaces share this offset, so resolve them in
                    // order of precision:
                    //
                    //  1. Venus host blobs are addressed by hostmem *window*
                    //     offset. `hostmem_translate` matches only offsets
                    //     inside a known host-blob window, so it is exact and
                    //     must be tried first: a window offset can numerically
                    //     land inside a dumb buffer's physical range, and
                    //     answering with the guest buffer there hands libvulkan
                    //     the wrong memory (it aborts on the first use).
                    //  2. Otherwise MAP_DUMB reported a dumb buffer's physical
                    //     base, so require the range to fall inside a live
                    //     buffer and clamp the length to its end. Passing the
                    //     raw offset to mmap_device would make any DRM fd an
                    //     arbitrary-physical-memory primitive.
                    //
                    // Anything else is EINVAL.
                    let requested = arg2 as usize;
                    let (phys, len) = match crate::drivers::drm::virtgpu::hostmem_translate(off, requested) {
                        Some((p, l)) => (p.0, l),
                        None => {
                            let drm = crate::drivers::drm::GLOBAL_DRM.lock();
                            match drm.as_ref().and_then(|d| d.resolve_dumb_mmap(off, requested)) {
                                Some((p, l)) => (p.0, l),
                                None => return EINVAL,
                            }
                        }
                    };
                    let res = mmap_device(phys, len, arg3, arg4 | MAP_DEVICE);
                    if cfg!(feature = "dsp_verbose") { crate::dprintln!("[mmap dev] fd={} off={:#x} len={:#x} -> ret={:#x}", fd, off, arg2, res); }
                    res
                } else {
                    // Check if fd is a PRIME dma-buf (synthetic fd from PRIME_HANDLE_TO_FD)
                    // These fds are real OpenFiles backed by VNode::DmaBuf, so resolve the
                    // fd's vnode directly — this works for any fd number in any process,
                    // not just the exporter's number.
                    let dma_buf_phys = {
                        let t = crate::tasks::current_thread();
                        if let Some(t) = t {
                            let p = t.process.lock();
                            let of_opt = p.fd_table.get(fd as usize).cloned().flatten();
                            drop(p);
                            match of_opt {
                                Some(of) => {
                                    let ofg = of.lock();
                                    let handle = match &ofg.vnode {
                                        crate::fs::vfs::VNode::DmaBuf(db) => Some(db.lock().handle),
                                        _ => None,
                                    };
                                    drop(ofg); // release fd-table lock before touching GLOBAL_DRM
                                    let drm = crate::drivers::drm::GLOBAL_DRM.lock();
                                    drm.as_ref().and_then(|dev| {
                                        handle.and_then(|h| dev.find_dumb(h).map(|b| (b.phys.0, b.size)))
                                    })
                                }
                                None => None,
                            }
                        } else { None }
                    };
                    if let Some((phys, size)) = dma_buf_phys {
                        let len = if arg2 == 0 { size as usize } else { arg2 as usize };
                        let res = mmap_device(phys, len, arg3, arg4 | MAP_DEVICE);
                        if cfg!(feature = "dsp_verbose") { crate::dprintln!("[mmap dma-buf] fd={} phys={:#x} len={:#x} -> ret={:#x}", fd, phys, len, res); }
                        res
                    } else {
                        // Keep the existing `prime_phys` table lookup as fallback, then `mmap_file`.
                        let prime_phys = {
                            let drm = crate::drivers::drm::GLOBAL_DRM.lock();
                            if let Some(dev) = drm.as_ref() {
                                if let Some(&handle) = dev.prime_handles.get(&fd) {
                                    dev.find_dumb(handle).map(|db| (db.phys.0, db.size))
                                } else { None }
                            } else { None }
                        };
                        if let Some((phys, size)) = prime_phys {
                            let len = if arg2 == 0 { size as usize } else { arg2 as usize };
                            let res = mmap_device(phys, len, arg3, arg4 | MAP_DEVICE);
                            if cfg!(feature = "dsp_verbose") { crate::dprintln!("[mmap prime] fd={} handle->phys={:#x} len={:#x} -> ret={:#x}", fd, phys, len, res); }
                            res
                        } else {
                            crate::memory::filemmap::mmap_file(arg1, arg2 as usize, arg3, arg4, fd, off)
                        }
                    }
                }
            }
        }
        10 => sys_mprotect(arg1, arg2 as usize, arg3),
        149 => misc::sys_mlock(arg1, arg2 as usize),
        150 => misc::sys_mlockall(arg2 as i32),
        151 => misc::sys_munlock(arg1, arg2 as usize),
        325 => misc::sys_mlock2(arg1, arg2 as usize, arg3 as i32),
        11 => sys_munmap(arg1, arg2 as usize),
        12 => sys_brk(arg1),
        13 => sys_rt_sigaction(
            arg1 as i32,
            arg2 as *const SigAction,
            arg3 as *mut SigAction,
        ),
        14 => sys_rt_sigprocmask(
            arg1 as i32,
            arg2 as *const u64,
            arg3 as *mut u64,
            arg4 as usize,
        ),
        15 => sys_rt_sigreturn(),
        16 => sys_ioctl(arg1 as usize, arg2, arg3),
        17 => sys_pread64(arg1 as usize, arg2, arg3 as usize, arg4 as u64),
        18 => sys_pwrite64(arg1 as usize, arg2, arg3 as usize, arg4 as u64),
        19 => sys_readv(arg1 as usize, arg2 as *const [u64; 2], arg3),
        20 => sys_writev(arg1 as usize, arg2 as *const [u64; 2], arg3),
        21 => sys_access(arg1, arg2 as u32),
        22 => sys_pipe(arg1 as *mut i32, false),
        24 => sys_sched_yield(),
        28 => sys_madvise(arg1, arg2 as usize, arg3 as i32),
        32 => sys_dup(arg1 as usize),
        53 => net::sys_socketpair(arg1, arg2, arg3, arg4),
        54 => net::sys_setsockopt(arg1, arg2, arg3, arg4, arg5),
        55 => net::sys_getsockopt(arg1, arg2, arg3, arg4, arg5),
        41 => net::sys_socket(arg1, arg2, arg3),
        42 => net::sys_connect(arg1, arg2, arg3),
        43 => net::sys_accept4(arg1, arg2, arg3, 0),
        44 => net::sys_sendto(arg1, arg2, arg3, arg4, arg5, arg6),
        45 => net::sys_recvfrom(arg1, arg2, arg3, arg4, arg5, arg6),
        46 => net::sys_sendmsg(arg1, arg2, arg3 as u64),
        47 => net::sys_recvmsg(arg1, arg2, arg3 as u64),
        48 => net::sys_shutdown(arg1, arg2),
        49 => net::sys_bind(arg1, arg2, arg3),
        50 => net::sys_listen(arg1, arg3 as i32),
        51 => net::sys_getsockname(arg1, arg2, arg3),
        52 => net::sys_getpeername(arg1, arg2, arg3),
        84 => sys_rmdir(arg1),
        87 => sys_unlink(arg1),
        288 => net::sys_accept4(arg1, arg2, arg3, arg4),
        33 => sys_dup2(arg1 as usize, arg2 as usize),
        292 => fsio::sys_dup3(arg1 as usize, arg2 as usize, arg3 as i32),
        34 => sys_pause(),
        35 => sys_nanosleep(arg1, arg2),
        38 => sys_setitimer(arg1 as u32, arg2, arg3),
        39 => sys_getpid(),
        56 => sys_clone(56, arg1, arg2, arg3, arg4, arg5, arg6, regs),
        435 => sys_clone(435, arg1, arg2, arg3, arg4, arg5, arg6, regs),
        57 | 58 => sys_fork(regs),
        59 => sys_execve(arg1, arg2, arg3),
        60 => sys_exit(arg1 as i32),
        231 => sys_exit_group(arg1 as i32),
        61 => sys_wait4(arg1 as i64, arg2 as *mut i32, arg3 as u32),
        62 => sys_kill(arg1 as i64, arg2 as i32),
        63 => sys_uname(arg1 as *mut u8),
        129 => sys_rt_sigqueueinfo(arg1 as i64, arg2 as i32, arg3),
        240 => sys_sigqueue(arg1 as i64, arg2 as i32, arg3 as i64),
        72 => sys_fcntl(arg1 as usize, arg2, arg3),
        73 => sys_flock(arg1 as usize, arg2 as i32),
        74 => sys_fsync(arg1 as usize),
        75 => sys_fsync(arg1 as usize),
        76 => sys_truncate(arg1, arg2),
        77 => sys_ftruncate(arg1 as usize, arg2),
        162 => sys_sync(),
        165 => sys_mount(arg1, arg2, arg3, arg4, arg5),
        166 => sys_umount2(arg1, arg2),
        79 => sys_getcwd(arg1 as *mut u8, arg2 as usize),
        80 => sys_chdir(arg1),
        161 => sys_chroot(arg1),
        83 => sys_mkdir(arg1, arg2 as u32),
        89 => sys_readlink(arg1, arg2 as *mut u8, arg3 as usize),
        96 => sys_gettimeofday(arg1),
        97 => sys_getrlimit(arg1 as usize, arg2 as *mut crate::tasks::RLimit),
        98 => sys_getrusage(arg1 as i32, arg2),
        // 95 = umask. Read returns the current mask, set returns the PREVIOUS
        // one — a program depends on the old value coming back, which is why
        // this cannot be a stub returning 0. It was absent entirely, so the
        // call fell through to ENOSYS and the caller read -38 as the old mask,
        // which nix rejects ("umask returned invalid Mode").
        95 => sys_umask(arg1 as u32),
        99 => sys_sysinfo(arg1 as *mut u8),
        100 => sys_times(arg1),
        102 => sys_getuid(),
        104 => sys_getgid(),
        105 => sys_setuid(arg1 as u32),
        106 => sys_setgid(arg1 as u32),
        107 => sys_geteuid(),
        108 => sys_getegid(),
        109 => sys_setpgid(arg1 as i64, arg2 as i64),
        // 172/173 = getpgid/getpgrp. getpgid existed but returned the pid
        // rather than the group id; getpgrp was not dispatched at all, so a
        // shell asking "who owns the terminal" got ENOSYS.
        172 => misc::sys_seccomp(arg1 as i32, arg2 as i32, arg3),
        175 => module::sys_init_module(arg1, arg2 as usize, arg3),
        313 => module::sys_finit_module(arg1 as i32, arg2, arg3 as i32),
        173 => sys_getpgrp(),
        110 => sys_getppid(),
        111 => with_current_process(|p| p.pid),
        112 => sys_setsid(),
        113 => sys_setreuid(arg1 as u32, arg2 as u32),
        114 => sys_setregid(arg1 as u32, arg2 as u32),
        115 => sys_getgroups(arg1 as i32, arg2 as *mut u32),
        116 => sys_setgroups(arg1 as usize, arg2 as *const u32),
        121 => sys_getpgid(arg1 as i64),
        124 => sys_getsid(arg1 as i64),
        117 => sys_setresuid(arg1 as u32, arg2 as u32, arg3 as u32),
        118 => sys_getresuid(arg1 as *mut u32, arg2 as *mut u32, arg3 as *mut u32),
        119 => sys_setresgid(arg1 as u32, arg2 as u32, arg3 as u32),
        120 => sys_getresgid(arg1 as *mut u32, arg2 as *mut u32, arg3 as *mut u32),
        131 => sys_sigaltstack(arg1, arg2),
        137 => sys_statfs(arg1, arg2 as *mut Statfs),
        138 => sys_fstatfs(arg1 as usize, arg2 as *mut Statfs),
        140 => sys_getpriority(arg1 as i32, arg2 as i32),
        141 => sys_setpriority(arg1 as i32, arg2 as i32, arg3 as i32),
        158 => sys_arch_prctl(arg1, arg2),
        160 => sys_setrlimit(arg1 as usize, arg2 as *const crate::tasks::RLimit),
        169 => sys_reboot(arg1 as usize, arg2 as usize, arg3 as usize, arg4 as usize),
        186 => sys_gettid(),
        202 => sys_futex(
            arg1 as *mut u32,
            arg2 as i32,
            arg3 as u32,
            arg4,
            arg5 as *mut u32,
            arg6 as u32,
        ),
        217 => sys_getdents64(arg1 as usize, arg2 as *mut u8, arg3 as usize),
        101 => misc::sys_ptrace(arg1 as i32, arg2 as i64, arg3, arg4),
        200 => sys_tgkill(arg1 as i64, arg2 as i64, arg3 as i32),
         218 => sys_set_tid_address(arg1),
        228 => sys_clock_gettime(arg1 as i32, arg2),
        229 => sys_clock_getres(arg1 as i32, arg2),
        230 => sys_clock_nanosleep(arg1 as i32, arg2 as i32, arg3, arg4),
        234 => sys_rt_tgsigqueueinfo(arg1 as i64, arg2 as i64, arg3 as i32, arg4),
        256 => sys_fstatat(arg1 as i32, arg2, arg3 as *mut Stat, arg4 as u32),
         258 => sys_mkdirat(arg1 as i32, arg2, arg3 as u32),
        // 268 = fchmodat. glibc's chmod() and Rust's fs::set_permissions() both
        // reach this on some builds, and it was not dispatched at all -- the
        // number fell through to the ENOSYS default arm.
        268 => sys_fchmodat(arg1 as i32, arg2, arg3 as u32),
        // 260 = fchownat: what glibc's chown(2) calls. Its absence meant that
        // giving a file a group of its own was impossible from userspace.
        260 => fsio::sys_fchownat(arg1 as i32, arg2, arg3 as u32, arg4 as u32, arg5 as u32),
        // The POSIX pty API. glibc's openpty(3) routes to these syscall numbers
        // directly rather than to open("/dev/ptmx"), so without them a program
        // using openpty fails with ENOSYS while one opening /dev/ptmx works —
        // which is why PTYs looked healthy and zellij still got no panes.
        622 => sys_posix_openpt(arg1 as u32),
        624 => sys_grantpt(arg1 as u32),
        625 => sys_unlockpt(arg1 as u32),
        626 => sys_ptsname_r(arg1 as u32, arg2, arg3 as usize),
         257 => {
            let r = sys_openat(arg1 as i32, arg2, arg3, arg4);
            #[cfg(feature = "trace_syscalls")]
            crate::println!("[sc-openat-ret] -> {:#x} ({})", r as i64, r as i64);
            r
        }
        262 => sys_fstatat(arg1 as i32, arg2, arg3 as *mut Stat, arg4 as u32),
        263 => sys_unlinkat(arg1 as i32, arg2, arg3 as u32),
        267 => sys_readlinkat(arg1 as i32, arg2, arg3 as *mut u8, arg4 as usize),
        269 => sys_faccessat(arg1 as i32, arg2, arg3 as u32, arg4 as u32),
        280 => fsio::sys_utimensat(arg1 as i32, arg2, arg3, arg4 as u32),
        272 => misc::sys_unshare(arg1 as i32),
        273 => sys_set_robust_list(arg1, arg2 as usize),
        274 => sys_get_robust_list(arg1 as i32, arg2, arg3),
        157 => misc::sys_prctl(arg1 as i32, arg2, arg3, arg4, arg5),
        203 => misc::sys_sched_setaffinity(arg1 as i32, arg2 as usize, arg3),
        204 => misc::sys_sched_getaffinity(arg1 as i32, arg2 as usize, arg3),
        213 => sys_epoll_create1(0),
        232 => sys_epoll_wait(
            arg1 as i32,
            arg2 as *mut epoll::EpollEvent,
            arg3 as i32,
            arg4 as i32,
        ),
        233 => sys_epoll_ctl(
            arg1 as i32,
            arg2 as i32,
            arg3 as i32,
            arg4 as *mut epoll::EpollEvent,
        ),
        281 => sys_epoll_wait(
            arg1 as i32,
            arg2 as *mut epoll::EpollEvent,
            arg3 as i32,
            arg4 as i32,
        ), // epoll_pwait without sigmask handling for now
        283 => sys_timerfd_create(arg1 as i32, arg2 as i32),
        284 => sys_eventfd(arg1 as u32, arg2 as i32),
        287 => epoll::sys_signalfd(arg1 as i32, arg2, arg3 as i32),
        277 => misc::sys_sync_file_range(arg1 as i32, arg2, arg3, arg4 as u32),
        285 => sys_fallocate(arg1 as usize, arg2 as i32, arg3, arg4),
        286 => {
            if epoll::is_timer_fd(arg1 as i32) {
                sys_timerfd_settime(
                    arg1 as i32,
                    arg2 as i32,
                    arg3 as *const epoll::Itimerspec,
                    arg4 as *mut epoll::Itimerspec,
                )
            } else {
                EINVAL
            }
        }
        290 => sys_eventfd(arg1 as u32, arg2 as i32),
        291 => sys_epoll_create1(arg1 as i32),
        293 => sys_pipe(arg1 as *mut i32, (arg2 & 0x800) != 0),
        300 => sys_faccessat(arg1 as i32, arg2, arg3 as u32, arg4 as u32),
        316 => fsio::sys_renameat(arg1 as i32, arg2, arg3 as i32, arg4),
        302 => sys_prlimit64(
            arg1 as i64,
            arg2 as usize,
            arg3 as *const crate::tasks::RLimit,
            arg4 as *mut crate::tasks::RLimit,
        ),
        318 => sys_getrandom(arg1, arg2 as usize, arg3 as u32),
        319 => sys_memfd_create(arg1, arg2 as u32),
        324 => sys_membarrier(arg1 as i32, arg2 as u32),
        253 => epoll::sys_inotify_init1(0),
        294 => epoll::sys_inotify_init1(arg1 as i32),
        299 => misc::sys_setns(arg1 as i32, arg2 as i32),
        254 => epoll::sys_inotify_add_watch(arg1 as i32, arg2, arg3 as u32),
        255 => epoll::sys_inotify_rm_watch(arg1 as i32, arg2 as i32),
        330 => ENOSYS, // sys_pkey_alloc
        452 => ENOSYS, // sys_landlock_create_ruleset
        453 => ENOSYS, // sys_landlock_add_rule
        454 => ENOSYS, // sys_landlock_restrict_self, // sys_pkey_alloc: MPK not supported by this CPU/QEMU, ENOSYS is correct Linux behavior
        332 => sys_statx(arg1 as i32, arg2, arg3 as u32, arg4 as u32, arg5 as *mut fsio::Statx),
        333 => sys_get_fb_info(arg1),
        334 => sys_rseq(arg1, arg2 as u32, arg3 as i32, arg4 as u32),
        425 => sys_io_uring_setup(arg1 as u32, arg2),
        426 => sys_io_uring_enter(
            arg1 as u64,
            arg2 as u32,
            arg3 as u32,
            arg4 as u32,
            arg5,
        ),
        427 => sys_io_uring_register(
            arg1 as u64,
            arg2 as u32,
            arg3,
            arg4 as u32,
        ),
        436 => sys_close_range(arg1 as u32, arg2 as u32, arg3 as u32),
        500 => sys_beep(arg1, arg2),
        _ => {
            ENOSYS
        }
    };

    if result == 0x100000000 {
    }
    // ponytail: TEMP-DIAG syscall census for the video-CPU investigation:
    // per-number counters, reported in the 8s stats dump. Remove after.
    SYSCALL_COUNTS[(sys_no as usize) & 511]
        .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    // Apply a pending SIG_DFL stop or fatal signal synchronously before
    // returning to user mode. Signals are otherwise delivered only on
    // interrupt frames, which a tight syscall loop can starve indefinitely;
    // see `apply_pending_signals_sync`. `result` is preserved across a stop.
    if pending_seen {
        crate::tasks::apply_pending_signals_sync();
    }
    // ponytail: sc-exit is debug spam (slows --gl by 3x); gate behind trace feature
    #[cfg(feature = "trace_syscalls")]
    crate::println!("[sc-exit] {} -> {:#x} ({})", sys_no, result as i64, result as i64);
    result
}

/// 38: `setitimer(which, new_value, old_value)`.
/// ponytail: stub accepts and reports disarmed — no SIGALRM is ever
/// delivered, so interval timers (sshd login-grace) never fire. Wire to the
/// timer tick if unenforced timeouts become a problem (leak, not corruption).
/// 34: `pause()` — block until a signal is delivered, then return `EINTR`.
///
/// x86-64 assigns `__NR_pause` the number 34; there is no `nice(2)` syscall on
/// this ABI (libc's `nice()` goes through `setpriority`). Handling 34 as
/// `nice` made every `pause()` return immediately, which turned an idling child
/// into a tight syscall loop — precisely the shape whose stop signals the tick
/// path cannot reliably deliver. See `apply_pending_signals_sync`.
fn sys_pause() -> u64 {
    loop {
        // A signal already pending => return EINTR (and let the dispatcher's
        // exit path apply any stop/fatal disposition).
        let ready = match crate::tasks::current_thread() {
            Some(t) => {
                let p = t.process.lock();
                (t.pending | p.pending) & !t.sigmask
            }
            None => return (-4i64) as u64, // EINTR
        };
        if ready != 0 {
            return (-4i64) as u64; // EINTR
        }
        // Nothing pending: sleep until a signal arrives. `sys_kill` wakes every
        // blocked thread of the target pid, so any signal releases us; the
        // loop re-checks, so a spurious wake just parks again.
        crate::tasks::block_current_thread(crate::tasks::PendingBlock::PipeWait);
    }
}

fn sys_setitimer(which: u32, new_ptr: u64, old_ptr: u64) -> u64 {
    if which > 2 {
        return EINVAL;
    }
    if new_ptr != 0 && !validate_user_read_range(new_ptr, 16) {
        return EFAULT;
    }
    if old_ptr != 0 {
        if !validate_user_write_range(old_ptr, 16) {
            return EFAULT;
        }
        unsafe {
            core::ptr::write_bytes(old_ptr as *mut u8, 0, 16);
        }
    }
    0
}

fn sys_nanosleep(req_ptr: u64, _rem_ptr: u64) -> u64 {
    if !validate_user_read_range(req_ptr, 16) {
        return EFAULT;
    }
    let req = unsafe { *(req_ptr as *const [u64; 2]) };
    let sec = req[0];
    let nsec = req[1];
    let ms = sec.saturating_mul(1000).saturating_add(nsec / 1_000_000);

    if ms > 0 {
        // Check for pending signal before sleeping (POSIX interruptible sleep).
        if let Some(t) = crate::tasks::current_thread() {
            let p = t.process.lock();
            let ready = (t.pending | p.pending) & !t.sigmask;
            if ready != 0 {
                return (-4i64) as u64; // EINTR
            }
        }
        let now = crate::time::TICKS.load(core::sync::atomic::Ordering::Relaxed) as u64;
        let deadline = now.saturating_add(ms);
        // Park the thread until the tick sweep expires the deadline.
        // Never busy-spin: that starves the timer IRQ that would end the sleep.
        crate::tasks::block_current_thread(crate::tasks::PendingBlock::Sleep { deadline });
        // On resume: check if we were interrupted by a signal.
        if let Some(t) = crate::tasks::current_thread() {
            let p = t.process.lock();
            let ready = (t.pending | p.pending) & !t.sigmask;
            if ready != 0 {
                return (-4i64) as u64; // EINTR
            }
        }
    }
    0
}

fn sys_clock_nanosleep(_clockid: i32, _flags: i32, req_ptr: u64, rem_ptr: u64) -> u64 {
    sys_nanosleep(req_ptr, rem_ptr)
}

fn sys_exit(code: i32) -> ! {
    crate::dprintln!("User program exited with code {}.", code);
    crate::dprintln!("[exit-tmp] pid={} code={}", crate::tasks::current_thread().map(|t| t.process.lock().pid).unwrap_or(0), code); // TEMP
    crate::tasks::exit_current_thread(code)
}

fn sys_exit_group(code: i32) -> ! {
    crate::dprintln!("User program exited with code {}.", code);
    crate::dprintln!("[exit_group] pid={} code={}", crate::tasks::current_thread().map(|t| t.process.lock().pid).unwrap_or(0), code);
    crate::tasks::exit_group(code)
}

fn handle_sigpipe() -> u64 {
    let mut t = crate::tasks::current_thread().expect("handle_sigpipe: no current thread");
    // ponytail: read the handler under the lock, then DROP the guard before
    // any exit path — exit_group re-locks process (tasks.rs) and a held
    // guard here panics as SAME-CPU REENTRY (sshd died on first closed-fd
    // write because of this).
    let handler = {
        let p = t.process.lock();
        p.sigactions[12].sa_handler
    };
    match handler {
        0 => {
            // SIG_DFL: terminate with signal death
            exit_group(128 + 13);
        }
        1 => {} // SIG_IGN: just EPIPE
        _ => {
            t.pending |= 1 << 12;
            t.sigqueue.push_back(crate::tasks::SigInfo { signo: 13, code: crate::tasks::SI_USER, pid: 0, value: 0 });
        }
    }
    crate::tasks::set_current_thread(Some(t));
    EPIPE
}
        
