use super::*;

pub(super) fn sys_getpid() -> u64 {
    with_current_process(|p| p.pid)
}

pub(super) fn sys_getppid() -> u64 {
    with_current_process(|p| p.ppid)
}

// `getpgid(2)` lives further down, next to `setpgid`, where the pgid field it
// reads is documented. The version that used to be here returned `p.pid` — the
// process's own id, not its group leader's — so every call was wrong except by
// coincidence when the caller happened to lead its own group, which is the
// common case and exactly why nobody noticed.

pub(super) fn sys_getuid() -> u64 {
    with_current_process(|p| p.uid as u64)
}

pub(super) fn sys_geteuid() -> u64 {
    with_current_process(|p| p.euid as u64)
}

pub(super) fn sys_getgid() -> u64 {
    with_current_process(|p| p.gid as u64)
}

pub(super) fn sys_getegid() -> u64 {
    with_current_process(|p| p.egid as u64)
}

/// Linux semantics (simplified): root (euid==0) sets real/effective/saved uid;
/// unprivileged may only set effective uid to a value it already has (real,
/// effective, or saved). Returns 0 on success, EPERM on refusal.
pub(super) fn sys_setuid(uid: u32) -> u64 {
    with_current_process(|p| setuid(p, uid))
}

pub fn setuid(p: &mut Process, uid: u32) -> u64 {
    let r = if p.euid == 0 {
        p.uid = uid;
        p.euid = uid;
        p.suid = uid;
        0
    } else if uid == p.uid || uid == p.euid || uid == p.suid {
        p.euid = uid;
        0
    } else {
        EPERM
    };
    if r == 0 {
        recompute_caps(p);
    }
    r
}

/// Recompute the capability set after a credential change.
///
/// Linux keeps the permitted set across `setuid` until the process is fully
/// unprivileged; this kernel's model is the simpler "a process is privileged
/// as long as its effective uid is 0". That is what makes `setuid(1000)`
/// actually cost the process its access — without this call the uid would
/// change and every DAC bypass would survive it.
pub fn recompute_caps(p: &mut Process) {
    p.caps = if p.euid == 0 { crate::fs::perm::CAP_ALL } else { 0 };
}

pub(super) fn sys_setgid(gid: u32) -> u64 {
    with_current_process(|p| {
        let r = if p.egid == 0 {
            p.gid = gid;
            p.egid = gid;
            p.sgid = gid;
            0
        } else if gid == p.gid || gid == p.egid || gid == p.sgid {
            p.egid = gid;
            0
        } else {
            EPERM
        };
        if r == 0 {
            recompute_caps(p);
        }
        r
    })
}

pub(super) fn sys_getresuid(ruid: *mut u32, euid: *mut u32, suid: *mut u32) -> u64 {
    let (r, e, s) = with_current_process(|p| (p.uid, p.euid, p.suid));
    let ptrs = [(ruid, r), (euid, e), (suid, s)];
    for (ptr, val) in ptrs {
        if !ptr.is_null() {
            if !validate_user_write_range(ptr as u64, 4) {
                return EFAULT;
            }
            unsafe { core::ptr::write_volatile(ptr, val) };
        }
    }
    0
}

pub(super) fn sys_getresgid(rgid: *mut u32, egid: *mut u32, sgid: *mut u32) -> u64 {
    let (r, e, s) = with_current_process(|p| (p.gid, p.egid, p.sgid));
    let ptrs = [(rgid, r), (egid, e), (sgid, s)];
    for (ptr, val) in ptrs {
        if !ptr.is_null() {
            if !validate_user_write_range(ptr as u64, 4) {
                return EFAULT;
            }
            unsafe { core::ptr::write_volatile(ptr, val) };
        }
    }
    0
}

/// `u32::MAX` (`-1`) means "leave unchanged". Root may set any; unprivileged
/// may only set each id to one it already holds (real/effective/saved).
/// Returns 0 on success, EPERM on refusal.
pub(super) fn sys_setresuid(ruid: u32, euid: u32, suid: u32) -> u64 {
    with_current_process(|p| setresid(p, ruid, euid, suid))
}

pub fn setresid(p: &mut Process, ruid: u32, euid: u32, suid: u32) -> u64 {
    if p.euid != 0 {
        let cur = [p.uid, p.euid, p.suid];
        for v in [ruid, euid, suid] {
            if v != u32::MAX && !cur.contains(&v) {
                return EPERM;
            }
        }
    }
    if ruid != u32::MAX {
        p.uid = ruid;
    }
    if euid != u32::MAX {
        p.euid = euid;
    }
    if suid != u32::MAX {
        p.suid = suid;
    }
    recompute_caps(p);
    0
}

pub(super) fn sys_setresgid(rgid: u32, egid: u32, sgid: u32) -> u64 {
    with_current_process(|p| {
        if p.egid != 0 {
            let cur = [p.gid, p.egid, p.sgid];
            for v in [rgid, egid, sgid] {
                if v != u32::MAX && !cur.contains(&v) {
                    return EPERM;
                }
            }
        }
        if rgid != u32::MAX {
            p.gid = rgid;
        }
        if egid != u32::MAX {
            p.egid = egid;
        }
        if sgid != u32::MAX {
            p.sgid = sgid;
        }
        recompute_caps(p);
        0
    })
}

pub(super) fn sys_setreuid(ruid: u32, euid: u32) -> u64 {
    with_current_process(|p| setresid(p, ruid, euid, u32::MAX))
}

pub(super) fn sys_setregid(rgid: u32, egid: u32) -> u64 {
    sys_setresgid(rgid, egid, u32::MAX)
}

/// `getgroups(2)`: copy the supplementary group list into `list`.
///
/// Returns the *full* count as Linux does, even when `size` is smaller than the
/// list, so that a caller probing with `size = 0` learns how much room it needs.
/// A `size` above zero with a null `list` is `EFAULT`.
pub(super) fn sys_getgroups(size: i32, list: *mut u32) -> u64 {
    if size < 0 {
        return EINVAL;
    }
    let groups = match crate::fs::perm::current_creds() {
        Some(c) => c.groups,
        None => return EPERM,
    };
    if size == 0 {
        return groups.len() as u64;
    }
    if list.is_null() {
        return EFAULT;
    }
    let n = core::cmp::min(size as usize, groups.len());
    for (i, gid) in groups.iter().take(n).enumerate() {
        let ptr = unsafe { list.add(i) };
        if !validate_user_write_range(ptr as u64, 4) {
            return EFAULT;
        }
        unsafe { core::ptr::write_volatile(ptr, *gid) };
    }
    groups.len() as u64
}

/// `setgroups(2)`: replace the supplementary group list. Root only, as on
/// Linux — an unprivileged process could otherwise add itself to any group and
/// bypass every group-class access check.
pub(super) fn sys_setgroups(size: usize, list: *const u32) -> u64 {
    // A cap on the list, so a caller cannot grow the process's allocation
    // without bound. Linux uses NGROUPS_MAX (65536); this is the same order.
    const NGROUPS_MAX: usize = 65536;
    if size > NGROUPS_MAX {
        return EINVAL;
    }
    let mut new_groups = alloc::vec::Vec::with_capacity(size);
    for i in 0..size {
        let ptr = unsafe { list.add(i) };
        if !validate_user_read_range(ptr as u64, 4) {
            return EFAULT;
        }
        new_groups.push(unsafe { core::ptr::read_volatile(ptr) });
    }
    with_current_process(|p| {
        if p.euid != 0 {
            return EPERM;
        }
        p.groups = new_groups;
        0
    })
}


pub(super) fn sys_clone(
    sys_no: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
    arg5: u64,
    arg6: u64,
    regs: SyscallRegs,
) -> u64 {
    const CLONE_SETTLS: u64 = 0x0008_0000;
    const CLONE_PARENT_SETTID: u64 = 0x0010_0000;
    const CLONE_CHILD_CLEARTID: u64 = 0x0020_0000;
    const CLONE_CHILD_SETTID: u64 = 0x0100_0000;
    const CLONE_THREAD: u64 = 0x0001_0000;

    // clone3 (syscall 435) passes arguments via `struct clone_args` in userspace:
    //   arg1 = pointer to struct clone_args, arg2 = size of struct
    // struct clone_args (88 bytes):
    //   [0] flags, [8] pidfd, [16] child_tid, [24] parent_tid,
    //   [32] exit_signal, [40] stack, [48] stack_size, [56] tls, ...
    let (flags, new_user_rsp, ptid_ptr, ctid_ptr, tls_val, r9_fn) =
        if sys_no == 435 {
            let args_ptr = arg1;
            let args_size = arg2;
            if args_size < 88 {
                return EINVAL;
            }
            if !validate_user_read_range(args_ptr, 88) {
                return EFAULT;
            }
            unsafe {
                let args = &*(args_ptr as *const [u64; 11]);
                crate::println!("[C3ARGS] flags={:#x} pidfd={:#x} child_tid={:#x} parent_tid={:#x} exitsig={:#x} stack={:#x} stack_sz={:#x} tls={:#x}",
                    args[0], args[1], args[2], args[3], args[4], args[5], args[6], args[7]);
                let clone_flags = args[0];
                let child_tid = args[2];
                let parent_tid = args[3];
                let exit_signal = args[4] & 0xFFFF;
                let stack = args[5];
                let stack_size = args[6];
                let tls = args[7];
                // Merge exit_signal into the low 16 bits of flags
                let flags = (clone_flags & !0xFFFF) | exit_signal;
                // clone3: stack points to the LOWEST address; compute top
                let user_stack = if stack != 0 && stack_size != 0 {
                    stack + stack_size
                } else {
                    stack
                };
                // r9_fn unused for clone3: child continues from return point
                (flags, user_stack, parent_tid, child_tid, tls, 0)
            }
        } else {
            (arg1, arg2, arg3, arg4, arg5, arg6)
        };

    let user_rip = crate::smp::with_cpu_state(|state| state.user_rip);
    let user_rflags = crate::smp::with_cpu_state(|state| state.user_rflags);
    let user_rsp = if new_user_rsp != 0 {
        new_user_rsp
    } else {
        crate::smp::with_cpu_state(|state| state.user_stack)
    };

    let parent = crate::tasks::current_thread().expect("sys_clone: no current thread");
    let parent_fs = parent.fs_base;
    let parent_sigmask = parent.sigmask;

    if (flags & CLONE_THREAD) == 0 {
        // Fork / posix_spawn: separate address space and new process
        let new_tid = next_tid();
        let new_pid = crate::tasks::next_pid();

        if (flags & CLONE_PARENT_SETTID) != 0 && validate_user_write_range(ptid_ptr, 4) {
            unsafe {
                *(ptid_ptr as *mut u32) = new_pid as u32;
            }
        }

        if (flags & CLONE_CHILD_SETTID) != 0 && validate_user_write_range(ctid_ptr, 4) {
            unsafe {
                *(ctid_ptr as *mut u32) = new_pid as u32;
            }
        }

        let parent_cr3 = parent.process.lock().cr3;
        let child_cr3 = {
            let mut alloc = FRAME_ALLOCATOR.lock();
            match fork_address_space(parent_cr3, &mut *alloc, HHDM_BASE.0) {
                Ok(p) => p,
                Err(_) => return EAGAIN,
            }
        };

        let child_process_obj = {
            let p = parent.process.lock();
            p.fork_from(new_pid, child_cr3)
        };

        let child_process = Arc::new(SpinLock::new(child_process_obj));
        PROCESS_TABLE.lock().insert(new_pid, child_process.clone());

        {
            let mut p = parent.process.lock();
            p.children.push(child_process.clone());
        }

        let (child_stack_ptr, child_stack_top, child_syscall_stack_top) = build_child_kernel_stack(
            user_rsp,
            user_rip,
            user_rflags,
            r9_fn,
            regs.rbx,
            regs.rbp,
            regs.r12,
            regs.r13,
            regs.r14,
            regs.r15,
        );

        let mut new_thread = Thread::new(
            new_tid,
            child_process,
            child_stack_ptr,
            child_stack_top,
            child_syscall_stack_top,
            10, // Default priority
            child_stack_top,
        );
        new_thread.fs_base = if (flags & CLONE_SETTLS) != 0 { tls_val } else { parent_fs };
        new_thread.sigmask = parent_sigmask;

        if (flags & CLONE_CHILD_CLEARTID) != 0 {
            new_thread.clear_child_tid = Some(ctid_ptr);
        }

        tasks::SCHEDULER.lock().push_back(new_thread);
        new_pid
    } else {
        // Thread creation within shared process
        // The Thread is moved into the scheduler queue below, and the process
        // keeps its own handle on it, so it is cloned rather than moved.
        let shared_process = parent.process.clone();
        shared_process.lock().threads_count += 1;
        let new_tid = crate::tasks::next_tid();
        let (child_stack_ptr, child_stack_top, child_syscall_stack_top) = build_child_kernel_stack(
            user_rsp,
            user_rip,
            user_rflags,
            r9_fn,
            regs.rbx,
            regs.rbp,
            regs.r12,
            regs.r13,
            regs.r14,
            regs.r15,
        );

        let mut new_thread = Thread::new(
            new_tid,
            shared_process.clone(),
            child_stack_ptr,
            child_stack_top,
            child_syscall_stack_top,
            10, // Default priority
            child_stack_top,
        );
        new_thread.fs_base = if (flags & CLONE_SETTLS) != 0 { tls_val } else { parent_fs };
        new_thread.sigmask = parent_sigmask;

        if (flags & CLONE_PARENT_SETTID) != 0 && validate_user_write_range(ptid_ptr, 4) {
            unsafe {
                *(ptid_ptr as *mut u32) = new_tid as u32;
            }
        }

        if (flags & CLONE_CHILD_SETTID) != 0 && validate_user_write_range(ctid_ptr, 4) {
            unsafe {
                *(ctid_ptr as *mut u32) = new_tid as u32;
            }
        }

        if (flags & CLONE_CHILD_CLEARTID) != 0 {
            new_thread.clear_child_tid = Some(ctid_ptr);
        }

        // Register the thread in its process's list *before* it becomes runnable.
        //
        // The ordering matters: a thread that is on the run queue but not yet
        // in `Process::threads` could be asked to exit (a signal, or its own
        // exit_group) and find no list entry to remove, which would leave the
        // count permanently high and make every later exit believe it was the
        // last thread. Registration first makes the invariant "a runnable
        // thread is always in the list" hold.
        {
            let mut sp = shared_process.lock();
            sp.threads.insert(new_thread.id, Arc::new(new_thread.clone()));
        }
        tasks::SCHEDULER.lock().push_back(new_thread);
        new_tid
    }
}

pub(super) fn sys_fork(regs: SyscallRegs) -> u64 {
    let user_rip = crate::smp::with_cpu_state(|state| state.user_rip);
    let user_rflags = crate::smp::with_cpu_state(|state| state.user_rflags);
    let user_rsp = crate::smp::with_cpu_state(|state| state.user_stack);

    let (parent_cr3, parent_fs, parent_sigmask) = {
        let parent = crate::tasks::current_thread().expect("fork: no current thread");
        let p = parent.process.lock();
        (p.cr3, parent.fs_base, parent.sigmask)
    };

    let child_cr3 = {
        let mut alloc = FRAME_ALLOCATOR.lock();
        match fork_address_space(parent_cr3, &mut *alloc, HHDM_BASE.0) {
            Ok(p) => p,
            Err(_) => return EAGAIN,
        }
    };

    let new_tid = next_tid();
    let new_pid = crate::tasks::next_pid();

    let child_process_obj = {
        let parent = crate::tasks::current_thread().expect("fork: no current thread");
        let p = parent.process.lock();
        p.fork_from(new_pid, child_cr3)
    };

    let child_process = Arc::new(SpinLock::new(child_process_obj));
    PROCESS_TABLE.lock().insert(new_pid, child_process.clone());

    let parent = crate::tasks::current_thread().expect("fork: no current thread");
    {
        let mut p = parent.process.lock();
        p.children.push(child_process.clone());
    }
    crate::tasks::set_current_thread(Some(parent));

    let (child_stack_ptr, child_stack_top, child_syscall_stack_top) = build_child_kernel_stack(
        user_rsp,
        user_rip,
        user_rflags,
        0,
        regs.rbx,
        regs.rbp,
        regs.r12,
        regs.r13,
        regs.r14,
        regs.r15,
    );

    let child_process_for_list = child_process.clone();

    let mut new_thread = Thread::new(
        new_tid,
        child_process,
        child_stack_ptr,
        child_stack_top,
        child_syscall_stack_top,
        10, // Default priority
        child_stack_top, // stack_limit: placeholder
    );
    new_thread.fs_base = parent_fs;
    new_thread.sigmask = parent_sigmask;

    {
        let cp = child_process_for_list.clone();
        let mut pp = cp.lock();
        pp.threads_count = pp.threads_count.max(1);
        pp.threads.insert(new_thread.id, Arc::new(new_thread.clone()));
    }

    SCHEDULER.lock().push_back(new_thread);

    new_pid
}

// Wait options
const WNOHANG: u32 = 0x1;
const WUNTRACED: u32 = 0x2;
const WCONTINUED: u32 = 0x8;

pub(super) fn sys_wait4(pid: i64, status_ptr: *mut i32, options: u32) -> u64 {
    if !status_ptr.is_null()
        && !validate_user_write_range(status_ptr as u64, core::mem::size_of::<i32>())
    {
        return EFAULT;
    }

    let want_stopped = (options & 0x2) != 0; // WUNTRACED
    let want_continued = (options & 0x8) != 0; // WCONTINUED

    loop {
        let thread = crate::tasks::current_thread().expect("sys_wait4: no current thread");
        // Phase A: snapshot the child list. The parent lock is held ONLY for
        // the clone — never while locking a child. Parent->child nesting is
        // an ABBA deadlock against exit's child->parent path (observed as
        // SPIN DEADLOCK at the child lock under reap load). Lock order per
        // AGENTS.md: process.lock() is innermost, never nested outward.
        let children = {
            let p = thread.process.lock();
            if p.children.is_empty() {
                return ECHILD;
            }
            p.children.clone()
        };
        if pid > 0 {
            let has_matching_child = children.iter().any(|c| c.lock().pid as i64 == pid);
            if !has_matching_child {
                return ECHILD;
            }
        }
        // Phase B: find a zombie OR stopped/continued child with each child locked ALONE.
        let mut target = None;
        let mut target_pid = 0u64;
        let mut target_code = 0i32;
        for child_arc in &children {
            let child = child_arc.lock();
            let status = child.status;
            let matches_pid = pid <= 0 || child.pid as i64 == pid;
            if matches_pid {
                if status == crate::tasks::ProcessStatus::Zombie {
                    target_pid = child.pid;
                    target_code = child.exit_code;
                    target = Some(child_arc.clone());
                    break;
                } else if status == crate::tasks::ProcessStatus::Stopped && want_stopped {
                    target_pid = child.pid;
                    target_code = child.stop_signal;
                    target = Some(child_arc.clone());
                    break;
                } else if child.continued && want_continued {
                    // Child was previously stopped and has been continued
                    target_pid = child.pid;
                    target_code = crate::arch::x86_64::abi::SIGCONT;
                    target = Some(child_arc.clone());
                    break;
                }
            }
            drop(child);
        }

        if let Some(t) = target {
            // Phase C: claim by Arc identity under the parent lock.
            let mut p = thread.process.lock();
            if let Some(pos) = p.children.iter().position(|c| Arc::ptr_eq(c, &t)) {
                let child_status = t.lock().status;
                // For stopped/continued children, we DON'T remove them from the children list
                // or PROCESS_TABLE - they're still alive. Only zombies are removed.
                if child_status == crate::tasks::ProcessStatus::Zombie {
                    p.children.remove(pos);
                    // Clear pending SIGCHLD unconditionally: scans (not this bit)
                    // find zombies, so this is safe; a stale bit would busy-loop
                    // below instead of blocking.
                    p.pending &= !(1u64 << 16);
                    drop(p);
                    // PROCESS_TABLE removal outside the parent lock: nesting it
                    // inside reverses sys_getpgid's PROCESS_TABLE->process order.
                    PROCESS_TABLE.lock().remove(&target_pid);
                } else if child_status == crate::tasks::ProcessStatus::Stopped {
                    // Stopped child: don't remove from children list or PROCESS_TABLE
                    // Just clear the pending bit and keep the child in the list
                    p.pending &= !(1u64 << 16);
                    drop(p);
                } else if t.lock().continued {
                    // Continued child: don't remove, just clear the continued flag
                    t.lock().continued = false;
                    p.pending &= !(1u64 << 16);
                    drop(p);
                } else {
                    drop(p);
                }
                if !status_ptr.is_null() {
                    let encoded = match child_status {
                        crate::tasks::ProcessStatus::Zombie => target_code << 8,
                        crate::tasks::ProcessStatus::Stopped => {
                            // Stopped: signal << 8 | 0x7f (Linux encoding)
                            (target_code << 8) | 0x7f
                        }
                        _ => {
                            // Continued: 0xFFFF (Linux encoding for WIFCONTINUED)
                            0xFFFF
                        }
                    };
                    unsafe {
                        *status_ptr = encoded;
                    }
                }
                return target_pid;
            }
            // Lost the race: rescan.
            continue;
        }

        if (options & 1) != 0 {
            // WNOHANG
            return 0;
        }

        // Lost-wakeup fix: child may have exited between the check above and
        // the block. Exit sets parent pending SIGCHLD before waking. If pending
        // is set, loop again without blocking to reap the zombie.
        let pending = crate::tasks::current_thread().map(|t| t.process.lock().pending & (1u64 << 16) != 0).unwrap_or(false);
        if pending {
            continue;
        }

        crate::tasks::block_current_thread(crate::tasks::PendingBlock::ChildWait);
    }
}


pub(super) fn sys_execve(arg1: u64, arg2: u64, arg3: u64) -> u64 {
    let path = match read_user_cstring(arg1) {
        Some(p) => p,
        None => return EFAULT,
    };
    crate::print!("[execve] starting path={}\n", path);
    let argv = if arg2 == 0 {
        Vec::new()
    } else {
        match read_user_string_array(arg2) {
            Some(a) => a,
            None => return EFAULT,
        }
    };
    let envp = if arg3 == 0 {
        Vec::new()
    } else {
        match read_user_string_array(arg3) {
            Some(a) => a,
            None => return EFAULT,
        }
    };

    let args_ref: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
    let envs_ref: Vec<&str> = envp.iter().map(|s| s.as_str()).collect();

    // Reset the VMA list BEFORE mapping, so the new image's mappings (including
    // the interpreter's and the user stack's) are recorded into a clean list
    // rather than appended to the previous image's.
    //
    // This must happen before `load_image`, not after. An earlier version of
    // this refactor reset afterwards, which discarded the stack VMA that
    // `load_elf` had just inserted — so the first user instruction faulted on
    // an unmapped stack top (`mem[0x7fffffffa7a0] = <unmapped>`) and every
    // execve'd program died.
    if let Some(t) = crate::tasks::current_thread() {
        t.process.lock().vmas = VmaList::default();
    }

    // Shared with the boot path (`exec::load_image`): lookup, read, resolve
    // PT_INTERP, create the address space, install it, map the image. The two
    // paths used to carry independent copies of this sequence, which had
    // already drifted -- notably the boot path synthesised an environment the
    // syscall path never saw. Linux's rule applies here and only here: a
    // program's environment comes from its parent, so `envp` is passed through
    // unchanged and no variables are invented.
    let (image, _file, _interp) = match crate::exec::load_image(
        &path,
        &args_ref,
        &envs_ref,
        |_| {},
    ) {
        Ok(v) => v,
        Err((err, _)) => {
            // `load_image` restored the old CR3 itself.
            crate::print!("[execve] load_image failed: {}\n", err.describe());
            return err.errno();
        }
    };
    // setuid/setgid bits: if the executable carries them and the caller could
    // execute it (the execute check in load_image already passed), the new
    // process takes the file's owner and/or group, exactly as on Linux. The
    // capability set is recomputed afterwards, so a setuid-root binary run by
    // an unprivileged process regains the full capability set, and a process
    // that drops away from uid 0 loses it.
    if let Some(vnode) = crate::fs::lookup(&path) {
        if let crate::fs::vfs::VNode::FsNode { node, .. } = &vnode {
            let perms = crate::fs::perm::perms_of(&**node);
            let thread = crate::tasks::current_thread().expect("sys_execve: no current thread");
            let mut p = thread.process.lock();
            if perms.mode & 0o4000 != 0 {
                p.uid = perms.uid;
                p.euid = perms.uid;
                p.suid = perms.uid;
            }
            if perms.mode & 0o2000 != 0 {
                p.gid = perms.gid;
                p.egid = perms.gid;
                p.sgid = perms.gid;
            }
            recompute_caps(&mut *p);
        }
    }

    let new_pml4 = image.new_pml4;
    let entry_point = image.entry_point;
    let stack_top = image.stack_top;
    let main_bin_end = image.main_bin_end;

    let (frame, new_kernel_stack_top, new_syscall_stack_top) =
        build_child_kernel_stack(stack_top, entry_point, 0x202, 0, 0, 0, 0, 0, 0, 0);

    let old_cr3_to_destroy;
    let mut t = crate::tasks::current_thread().expect("sys_exec: no current thread");
    // Sibling threads must be gone before the old address space is freed.
    //
    // Every thread of a process shares `p.cr3`, so `destroy_address_space`
    // below frees the page tables out from under any sibling that is still
    // running, or that gets dispatched later onto a queue it was never removed
    // from. Linux orders this the other way round for the same reason:
    // `de_thread()` kills every sibling and *waits for them to finish
    // `exit_mm`* before `begin_new_exec` proceeds. A sibling that is merely
    // flagged, rather than reaped, is still a sibling.
    //
    // Symptom when this is missing, and it is silent: `qemu.rs` passes
    // `-no-reboot`, so the resulting triple fault resets the machine and QEMU
    // exits with no message at all (docs/virgl-vs-linux.md 37). It looks like
    // the guest hung, or like a rendering fault, because no panic ever prints.
    let sibling_ids: alloc::collections::BTreeSet<u64> = {
        let mut old_p = t.process.lock();
        let me = t.id;
        let ids: alloc::collections::BTreeSet<u64> = old_p
            .threads
            .keys()
            .copied()
            .filter(|id| *id != me)
            .collect();
        // Remove them from the process's thread list too, so nothing later
        // treats them as live members of a process whose mm is about to go.
        for id in &ids {
            old_p.threads.remove(id);
        }
        if !ids.is_empty() {
            crate::print!("[execve] reaping {} sibling thread(s) first\n", ids.len());
        }
        ids
    };
    if !sibling_ids.is_empty() {
        crate::tasks::purge_threads_from_queues(&sibling_ids);
    }
    {
        let mut old_p = t.process.lock();
        old_cr3_to_destroy = old_p.cr3;
        old_p.cr3 = new_pml4;
        old_p.heap_brk = VirtAddr(main_bin_end);
        old_p.mmap_base = VirtAddr(0x0000_7000_0000_0000);
        old_p.exe_path = crate::fs::canonicalize_path(&old_p.cwd, &path);
        // Preserve open file descriptors across execve, ensuring fd 0, 1, 2 default to TtyDevice
        let tty_vnode = || Arc::new(SpinLock::new(crate::fs::vfs::OpenFile {
            vnode: crate::fs::vfs::VNode::TtyDevice { vt: 0 },
            flags: 2,
            path: alloc::string::String::from("/dev/tty"),
        }));
        if old_p.fd_table.len() < 3 {
            old_p.fd_table.resize(3, None);
        }
        if old_p.fd_table[0].is_none() { old_p.fd_table[0] = Some(tty_vnode()); }
        if old_p.fd_table[1].is_none() { old_p.fd_table[1] = Some(tty_vnode()); }
        if old_p.fd_table[2].is_none() { old_p.fd_table[2] = Some(tty_vnode()); }
        old_p.sigactions = [SigAction::default(); 32];
        old_p.pending = 0;
        old_p.sigqueue.clear();
    }
    t.fs_base = 0;
    t.pending = 0;
    t.sigqueue.clear();
    t.stack_ptr = frame;
    t.kernel_stack_top = new_kernel_stack_top;
    t.syscall_stack_top = new_syscall_stack_top;
    crate::tasks::set_current_thread(Some(t));

    crate::print!("[execve] destroying old cr3={:?}...\n", old_cr3_to_destroy);
    if old_cr3_to_destroy != shared::memory::address::PhysAddr(0) {
        crate::memory::space::destroy_address_space(old_cr3_to_destroy);
    }
    crate::print!("[execve] destroyed old cr3, jumping to user\n");

    with_cpu_tss(|tss| set_rsp0(tss, new_kernel_stack_top));
    crate::smp::with_cpu_state(|state| state.kernel_stack = new_syscall_stack_top);
    unsafe { switch_to_stack_and_iretq(frame) }
}

pub(crate) fn sys_kill(pid: i64, sig: i32) -> u64 {
    if sig < 1 || sig > 31 {
        return EINVAL;
    }
    if pid <= 0 {
        return 0;
    }
    let my_pid = with_current_process(|pp| pp.pid);
    let table = PROCESS_TABLE.lock();
    let mut resumed = false;
    // Save ppid before dropping p, for waking parent's waitpid(WCONTINUED)
    // when SIGCONT resumes a stopped process, and waitpid(WUNTRACED) when a
    // job control stop signal (SIGTSTP/SIGSTOP/SIGTTIN/SIGTTOU) is sent.
    let wake_parent_ppid = if let Some(proc) = table.get(&(pid as u64)) {
        let p = proc.lock();
        if p.status == ProcessStatus::Stopped && sig == crate::arch::x86_64::abi::SIGCONT {
            Some(p.ppid)
        } else if sig == crate::arch::x86_64::abi::SIGTSTP
            || sig == crate::arch::x86_64::abi::SIGSTOP
            || sig == crate::arch::x86_64::abi::SIGTTIN
            || sig == crate::arch::x86_64::abi::SIGTTOU
        {
            Some(p.ppid)
        } else {
            None
        }
    } else {
        None
    };
    if let Some(proc) = table.get(&(pid as u64)) {
        let mut p = proc.lock();
        if p.status == ProcessStatus::Zombie {
            return 0;
        }
        // ptrace: a traced process stops on any signal except SIGKILL /
        // SIGCONT instead of receiving it, so its tracer can inspect it. The
        // signal is *queued* as well: the tracee's next delivery boundary
        // consumes it via `ptrace_trap` and halts the tracee. The sender
        // wakes the tracer's waitpid(WUNTRACED) now. A tracee that is
        // already ptrace-stopped falls through and queues the signal, to be
        // delivered on PTRACE_CONT.
        if p.tracer_pid != 0
            && sig != crate::arch::x86_64::abi::SIGKILL
            && sig != crate::arch::x86_64::abi::SIGCONT
            && p.status != ProcessStatus::Stopped
        {
            p.status = ProcessStatus::Stopped;
            p.stopped.store(true, core::sync::atomic::Ordering::Release);
            p.stop_signal = sig;
            p.ptrace_sig = sig;
            p.pending |= 1u64 << (sig - 1);
            p.sigqueue.push_back(crate::tasks::SigInfo {
                signo: sig,
                code: crate::tasks::SI_USER,
                pid: my_pid,
                value: 0,
            });
            p.ptrace_thread = Some(crate::tasks::current_thread().expect("sys_kill ptrace thread"));
            let tracer = p.tracer_pid;
            let ppid = p.ppid;
            drop(p);
            drop(table);
            crate::tasks::wake_parked_for_pid(tracer);
            if ppid != 0 {
                crate::tasks::wake_parked_for_pid(ppid);
            }
            return 0;
        }
        // Handle stopped processes specially: SIGCONT resumes them immediately
        if p.status == ProcessStatus::Stopped && sig == crate::arch::x86_64::abi::SIGCONT {
            p.status = ProcessStatus::Running;
            p.stop_signal = 0;
            p.continued = true; // Mark as continued for waitpid(WCONTINUED)
            // The run-queue entry is already there (a stopped thread keeps its
            // place in SCHEDULER; `pick_next` skips it while the flag is set).
            // Clearing the flag is therefore the entire resume -- without it
            // the scheduler would keep skipping the thread and SIGCONT would
            // silently do nothing.
            p.stopped.store(false, core::sync::atomic::Ordering::Release);
            resumed = true;
        } else if p.status == ProcessStatus::Stopped && sig != crate::arch::x86_64::abi::SIGCONT {
            // A fatal signal aimed at a *stopped* process must kill it now.
            //
            // Queuing it instead leaves the process stopped and asleep: the
            // only thing that could dispatch the thread to deliver the signal
            // is the very resume we are not doing, so the parent waits in
            // waitpid() forever. Linux tears a stopped process down
            // immediately for any signal that would terminate it.
            p.pending |= 1u64 << (sig - 1);
            p.sigqueue.push_back(crate::tasks::SigInfo { signo: sig, code: crate::tasks::SI_USER, pid: my_pid, value: 0 });
            // Mark it runnable so the scheduler reaches the fatal-signal check
            // and exits the group.
            p.status = ProcessStatus::Running;
            p.stopped.store(false, core::sync::atomic::Ordering::Release);
            resumed = true;
        } else {
            p.pending |= 1u64 << (sig - 1);
            p.sigqueue.push_back(crate::tasks::SigInfo { signo: sig, code: crate::tasks::SI_USER, pid: my_pid, value: 0 });
        }
        drop(p);
        drop(table);
        // Wake any thread of this process that is blocked in nanosleep, so
        // it resumes promptly and delivers the signal.  Lock order respected:
        // PROCESS_TABLE is fully released before we take SCHEDULER here.
        crate::tasks::wake_sleep_waiters_for_pid(pid as u64);
        // Wake every *blocked* thread of this process, not just sleepers.
        //
        // A thread parked in a blocking read (a pty read waiting for input is
        // the common case) never returns to user mode on its own, so a fatal
        // signal queued for it is never delivered and the process cannot die.
        // The parent then blocks in waitpid() forever — the classic "child
        // ignores SIGTERM" hang, caused here by parking rather than by the
        // signal. Waking it lets the scheduler reach the fatal-signal check.
        crate::tasks::wake_blocked_for_pid(pid as u64);
        // Wake the thread itself, not just an idle CPU.
        //
        // A thread stopped by job control (SIGTTIN/SIGTTOU on terminal I/O)
        // marked itself stopped and then slept inside its read. Clearing the
        // stopped flag is not enough: the thread is in a wait queue, and the
        // only thing that could dispatch it — and so deliver this signal — is
        // a resume that never happens on its own. Sweeping the wait queues is
        // what makes both the resume and a queued fatal signal take effect.
        //
        // Done for every signal, not just SIGCONT: `kill(child, SIGTERM)` on
        // a thread asleep in a tty read has to reach it too.
        //
        // Takes SCHEDULER, so it must come after PROCESS_TABLE is released
        // (invariant 1). `enqueue_with_boost` also bumps RUNNABLE_COUNT,
        // releasing a CPU parked in the idle loop.
        let _ = resumed;
        crate::tasks::wake_parked_for_pid(pid as u64);
        // Wake the parent's waitpid(WCONTINUED) if SIGCONT resumed a stopped
        // process. The parent is parked in CHILD_WAIT_QUEUE and nothing else
        // will wake it when the child is continued but doesn't exit.
        if let Some(ppid) = wake_parent_ppid {
            crate::tasks::wake_parked_for_pid(ppid);
        }
        // Threads parked by terminal job control re-test their foreground
        // condition on wake, so they need a nudge even when the sweep above
        // found nothing (a thread that parks after the sweep ran).
        crate::tasks::wake_jobcontrol_waiters();
        // ponytail: self-directed fatal signals (glibc abort() pattern) must
        // die before this syscall returns — the tick path can miss the window.
        if (pid as u64) == my_pid {
            crate::tasks::check_fatal_signal(sig);
        }
        return 0;
    }
    ESRCH
}

/// Queue `sig` to `proc` from `from`, applying the ptrace-stop rule first.
///
/// A traced process that is not already stopped is made to stop on any signal
/// except SIGKILL / SIGCONT (which must always kill / resume): the signal is
/// queued and the process marked stopped, and the tracer's (and, if
/// different, the parent's) waitpid(WUNTRACED) is woken. Returns true if a
/// ptrace-stop was taken. No locks may be held by the caller.
fn queue_signal_ptrace(
    proc: &Arc<SpinLock<Process>>,
    sig: i32,
    from: u64,
    code: i32,
    value: i64,
) -> bool {
    let mut p = proc.lock();
    let tracer = p.tracer_pid;
    if tracer != 0
        && sig != crate::arch::x86_64::abi::SIGKILL
        && sig != crate::arch::x86_64::abi::SIGCONT
        && p.status != ProcessStatus::Stopped
    {
        p.status = ProcessStatus::Stopped;
        p.stopped.store(true, core::sync::atomic::Ordering::Release);
        p.stop_signal = sig;
        p.ptrace_sig = sig;
        p.pending |= 1u64 << (sig - 1);
        p.sigqueue.push_back(crate::tasks::SigInfo {
            signo: sig,
            code,
            pid: from,
            value,
        });
        let ppid = p.ppid;
        drop(p);
        crate::tasks::wake_parked_for_pid(tracer);
        if ppid != 0 && ppid != tracer {
            crate::tasks::wake_parked_for_pid(ppid);
        }
        return true;
    }
    p.pending |= 1u64 << (sig - 1);
    p.sigqueue.push_back(crate::tasks::SigInfo {
        signo: sig,
        code,
        pid: from,
        value,
    });
    false
}

pub(super) fn sys_sigqueue(pid: i64, sig: i32, value: i64) -> u64 {
    if sig < 1 || sig > 31 {
        return EINVAL;
    }
    if pid <= 0 {
        return EINVAL;
    }
    let my_pid = with_current_process(|pp| pp.pid);
    let proc = {
        let table = PROCESS_TABLE.lock();
        match table.get(&(pid as u64)) {
            Some(p) => p.clone(),
            None => return ESRCH,
        }
    };
    if proc.lock().status == ProcessStatus::Zombie {
        return 0;
    }
    queue_signal_ptrace(&proc, sig, my_pid, crate::tasks::SI_QUEUE, value);
    if (pid as u64) == my_pid {
        crate::tasks::check_fatal_signal(sig);
    }
    0
}

pub(super) fn sys_rt_sigqueueinfo(pid: i64, sig: i32, uinfo: u64) -> u64 {
    if sig < 1 || sig > 31 {
        return EINVAL;
    }
    if pid <= 0 {
        return EINVAL;
    }
    // Linux rt_sigqueueinfo(tgid,sig, siginfo_t *uinfo) — uinfo carries si_value at 24 (sival_int) / 32 (sival_ptr)
    let value: i64 = if uinfo != 0 && validate_user_read_range(uinfo, 128) {
        unsafe { core::ptr::read_unaligned((uinfo + 24) as *const i32) as i64 }
    } else {
        0
    };
    let my_pid = with_current_process(|pp| pp.pid);
    let proc = {
        let table = PROCESS_TABLE.lock();
        match table.get(&(pid as u64)) {
            Some(p) => p.clone(),
            None => return ESRCH,
        }
    };
    if proc.lock().status == ProcessStatus::Zombie {
        return 0;
    }
    queue_signal_ptrace(&proc, sig, my_pid, crate::tasks::SI_QUEUE, value);
    if (pid as u64) == my_pid {
        crate::tasks::check_fatal_signal(sig);
    }
    0
}

pub(super) fn sys_uname(buf: *mut u8) -> u64 {
    if !validate_user_write_range(buf as u64, 390) {
        return EFAULT;
    }
    unsafe {
        core::ptr::write_bytes(buf, 0, 390);
        core::ptr::copy_nonoverlapping(b"DragoOS\0".as_ptr(), buf.add(0), 8);
        core::ptr::copy_nonoverlapping(b"localhost\0".as_ptr(), buf.add(65), 10);
        core::ptr::copy_nonoverlapping(b"1.0.0\0".as_ptr(), buf.add(130), 6);
        core::ptr::copy_nonoverlapping(b"1.0\0".as_ptr(), buf.add(195), 4);
        core::ptr::copy_nonoverlapping(b"x86_64\0".as_ptr(), buf.add(260), 7);
    }
    0
}

pub(super) fn sys_arch_prctl(code: u64, addr: u64) -> u64 {
    const ARCH_SET_FS: u64 = 0x1002;
    if code == ARCH_SET_FS {
        unsafe {
            wrmsr(IA32_FS_BASE, addr);
        }
        if let Some(mut t) = crate::tasks::current_thread() {
            t.fs_base = addr;
            crate::tasks::set_current_thread(Some(t));
        }
        return 0;
    }
    EINVAL
}

pub(super) fn sys_reboot(magic1: usize, magic2: usize, cmd: usize, _arg: usize) -> u64 {
    const LINUX_REBOOT_CMD_RESTART: usize = 0x01234567;
    const LINUX_REBOOT_CMD_HALT: usize = 0xCDEF0123;
    const LINUX_REBOOT_CMD_CAD_ON: usize = 0x89ABCDEF;
    const LINUX_REBOOT_CMD_CAD_OFF: usize = 0x00000000;
    const LINUX_REBOOT_CMD_POWER_OFF: usize = 0x4321FEDC;
    const LINUX_REBOOT_CMD_RESTART2: usize = 0xA1B2C3D4;

    crate::tprintln!("sys_reboot: magic1={:#x}, magic2={:#x}, cmd={:#x}", magic1, magic2, cmd);

    let is_root = with_current_process(|p| p.euid == 0);
    if !is_root {
        return EPERM;
    }

    match cmd {
        LINUX_REBOOT_CMD_POWER_OFF | LINUX_REBOOT_CMD_HALT => {
            crate::arch::x86_64::acpi::poweroff();
        }
        LINUX_REBOOT_CMD_RESTART | LINUX_REBOOT_CMD_RESTART2 => {
            crate::arch::x86_64::acpi::reboot();
        }
        LINUX_REBOOT_CMD_CAD_ON | LINUX_REBOOT_CMD_CAD_OFF => 0,
        _ => EINVAL,
    }
}

pub(super) fn sys_set_tid_address(tidptr: u64) -> u64 {
    let cpu_id = crate::smp::current_cpu_id();
    let mut cpus = crate::smp::CPUS[cpu_id as usize].lock();
    if let Some(t) = cpus.as_mut().and_then(|c| c.current.as_mut()) {
        t.clear_child_tid = Some(tidptr);
        t.id
    } else {
        1
    }
}

pub(super) fn sys_rt_tgsigqueueinfo(tgid: i64, tid: i64, sig: i32, uinfo: u64) -> u64 {
    if sig < 1 || sig > 31 {
        return EINVAL;
    }
    if tgid <= 0 || tid <= 0 {
        return EINVAL;
    }
    // Linux rt_tgsigqueueinfo(tgid, tid, sig, siginfo_t *uinfo)
    let value: i64 = if uinfo != 0 && validate_user_read_range(uinfo, 128) {
        unsafe { core::ptr::read_unaligned((uinfo + 24) as *const i32) as i64 }
    } else {
        0
    };
    let my_pid = with_current_process(|pp| pp.pid);
    let table = PROCESS_TABLE.lock();
    // Find the target thread (by tid). In our model, threads are processes
    // sharing a thread group; tid is the thread's pid.
    if let Some(proc) = table.get(&(tid as u64)) {
        let mut p = proc.lock();
        if p.status == ProcessStatus::Zombie {
            drop(p);
            drop(table);
            return 0;
        }
        // In our model, threads are processes; accept any valid tid.
        p.pending |= 1u64 << (sig - 1);
        p.sigqueue.push_back(crate::tasks::SigInfo { signo: sig, code: crate::tasks::SI_QUEUE, pid: my_pid, value });
        drop(p);
        drop(table);
        if (tid as u64) == my_pid {
            crate::tasks::check_fatal_signal(sig);
        }
        return 0;
    }
    ESRCH
}

pub(super) fn sys_tgkill(tgid: i64, _tid: i64, sig: i32) -> u64 {
    let my_pid = with_current_process(|pp| pp.pid);
    if sig < 1 || sig > 31 {
        return EINVAL;
    }
    let proc = {
        let table = PROCESS_TABLE.lock();
        match table.get(&(tgid as u64)) {
            Some(p) => p.clone(),
            None => return ESRCH,
        }
    };
    queue_signal_ptrace(&proc, sig, my_pid, crate::tasks::SI_USER, 0);
    // ponytail: self-directed fatal signals (glibc abort() pattern) must
    // die before this syscall returns — the tick path can miss the window
    // entirely, letting execution fall through to abort's hlt trap.
    if (tgid as u64) == my_pid {
        crate::tasks::check_fatal_signal(sig);
    }
    0
}

pub(super) fn sys_gettid() -> u64 {
    let thread = crate::tasks::current_thread().expect("No current thread for gettid");
    thread.id
}

/// `setpgid(2)`.
///
/// This was a stub returning 0, so `setpgid` always "succeeded" while changing
/// nothing. A shell could put a job in the background, `tcsetpgrp` in the child,
/// and the foreground-group concept had no state behind it.
pub(super) fn sys_setpgid(pid: i64, pgid: i64) -> u64 {
    // Linux lets a process move only itself or one of its own children.
    let me = with_current_process(|p| p.pid);
    let target_pid = if pid <= 0 { me } else { pid as u64 };
    if target_pid != me && target_pid != 0 {
        // Moving a child requires it to be our child and not yet exec'd; the
        // "not yet exec'd" rule is not tracked, so the parent check is what we
        // can honour. A cross-tree move is refused rather than silently applied.
        let mine = {
            let t = crate::tasks::PROCESS_TABLE.lock();
            match t.get(&target_pid) {
                Some(p) => p.lock().ppid == me,
                None => false,
            }
        };
        if !mine {
            return ESRCH;
        }
    }
    if pgid < 0 {
        return EINVAL;
    }
    // pgid 0 means "the target's own pid".
    let new_pgid = if pgid == 0 { target_pid } else { pgid as u64 };

    let table = PROCESS_TABLE.lock();
    let Some(proc) = table.get(&target_pid) else {
        return ESRCH;
    };
    let mut p = proc.lock();
    if p.status == ProcessStatus::Zombie {
        return ESRCH;
    }
    p.pgid = new_pgid;
    0
}

/// `getpgid(2)` (x86-64 172) — was not dispatched at all.
pub(super) fn sys_getpgid(pid: i64) -> u64 {
    let me = with_current_process(|p| p.pid);
    let target = if pid <= 0 { me } else { pid as u64 };
    if target == me {
        return with_current_process(|p| p.pgid);
    }
    let table = PROCESS_TABLE.lock();
    match table.get(&target) {
        Some(proc) => {
            let p = proc.lock();
            if p.status == ProcessStatus::Zombie { ESRCH } else { p.pgid }
        }
        None => ESRCH,
    }
}

/// `getpgrp(2)` (x86-64 173) — getpgid(0), the call a shell makes to find out
/// which group currently owns the terminal.
pub(super) fn sys_getpgrp() -> u64 {
    sys_getpgid(0)
}

pub(super) fn sys_setsid() -> u64 {
    // POSIX: fail with EPERM if the caller is already a process group leader.
    // Without that check a caller could create a second session while keeping
    // its old group, and a child could then never detach cleanly.
    let _me = with_current_process(|p| p.pid);
    let is_leader = with_current_process(|p| p.pgid == p.pid);
    if is_leader {
        return crate::arch::x86_64::syscalls::EPERM;
    }
    with_current_process(|p| {
        p.sid = p.pid;
        p.pgid = p.pid;
        // A new session has no controlling terminal until TIOCSCTTY.
        p.ctty_vt = 0;
        p.ctty_owner_sid = 0;
        p.pid
    })
}

/// `getsid(2)` (x86-64 124) — the session leader's pid.
///
/// This returned `p.pid`, not `p.sid`. For a session *leader* those are the
/// same number, so every caller that was its own leader got the right answer by
/// accident and the bug was invisible. Any other member of a session got its own
/// pid back, which is wrong for every process but one per session.
///
/// `sid` exists on `Process` precisely so this can be answered; it was simply
/// never read. Caught by initrd/jobctltest.c, which forks a session member and
/// asserts `getsid(0) == getppid()` — the leader's pid.
pub(super) fn sys_getsid(pid: i64) -> u64 {
    if pid == 0 {
        with_current_process(|p| p.sid)
    } else {
        let table = PROCESS_TABLE.lock();
        if let Some(proc) = table.get(&(pid as u64)) {
            let p = proc.lock();
            if p.status == ProcessStatus::Zombie {
                return ESRCH;
            }
            p.sid
        } else {
            ESRCH
        }
    }
}

pub(super) fn sys_getpriority(_which: i32, _who: i32) -> u64 {
    match crate::tasks::current_thread() {
        Some(t) => {
            let p = t.process.lock();
            (20 - p.nice) as u64
        }
        None => ESRCH,
    }
}

pub(super) fn sys_setpriority(_which: i32, _who: i32, prio: i32) -> u64 {
    match crate::tasks::current_thread() {
        Some(mut t) => {
            let new_nice = {
                let mut p = t.process.lock();
                let new_nice = prio.clamp(-20, 19);
                p.nice = new_nice;
                new_nice
            };
            t.nice = new_nice;
            crate::tasks::set_current_thread(Some(t));
            0
        }
        None => ESRCH,
    }
}

/// `times(buf)`: fills `struct tms` and returns clock ticks since boot.
#[repr(C)]
struct Tms {
    tms_utime: i64,
    tms_stime: i64,
    tms_cutime: i64,
    tms_cstime: i64,
}

pub(super) fn sys_times(buf_ptr: u64) -> u64 {
    if !validate_user_write_range(buf_ptr, core::mem::size_of::<Tms>()) {
        return EFAULT;
    }
    let (utime, stime) = with_current_process(|p| (p.utime, p.stime));
    let tms = Tms {
        tms_utime: utime as i64,
        tms_stime: stime as i64,
        tms_cutime: 0,
        tms_cstime: 0,
    };
    unsafe {
        core::ptr::write(buf_ptr as *mut Tms, tms);
    }
    crate::time::TICKS.load(core::sync::atomic::Ordering::Relaxed) as u64
}

/// `getrusage(who, usage)`: fills `struct rusage`. Only RUSAGE_SELF (0) is
/// meaningfully supported; children times are not tracked.
#[repr(C)]
struct RUsage {
    ru_utime: [u64; 2], // timeval { sec, usec }
    ru_stime: [u64; 2],
    ru_maxrss: u64,
    ru_ixrss: u64,
    ru_idrss: u64,
    ru_isrss: u64,
    ru_minflt: u64,
    ru_majflt: u64,
    ru_nswap: u64,
    ru_inblock: u64,
    ru_oublock: u64,
    ru_msgsnd: u64,
    ru_msgrcv: u64,
    ru_nsignals: u64,
    ru_nvcsw: u64,
    ru_nivcsw: u64,
}

pub(super) fn sys_getrusage(who: i32, usage_ptr: u64) -> u64 {
    if who != 0 && who != -1 {
        return EINVAL;
    }
    if !validate_user_write_range(usage_ptr, core::mem::size_of::<RUsage>()) {
        return EFAULT;
    }
    let (utime, stime) = with_current_process(|p| (p.utime, p.stime));
    let r = RUsage {
        ru_utime: [utime / 1000, (utime % 1000) * 1000],
        ru_stime: [stime / 1000, (stime % 1000) * 1000],
        ru_maxrss: 0,
        ru_ixrss: 0,
        ru_idrss: 0,
        ru_isrss: 0,
        ru_minflt: 0,
        ru_majflt: 0,
        ru_nswap: 0,
        ru_inblock: 0,
        ru_oublock: 0,
        ru_msgsnd: 0,
        ru_msgrcv: 0,
        ru_nsignals: 0,
        ru_nvcsw: 0,
        ru_nivcsw: 0,
    };
    unsafe {
        core::ptr::write(usage_ptr as *mut RUsage, r);
    }
    0
}

pub(super) fn sys_prlimit64(
    pid: i64,
    resource: usize,
    new_rlim_ptr: *const crate::tasks::RLimit,
    old_rlim_ptr: *mut crate::tasks::RLimit,
) -> u64 {
    if resource >= crate::tasks::RLIM_NLIMITS {
        return EINVAL;
    }
    let target_arc = if pid == 0 {
        let t = match crate::tasks::current_thread() {
            Some(t) => t,
            None => return ESRCH,
        };
        t.process.clone()
    } else {
        let table = PROCESS_TABLE.lock();
        match table.get(&(pid as u64)) {
            Some(p) => p.clone(),
            None => return ESRCH,
        }
    };

    if !old_rlim_ptr.is_null() {
        if !validate_user_write_range(
            old_rlim_ptr as u64,
            core::mem::size_of::<crate::tasks::RLimit>(),
        ) {
            return EFAULT;
        }
        let cur = target_arc.lock().rlimits[resource];
        unsafe {
            core::ptr::write(old_rlim_ptr, cur);
        }
    }

    if !new_rlim_ptr.is_null() {
        if !validate_user_read_range(
            new_rlim_ptr as u64,
            core::mem::size_of::<crate::tasks::RLimit>(),
        ) {
            return EFAULT;
        }
        let new_rlim = unsafe { *new_rlim_ptr };
        let mut p = target_arc.lock();
        p.rlimits[resource] = new_rlim;
        if resource == crate::tasks::RLIMIT_STACK {
            if let Some(vma) = p
                .vmas
                .vmas
                .iter_mut()
                .find(|v| v.kind == crate::memory::vma::VmaKind::Stack)
            {
                let stack_top = vma.end;
                vma.start = stack_top.saturating_sub(new_rlim.rlim_cur);
            }
        }
    }

    0
}

pub(super) fn sys_getrlimit(resource: usize, rlim_ptr: *mut crate::tasks::RLimit) -> u64 {
    sys_prlimit64(0, resource, core::ptr::null(), rlim_ptr)
}

pub(super) fn sys_setrlimit(resource: usize, rlim_ptr: *const crate::tasks::RLimit) -> u64 {
    sys_prlimit64(0, resource, rlim_ptr, core::ptr::null_mut())
}
