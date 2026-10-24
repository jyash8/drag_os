unsafe fn push_u64(rsp: &mut u64, val: u64) {
    *rsp -= 8;
    unsafe {
        *(*rsp as *mut u64) = val;
    }
}

unsafe fn push_str(rsp: &mut u64, s: &str) -> u64 {
    *rsp -= (s.len() + 1) as u64;
    let ptr = *rsp as *mut u8;

    unsafe {
        core::ptr::copy_nonoverlapping(s.as_ptr(), ptr, s.len());
        ptr.add(s.len()).write(0);
    }
    *rsp
}

pub unsafe fn prepare_user_stack(
    mut rsp: u64,
    args: &[&str],
    envs: &[&str],
    phdr_vaddr: u64,
    phent: u64,
    phnum: u64,
    entry: u64,
    random_ptr: u64,
    base: u64,
) -> u64 {
    const MAX_PTRS: usize = 32;
    let mut arg_addrs = [0u64; MAX_PTRS];
    let mut env_addrs = [0u64; MAX_PTRS];

    // ponytail: truncate overly-large argv/envp to avoid kernel panics.
    // Userspace should be able to exec even if the caller provides >32 strings.
    let arg_count = args.len().min(MAX_PTRS);
    let env_count = envs.len().min(MAX_PTRS);

    for (i, env) in envs.iter().rev().take(env_count).enumerate() {
        env_addrs[i] = unsafe { push_str(&mut rsp, env) };
    }
    for (i, arg) in args.iter().rev().take(arg_count).enumerate() {
        arg_addrs[i] = unsafe { push_str(&mut rsp, arg) };
    }

    rsp &= !0xF; // 16-byte align

    let aux_pairs = if arg_count == 0 { 14 } else { 15 };
    let items = 1 + (arg_count + 1) + (env_count + 1) + (aux_pairs * 2);
    unsafe {
        if items % 2 != 0 {
            push_u64(&mut rsp, 0);
        }

        // --- AUXILIARY VECTOR (Pushed in reverse order: Value, Type) ---
        push_u64(&mut rsp, 0);
        push_u64(&mut rsp, 0); // AT_NULL
        if arg_count != 0 {
            push_u64(&mut rsp, arg_addrs[0]);
            push_u64(&mut rsp, 31); // AT_EXECFN
        }
        push_u64(&mut rsp, random_ptr);
        push_u64(&mut rsp, 25); // AT_RANDOM
        push_u64(&mut rsp, 0);
        push_u64(&mut rsp, 23); // AT_SECURE
        push_u64(&mut rsp, 100);
        push_u64(&mut rsp, 17); // AT_CLKTCK
        push_u64(&mut rsp, 0xbfebfbff);
        push_u64(&mut rsp, 16); // AT_HWCAP
        push_u64(&mut rsp, 0);
        push_u64(&mut rsp, 14); // AT_EGID
        push_u64(&mut rsp, 0);
        push_u64(&mut rsp, 13); // AT_GID
        push_u64(&mut rsp, 0);
        push_u64(&mut rsp, 12); // AT_EUID
        push_u64(&mut rsp, 0);
        push_u64(&mut rsp, 11); // AT_UID
        push_u64(&mut rsp, entry);
        push_u64(&mut rsp, 9); // AT_ENTRY
        push_u64(&mut rsp, 0);
        push_u64(&mut rsp, 8); // AT_FLAGS
        push_u64(&mut rsp, base);
        push_u64(&mut rsp, 7); // AT_BASE
        push_u64(&mut rsp, 4096);
        push_u64(&mut rsp, 6); // AT_PAGESZ
        push_u64(&mut rsp, phnum);
        push_u64(&mut rsp, 5); // AT_PHNUM
        push_u64(&mut rsp, phent);
        push_u64(&mut rsp, 4); // AT_PHENT
        push_u64(&mut rsp, phdr_vaddr);
        push_u64(&mut rsp, 3); // AT_PHDR

        // --- ENVS ---
        push_u64(&mut rsp, 0);
        for i in 0..env_count {
            push_u64(&mut rsp, env_addrs[i]);
        }

        // --- ARGS ---
        push_u64(&mut rsp, 0);
        for i in 0..arg_count {
            push_u64(&mut rsp, arg_addrs[i]);
        }

        // --- ARGC ---
        push_u64(&mut rsp, arg_count as u64);
    }

    rsp
}
