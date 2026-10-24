use super::*;

pub(super) fn sys_rt_sigaction(sig: i32, act: *const SigAction, oldact: *mut SigAction) -> u64 {
    if sig < 1 || sig > 31 || sig == 9 || sig == 19 {
        return EINVAL;
    }
    let t = crate::tasks::current_thread().expect("sys_rt_sigaction: no current thread");
    let result = {
        let mut p = t.process.lock();
        if oldact as u64 != 0 {
            if !validate_user_write_range(oldact as u64, core::mem::size_of::<SigAction>()) {
                EFAULT
            } else {
                unsafe {
                    *oldact = p.sigactions[(sig - 1) as usize];
                }
                if act as u64 != 0 {
                    if !validate_user_read_range(act as u64, core::mem::size_of::<SigAction>()) {
                        EFAULT
                    } else {
                        let sa = unsafe { &*act };
                        p.sigactions[(sig - 1) as usize] = *sa;
                        0
                    }
                } else {
                    0
                }
            }
        } else if act as u64 != 0 {
            if !validate_user_read_range(act as u64, core::mem::size_of::<SigAction>()) {
                EFAULT
            } else {
                let sa = unsafe { &*act };
                p.sigactions[(sig - 1) as usize] = *sa;
                0
            }
        } else {
            0
        }
    };
    crate::tasks::set_current_thread(Some(t));
    result
}

pub(super) fn sys_rt_sigreturn() -> ! {
    let rsp = crate::smp::with_cpu_state(|state| state.user_stack);

    let sf_opt1 = rsp.wrapping_sub(8);
    let sf_opt2 = rsp;

    let user_cs = crate::arch::x86_64::gdt::USER_CODE_SELECTOR as u64;

    let sf = if validate_user_read_range(sf_opt1, core::mem::size_of::<RtSigFrame>())
        && (unsafe { (*(sf_opt1 as *const RtSigFrame)).uc.uc_mcontext.cs } as u64) == user_cs
    {
        sf_opt1 as *const RtSigFrame
    } else if validate_user_read_range(sf_opt2, core::mem::size_of::<RtSigFrame>())
        && (unsafe { (*(sf_opt2 as *const RtSigFrame)).uc.uc_mcontext.cs } as u64) == user_cs
    {
        sf_opt2 as *const RtSigFrame
    } else if validate_user_read_range(sf_opt1, core::mem::size_of::<RtSigFrame>()) {
        sf_opt1 as *const RtSigFrame
    } else {
        sf_opt2 as *const RtSigFrame
    };

    if !validate_user_read_range(sf as u64, core::mem::size_of::<RtSigFrame>()) {
        crate::println!("sys_rt_sigreturn: invalid sigframe pointer {:#x}", sf as u64);
        crate::tasks::exit_current_thread(128 + 11);
    }

    let mc = unsafe { &(*sf).uc.uc_mcontext };
    let mask = unsafe { (*sf).uc.uc_sigmask };

    if mc.rip == 0 {
        crate::println!("sys_rt_sigreturn: attempted to return to RIP = 0! Exiting thread cleanly.");
        crate::tasks::exit_current_thread(128 + 11);
    }



    // Restore the pre-handler FPU state saved at delivery time (per-thread
    // buffer — no CPUS lock involved).
    {
        let t = crate::tasks::current_thread().expect("rt_sigreturn: no thread");
        crate::tasks::fpu_restore_from(core::ptr::addr_of!(t.uc_fpu.data) as u64);
    }

    let frame = {
        let mut t = crate::tasks::current_thread().expect("rt_sigreturn: no thread");
        t.sigmask = mask;
        let stack_top = t.kernel_stack_top;
        crate::tasks::set_current_thread(Some(t));
        stack_top - THREAD_FRAME_SIZE
    };

    let f = frame as *mut u64;
    unsafe {
        *f.add(0) = mc.rax;
        *f.add(1) = mc.rbx;
        *f.add(2) = mc.rcx;
        *f.add(3) = mc.rdx;
        *f.add(4) = mc.rsi;
        *f.add(5) = mc.rdi;
        *f.add(6) = mc.rbp;
        *f.add(7) = mc.r8;
        *f.add(8) = mc.r9;
        *f.add(9) = mc.r10;
        *f.add(10) = mc.r11;
        *f.add(11) = mc.r12;
        *f.add(12) = mc.r13;
        *f.add(13) = mc.r14;
        *f.add(14) = mc.r15;
        *f.add(15) = mc.rip;
        *f.add(16) = user_cs;
        *f.add(17) = mc.eflags;
        *f.add(18) = mc.rsp;
        *f.add(19) = crate::arch::x86_64::gdt::USER_DATA_SELECTOR as u64;
    }
    unsafe { switch_to_stack_and_iretq(frame) }
}

pub(super) fn sys_rt_sigprocmask(how: i32, set: *const u64, oldset: *mut u64, sigsetsize: usize) -> u64 {
    if sigsetsize != 8 {
        return EINVAL;
    }

    let mut t = crate::tasks::current_thread().expect("No current thread for sigprocmask");
    let current_mask = t.sigmask;

    if oldset as u64 != 0 {
        if !validate_user_write_range(oldset as u64, core::mem::size_of::<u64>()) {
            crate::tasks::set_current_thread(Some(t));
            return EFAULT;
        }
        unsafe {
            *oldset = current_mask;
        }
    }

    if set as u64 != 0 {
        if !validate_user_read_range(set as u64, core::mem::size_of::<u64>()) {
            crate::tasks::set_current_thread(Some(t));
            return EFAULT;
        }
        let new_mask = unsafe { *set };
        match how {
            0 => t.sigmask |= new_mask,
            1 => t.sigmask &= !new_mask,
            2 => t.sigmask = new_mask,
            _ => {
                crate::tasks::set_current_thread(Some(t));
                return EINVAL;
            }
        }
    }

    crate::tasks::set_current_thread(Some(t));
    // ponytail: unblocking may release a queued fatal signal (glibc abort()
    // unblocks SIGABRT before its final raise) — die now, not at the hlt trap.
    crate::tasks::check_any_fatal();
    0
}

pub(super) fn sys_sigaltstack(ss_ptr: u64, oss_ptr: u64) -> u64 {
    const SS_DISABLE: i32 = 1;
    const SS_ONSTACK: i32 = 2;

    let mut t = crate::tasks::current_thread().expect("sys_sigaltstack: no current thread");

    // If oss_ptr is non-null, return the current alt stack settings.
    if oss_ptr as u64 != 0 {
        if !validate_user_write_range(oss_ptr as u64, core::mem::size_of::<crate::arch::x86_64::abi::StackT>()) {
            return EFAULT;
        }
        unsafe {
            *(oss_ptr as *mut crate::arch::x86_64::abi::StackT) = t.alt_stack;
        }
    }

    // If ss_ptr is non-null, set the new alt stack.
    if ss_ptr as u64 != 0 {
        if !validate_user_read_range(ss_ptr as u64, core::mem::size_of::<crate::arch::x86_64::abi::StackT>()) {
            return EFAULT;
        }
        let new_ss = unsafe { *(ss_ptr as *const crate::arch::x86_64::abi::StackT) };

        // ss_flags must only contain SS_DISABLE (bits 0), SS_ONSTACK is read-only.
        if new_ss.ss_flags & !(SS_DISABLE) != 0 {
            return EINVAL;
        }

        // If currently on the alternate stack (SS_ONSTACK), cannot replace it.
        if t.alt_stack.ss_flags & SS_ONSTACK != 0 {
            return EPERM;
        }

        // ss_size must be at least MINSIGSTKSZ (2048) if not disabling.
        if new_ss.ss_flags & SS_DISABLE == 0 && new_ss.ss_size < 2048 {
            return ENOMEM;
        }

        t.alt_stack = new_ss;
    }

    crate::tasks::set_current_thread(Some(t));
    0
}
