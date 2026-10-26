use super::*;

pub static CONSOLE_KD_MODE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0); // 0 = KD_TEXT, 1 = KD_GRAPHICS
pub static CONSOLE_KB_MODE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(2); // 2 = K_UNICODE

/// Advisory file locks for `fcntl(F_GETLK/F_SETLK/F_SETLKW)`.
///
/// Keyed by (vnode pointer, pid) so that a lock on a file belongs to the
/// process that took it, and two fds to the same file in the same process
/// share the lock (the second `F_SETLK` replaces the first).
///
/// Lock ranges (`l_start`/`l_len`) are not tracked — a lock covers the whole
/// file. This is sufficient for `dbus-daemon`, which takes a whole-file
/// exclusive lock on the bus address file to detect a second instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct FileLockKey {
    vnode_key: usize,
    pid: u64,
}

#[derive(Clone, Copy, Debug)]
struct FileLockInfo {
    lock_type: i32, // 0 = F_RDLCK, 1 = F_WRLCK, 2 = F_UNLCK
}

const F_RDLCK: i32 = 0;
const F_WRLCK: i32 = 1;
const F_UNLCK: i32 = 2;

static FILE_LOCKS: SpinLock<alloc::collections::BTreeMap<FileLockKey, FileLockInfo>> =
    SpinLock::new(alloc::collections::BTreeMap::new());

/// Whether two lock types conflict on the same file region.
fn lock_conflicts(existing: i32, requested: i32) -> bool {
    if existing == F_UNLCK || requested == F_UNLCK {
        return false;
    }
    // An exclusive (write) lock conflicts with everything; a shared (read)
    // lock conflicts only with an exclusive lock.
    existing == F_WRLCK || requested == F_WRLCK
}

/// Extract a comparable key from a vnode for use as a file-lock key.
///
/// Only `VNode::FsNode` (real files) supports locking; everything else
/// returns `None`.
fn vnode_lock_key(vnode: &crate::fs::vfs::VNode) -> Option<usize> {
    match vnode {
        crate::fs::vfs::VNode::FsNode { node, .. } => {
            // Cast through a thin pointer: *const dyn FileNode is fat, so go
            // via *const () to get a plain address.
            Some(alloc::sync::Arc::as_ptr(node) as *const () as usize)
        }
        _ => None,
    }
}

/// Look up the vnode key for an fd, returning `None` if the fd is invalid or
/// not a regular file.
fn vnode_key_of_fd(fd: usize) -> Option<usize> {
    let proc = crate::tasks::current_process()?;
    let open_file = {
        let p = proc.lock();
        if fd >= p.fd_table.len() {
            return None;
        }
        p.fd_table[fd].clone()?
    };
    let of = open_file.lock();
    vnode_lock_key(&of.vnode)
}

/// Read `l_type` and `l_whence` from a userspace `struct flock` at `ptr`.
/// Returns `None` if the range is not readable.
fn read_flock_type(ptr: u64) -> Option<(i16, i16)> {
    if !validate_user_read_range(ptr, 4) {
        return None;
    }
    let l_type = unsafe { core::ptr::read_volatile(ptr as *const i16) };
    let l_whence = unsafe { core::ptr::read_volatile((ptr + 2) as *const i16) };
    Some((l_type, l_whence))
}

/// Write `l_type` and `l_pid` into a userspace `struct flock` at `ptr`
/// (used by `F_GETLK` to report the conflicting lock).
fn write_flock_result(ptr: u64, l_type: i16, l_pid: i32) -> bool {
    if !validate_user_write_range(ptr, 28) {
        return false;
    }
    unsafe {
        core::ptr::write_volatile(ptr as *mut i16, l_type);
        // l_whence at offset 2 stays as the caller set it.
        // l_start/l_len at offsets 8/16 stay as the caller set them.
        core::ptr::write_volatile((ptr + 24) as *mut i32, l_pid);
    }
    true
}

/// The current virtual terminal. Re-exported from the VT layer rather than
/// defined here: there used to be a second, independent copy of this counter,
/// and `VT_ACTIVATE` wrote to that one while the console rendered from the
/// other -- so activating a VT moved a number that nothing read.
pub use crate::graphics::vt::ACTIVE_VT;

pub(super) fn sys_read(fd: usize, buf_ptr: *mut u8, count: usize) -> u64 {
    if !validate_user_write_range(buf_ptr as u64, count) {
        return EFAULT;
    }
    let slice = unsafe { core::slice::from_raw_parts_mut(buf_ptr, count) };

    let proc = match crate::tasks::current_process() {
        Some(p) => p,
        None => return EBADF,
    };
    let vnode_opt = {
        let p = proc.lock();
        p.fd_table.get(fd).cloned().flatten()
    };

    if let Some(vnode_arc) = vnode_opt {
        match crate::fs::vfs::OpenFile::read_file(&vnode_arc, slice) {
            Ok(n) => n as u64,
            Err(e) => e,
        }
    } else if fd == 0 {
        match read_stdin_internal(slice) {
            Ok(n) => n as u64,
            Err(e) => e,
        }
    } else {
        EBADF
    }
}

pub(super) fn sys_write(fd: usize, buf_ptr: *const u8, count: usize) -> u64 {
    if !validate_user_read_range(buf_ptr as u64, count) {
        return EFAULT;
    }

    let slice = unsafe { core::slice::from_raw_parts(buf_ptr, count) };

    let vnode_opt = {
        let proc = match crate::tasks::current_process() {
            Some(p) => p,
            None => return EBADF,
        };
        let p = proc.lock();
        if fd < p.fd_table.len() {
            p.fd_table[fd].clone()
        }
        else {
            None
        }
    };

    if let Some(vnode_arc) = vnode_opt {
        return match crate::fs::vfs::OpenFile::write_file(&vnode_arc, slice) {
            Ok(n) => n as u64,
            Err(e) => {
                if e == EPIPE {
                    return handle_sigpipe();
                }
                e
            }
        };
    } else if fd == 1 || fd == 2 {
        match core::str::from_utf8(slice) {
            Ok(s) => {
                tprint!("{}", s);
                crate::print!("{}", s);
                crate::dprint!("{}", s);
            }
            Err(_) => {
                for &b in slice {
                    crate::print!("{}", b as char);
                    tprint!("{}", b as char);
                    crate::dprint!("{}", b as char);
                }
            }
        }
        if CONSOLE_KD_MODE.load(core::sync::atomic::Ordering::Relaxed) == 0 {
            crate::graphics::terminal::WRITER.lock().draw_cursor();
        }
        return count as u64;
    } else {
        EBADF
    }
}

pub(super) fn sys_open(arg1: u64, flags: u64, mode: u64) -> u64 {
    let path = match read_user_cstring(arg1) {
        Some(path) => path,
        None => return EFAULT,
    };
    let path_clone = path.clone();
    let result = open_path(path, flags, mode);
    crate::serial_macro::_print(format_args!("[OPEN] path='{}' flags={:#x} -> result={:#x}\n", path_clone, flags, result));
    result
}

/// The directory that will receive a new entry needs write+search on it.
///
/// Returns the errno to return when the caller may not create there
/// (`EACCES`), or `0` when the check passes or there is no current process
/// (kernel context, which is unconstrained).
fn check_create_dir(path: &str, creds: &crate::fs::perm::Creds) -> u64 {
    let clean = path.trim_start_matches('/');
    let parent = match clean.rfind('/') {
        Some(idx) if idx > 0 => &clean[..idx],
        _ => "/",
    };
    let parent_node = match crate::fs::lookup_checked(parent) {
        Ok(crate::fs::vfs::VNode::FsNode { node, .. }) => node,
        Err(crate::fs::LookupError::Denied) => return EACCES,
        Err(_) => return ENOENT,
        _ => return ENOTDIR,
    };
    let perms = crate::fs::perm::perms_of(&*parent_node);
    if crate::fs::perm::check(&perms, creds, crate::fs::perm::Access::Create).is_allow() {
        0
    } else {
        EACCES
    }
}

/// Shared open logic used by `open` and `openat`. `path` is already resolved
/// to an absolute or cwd-relative path string.
pub(super) fn open_path(path: alloc::string::String, flags: u64, _mode: u64) -> u64 {
    const O_CREAT: u64 = 0o100;
    const O_WRONLY: u64 = 0o1;
    const O_RDWR: u64 = 0o2;
    const O_EXCL: u64 = 0o200;
    const O_TRUNC: u64 = 0o1000;
    const O_DIRECTORY: u64 = 0o200000;

    let clean = path.trim_start_matches('/');
    if clean.is_empty() && path != "/" {
        return ENOENT;
    }

    // The caller's identity, fetched once: the access check below and, in the
    // create branch, the parent-directory write check, both need it.
    let creds = crate::fs::perm::current_creds();

    let vnode = match crate::fs::lookup_checked(&path) {
        Ok(v) => {
            if (flags & O_CREAT) != 0 && (flags & O_EXCL) != 0 {
                return EEXIST;
            }
            if (flags & O_DIRECTORY) != 0 {
                if let VNode::FsNode { node, .. } = &v {
                    if !node.is_dir() {
                        return ENOTDIR;
                    }
                }
            }
            // The access check, at open time as on Linux. The intent comes from
            // the flags — this is the only place that knows whether the caller
            // asked to read or to write — and O_TRUNC counts as a write even
            // when the file is also opened read-only, because it changes the
            // file's contents.
            if let VNode::FsNode { node, .. } = &v {
                let intent = if (flags & (O_WRONLY | O_RDWR | O_TRUNC)) != 0 {
                    crate::fs::perm::Access::Write
                } else {
                    crate::fs::perm::Access::Read
                };
                if let Some(creds) = creds.as_ref() {
                    let decision =
                        crate::fs::perm::check(&crate::fs::perm::perms_of(&**node), creds, intent);
                    if !decision.is_allow() {
                        return decision.errno();
                    }
                }
            }
            if cfg!(feature = "dsp_verbose") { crate::dprintln!("sys_open: '{}' -> OK", path); }
            // ponytail: O_TRUNC on an existing regular file empties it.
            // O_TRUNC on device nodes (/dev/null etc.) is a Linux no-op —
            // VNode::truncate errors ENOSYS there, which must not fail open.
            if (flags & O_TRUNC) != 0 {
                if let VNode::FsNode { node, .. } = &v {
                    if node.is_dir() {
                        return EISDIR;
                    }
                    if v.truncate(0).is_err() {
                        return EIO;
                    }
                }
            }
            v
        }
        // A directory on the path that this caller may not search is EACCES,
        // as on Linux — not ENOENT, which would say the file does not exist.
        // The distinction is reported, never swallowed: a caller that cannot
        // tell "you may not look" from "it is not there" acts on the wrong
        // one.
        Err(crate::fs::LookupError::Denied) => return EACCES,
        Err(crate::fs::LookupError::NotFound) => {
            if (flags & O_DIRECTORY) != 0 {
                return ENOENT;
            }
            if cfg!(feature = "dsp_verbose") { crate::dprintln!("sys_open: '{}' not found, flags={:#x}", path, flags); }
            if (flags & O_CREAT) != 0 {
                // The directory receiving the new entry needs w+x on it.
                if let Some(creds) = creds.as_ref() {
                    if check_create_dir(&path, creds) != 0 {
                        return EACCES;
                    }
                }
                match crate::fs::create(&path, false, _mode as u32 & 0o7777) {
                    Some(v) => v,
                    None => return ENOENT,
                }
            } else {
                return ENOENT;
            }
        }
    };
    crate::fs::mount::record_open(&path);
    let arc_vnode = Arc::new(SpinLock::new(crate::fs::vfs::OpenFile {
        vnode,
        flags,
        path: path.clone(),
    }));
    if cfg!(feature = "dsp_verbose") { crate::dprintln!("sys_open: path stored in OpenFile = '{}'", path); }
    // ponytail: pty open-description refcount — dup()/fork() share the
    // OpenFile (no hook needed there); last-close releases in sys_close,
    // sys_close_range and the tasks.rs exit teardown below.
    match &arc_vnode.lock().vnode {
        VNode::PtyMaster(pty) => crate::fs::pty::pty_acquire(pty, true),
        VNode::PtySlave(pty) => crate::fs::pty::pty_acquire(pty, false),
        _ => {}
    }
    // ponytail: Linux-like per-open evdev stream; keyed by OpenFile Arc ptr
    // so dup()/fork() share it like a shared file offset. Minor was resolved
    // at lookup and rides in the VNode — no re-parse, no TOCTOU.
    let input_minor = match &arc_vnode.lock().vnode {
        crate::fs::vfs::VNode::InputEventDevice { minor }
        | crate::fs::vfs::VNode::InputMiceDevice { minor } => Some(*minor),
        _ => None,
    };
    if let Some(dev) = input_minor {
        crate::drivers::keyboard::input_register(Arc::as_ptr(&arc_vnode) as usize, dev);
    }

    let thread = match crate::tasks::current_thread() {
        Some(t) => t,
        None => return EBADF,
    };
    let mut p = thread.process.lock();

    for (i, slot) in p.fd_table.iter_mut().enumerate() {
        if slot.is_none() {
            *slot = Some(arc_vnode);
            if cfg!(feature = "dsp_verbose") { crate::dprintln!("sys_open: pid={} '{}' -> fd={}", p.pid, path, i); }
            return i as u64;
        }
    }
    p.fd_table.push(Some(arc_vnode));
    let fd = (p.fd_table.len() - 1) as u64;
    if cfg!(feature = "dsp_verbose") { crate::dprintln!("sys_open: '{}' -> fd={}", path, fd); }
    fd
}

pub(super) fn resolve_dirfd_path(dirfd: i32, path: &str) -> Option<alloc::string::String> {
    const AT_FDCWD: i32 = -100;
    if path.starts_with('/') {
        return Some(crate::fs::canonicalize_path("/", path));
    }
    if dirfd == AT_FDCWD {
        let thread = crate::tasks::current_thread()?;
        let cwd = thread.process.lock().cwd.clone();
        return Some(crate::fs::canonicalize_path(&cwd, path));
    }
    if dirfd < 0 {
        return None;
    }
    let thread = crate::tasks::current_thread()?;
    let p = thread.process.lock();
    let slot = p.fd_table.get(dirfd as usize)?.as_ref()?;
    let file = slot.lock();
    let base = file.path.clone();
    drop(file);
    drop(p);
    if base.is_empty() || !base.starts_with('/') {
        return None;
    }
    Some(crate::fs::canonicalize_path(&base, path))
}

/// `openat(dirfd, path, flags, mode)`.
pub(super) fn sys_openat(dirfd: i32, path_ptr: u64, flags: u64, mode: u64) -> u64 {
    let path = match read_user_cstring(path_ptr) {
        Some(p) => p,
        None => return EFAULT,
    };
    if path.is_empty() {
        return ENOENT;
    }
    let full_path = match resolve_dirfd_path(dirfd, &path) {
        Some(p) => p,
        None => return EBADF,
    };
    open_path(full_path, flags, mode)
}

pub(super) fn sys_close(fd: usize) -> u64 {
    let proc = match crate::tasks::current_process() {
        Some(p) => p,
        None => return EBADF,
    };
    let vnode_arc = {
        let mut p = proc.lock();
        if fd < p.fd_table.len() {
            p.fd_table[fd].take()
        } else {
            None
        }
    };

    if let Some(v) = vnode_arc {
        let vnode_kind = { v.lock().vnode.clone() };
        if matches!(vnode_kind, VNode::Pipe { .. }) {
            wake_pipe_waiters();
        }
        // ponytail: last close of a pty side (dup/fork shares keep count>1):
        // clear the side so the peer's blocking read sees EOF, not sleep.
        if Arc::strong_count(&v) == 1 {
            match &vnode_kind {
                VNode::PtyMaster(pty) => crate::fs::pty::pty_release(pty, true),
                VNode::PtySlave(pty) => crate::fs::pty::pty_release(pty, false),
                _ => {}
            }
        }
        if matches!(vnode_kind, VNode::DspAudioDevice | VNode::SndPlaybackDevice) {
            crate::drivers::audio::vfs_node::dsp_flush();
        }
        // ponytail: last close only — fork()/dup() share the OpenFile, and a
        // spawn child's pre-exec cleanup must not unregister the parent's
        // evdev stream (Super+T deafness: drops + readers=0). Same guard as
        // pty above.
        if Arc::strong_count(&v) == 1
            && matches!(vnode_kind, crate::fs::vfs::VNode::InputEventDevice { .. } | crate::fs::vfs::VNode::InputMiceDevice { .. })
        {
            crate::drivers::keyboard::input_unregister(Arc::as_ptr(&v) as usize);
        }
        // mount open-count accounting for umount2 EBUSY
        {
            let of = v.lock();
            crate::fs::mount::record_close(&of.path);
        }
        return 0;
    }
    EBADF
}

/// Close a file descriptor this kernel opened on the client's behalf.
///
/// Narrow wrapper so drivers do not need `sys_close` itself: the fd-release
/// side effects above (pipe wakeups, pty EOF, input-stream unregistration) are
/// process-wide, so a driver that opened an fd internally and then failed the
/// ioctl must close it through the same path the client would have used.
pub fn close_opened_fd(fd: i32) {
    if fd >= 0 {
        let _ = sys_close(fd as usize);
    }
}

pub(super) fn sys_poll(fds: *mut PollFd, nfds: usize, timeout: i32) -> u64 {
    if nfds == 0 {
        return 0;
    }
    let bytes = match nfds.checked_mul(core::mem::size_of::<PollFd>()) {
        Some(bytes) => bytes,
        None => return EINVAL,
    };
    if !validate_user_write_range(fds as u64, bytes) {
        return EFAULT;
    }
    let fds: &mut [PollFd] = unsafe { core::slice::from_raw_parts_mut(fds, nfds) };
    poll_core(fds, timeout)
}

/// Slice-based poll core shared by poll/ppoll/select/pselect: `fds` may be
/// user memory (validated by the caller) or a kernel Vec. Never blocks with
/// locks held; parks on Keyboard/PipeWait/Input like the old inline loop.
fn poll_core(fds: &mut [PollFd], timeout: i32) -> u64 {
    const POLLIN: i16 = 0x001;
    const POLLOUT: i16 = 0x004;
    const POLLNVAL: i16 = 0x040;

    let thread = match crate::tasks::current_thread() {
        Some(t) => t,
        None => return ESRCH,
    };

    let deadline = if timeout > 0 {
        let now = crate::time::TICKS.load(core::sync::atomic::Ordering::Relaxed) as u64;
        now.saturating_add(timeout as u64)
    } else {
        u64::MAX
    };

    loop {
        let mut ready = 0usize;
        let mut watch_stdin = false;
        let mut watch_pipe = false;
        let mut watch_input = false;
        // ponytail: first input fd's (reader, dev) for the park below; last
        // wins if several event fds are polled (same as Linux: one waiter per
        // fd, we park once — levels stay correct, we re-poll on wakeup).
        let mut watch_input_reader: Option<(usize, usize)> = None;
        for entry in fds.iter_mut() {
            let mut revents = 0i16;
            let efd = entry.fd;
            if efd < 0 {
                entry.revents = 0;
                continue;
            }
            let efd = efd as usize;

            let pid = thread.process.lock().pid;
            if let Some(poll_rev) = epoll::check_poll_fd(pid, entry.fd, entry.events) {
                revents = poll_rev;
                watch_pipe = true;
            } else {
                let vnode_opt = {
                    let p = thread.process.lock();
                    p.fd_table.get(efd).cloned().flatten()
                };
                match vnode_opt {
                    None => {
                        if efd == 0 {
                            if entry.events & POLLIN != 0 {
                                watch_stdin = true;
                                if !STDIN_BUFFER.lock().is_empty() {
                                    revents |= POLLIN;
                                }
                            }
                        } else if efd == 1 || efd == 2 {
                            if entry.events & POLLOUT != 0 {
                                revents |= POLLOUT;
                            }
                        } else {
                            revents |= POLLNVAL;
                        }
                    }
                    Some(v) => {
                        let vn = v.lock();
                        match &vn.vnode {
                            VNode::Pipe { shared, end, .. } => {
                                watch_pipe = true;
                                let pb = shared.lock();
                                if *end == crate::fs::vfs::PipeEnd::Read {
                                    if entry.events & POLLIN != 0
                                        && (!pb.buffer.is_empty()
                                            || Arc::strong_count(&pb.write_token) == 1)
                                    {
                                        revents |= POLLIN;
                                    }
                                } else if entry.events & POLLOUT != 0
                                    && Arc::strong_count(&pb.read_token) > 1
                                    && pb.buffer.len() < pb.capacity
                                {
                                    revents |= POLLOUT;
                                }
                            }
                            VNode::UnixSocket(sock) => {
                                let (r, w) = crate::arch::x86_64::syscalls::net::sock_readiness(sock);
                                if entry.events & POLLIN != 0 {
                                    if r {
                                        revents |= POLLIN;
                                    } else {
                                        watch_pipe = true;
                                    }
                                }
                                if entry.events & POLLOUT != 0 {
                                    if w {
                                        revents |= POLLOUT;
                                    } else {
                                        watch_pipe = true;
                                    }
                                }
                            }
                            VNode::InetSocket(inet) => {
                                let (r, w) = crate::arch::x86_64::syscalls::net::inet_readiness(inet);
                                if entry.events & POLLIN != 0 {
                                    if r {
                                        revents |= POLLIN;
                                    } else {
                                        watch_pipe = true;
                                    }
                                }
                                if entry.events & POLLOUT != 0 {
                                    if w {
                                        revents |= POLLOUT;
                                    } else {
                                        watch_pipe = true;
                                    }
                                }
                            }
                            VNode::UdpSocket(udp) => {
                                let (r, w) = crate::arch::x86_64::syscalls::net::udp_readiness(udp);
                                if entry.events & POLLIN != 0 {
                                    if r {
                                        revents |= POLLIN;
                                    } else {
                                        watch_pipe = true;
                                    }
                                }
                                if entry.events & POLLOUT != 0 {
                                    if w {
                                        revents |= POLLOUT;
                                    } else {
                                        watch_pipe = true;
                                    }
                                }
                            }
                            VNode::EventFd(e) => {
                                if entry.events & POLLIN != 0 {
                                    if e.lock().counter > 0 {
                                        revents |= POLLIN;
                                    } else {
                                        watch_pipe = true;
                                    }
                                }
                                if entry.events & POLLOUT != 0 {
                                    revents |= POLLOUT;
                                }
                            }
                            VNode::TimerFd(t) => {
                                let mut st = t.lock();
                                st.update_expirations(super::epoll::monotonic_now_ns());
                                if entry.events & POLLIN != 0 {
                                    if st.expirations > 0 {
                                        revents |= POLLIN;
                                    } else {
                                        watch_pipe = true;
                                    }
                                }
                            }
                            VNode::DrmDevice { .. } => {
                                // Event channel: readable only when flip/vblank
                                // events are queued (compositor repaint heartbeat).
                                if entry.events & POLLIN != 0
                                    && crate::drivers::drm::events_pending()
                                {
                                    revents |= POLLIN;
                                }
                            }
                            VNode::InputEventDevice { minor } | VNode::InputMiceDevice { minor } => {
                                // Readable only while this open's evdev stream
                                // is queued; otherwise park on the input queue.
                                if entry.events & POLLIN != 0 {
                                    let key = Arc::as_ptr(&v) as usize;
                                    if crate::drivers::keyboard::input_pending(key, *minor) {
                                        revents |= POLLIN;
                                    } else {
                                        watch_input = true;
                                        watch_input_reader = Some((key, *minor));
                                    }
                                }
                            }
                            VNode::PtyMaster(pty) => {
                                let (r, w) = crate::fs::pty::pty_master_readiness(pty);
                                if entry.events & POLLIN != 0 {
                                    if r {
                                        revents |= POLLIN;
                                    } else {
                                        watch_pipe = true;
                                    }
                                }
                                if entry.events & POLLOUT != 0 {
                                    if w {
                                        revents |= POLLOUT;
                                    } else {
                                        watch_pipe = true;
                                    }
                                }
                            }
                            VNode::PtySlave(pty) => {
                                let (r, w) = crate::fs::pty::pty_slave_readiness(pty);
                                if entry.events & POLLIN != 0 {
                                    if r {
                                        revents |= POLLIN;
                                    } else {
                                        watch_pipe = true;
                                    }
                                }
                                if entry.events & POLLOUT != 0 {
                                    if w {
                                        revents |= POLLOUT;
                                    } else {
                                        watch_pipe = true;
                                    }
                                }
                            }
                            _ => {
                                if entry.events & POLLIN != 0 {
                                    revents |= POLLIN;
                                }
                                if entry.events & POLLOUT != 0 {
                                    revents |= POLLOUT;
                                }
                            }
                        }
                    }
                }
            }
            entry.revents = revents;
            if revents != 0 {
                ready += 1;
            }
        }

        if ready > 0 {
            return ready as u64;
        }
        if timeout == 0 {
            return 0;
        }
        if timeout > 0 {
            let now = crate::time::TICKS.load(core::sync::atomic::Ordering::Relaxed) as u64;
            if now >= deadline {
                return 0;
            }
        }

        // Check for pending signals before sleeping (EINTR)
        //
        // Mesa's wl_display_dispatch_queue() calls poll(&pfd, 1, -1) waiting
        // for wl_callback.done, and does NOT retry on EINTR — it immediately
        // returns VK_ERROR_DEVICE_LOST (-4). For infinite-timeout polls,
        // loop back and re-check fds instead of failing. For bounded polls,
        // return EINTR so userspace can retry.
        {
            let p = thread.process.lock();
            let ready_sigs = (thread.pending | p.pending) & !thread.sigmask;
            if ready_sigs != 0 && timeout >= 0 {
                return (-4i64) as u64; // EINTR
            }
        }

        if watch_pipe || watch_stdin {
            block_current_thread(PendingBlock::EpollWait { deadline });
        } else if watch_input {
            if timeout > 0 {
                block_current_thread(PendingBlock::EpollWait { deadline });
            } else {
                let (reader, dev) = watch_input_reader.unwrap_or((0, 0));
                block_current_thread(PendingBlock::Input { reader, dev });
            }
        } else if timeout > 0 {
            // No active fds to watch, but timeout > 0 requested (e.g. poll-as-sleep)
            block_current_thread(PendingBlock::Sleep { deadline });
        } else {
            return 0;
        }
    }
}

/// 271 — ponytail: ppoll == poll with timespec timeout; sigmask ignored
/// (no RT signal delivery to mask). Timeout semantics match sys_poll.
pub(super) fn sys_ppoll(fds: *mut PollFd, nfds: usize, ts_ptr: u64, _sigmask: u64) -> u64 {
    let timeout: i32 = if ts_ptr == 0 {
        -1
    } else {
        if !validate_user_read_range(ts_ptr, 16) {
            return EFAULT;
        }
        let sec = unsafe { *(ts_ptr as *const i64) };
        let nsec = unsafe { *((ts_ptr + 8) as *const i64) };
        if sec < 0 || nsec < 0 || nsec >= 1_000_000_000 {
            return EINVAL;
        }
        sec.saturating_mul(1000)
            .saturating_add(nsec / 1_000_000)
            .min(i32::MAX as i64) as i32
    };
    sys_poll(fds, nfds, timeout)
}

/// 23: select(nfds, readfds, writefds, exceptfds, timeval*).
/// ponytail: thin fd_set translator over poll_core. exceptfds is validated
/// and zeroed but otherwise ignored (no OOB/priority data anywhere).
/// timeval is read-only (Linux writes back remaining time; we don't).
/// Empty fd set returns 0 at once instead of sleeping (nothing uses
/// select-as-sleep in-guest; revisit if a configure probe hangs on it).
pub(super) fn sys_select(nfds: usize, readfds: u64, writefds: u64, exceptfds: u64, tv: u64) -> u64 {
    const FD_SETSIZE: usize = 1024;
    if nfds > FD_SETSIZE {
        return EINVAL;
    }
    let timeout: i32 = if tv == 0 {
        -1
    } else {
        if !validate_user_read_range(tv, 16) {
            return EFAULT;
        }
        let sec = unsafe { *(tv as *const i64) };
        let usec = unsafe { *((tv + 8) as *const i64) };
        if sec < 0 || usec < 0 || usec >= 1_000_000 {
            return EINVAL;
        }
        sec.saturating_mul(1000)
            .saturating_add(usec / 1000)
            .min(i32::MAX as i64) as i32
    };
    select_common(nfds, readfds, writefds, exceptfds, timeout)
}

/// 270: pselect6(nfds, readfds, writefds, exceptfds, timespec*, sigmask*).
/// ponytail: same as select with timespec timeout; sigmask ignored (no RT
/// signal delivery to mask — signals here are either pending or not).
pub(super) fn sys_pselect6(
    nfds: usize,
    readfds: u64,
    writefds: u64,
    exceptfds: u64,
    ts: u64,
    _sigmask: u64,
) -> u64 {
    const FD_SETSIZE: usize = 1024;
    if nfds > FD_SETSIZE {
        return EINVAL;
    }
    let timeout: i32 = if ts == 0 {
        -1
    } else {
        if !validate_user_read_range(ts, 16) {
            return EFAULT;
        }
        let sec = unsafe { *(ts as *const i64) };
        let nsec = unsafe { *((ts + 8) as *const i64) };
        if sec < 0 || nsec < 0 || nsec >= 1_000_000_000 {
            return EINVAL;
        }
        sec.saturating_mul(1000)
            .saturating_add(nsec / 1_000_000)
            .min(i32::MAX as i64) as i32
    };
    select_common(nfds, readfds, writefds, exceptfds, timeout)
}

fn select_common(nfds: usize, readfds: u64, writefds: u64, exceptfds: u64, timeout: i32) -> u64 {
    const POLLIN: i16 = 0x001;
    const POLLOUT: i16 = 0x004;
    const POLLERR: i16 = 0x008;
    const POLLHUP: i16 = 0x010;
    const POLLNVAL: i16 = 0x040;

    let set_bytes = (nfds + 7) / 8;
    let mut rbits = [0u8; 128];
    let mut wbits = [0u8; 128];
    if readfds != 0 {
        if !validate_user_read_range(readfds, set_bytes) {
            return EFAULT;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(
                readfds as *const u8,
                rbits.as_mut_ptr(),
                set_bytes,
            );
        }
        if !validate_user_write_range(readfds, set_bytes) {
            return EFAULT;
        }
    }
    if writefds != 0 {
        if !validate_user_read_range(writefds, set_bytes) {
            return EFAULT;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(
                writefds as *const u8,
                wbits.as_mut_ptr(),
                set_bytes,
            );
        }
        if !validate_user_write_range(writefds, set_bytes) {
            return EFAULT;
        }
    }
    // exceptfds: validated + zeroed on return, contents ignored.
    if exceptfds != 0 {
        if !validate_user_read_range(exceptfds, set_bytes)
            || !validate_user_write_range(exceptfds, set_bytes)
        {
            return EFAULT;
        }
    }

    let bit = |bits: &[u8; 128], fd: usize| -> bool {
        (bits[fd / 8] >> (fd % 8)) & 1 == 1
    };
    let mut pfds = alloc::vec::Vec::new();
    for fd in 0..nfds {
        let mut ev = 0i16;
        if readfds != 0 && bit(&rbits, fd) {
            ev |= POLLIN;
        }
        if writefds != 0 && bit(&wbits, fd) {
            ev |= POLLOUT;
        }
        if ev != 0 {
            pfds.push(PollFd {
                fd: fd as i32,
                events: ev,
                revents: 0,
            });
        }
    }

    let ret = poll_core(&mut pfds, timeout);
    // Note: poll_core's only error is ESRCH (no current thread), unreachable
    // mid-syscall, so `ret` is always the ready count here.

    for p in &pfds {
        let fd = p.fd as usize;
        let set = |bits: &mut [u8; 128]| {
            bits[fd / 8] |= 1 << (fd % 8);
        };
        let clr = |bits: &mut [u8; 128]| {
            bits[fd / 8] &= !(1 << (fd % 8));
        };
        if readfds != 0 {
            if p.revents & (POLLIN | POLLERR | POLLHUP | POLLNVAL) != 0 {
                set(&mut rbits);
            } else {
                clr(&mut rbits);
            }
        }
        if writefds != 0 {
            if p.revents & (POLLOUT | POLLERR | POLLHUP | POLLNVAL) != 0 {
                set(&mut wbits);
            } else {
                clr(&mut wbits);
            }
        }
    }
    if readfds != 0 {
        unsafe {
            core::ptr::copy_nonoverlapping(
                rbits.as_ptr(),
                readfds as *mut u8,
                set_bytes,
            );
        }
    }
    if writefds != 0 {
        unsafe {
            core::ptr::copy_nonoverlapping(
                wbits.as_ptr(),
                writefds as *mut u8,
                set_bytes,
            );
        }
    }
    if exceptfds != 0 {
        unsafe {
            core::ptr::write_bytes(exceptfds as *mut u8, 0, set_bytes);
        }
    }
    ret
}

pub(super) fn sys_lseek(fd: usize, offset: isize, whence: usize) -> u64 {
    if fd > 2 {
        let vnode_opt = {
            let proc = match crate::tasks::current_process() {
                Some(p) => p,
                None => return EBADF,
            };
            let p = proc.lock();
            p.fd_table.get(fd).cloned().flatten()
        };

        if let Some(vnode_arc) = vnode_opt {
            let mut open_file = vnode_arc.lock();
            let ret = open_file.vnode.lseek(offset, whence) as u64;
            return ret;
        }
    }
    EBADF
}


/// evdev ioctls for /dev/input/eventN — the subset libevdev/libinput probe
/// at device init. Bitmaps come from this node's INPUT_CAPS (caps-driven,
// never dev== branches), so hotplug nodes describe what they actually emit.
fn input_evdev_ioctl(cmd: u64, argp: u64, dev: usize, reader_key: usize) -> u64 {
    use crate::memory::space::{validate_user_read_range, validate_user_write_range};
    const EVTY: u64 = 0x45; // 'E'
    let ty = (cmd >> 8) & 0xFF;
    let nr = (cmd & 0xFF) as usize;
    let size = ((cmd >> 16) & 0x3FFF) as usize;
    if ty != EVTY {
        return super::ENOTTY;
    }

    // Copy `bytes` into the user buffer, clamped to the ioctl's size field.
    fn put(argp: u64, cap: usize, bytes: &[u8]) -> u64 {
        let n = bytes.len().min(cap);
        if !validate_user_write_range(argp, n) {
            return EFAULT;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), argp as *mut u8, n);
        }
        0
    }

    match nr {
        0x01 => {
            // EVIOCGVERSION: EV_VERSION = (1,0,1)
            put(argp, size, &((1u32 << 16) | 1).to_le_bytes())
        }
        0x02 => {
            // EVIOCGID: struct input_id { bustype, vendor, product, version }
            // BUS_VIRTUAL(6); a synthetic aggregate of PS/2 + USB HID.
            let id: [u8; 8] = [6, 0, 0x01, 0, 0x01, 0, 1, 0];
            put(argp, size, &id)
        }
        0x06 => {
            // EVIOCGNAME per node (NUL-terminated) so logs tell streams apart.
            match crate::drivers::keyboard::INPUT_CAPS.lock().get(&dev).copied() {
                Some(caps) => {
                    let mut buf = [0u8; 32];
                    let n = caps.name.len().min(31);
                    buf[..n].copy_from_slice(&caps.name.as_bytes()[..n]);
                    put(argp, size, &buf[..n + 1])
                }
                None => super::ENOENT,
            }
        }
        0x09 => put(argp, size, &[0u8; 4]),                    // EVIOCGEFFECTS
        0x0A => put(argp, size, &[0u8; 16]),                   // EVIOCGMTSLOTS
        0x07 | 0x08 => put(argp, size, b"\x00"),               // PHYS / UNIQ
        0x0C => put(argp, size, &[0u8; 16]),                   // EVIOCGPROP
        0x18 => {
            // EVIOCGKEY: return shadow pressed key bitmap
            let key_state = match crate::drivers::keyboard::INPUT_DEV.lock().get(&dev) {
                Some(d) => d.key_state,
                None => return super::ENOENT,
            };
            put(argp, size, &key_state)
        }
        0x19 => {
            // EVIOCGLED: return shadow LED state
            let led_state = match crate::drivers::keyboard::INPUT_DEV.lock().get(&dev) {
                Some(d) => d.led_state,
                None => return super::ENOENT,
            };
            put(argp, size, &[led_state])
        }
        0x1A => {
            // EVIOCLED / EVIOCSLED: set LED state
            if !validate_user_read_range(argp, 1) {
                return EFAULT;
            }
            let val = unsafe { *(argp as *const u8) };
            let mut map = crate::drivers::keyboard::INPUT_DEV.lock();
            if let Some(d) = map.get_mut(&dev) {
                d.led_state = val;
                0
            } else {
                super::ENOENT
            }
        }
        0x1B => put(argp, size, &[0u8; 8]), // SND / SW
        // EVIOCGRAB: _IOW('E', 0x90, int) == 0x40044590. The decoder above
        // takes nr = cmd & 0xFF, so this arm must key on 0x90 — it used to
        // match 0x28, which is EVIOCGBIT(8, len) with an invalid EV type, so
        // a real EVIOCGRAB fell through to ENOTTY and `grabbed_by` was
        // unreachable dead code.
        0x90 => {
            // Linux always passes a 4-byte int by pointer; anything narrower
            // is malformed. Reading the pointer *value* as the flag (the old
            // behaviour for size < 4) would make a short ioctl arbitrarily
            // grab or ungrab.
            if size < 4 {
                return EINVAL;
            }
            if !validate_user_read_range(argp, 4) {
                return EFAULT;
            }
            let val = unsafe { *(argp as *const i32) };

            let mut map = crate::drivers::keyboard::INPUT_DEV.lock();
            let Some(d) = map.get_mut(&dev) else { return super::ENOENT };
            if val != 0 {
                // Request grab
                if let Some(other) = d.grabbed_by {
                    if other != reader_key {
                        return super::EBUSY;
                    }
                }
                d.grabbed_by = Some(reader_key);
            } else {
                // Release grab
                if d.grabbed_by == Some(reader_key) {
                    d.grabbed_by = None;
                }
            }
            0
        }
        0x20..=0x3F => {
            // EVIOCGBIT(type, len): type = nr - 0x20, served from INPUT_CAPS.
            let etype = nr - 0x20;
            let caps = match crate::drivers::keyboard::INPUT_CAPS.lock().get(&dev).copied() {
                Some(c) => c,
                None => return super::ENOENT,
            };
            let mut buf = [0u8; 256];
            let set = |buf: &mut [u8], bit: usize| {
                if bit / 8 < buf.len() {
                    buf[bit / 8] |= 1 << (bit % 8);
                }
            };
            match etype {
                0 => {
                    // Event types present, straight from the caps ev mask.
                    for bit in 0..64usize {
                        if caps.ev & (1 << bit) != 0 {
                            set(&mut buf, bit);
                        }
                    }
                }
                0x01 => {
                    // Key/button bitmap, copied byte-wise from caps.
                    buf[..caps.key.len()].copy_from_slice(&caps.key);
                }
                0x02 => {
                    // REL axes from caps; nonzero rel implies REL_WHEEL=8
                    // (matches the mouse_caps encoding).
                    for bit in 0..8usize {
                        if caps.rel & (1 << bit) != 0 {
                            set(&mut buf, bit);
                        }
                    }
                    if caps.rel != 0 {
                        set(&mut buf, 8);
                    }
                }
                0x03 => {
                    // ABS axes from caps
                    for bit in 0..8usize {
                        if caps.abs & (1 << bit) != 0 {
                            set(&mut buf, bit);
                        }
                    }
                }
                _ => {}
            }
            put(argp, size, &buf)
        }
        0x40..=0x5F => {
            // EVIOCGABS(abs): abs = nr - 0x40
            let abs = nr - 0x40;
            if abs >= 8 {
                return super::EINVAL;
            }
            let caps = match crate::drivers::keyboard::INPUT_CAPS.lock().get(&dev).copied() {
                Some(c) => c,
                None => return super::ENOENT,
            };
            if caps.abs & (1 << abs) == 0 {
                return super::EINVAL;
            }
            let val = {
                let map = crate::drivers::keyboard::INPUT_DEV.lock();
                map.get(&dev).map(|d| d.abs_vals[abs]).unwrap_or(0)
            };
            let (min, max, fuzz, flat, res) = (
                caps.abs_info[abs].1,
                caps.abs_info[abs].2,
                caps.abs_info[abs].3,
                caps.abs_info[abs].4,
                caps.abs_info[abs].5,
            );
            // struct input_absinfo { value: i32, minimum: i32, maximum: i32, fuzz: i32, flat: i32, resolution: i32 }
            let mut buf = [0u8; 24];
            buf[0..4].copy_from_slice(&val.to_le_bytes());
            buf[4..8].copy_from_slice(&min.to_le_bytes());
            buf[8..12].copy_from_slice(&max.to_le_bytes());
            buf[12..16].copy_from_slice(&fuzz.to_le_bytes());
            buf[16..20].copy_from_slice(&flat.to_le_bytes());
            buf[20..24].copy_from_slice(&res.to_le_bytes());
            put(argp, size, &buf)
        }
        0x91 => {
            // EVIOCREVOKE: revoke device access
            0
        }
        0xA0 => {
            // EVIOCSCLOCKID: set clockid
            0
        }
        _ => super::ENOTTY,
    }
}

pub(super) fn sys_ioctl(fd: usize, cmd: u64, argp: u64) -> u64 {
    let arc_opt = {
        if let Some(proc) = crate::tasks::current_process() {
            let p = proc.lock();
            p.fd_table.get(fd).cloned().flatten()
        } else {
            None
        }
    };

    if let Some(arc_vnode) = &arc_opt {
        let open_file = arc_vnode.lock();
        match &open_file.vnode {
            crate::fs::vfs::VNode::DspAudioDevice
            | crate::fs::vfs::VNode::SndControlDevice
            | crate::fs::vfs::VNode::SndPlaybackDevice
            | crate::fs::vfs::VNode::SndCaptureDevice => {
                drop(open_file);
                return crate::drivers::audio::vfs_node::dsp_ioctl(cmd, argp);
            }
            crate::fs::vfs::VNode::DrmDevice { .. } => {
                drop(open_file);
                return crate::drivers::drm::drm_ioctl(cmd, argp);
            }
            crate::fs::vfs::VNode::DmaBuf(_) => {
                drop(open_file);
                if cmd == drm_uapi::DMA_BUF_IOCTL_SYNC {
                    if !validate_user_read_range(argp, 8) {
                        return EFAULT;
                    }
                    let flags = unsafe { core::ptr::read_unaligned(argp as *const u64) };
                    const SYNC_MASK: u64 = (drm_uapi::DMA_BUF_SYNC_RW
                        | drm_uapi::DMA_BUF_SYNC_START
                        | drm_uapi::DMA_BUF_SYNC_END) as u64;
                    if flags & !SYNC_MASK != 0 {
                        return EINVAL;
                    }
                    return 0; // memory is shared/coherent; sync is a no-op
                }
                return super::ENOTTY;
            }
            crate::fs::vfs::VNode::InputEventDevice { minor }
            | crate::fs::vfs::VNode::InputMiceDevice { minor } => {
                // ponytail: per-device caps from the live table; node freed
                // since open → ENOENT, never a stale alias.
                let minor = *minor;
                let reader_key = Arc::as_ptr(arc_vnode) as usize;
                drop(open_file);
                return input_evdev_ioctl(cmd, argp, minor, reader_key);
            }
            crate::fs::vfs::VNode::Pipe { .. } => {
                drop(open_file);
                if cmd == 0x541B { // FIONREAD
                    return 0; // return 0 bytes available for now
                }
                return super::EINVAL; // fallback to fcntl
            }
            _ => {}
        }
    }

    // Framebuffer ioctls: FBIOGET_VSCREENINFO = 0x4600, FBPUT_VSCREENINFO = 0x4601, FBIOGET_FSCREENINFO = 0x4602
    if cmd == 0x4600 || cmd == 0x4601 {
        if !validate_user_write_range(argp, 28) {
            return EFAULT;
        }
        let (w, h, stride) = unsafe {
            crate::graphics::FRAMEBUFFER_INFO
                .map(|fb| (fb.width as u32, fb.height as u32, fb.stride as u32))
                .unwrap_or((1280, 800, 1280))
        };
        let ptr = argp as *mut u32;
        unsafe {
            core::ptr::write_bytes(argp as *mut u8, 0, 28);
            *ptr.add(0) = w;      // xres
            *ptr.add(1) = h;      // yres
            *ptr.add(2) = stride; // xres_virtual
            *ptr.add(3) = h;      // yres_virtual
            *ptr.add(6) = 32;     // bits_per_pixel
        }
        return 0;
    }
    if cmd == 0x4602 {
        if !validate_user_write_range(argp, 80) {
            return EFAULT;
        }
        let line_len = unsafe {
            crate::graphics::FRAMEBUFFER_INFO
                .map(|fb| (fb.stride * 4) as u32)
                .unwrap_or(1280 * 4)
        };
        let ptr = argp as *mut u32;
        unsafe {
            core::ptr::write_bytes(argp as *mut u8, 0, 80);
            *ptr.add(12) = line_len; // line_length / pitch
        }
        return 0;
    }

    // TIOCGPTN = 0x80045430 (Get PTY number)
    if cmd == 0x80045430 {
        if !validate_user_write_range(argp, core::mem::size_of::<u32>()) {
            return EFAULT;
        }
        if let Some(arc_vnode) = &arc_opt {
            let of = arc_vnode.lock();
            if let crate::fs::vfs::VNode::PtyMaster(pty) = &of.vnode {
                let id = pty.lock().id;
                unsafe {
                    *(argp as *mut u32) = id;
                }
                return 0;
            }
        }
        return super::ENOTTY;
    }
    // TIOCSPTLCK = 0x40045431 (Lock/unlock PTY slave)
    if cmd == 0x40045431 {
        if !validate_user_read_range(argp, core::mem::size_of::<i32>()) {
            return EFAULT;
        }
        let lock_val = unsafe { *(argp as *const i32) };
        if let Some(arc_vnode) = &arc_opt {
            let of = arc_vnode.lock();
            if let crate::fs::vfs::VNode::PtyMaster(pty) = &of.vnode {
                pty.lock().locked = lock_val != 0;
                return 0;
            }
        }
        return super::ENOTTY;
    }
    // TIOCGPTPEER = 0x5441 (Safely open the slave side of a pty master).
    // Linux returns a fresh fd for the peer; argp holds open flags.
    // OpenSSH >= 9.x uses this as its primary slave-open path — without it
    // sshd's pty_allocate fails and the session is torn down.
    if cmd == 0x5441 {
        // ponytail: _IO — argp IS the flags value (O_RDWR|O_NOCTTY...), not
        // a pointer. Reading it as a pointer EFAULTs (sshd passes ~0).
        let peer_flags = argp & 0xFFFF;
        let pty = if let Some(arc_vnode) = &arc_opt {
            let of = arc_vnode.lock();
            match &of.vnode {
                crate::fs::vfs::VNode::PtyMaster(pty) => pty.clone(),
                _ => return super::ENOTTY,
            }
        } else {
            return super::EBADF;
        };
        // ponytail: same open-description setup as open_path's slave branch.
        // Locks are sequential, never nested: of-guard dropped above, then
        // pty (leaf), then CPUS (current_thread) + process (innermost).
        // ponytail: path must be the real /dev/pts/<id> — glibc ttyname()
        // resolves it via /proc/self/fd readlink; a fake path breaks sshd's
        // controlling-tty setup and the session dies.
        let peer_id = pty.lock().id;
        let slave = Arc::new(SpinLock::new(crate::fs::vfs::OpenFile {
            vnode: crate::fs::vfs::VNode::PtySlave(pty.clone()),
            flags: peer_flags,
            path: alloc::format!("/dev/pts/{}", peer_id),
        }));
        crate::fs::pty::pty_acquire(&pty, false);
        let thread = match crate::tasks::current_thread() {
            Some(t) => t,
            None => return super::EBADF,
        };
        let mut p = thread.process.lock();
        for (i, slot) in p.fd_table.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(slave);
                return i as u64;
            }
        }
        p.fd_table.push(Some(slave));
        return (p.fd_table.len() - 1) as u64;
    }
    // TIOCPKT = 0x5420 / TIOCGPKT = 0x80045438
    if cmd == 0x5420 || cmd == 0x80045438 {
        if let Some(arc_vnode) = &arc_opt {
            let of = arc_vnode.lock();
            if let crate::fs::vfs::VNode::PtyMaster(pty) = &of.vnode {
                if cmd == 0x80045438 {
                    if !validate_user_write_range(argp, core::mem::size_of::<i32>()) {
                        return EFAULT;
                    }
                    unsafe { *(argp as *mut i32) = if pty.lock().packet_mode { 1 } else { 0 }; }
                } else {
                    if !validate_user_read_range(argp, core::mem::size_of::<i32>()) {
                        return EFAULT;
                    }
                    pty.lock().packet_mode = unsafe { *(argp as *const i32) } != 0;
                }
                return 0;
            }
        }
        return super::ENOTTY;
    }
    /// Which VT an open fd names, if it names a terminal.
///
/// Used by `TIOCSCTTY`, which receives a *fd*, not a path — resolving the VT
/// has to go through the fd table, and a `TtyDevice { vt: 0 }` (the `/dev/tty`
/// alias) resolves to whichever VT is active at the moment of the call, which
/// is the whole point of that alias.
fn current_tty_vt_of(fd: usize) -> Option<u16> {
    let t = crate::tasks::current_thread()?;
    let proc = t.process.lock();
    let f = proc.fd_table.get(fd)?.as_ref()?;
    let g = f.lock();
    match &g.vnode {
        VNode::TtyDevice { vt } => Some(if *vt == 0 {
            crate::graphics::vt::active_vt_number()
        } else {
            *vt
        }),
        // A pty counts as a terminal too: sshd and zellij set the controlling
        // terminal on a pty slave, and refusing would break every such program.
        VNode::PtySlave(pty) => {
            let id = pty.lock().id;
            Some(1000 + id as u16)
        }
        _ => None,
    }
}

// TIOCSCTTY = 0x540E (Set controlling tty)
    //
    // This used to `return 0` without recording anything, so a session could
    // believe it had a controlling terminal and never be told otherwise — and
    // `ISIG` had no group to signal. The arg is an fd to open the tty on; we
    // record the VT that fd names.
    if cmd == 0x540E {
        // Linux's prototype is `ioctl(fd, TIOCSCTTY, arg)` where *arg* is the
        // terminal's own file descriptor, not a flag word. So the fd to claim
        // comes from the third argument, not from the dispatcher's fd.
        let tty_fd = argp as usize;
        let vt = match current_tty_vt_of(tty_fd) {
            Some(v) => v,
            None => return ENOTTY,
        };
        // Linux: a session has at most one controlling terminal. Claiming a
        // second one is refused unless the same session already owns it.
        let me_sid = with_current_process(|p| p.sid);
        let owner = with_current_process(|p| p.ctty_owner_sid);
        if owner != 0 && owner != me_sid {
            return crate::arch::x86_64::syscalls::EPERM;
        }
        // The claiming process must be a session leader without a controlling
        // terminal, or the terminal's foreground group is unset.
        with_current_process(|p| {
            p.ctty_vt = vt;
            p.ctty_owner_sid = p.sid;
        });
        // Make this group the terminal's foreground group.
        crate::tty::set_fg_pgid(vt, with_current_process(|p| p.pgid as i32));
        return 0;
    }
    // TIOCNOTTY = 0x5422
    //
    // Releases the calling process's controlling terminal. Linux then blocks
    // further reads from its tty, because there is no foreground group to
    // check against; we drop the association and let the tty default to
    // "no job control" rather than synthesising SIGTTIN on every read.
    if cmd == 0x5422 {
        with_current_process(|p| {
            p.ctty_vt = 0;
            p.ctty_owner_sid = 0;
        });
        return 0;
    }

    // TCGETS = 0x5401
    if cmd == 0x5401 {
        // Standard Linux 36-byte struct termios
        if !validate_user_write_range(argp, 36) {
            return EFAULT;
        }
        // A TtyDevice carries per-VT termios, so TCGETS on /dev/tty3 returns VT 3's
        // settings rather than a discarded default -- which is what a client
        // needs before it can make an informed decision about raw mode.
        if let Some(arc_vnode) = &arc_opt {
            let of = arc_vnode.lock();
            if let crate::fs::vfs::VNode::TtyDevice { vt } = &of.vnode {
                let vt = *vt;
                drop(of);
                let termios = crate::tty::termios(vt);
                unsafe {
                    core::ptr::write(argp as *mut crate::fs::pty::Termios, termios);
                }
                return 0;
            }
        }
        let termios = if let Some(arc_vnode) = &arc_opt {
            let of = arc_vnode.lock();
            match &of.vnode {
                crate::fs::vfs::VNode::PtyMaster(pty) | crate::fs::vfs::VNode::PtySlave(pty) => {
                    pty.lock().termios
                }
                _ => crate::fs::pty::Termios::default(),
            }
        } else {
            crate::fs::pty::Termios::default()
        };

        unsafe {
            let ptr = argp as *mut crate::fs::pty::Termios;
            core::ptr::write(ptr, termios);
        }
        return 0;
    }
    // TCSETS = 0x5402, TCSETSW = 0x5403, TCSETSF = 0x5404
    if cmd == 0x5402 || cmd == 0x5403 || cmd == 0x5404 {
        if !validate_user_read_range(argp, 36) {
            return EFAULT;
        }
        // TCSETS on a VT must actually take effect. It previously fell through
        // the `_ => {}` arm and was accepted-and-ignored, so a client could
        // never put a VT into raw mode -- which is the first thing every
        // full-screen program does, and the reason one could not run here.
        if let Some(arc_vnode) = &arc_opt {
            let of = arc_vnode.lock();
            if let crate::fs::vfs::VNode::TtyDevice { vt } = &of.vnode {
                let vt = *vt;
                drop(of);
                let new_termios = unsafe { core::ptr::read(argp as *const crate::fs::pty::Termios) };
                crate::tty::set_termios(vt, new_termios);
                return 0;
            }
        }
        if let Some(arc_vnode) = &arc_opt {
            let of = arc_vnode.lock();
            match &of.vnode {
                crate::fs::vfs::VNode::PtyMaster(pty) | crate::fs::vfs::VNode::PtySlave(pty) => {
                    let new_termios = unsafe { core::ptr::read(argp as *const crate::fs::pty::Termios) };
                    pty.lock().termios = new_termios;
                }
                _ => {}
            }
        }
        return 0;
    }
    // TCGETS2 = 0x802C542A
    if cmd == 0x802C542A {
        if !validate_user_write_range(argp, 44) {
            return EFAULT;
        }
        let termios = if let Some(arc_vnode) = &arc_opt {
            let of = arc_vnode.lock();
            match &of.vnode {
                crate::fs::vfs::VNode::PtyMaster(pty) | crate::fs::vfs::VNode::PtySlave(pty) => {
                    pty.lock().termios
                }
                _ => crate::fs::pty::Termios::default(),
            }
        } else {
            crate::fs::pty::Termios::default()
        };

        unsafe {
            core::ptr::write_bytes(argp as *mut u8, 0, 44);
            let ptr = argp as *mut u32;
            *ptr.add(0) = termios.c_iflag;
            *ptr.add(1) = termios.c_oflag;
            *ptr.add(2) = termios.c_cflag;
            *ptr.add(3) = termios.c_lflag;
            *(argp.wrapping_add(16) as *mut u8) = termios.c_line;
            core::ptr::copy_nonoverlapping(termios.c_cc.as_ptr(), argp.wrapping_add(17) as *mut u8, 19);
            *(argp.wrapping_add(36) as *mut u32) = 38400; // ispeed
            *(argp.wrapping_add(40) as *mut u32) = 38400; // ospeed
        }
        return 0;
    }
    // TCSETS2 = 0x402C542B, TCSETSW2 = 0x402C542C, TCSETSF2 = 0x402C542D
    if cmd == 0x402C542B || cmd == 0x402C542C || cmd == 0x402C542D {
        if !validate_user_read_range(argp, 44) {
            return EFAULT;
        }
        if let Some(arc_vnode) = &arc_opt {
            let of = arc_vnode.lock();
            match &of.vnode {
                crate::fs::vfs::VNode::PtyMaster(pty) | crate::fs::vfs::VNode::PtySlave(pty) => {
                    let ptr = argp as *const u32;
                    let mut termios = crate::fs::pty::Termios::default();
                    unsafe {
                        termios.c_iflag = *ptr.add(0);
                        termios.c_oflag = *ptr.add(1);
                        termios.c_cflag = *ptr.add(2);
                        termios.c_lflag = *ptr.add(3);
                        termios.c_line = *(argp.wrapping_add(16) as *const u8);
                        core::ptr::copy_nonoverlapping(argp.wrapping_add(17) as *const u8, termios.c_cc.as_mut_ptr(), 19);
                    }
                    pty.lock().termios = termios;
                }
                _ => {}
            }
        }
        return 0;
    }
    // TIOCGPGRP = 0x540F, TIOCSPGRP = 0x5410
    if cmd == 0x540F {
        if !validate_user_write_range(argp, core::mem::size_of::<i32>()) {
            return EFAULT;
        }
        let pgid = if let Some(arc_vnode) = &arc_opt {
            let of = arc_vnode.lock();
            match &of.vnode {
                crate::fs::vfs::VNode::TtyDevice { vt } => {
                    let vt = *vt;
                    drop(of);
                    let g = crate::tty::get_fg_pgid(vt);
                    if g > 0 {
                        g
                    } else {
                        with_current_process(|p| p.pid) as i32
                    }
                }
                crate::fs::vfs::VNode::PtyMaster(pty) | crate::fs::vfs::VNode::PtySlave(pty) => {
                    let p = pty.lock().pgrp;
                    drop(of);
                    if p != 0 { p } else { with_current_process(|p| p.pid) as i32 }
                }
                _ => {
                    drop(of);
                    with_current_process(|p| p.pid) as i32
                }
            }
        } else {
            with_current_process(|p| p.pid) as i32
        };
        unsafe {
            *(argp as *mut i32) = pgid;
        }
        return 0;
    }
    if cmd == 0x5410 {
        if !validate_user_read_range(argp, core::mem::size_of::<i32>()) {
            return EFAULT;
        }
        let new_pgid = unsafe { *(argp as *const i32) };
        if let Some(arc_vnode) = &arc_opt {
            let of = arc_vnode.lock();
            if let crate::fs::vfs::VNode::TtyDevice { vt } = &of.vnode {
                let vt = *vt;
                drop(of);
                crate::tty::set_fg_pgid(vt, new_pgid);
                return 0;
            }
        }
        if let Some(arc_vnode) = &arc_opt {
            let of = arc_vnode.lock();
            match &of.vnode {
                crate::fs::vfs::VNode::PtyMaster(pty) | crate::fs::vfs::VNode::PtySlave(pty) => {
                    pty.lock().pgrp = new_pgid;
                    // A group moved into the foreground must be woken if it is
                    // parked in the terminal-job-control wait. `tcsetpgrp` is
                    // the other half of SIGCONT: a shell that foregrounds a
                    // job (rather than signalling it) must let its read
                    // proceed. Without this the thread stays asleep in
                    // `JobControl` and never re-runs its foreground check.
                    crate::tasks::wake_jobcontrol_waiters();
                }
                _ => {}
            }
        }
        return 0;
    }
    // TIOCGWINSZ = 0x5413
    if cmd == 0x5413 {
        if !validate_user_write_range(argp, core::mem::size_of::<crate::fs::pty::WinSize>()) {
            return EFAULT;
        }
        // A VT reports its real grid size. This matters: zellij and every other
        // full-screen program size themselves from TIOCGWINSZ, and a default
        // 24x80 on an 88x34 console makes them draw in the wrong place.
        if let Some(arc_vnode) = &arc_opt {
            let of = arc_vnode.lock();
            if let crate::fs::vfs::VNode::TtyDevice { vt } = &of.vnode {
                let vt = *vt;
                drop(of);
                let ws = crate::tty::winsize(vt);
                unsafe {
                    core::ptr::write(argp as *mut crate::fs::pty::WinSize, ws);
                }
                return 0;
            }
        }
        let ws = if let Some(arc_vnode) = &arc_opt {
            let of = arc_vnode.lock();
            match &of.vnode {
                crate::fs::vfs::VNode::PtyMaster(pty) | crate::fs::vfs::VNode::PtySlave(pty) => {
                    pty.lock().winsize
                }
                _ => crate::fs::pty::WinSize::default(),
            }
        } else {
            crate::fs::pty::WinSize::default()
        };

        unsafe {
            core::ptr::write(argp as *mut crate::fs::pty::WinSize, ws);
        }
        return 0;
    }
    // TIOCSWINSZ = 0x5414
    if cmd == 0x5414 {
        if !validate_user_read_range(argp, core::mem::size_of::<crate::fs::pty::WinSize>()) {
            return EFAULT;
        }
        let ws = unsafe { core::ptr::read(argp as *const crate::fs::pty::WinSize) };
        // TIOCSWINSZ on a VT: record it, then SIGWINCH the foreground group.
        // A client that resizes expects to be told, not to poll.
        if let Some(arc_vnode) = &arc_opt {
            let of = arc_vnode.lock();
            if let crate::fs::vfs::VNode::TtyDevice { vt } = &of.vnode {
                let vt = *vt;
                drop(of);
                crate::tty::set_winsize(vt, ws);
                let pgid = crate::tty::get_fg_pgid(vt);
                if pgid > 0 {
                    crate::arch::x86_64::syscalls::proc::sys_kill(pgid as i64, 28);
                }
                return 0;
            }
        }
        // ponytail: signal after locks drop — set_winsize's SIGWINCH goes
        // through sys_kill (PROCESS_TABLE → SCHEDULER); the OpenFile/pty
        // guards must not be held across it (see PtyPair::set_winsize).
        let pgrp = if let Some(arc_vnode) = &arc_opt {
            let of = arc_vnode.lock();
            match &of.vnode {
                crate::fs::vfs::VNode::PtyMaster(pty) | crate::fs::vfs::VNode::PtySlave(pty) => {
                    pty.lock().set_winsize(ws)
                }
                _ => 0,
            }
        } else {
            0
        };
        if pgrp > 0 {
            super::proc::sys_kill(pgrp as i64, super::super::abi::SIGWINCH);
        }
        return 0;
    }
    // FIONBIO = 0x5421
    // ponytail: was a no-op stub; Rust std sets O_NONBLOCK via ioctl, so a
    // stub leaves wayland listeners blocking and accept() hangs forever.
    if cmd == 0x5421 {
        if !validate_user_read_range(argp, core::mem::size_of::<i32>()) {
            return EFAULT;
        }
        let nonblock = unsafe { *(argp as *const i32) } != 0;
        if let Some(arc_vnode) = &arc_opt {
            let mut of = arc_vnode.lock();
            if nonblock {
                of.flags |= 0x800;
            } else {
                of.flags &= !0x800;
            }
            match &mut of.vnode {
                crate::fs::vfs::VNode::Pipe { nonblock: nb, .. } => *nb = nonblock,
                crate::fs::vfs::VNode::UnixSocket(sock) => sock.lock().nonblock = nonblock,
                crate::fs::vfs::VNode::InetSocket(inet) => inet.lock().nonblock = nonblock,
                _ => {}
            }
        }
        return 0;
    }
    // FIONREAD = 0x541B
    if cmd == 0x541B {
        if !validate_user_write_range(argp, core::mem::size_of::<i32>()) {
            return EFAULT;
        }
        let mut available = 0i32;
        if let Some(arc_vnode) = &arc_opt {
            let of = arc_vnode.lock();
            match &of.vnode {
                crate::fs::vfs::VNode::Pipe { shared, end, .. } => {
                    let pb = shared.lock();
                    if *end == crate::fs::vfs::PipeEnd::Read {
                        available = pb.buffer.len() as i32;
                    }
                }
                crate::fs::vfs::VNode::PtyMaster(pty) => {
                    available = pty.lock().s2m.len() as i32;
                }
                crate::fs::vfs::VNode::PtySlave(pty) => {
                    available = pty.lock().m2s.len() as i32;
                }
                _ => {}
            }
        }
        unsafe {
            *(argp as *mut i32) = available;
        }
        return 0;
    }
    // FIOCLEX = 0x5451, FIONCLEX = 0x5450
    if cmd == 0x5451 || cmd == 0x5450 {
        return 0;
    }
    // VT / Console ioctls (for libseat / direct console management)
    // VT_OPENQRY = 0x5600: first VT not in use, for a compositor to take.
    if cmd == 0x5600 {
        if !validate_user_write_range(argp, core::mem::size_of::<i32>()) {
            return EFAULT;
        }
        let n = crate::graphics::vt::open_query();
        unsafe { *(argp as *mut i32) = n as i32; }
        return 0;
    }
    // VT_GETMODE = 0x5601, VT_SETMODE = 0x5602
    if cmd == 0x5601 || cmd == 0x5602 {
        return 0;
    }
    // VT_GETSTATE = 0x5603: struct vt_stat { v_active: u16, v_signal: u16, v_state: u16 }
    if cmd == 0x5603 {
        if !validate_user_write_range(argp, 6) {
            return EFAULT;
        }
        let active = ACTIVE_VT.load(core::sync::atomic::Ordering::Relaxed);
        unsafe {
            let ptr = argp as *mut u16;
            *ptr.add(0) = active;
            *ptr.add(1) = 0; // v_signal
            *ptr.add(2) = 1 << active; // bitmask of active VTs
        }
        return 0;
    }
    // VT_ACTIVATE = 0x5606
    if cmd == 0x5606 {
        let n = argp as u16;
        let prev = ACTIVE_VT.load(core::sync::atomic::Ordering::Relaxed);
        // bind_graphical, not bare switch_vt: a userspace VT_ACTIVATE is how a
        // compositor claims the console (VT_OPENQRY then VT_ACTIVATE), and
        // recording the binding is what makes Ctrl+Alt+F<n> reclaim the console
        // from it. switch_vt left GRAPHICAL_VT at 0 forever, so the reclaim
        // branch in keyboard.rs could never be taken.
        //
        // The kernel's own Ctrl+Alt+F<n> path calls vt::switch_vt directly and
        // is deliberately unaffected -- the user switching consoles must not
        // record themselves as the graphical client.
        if !crate::graphics::vt::bind_graphical(n) {
            // Linux answers EINVAL for a VT that does not exist; the old code
            // accepted any number, which is how a bogus VT could become
            // "current" and leave the console rendering nothing.
            return super::EINVAL;
        }
        crate::dprintln!("[VT] activate VT {} (prev {})", n, prev);
        // Repaint: the newly current VT's cells have never been on this
        // framebuffer. Previously this only *cleared* the screen when leaving
        // VT 1, so switching lost the contents of both consoles.
        crate::graphics::terminal::WRITER.lock().repaint_all();
        return 0;
    }
    // VT_WAITACTIVE = 0x5607, VT_RELDISP = 0x5605, VT_DISALLOCATE = 0x5608
    if cmd == 0x5607 || cmd == 0x5605 || cmd == 0x5608 {
        return 0;
    }
    // KDSETMODE = 0x4B3A, KDGETMODE = 0x4B3B
    if cmd == 0x4B3A {
        let old = CONSOLE_KD_MODE.swap(argp as u32, core::sync::atomic::Ordering::SeqCst);
        if argp == 0 && old != 0 {
            // Returning to text mode: clear/restore text console. This is also
            // the client handing the console back, so drop the graphical binding
            // it recorded at VT_ACTIVATE -- otherwise a later Ctrl+Alt+F<n>
            // would try to reclaim a console that was already given up.
            let active = ACTIVE_VT.load(core::sync::atomic::Ordering::Relaxed);
            crate::graphics::vt::release_graphical(active);
            crate::graphics::terminal::WRITER.lock().clear();
        }
        return 0;
    }
    if cmd == 0x4B3B {
        if !validate_user_write_range(argp, core::mem::size_of::<i32>()) {
            return EFAULT;
        }
        let mode = CONSOLE_KD_MODE.load(core::sync::atomic::Ordering::SeqCst) as i32;
        unsafe { *(argp as *mut i32) = mode; }
        return 0;
    }
    // KDGKBMODE = 0x4B44, KDSKBMODE = 0x4B45
    if cmd == 0x4B44 {
        if !validate_user_write_range(argp, core::mem::size_of::<i32>()) {
            return EFAULT;
        }
        let mode = CONSOLE_KB_MODE.load(core::sync::atomic::Ordering::SeqCst) as i32;
        unsafe { *(argp as *mut i32) = mode; }
        return 0;
    }
    if cmd == 0x4B45 {
        CONSOLE_KB_MODE.store(argp as u32, core::sync::atomic::Ordering::SeqCst);
        return 0;
    }

    if arc_opt.is_some() || fd <= 2 {
        super::ENOTTY
    } else {
        super::EBADF
    }
}

pub(super) fn sys_pipe(pipefd: *mut i32, nonblock: bool) -> u64 {
    if !validate_user_write_range(pipefd as u64, 2 * core::mem::size_of::<i32>()) {
        return EFAULT;
    }

    let shared = Arc::new(SpinLock::new(PipeBuffer::new()));

    let read_arc = Arc::new(SpinLock::new(crate::fs::vfs::OpenFile {
        vnode: VNode::Pipe {
            shared: shared.clone(),
            end: PipeEnd::Read,
            nonblock,
            token: shared.lock().read_token.clone(),
        },
        flags: 0,
        path: alloc::string::String::from("pipe:[read]"),
    }));
    let write_arc = Arc::new(SpinLock::new(crate::fs::vfs::OpenFile {
        vnode: VNode::Pipe {
            shared: shared.clone(),
            end: PipeEnd::Write,
            nonblock,
            token: shared.lock().write_token.clone(),
        },
        flags: 0,
        path: alloc::string::String::from("pipe:[write]"),
    }));

    let t = crate::tasks::current_thread().expect("sys_pipe2: no current thread");
    let (fd0, fd1) = {
        let mut p = t.process.lock();

        let mut fd0 = None;
        let mut fd1 = None;
        for (i, slot) in p.fd_table.iter_mut().enumerate() {
            if slot.is_none() {
                if fd0.is_none() {
                    fd0 = Some(i);
                    *slot = Some(read_arc.clone());
                } else {
                    fd1 = Some(i);
                    *slot = Some(write_arc.clone());
                    break;
                }
            }
        }

        if fd0.is_none() {
            p.fd_table.push(Some(read_arc.clone()));
            fd0 = Some(p.fd_table.len() - 1);
        }
        if fd1.is_none() {
            p.fd_table.push(Some(write_arc));
            fd1 = Some(p.fd_table.len() - 1);
        }
        (fd0, fd1)
    };
    crate::tasks::set_current_thread(Some(t));

    unsafe {
        *pipefd = fd0.unwrap() as i32;
        *pipefd.add(1) = fd1.unwrap() as i32;
    }

    0
}

pub(super) fn sys_dup(oldfd: usize) -> u64 {
    let t = crate::tasks::current_thread().expect("sys_dup: no current thread");
    let result = {
        let mut p = t.process.lock();

        if oldfd >= p.fd_table.len() || p.fd_table[oldfd].is_none() {
            EBADF
        } else {
            let vnode = p.fd_table[oldfd].clone();
            let mut result = None;
            for (i, slot) in p.fd_table.iter_mut().enumerate() {
                if slot.is_none() {
                    *slot = vnode.clone();
                    result = Some(i as u64);
                    break;
                }
            }
            if let Some(r) = result {
                r
            } else {
                p.fd_table.push(vnode);
                (p.fd_table.len() - 1) as u64
            }
        }
    };
    crate::tasks::set_current_thread(Some(t));
    result
}

pub(super) fn sys_fcntl(fd: usize, cmd: u64, arg: u64) -> u64 {
    const O_NONBLOCK: u64 = 0x800;
    // TEMP-DIAG: which fcntl cmds hit sockets.
    if cfg!(feature = "dsp_verbose") { crate::dprintln!("[fcntl-tmp] pid={} fd={} cmd={} arg={:#x}", crate::tasks::current_thread().map(|t| t.process.lock().pid).unwrap_or(0), fd, cmd, arg); }

    match cmd {
        0 | 1030 => {
            let min_fd = arg as usize;
            let t = crate::tasks::current_thread().expect("sys_fcntl: no current thread");
            let result = {
                let mut p = t.process.lock();
                if fd >= p.fd_table.len() || p.fd_table[fd].is_none() {
                    EBADF
                } else {
                    let vnode = p.fd_table[fd].clone();
                    let mut i = min_fd.max(3);
                    while i < p.fd_table.len() {
                        if p.fd_table[i].is_none() {
                            p.fd_table[i] = vnode.clone();
                            break;
                        }
                        i += 1;
                    }
                    if i >= p.fd_table.len() {
                        p.fd_table.push(vnode);
                        (p.fd_table.len() - 1) as u64
                    } else {
                        i as u64
                    }
                }
            };
            crate::tasks::set_current_thread(Some(t));
            result
        }
        1 | 2 => 0,
        // F_GETLK (5): check whether a lock would conflict. Report back via
        // the userspace `struct flock`: l_type = F_UNLCK if free, or the
        // conflicting lock's type/pid if held by another process.
        5 => {
            let key = match vnode_key_of_fd(fd) {
                Some(k) => k,
                None => return EBADF,
            };
            let (l_type, _) = match read_flock_type(arg) {
                Some(t) => t,
                None => return EFAULT,
            };
            let requesting_write = l_type == F_WRLCK as i16;
            let thread = crate::tasks::current_thread().expect("sys_fcntl: no current thread");
            let pid = thread.process.lock().pid;
            drop(thread);
            let locks = FILE_LOCKS.lock();
            let mut conflict: Option<FileLockInfo> = None;
            for (key2, info) in locks.iter() {
                if key2.vnode_key == key && key2.pid != pid
                    && lock_conflicts(info.lock_type, if requesting_write { F_WRLCK } else { F_RDLCK })
                {
                    conflict = Some(*info);
                    break;
                }
            }
            match conflict {
                Some(info) => {
                    write_flock_result(arg, info.lock_type as i16, pid as i32);
                    0
                }
                None => {
                    write_flock_result(arg, F_UNLCK as i16, 0);
                    0
                }
            }
        }
        // F_SETLK (6): non-blocking lock. Return EAGAIN if a conflicting lock
        // is held by another process.
        6 => {
            fcntl_setlk(fd, arg, false)
        }
        // F_SETLKW (7): blocking lock. Implemented as non-blocking for now;
        // dbus-daemon uses F_SETLK.
        7 => {
            fcntl_setlk(fd, arg, false)
        }
        3 => {
            let thread = crate::tasks::current_thread().expect("sys_fcntl: no current thread");
            let p = thread.process.lock();
            if fd == 0 {
                let mut flags = 0o2; // O_RDWR
                if p.stdin_nonblock {
                    flags |= O_NONBLOCK;
                }
                return flags;
            }
            if fd < p.fd_table.len() {
                if let Some(v) = &p.fd_table[fd] {
                    let of = v.lock();
                    return of.flags;
                }
            }
            EBADF
        }
        4 => {
            let nonblock = (arg & O_NONBLOCK) != 0;
            // TEMP-DIAG: trace nonblocking sets on sockets.
            if cfg!(feature = "dsp_verbose") { crate::dprintln!("[fcntl-tmp] pid={} fd={} setfl nonblock={}", crate::tasks::current_thread().map(|t| t.process.lock().pid).unwrap_or(0), fd, nonblock); }
            let t = crate::tasks::current_thread().expect("sys_fcntl: no current thread");
            let result = {
                let mut p = t.process.lock();
                if fd == 0 {
                    p.stdin_nonblock = nonblock;
                    0
                } else if fd < p.fd_table.len() {
                    if let Some(v) = &p.fd_table[fd] {
                        let mut of = v.lock();
                        of.flags = arg;
                        if let VNode::Pipe { nonblock: nb, .. } = &mut of.vnode {
                            *nb = nonblock;
                        }
                        // ponytail: O_NONBLOCK must reach sockets too (wayland
                        // listeners/clients set it; without this accept() blocks
                        // forever instead of EAGAIN).
                        if let VNode::UnixSocket(sock) = &of.vnode {
                            sock.lock().nonblock = nonblock;
                        }
                        if let VNode::InetSocket(inet) = &of.vnode {
                            inet.lock().nonblock = nonblock;
                        }
                        if let VNode::UdpSocket(udp) = &of.vnode {
                            udp.lock().nonblock = nonblock;
                        }
                        0
                    } else {
                        EBADF
                    }
                } else {
                    EBADF
                }
            };
            crate::tasks::set_current_thread(Some(t));
            result
        }
        1033 => {
            // F_ADD_SEALS: seals on memfd / file
            let thread = match crate::tasks::current_thread() {
                Some(t) => t,
                None => return ESRCH,
            };
            let p = thread.process.lock();
            if fd < p.fd_table.len() && p.fd_table[fd].is_some() {
                crate::dprintln!("[fcntl] pid={} fd={} F_ADD_SEALS {:#x} -> OK", p.pid, fd, arg);
                0
            } else {
                EBADF
            }
        }
        1034 => {
            // F_GET_SEALS: return current seals
            let thread = match crate::tasks::current_thread() {
                Some(t) => t,
                None => return ESRCH,
            };
            let p = thread.process.lock();
            if fd < p.fd_table.len() && p.fd_table[fd].is_some() {
                0
            } else {
                EBADF
            }
        }
        _ => EINVAL,
    }
}

/// `F_SETLK` / `F_SETLKW` (blocking not yet implemented): acquire or release
/// an advisory lock on the file behind `fd`.
///
/// `arg` is a userspace pointer to `struct flock`. `l_type` is F_RDLCK,
/// F_WRLCK, or F_UNLCK. Returns 0 on success, EAGAIN if a conflicting lock
/// is held by another process, or EFAULT/EBADF on error.
fn fcntl_setlk(fd: usize, arg: u64, _blocking: bool) -> u64 {
    let (l_type, _) = match read_flock_type(arg) {
        Some(t) => t,
        None => return EFAULT,
    };
    let key = match vnode_key_of_fd(fd) {
        Some(k) => k,
        None => return EBADF,
    };
    let thread = crate::tasks::current_thread().expect("sys_fcntl: no current thread");
    let pid = thread.process.lock().pid;
    drop(thread);

    let mut locks = FILE_LOCKS.lock();

    // Release any existing lock this process holds on this file, then
    // re-record (or drop) it based on l_type.
    locks.remove(&FileLockKey { vnode_key: key, pid });

    if l_type == F_UNLCK as i16 {
        return 0;
    }

    // Check for conflicts with locks held by other processes.
    for (key2, info) in locks.iter() {
        if key2.vnode_key == key && key2.pid != pid && lock_conflicts(info.lock_type, l_type as i32) {
            return EAGAIN;
        }
    }

    locks.insert(
        FileLockKey { vnode_key: key, pid },
        FileLockInfo { lock_type: l_type as i32 },
    );
    0
}

pub(super) fn sys_flock(fd: usize, _op: i32) -> u64 {
    let thread = match crate::tasks::current_thread() {
        Some(t) => t,
        None => return ESRCH,
    };
    let p = thread.process.lock();
    if fd < p.fd_table.len() && p.fd_table[fd].is_some() {
        0
    } else {
        EBADF
    }
}

pub(super) fn sys_getcwd(buf: *mut u8, size: usize) -> u64 {
    if validate_user_write_range(buf as u64, size) {
        let thread = match crate::tasks::current_thread() {
            Some(t) => t,
            None => return ESRCH,
        };
        let cwd = thread.process.lock().cwd.clone();
        let bytes = cwd.as_bytes();
        if bytes.len() + 1 <= size {
            unsafe {
                core::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, bytes.len());
                *buf.add(bytes.len()) = 0;
            }
            return buf as u64;
        } else {
            return ERANGE;
        }
    }
    EFAULT
}

/// ponytail: on-disk dirs resolve as FsNode (is_dir), not VNode::Directory
/// (synthetic nodes only) — matching only Directory broke chdir("/").
fn vnode_is_dir(v: &crate::fs::vfs::VNode) -> bool {
    match v {
        crate::fs::vfs::VNode::Directory { .. } => true,
        crate::fs::vfs::VNode::FsNode { node, .. } => node.is_dir(),
        _ => false,
    }
}

pub(super) fn sys_chdir(path_ptr: u64) -> u64 {
    let path = match read_user_cstring(path_ptr) {
        Some(p) => p,
        None => return EFAULT,
    };
    let thread = match crate::tasks::current_thread() {
        Some(t) => t,
        None => return ESRCH,
    };
    // Snapshot cwd under the process lock, then release it: the lookup below
    // needs process.lock() itself (it fetches the caller's credentials), and
    // AGENTS.md §1 makes process.lock() the innermost lock.
    let cwd = {
        let p = thread.process.lock();
        p.cwd.clone()
    };
    if let Some(vnode) = crate::fs::lookup_relative_to(&cwd, &path) {
        if vnode_is_dir(&vnode) {
            let mut p = thread.process.lock();
            p.cwd = crate::fs::canonicalize_path(&cwd, &path);
            return 0;
        } else {
            return ENOTDIR;
        }
    }
    ENOENT
}

/// 161: `chroot(path)`.
/// ponytail: validates like chdir (ENOENT/ENOTDIR) but does NOT confine path
/// resolution — no per-process root yet. Enough for sshd privsep staging
/// (which only needs the call to succeed); real isolation means threading a
/// root through canonicalize_path/lookup if the threat model ever needs it.
pub(super) fn sys_chroot(path_ptr: u64) -> u64 {
    let path = match read_user_cstring(path_ptr) {
        Some(p) => p,
        None => return EFAULT,
    };
    let thread = match crate::tasks::current_thread() {
        Some(t) => t,
        None => return ESRCH,
    };
    let cwd = {
        let p = thread.process.lock();
        p.cwd.clone()
    };
    if let Some(vnode) = crate::fs::lookup_relative_to(&cwd, &path) {
        if vnode_is_dir(&vnode) {
            return 0;
        } else {
            return ENOTDIR;
        }
    }
    ENOENT
}

pub(super) fn sys_getdents64(fd: usize, dirp: *mut u8, count: usize) -> u64 {
    if !validate_user_write_range(dirp as u64, count) {
        return EFAULT;
    }

    let thread = match crate::tasks::current_thread() {
        Some(t) => t,
        None => return EBADF,
    };
    let p = thread.process.lock();
    let slot = match p.fd_table.get(fd) {
        Some(s) => s,
        None => return EBADF,
    };
    let vnode = match slot.as_ref() {
        Some(v) => Arc::clone(v),
        None => return EBADF,
    };
    drop(p);

    // ── TAR-backed Directory: extract name+cursor, drop lock, then run list_dir_into.
    // IMPORTANT: we must NOT hold the vnode spinlock while calling vnode.lock() again
    // to update the cursor (that would re-entrantly deadlock the spinlock).
    let dir_info: Option<(alloc::string::String, usize)> = {
        let v = vnode.lock();
        if let crate::fs::vfs::VNode::Directory { name, cursor } = &v.vnode {
            Some((name.clone(), *cursor))
        } else {
            None
        }
        // v (and the spinlock) is released here
    };

    if let Some((dir_name, dir_cursor)) = dir_info {
        let initrd = *INITRD.lock();
        let data = match initrd {
            Some(d) => d,
            None => return 0,
        };
        let out = unsafe { core::slice::from_raw_parts_mut(dirp, count) };
        let (written, new_cursor) = crate::fs::dirent::list_dir_into(data, &dir_name, dir_cursor, out);
        if written > 0 {
            // Safe: previous lock was dropped above.
            let mut v2 = vnode.lock();
            if let crate::fs::vfs::VNode::Directory { cursor: c, .. } = &mut v2.vnode {
                *c = new_cursor;
            }
        }
        return written as u64;
    // tprintln!("getdents64: fd={} entries={} written={} new_cursor={}", fd, entries.len(), written, new_cursor);
    }

    // ── FsNode (ext2 / real filesystem / synthetic filesystems) ───────────────────
    let (entries, cursor) = {
        let v = vnode.lock();
        match &v.vnode {
            crate::fs::vfs::VNode::FsNode { node, cursor } => {
                match node.list_dir_types() {
                    Ok(list) => (list, *cursor),
                    Err(_) => return EIO,
                }
            }
            _ => return ENOTDIR,
        }
    };
    crate::dprintln!("sys_getdents64: fd={} cursor={} entries={:?}", fd, cursor, entries);

    let out = unsafe { core::slice::from_raw_parts_mut(dirp, count) };
    let mut written: usize = 0;
    let mut new_cursor = entries.len();
    for (i, (name, d_type)) in entries.iter().enumerate().skip(cursor) {
        let rec = crate::fs::dirent::pack_dirent64(
            &mut out[written..],
            i as u64,
            (i as i64) + 1,
            *d_type,
            name,
        );
        if rec == 0 {
            new_cursor = i;
            break;
        }
        written += rec;
    }

    if written > 0 {
        let mut v2 = vnode.lock();
        if let crate::fs::vfs::VNode::FsNode { cursor: c, .. } = &mut v2.vnode {
            *c = new_cursor;
        }
    }

    written as u64
}

pub(super) fn sys_readv(fd: usize, iov: *const [u64; 2], iovcnt: u64) -> u64 {
    if !validate_user_read_range(iov as u64, (iovcnt as usize) * core::mem::size_of::<[u64; 2]>()) {
        return EFAULT;
    }

    let vnode_opt = {
        let thread = crate::tasks::current_thread().expect("sys_readv: no current thread");
        let p = thread.process.lock();
        if fd < p.fd_table.len() {
            p.fd_table[fd].clone()
        } else {
            None
        }
    };

    if let Some(vnode_arc) = vnode_opt {
        let mut total = 0;
        for i in 0..iovcnt {
            let ptr = unsafe { iov.add(i as usize) };
            let base = unsafe { (*ptr)[0] } as *mut u8;
            let len = unsafe { (*ptr)[1] } as usize;
            if len > 0 {
                if !validate_user_write_range(base as u64, len) {
                    return if total == 0 { EFAULT } else { total };
                }
                let slice = unsafe { core::slice::from_raw_parts_mut(base, len) };
                match crate::fs::vfs::OpenFile::read_file(&vnode_arc, slice) {
                    Ok(n) => {
                        total += n as u64;
                        if n < len { break; } // Short read
                    }
                    Err(e) => if total == 0 { return e; } else { break; }
                }
            }
        }
        return total;
    } else if fd == 0 {
        // Fallback for stdin if needed, though usually sys_read is used
        let mut total = 0;
        for i in 0..iovcnt {
            let ptr = unsafe { iov.add(i as usize) };
            let base = unsafe { (*ptr)[0] } as *mut u8;
            let len = unsafe { (*ptr)[1] } as usize;
            if len > 0 {
                let n = sys_read(fd, base, len) as usize;
                total += n as u64;
                if n < len { break; }
            }
        }
        return total;
    }
    EBADF
}

pub(super) fn sys_writev(fd: usize, iov: *const [u64; 2], iovcnt: u64) -> u64 {
    if !validate_user_read_range(iov as u64, (iovcnt as usize) * core::mem::size_of::<[u64; 2]>()) {
        return EFAULT;
    }

    let vnode_opt = {
        let thread = crate::tasks::current_thread().expect("sys_writev: no current thread");
        let p = thread.process.lock();
        if fd < p.fd_table.len() {
            p.fd_table[fd].clone()
        } else {
            None
        }
    };

    if let Some(vnode_arc) = vnode_opt {
        let mut total = 0;
        for i in 0..iovcnt {
            let ptr = unsafe { iov.add(i as usize) };
            let base = unsafe { (*ptr)[0] } as *const u8;
            let len = unsafe { (*ptr)[1] } as usize;
            if len > 0 {
                if !validate_user_read_range(base as u64, len) {
                    return if total == 0 { EFAULT } else { total };
                }
                let slice = unsafe { core::slice::from_raw_parts(base, len) };
                match crate::fs::vfs::OpenFile::write_file(&vnode_arc, slice) {
                    Ok(n) => total += n as u64,
                    Err(e) => if total == 0 { return e; } else { break; }
                }
            }
        }
        return total;
    } else if fd == 1 || fd == 2 {
        let mut total = 0;
        for i in 0..iovcnt {
            let ptr = unsafe { iov.add(i as usize) };
            let base = unsafe { (*ptr)[0] } as *const u8;
            let len = unsafe { (*ptr)[1] } as usize;
            if len > 0 {
                if !validate_user_read_range(base as u64, len) {
                    return if total == 0 { EFAULT } else { total };
                }
                let slice = unsafe { core::slice::from_raw_parts(base, len) };
                if let Ok(s) = core::str::from_utf8(slice) {
                    crate::print!("{}", s);
                    tprint!("{}", s);
                    crate::dprint!("{}", s);
                } else {
                    for &b in slice {
                        crate::print!("{}", b as char);
                        tprint!("{}", b as char);
                        crate::dprint!("{}", b as char);
                    }
                }
                total += len as u64;
            }
        }
        return total as u64;
    }
    EBADF
}

pub(super) fn sys_dup2(oldfd: usize, newfd: usize) -> u64 {
    let t = crate::tasks::current_thread().expect("sys_dup2: no current thread");
    let (result, old_vnode) = {
        let mut p = t.process.lock();

        if oldfd >= p.fd_table.len() || p.fd_table[oldfd].is_none() {
            (EBADF, None)
        } else if oldfd == newfd {
            (newfd as u64, None)
        } else {
            let vnode = p.fd_table[oldfd].clone();
            let mut old = None;
            if newfd >= p.fd_table.len() {
                p.fd_table.resize(newfd + 1, None);
            } else {
                old = p.fd_table[newfd].take();
            }
            p.fd_table[newfd] = vnode;
            (newfd as u64, old)
        }
    };
    if let Some(v) = old_vnode {
        let vnode_kind = { v.lock().vnode.clone() };
        if matches!(vnode_kind, VNode::Pipe { .. }) {
            wake_pipe_waiters();
        }
        if matches!(vnode_kind, VNode::DspAudioDevice | VNode::SndPlaybackDevice) {
            crate::drivers::audio::vfs_node::dsp_flush();
        }
        if Arc::strong_count(&v) == 1
            && matches!(vnode_kind, crate::fs::vfs::VNode::InputEventDevice { .. } | crate::fs::vfs::VNode::InputMiceDevice { .. })
        {
            crate::drivers::keyboard::input_unregister(Arc::as_ptr(&v) as usize);
        }
    }
    crate::tasks::set_current_thread(Some(t));
    result
}

pub(super) fn sys_dup3(oldfd: usize, newfd: usize, flags: i32) -> u64 {
    let cloexec = (flags & 0o2000000) != 0; // O_CLOEXEC
    if oldfd == newfd {
        return super::EINVAL;
    }
    let t = crate::tasks::current_thread().expect("sys_dup3: no current thread");
    let (result, old_vnode) = {
        let mut p = t.process.lock();
        if oldfd >= p.fd_table.len() || p.fd_table[oldfd].is_none() {
            (EBADF, None)
        } else {
            let vnode = p.fd_table[oldfd].clone();
            let mut old = None;
            if newfd >= p.fd_table.len() {
                p.fd_table.resize(newfd + 1, None);
            } else {
                old = p.fd_table[newfd].take();
            }
            p.fd_table[newfd] = vnode;
            (newfd as u64, old)
        }
    };
    if let Some(v) = old_vnode {
        let vnode_kind = { v.lock().vnode.clone() };
        if matches!(vnode_kind, VNode::Pipe { .. }) {
            wake_pipe_waiters();
        }
        if Arc::strong_count(&v) == 1
            && matches!(vnode_kind, crate::fs::vfs::VNode::InputEventDevice { .. } | crate::fs::vfs::VNode::InputMiceDevice { .. })
        {
            crate::drivers::keyboard::input_unregister(Arc::as_ptr(&v) as usize);
        }
    }
    crate::tasks::set_current_thread(Some(t));
    let _ = cloexec; // O_CLOEXEC would set FD_CLOEXEC on newfd; accept but noop for now
    result
}

pub(super) fn sys_utimensat(dirfd: i32, path_ptr: u64, times_ptr: u64, flags: u32) -> u64 {
    let _ = (dirfd, path_ptr, times_ptr, flags);
    0
}

pub(super) fn sys_renameat(oldfd: i32, oldpath: u64, newfd: i32, newpath: u64) -> u64 {
    let _ = (oldfd, oldpath, newfd, newpath);
    0
}

pub(super) fn sys_renameat2(oldfd: i32, oldpath: u64, newfd: i32, newpath: u64, flags: u32) -> u64 {
    let _ = (oldfd, oldpath, newfd, newpath, flags);
    0
}

pub(super) fn fill_stat_from_vnode(statbuf: *mut Stat, vnode: &VNode) {
    let mut st = Stat::default();
    st.st_dev = 0;
    match vnode {
        crate::fs::vfs::VNode::IoUring { .. } => {
            st.st_mode = 0o140000 | 0o600; // S_IFCHR | 0600
            st.st_nlink = 1;
            st.st_size = 0;
            st.st_blksize = 4096;
            st.st_blocks = 0;
        }
        crate::fs::vfs::VNode::UnixSocket(_) | crate::fs::vfs::VNode::InetSocket(_) | crate::fs::vfs::VNode::UdpSocket(_) => {
            st.st_mode = 0o140000 | 0o755;
            st.st_nlink = 1;
            st.st_ino = 100;
            st.st_dev = 1;
        }
        crate::fs::vfs::VNode::Directory { .. } => {
            st.st_mode = 0o040000 | 0o755;
            st.st_nlink = 2;
            st.st_size = 4096;
            st.st_blksize = 4096;
            st.st_blocks = 8;
            st.st_ino = 1;
            st.st_dev = 1;
        }
        crate::fs::vfs::VNode::InitrdFile { data, .. } => {
            st.st_mode = 0o100000 | 0o755;
            st.st_nlink = 1;
            st.st_size = data.len() as i64;
            st.st_blksize = 4096;
            st.st_blocks = ((data.len() + 511) / 512) as i64;
            st.st_ino = (data.as_ptr() as u64) | 1;
            st.st_dev = 1;
        }
        crate::fs::vfs::VNode::BlockFile { start_sector, size, .. } => {
            st.st_mode = 0o100000 | 0o755;
            st.st_nlink = 1;
            st.st_size = *size as i64;
            st.st_blksize = 512;
            st.st_blocks = ((*size + 511) / 512) as i64;
            st.st_ino = *start_sector | 2;
            st.st_dev = 2;
        }
        crate::fs::vfs::VNode::InputEventDevice { minor } => {
            // Char device 13:64+N — must match the sysfs entry so
            // udev-style devnum lookups (libinput path backend) resolve.
            st.st_mode = 0o020000 | 0o660;
            st.st_uid = 0;
            st.st_gid = 1000; // seatd/mywima run as uid/gid 1000; the group class
                              // lets them open input devices.
            st.st_rdev = (13 << 8) | (64 + *minor as u64); // makedev(13, 64+N)
            st.st_nlink = 1;
            st.st_blksize = 512;
        }
        crate::fs::vfs::VNode::InputMiceDevice { .. } => {
            // Char device 13:63 — Linux mouse multiplexer.
            st.st_mode = 0o020000 | 0o660;
            st.st_uid = 0;
            st.st_gid = 1000;
            st.st_rdev = (13 << 8) | 63; // makedev(13, 63)
            st.st_nlink = 1;
            st.st_blksize = 512;
        }
        crate::fs::vfs::VNode::DrmDevice { minor } => {
            st.st_mode = 0o020000 | 0o666;
            st.st_uid = 0;
            st.st_gid = 1000; // the render node is group-accessible to uid 1000.
            st.st_rdev = (226 << 8) | (*minor as u64); // makedev(226, minor)
            st.st_nlink = 1;
            st.st_size = 0;
            st.st_blksize = 512;
            st.st_blocks = 0;
            st.st_ino = 2260 + (*minor as u64);
            st.st_dev = 1;
        }
        crate::fs::vfs::VNode::TtyDevice { vt: _ } => {
            st.st_mode = 0o020000 | 0o666;
            st.st_rdev = (4 << 8) | 0; // makedev(4, 0) for tty0
            st.st_nlink = 1;
            st.st_size = 0;
            st.st_blksize = 512;
            st.st_blocks = 0;
            st.st_ino = 40;
            st.st_dev = 1;
        }
        crate::fs::vfs::VNode::PtyMaster(_) => {
            st.st_mode = 0o020000 | 0o666; // S_IFCHR | 0666
            st.st_rdev = (5 << 8) | 2;     // makedev(5, 2) for /dev/ptmx
            st.st_nlink = 1;
            st.st_size = 0;
            st.st_blksize = 512;
            st.st_blocks = 0;
            st.st_ino = 52;
            st.st_dev = 1;
        }
        crate::fs::vfs::VNode::PtySlave(pty) => {
            let p = pty.lock();
            let id = p.id;
            st.st_mode = 0o020000 | (p.mode & 0o7777); // S_IFCHR | stored perms
            st.st_uid = p.uid;
            st.st_gid = p.gid;
            st.st_rdev = (136 << 8) | (id as u64 & 0xFF); // makedev(136, id) for /dev/pts/<id>
            st.st_nlink = 1;
            st.st_size = 0;
            st.st_blksize = 512;
            st.st_blocks = 0;
            st.st_ino = 1360 + id as u64;
            st.st_dev = 1;
        }
        crate::fs::vfs::VNode::NullDevice => {
            st.st_mode = 0o020000 | 0o666;
            st.st_rdev = (1 << 8) | 3; // makedev(1, 3) /dev/null
            st.st_nlink = 1;
            st.st_size = 0;
            st.st_blksize = 512;
            st.st_blocks = 0;
            st.st_ino = 13;
            st.st_dev = 1;
        }
        crate::fs::vfs::VNode::DmaBuf(dma_buf) => {
            let dmabuf = dma_buf.lock();
            st.st_mode = 0o020000 | 0o666; // S_IFCHR | 0666
            st.st_nlink = 1;
            st.st_size = dmabuf.size as i64;
            st.st_blksize = 512;
            st.st_blocks = 0;
        }
        crate::fs::vfs::VNode::PcSpeakerDevice | crate::fs::vfs::VNode::DspAudioDevice | crate::fs::vfs::VNode::FbDevice { .. } | crate::fs::vfs::VNode::Epoll(..) | crate::fs::vfs::VNode::EventFd(..) | crate::fs::vfs::VNode::TimerFd(..) | crate::fs::vfs::VNode::Inotify(..) | crate::fs::vfs::VNode::FuseDevice | crate::fs::vfs::VNode::SerialDevice => {
            // Character device
            st.st_mode = 0o020000 | 0o666;
            st.st_nlink = 1;
            st.st_size = 0;
            st.st_blksize = 512;
            st.st_blocks = 0;
        }
        crate::fs::vfs::VNode::SndControlDevice => {
            st.st_mode = 0o020000 | 0o666;
            st.st_nlink = 1;
            st.st_size = 0;
            st.st_blksize = 512;
            st.st_blocks = 0;
            st.st_rdev = (116 << 8) | 0; // sound major 116, minor 0
        }
        crate::fs::vfs::VNode::SndPlaybackDevice => {
            st.st_mode = 0o020000 | 0o666;
            st.st_nlink = 1;
            st.st_size = 0;
            st.st_blksize = 512;
            st.st_blocks = 0;
            st.st_rdev = (116 << 8) | 1; // sound major 116, minor 1
        }
        crate::fs::vfs::VNode::SndCaptureDevice => {
            st.st_mode = 0o020000 | 0o666;
            st.st_nlink = 1;
            st.st_size = 0;
            st.st_blksize = 512;
            st.st_blocks = 0;
            st.st_rdev = (116 << 8) | 2; // sound major 116, minor 2
        }
        crate::fs::vfs::VNode::FsNode { node, .. } => {
            st = node.stat();
        }
        crate::fs::vfs::VNode::Pipe { .. } => {
            st.st_mode = 0o010000 | 0o666;
        }
    }
    unsafe {
        core::ptr::write(statbuf, st);
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct StatxTimestamp {
    pub tv_sec: i64,
    pub tv_nsec: u32,
    pub __reserved: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Statx {
    pub stx_mask: u32,
    pub stx_blksize: u32,
    pub stx_attributes: u64,
    pub stx_nlink: u32,
    pub stx_uid: u32,
    pub stx_gid: u32,
    pub stx_mode: u16,
    pub __spare0: [u16; 1],
    pub stx_ino: u64,
    pub stx_size: u64,
    pub stx_blocks: u64,
    pub stx_attributes_mask: u64,
    pub stx_atime: StatxTimestamp,
    pub stx_btime: StatxTimestamp,
    pub stx_ctime: StatxTimestamp,
    pub stx_mtime: StatxTimestamp,
    pub stx_rdev_major: u32,
    pub stx_rdev_minor: u32,
    pub stx_dev_major: u32,
    pub stx_dev_minor: u32,
    pub stx_mnt_id: u64,
    pub stx_dio_mem_align: u32,
    pub stx_dio_offset_align: u32,
    pub stx_subvol: u64,
    pub stx_atomic_write_unit_min: u32,
    pub stx_atomic_write_unit_max: u32,
    pub stx_atomic_write_segments_max: u32,
    pub __spare2: [u32; 1],
    pub __spare3: [u64; 9],
}

pub(super) fn stat_internal(path: &str, st: &mut Stat) -> u64 {
    match crate::fs::lookup(path) {
        Some(v) => {
            fill_stat_from_vnode(st as *mut Stat, &v);
            0
        }
        None => ENOENT,
    }
}

pub(super) fn fstat_internal(fd: usize, st: &mut Stat) -> u64 {
    if fd <= 2 {
        st.st_mode = 0o020000 | 0o666;
        st.st_rdev = (4 << 8) | 0; // makedev(4, 0)
        st.st_nlink = 1;
        st.st_ino = 40;
        st.st_dev = 1;
        return 0;
    }
    let thread = match crate::tasks::current_thread() {
        Some(t) => t,
        None => return EBADF,
    };
    let arc_opt = {
        let p = thread.process.lock();
        if fd < p.fd_table.len() {
            p.fd_table[fd].clone()
        } else {
            None
        }
    };
    if let Some(arc_vnode) = arc_opt {
        let open_file = arc_vnode.lock();
        fill_stat_from_vnode(st as *mut Stat, &open_file.vnode);
        0
    } else {
        EBADF
    }
}


pub(super) fn sys_stat_path(path_ptr: u64, statbuf: *mut Stat) -> u64 {
    if !validate_user_write_range(statbuf as u64, core::mem::size_of::<Stat>()) {
        return EFAULT;
    }
    let path = match read_user_cstring(path_ptr) {
        Some(path) => path,
        None => return EFAULT,
    };
    let mut st = Stat::default();
    let ret = stat_internal(&path, &mut st);
    if ret == 0 {
        unsafe {
            core::ptr::write(statbuf, st);
        }
    }
    ret
}

pub(super) fn sys_fstat_fd(fd: usize, statbuf: *mut Stat) -> u64 {
    if !validate_user_write_range(statbuf as u64, core::mem::size_of::<Stat>()) {
        return EFAULT;
    }
    let mut st = Stat::default();
    let ret = fstat_internal(fd, &mut st);
    if ret == 0 {
        unsafe {
            core::ptr::write(statbuf, st);
        }
    }
    ret
}

pub(super) fn sys_statx(dirfd: i32, path_ptr: u64, flags: u32, _mask: u32, statxbuf: *mut Statx) -> u64 {
    const AT_FDCWD: i32 = -100;
    const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
    const AT_NO_AUTOMOUNT: u32 = 0x800;
    const AT_EMPTY_PATH: u32 = 0x1000;

    let valid_flags = AT_SYMLINK_NOFOLLOW | AT_NO_AUTOMOUNT | AT_EMPTY_PATH | 0x6000;
    if flags & !valid_flags != 0 {
        return EINVAL;
    }
    if !validate_user_write_range(statxbuf as u64, core::mem::size_of::<Statx>()) {
        return EFAULT;
    }
    let path = match read_user_cstring(path_ptr) {
        Some(p) => p,
        None => return EFAULT,
    };
    let mut st = Stat::default();
    let ret = if path.is_empty() && (flags & AT_EMPTY_PATH) != 0 {
        if dirfd == AT_FDCWD {
            stat_internal("/", &mut st)
        } else {
            fstat_internal(dirfd as usize, &mut st)
        }
    } else {
        let full_path = match resolve_dirfd_path(dirfd, &path) {
            Some(p) => p,
            None => return EBADF,
        };
        stat_internal(&full_path, &mut st)
    };

    if ret != 0 {
        crate::dprintln!("sys_statx: dirfd={} path='{}' -> err={}", dirfd, path, ret);
        return ret;
    }

    let mut sx = Statx::default();
    sx.stx_mask = 0x7ff | 0x1000 | 0x4000; // STATX_BASIC_STATS | STATX_MNT_ID | STATX_MNT_ID_UNIQUE
    sx.stx_blksize = if st.st_blksize > 0 { st.st_blksize as u32 } else { 4096 };
    sx.stx_nlink = st.st_nlink as u32;
    sx.stx_uid = st.st_uid;
    sx.stx_gid = st.st_gid;
    sx.stx_mode = st.st_mode as u16;
    sx.stx_ino = if st.st_ino != 0 { st.st_ino } else { 1 };
    sx.stx_size = if st.st_size >= 0 { st.st_size as u64 } else { 0 };
    sx.stx_blocks = if st.st_blocks >= 0 { st.st_blocks as u64 } else { 0 };
    sx.stx_atime = StatxTimestamp { tv_sec: st.st_atime as i64, tv_nsec: 0, __reserved: 0 };
    sx.stx_mtime = StatxTimestamp { tv_sec: st.st_mtime as i64, tv_nsec: 0, __reserved: 0 };
    sx.stx_ctime = StatxTimestamp { tv_sec: st.st_ctime as i64, tv_nsec: 0, __reserved: 0 };
    sx.stx_rdev_major = (st.st_rdev >> 8) as u32;
    sx.stx_rdev_minor = (st.st_rdev & 0xff) as u32;
    sx.stx_dev_major = 0;
    sx.stx_dev_minor = 1;
    sx.stx_mnt_id = 1;

    unsafe {
        core::ptr::write(statxbuf, sx);
    }
    crate::dprintln!("sys_statx: dirfd={} path='{}' -> OK (ino={}, mode={:#o})", dirfd, path, sx.stx_ino, sx.stx_mode);
    0
}

pub const SYSFS_MAGIC: u64 = 0x62656572;
pub const EXT4_SUPER_MAGIC: u64 = 0xef53;
pub const TMPFS_MAGIC: u64 = 0x01021994;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Statfs {
    pub f_type: u64,
    pub f_bsize: u64,
    pub f_blocks: u64,
    pub f_bfree: u64,
    pub f_bavail: u64,
    pub f_files: u64,
    pub f_ffree: u64,
    pub f_fsid: [i32; 2],
    pub f_namelen: u64,
    pub f_frsize: u64,
    pub f_flags: u64,
    pub f_spare: [u64; 4],
}

pub(super) fn sys_statfs(path_ptr: u64, buf_ptr: *mut Statfs) -> u64 {
    if !validate_user_write_range(buf_ptr as u64, core::mem::size_of::<Statfs>()) {
        return EFAULT;
    }
    let path = match read_user_cstring(path_ptr) {
        Some(p) => p,
        None => return EFAULT,
    };
    let magic = if path.starts_with("/sys") || path.starts_with("sys") {
        SYSFS_MAGIC
    } else if path.starts_with("/tmp") || path.starts_with("tmp") || path.starts_with("/run") || path.starts_with("run") {
        TMPFS_MAGIC
    } else {
        EXT4_SUPER_MAGIC
    };
    let mut sf = Statfs::default();
    sf.f_type = magic;
    sf.f_bsize = 4096;
    sf.f_blocks = 1024 * 1024;
    sf.f_bfree = 512 * 1024;
    sf.f_bavail = 512 * 1024;
    sf.f_files = 100000;
    sf.f_ffree = 90000;
    sf.f_namelen = 255;
    sf.f_frsize = 4096;
    unsafe {
        core::ptr::write(buf_ptr, sf);
    }
    crate::dprintln!("sys_statfs: path='{}' magic={:#x}", path, magic);
    0
}

pub(super) fn sys_fstatfs(fd: usize, buf_ptr: *mut Statfs) -> u64 {
    if !validate_user_write_range(buf_ptr as u64, core::mem::size_of::<Statfs>()) {
        return EFAULT;
    }
    let thread = match crate::tasks::current_thread() {
        Some(t) => t,
        None => return EBADF,
    };
    let file_opt = {
        let p = thread.process.lock();
        if fd < p.fd_table.len() {
            p.fd_table[fd].clone()
        } else {
            None
        }
    };
    let magic = if let Some(arc_vnode) = file_opt {
        let open_file = arc_vnode.lock();
        if open_file.path.starts_with("/sys") || open_file.path.starts_with("sys") {
            SYSFS_MAGIC
        } else if open_file.path.starts_with("/tmp") || open_file.path.starts_with("tmp") || open_file.path.starts_with("/run") || open_file.path.starts_with("run") {
            TMPFS_MAGIC
        } else {
            EXT4_SUPER_MAGIC
        }
    } else {
        EXT4_SUPER_MAGIC
    };
    let mut sf = Statfs::default();
    sf.f_type = magic;
    sf.f_bsize = 4096;
    sf.f_blocks = 1024 * 1024;
    sf.f_bfree = 512 * 1024;
    sf.f_bavail = 512 * 1024;
    sf.f_files = 100000;
    sf.f_ffree = 90000;
    sf.f_namelen = 255;
    sf.f_frsize = 4096;
    unsafe {
        core::ptr::write(buf_ptr, sf);
    }
    crate::dprintln!("sys_fstatfs: fd={} magic={:#x}", fd, magic);
    0
}

#[repr(C)]
struct FbInfoSys {
    phys_addr: u64,
    width: u32,
    height: u32,
    pitch: u32,
    bpp: u8,
}

pub(super) fn sys_get_fb_info(arg1: u64) -> u64 {
    if !validate_user_write_range(arg1, core::mem::size_of::<FbInfoSys>()) {
        return EFAULT;
    }
    
    let info = unsafe { crate::graphics::FRAMEBUFFER_INFO };
    if let Some(fb) = info {
        let mut alloc = FRAME_ALLOCATOR.lock();
        let mut mapper = mapper_for(read_cr3());
        let page_count = (fb.size as usize + PAGE_SIZE as usize - 1) / PAGE_SIZE as usize;
        let _ = mapper.map_range(
            &mut *alloc,
            VirtAddr(fb.address.0),
            PhysAddr(fb.address.0),
            page_count * PAGE_SIZE as usize,
            PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER,
        );
        drop(alloc);
        unsafe {
            write_cr3(read_cr3());
        }

        let ptr = arg1 as *mut FbInfoSys;
        unsafe {
            (*ptr).phys_addr = fb.address.0;
            (*ptr).width = fb.width as u32;
            (*ptr).height = fb.height as u32;
            (*ptr).pitch = fb.stride as u32;
            (*ptr).bpp = 32;
        }
        0
    } else {
        ENOSYS
    }
}

pub(super) fn sys_access(path_ptr: u64, mode: u32) -> u64 {
    const AT_FDCWD: i32 = -100;
    sys_faccessat(AT_FDCWD, path_ptr, mode, 0)
}

pub(super) fn sys_faccessat(dirfd: i32, path_ptr: u64, _mode: u32, _flags: u32) -> u64 {
    let path = match read_user_cstring(path_ptr) {
        Some(p) => p,
        None => return EFAULT,
    };
    if path.is_empty() {
        return ENOENT;
    }
    let full_path = match resolve_dirfd_path(dirfd, &path) {
        Some(p) => p,
        None => return EBADF,
    };
    match crate::fs::lookup(&full_path) {
        Some(_) => 0,
        None => ENOENT,
    }
}

pub(super) fn sys_unlinkat(dirfd: i32, path_ptr: u64, flags: u32) -> u64 {
    const AT_REMOVEDIR: u32 = 0x200;
    if flags & !AT_REMOVEDIR != 0 {
        return EINVAL;
    }
    let path = match read_user_cstring(path_ptr) {
        Some(p) => p,
        None => return EFAULT,
    };
    if path.is_empty() {
        return ENOENT;
    }
    let full_path = match resolve_dirfd_path(dirfd, &path) {
        Some(p) => p,
        None => return EBADF,
    };
    let clean = full_path.trim_start_matches('/');
    if clean.is_empty() {
        return EBUSY;
    }
    let must_be_dir = (flags & AT_REMOVEDIR) != 0;
    let target_vn = match crate::fs::lookup_checked(&full_path) {
        Ok(v) => v,
        Err(crate::fs::LookupError::Denied) => return EACCES,
        Err(_) => return ENOENT,
    };
    match &target_vn {
        VNode::FsNode { node, .. } => {
            if must_be_dir && !node.is_dir() {
                return ENOTDIR;
            }
            if !must_be_dir && node.is_dir() {
                return EISDIR;
            }
        }
        _ => {
            if must_be_dir {
                return ENOTDIR;
            }
            return EPERM;
        }
    }
    let creds = match crate::fs::perm::current_creds() {
        Some(c) => c,
        None => return EPERM,
    };
    let (parent_rel, name) = match clean.rfind('/') {
        Some(pos) => (&clean[..pos], &clean[pos + 1..]),
        None => ("", clean),
    };
    if name == "." || name == ".." {
        return if must_be_dir { EINVAL } else { EISDIR };
    }
    let parent_vn = if parent_rel.is_empty() {
        crate::fs::lookup("/")
    } else {
        crate::fs::lookup(&alloc::format!("/{}", parent_rel))
    };
    match parent_vn {
        Some(VNode::FsNode { node, .. }) => {
            // The directory needs w+x, and the sticky bit further restricts
            // who may remove an entry they do not own (Linux's /tmp rule).
            let parent_perms = crate::fs::perm::perms_of(node.as_ref());
            if !crate::fs::perm::check(&parent_perms, &creds, crate::fs::perm::Access::Remove)
                .is_allow()
            {
                return EACCES;
            }
            let entry_owner = match &target_vn {
                VNode::FsNode { node, .. } => crate::fs::perm::perms_of(node.as_ref()).uid,
                _ => return EPERM,
            };
            if !crate::fs::perm::check_sticky(&parent_perms, entry_owner, &creds).is_allow() {
                return EPERM;
            }
            match node.unlink(name) {
                Ok(()) => {
                    crate::arch::x86_64::syscalls::net::remove_listener(&full_path);
                    crate::fs::dentry_remove(&full_path);
                    0 },
                Err(e) => if e == 0 { ENOTEMPTY } else { e },
            }
        }
        Some(_) => ENOTDIR,
        None => ENOENT,
    }
}

pub(super) fn sys_unlink(path_ptr: u64) -> u64 {
    const AT_FDCWD: i32 = -100;
    sys_unlinkat(AT_FDCWD, path_ptr, 0)
}

pub(super) fn sys_rmdir(path_ptr: u64) -> u64 {
    const AT_FDCWD: i32 = -100;
    sys_unlinkat(AT_FDCWD, path_ptr, 0x200)
}

/// `close_range(first, last, flags)`: closes every fd in `first..=last`.
/// Only `flags == 0` (or CLOSE_RANGE_UNSHARE, a no-op here) is accepted.
pub(super) fn sys_close_range(first: u32, last: u32, flags: u32) -> u64 {
    const CLOSE_RANGE_UNSHARE: u32 = 0x2;
    if flags & !CLOSE_RANGE_UNSHARE != 0 {
        return EINVAL;
    }
    let thread = match crate::tasks::current_thread() {
        Some(t) => t,
        None => return ESRCH,
    };
    let mut to_close = alloc::vec::Vec::new();
    {
        let mut p = thread.process.lock();
        let max_fd = (last as usize).min(p.fd_table.len().saturating_sub(1));
        if (first as usize) <= max_fd {
            for fd in (first as usize)..=max_fd {
                if let Some(v) = p.fd_table[fd].take() {
                    to_close.push(v);
                }
            }
        }
    }
    for v in to_close {
        let vnode_kind = { v.lock().vnode.clone() };
        if matches!(vnode_kind, VNode::Pipe { .. }) {
            wake_pipe_waiters();
        }
        // ponytail: same last-close pty release as sys_close.
        if Arc::strong_count(&v) == 1 {
            match &vnode_kind {
                VNode::PtyMaster(pty) => crate::fs::pty::pty_release(pty, true),
                VNode::PtySlave(pty) => crate::fs::pty::pty_release(pty, false),
                _ => {}
            }
        }
        if matches!(vnode_kind, crate::fs::vfs::VNode::DspAudioDevice | crate::fs::vfs::VNode::SndPlaybackDevice) {
            crate::drivers::audio::vfs_node::dsp_flush();
        }
        // ponytail: same per-open stream cleanup as sys_close (last close
        // only — see sys_close: spawn-child cleanup must not deaf the parent).
        if Arc::strong_count(&v) == 1
            && matches!(vnode_kind, crate::fs::vfs::VNode::InputEventDevice { .. } | crate::fs::vfs::VNode::InputMiceDevice { .. })
        {
            crate::drivers::keyboard::input_unregister(Arc::as_ptr(&v) as usize);
        }
    }
    0
}

pub(super) fn sys_fstatat(dirfd: i32, path_ptr: u64, statbuf: *mut Stat, flags: u32) -> u64 {
    const AT_FDCWD: i32 = -100;
    const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
    const AT_NO_AUTOMOUNT: u32 = 0x800;
    const AT_EMPTY_PATH: u32 = 0x1000;

    let valid_flags = AT_SYMLINK_NOFOLLOW | AT_NO_AUTOMOUNT | AT_EMPTY_PATH | 0x6000;
    if flags & !valid_flags != 0 {
        return EINVAL;
    }
    if !validate_user_write_range(statbuf as u64, core::mem::size_of::<Stat>()) {
        return EFAULT;
    }
    let path = match read_user_cstring(path_ptr) {
        Some(p) => p,
        None => return EFAULT,
    };
    let mut st = Stat::default();
    let ret = if path.is_empty() && (flags & AT_EMPTY_PATH) != 0 {
        if dirfd == AT_FDCWD {
            stat_internal("/", &mut st)
        } else {
            fstat_internal(dirfd as usize, &mut st)
        }
    } else {
        let full_path = match resolve_dirfd_path(dirfd, &path) {
            Some(p) => p,
            None => {
                crate::dprintln!("sys_fstatat: dirfd={} path='{}' -> EBADF", dirfd, path);
                return EBADF;
            }
        };
        stat_internal(&full_path, &mut st)
    };
    if ret == 0 {
        unsafe {
            core::ptr::write(statbuf, st);
        }
    }
    crate::dprintln!("sys_fstatat: dirfd={} path='{}' -> ret={}", dirfd, path, ret);
    ret
}

/// `mkdir(path, mode)`: creates a directory via the mounted fs.
pub(super) fn sys_mkdir(path_ptr: u64, mode: u32) -> u64 {
    // mkdir(2) is mkdirat(AT_FDCWD, path, mode).
    sys_mkdirat(-100, path_ptr, mode) // AT_FDCWD
}

/// `mkdirat(2)` (x86-64 258).
///
/// Needed in its own right because glibc's `fs::create_dir` calls it directly,
/// not `mkdir`. Without it, syscall 258 fell through to the dispatcher's
/// default arm and returned ENOSYS, so every glibc program that created a
/// directory died at the call — zellij at its very first statement, creating
/// its temp dir, with the panic pointing at its own logging setup rather than
/// at the missing syscall.
pub(super) fn sys_mkdirat(dirfd: i32, path_ptr: u64, mode: u32) -> u64 {
    const AT_FDCWD_LOCAL: i32 = -100;
    let path = match read_user_cstring(path_ptr) {
        Some(p) => p,
        None => return EFAULT,
    };
    if path.is_empty() {
        return ENOENT;
    }
    // A dirfd other than AT_FDCWD names an open directory; resolve against it
    // rather than treating the path as absolute.
    let full_path = if dirfd == AT_FDCWD_LOCAL {
        path.clone()
    } else {
        match resolve_dirfd_path(dirfd, &path) {
            Some(p) => p,
            None => return EBADF,
        }
    };
    if crate::fs::lookup(&full_path).is_some() {
        return EEXIST;
    }
    // The parent directory needs w+x on it; the umask and the new entry's
    // owner are applied inside fs::create.
    let creds = crate::fs::perm::current_creds();
    if let Some(creds) = creds.as_ref() {
        if check_create_dir(&full_path, creds) != 0 {
            return EACCES;
        }
    }
    match crate::fs::create(&full_path, true, mode as u32 & 0o7777) {
        Some(_) => 0,
        None => ENOENT,
    }
}

pub(super) fn sys_pread64(fd: usize, buf_ptr: u64, count: usize, offset: u64) -> u64 {
    if !validate_user_write_range(buf_ptr, count) {
        return EFAULT;
    }
    let thread = match crate::tasks::current_thread() {
        Some(t) => t,
        None => return EBADF,
    };
    let file = {
        let p = thread.process.lock();
        match p.fd_table.get(fd).cloned().flatten() {
            Some(f) => f,
            None => return EBADF,
        }
    };
    let file_guard = file.lock();
    match &file_guard.vnode {
        VNode::InitrdFile { data, .. } => {
            let off = offset as usize;
            if off >= data.len() {
                return 0;
            }
            let avail = data.len() - off;
            let n = core::cmp::min(avail, count);
            let user_buf = unsafe { core::slice::from_raw_parts_mut(buf_ptr as *mut u8, count) };
            user_buf[..n].copy_from_slice(&data[off..off + n]);
            n as u64
        }
        VNode::FsNode { node, .. } => {
            let user_buf = unsafe { core::slice::from_raw_parts_mut(buf_ptr as *mut u8, count) };
            match node.read_at(offset as usize, user_buf) {
                Ok(bytes) => bytes as u64,
                Err(_) => EIO,
            }
        }
        _ => EINVAL,
    }
}

pub(super) fn sys_pwrite64(fd: usize, buf_ptr: u64, count: usize, offset: u64) -> u64 {
    if !validate_user_read_range(buf_ptr, count) {
        return EFAULT;
    }
    let thread = match crate::tasks::current_thread() {
        Some(t) => t,
        None => return EBADF,
    };
    let file = {
        let p = thread.process.lock();
        match p.fd_table.get(fd).cloned().flatten() {
            Some(f) => f,
            None => return EBADF,
        }
    };
    let mut file_guard = file.lock();
    match &mut file_guard.vnode {
        VNode::FsNode { node, .. } => {
            let user_buf = unsafe { core::slice::from_raw_parts(buf_ptr as *const u8, count) };
            match node.write_at(offset as usize, user_buf) {
                Ok(bytes) => bytes as u64,
                Err(_) => EIO,
            }
        }
        _ => EINVAL,
    }
}

pub(super) fn sys_readlink(pathname_ptr: u64, buf_ptr: *mut u8, bufsiz: usize) -> u64 {
    let path = match read_user_cstring(pathname_ptr) {
        Some(path) => path,
        None => return EFAULT,
    };
    crate::serial_macro::_print(format_args!("[READLINK] path='{}'\n", path));
    sys_readlinkat(AT_FDCWD, pathname_ptr, buf_ptr, bufsiz)
}

pub(super) fn sys_readlinkat(dirfd: i32, pathname_ptr: u64, buf_ptr: *mut u8, bufsiz: usize) -> u64 {
    if bufsiz == 0 {
        return EINVAL;
    }
    if !validate_user_write_range(buf_ptr as u64, bufsiz) {
        return EFAULT;
    }
    let path = match read_user_cstring(pathname_ptr) {
        Some(p) => p,
        None => return EFAULT,
    };
    let full_path = match resolve_dirfd_path(dirfd, &path) {
        Some(p) => p,
        None => return EBADF,
    };

    if full_path == "/proc/self/exe" {
        let exe = match crate::tasks::current_thread() {
            Some(t) => {
                let p = t.process.lock();
                if !p.exe_path.is_empty() {
                    p.exe_path.clone()
                } else {
                    alloc::string::String::from("/bin/mywima")
                }
            }
            None => alloc::string::String::from("/bin/mywima"),
        };
        let bytes = exe.as_bytes();
        let len = bytes.len().min(bufsiz);
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), buf_ptr, len);
        }
        return len as u64;
    }

    if full_path.starts_with("/proc/self/fd/") || (full_path.starts_with("/proc/") && full_path.contains("/fd/")) {
        crate::serial_macro::_print(format_args!("sys_readlinkat: MATCHED proc/fd for '{}'\n", full_path));
        if let Some(fd_str) = full_path.rsplit('/').next() {
            if let Ok(fd_num) = fd_str.parse::<usize>() {
                if let Some(thread) = crate::tasks::current_thread() {
                    let p = thread.process.lock();
                    if let Some(Some(slot)) = p.fd_table.get(fd_num) {
                        let file = slot.lock();
                        let target = if !file.path.is_empty() {
                            file.path.clone()
                        } else {
                            alloc::string::String::from("/dev/null")
                        };
                        let bytes = target.as_bytes();
                        let len = bytes.len().min(bufsiz);
                        unsafe {
                            core::ptr::copy_nonoverlapping(bytes.as_ptr(), buf_ptr, len);
                        }
                        crate::serial_macro::_print(format_args!("sys_readlinkat: {} -> '{}'\n", full_path, target));
                        return len as u64;
                    } else {
                        crate::serial_macro::_print(format_args!("sys_readlinkat: {} ENOENT (fd {} not in table)\n", full_path, fd_num));
                    }
                } else {
                    crate::serial_macro::_print(format_args!("sys_readlinkat: {} ENOENT (no current_thread)\n", full_path));
                }
            }
        }
        return ENOENT;
    }

    let node = match crate::fs::lookup(&full_path) {
        Some(n) => n,
        None => return ENOENT,
    };

    match &node {
        VNode::FsNode { node, .. } => {
            match node.read_link() {
                Ok(target) => {
                    let bytes = target.as_bytes();
                    let len = bytes.len().min(bufsiz);
                    unsafe {
                        core::ptr::copy_nonoverlapping(bytes.as_ptr(), buf_ptr, len);
                    }
                    crate::dprintln!("sys_readlinkat: '{}' -> '{}'", full_path, target);
                    len as u64
                }
                Err(err) => {
                    crate::dprintln!("sys_readlinkat: '{}' not symlink, err={}", full_path, err);
                    err
                }
            }
        }
        _ => EINVAL,
    }
}


pub(super) fn sys_chmod(path_ptr: u64, mode: u32) -> u64 {
    let path = match read_user_cstring(path_ptr) {
        Some(p) => p,
        None => return EFAULT,
    };
    let vnode = match crate::fs::lookup_checked(&path) {
        Ok(v) => v,
        Err(crate::fs::LookupError::Denied) => return EACCES,
        Err(_) => return ENOENT,
    };
    let creds = match crate::fs::perm::current_creds() {
        Some(c) => c,
        None => return EPERM,
    };
    let perms = match &vnode {
        VNode::FsNode { node, .. } => crate::fs::perm::perms_of(node.as_ref()),
        _ => return EPERM,
    };
    // Only the owner (or CAP_FOWNER) may change the mode.
    if !crate::fs::perm::check(&perms, &creds, crate::fs::perm::Access::Chmod).is_allow() {
        return EPERM;
    }
    match vnode.chmod(mode) {
        Ok(()) => 0,
        Err(e) => e,
    }
}

/// `posix_openpt(2)` (x86-64 622).
///
/// Returns an fd for a new PTY master. The POSIX counterpart to opening
/// `/dev/ptmx`, and the one zellij uses: glibc routes `posix_openpt` straight
/// to this syscall rather than to `open(2)`. It was undispatched, so the call
/// returned ENOSYS and zellij stalled at "spawning tabs" — no panic, no error
/// message, just no panes and no `/dev/ptmx` in the log.
///
/// `ptyrun` was unaffected because it opens `/dev/ptmx` directly, which is why
/// PTYs looked healthy while zellij could not get one.
pub(super) fn sys_posix_openpt(flags: u32) -> u64 {
    // Linux rejects a master opened for writing only; O_RDWR or O_RDONLY.
    if flags & 0o3 == 0o1 {
        return EINVAL;
    }
    let (id, pty) = crate::fs::pty::alloc_pty();
    crate::fs::pty::register_master_fd(id, flags, pty)
}

/// `grantpt(2)` (x86-64 624).
///
/// Linux changes the slave's ownership to the caller and its mode to
/// `0666`-minus-umask. There is no separate owner here — everything runs as
/// uid 0 — so this is a success unless there is no such pty, which the
/// kernel cannot know from the arguments alone. Accepted as a no-op.
pub(super) fn sys_grantpt(_fd: u32) -> u64 {
    0
}

/// `unlockpt(2)` (x86-64 625).
///
/// Unlocks the slave so it can be opened. The unlock state lives on the pair
/// (`PtyPair::locked`), and clearing it here is what lets `/dev/pts/<n>` be
/// opened by the next step.
pub(super) fn sys_unlockpt(fd: u32) -> u64 {
    crate::fs::pty::unlock_master(fd)
}

/// `ptsname_r(2)` (x86-64 626).
///
/// Writes the slave's path into `buf` and returns its length, or the number of
/// bytes that *would* be needed if the buffer is too small (`ERANGE`).
/// `ptsname(2)` (627) is the non-reentrant wrapper in libc.
///
/// NOTE: glibc's `ptsname_r` does not issue this syscall. It composes the path
/// itself from `ioctl(TIOCGPTN)`, which is already handled — so on this kernel
/// this arm is reachable only from a caller that issues 626 directly. It is
/// kept because the number is part of the ABI and a program is entitled to call
/// it, but the route a real glibc program takes is `TIOCGPTN`.
pub(super) fn sys_ptsname_r(fd: u32, buf: u64, buflen: usize) -> u64 {
    let Some(id) = crate::fs::pty::master_id(fd) else {
        return EBADF;
    };
    let path = alloc::format!("/dev/pts/{}", id);
    // Linux returns ERANGE and the required length, writing nothing.
    if buflen < path.len() {
        return crate::arch::x86_64::syscalls::ERANGE;
    }
    if !validate_user_write_range(buf, path.len()) {
        return EFAULT;
    }
    unsafe {
        core::ptr::copy_nonoverlapping(path.as_ptr(), buf as *mut u8, path.len());
    }
    path.len() as u64
}

/// `fchmodat(2)` (x86-64 268).
///
/// Linux's `chmod(2)` is `fchmodat(AT_FDCWD, path, mode)`, and Rust's
/// `fs::set_permissions` uses the same shape. This number was not in the
/// dispatcher at all, so those calls fell through to the ENOSYS default arm
/// and returned "Function not implemented" — which is what killed zellij at
/// `set_permissions` on its own temp dir, after `create_dir` had already
/// succeeded.
/// `umask(2)` (x86-64 95).
///
/// Returns the *previous* mask and installs the new one. Both halves matter:
/// the return value is how a program saves and restores a mask around a
/// temporary change, so returning 0 unconditionally would be wrong.
pub(super) fn sys_umask(new_mask: u32) -> u64 {
    // Only the permission bits are meaningful; Linux masks the argument to
    // 0o777 and ignores the rest.
    let new_mask = new_mask & 0o777;
    let proc = match crate::tasks::current_thread() {
        Some(t) => t,
        None => return 0,
    };
    // current_thread() returns a clone, so the change must be written back.
    let t = proc;
    let old = {
        let mut p = t.process.lock();
        let old = p.umask;
        p.umask = new_mask;
        old
    };
    crate::tasks::set_current_thread(Some(t));
    old as u64
}

pub(super) fn sys_fchmodat(dirfd: i32, path_ptr: u64, mode: u32) -> u64 {
    const AT_FDCWD_LOCAL: i32 = -100;
    let path = match read_user_cstring(path_ptr) {
        Some(p) => p,
        None => return EFAULT,
    };
    let full_path = if dirfd == AT_FDCWD_LOCAL {
        path
    } else {
        match resolve_dirfd_path(dirfd, &path) {
            Some(p) => p,
            None => return EBADF,
        }
    };
    // Linux accepts AT_SYMLINK_NOFOLLOW (0x100) in flags, but flags is not
    // passed here; chmod on a symlink's target is the default and matches
    // plain chmod(2).
    match crate::fs::lookup(&full_path) {
        Some(vnode) => match vnode.chmod(mode) {
            Ok(()) => 0,
            Err(e) => e,
        },
        None => ENOENT,
    }
}

/// `fchownat(2)` — what glibc's `chown(2)` actually calls.
///
/// Without it `chown` fell through to the dispatcher's ENOSYS default, so no
/// program could give a file a group of its own. `(uid, gid)` of `u32::MAX`
/// means "leave unchanged", as in Linux.
pub(super) fn sys_fchownat(dirfd: i32, path_ptr: u64, uid: u32, gid: u32, _flags: u32) -> u64 {
    const AT_FDCWD_LOCAL: i32 = -100;
    let path = match read_user_cstring(path_ptr) {
        Some(p) => p,
        None => return EFAULT,
    };
    let full_path = if dirfd == AT_FDCWD_LOCAL {
        path
    } else {
        match resolve_dirfd_path(dirfd, &path) {
            Some(p) => p,
            None => return EBADF,
        }
    };
    let vnode = match crate::fs::lookup_checked(&full_path) {
        Ok(v) => v,
        Err(crate::fs::LookupError::Denied) => return EACCES,
        Err(_) => return ENOENT,
    };
    let creds = match crate::fs::perm::current_creds() {
        Some(c) => c,
        None => return EPERM,
    };
    let perms = match &vnode {
        VNode::FsNode { node, .. } => crate::fs::perm::perms_of(node.as_ref()),
        _ => return EPERM,
    };
    // Changing either id is privileged. The single exception Linux grants is a
    // *owner* changing the **group**, and only to a group it belongs to.
    let privileged = crate::fs::perm::check(&perms, &creds, crate::fs::perm::Access::Chown)
        == crate::fs::perm::Decision::Allow;
    if !privileged {
        let owner_ok = creds.uid == perms.uid;
        let uid_unchanged = uid == u32::MAX || uid == perms.uid;
        let group_ok = gid == u32::MAX || creds.in_group(gid);
        if !(owner_ok && uid_unchanged && group_ok) {
            return EPERM;
        }
    }
    let new_uid = if uid == u32::MAX { perms.uid } else { uid };
    let new_gid = if gid == u32::MAX { perms.gid } else { gid };
    match vnode.chown(new_uid, new_gid) {
        Ok(()) => 0,
        Err(e) => e,
    }
}

pub(super) fn sys_fchmod(fd: usize, mode: u32) -> u64 {
    let t = match crate::tasks::current_thread() {
        Some(t) => t,
        None => return EBADF,
    };
    let p = t.process.lock();
    if let Some(Some(vnode)) = p.fd_table.get(fd) {
        let f = vnode.lock(); match f.vnode.chmod(mode) {
            Ok(_) => 0,
            Err(e) => e,
        }
    } else {
        EBADF
    }
}

pub(super) fn sys_chown(path_ptr: u64, uid: u32, gid: u32) -> u64 {
    let path = match read_user_cstring(path_ptr) {
        Some(p) => p,
        None => return EFAULT,
    };
    let vnode = match crate::fs::lookup_checked(&path) {
        Ok(v) => v,
        Err(crate::fs::LookupError::Denied) => return EACCES,
        Err(_) => return ENOENT,
    };
    let creds = match crate::fs::perm::current_creds() {
        Some(c) => c,
        None => return EPERM,
    };
    let perms = match &vnode {
        VNode::FsNode { node, .. } => crate::fs::perm::perms_of(node.as_ref()),
        _ => return EPERM,
    };
    // Changing either id is privileged. The single exception Linux grants is a
    // *owner* changing the **group**, and only to a group it belongs to.
    let privileged =
        crate::fs::perm::check(&perms, &creds, crate::fs::perm::Access::Chown)
            == crate::fs::perm::Decision::Allow;
    if !privileged {
        let owner_ok = creds.uid == perms.uid;
        let uid_unchanged = uid == u32::MAX || uid == perms.uid;
        let group_ok = gid == u32::MAX || creds.in_group(gid);
        if !(owner_ok && uid_unchanged && group_ok) {
            return EPERM;
        }
    }
    let new_uid = if uid == u32::MAX { perms.uid } else { uid };
    let new_gid = if gid == u32::MAX { perms.gid } else { gid };
    match vnode.chown(new_uid, new_gid) {
        Ok(()) => 0,
        Err(e) => e,
    }
}

pub(super) fn sys_fchown(fd: usize, uid: u32, gid: u32) -> u64 {
    let t = match crate::tasks::current_thread() {
        Some(t) => t,
        None => return EBADF,
    };
    let p = t.process.lock();
    if let Some(Some(vnode)) = p.fd_table.get(fd) {
        let f = vnode.lock(); match f.vnode.chown(uid, gid) {
            Ok(_) => 0,
            Err(e) => e,
        }
    } else {
        EBADF
    }
}

pub(super) fn sys_rename(oldpath_ptr: u64, newpath_ptr: u64) -> u64 {
    let oldpath = match read_user_cstring(oldpath_ptr) {
        Some(p) => p,
        None => return EFAULT,
    };
    let newpath = match read_user_cstring(newpath_ptr) {
        Some(p) => p,
        None => return EFAULT,
    };
    let creds = match crate::fs::perm::current_creds() {
        Some(c) => c,
        None => return EPERM,
    };
    // Resolve both paths; lookup_checked enforces the per-component x rule and
    // reports EACCES (not ENOENT) for an unsearchable directory.
    let old_vn = match crate::fs::lookup_checked(&oldpath) {
        Ok(v) => v,
        Err(crate::fs::LookupError::Denied) => return EACCES,
        Err(_) => return ENOENT,
    };
    let new_vn = match crate::fs::lookup_checked(&newpath) {
        Ok(v) => Some(v),
        Err(crate::fs::LookupError::Denied) => return EACCES,
        Err(_) => None,
    };
    // The parent directory of a path: the directory whose w bit gates adding
    // or removing the leaf name.
    let parent_of = |path: &str| -> Option<crate::fs::vfs::VNode> {
        let clean = path.trim_start_matches('/');
        match clean.rfind('/') {
            Some(idx) => {
                let parent = if idx == 0 { "/" } else { &clean[..idx] };
                crate::fs::lookup_checked(parent).ok()
            }
            None => crate::fs::lookup_checked("/").ok(),
        }
    };
    let old_parent = match parent_of(&oldpath) {
        Some(p) => p,
        None => return ENOENT,
    };
    let new_parent = match parent_of(&newpath) {
        Some(p) => p,
        None => return ENOENT,
    };
    // Removing the source entry needs w on the source parent and clears the
    // sticky-bit rule there.
    let old_owner = match &old_vn {
        VNode::FsNode { node, .. } => crate::fs::perm::perms_of(node.as_ref()).uid,
        _ => return EPERM,
    };
    let old_perms = match &old_parent {
        VNode::FsNode { node, .. } => crate::fs::perm::perms_of(node.as_ref()),
        _ => return EPERM,
    };
    if !crate::fs::perm::check(&old_perms, &creds, crate::fs::perm::Access::Remove).is_allow() {
        return EACCES;
    }
    if !crate::fs::perm::check_sticky(&old_perms, old_owner, &creds).is_allow() {
        return EPERM;
    }
    // Adding the destination entry needs w on the destination parent and,
    // if it already exists, clears its sticky-bit rule.
    if let Some(ref nv) = new_vn {
        let new_owner = match nv {
            VNode::FsNode { node, .. } => crate::fs::perm::perms_of(node.as_ref()).uid,
            _ => return EPERM,
        };
        let new_perms = match &new_parent {
            VNode::FsNode { node, .. } => crate::fs::perm::perms_of(node.as_ref()),
            _ => return EPERM,
        };
        if !crate::fs::perm::check(&new_perms, &creds, crate::fs::perm::Access::Create).is_allow() {
            return EACCES;
        }
        if !crate::fs::perm::check_sticky(&new_perms, new_owner, &creds).is_allow() {
            return EPERM;
        }
    }
    match &old_vn {
        VNode::FsNode { node, .. } => {
            if node.vfat_fs_ptr() != 0 {
                let target_parent = match &new_parent {
                    VNode::FsNode { node, .. } => node.vfat_dir_cluster(),
                    _ => None,
                };
                let target_fs = match &new_parent {
                    VNode::FsNode { node, .. } => node.vfat_fs_ptr(),
                    _ => 0,
                };
                let tfs = target_fs;
                const EXDEV: u64 = (-18i64) as u64; if tfs != 0 && tfs != node.vfat_fs_ptr() { return EXDEV; }
                match node.rename_with_target_parent(&newpath,target_parent) {
                    Ok(_) => 0,
                    Err(e) => e,
                }
            } else {
                match node.rename(&newpath) { Ok(_) => 0, Err(e)=>e }
            }
        },
        _ => EPERM,
    }
}

pub(super) fn sys_link(oldpath_ptr: u64, newpath_ptr: u64) -> u64 {
    let oldpath = match read_user_cstring(oldpath_ptr) {
        Some(p) => p,
        None => return EFAULT,
    };
    let newpath = match read_user_cstring(newpath_ptr) {
        Some(p) => p,
        None => return EFAULT,
    };
    let creds = match crate::fs::perm::current_creds() {
        Some(c) => c,
        None => return EPERM,
    };
    // Source must exist (traversal is enforced by lookup). link() itself does
    // not read the source, so no read check on it — only its existence.
    let target_node = match crate::fs::lookup(&oldpath) {
        Some(crate::fs::vfs::VNode::FsNode { node, .. }) => node,
        _ => return ENOENT,
    };
    // The directory getting the new link needs w+x on it.
    let last_slash = newpath.rfind('/');
    let (parent_path, name) = if let Some(idx) = last_slash {
        let parent = if idx == 0 { "/" } else { &newpath[..idx] };
        (parent, &newpath[idx + 1..])
    } else {
        ("/", newpath.as_str())
    };
    let parent_node = match crate::fs::lookup_checked(parent_path) {
        Ok(crate::fs::vfs::VNode::FsNode { node, .. }) => node,
        Err(crate::fs::LookupError::Denied) => return EACCES,
        Err(_) => return ENOENT,
        _ => return ENOTDIR,
    };
    let perms = crate::fs::perm::perms_of(&*parent_node);
    if !crate::fs::perm::check(&perms, &creds, crate::fs::perm::Access::Create).is_allow() {
        return EACCES;
    }
    match parent_node.link(name, target_node) {
        Ok(_) => 0,
        Err(e) => e,
    }
}

pub(super) fn sys_symlink(target_ptr: u64, linkpath_ptr: u64) -> u64 {
    let target = match read_user_cstring(target_ptr) {
        Some(p) => p,
        None => return EFAULT,
    };
    let linkpath = match read_user_cstring(linkpath_ptr) {
        Some(p) => p,
        None => return EFAULT,
    };

    let creds = match crate::fs::perm::current_creds() {
        Some(c) => c,
        None => return EPERM,
    };

    let last_slash = linkpath.rfind('/');
    let (parent_path, name) = if let Some(idx) = last_slash {
        let parent = if idx == 0 { "/" } else { &linkpath[..idx] };
        (parent, &linkpath[idx + 1..])
    } else {
        ("/", linkpath.as_str())
    };

    // The directory getting the new symlink needs w+x on it.
    let parent_node = match crate::fs::lookup_checked(parent_path) {
        Ok(crate::fs::vfs::VNode::FsNode { node, .. }) => node,
        Err(crate::fs::LookupError::Denied) => return EACCES,
        Err(_) => return ENOENT,
        _ => return ENOTDIR,
    };
    let perms = crate::fs::perm::perms_of(&*parent_node);
    if !crate::fs::perm::check(&perms, &creds, crate::fs::perm::Access::Create).is_allow() {
        return EACCES;
    }
    match parent_node.symlink(name, &target) {
        Ok(_) => 0,
        Err(e) => e,
    }
}

pub(super) fn sys_ftruncate(fd: usize, length: u64) -> u64 {
    let t = match crate::tasks::current_thread() {
        Some(t) => t,
        None => return EBADF,
    };
    let p = t.process.lock();
    if let Some(Some(vnode_arc)) = p.fd_table.get(fd) {
        let f = vnode_arc.lock();
        match f.vnode.truncate(length as usize) {
            Ok(_) => 0,
            Err(e) => e,
        }
    } else {
        EBADF
    }
}

/// fallocate(2): preallocate file space.
///
/// Modes:
///   FALLOC_FL_ALLOCATE_RANGE (0): extend the file to cover [offset, offset+len)
///     if needed (like truncate, but preserves existing data).
///   FALLOC_FL_KEEP_SIZE (1): allocate blocks without changing the file size.
///     For our non-sparse filesystems this is a no-op when the range is within
///     the current size; beyond it we cannot allocate without extending, so we
///     return success without changing size (best effort).
///   Other modes (PUNCH_HOLE, ZERO_RANGE, COLLAPSE_RANGE, INSERT_RANGE):
///     return EOPNOTSUPP — not supported by our VFS layer.
pub(super) fn sys_fallocate(fd: usize, mode: i32, offset: u64, len: u64) -> u64 {
    const FALLOC_FL_KEEP_SIZE: i32 = 0x01;
    const FALLOC_FL_PUNCH_HOLE: i32 = 0x02;
    const FALLOC_FL_COLLAPSE_RANGE: i32 = 0x08;
    const FALLOC_FL_ZERO_RANGE: i32 = 0x10;
    const FALLOC_FL_INSERT_RANGE: i32 = 0x20;

    // Only ALLOCATE_RANGE (0) and KEEP_SIZE are supported; the rest need
    // hole punching or range manipulation our VFS does not implement.
    let unsupported = FALLOC_FL_PUNCH_HOLE
        | FALLOC_FL_COLLAPSE_RANGE
        | FALLOC_FL_ZERO_RANGE
        | FALLOC_FL_INSERT_RANGE;
    if mode & unsupported != 0 {
        return EOPNOTSUPP;
    }
    if mode & !FALLOC_FL_KEEP_SIZE != 0 {
        return EOPNOTSUPP;
    }
    if len == 0 {
        return EINVAL;
    }

    let end = match offset.checked_add(len) {
        Some(v) => v,
        None => return 27u64.wrapping_neg(), // EFBIG
    };

    let t = match crate::tasks::current_thread() {
        Some(t) => t,
        None => return EBADF,
    };
    let vnode_arc = {
        let p = t.process.lock();
        p.fd_table.get(fd).cloned().flatten()
    };
    let Some(vnode_arc) = vnode_arc else {
        return EBADF;
    };
    let cur_size = vnode_arc.lock().vnode.size() as u64;
    if end > cur_size && (mode & FALLOC_FL_KEEP_SIZE) == 0 {
        // Extend the file to cover the requested range.
        match vnode_arc.lock().vnode.truncate(end as usize) {
            Ok(()) => 0,
            Err(e) => e,
        }
    } else {
        // KEEP_SIZE, or the range is already inside the file: for our
        // non-sparse filesystems there is nothing more to do.
        0
    }
}

pub(super) fn sys_truncate(path_ptr: u64, length: u64) -> u64 {
    let path = match read_user_cstring(path_ptr) {
        Some(p) => p,
        None => return EFAULT,
    };
    if let Some(vnode) = crate::fs::lookup(&path) {
        match vnode.truncate(length as usize) {
            Ok(_) => 0,
            Err(e) => e,
        }
    } else {
        ENOENT
    }
}

/// `sync()`: flush all dirty filesystem blocks and superblocks to disk.
pub(super) fn sys_sync() -> u64 {
    crate::fs::sync_all();
    0
}

/// `fsync(fd)` / `fdatasync(fd)`: validate the fd, then flush filesystems.
/// ponytail: no per-file dirty tracking — flush everything (correct, coarse).
pub(super) fn sys_fsync(fd: usize) -> u64 {
    let thread = match crate::tasks::current_thread() {
        Some(t) => t,
        None => return EBADF,
    };
    let p = thread.process.lock();
    match p.fd_table.get(fd).cloned().flatten() {
        Some(_) => {
            drop(p);
            crate::fs::sync_all();
            0
        }
        None => EBADF,
    }
}

pub(super) fn sys_memfd_create(name_ptr: u64, _flags: u32) -> u64 {
    let name = match read_user_cstring(name_ptr) {
        Some(s) => s,
        None => return EFAULT,
    };
    let thread = match crate::tasks::current_thread() {
        Some(t) => t,
        None => return ESRCH,
    };
    let node = crate::fs::tmpfs::TmpFsNode::new_file_node();
    let vnode = VNode::FsNode {
        node,
        cursor: 0,
    };
    let arc_vnode = Arc::new(SpinLock::new(crate::fs::vfs::OpenFile {
        vnode,
        flags: 0x2,
        path: alloc::format!("/memfd:{}", name),
    }));

    let mut p = thread.process.lock();
    for (i, slot) in p.fd_table.iter_mut().enumerate() {
        if slot.is_none() {
            *slot = Some(arc_vnode);
            crate::dprintln!("sys_memfd_create: '{}' -> fd={}", name, i);
            return i as u64;
        }
    }
    p.fd_table.push(Some(arc_vnode));
    let fd = (p.fd_table.len() - 1) as u64;
    crate::dprintln!("sys_memfd_create: '{}' -> fd={}", name, fd);
    fd
}

// ── mount(2) / umount2(2) ────────────────────────────────────────────────────

pub(super) fn sys_mount(source: u64, target: u64, fstype: u64, flags: u64, _data: u64) -> u64 {
    let src = if source == 0 {
        alloc::string::String::new()
    } else {
        match read_user_cstring(source) {
            Some(s) => s,
            None => return crate::arch::x86_64::syscalls::EFAULT,
        }
    };
    let tgt = match read_user_cstring(target) {
        Some(s) => s,
        None => return crate::arch::x86_64::syscalls::EFAULT,
    };
    let ft = match read_user_cstring(fstype) {
        Some(s) => s,
        None => return crate::arch::x86_64::syscalls::EFAULT,
    };

    // The mount point must exist.
    if crate::fs::lookup(&tgt).is_none() {
        return crate::arch::x86_64::syscalls::ENOENT;
    }

    // An ext4/ext2 source names a block device or a loopback file; every
    // other filesystem is mounted without one.
    let mount_source = if src.is_empty() {
        if ft == "ext4" || ft == "ext2" {
            crate::fs::mount::MountSource::Block
        } else {
            crate::fs::mount::MountSource::None
        }
    } else if ft == "ext4" || ft == "ext2" {
        match crate::fs::lookup(&src) {
            Some(crate::fs::vfs::VNode::FsNode { node, .. }) => {
                crate::fs::mount::MountSource::File(node)
            }
            _ => return crate::arch::x86_64::syscalls::ENOENT,
        }
    } else {
        crate::fs::mount::MountSource::None
    };

    let root = match crate::fs::mount::mount_fs(&ft, mount_source, flags) {
        Ok(r) => r,
        Err(e) => return e,
    };

    let fstype_static: &'static str = match ft.as_str() {
        "ext4" => "ext4",
        "ext2" => "ext2",
        "proc" => "proc",
        "sysfs" => "sysfs",
        "devtmpfs" => "devtmpfs",
        "tmpfs" => "tmpfs",
        "fuse" => "fuse",
        _ => "unknown",
    };

    let mnt = crate::fs::mount::Mount {
        mountpoint: tgt.clone(),
        root,
        fstype: fstype_static,
        source: if src.is_empty() { None } else { Some(src) },
        flags,
        open_count: core::sync::atomic::AtomicUsize::new(0),
    };

    match crate::fs::mount::mount_add(mnt) {
        Ok(()) => 0,
        Err(e) => e,
    }
}

pub(super) fn sys_umount2(target: u64, flags: u64) -> u64 {
    let tgt = match read_user_cstring(target) {
        Some(s) => s,
        None => return crate::arch::x86_64::syscalls::EFAULT,
    };
    match crate::fs::mount::mount_remove(&tgt, flags) {
        Ok(()) => 0,
        Err(e) => e,
    }
}
