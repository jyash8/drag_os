//! Loadable kernel modules — `docs/module-system.md`.
//!
//! Status: **milestone 2, phase 2 only.** What exists today is ELF parsing and
//! validation ([`elf`]). It runs before any slot is reserved and before any page
//! table is touched, so it is the one part of the loader that cannot corrupt
//! kernel state no matter what it is fed. Phases 3-9 (reserve, allocate, copy,
//! relocate, zero, publish, init) are not implemented; [`elf::parse_module`]
//! deliberately returns everything those phases need and nothing more.
//!
//! The ordering is the point. §8.1 requires validation to precede allocation so
//! that a malformed module — which is ordinary userspace input, reachable via
//! `fs::lookup` + `VNode::read` per §8.1 phase 1 — costs one bounds check.
//!
//! Do not reorder these phases. See `docs/module-system.md` §12 for the
//! invariants module code must honour, and AGENTS.md §16.3 for the `MODULE_START`
//! range in `tasks.rs` that must stay in step with
//! [`elf::MODULE_WINDOW_BASE`].

pub mod elf;
pub mod exports;
pub mod loader;
pub mod reloc;
pub mod selftest;
pub mod slot;

/// The kernel's `vermagic` (§5.2). A module built against a different kernel
/// is refused rather than loaded and crashed into.
///
/// Kept as a constant rather than derived from the build so that bumping it is
/// a deliberate, reviewable edit: it is the compatibility contract between the
/// kernel and every module, and deriving it automatically would defeat the
/// point of having one.
pub const KERNEL_VERMAGIC: &str = "dragOS 0.1.0 x86_64";

/// Parse and validate a module image with the kernel's own `vermagic`.
///
/// Thin wrapper so callers do not have to pass `KERNEL_VERMAGIC`, which is the
/// only value that should ever be used in production.
pub fn validate(data: &[u8]) -> Result<elf::ModuleImage<'_>, elf::ModuleError> {
    // Base 0: the image is measured, not placed. The loader re-bases it with
    // `elf::layout` once a slot exists.
    elf::parse_module(data, KERNEL_VERMAGIC, 0)
}

/// Read a whole file through the normal VNode path, for the boot self-check.
///
/// §8.1 phase 1 requires module loading to go through `fs::lookup` +
/// `VNode::read` rather than a private read path, so a staged module gets the
/// same ext4 validation and audit trail as any other file. This helper is that
/// path, factored out so it can be reused by the loader once phases 3-9 exist.
pub fn read_whole(path: &str) -> Result<alloc::vec::Vec<u8>, alloc::string::String> {
    use alloc::format;
    let mut node = crate::fs::lookup(path).ok_or_else(|| format!("{path}: not found"))?;
    let mut out = alloc::vec::Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match node.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if n > chunk.len() {
                    return Err(format!("{path}: read returned {n}, larger than the buffer"));
                }
                out.extend_from_slice(&chunk[..n]);
            }
            Err(e) => return Err(format!("{path}: read error {e}")),
        }
        // A cap so a pathological VNode cannot spin here forever. The fixture
        // is ~2.5 KiB, so this is three orders of magnitude of headroom.
        if out.len() > 16 * 1024 * 1024 {
            return Err(format!("{path}: exceeds the 16 MiB cap"));
        }
    }
    Ok(out)
}

/// `init_module(2)` — load a module image from userspace.
///
/// Returns 0 on success. Non-zero module-init return is reported as the raw
/// signed value (Linux convention) so the caller sees a real errno.
pub fn load_bytes(data: &[u8]) -> u64 {
    const EBADMSG: u64 = (-74i64) as u64;
    use crate::arch::x86_64::syscalls::EINVAL;
    use crate::arch::x86_64::syscalls::ENOMEM;
    match unsafe { crate::module::loader::load(data) } {
        Ok(loaded) => loaded.init_result as u64,
        Err(crate::module::loader::LoadError::Invalid(_)) => EINVAL,
        Err(crate::module::loader::LoadError::NotElf) => EBADMSG,
        Err(crate::module::loader::LoadError::NoSlot)
        | Err(crate::module::loader::LoadError::OutOfMemory) => ENOMEM,
        Err(crate::module::loader::LoadError::MapFailed) => ENOMEM,
        Err(crate::module::loader::LoadError::Relocation(_)) => EINVAL,
        Err(crate::module::loader::LoadError::Layout(_)) => EINVAL,
        Err(crate::module::loader::LoadError::InitFailed(r)) => r as u64,
    }
}

pub fn sys_init_module(image: u64, len: usize, _uargs: u64) -> u64 {
    use crate::arch::x86_64::syscalls::{EFAULT, EINVAL, EPERM, ESRCH};
    use crate::memory::space::validate_user_read_range;
    let thread = match crate::tasks::current_thread() {
        Some(t) => t,
        None => return ESRCH,
    };
    {
        let p = thread.process.lock();
        if p.euid != 0 {
            return EPERM;
        }
    }
    if len == 0 || len > 32 * 1024 * 1024 {
        return EINVAL;
    }
    if !validate_user_read_range(image, len) {
        return EFAULT;
    }
    let mut data = alloc::vec![0u8; len];
    unsafe {
        core::ptr::copy_nonoverlapping(image as *const u8, data.as_mut_ptr(), len);
    }
    load_bytes(&data)
}

/// Read a module blob through an fd-based kernel file path and hand it to the
/// loader.  Permission check is the opener's; this mostly exists for symmetry.
pub fn sys_finit_module(fd: i32, _uargs: u64, _flags: i32) -> u64 {
    // Minimal fallback until file-backed fd loading is wired.
    let _ = fd;
    crate::arch::x86_64::syscalls::ENOSYS
}
