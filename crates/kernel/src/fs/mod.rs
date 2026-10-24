use crate::arch::sync::spinlock::SpinLock;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec::Vec;
use shared::memory::allocator::FrameAllocator;
use vfs::{FileNode, VNode};

pub mod devdri;
pub mod devinput;
pub mod dirent;
pub mod ext2;
pub mod ext4;
pub mod firmware;
pub mod mount;
pub mod perm;
pub mod pty;
pub mod reader;
pub mod sysfs;
pub mod tar;
pub mod tmpfs;
pub mod udev;
pub mod vfs;

/// Static /proc/asound/cards content for ALSA card detection.
static PROC_ASOUND_CARDS: &[u8] = b" 0 [AC97          ]: AC97 - Intel 82801AA-ICH\n                      Intel 82801AA-ICH with ALC203 at irq 5\n";
static PROC_ASOUND_DEVICES: &[u8] = b"  0: [0- 0]: digital audio playback\n  1: [0- 0]: digital audio capture\n  3: [0- 0]: timer\n";
static PROC_ASOUND_PCM: &[u8] = b"00-00: AC97 HiFi : AC97 HiFi : playback 1 : capture 1\n";
static PROC_ASOUND_VERSION: &[u8] = b"Advanced Linux Sound Architecture Driver Version k6.1.0.\n";

/// Runtime-loaded initrd (tar archive) placed in physical memory by the
/// bootloader and exposed through HHDM. Set once during boot from
/// `BootInfo.initrd`; `None` when the bootloader found no archive.
pub static INITRD: SpinLock<Option<&'static [u8]>> = SpinLock::new(None);
pub static ROOT_NODE: SpinLock<Option<Arc<dyn FileNode>>> = SpinLock::new(None);
/// Dentry cache: maps canonical path -> VNode. Accelerates path lookups for
/// frequently-accessed files (e.g. /bin/sh, /usr/bin/foot). Invalidated on
/// unlink/rename and cleared when the root filesystem changes.
///
/// Size bounded at 256 entries; on overflow the oldest entry is evicted.
pub static DENTRY_CACHE: SpinLock<BTreeMap<alloc::string::String, VNode>> =
    SpinLock::new(BTreeMap::new());
pub static DENTRY_CACHE_ORDER: SpinLock<alloc::vec::Vec<alloc::string::String>> =
    SpinLock::new(alloc::vec::Vec::new());
const DENTRY_CACHE_MAX: usize = 256;
/// Handles of the mounted disk filesystem for sync/fsync (whichever mounted).
pub static MOUNTED_EXT4: SpinLock<Option<Arc<ext4::Ext4Fs>>> = SpinLock::new(None);
pub static MOUNTED_EXT2: SpinLock<Option<Arc<ext2::Ext2Fs>>> = SpinLock::new(None);

/// Flush all dirty filesystem blocks and superblocks to disk.
pub fn sync_all() {
    if let Some(fs) = MOUNTED_EXT4.lock().as_ref() {
        let _ = fs.sync_all();
    }
    if let Some(fs) = MOUNTED_EXT2.lock().as_ref() {
        let _ = fs.cache.sync();
        let _ = fs.update_superblock();
    }
}

/// Points the initrd at `base + size` in the higher-half direct map.
pub fn set_initrd(hhdm_base: u64, base: u64, size: usize) {
    *INITRD.lock() =
        Some(unsafe { core::slice::from_raw_parts((hhdm_base + base) as *const u8, size) });
}

/// Frees the initrd memory pages and clears the global reference.
/// Call this once the root filesystem (ext2/ext4) is mounted and the tar
/// is no longer needed, to reclaim the bootloader-allocated region.
pub fn clear_initrd() {
    let mut guard = INITRD.lock();
    if let Some(slice) = guard.as_ref() {
        let ptr = slice.as_ptr() as u64;
        let size = slice.len();
        let pages = (size + shared::memory::address::PAGE_SIZE as usize - 1)
            / shared::memory::address::PAGE_SIZE as usize;
        let hhdm = shared::boot::boot_info::HHDM_BASE.0;
        let first_page_virt = ptr & !(shared::memory::address::PAGE_SIZE - 1);
        let mut alloc = crate::memory::FRAME_ALLOCATOR.lock();
        for i in 0..pages {
            let v = first_page_virt + (i * shared::memory::address::PAGE_SIZE as usize) as u64;
            let phys = v - hhdm;
            alloc.dealloc_pages(shared::memory::address::PhysAddr(phys), 1);
        }
        drop(alloc);
        crate::dprintln!("[initrd] freed {} pages ({} bytes)", pages, size);
    }
    *guard = None;
    dentry_clear();
}

/// Mounts the ext2/ext4 root filesystem. Returns `true` if it mounted successfully.
/// When it returns `true`, the TAR initrd is not needed and `set_initrd` should
/// be skipped entirely (ext2/ext4 is the only filesystem in use).
pub fn mount_root() -> bool {
    crate::fs::mount::register_default_filesystems();
    // Try Ext4 first (handles ext2/ext3/ext4 depending on features).
    if let Some(fs) = ext4::Ext4Fs::new() {
        crate::dprintln!("[mount] ext4 sb ok");
        if let Ok(root) = ext4::Ext4Node::new(fs.clone(), ext4::structs::EXT4_INODE_ROOT) {
            let root_arc = Arc::new(root);
            crate::tprintln!("Mounted Ext4 filesystem");
            *ROOT_NODE.lock() = Some(root_arc.clone());
            *MOUNTED_EXT4.lock() = Some(fs);
            crate::fs::mount::mount_add(crate::fs::mount::Mount {
                mountpoint: String::from("/"),
                root: root_arc,
                fstype: "ext4",
                source: Some(String::from("vda")),
                flags: 0,
                open_count: core::sync::atomic::AtomicUsize::new(0),
            }).ok();
            dentry_clear();
            return true;
        }
    }

    if let Some(fs) = ext2::Ext2Fs::new() {
        if let Ok(root) = ext2::Ext2Node::new(fs.clone(), ext2::structs::EXT2_INODE_ROOT) {
            let root_arc = Arc::new(root);
            crate::tprintln!("Mounted Ext2 filesystem");
            *ROOT_NODE.lock() = Some(root_arc.clone());
            *MOUNTED_EXT2.lock() = Some(fs);
            crate::fs::mount::mount_add(crate::fs::mount::Mount {
                mountpoint: String::from("/"),
                root: root_arc,
                fstype: "ext2",
                source: Some(String::from("vda")),
                flags: 0,
                open_count: core::sync::atomic::AtomicUsize::new(0),
            }).ok();
            dentry_clear();
            return true;
        }
    }
    crate::tprintln!("WARNING: ext2/ext4 mount failed; falling back to TAR initrd");
    false
}

pub fn canonicalize_path(cwd: &str, path: &str) -> alloc::string::String {
    use alloc::string::String;
    use alloc::vec::Vec;

    let absolute = if path.starts_with('/') {
        String::from(path)
    } else {
        let mut s = String::from(cwd);
        if !s.ends_with('/') {
            s.push('/');
        }
        s.push_str(path);
        s
    };

    let mut parts: Vec<&str> = Vec::new();
    for part in absolute.split('/') {
        if part.is_empty() || part == "." {
            continue;
        } else if part == ".." {
            parts.pop();
        } else {
            parts.push(part);
        }
    }

    let mut res = String::from("/");
    for (i, part) in parts.iter().enumerate() {
        res.push_str(part);
        if i + 1 < parts.len() {
            res.push('/');
        }
    }
    res
}

pub fn lookup(path: &str) -> Option<VNode> {
    let cwd = match crate::tasks::current_thread() {
        Some(t) => t.process.lock().cwd.clone(),
        None => alloc::string::String::from("/"),
    };
    lookup_relative_to(&cwd, path)
}

/// Why a lookup failed.
///
/// The distinction matters to the caller: Linux answers `EACCES` when a
/// directory on the path cannot be searched and `ENOENT` when the path simply
/// does not exist, and a program that cannot tell them apart cannot tell "you
/// may not look here" from "there is nothing here" — so it reports the wrong
/// thing, or retries, or gives up on the wrong branch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LookupError {
    /// No such file or directory.
    NotFound,
    /// A directory on the path is not searchable by this caller.
    Denied,
}

/// Look `path` up relative to `cwd`, discarding the reason for a failure.
pub fn lookup_relative_to(cwd: &str, path: &str) -> Option<VNode> {
    lookup_relative_to_checked(cwd, path).ok()
}

/// Look `path` up relative to the process's working directory, keeping the
/// reason for a failure.
pub fn lookup_checked(path: &str) -> Result<VNode, LookupError> {
    let cwd = match crate::tasks::current_thread() {
        Some(t) => t.process.lock().cwd.clone(),
        None => alloc::string::String::from("/"),
    };
    lookup_relative_to_checked(&cwd, path)
}

pub fn lookup_relative_to_checked(cwd: &str, path: &str) -> Result<VNode, LookupError> {
    let canonical = canonicalize_path(cwd, path);
    // The caller's identity, fetched once, before any lock is taken: the walk
    // below holds ROOT_NODE while it descends and `current_creds` locks the
    // process (AGENTS.md §1 puts process.lock() innermost).
    let creds = perm::current_creds();

    // The dentry cache is a shortcut that skips the traversal check, so only a
    // caller that cannot lose the check may use it. For anyone else the path
    // is walked, which is what applies the per-component `x` rule below.
    if creds.as_ref().map_or(true, |c| c.bypasses_dac()) {
        let cache = DENTRY_CACHE.lock();
        if let Some(vnode) = cache.get(&canonical) {
            return Ok(vnode.clone());
        }
    }

    let clean = canonical.trim_start_matches('/');

    // Descending into a directory needs the right to search it. This is what
    // makes `/root/secret` unreachable to an unprivileged caller even when
    // `secret` is itself world-readable: the `x` bit is checked on every
    // directory the path passes *through*, not on the final component (which
    // `open` checks with its real intent, so `stat` of a `000` file still
    // works).
    let may_search = |dir: &dyn FileNode| -> Result<(), LookupError> {
        match &creds {
            Some(c) => {
                if perm::check(&perm::perms_of(dir), c, perm::Access::Execute).is_allow() {
                    Ok(())
                } else {
                    Err(LookupError::Denied)
                }
            }
            None => Ok(()),
        }
    };

    // Phase 1: consult the mount table before the legacy hardcoded arms.
    // Pseudo-filesystems (proc/sysfs/devtmpfs/tmpfs) are skipped here and
    // resolved by the arms below.
    if let Some((mnt, rel)) = crate::fs::mount::resolve_mount(&canonical) {
        if mnt.mountpoint != "/" {
            if rel.is_empty() {
                let vnode = VNode::FsNode { node: mnt.root.clone(), cursor: 0 };
                dentry_insert(&canonical, vnode.clone());
                return Ok(vnode);
            }
            let mut current = mnt.root.clone();
            for part in rel.split('/') {
                if part.is_empty() { continue; }
                may_search(&*current)?;
                match current.lookup(part) {
                    Ok(n) => current = n,
                    Err(_) => return Err(LookupError::NotFound),
                }
            }
            let vnode = VNode::FsNode { node: current, cursor: 0 };
            dentry_insert(&canonical, vnode.clone());
            return Ok(vnode);
        }                                   // closes if mnt.mountpoint
    }                                         // closes if let

    if clean == "dev/pcspeaker" {
        return Ok(VNode::PcSpeakerDevice);
    }
    if clean == "dev/dsp" || clean == "dev/audio" || clean == "dev/mixer" {
        return Ok(VNode::DspAudioDevice);
    }
    if clean == "dev/ptmx" || clean == "ptmx" {
        let (_id, pty) = pty::alloc_pty();
        return Ok(VNode::PtyMaster(pty));
    }
    if clean.starts_with("dev/pts/") || clean.starts_with("pts/") {
        let suffix = if clean.starts_with("dev/pts/") {
            &clean["dev/pts/".len()..]
        } else {
            &clean["pts/".len()..]
        };
        if let Ok(id) = suffix.parse::<u32>() {
            if let Some(pty) = pty::get_pty(id) {
                let mut p = pty.lock();
                p.slave_open = true;
                if p.pgrp == 0 {
                    p.pgrp = crate::tasks::get_current_pid() as i32;
                }
                drop(p);
                return Ok(VNode::PtySlave(pty));
            }
        }
        return Err(LookupError::NotFound);
    }
    // Linux's rule: /dev/tty, /dev/console and /dev/tty0 all mean "the current
    // virtual terminal"; /dev/ttyN names VT N specifically. Resolving 0 at
    // *access* time rather than at open time is what lets a program that opened
    // /dev/tty0 follow the user across a VT switch, which is the point of the
    // tty0 alias.
    if clean == "dev/tty" || clean == "dev/console" || clean == "dev/tty0" {
        return Ok(VNode::TtyDevice { vt: 0 });
    }
    if let Some(num) = clean.strip_prefix("dev/tty") {
        if let Ok(n) = num.parse::<u16>() {
            if (1..=crate::graphics::vt::MAX_VTS as u16).contains(&n) {
                return Ok(VNode::TtyDevice { vt: n });
            }
            // A VT that does not exist must not silently fall back to the
            // current console: that would let a program scribble on VT 1 while
            // believing it had a screen of its own.
            return Err(LookupError::NotFound);
        }
    }
    if clean == "dev/fb0" || clean == "dev/fb" {
        return Ok(VNode::FbDevice { cursor: 0 });
    }
    // ── Dynamic /dev/input synthetic filesystem ──────────────────────────────
    if clean == "dev/input" || clean == "dev/input/" {
        return Ok(VNode::FsNode {
            node: Arc::new(devinput::DevInputDir),
            cursor: 0,
        });
    }
    if clean == "dev/input/by-path" || clean == "dev/input/by-path/" {
        return Ok(VNode::FsNode {
            node: Arc::new(devinput::DevInputByPathDir),
            cursor: 0,
        });
    }
    if clean == "dev/input/by-id" || clean == "dev/input/by-id/" {
        return Ok(VNode::FsNode {
            node: Arc::new(devinput::DevInputByIdDir),
            cursor: 0,
        });
    }
    if let Some(target) = clean.strip_prefix("dev/input/by-path/") {
        if let Some(minor) = devinput::resolve_by_path(target) {
            return Ok(VNode::InputEventDevice { minor });
        }
    }
    if let Some(target) = clean.strip_prefix("dev/input/by-id/") {
        if let Some(minor) = devinput::resolve_by_id(target) {
            return Ok(VNode::InputEventDevice { minor });
        }
    }
    if clean == "dev/input/mice" {
        return devinput::mouse_minor()
            .map(|minor| VNode::InputMiceDevice { minor })
            .ok_or(LookupError::NotFound);
    }
    if let Some(rest) = clean.strip_prefix("dev/input/mouse") {
        if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
            if let Ok(n) = rest.parse::<usize>() {
                return devinput::mouse_minor_at(n)
                    .map(|minor| VNode::InputEventDevice { minor })
                    .ok_or(LookupError::NotFound);
            }
        }
    }
    if clean == "dev/input/events" {
        // ponytail: legacy aggregate alias → PS/2 keyboard's own node.
        return crate::drivers::keyboard::input_dev_of("dev/input/events")
            .map(|minor| VNode::InputEventDevice { minor })
            .ok_or(LookupError::NotFound);
    }
    // ponytail: generic eventN (major 13, minor 64+N) gated on the live
    // INPUT_DEV table — unknown minor resolves to nothing (→ ENOENT at
    // open), never an alias to the keyboard. event999 must not route to kbd.
    for prefix in ["dev/input/event", "dev/event"] {
        if let Some(rest) = clean.strip_prefix(prefix) {
            if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
                if let Ok(n) = rest.parse::<usize>() {
                    if crate::drivers::keyboard::INPUT_DEV.lock().contains_key(&n) {
                        return Ok(VNode::InputEventDevice { minor: n });
                    }
                    return Err(LookupError::NotFound);
                }
            }
        }
    }
    if clean == "dev/null" || clean == "dev/zero" || clean == "dev/urandom" || clean == "dev/random" {
        return Ok(VNode::NullDevice);
    }
    // ponytail: the serial console, as its own node rather than another VT alias.
    // A TtyDevice write never reaches COM1, so this is the only way userspace
    // output gets to the host -- which is what the test harness reads.
    if clean == "dev/ttyS0" || clean == "dev/ttyS1" || clean == "dev/serial0" {
        return Ok(VNode::SerialDevice);
    }
    if clean == "dev/dri" || clean == "dev/dri/" {
        // ponytail: listable alias — the nodes themselves resolve below
        // with per-node minors; the on-disk /dev/dri stays empty.
        return Ok(VNode::FsNode {
            node: Arc::new(devdri::DriDir),
            cursor: 0,
        });
    }
    if clean == "dev/dri/card0" || clean == "dev/dri/card" {
        return Ok(VNode::DrmDevice { minor: 0 });
    }
    if clean == "dev/dri/renderD128" || clean == "dev/dri/render" {
        return Ok(VNode::DrmDevice { minor: 128 });
    }
    if clean == "dev/snd/controlC0D0" || clean == "dev/snd/controlC0" {
        return Ok(VNode::SndControlDevice);
    }
    if clean == "dev/snd/pcmC0D0p" {
        return Ok(VNode::SndPlaybackDevice);
    }
    if clean == "dev/snd/pcmC0D0c" {
        return Ok(VNode::SndCaptureDevice);
    }
    if clean == "dev/fuse" {
        return Ok(VNode::FuseDevice);
    }

    if clean == "proc/self/exe" {
        return lookup("/bin/mywima").ok_or(LookupError::NotFound);
    }

    if clean.starts_with("proc/self/fd/") || (clean.starts_with("proc/") && clean.contains("/fd/")) {
        if let Some(fd_str) = clean.rsplit('/').next() {
            if let Ok(fd_num) = fd_str.parse::<usize>() {
                if let Some(thread) = crate::tasks::current_thread() {
                    let p = thread.process.lock();
                    if let Some(Some(slot)) = p.fd_table.get(fd_num) {
                        return Ok(slot.lock().clone_vnode());
                    }
                }
            }
        }
    }

    // Only `/proc` itself and the two paths we actually implement exist. This used
    // to claim EVERY `/proc/...` path as a directory, so `open("/proc/self/cgroup")`
    // succeeded and returned a directory vnode whose `read` yields 0 forever.
    //
    // That is not a harmless wrong answer: Rust's runtime probes for CPU count
    // by reading `/proc/self/cgroup` (and friends) and loops until the read
    // returns something other than EOF, so the process spun at ~2M read(2) per
    // 8 s and never made progress. zellij's screen thread stalled in exactly
    // that loop, so it never reached `openpty` and never created a pane.
    //
    // Linux returns ENOENT for a procfs entry that does not exist. Matching
    // that is what makes a probing program give up instead of spinning.
    if clean == "proc" {
        return Ok(VNode::Directory { name: "proc".into(), cursor: 0 });
    }
    if clean == "proc/self" {
        return Ok(VNode::Directory { name: "self".into(), cursor: 0 });
    }
    // /proc/asound/* — minimal ALSA procfs for card detection
    if clean == "proc/asound" || clean == "proc/asound/" {
        return Ok(VNode::Directory { name: "asound".into(), cursor: 0 });
    }
    if clean == "proc/asound/cards" {
        return Ok(VNode::InitrdFile {
            data: PROC_ASOUND_CARDS,
            cursor: 0,
        });
    }
    if clean == "proc/asound/devices" {
        return Ok(VNode::InitrdFile {
            data: PROC_ASOUND_DEVICES,
            cursor: 0,
        });
    }
    if clean == "proc/asound/pcm" {
        return Ok(VNode::InitrdFile {
            data: PROC_ASOUND_PCM,
            cursor: 0,
        });
    }
    if clean == "proc/asound/version" {
        return Ok(VNode::InitrdFile {
            data: PROC_ASOUND_VERSION,
            cursor: 0,
        });
    }
    if clean.starts_with("proc/") {
        return Err(LookupError::NotFound);
    }

    if clean == "sys" || clean.starts_with("sys/") {
        let rel = clean.trim_start_matches("sys").trim_start_matches('/');
        if let Some(node) = sysfs::resolve(rel) {
            return Ok(VNode::FsNode { node, cursor: 0 });
        }
        return Err(LookupError::NotFound);
    }

    if clean == "tmp" || clean.starts_with("tmp/") {
        let rel = clean.trim_start_matches("tmp").trim_start_matches('/');
        if let Some(node) = tmpfs::resolve(rel, false, false) {
            return Ok(VNode::FsNode { node, cursor: 0 });
        }
    }

    if clean == "run/udev" || clean.starts_with("run/udev/") {
        let rel = clean["run/udev".len()..].trim_start_matches('/');
        if let Some(node) = udev::resolve(rel) {
            return Ok(VNode::FsNode { node, cursor: 0 });
        }
    }

    if clean == "run" || clean.starts_with("run/") {
        let rel = clean.trim_start_matches("run").trim_start_matches('/');
        if let Some(node) = tmpfs::resolve(rel, false, false) {
            return Ok(VNode::FsNode { node, cursor: 0 });
        }
    }

    let root_guard = ROOT_NODE.lock();
    if let Some(root) = root_guard.as_ref() {
        let mut current = root.clone();
        drop(root_guard);
        if clean.is_empty() {
            let vnode = VNode::FsNode { node: current, cursor: 0 };
            dentry_insert(&canonical, vnode.clone());
            return Ok(vnode);
        }
        for component in clean.split('/') {
            if component.is_empty() { continue; }
            may_search(&*current)?;
            if let Ok(next) = current.lookup(component) {
                current = next;
            } else {
                return Err(LookupError::NotFound);
            }
        }
        let vnode = VNode::FsNode { node: current, cursor: 0 };
        dentry_insert(&canonical, vnode.clone());
        return Ok(vnode);
    } else {
        drop(root_guard);
    }

    if let Some(vnode) = tar::find_vnode(&canonical) {
        dentry_insert(&canonical, vnode.clone());
        return Ok(vnode);
    }
    Err(LookupError::NotFound)
}

/// Insert a VNode into the dentry cache, evicting the oldest entry if full.
fn dentry_insert(path: &str, vnode: VNode) {
    let mut cache = DENTRY_CACHE.lock();
    let mut order = DENTRY_CACHE_ORDER.lock();
    if cache.len() >= DENTRY_CACHE_MAX {
        if let Some(oldest) = order.first().cloned() {
            order.remove(0);
            cache.remove(&oldest);
        }
    }
    cache.insert(path.to_string(), vnode);
    order.push(path.to_string());
}

/// Remove a path from the dentry cache (called on unlink/rename).
pub fn dentry_remove(path: &str) {
    let mut cache = DENTRY_CACHE.lock();
    let mut order = DENTRY_CACHE_ORDER.lock();
    cache.remove(path);
    if let Some(pos) = order.iter().position(|p| p == path) {
        order.remove(pos);
    }
}

/// Clear the entire dentry cache (called when root filesystem changes).
pub fn dentry_clear() {
    let mut cache = DENTRY_CACHE.lock();
    let mut order = DENTRY_CACHE_ORDER.lock();
    cache.clear();
    order.clear();
}

pub fn create(path: &str, is_dir: bool, mode: u32) -> Option<VNode> {
    let cwd = match crate::tasks::current_thread() {
        Some(t) => t.process.lock().cwd.clone(),
        None => alloc::string::String::from("/"),
    };
    let canonical = canonicalize_path(&cwd, path);
    let clean = canonical.trim_start_matches('/');

    let creds = perm::current_creds();
    // The umask masks off permission bits at creation; the file-type bits are
    // never masked. Read it once — it is per-process and does not change mid-call.
    let umask = crate::tasks::current_process()
        .map(|p| p.lock().umask)
        .unwrap_or(0o022);

    // Create `leaf` under directory `current`, giving it the caller's
    // ownership (`perm::new_file_owner`) and the umasked mode. The mode
    // argument carries the permission bits only; the file-type bits are OR'd
    // in by the filesystem's create.
    let create_leaf = |current: &Arc<dyn FileNode>, leaf: &str| -> Option<Arc<dyn FileNode>> {
        let creds = creds.as_ref()?;
        let parent_perms = perm::perms_of(&**current);
        let (uid, gid) = perm::new_file_owner(&parent_perms, creds);
        let final_mode = perm::apply_umask(mode, umask);
        current.create(leaf, is_dir, final_mode, uid, gid).ok()
    };

    // Phase 1: consult the mount table for real filesystems.
    if let Some((mnt, rel)) = crate::fs::mount::resolve_mount(&canonical) {
        if mnt.mountpoint != "/" && !rel.is_empty() {
            let parts: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
            if !parts.is_empty() {
                let leaf = parts[parts.len() - 1];
                let parent_rel = &parts[..parts.len() - 1];
                let mut current = mnt.root.clone();
                for p in parent_rel {
                    match current.lookup(p) {
                        Ok(n) => current = n,
                        Err(_) => return None,
                    }
                }
                let node = create_leaf(&current, leaf)?;
                let vnode = VNode::FsNode { node, cursor: 0 };
                return Some(vnode);
            }
        }
    }

    if clean == "tmp" || clean.starts_with("tmp/") {
        let rel = clean.trim_start_matches("tmp").trim_start_matches('/');
        // Walk to the parent within tmpfs, then create the leaf.
        let parts: Vec<&str> = rel.split('/').filter(|c| !c.is_empty()).collect();
        if parts.is_empty() { return None; }
        let parent_rel: Vec<&str> = parts[..parts.len()-1].to_vec();
        let parent = tmpfs::resolve(&parent_rel.join("/"), true, false)?;
        let leaf = parts[parts.len()-1];
        let node = create_leaf(&parent, leaf)?;
        return Some(VNode::FsNode { node, cursor: 0 });
    }

    if clean == "run" || clean.starts_with("run/") {
        let rel = clean.trim_start_matches("run").trim_start_matches('/');
        let parts: Vec<&str> = rel.split('/').filter(|c| !c.is_empty()).collect();
        if parts.is_empty() { return None; }
        let parent_rel: Vec<&str> = parts[..parts.len()-1].to_vec();
        let parent = tmpfs::resolve(&parent_rel.join("/"), true, false)?;
        let leaf = parts[parts.len()-1];
        let node = create_leaf(&parent, leaf)?;
        return Some(VNode::FsNode { node, cursor: 0 });
    }

    let root_guard = ROOT_NODE.lock();
    if let Some(root) = root_guard.as_ref() {
        let mut components: alloc::vec::Vec<&str> = clean.split('/').filter(|c| !c.is_empty()).collect();
        if components.is_empty() { return None; }

        let new_file_name = components.pop().unwrap();

        let mut current = root.clone();
        drop(root_guard);
        for component in components {
            if let Ok(next) = current.lookup(component) {
                current = next;
            } else {
                return None;
            }
        }

        if let Some(new_node) = create_leaf(&current, new_file_name) {
            return Some(VNode::FsNode { node: new_node, cursor: 0 });
        }
    }
    None
}
