//! File permission checks — Linux's discretionary access control.
//!
//! Before this module existed the kernel *stored* every file's mode and owner,
//! reported them from `stat`, and never once compared them against the caller.
//! `setuid(1000)` changed the reported uid and cost the process nothing: it
//! could still read `/etc/shadow`. This module is the comparison.
//!
//! ## Why the check takes an *intent*
//!
//! The rules are not a function of the file alone. Whether the caller may touch
//! it depends on what the caller intends: a `0640` file is readable by its
//! group and not writable by it, so the same file must answer differently to
//! `open(O_RDONLY)` and `open(O_WRONLY)`. Only the caller knows the intent — the
//! fs layer's `read_at` does not — so the syscall layer supplies it and the fs
//! layer stays free of policy.
//!
//! Following Linux, the check happens at **open** time. Once an fd is open its
//! access mode is the authority: a write to an fd opened `O_RDONLY` is refused
//! by the fd's own mode, not by re-checking the file's bits against whatever the
//! process's credentials are by then.
//!
//! ## What the checks cover
//!
//! | intent | what it needs |
//! |---|---|
//! | `Read` | `r` in the caller's class |
//! | `Write` | `w` in the caller's class |
//! | `Execute` | `x` in the caller's class |
//! | `Create` | `w` on the parent directory |
//! | `Remove` | `w` on the parent directory, plus the sticky bit's rule |
//! | `Chmod` | ownership, or `CAP_FOWNER` |
//! | `Chown` | `CAP_CHOWN` |
//!
//! Traversing a directory needs `x` on it, which is why it is a separate check
//! (`Access::Execute`) applied by the path walk to every intermediate
//! component: `/root/secret` is unreachable when `/root` is `0700`, even
//! though `secret` itself might be `0644`.
//!
//! ## The class
//!
//! The caller is classified against the file, most specific first: **owner** if
//! the effective uid matches; otherwise **group** if the file's gid is the
//! effective gid *or any supplementary group*; otherwise **other**. Getting the
//! supplementary list wrong here is how a real system silently denies a user
//! access to files they are supposed to share.

use alloc::vec::Vec;

use super::vfs::FileNode;
use crate::arch::x86_64::syscalls::{EACCES, EPERM};

// ── Capabilities ────────────────────────────────────────────────────────────
//
// A subset of Linux's — the ones an access check actually consults. They are
// bits in a `u32` on the process rather than a separate table because nothing
// here grants or revokes them individually.

/// Bypass the read/write checks (Linux's `CAP_DAC_OVERRIDE`).
pub const CAP_DAC_OVERRIDE: u32 = 1 << 0;
/// Bypass the read and directory-traversal checks (Linux's
/// `CAP_DAC_READ_SEARCH`).
pub const CAP_DAC_READ_SEARCH: u32 = 1 << 1;
/// Bypass the ownership check on `chmod` (Linux's `CAP_FOWNER`).
pub const CAP_FOWNER: u32 = 1 << 2;
/// Change a file's owner and group (Linux's `CAP_CHOWN`).
pub const CAP_CHOWN: u32 = 1 << 3;
/// Change any of the caller's own uids (Linux's `CAP_SETUID`).
pub const CAP_SETUID: u32 = 1 << 4;
/// Change any of the caller's own gids (Linux's `CAP_SETGID`).
pub const CAP_SETGID: u32 = 1 << 5;
/// Load and unload kernel modules (Linux's `CAP_SYS_MODULE`).
pub const CAP_SYS_MODULE: u32 = 1 << 6;

/// Every capability this kernel knows about.
pub const CAP_ALL: u32 = CAP_DAC_OVERRIDE
    | CAP_DAC_READ_SEARCH
    | CAP_FOWNER
    | CAP_CHOWN
    | CAP_SETUID
    | CAP_SETGID
    | CAP_SYS_MODULE;

// ── The intent ──────────────────────────────────────────────────────────────

/// What the caller wants to do with a file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Access {
    /// Read the file's contents (`O_RDONLY`).
    Read,
    /// Write the file's contents (`O_WRONLY`, `O_RDWR`, `O_TRUNC`).
    Write,
    /// Execute the file (`execve`), or traverse a directory.
    Execute,
    /// Create an entry in this directory.
    Create,
    /// Remove or rename an entry in this directory.
    Remove,
    /// Change the file's mode.
    Chmod,
    /// Change the file's owner or group.
    Chown,
}

// ── The identity ────────────────────────────────────────────────────────────

/// The caller's identity.
///
/// This is the **effective** identity: the real and saved ids matter to
/// `setuid` and `execve`, not to access checks.
#[derive(Clone, Debug)]
pub struct Creds {
    pub uid: u32,
    pub gid: u32,
    /// The supplementary group list (`getgroups(2)`).
    pub groups: Vec<u32>,
    /// The capability bits that survived the last credential change.
    pub caps: u32,
}

impl Creds {
    /// Whether this credential set may take the dentry cache's fast path.
    ///
    /// A caller that can lose the traversal check must not skip it, so only a
    /// caller holding the DAC-override capabilities may.
    pub fn bypasses_dac(&self) -> bool {
        self.caps & (CAP_DAC_OVERRIDE | CAP_DAC_READ_SEARCH) != 0
    }

    /// Whether `gid` is the effective gid or one of the supplementary groups.
    pub fn in_group(&self, gid: u32) -> bool {
        self.gid == gid || self.groups.contains(&gid)
    }
}

/// The file's identity, as the fs layer stores it.
#[derive(Clone, Copy, Debug)]
pub struct FilePerms {
    /// The full `st_mode`, including the file-type bits.
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

impl FilePerms {
    /// The sticky bit (`01000`).
    pub fn sticky(&self) -> bool {
        self.mode & 0o1000 != 0
    }
}

/// The verdict of a check.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decision {
    Allow,
    /// Refused; carries the errno to return (`EACCES` or `EPERM`).
    Deny(u64),
}

impl Decision {
    pub fn is_allow(&self) -> bool {
        matches!(self, Decision::Allow)
    }

    /// The errno on refusal, `0` on success — the shape the syscall layer
    /// returns.
    pub fn errno(&self) -> u64 {
        match self {
            Decision::Allow => 0,
            Decision::Deny(e) => *e,
        }
    }
}

// ── Reading the credentials ─────────────────────────────────────────────────

/// The current thread's credentials, or `None` outside a process (kernel
/// boot, interrupt context).
///
/// `None` means "the kernel itself", which is unconstrained: a kernel that
/// could not read its own filesystem would not boot.
///
/// **Do not call this while holding another lock.** It takes `process.lock()`,
/// which is the innermost lock in the ordering (AGENTS.md §1).
pub fn current_creds() -> Option<Creds> {
    let thread = crate::tasks::current_thread()?;
    let process = thread.process.lock();
    Some(Creds {
        uid: process.euid,
        gid: process.egid,
        groups: process.groups.clone(),
        caps: process.caps,
    })
}

/// Build a [`FilePerms`] from a node's `stat`.
pub fn perms_of(node: &dyn FileNode) -> FilePerms {
    let st = node.stat();
    FilePerms {
        mode: st.st_mode,
        uid: st.st_uid,
        gid: st.st_gid,
    }
}

/// The permission bits of a mode, without the file type or the setuid/setgid
/// bits — what the class lookup works on.
fn class_bits(mode: u32, class: Class) -> u32 {
    match class {
        Class::Owner => (mode >> 6) & 7,
        Class::Group => (mode >> 3) & 7,
        Class::Other => mode & 7,
    }
}

enum Class {
    Owner,
    Group,
    Other,
}

fn class_of(file: &FilePerms, creds: &Creds) -> Class {
    if creds.uid == file.uid {
        Class::Owner
    } else if creds.in_group(file.gid) {
        Class::Group
    } else {
        Class::Other
    }
}

// ── The rules ───────────────────────────────────────────────────────────────

/// The bit a plain (`r`/`w`/`x`) intent needs in its class.
fn required_bit(access: Access) -> Option<u32> {
    match access {
        Access::Read => Some(0o4),
        Access::Write => Some(0o2),
        Access::Execute => Some(0o1),
        // Creating or removing an entry modifies the *directory*, so both need
        // the directory's write bit. The execute bit (the right to search the
        // directory) is the path walk's concern.
        Access::Create | Access::Remove => Some(0o2),
        Access::Chmod | Access::Chown => None,
    }
}

/// Decide whether `creds` may perform `access` on `file`.
pub fn check(file: &FilePerms, creds: &Creds, access: Access) -> Decision {
    // The capability bypasses come first, and they are narrower than "root may
    // do anything":
    //  * DAC_OVERRIDE covers read and write, but NOT execute (that is
    //    DAC_READ_SEARCH) and NOT the sticky bit or chmod/chown's ownership
    //    rules — Linux's root cannot rename another user's file out of a
    //    sticky directory, and a kernel that let it would be more permissive
    //    than the system it is imitating.
    if creds.caps & CAP_DAC_OVERRIDE != 0 && matches!(access, Access::Read | Access::Write) {
        return Decision::Allow;
    }
    if creds.caps & CAP_DAC_READ_SEARCH != 0 && access == Access::Execute {
        return Decision::Allow;
    }

    match access {
        Access::Chmod => {
            // Only the owner (or CAP_FOWNER) may change the mode.
            if creds.uid == file.uid || creds.caps & CAP_FOWNER != 0 {
                Decision::Allow
            } else {
                Decision::Deny(EPERM)
            }
        }
        Access::Chown => {
            // Changing the *owner* is root-only. The "owner may change the
            // group, to a group they belong to" exception is the syscall's,
            // because only it knows which of the two ids is changing.
            if creds.caps & CAP_CHOWN != 0 {
                Decision::Allow
            } else {
                Decision::Deny(EPERM)
            }
        }
        _ => {
            let need = required_bit(access).unwrap_or(0);
            let bits = class_bits(file.mode, class_of(file, creds));
            if bits & need == need {
                Decision::Allow
            } else {
                Decision::Deny(EACCES)
            }
        }
    }
}

/// Whether the caller may remove (or rename away) the entry `entry_owner`
/// owns, from the directory `dir`.
///
/// The sticky bit is what protects `/tmp`: without it, a world-writable
/// directory lets anyone delete anyone's file. With it, only the entry's
/// owner, the directory's owner, or `CAP_FOWNER` may.
pub fn check_sticky(dir: &FilePerms, entry_owner: u32, creds: &Creds) -> Decision {
    if !dir.sticky() {
        return Decision::Allow;
    }
    if creds.uid == entry_owner || creds.uid == dir.uid || creds.caps & CAP_FOWNER != 0 {
        Decision::Allow
    } else {
        Decision::Deny(EPERM)
    }
}

/// The `(uid, gid)` a new entry created in `parent` by `creds` gets.
///
/// Linux's rule: the owner is the caller; the group is the parent directory's
/// **if the caller is in it** (which makes a shared directory's contents stay
/// in the shared group), otherwise the caller's effective gid.
pub fn new_file_owner(parent: &FilePerms, creds: &Creds) -> (u32, u32) {
    let gid = if creds.in_group(parent.gid) {
        parent.gid
    } else {
        creds.gid
    };
    (creds.uid, gid)
}

/// Apply a new file's mode: the requested bits, minus the umask.
pub fn apply_umask(mode: u32, umask: u32) -> u32 {
    mode & 0o7777 & !(umask & 0o7777)
}
