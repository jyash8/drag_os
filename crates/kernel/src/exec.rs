//! Loading an ELF and handing control to it, and booting the first user process.
//!
//! Two entry points, one core:
//!
//! - [`load_image`] — read a file, resolve its `PT_INTERP`, build an address
//!   space, map the segments, apply relocations, build the user stack. Shared by
//!   `exec_initrd` (the boot path) and `sys_execve` (the syscall).
//! - [`exec_initrd`] / [`exec_initrd_args`] — boot-only wrappers that additionally
//!   create pid 1 and never return, because there is no user frame to return to.
//!
//! # Why the split
//!
//! These used to be two independent implementations of the same sequence —
//! `create_address_space`, `write_cr3`, interpreter resolution via `Elf::parse`,
//! `load_elf` — which had already drifted: the boot path synthesised a fixed
//! environment that the syscall path never saw. Anything that behaved
//! differently at boot than under `execve` was a latent bug in one of the two.
//!
//! What genuinely cannot be shared is the *entry* mechanism. `execve` runs on an
//! existing thread and returns through the syscall dispatcher to a user frame;
//! the boot path has no current thread, so it registers one and calls
//! `execute_and_resume` directly, never returning.
//!
//! # Environment
//!
//! [`boot_environment`] is **boot-only**. Linux gives a program its environment
//! from its parent, and `execve` passes the caller's `envp` straight through —
//! it does not invent variables. The boot path has no parent to inherit from, so
//! it supplies a default set (PATH, TERM, WAYLAND_DISPLAY, the seat variables).
//! That set is deliberately not applied to `execve`: doing so would silently
//! override a program's real environment, and `draginit`'s daemons are started
//! with the environment they inherit from it.

use shared::boot::boot_info::HHDM_BASE;
use shared::memory::address::PhysAddr;

use alloc::vec::Vec;

use crate::{
    arch::x86_64::cpu::{execute_and_resume, read_cr3, write_cr3},
    elf::loader::load_elf,
    memory::{FRAME_ALLOCATOR, space::create_address_space},
    tasks::register_exec_thread,
    tprintln,
};

/// Why a load failed, mapped to an errno by the caller.
///
/// `sys_execve` must return an errno; the boot path only prints. Keeping the
/// reason distinct means the syscall can report `ENOENT` for a missing
/// interpreter the way Linux does, instead of lumping it in with "bad ELF".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadError {
    /// The file named by the path does not exist.
    NoSuchFile,
    /// The path exists but is not a regular file (directory, device).
    NotRegular,
    /// The file or its interpreter could not be read.
    Unreadable,
    /// No address space could be allocated.
    OutOfMemory,
    /// `PT_INTERP` named a loader that is not present.
    MissingInterpreter,
    /// The file's permission bits forbid executing it (the caller lacks `x`).
    PermissionDenied,
    /// The ELF headers or segments were rejected.
    BadElf,
}

impl LoadError {
    /// The errno Linux returns for this failure.
    pub fn errno(self) -> u64 {
        match self {
            LoadError::NoSuchFile => crate::arch::x86_64::syscalls::ENOENT,
            LoadError::NotRegular | LoadError::Unreadable => crate::arch::x86_64::syscalls::ENOEXEC,
            LoadError::OutOfMemory => crate::arch::x86_64::syscalls::ENOMEM,
            LoadError::MissingInterpreter => crate::arch::x86_64::syscalls::ENOENT,
            LoadError::PermissionDenied => crate::arch::x86_64::syscalls::EACCES,
            LoadError::BadElf => crate::arch::x86_64::syscalls::ENOEXEC,
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            LoadError::NoSuchFile => "file not found",
            LoadError::NotRegular => "not a regular file",
            LoadError::Unreadable => "could not read file",
            LoadError::OutOfMemory => "out of memory for address space",
            LoadError::MissingInterpreter => "ELF interpreter not found",
            LoadError::PermissionDenied => "execute permission denied",
            LoadError::BadElf => "failed to load ELF",
        }
    }
}

/// A loaded image, ready to be entered.
pub struct LoadedImage {
    /// User-space entry point.
    pub entry_point: u64,
    /// Top of the user stack that `prepare_user_stack` built.
    pub stack_top: u64,
    /// End of the main binary's loadable segments, used as the initial
    /// `heap_brk`.
    pub main_bin_end: u64,
    /// The new PML4. The caller installs it and restores `old_cr3` on failure.
    pub new_pml4: PhysAddr,
    /// The CR3 that was active on entry, so a failure can put it back.
    pub old_cr3: PhysAddr,
}

/// Read a whole vnode into a buffer.
///
/// Borrowed data (an initrd tar entry) is returned by reference; block-backed
/// and filesystem-backed content is read into `scratch`, which the caller must
/// keep alive for as long as the returned slice is used.
fn read_whole<'a>(
    vnode: &'a crate::fs::vfs::VNode,
    scratch: &'a mut Vec<u8>,
) -> Result<&'a [u8], LoadError> {
    match vnode {
        crate::fs::vfs::VNode::InitrdFile { data, .. } => Ok(data),
        crate::fs::vfs::VNode::BlockFile { start_sector, size, cursor } => {
            scratch.resize(*size, 0);
            let mut bf = crate::fs::vfs::VNode::BlockFile {
                start_sector: *start_sector,
                size: *size,
                cursor: *cursor,
            };
            if bf.read(scratch) == Ok(*size) {
                Ok(scratch.as_slice())
            } else {
                Err(LoadError::Unreadable)
            }
        }
        crate::fs::vfs::VNode::FsNode { node, .. } => {
            let size = node.size();
            scratch.resize(size, 0);
            if node.read_at(0, scratch) == Ok(size) {
                Ok(scratch.as_slice())
            } else {
                Err(LoadError::Unreadable)
            }
        }
        _ => Err(LoadError::NotRegular),
    }
}

/// The shared core of `execve` and the boot path.
///
/// Looks up `path`, reads it, resolves its `PT_INTERP`, creates an address
/// space, installs it, and maps the image. On success the caller must enter
/// `loaded.entry_point` / `loaded.stack_top`; on failure `loaded.old_cr3` has
/// been restored and the caller should report `err.errno()`.
///
/// CR3 is installed here because the segments are mapped into the *current*
/// address space, so it must be live before mapping. The boot path registers
/// pid 1 against the returned `new_pml4` **after** this call, which is safe
/// only because `load_image` also returns the mapping result and the boot path
/// re-installs nothing — see `exec_initrd_args` for why the registration cannot
/// simply move earlier.
pub fn load_image(
    path: &str,
    argv: &[&str],
    envs: &[&str],
    register: impl FnOnce(PhysAddr),
) -> Result<(LoadedImage, Vec<u8>, Vec<u8>), (LoadError, u64)> {
    let vnode = crate::fs::lookup(path).ok_or((LoadError::NoSuchFile, 0))?;
    let file_inode = crate::fs::vfs::vnode_cache_id(&vnode);

    // Only an executable file may be run: the caller needs `x` in its class
    // (root bypasses via CAP_DAC_READ_SEARCH). A directory, a device, or a
    // file without any execute bit is refused with EACCES, as on Linux. The
    // same rule is applied to the interpreter below.
    if let crate::fs::vfs::VNode::FsNode { node, .. } = &vnode {
        if let Some(creds) = crate::fs::perm::current_creds() {
            let perms = crate::fs::perm::perms_of(&**node);
            if !crate::fs::perm::check(&perms, &creds, crate::fs::perm::Access::Execute)
                .is_allow()
            {
                return Err((
                    LoadError::PermissionDenied,
                    crate::arch::x86_64::syscalls::EACCES,
                ));
            }
        }
    }

    // Both scratch buffers are returned to the caller, which must hold them
    // alive: `load_elf` maps segments from these slices, and the mapping reads
    // them during the call only, but keeping them owned here makes the lifetime
    // explicit rather than relying on the borrow checker to notice.
    let mut file_scratch = Vec::new();
    let mut interp_scratch = Vec::new();

    // Read the file and its interpreter BEFORE switching CR3.
    //
    // This ordering is load-bearing and was lost in an earlier version of this
    // refactor, which read them afterwards. `read_whole` dereferences
    // `VNode::FsNode`'s node to call `read_at`, and the VNode was allocated in
    // the *old* address space; after `write_cr3` the kernel is running against a
    // table set that does not yet contain those structures. The resulting fault
    // was observed as a WRONGSTACK report and a hang before the first user
    // instruction -- which looks like a scheduler problem and is not one.
    //
    // Note the register hook below therefore runs after the reads but before
    // any mapping, which is the property invariant 6 actually needs.
    let file_data: &[u8] = match read_whole(&vnode, &mut file_scratch) {
        Ok(d) => d,
        Err(e) => return Err((e, 0)),
    };

    let new_pml4 = {
        let mut allocator_guard = FRAME_ALLOCATOR.lock();
        create_address_space(&mut *allocator_guard, HHDM_BASE.0)
            .map_err(|_| (LoadError::OutOfMemory, 0))?
    };
    let old_cr3 = read_cr3();
    unsafe { write_cr3(new_pml4) };

    // The boot path has no current thread yet, and the segments below are
    // recorded as VMAs against the current process. So hand the new PML4 to the
    // caller, which registers pid 1 with it *now*, before any mapping happens.
    register(new_pml4);

    let interp_path = goblin::elf::Elf::parse(file_data)
        .ok()
        .and_then(|e| e.interpreter);
    let (interp_data, interp_inode): (Option<&[u8]>, Option<u64>) = match interp_path {
        Some(ip) => {
            let inode_node = match crate::fs::lookup(&ip) {
                Some(n) => n,
                None => {
                    crate::tprintln!("exec: interpreter not found: {}", ip);
                    unsafe { write_cr3(old_cr3) };
                    return Err((LoadError::MissingInterpreter, 0));
                }
            };
            // The interpreter is executed on the process's behalf, so it needs
            // `x` too, exactly like the main binary.
            if let crate::fs::vfs::VNode::FsNode { node, .. } = &inode_node {
                if let Some(creds) = crate::fs::perm::current_creds() {
                    let perms = crate::fs::perm::perms_of(&**node);
                    if !crate::fs::perm::check(&perms, &creds, crate::fs::perm::Access::Execute)
                        .is_allow()
                    {
                        unsafe { write_cr3(old_cr3) };
                        return Err((
                            LoadError::PermissionDenied,
                            crate::arch::x86_64::syscalls::EACCES,
                        ));
                    }
                }
            }
            let inode = crate::fs::vfs::vnode_cache_id(&inode_node);
            match read_whole(&inode_node, &mut interp_scratch) {
                Ok(d) => {
                    // Re-borrow: `interp_scratch` is borrowed for 'a below.
                    let d: &[u8] = unsafe { core::slice::from_raw_parts(d.as_ptr(), d.len()) };
                    (Some(d), inode)
                }
                Err(_) => {
                    unsafe { write_cr3(old_cr3) };
                    return Err((LoadError::Unreadable, 0));
                }
            }
        }
        None => (None, None),
    };

    let mut allocator_guard = FRAME_ALLOCATOR.lock();
    let loaded = load_elf(
        file_data,
        file_inode,
        interp_data,
        interp_inode,
        &mut *allocator_guard,
        argv,
        envs,
    );
    drop(allocator_guard);

    match loaded {
        Some((entry_point, stack_top, main_bin_end)) => Ok((
            LoadedImage { entry_point, stack_top, main_bin_end, new_pml4, old_cr3 },
            file_scratch,
            interp_scratch,
        )),
        None => {
            unsafe { write_cr3(old_cr3) };
            Err((LoadError::BadElf, 0))
        }
    }
}

/// The environment the boot path hands to the first user process.
///
/// Boot-only, and deliberately not used by `execve`: a program started by
/// another program inherits its environment, and inventing variables at exec
/// time would override the caller's.
pub fn boot_environment(test_mode: bool) -> Vec<&'static str> {
    let mut envs = alloc::vec![
        "USER=root",
        "HOME=/root",
        "PATH=/bin:/usr/bin",
        "TERM=linux",
        "XDG_RUNTIME_DIR=/tmp",
        "WAYLAND_DISPLAY=wayland-0",
        "LIBSEAT_BACKEND=seatd",
        "LIBSEAT_LOG_LEVEL=debug",
        "SEATD_SOCK=/run/seatd.sock",
        "SEAT=seat0",
        "XDG_SEAT=seat0",
        "XDG_SESSION_TYPE=wayland",
    ];
    if test_mode {
        envs.push("DRAGOS_TEST_MODE=1");
    }
    envs
}

/// Locates `path`, loads it as an ELF, and transfers control to it. Does not
/// return on success.
///
/// Boot-only. There is no current thread to return to, so this registers pid 1
/// and enters user mode with `execute_and_resume` — see the module docs for why
/// this cannot simply be `execve`.
pub fn exec_initrd(path: &str, uid: u32, gid: u32) {
    exec_initrd_args(path, &[path], uid, gid);
}

/// Loads `path` and transfers control to it.
///
/// `main` relies on this *returning* when the file cannot be loaded, so that it
/// can try its next candidate.
pub fn exec_initrd_args(path: &str, argv: &[&str], uid: u32, gid: u32) {
    if exec_try(path, argv, uid, gid) {
        return;
    }
    // Loaded successfully, but control can only come back here if userspace
    // faulted and the fault unwound into this frame. Halting is correct: falling
    // out would run `main`'s next candidate and load a second image over the
    // first. This was observed happening — two `register_exec` traces and two
    // `main_entry` values in a single boot.
    loop {
        crate::arch::x86_64::cpu::hlt();
    }
}

/// Attempt to load and enter `path`. Returns false only if the image could not
/// be loaded at all, which is the case `main` uses to try its next candidate.
fn exec_try(path: &str, argv: &[&str], uid: u32, gid: u32) -> bool {
    crate::dprintln!("[exec] lookup {}", path);

    // The boot path has no environment to inherit, so it supplies the default
    // set. `execve` does not use this -- see `boot_environment`.
    let envs = boot_environment(
        crate::BOOT_TEST_MODE.load(core::sync::atomic::Ordering::Relaxed),
    );

    // Invariant 6: `register_exec_thread` must run before the segments are
    // mapped, because they are recorded as VMAs against the current process.
    // `load_image` calls this hook immediately after it installs the new CR3,
    // which is before any mapping -- so the ordering holds without this
    // function having to know the address space in advance.
    //
    // It must NOT allocate an address space itself: `load_image` does that, and
    // an earlier version of this function allocated its own, leaving pid 1
    // registered against a PML4 that CR3 never pointed at. The guest then hung
    // on its first userspace entry with a WRONGSTACK diagnostic.
    match load_image(path, argv, &envs, |pml4| {
        register_exec_thread(pml4, uid, gid, path);
    }) {
        Ok((image, _file, _interp)) => {
            if let Some(t) = crate::tasks::current_thread() {
                t.process.lock().heap_brk =
                    shared::memory::address::VirtAddr(image.main_bin_end);
            }
            tprintln!("Dropping to Ring 3 at {:#X}...", image.entry_point);
            unsafe {
                execute_and_resume(image.entry_point, image.stack_top);
            }
            // Only reachable if userspace faulted and unwound back to here.
            true
        }
        Err((err, _)) => {
            // `load_image` restored the old CR3 itself on every failure path.
            tprintln!("exec: {}", err.describe());
            false
        }
    }
}
