use super::*;

/// `getrandom(buf, buflen, flags)`. `GRND_NONBLOCK`/`GRND_RANDOM` are accepted
/// but ignored (we never block). Any other flag bits → `EINVAL`.
pub(super) fn sys_getrandom(buf_ptr: u64, buflen: usize, flags: u32) -> u64 {
    const GRND_NONBLOCK: u32 = 0x1;
    const GRND_RANDOM: u32 = 0x2;
    if flags & !(GRND_NONBLOCK | GRND_RANDOM) != 0 {
        return EINVAL;
    }
    if buflen == 0 {
        return 0;
    }
    if !validate_user_write_range(buf_ptr, buflen) {
        return EFAULT;
    }
    let buf = unsafe { core::slice::from_raw_parts_mut(buf_ptr as *mut u8, buflen) };
    crate::arch::x86_64::random::fill_random(buf);
    buflen as u64
}

/// `access(path, mode)`: existence check only — the initrd/ext2 have no
/// permission bits, so an existing path is always accessible.
pub(super) fn sys_beep(freq: u64, duration_ms: u64) -> u64 {
    if freq == 0 {
        crate::drivers::audio::pcspeaker::stop_tone();
    } else if duration_ms == 0 {
        crate::drivers::audio::pcspeaker::play_tone(freq as u32);
    } else {
        crate::drivers::audio::pcspeaker::beep(freq as u32, duration_ms as u32);
    }
    0
}

/// `sys_exit` is `-> !` (diverges). Match arms in the dispatcher can still
/// route to it because `!` coerces to `u64`, matching the other arms.
pub(super) fn sys_sysinfo(info_ptr: *mut u8) -> u64 {
    if !validate_user_write_range(info_ptr as u64, 112) {
        return EFAULT;
    }
    let uptime = (crate::time::TICKS.load(core::sync::atomic::Ordering::Relaxed) / 1000) as i64;
    let total_pages = crate::memory::space::COW_COUNTS
        .lock()
        .as_ref()
        .map(|c| c.len())
        .unwrap_or(32768) as u64;
    let page_size = 4096u64;
    let procs_count = PROCESS_TABLE.lock().len() as u16;

    unsafe {
        core::ptr::write_bytes(info_ptr, 0, 112);
        let dst = info_ptr as *mut i64;
        *dst.add(0) = uptime; // uptime
        *dst.add(1) = 0;      // loads[0]
        *dst.add(2) = 0;      // loads[1]
        *dst.add(3) = 0;      // loads[2]
        *dst.add(4) = (total_pages * page_size) as i64; // totalram
        *dst.add(5) = ((total_pages / 2) * page_size) as i64; // freeram
        *dst.add(6) = 0;      // sharedram
        *dst.add(7) = 0;      // bufferram
        *dst.add(8) = 0;      // totalswap
        *dst.add(9) = 0;      // freeswap
        let procs_ptr = info_ptr.add(80) as *mut u16;
        *procs_ptr = procs_count;
        let mem_unit_ptr = info_ptr.add(96) as *mut u32;
        *mem_unit_ptr = 1;
    }
    0
}

pub(super) fn sys_set_robust_list(_head_ptr: u64, _len: usize) -> u64 {
    0
}

pub(super) fn sys_get_robust_list(_pid: i32, _head_ptr_ptr: u64, _len_ptr: u64) -> u64 {
    0
}

pub(super) fn sys_rseq(_rseq_ptr: u64, _rseq_len: u32, _flags: i32, _sig: u32) -> u64 {
    ENOSYS
}

/// `membarrier(cmd, fd)`: memory barrier for multithreaded programs.
///
/// On x86-64 with TSO (Total Store Order), hardware memory ordering is
/// strong, so most membarrier commands are no-ops at the hardware level.
/// We still implement the query and barrier commands so that multithreaded
/// programs (e.g. those using glibc's atomic operations) don't fail.
///
/// Supported commands:
///   MEMBARRIER_CMD_QUERY (0) — returns bitmask of supported commands
///   MEMBARRIER_CMD_GLOBAL (1) — global memory barrier (no-op on x86 TSO)
///   MEMBARRIER_CMD_GLOBAL_EXPEDITED (2) — expedited global barrier (no-op)
///   MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED (3) — register for expedited (no-op)
///   MEMBARRIER_CMD_PRIVATE_EXPEDITED (4) — private expedited barrier (no-op)
///   MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED (5) — register for private (no-op)
///   MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ (6) — private with rseq (no-op)
pub(super) fn sys_membarrier(cmd: i32, _fd: u32) -> u64 {
    const MEMBARRIER_CMD_QUERY: i32 = 0;
    const MEMBARRIER_CMD_GLOBAL: i32 = 1;
    const MEMBARRIER_CMD_GLOBAL_EXPEDITED: i32 = 2;
    const MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED: i32 = 3;
    const MEMBARRIER_CMD_PRIVATE_EXPEDITED: i32 = 4;
    const MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED: i32 = 5;
    const MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ: i32 = 6;

    // Bitmask of commands we support: QUERY (bit 0), plus all barrier commands
    // (bits 1-6). x86 TSO makes these all correct no-ops.
    const SUPPORTED_CMDS: u32 = (1 << MEMBARRIER_CMD_QUERY)
        | (1 << MEMBARRIER_CMD_GLOBAL)
        | (1 << MEMBARRIER_CMD_GLOBAL_EXPEDITED)
        | (1 << MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED)
        | (1 << MEMBARRIER_CMD_PRIVATE_EXPEDITED)
        | (1 << MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED)
        | (1 << MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ);

    match cmd {
        MEMBARRIER_CMD_QUERY => SUPPORTED_CMDS as u64,
        MEMBARRIER_CMD_GLOBAL
        | MEMBARRIER_CMD_GLOBAL_EXPEDITED
        | MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED
        | MEMBARRIER_CMD_PRIVATE_EXPEDITED
        | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED
        | MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ => {
            // x86 TSO guarantees that loads are not reordered with other loads,
            // stores are not reordered with other stores, and stores are not
            // reordered with earlier loads. A full memory barrier is implicit
            // for most operations; MFENCE/LFENCE/SFENCE are only needed for
            // non-temporal stores and write-combining memory. Return success.
            0
        }
        _ => EINVAL,
    }
}

const FUTEX_WAIT: i32 = 0;
const FUTEX_WAKE: i32 = 1;
const FUTEX_REQUEUE: i32 = 3;
const FUTEX_CMP_REQUEUE: i32 = 4;
const FUTEX_WAIT_BITSET: i32 = 9;
const FUTEX_WAKE_BITSET: i32 = 10;
const FUTEX_PRIVATE_FLAG: i32 = 128;
const FUTEX_CLOCK_REALTIME: i32 = 256;

pub(super) fn sys_futex(
    uaddr: *mut u32,
    futex_op: i32,
    val: u32,
    _timeout_ptr: u64,
    _uaddr2: *mut u32,
    val3: u32,
) -> u64 {
    if !validate_user_read_range(uaddr as u64, 4) {
        return EFAULT;
    }
    let caller_pid = crate::tasks::current_thread()
        .map(|t| t.process.lock().pid)
        .unwrap_or(0);
    let cmd = futex_op & !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);
    if cfg!(feature = "dsp_verbose") { crate::dprintln!("[FUTEX] op={:#x} cmd={} uaddr={:p} val={} to_ptr={:#x} pid={}", futex_op, cmd, uaddr, val, _timeout_ptr, caller_pid); }
    match cmd {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            if cmd == FUTEX_WAIT_BITSET && val3 == 0 {
                return EINVAL;
            }
            let current_val = unsafe { core::ptr::read_volatile(uaddr) };
            if current_val != val {
                return EAGAIN;
            }
            // Timed wait: park on the futex queue with a tick deadline; the
            // per-tick sweep wakes expiry (ETIMEDOUT) and FUTEX_WAKE wakes
            // early (0). Spinning here on sched_yield burned both cores under
            // Mesa/llvmpipe's timed waits — never spin in a syscall again.
            // Deadline unit is timer ticks (~1ms); sub-ms timeouts expire on
            // the next tick (~1ms late), which condvar users tolerate.
            let deadline = if _timeout_ptr != 0 && validate_user_read_range(_timeout_ptr, 16) {
                let ts = unsafe { *(_timeout_ptr as *const (i64, i64)) };
                let target_ns = (ts.0.max(0) as u64).saturating_mul(1_000_000_000).saturating_add(ts.1.max(0) as u64);
                let now_ticks = crate::time::TICKS.load(core::sync::atomic::Ordering::Relaxed) as u64;
                if cmd == FUTEX_WAIT_BITSET {
                    // Absolute timeout: FUTEX_CLOCK_REALTIME selects CLOCK_REALTIME, else CLOCK_MONOTONIC
                    let current_ns = if (futex_op & FUTEX_CLOCK_REALTIME) != 0 {
                        crate::time::realtime_nanos().max(0) as u64
                    } else {
                        crate::time::monotonic_nanos(now_ticks as usize) as u64
                    };
                    if target_ns <= current_ns {
                        return super::ETIMEDOUT;
                    }
                    let diff_ns = target_ns - current_ns;
                    now_ticks.saturating_add(diff_ns.saturating_add(999_999) / 1_000_000)
                } else {
                    // Relative timeout
                    now_ticks.saturating_add(target_ns.saturating_add(999_999) / 1_000_000)
                }
            } else {
                u64::MAX
            };
            crate::tasks::block_current_thread(crate::tasks::PendingBlock::Futex {
                uaddr: uaddr as u64,
                expected: val,
                deadline,
                pid: caller_pid,
            });
            // Woken: value changed (WAKE or racing store) -> success; past
            // deadline with value unchanged -> the tick sweep expired us.
            // A WAKE with unchanged value (pthread_cond_signal) is success —
            // the waiter re-checks its own predicate, not the futex word.
            let cv = unsafe { core::ptr::read_volatile(uaddr) };
            if cv != val {
                return 0;
            }
            let now = crate::time::TICKS.load(core::sync::atomic::Ordering::Relaxed) as u64;
            if now >= deadline {
                return super::ETIMEDOUT;
            }
            0
        }
        FUTEX_WAKE | FUTEX_WAKE_BITSET => {
            if cmd == FUTEX_WAKE_BITSET && val3 == 0 {
                return EINVAL;
            }
            let count = val as usize;
            let woken = crate::tasks::wake_futex_waiters(uaddr as u64, count, caller_pid);
            woken as u64
        }
        FUTEX_CMP_REQUEUE => {
            if _uaddr2.is_null() || !validate_user_read_range(_uaddr2 as u64, 4) {
                return EFAULT;
            }
            let current_val = unsafe { core::ptr::read_volatile(uaddr) };
            if current_val != val3 {
                return EAGAIN;
            }
            let max_wake = val as usize;
            let max_requeue = _timeout_ptr as u32 as usize;
            let (woken, requeued) = crate::tasks::requeue_futex_waiters(
                uaddr as u64,
                max_wake,
                _uaddr2 as u64,
                max_requeue,
                caller_pid,
            );
            (woken + requeued) as u64
        }
        FUTEX_REQUEUE => {
            if _uaddr2.is_null() || !validate_user_read_range(_uaddr2 as u64, 4) {
                return EFAULT;
            }
            let max_wake = val as usize;
            let max_requeue = _timeout_ptr as u32 as usize;
            let (woken, requeued) = crate::tasks::requeue_futex_waiters(
                uaddr as u64,
                max_wake,
                _uaddr2 as u64,
                max_requeue,
                caller_pid,
            );
            (woken + requeued) as u64
        }
        _ => 0,
    }
}


pub(super) fn sys_prctl(option: i32, arg2: u64, _arg3: u64, _arg4: u64, _arg5: u64) -> u64 {
    const PR_SET_NAME: i32 = 15;
    if option == PR_SET_NAME {
        if !validate_user_read_range(arg2, 16) {
            return super::EFAULT;
        }
        let mut name = [0u8; 16];
        let mut i = 0;
        while i < 15 {
            let b = unsafe { core::ptr::read((arg2 + i as u64) as *const u8) };
            if b == 0 { break; }
            name[i] = b;
            i += 1;
        }
        return 0;
    }
    0
}

pub(super) fn sys_sched_getaffinity(_pid: i32, cpusetsize: usize, mask_ptr: u64) -> u64 {
    if mask_ptr == 0 || !validate_user_write_range(mask_ptr, cpusetsize) {
        return super::EFAULT;
    }
    unsafe {
        core::ptr::write_bytes(mask_ptr as *mut u8, 0, cpusetsize);
        let ncpus = crate::smp::detect_cpu_count();
        let mut mask: u64 = 0;
        for i in 0..ncpus {
            mask |= 1 << i;
        }
        core::ptr::write(mask_ptr as *mut u64, mask);
    }
    cpusetsize as u64
}

pub(super) fn sys_sched_setaffinity(pid: i32, cpusetsize: usize, mask_ptr: u64) -> u64 {
    if mask_ptr == 0 || !validate_user_read_range(mask_ptr, cpusetsize) {
        return super::EFAULT;
    }
    if pid != 0 && pid != crate::tasks::current_thread().map(|t| t.id as i32).unwrap_or(-1) {
        return super::ESRCH;
    }
    // DragOS runs all threads on all available CPUs; accept any affinity mask.
    0
}

pub(super) fn sys_mlock(_addr: u64, _len: usize) -> u64 {
    // DragOS locks all kernel memory; user pages are never swapped.
    0
}

pub(super) fn sys_mlockall(_flags: i32) -> u64 {
    0
}

pub(super) fn sys_munlock(_addr: u64, _len: usize) -> u64 {
    0
}

pub(super) fn sys_mlock2(_addr: u64, _len: usize, _flags: i32) -> u64 {
    0
}

pub(super) fn sys_munlockall() -> u64 {
    0
}

pub(super) fn sys_unshare(flags: i32) -> u64 {
    // Accept isolation flags (CLONE_NEW*) but provide no actual isolation.
    // Returning 0 tells the caller "unshare succeeded" so sandboxed code
    // proceeds. Real isolation would require namespace infrastructure.
    let _ = flags;
    0
}

pub(super) fn sys_setns(fd: i32, nstype: i32) -> u64 {
    // Accept any fd/nstype pair and report success.
    let _ = (fd, nstype);
    0
}

pub(super) fn sys_seccomp(op: i32, _flags: i32, args: u64) -> u64 {
    // SECCOMP_SET_MODE_STRICT (0): install a default-deny policy (no-op safe).
    // SECCOMP_SET_MODE_FILTER (1): accept the filter prog but ignore it.
    match op {
        0 | 1 => {
            let _ = args;
            0
        }
        _ => super::EINVAL,
    }
}

pub(super) fn sys_ptrace(request: i32, pid_arg: i64, addr: u64, data: u64) -> u64 {
    let me = crate::tasks::get_current_pid();

    match request {
        PTRACE_TRACEME => {
            // The tracee asks to be traced by its parent. No pid argument.
            let t = match crate::tasks::current_thread() {
                Some(t) => t,
                None => return ESRCH,
            };
            {
                let mut p = t.process.lock();
                if p.tracer_pid != 0 {
                    return EPERM; // already traced
                }
                p.tracer_pid = p.ppid;
            }
            0
        }

        PTRACE_ATTACH | PTRACE_SEIZE => {
            if pid_arg <= 0 || (pid_arg as u64) == me {
                return EPERM;
            }
            let target = match ptrace_target(pid_arg as u64) {
                Some(t) => t,
                None => return ESRCH,
            };
            {
                let mut p = target.lock();
                if p.status == ProcessStatus::Zombie || p.tracer_pid != 0 {
                    return EPERM;
                }
                p.tracer_pid = me;
                if request == PTRACE_SEIZE {
                    p.ptrace_flags = data as u32;
                }
            }
            // ATTACH stops the tracee immediately (a SIGSTOP-style ptrace-stop)
            // so the tracer's waitpid(WUNTRACED) can reap it. SEIZE only arms
            // the relationship; PTRACE_INTERRUPT stops on demand.
            if request == PTRACE_ATTACH {
                ptrace_stop(&target, crate::arch::x86_64::abi::SIGSTOP);
            }
            0
        }

        PTRACE_INTERRUPT => {
            if pid_arg <= 0 {
                return EINVAL;
            }
            let target = match ptrace_target(pid_arg as u64) {
                Some(t) => t,
                None => return ESRCH,
            };
            {
                let p = target.lock();
                if p.tracer_pid != me {
                    return EPERM;
                }
            }
            ptrace_stop(&target, crate::arch::x86_64::abi::SIGSTOP);
            0
        }

        PTRACE_LISTEN => {
            // Listen mode (used by seccomp-bpf tracers): accepted, no-op.
            0
        }

        PTRACE_CONT | PTRACE_DETACH => {
            if pid_arg <= 0 {
                return EINVAL;
            }
            let target = match ptrace_target(pid_arg as u64) {
                Some(t) => t,
                None => return ESRCH,
            };
            {
                let p = target.lock();
                if p.tracer_pid != me {
                    return EPERM;
                }
                if p.status == ProcessStatus::Zombie {
                    return 0;
                }
            }
            ptrace_resume(&target, data as i32, request == PTRACE_DETACH);
            0
        }

        PTRACE_KILL => {
            if pid_arg <= 0 {
                return EINVAL;
            }
            let target = match ptrace_target(pid_arg as u64) {
                Some(t) => t,
                None => return ESRCH,
            };
            {
                let p = target.lock();
                if p.tracer_pid != me {
                    return EPERM;
                }
                if p.status == ProcessStatus::Zombie {
                    return 0;
                }
            }
            ptrace_kill(&target);
            0
        }

        PTRACE_PEEKTEXT | PTRACE_PEEKDATA => {
            let cr3 = match ptrace_tracee_cr3(pid_arg, me) {
                Ok(c) => c,
                Err(e) => return e,
            };
            let mut word = [0u8; 8];
            if !crate::memory::space::read_user_mem_cr3(cr3, addr, &mut word) {
                return EIO;
            }
            if !validate_user_write_range(data, 8) {
                return EFAULT;
            }
            unsafe {
                *(data as *mut u64) = u64::from_le_bytes(word);
            }
            0
        }

        PTRACE_POKETEXT | PTRACE_POKEDATA => {
            let cr3 = match ptrace_tracee_cr3(pid_arg, me) {
                Ok(c) => c,
                Err(e) => return e,
            };
            if !crate::memory::space::write_user_mem_cr3(cr3, addr, &data.to_le_bytes()) {
                return EIO;
            }
            0
        }

        // User-area register access (offset-based) is not supported: the
        // x86-64 user_regs_struct offset map is not worth replicating when
        // GETREGS/SETREGS (or the GETREGSET iovec form) cover register access.
        PTRACE_PEEKUSR | PTRACE_POKEUSR => EIO,

        PTRACE_GETREGS => {
            let (t, sp) = match ptrace_tracee_stack(pid_arg, me) {
                Ok(x) => x,
                Err(e) => return e,
            };
            if !validate_user_write_range(data, 27 * 8) {
                return EFAULT;
            }
            let ctx = unsafe { crate::tasks::context_from_stack(sp) };
            let buf = unsafe { core::slice::from_raw_parts_mut(data as *mut u64, 27) };
            buf[0] = ctx.r15;
            buf[1] = ctx.r14;
            buf[2] = ctx.r13;
            buf[3] = ctx.r12;
            buf[4] = ctx.rbp;
            buf[5] = ctx.rbx;
            buf[6] = ctx.r11;
            buf[7] = ctx.r10;
            buf[8] = ctx.r9;
            buf[9] = ctx.r8;
            buf[10] = ctx.rax;
            buf[11] = ctx.rcx;
            buf[12] = ctx.rdx;
            buf[13] = ctx.rsi;
            buf[14] = ctx.rdi;
            buf[15] = t.last_syscall_nr; // orig_rax
            buf[16] = ctx.rip;
            buf[17] = ctx.cs;
            buf[18] = ctx.rflags; // eflags
            buf[19] = ctx.rsp;
            buf[20] = ctx.ss;
            buf[21] = t.fs_base; // fs_base
            buf[22] = 0;         // gs_base
            buf[23] = 0;         // ds
            buf[24] = 0;         // es
            buf[25] = 0;         // fs
            buf[26] = 0;         // gs
            0
        }

        PTRACE_SETREGS => {
            if !validate_user_read_range(data, 27 * 8) {
                return EFAULT;
            }
            let (_t, sp) = match ptrace_tracee_stack(pid_arg, me) {
                Ok(x) => x,
                Err(e) => return e,
            };
            let ctx = unsafe { crate::tasks::context_from_stack(sp) };
            let buf = unsafe { core::slice::from_raw_parts(data as *const u64, 27) };
            ctx.r15 = buf[0];
            ctx.r14 = buf[1];
            ctx.r13 = buf[2];
            ctx.r12 = buf[3];
            ctx.rbp = buf[4];
            ctx.rbx = buf[5];
            ctx.r11 = buf[6];
            ctx.r10 = buf[7];
            ctx.r9 = buf[8];
            ctx.r8 = buf[9];
            ctx.rax = buf[10];
            ctx.rcx = buf[11];
            ctx.rdx = buf[12];
            ctx.rsi = buf[13];
            ctx.rdi = buf[14];
            ctx.rip = buf[16];
            ctx.rflags = buf[18];
            ctx.rsp = buf[19];
            0
        }

        PTRACE_GETREGSET | PTRACE_SETREGSET => {
            if addr != NT_PRSTATUS {
                return EINVAL;
            }
            let mut iov = [0u8; 16];
            if !validate_user_read_range(data, 16) {
                return EFAULT;
            }
            unsafe {
                iov.copy_from_slice(core::slice::from_raw_parts(data as *const u8, 16));
            }
            let base = u64::from_le_bytes(iov[0..8].try_into().unwrap());
            let len = u64::from_le_bytes(iov[8..16].try_into().unwrap());
            const REGS_SIZE: usize = 27 * 8;
            let cap = core::cmp::min(len as usize, REGS_SIZE);
            if request == PTRACE_GETREGSET {
                if !validate_user_write_range(base, cap) {
                    return EFAULT;
                }
                let (t, sp) = match ptrace_tracee_stack(pid_arg, me) {
                    Ok(x) => x,
                    Err(e) => return e,
                };
                let ctx = unsafe { crate::tasks::context_from_stack(sp) };
                let mut tmp = [0u64; 27];
                tmp[0] = ctx.r15;
                tmp[1] = ctx.r14;
                tmp[2] = ctx.r13;
                tmp[3] = ctx.r12;
                tmp[4] = ctx.rbp;
                tmp[5] = ctx.rbx;
                tmp[6] = ctx.r11;
                tmp[7] = ctx.r10;
                tmp[8] = ctx.r9;
                tmp[9] = ctx.r8;
                tmp[10] = ctx.rax;
                tmp[11] = ctx.rcx;
                tmp[12] = ctx.rdx;
                tmp[13] = ctx.rsi;
                tmp[14] = ctx.rdi;
                tmp[15] = t.last_syscall_nr;
                tmp[16] = ctx.rip;
                tmp[17] = ctx.cs;
                tmp[18] = ctx.rflags;
                tmp[19] = ctx.rsp;
                tmp[20] = ctx.ss;
                tmp[21] = t.fs_base;
                let buf = unsafe { core::slice::from_raw_parts_mut(base as *mut u64, cap / 8) };
                buf.copy_from_slice(&tmp[..buf.len()]);
                // Report the number of bytes copied back through the iovec.
                let new_len = (cap as u64).to_le_bytes();
                if !validate_user_write_range(data + 8, 8) {
                    return EFAULT;
                }
                unsafe {
                    core::ptr::copy_nonoverlapping(new_len.as_ptr(), (data + 8) as *mut u8, 8);
                }
                0
            } else {
                if !validate_user_read_range(base, cap) {
                    return EFAULT;
                }
                let (_t, sp) = match ptrace_tracee_stack(pid_arg, me) {
                    Ok(x) => x,
                    Err(e) => return e,
                };
                let ctx = unsafe { crate::tasks::context_from_stack(sp) };
                let buf = unsafe { core::slice::from_raw_parts(base as *const u64, cap / 8) };
                if buf.len() >= 1 {
                    ctx.r15 = buf[0];
                }
                if buf.len() >= 2 {
                    ctx.r14 = buf[1];
                }
                if buf.len() >= 3 {
                    ctx.r13 = buf[2];
                }
                if buf.len() >= 4 {
                    ctx.r12 = buf[3];
                }
                if buf.len() >= 5 {
                    ctx.rbp = buf[4];
                }
                if buf.len() >= 6 {
                    ctx.rbx = buf[5];
                }
                if buf.len() >= 7 {
                    ctx.r11 = buf[6];
                }
                if buf.len() >= 8 {
                    ctx.r10 = buf[7];
                }
                if buf.len() >= 9 {
                    ctx.r9 = buf[8];
                }
                if buf.len() >= 10 {
                    ctx.r8 = buf[9];
                }
                if buf.len() >= 11 {
                    ctx.rax = buf[10];
                }
                if buf.len() >= 12 {
                    ctx.rcx = buf[11];
                }
                if buf.len() >= 13 {
                    ctx.rdx = buf[12];
                }
                if buf.len() >= 14 {
                    ctx.rsi = buf[13];
                }
                if buf.len() >= 15 {
                    ctx.rdi = buf[14];
                }
                if buf.len() >= 17 {
                    ctx.rip = buf[16];
                }
                if buf.len() >= 19 {
                    ctx.rflags = buf[18];
                }
                if buf.len() >= 20 {
                    ctx.rsp = buf[19];
                }
                0
            }
        }

        PTRACE_GETSIGINFO => {
            if pid_arg <= 0 {
                return EINVAL;
            }
            let sig = {
                let target = match ptrace_target(pid_arg as u64) {
                    Some(t) => t,
                    None => return ESRCH,
                };
                let p = target.lock();
                if p.tracer_pid != me {
                    return EPERM;
                }
                p.ptrace_sig
            };
            if !validate_user_write_range(data, 128) {
                return EFAULT;
            }
            unsafe {
                core::ptr::write_bytes(data as *mut u8, 0, 128);
                let p = data as *mut i32;
                *p = sig; // si_signo
                *p.add(1) = 0; // si_errno
                *p.add(2) = 0; // si_code = SI_USER
            }
            0
        }

        PTRACE_SETSIGINFO => {
            if !validate_user_read_range(data, 128) {
                return EFAULT;
            }
            // Accepted but not stored: the only trace state we track is the trap
            // signal itself, which the tracer controls via CONT/DETACH.
            0
        }

        PTRACE_GETSIGMASK | PTRACE_SETSIGMASK => {
            let mut iov = [0u8; 16];
            if !validate_user_read_range(data, 16) {
                return EFAULT;
            }
            unsafe {
                iov.copy_from_slice(core::slice::from_raw_parts(data as *const u8, 16));
            }
            let base = u64::from_le_bytes(iov[0..8].try_into().unwrap());
            let len = u64::from_le_bytes(iov[8..16].try_into().unwrap());
            if len < 8 {
                return EINVAL;
            }
            if request == PTRACE_GETSIGMASK {
                let (t, _sp) = match ptrace_tracee_stack(pid_arg, me) {
                    Ok(x) => x,
                    Err(e) => return e,
                };
                if !validate_user_write_range(base, 8) {
                    return EFAULT;
                }
                unsafe {
                    *(base as *mut u64) = t.sigmask;
                }
                0
            } else {
                if !validate_user_read_range(base, 8) {
                    return EFAULT;
                }
                let mask = unsafe { *(base as *const u64) };
                if pid_arg <= 0 {
                    return EINVAL;
                }
                let target = match ptrace_target(pid_arg as u64) {
                    Some(t) => t,
                    None => return ESRCH,
                };
                let (new_thread, tid) = {
                    let p = target.lock();
                    if p.tracer_pid != me {
                        return EPERM;
                    }
                    let mut tp = match p.threads.values().next() {
                        Some(t) => (**t).clone(),
                        None => return ESRCH,
                    };
                    let tid = tp.id;
                    tp.sigmask = mask;
                    (tp, tid)
                };
                let mut p = target.lock();
                p.threads.insert(tid, Arc::new(new_thread));
                0
            }
        }

        PTRACE_SINGLESTEP | PTRACE_SYSCALL => EIO,

        _ => EIO,
    }
}

const PTRACE_TRACEME: i32 = 0;
const PTRACE_PEEKTEXT: i32 = 1;
const PTRACE_PEEKDATA: i32 = 2;
const PTRACE_PEEKUSR: i32 = 3;
const PTRACE_POKETEXT: i32 = 4;
const PTRACE_POKEDATA: i32 = 5;
const PTRACE_POKEUSR: i32 = 6;
const PTRACE_CONT: i32 = 7;
const PTRACE_KILL: i32 = 8;
const PTRACE_SINGLESTEP: i32 = 9;
const PTRACE_GETREGS: i32 = 12;
const PTRACE_SETREGS: i32 = 13;
const PTRACE_ATTACH: i32 = 16;
const PTRACE_DETACH: i32 = 17;
const PTRACE_SYSCALL: i32 = 24;
const PTRACE_GETSIGINFO: i32 = 0x4202;
const PTRACE_SETSIGINFO: i32 = 0x4203;
const PTRACE_GETREGSET: i32 = 0x4204;
const PTRACE_SETREGSET: i32 = 0x4205;
const PTRACE_SEIZE: i32 = 0x4206;
const PTRACE_INTERRUPT: i32 = 0x4207;
const PTRACE_LISTEN: i32 = 0x4208;
const PTRACE_GETSIGMASK: i32 = 0x420a;
const PTRACE_SETSIGMASK: i32 = 0x420b;

const NT_PRSTATUS: u64 = 1;

/// Find a live process by pid (Arc clone; the table lock is dropped before
/// returning, per the PROCESS_TABLE → process lock ordering).
fn ptrace_target(pid: u64) -> Option<Arc<SpinLock<Process>>> {
    PROCESS_TABLE.lock().get(&pid).cloned()
}

/// Mark `target` stopped as a ptrace-stop with `sig`, then wake its tracer's
/// waitpid(WUNTRACED). No locks are held on entry or exit. The signal is also
/// queued so the tracee's next delivery boundary confirms the trap via
/// `ptrace_trap` and halts the tracee; `ptrace_trap` discards the bit there.
fn ptrace_stop(target: &Arc<SpinLock<Process>>, sig: i32) {
    let tracer = {
        let mut p = target.lock();
        if p.status != ProcessStatus::Stopped {
            p.status = ProcessStatus::Stopped;
            p.stopped.store(true, core::sync::atomic::Ordering::Release);
        }
        p.stop_signal = sig;
        p.ptrace_sig = sig;
        p.pending |= 1u64 << (sig - 1);
        p.sigqueue.retain(|i| i.signo != sig);
        p.sigqueue.push_back(crate::tasks::SigInfo {
            signo: sig,
            code: crate::tasks::SI_USER,
            pid: 0,
            value: 0,
        });
        p.ptrace_thread = Some(crate::tasks::current_thread().expect("ptrace_stop thread"));
        p.tracer_pid
    };
    if tracer != 0 {
        crate::tasks::wake_parked_for_pid(tracer);
    }
}

/// Resume `target` from a stop (`PTRACE_CONT` / `PTRACE_DETACH`). If `sig`
/// is nonzero it is re-delivered (the consumed trap signal). `detach` clears
/// the tracer relationship.
fn ptrace_resume(target: &Arc<SpinLock<Process>>, sig: i32, detach: bool) {
    let pid = target.lock().pid;
    {
        let mut p = target.lock();
        if (1..=31).contains(&sig) {
            // Idempotent: the consumed trap signal may already be queued if
            // the tracer continues before the delivery boundary consumed it.
            p.sigqueue.retain(|i| i.signo != sig);
            p.pending |= 1u64 << (sig - 1);
            p.sigqueue.push_back(crate::tasks::SigInfo {
                signo: sig,
                code: crate::tasks::SI_USER,
                pid: 0,
                value: 0,
            });
        }
        p.ptrace_thread = None;
        p.status = ProcessStatus::Running;
        p.stopped.store(false, core::sync::atomic::Ordering::Release);
        p.stop_signal = 0;
        p.ptrace_sig = 0;
        if detach {
            p.tracer_pid = 0;
        }
    }
    crate::tasks::wake_sleep_waiters_for_pid(pid);
    crate::tasks::wake_blocked_for_pid(pid);
    crate::tasks::wake_parked_for_pid(pid);
}

/// Kill a tracee (`PTRACE_KILL`): SIGKILL is always fatal, so queue it and
/// force the process runnable so the scheduler reaches the fatal-signal check.
fn ptrace_kill(target: &Arc<SpinLock<Process>>) {
    let pid = target.lock().pid;
    {
        let mut p = target.lock();
        p.pending |= 1u64 << (crate::arch::x86_64::abi::SIGKILL - 1);
        p.sigqueue.push_back(crate::tasks::SigInfo {
            signo: crate::arch::x86_64::abi::SIGKILL,
            code: crate::tasks::SI_USER,
            pid: 0,
            value: 0,
        });
        p.ptrace_thread = None;
        p.status = ProcessStatus::Running;
        p.stopped.store(false, core::sync::atomic::Ordering::Release);
        p.stop_signal = 0;
        p.ptrace_sig = 0;
    }
    crate::tasks::wake_sleep_waiters_for_pid(pid);
    crate::tasks::wake_blocked_for_pid(pid);
    crate::tasks::wake_parked_for_pid(pid);
}

/// Validate `pid_arg` names a live process traced by `me`; return its cr3.
fn ptrace_tracee_cr3(pid_arg: i64, me: u64) -> Result<PhysAddr, u64> {
    if pid_arg <= 0 {
        return Err(EINVAL);
    }
    let target = match ptrace_target(pid_arg as u64) {
        Some(t) => t,
        None => return Err(ESRCH),
    };
    let p = target.lock();
    if p.tracer_pid != me {
        return Err(EPERM);
    }
    Ok(p.cr3)
}

/// Validate `pid_arg` names a process traced by `me` and currently stopped;
/// return its first thread and kernel-stack frame pointer.
fn ptrace_tracee_stack(pid_arg: i64, me: u64) -> Result<(Arc<Thread>, u64), u64> {
    if pid_arg <= 0 {
        return Err(EINVAL);
    }
    let target = match ptrace_target(pid_arg as u64) {
        Some(t) => t,
        None => return Err(ESRCH),
    };
    let t = {
        let p = target.lock();
        if p.tracer_pid != me {
            return Err(EPERM);
        }
        if p.status != ProcessStatus::Stopped {
            return Err(ESRCH);
        }
        if let Some(pt) = &p.ptrace_thread {
            return Ok((Arc::new(pt.clone()), pt.stack_ptr));
        }
        p.threads.values().next().cloned()
    };
    let t = match t {
        Some(t) => t,
        None => return Err(ESRCH),
    };
    if t.stack_ptr == 0 || t.stack_ptr & 7 != 0 {
        return Err(ESRCH);
    }
    let sp = t.stack_ptr;
    Ok((t, sp))
}

pub(super) fn sys_sync_file_range(fd: i32, _offset: u64, _nbytes: u64, _flags: u32) -> u64 {
    // Kernel doesn't have page cache; return ENOSYS so callers fall back
    // to fsync(). Keeping ENOSYS signals "not implemented" to probing code.
    let _ = fd;
    super::ENOSYS
}
