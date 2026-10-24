use alloc::sync::Arc;
use alloc::vec::Vec;
use crate::arch::sync::spinlock::SpinLock;
use crate::arch::x86_64::syscalls::{validate_user_read_range, validate_user_write_range};
use crate::arch::x86_64::syscalls::{EAGAIN, EBADF, EFAULT, EINVAL, ESRCH};
use crate::fs::vfs::{OpenFile, VNode};
use crate::tasks::PendingBlock;
use crate::tasks::wake_pipe_waiters;
pub use crate::fs::vfs::EpollEvent;
use crate::fs::vfs::{EpollInstance, EventFdState, TimerFdState, InotifyState};

pub const EFD_SEMAPHORE: i32 = 1;
pub const EFD_CLOEXEC: i32 = 0x80000;
pub const EFD_NONBLOCK: i32 = 0x800;

pub const TFD_TIMER_ABSTIME: i32 = 1;
pub const TFD_CLOEXEC: i32 = 0x80000;
pub const TFD_NONBLOCK: i32 = 0x800;

pub const EPOLL_CTL_ADD: i32 = 1;
pub const EPOLL_CTL_DEL: i32 = 2;
pub const EPOLL_CTL_MOD: i32 = 3;

pub const EPOLLIN: u32 = 0x001;
pub const EPOLLOUT: u32 = 0x004;
pub const EPOLLERR: u32 = 0x008;
pub const EPOLLHUP: u32 = 0x010;
pub const EPOLLNVAL: u32 = 0x020;
pub const EPOLLET: u32 = 0x8000_0000;

#[derive(Debug, Clone, Copy, Default)]
pub struct Timespec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Itimerspec {
    pub it_interval: Timespec,
    pub it_value: Timespec,
}

fn current_pid_and_thread() -> Option<(u64, Arc<crate::tasks::Thread>)> {
    let t = crate::tasks::current_thread()?;
    let pid = t.process.lock().pid;
    Some((pid, Arc::new(t)))
}

pub fn on_close(_fd: usize) {}

// ---------------------------------------------------------------------------
// Inotify (proper implementation, not stubs)
// ---------------------------------------------------------------------------

pub fn sys_inotify_init1(flags: i32) -> u64 {
    let (pid, thread) = match current_pid_and_thread() {
        Some(t) => t,
        None => return ESRCH,
    };
    let cloexec = (flags & 0x80000) != 0;
    let nonblock = (flags & 0x800) != 0;

    let state = Arc::new(SpinLock::new(InotifyState {
        watches: alloc::collections::BTreeMap::new(),
        next_wd: 1,
        events: alloc::collections::VecDeque::new(),
        nonblock,
    }));

    let fd_flags = if cloexec { 0x80000u64 } else { 0u64 };
    match alloc_fd_for_vnode(VNode::Inotify(state), fd_flags) {
        Ok((_pid, fd)) => fd as u64,
        Err(e) => e,
    }
}

pub fn sys_inotify_add_watch(fd: i32, path_ptr: u64, mask: u32) -> u64 {
    if fd < 0 { return EBADF as u64; }
    let thread = match crate::tasks::current_thread() {
        Some(t) => t,
        None => return ESRCH as u64,
    };
    let p = thread.process.lock();
    let arc_vnode = match p.fd_table.get(fd as usize).and_then(|s| s.clone()) {
        Some(v) => v,
        None => {
            drop(p);
            return EBADF as u64;
        }
    };
    drop(p);

    let vnode = arc_vnode.lock().vnode.clone();
    match vnode {
        VNode::Inotify(state) => {
            let mut s = state.lock();
            let wd = s.next_wd;
            s.next_wd += 1;
            // Read the path from userspace
            let path = match crate::memory::space::read_user_cstring(path_ptr) {
                Some(p) => p,
                None => return EFAULT as u64,
            };
            s.watches.insert(wd, (path, mask));
            wd as u64
        }
        _ => EBADF as u64,
    }
}

pub fn sys_inotify_rm_watch(fd: i32, wd: i32) -> u64 {
    if fd < 0 { return EBADF as u64; }
    let thread = match crate::tasks::current_thread() {
        Some(t) => t,
        None => return ESRCH as u64,
    };
    let p = thread.process.lock();
    let arc_vnode = match p.fd_table.get(fd as usize).and_then(|s| s.clone()) {
        Some(v) => v,
        None => {
            drop(p);
            return EBADF as u64;
        }
    };
    drop(p);

    let vnode = arc_vnode.lock().vnode.clone();
    match vnode {
        VNode::Inotify(state) => {
            let mut s = state.lock();
            if s.watches.remove(&wd).is_some() {
                0
            } else {
                EINVAL as u64
            }
        }
        _ => EBADF as u64,
    }
}

pub fn alloc_fd_for_vnode(vnode: VNode, flags: u64) -> Result<(u64, usize), u64> {
    let (pid, thread) = current_pid_and_thread().ok_or(ESRCH)?;
    let mut p = thread.process.lock();
    let arc_vnode = Arc::new(SpinLock::new(OpenFile {
        vnode,
        flags,
        path: alloc::string::String::new(),
    }));

    for (i, slot) in p.fd_table.iter_mut().enumerate() {
        if slot.is_none() {
            *slot = Some(arc_vnode);
            return Ok((pid, i));
        }
    }
    p.fd_table.push(Some(arc_vnode));
    let fd = p.fd_table.len() - 1;
    Ok((pid, fd))
}

pub fn monotonic_now_ns() -> u64 {
    let ticks = crate::time::TICKS.load(core::sync::atomic::Ordering::Relaxed);
    crate::time::monotonic_nanos(ticks) as u64
}

fn get_epoll_instance(epfd: i32) -> Option<Arc<SpinLock<EpollInstance>>> {
    if epfd < 0 { return None; }
    let thread = crate::tasks::current_thread()?;
    let p = thread.process.lock();
    let vnode_arc = p.fd_table.get(epfd as usize).cloned().flatten()?;
    let open_file = vnode_arc.lock();
    match &open_file.vnode {
        VNode::Epoll(ep) => Some(ep.clone()),
        _ => None,
    }
}

fn get_timerfd_state(fd: i32) -> Option<Arc<SpinLock<TimerFdState>>> {
    if fd < 0 { return None; }
    let thread = crate::tasks::current_thread()?;
    let p = thread.process.lock();
    let vnode_arc = p.fd_table.get(fd as usize).cloned().flatten()?;
    let open_file = vnode_arc.try_lock()?;
    match &open_file.vnode {
        VNode::TimerFd(t) => Some(t.clone()),
        _ => None,
    }
}

fn get_eventfd_state(fd: i32) -> Option<Arc<SpinLock<EventFdState>>> {
    if fd < 0 { return None; }
    let thread = crate::tasks::current_thread()?;
    let p = thread.process.lock();
    let vnode_arc = p.fd_table.get(fd as usize).cloned().flatten()?;
    let open_file = vnode_arc.try_lock()?;
    match &open_file.vnode {
        VNode::EventFd(e) => Some(e.clone()),
        _ => None,
    }
}
/// Raise an eventfd owned by the current process from kernel context.
///
/// Equivalent to userspace `write(fd, &1u64, 8)`: bumps the counter so the
/// fd becomes readable and any epoll/timerfd waiter on it is woken. Used by
/// drivers that hand an eventfd to userspace as a completion signal (the
/// virtgpu out-fence) and then have to raise it themselves once the work is
/// actually done.
///
/// Returns false if `fd` is not an eventfd of the current process.
pub fn eventfd_signal_fd(fd: i32) -> bool {
    let Some(state) = get_eventfd_state(fd) else { return false };
    let mut st = state.lock();
    // A write of 1 to a non-semaphore eventfd adds to the counter; for a
    // semaphore eventfd it only wakes one waiter, so saturate at 1.
    if st.semaphore {
        st.counter = st.counter.max(1);
    } else {
        st.counter = st.counter.saturating_add(1);
    }
    // Wake waiters blocked on this eventfd (EPOLLIN) and epoll sets watching
    // it. Both are on PIPE_WAIT_QUEUE as far as blocking readers are concerned.
    drop(st);
    crate::tasks::wake_pipe_waiters();
    true
}

pub fn sys_eventfd(initval: u32, flags: i32) -> u64 {
    let state = Arc::new(SpinLock::new(EventFdState {
        counter: initval as u64,
        semaphore: (flags & EFD_SEMAPHORE) != 0,
        nonblock: (flags & EFD_NONBLOCK) != 0,
    }));
    let (_pid, fd) = match alloc_fd_for_vnode(VNode::EventFd(state), flags as u64) {
        Ok(res) => res,
        Err(e) => return e,
    };
    fd as u64
}

pub fn sys_timerfd_create(clockid: i32, flags: i32) -> u64 {
    if clockid != 0 && clockid != 1 {
        return EINVAL;
    }
    let state = Arc::new(SpinLock::new(TimerFdState {
        deadline_ns: 0,
        interval_ns: 0,
        expirations: 0,
        nonblock: (flags & TFD_NONBLOCK) != 0,
    }));
    let (_pid, fd) = match alloc_fd_for_vnode(VNode::TimerFd(state), flags as u64) {
        Ok(res) => res,
        Err(e) => return e,
    };
    fd as u64
}

pub fn sys_timerfd_settime(
    fd: i32,
    _flags: i32,
    new_value: *const Itimerspec,
    old_value: *mut Itimerspec,
) -> u64 {
    if fd < 0 {
        return EBADF;
    }
    if !validate_user_read_range(new_value as u64, core::mem::size_of::<Itimerspec>()) {
        return EFAULT;
    }
    if !old_value.is_null()
        && !validate_user_write_range(old_value as u64, core::mem::size_of::<Itimerspec>())
    {
        return EFAULT;
    }

    let new_spec = unsafe { core::ptr::read_unaligned(new_value) };
    if new_spec.it_value.tv_nsec < 0
        || new_spec.it_value.tv_nsec >= 1_000_000_000
        || new_spec.it_interval.tv_nsec < 0
        || new_spec.it_interval.tv_nsec >= 1_000_000_000
    {
        return EINVAL;
    }

    let timer_arc = match get_timerfd_state(fd) {
        Some(t) => t,
        None => return EBADF,
    };

    let now_ns = monotonic_now_ns();
    let mut state = timer_arc.lock();

    if !old_value.is_null() {
        let remaining_ns = if state.deadline_ns > now_ns {
            state.deadline_ns - now_ns
        } else {
            0
        };
        let old_spec = Itimerspec {
            it_interval: Timespec {
                tv_sec: (state.interval_ns / 1_000_000_000) as i64,
                tv_nsec: (state.interval_ns % 1_000_000_000) as i64,
            },
            it_value: Timespec {
                tv_sec: (remaining_ns / 1_000_000_000) as i64,
                tv_nsec: (remaining_ns % 1_000_000_000) as i64,
            },
        };
        unsafe {
            core::ptr::write_unaligned(old_value, old_spec);
        }
    }

    state.interval_ns =
        (new_spec.it_interval.tv_sec as u64) * 1_000_000_000 + (new_spec.it_interval.tv_nsec as u64);
    let value_ns =
        (new_spec.it_value.tv_sec as u64) * 1_000_000_000 + (new_spec.it_value.tv_nsec as u64);

    if value_ns == 0 {
        state.deadline_ns = 0;
    } else {
        state.deadline_ns = now_ns + value_ns;
    }
    state.expirations = 0;

    0
}

pub fn sys_timerfd_gettime(fd: i32, curr_value: *mut Itimerspec) -> u64 {
    if fd < 0 {
        return EBADF;
    }
    if !validate_user_write_range(curr_value as u64, core::mem::size_of::<Itimerspec>()) {
        return EFAULT;
    }

    let timer_arc = match get_timerfd_state(fd) {
        Some(t) => t,
        None => return EBADF,
    };

    let now_ns = monotonic_now_ns();
    let state = timer_arc.lock();

    let remaining_ns = if state.deadline_ns > now_ns {
        state.deadline_ns - now_ns
    } else {
        0
    };

    let spec = Itimerspec {
        it_interval: Timespec {
            tv_sec: (state.interval_ns / 1_000_000_000) as i64,
            tv_nsec: (state.interval_ns % 1_000_000_000) as i64,
        },
        it_value: Timespec {
            tv_sec: (remaining_ns / 1_000_000_000) as i64,
            tv_nsec: (remaining_ns % 1_000_000_000) as i64,
        },
    };

    unsafe {
        core::ptr::write_unaligned(curr_value, spec);
    }

    0
}
pub fn sys_epoll_create1(_flags: i32) -> u64 {
    let instance = Arc::new(SpinLock::new(EpollInstance { interest: alloc::collections::BTreeMap::new(), last_reported_events: alloc::collections::BTreeMap::new() }));
    let (_pid, fd) = match alloc_fd_for_vnode(VNode::Epoll(instance), 0) {
        Ok(res) => res,
        Err(e) => return e,
    };
    fd as u64
}

pub fn sys_epoll_ctl(epfd: i32, op: i32, fd: i32, event: *mut EpollEvent) -> u64 {
    if epfd < 0 || fd < 0 {
        crate::dprintln!("sys_epoll_ctl: epfd={} op={} fd={} -> EBADF", epfd, op, fd);
        return EBADF;
    }

    let epoll_arc = match get_epoll_instance(epfd) {
        Some(ep) => ep,
        None => {
            crate::dprintln!("sys_epoll_ctl: get_epoll_instance failed for epfd={}", epfd);
            return EBADF;
        }
    };

    let mut ep = epoll_arc.lock();

    let res = match op {
        EPOLL_CTL_ADD => {
            if ep.interest.contains_key(&fd) {
                crate::arch::x86_64::syscalls::EEXIST
            } else if !validate_user_read_range(event as u64, core::mem::size_of::<EpollEvent>()) {
                crate::dprintln!("sys_epoll_ctl: validate_user_read_range failed for event {:p}", event);
                EFAULT
            } else {
                let ev = unsafe { core::ptr::read_unaligned(event) };
                ep.interest.insert(fd, ev);
                0
            }
        }
        EPOLL_CTL_MOD => {
            if !ep.interest.contains_key(&fd) {
                crate::arch::x86_64::syscalls::ENOENT
            } else if !validate_user_read_range(event as u64, core::mem::size_of::<EpollEvent>()) {
                EFAULT
            } else {
                let ev = unsafe { core::ptr::read_unaligned(event) };
                ep.interest.insert(fd, ev);
                0
            }
        }
        EPOLL_CTL_DEL => {
            if ep.interest.remove(&fd).is_none() {
                crate::arch::x86_64::syscalls::ENOENT
            } else {
                0
            }
        }
        _ => EINVAL,
    };
    res
}

fn check_fd_readiness(_pid: u64, fd: i32) -> u32 {
    check_fd_readiness_depth(_pid, fd, 0)
}

/// Readiness probe with nesting depth for epoll-in-epoll. Nested instance
/// locks use try_lock (never block): contention just reports not-ready and
/// level-triggered polling retries. Depth cap breaks A-watches-B-watches-A
/// cycles (Linux rejects those with ELOOP at ADD time; we bound instead).
fn check_fd_readiness_depth(_pid: u64, fd: i32, depth: u32) -> u32 {
    let ufd = fd as usize;
    if ufd == 0 {
        // Per-VT queues now, not the old global STDIN_BUFFER. Probe every VT:
        // epoll has no idea which one this descriptor names, and reporting
        // ready when any VT has input is the conservative answer for a
        // level-triggered watcher.
        return if (1..=crate::graphics::vt::MAX_VTS as u16)
            .any(|n| crate::tty::has_input(n))
        {
            EPOLLIN
        } else {
            0
        };
    } else if ufd == 1 || ufd == 2 {
        return EPOLLOUT;
    }

    let thread = match crate::tasks::current_thread() {
        Some(t) => t,
        None => return EPOLLERR,
    };
    let vnode_opt = {
        let p = thread.process.lock();
        p.fd_table.get(ufd).cloned().flatten()
    };

    match vnode_opt {
        None => EPOLLERR | EPOLLNVAL,
        Some(v) => {
            let vn = v.lock();
            match &vn.vnode {
                VNode::Pipe { shared, end, .. } => {
                    let pb = shared.lock();
                    let mut rev = 0;
                    if *end == crate::fs::vfs::PipeEnd::Read {
                        if !pb.buffer.is_empty() {
                            rev |= EPOLLIN;
                        }
                        if Arc::strong_count(&pb.write_token) == 1 {
                            rev |= EPOLLHUP | EPOLLIN;
                        }
                    } else {
                        if Arc::strong_count(&pb.read_token) == 1 {
                            rev |= EPOLLERR | EPOLLHUP;
                        }
                        if pb.buffer.len() < pb.capacity {
                            rev |= EPOLLOUT;
                        }
                    }
                    rev
                }
                VNode::EventFd(e) => {
                    let mut rev = 0;
                    if e.lock().counter > 0 { rev |= EPOLLIN; }
                    rev |= EPOLLOUT;
                    rev
                }
                VNode::TimerFd(t) => {
                    let mut st = t.lock();
                    st.update_expirations(monotonic_now_ns());
                    if st.expirations > 0 { EPOLLIN } else { 0 }
                }
                VNode::UnixSocket(sock) => {
                    let (r, w) = crate::arch::x86_64::syscalls::net::sock_readiness(sock);
                    let mut rev = 0;
                    if r { rev |= EPOLLIN; }
                    if w { rev |= EPOLLOUT; }
                    rev
                }
                VNode::InetSocket(inet) => {
                    let (r, w) = crate::arch::x86_64::syscalls::net::inet_readiness(inet);
                    let mut rev = 0;
                    if r { rev |= EPOLLIN; }
                    if w { rev |= EPOLLOUT; }
                    rev
                }
                VNode::UdpSocket(udp) => {
                    let (r, w) = crate::arch::x86_64::syscalls::net::udp_readiness(udp);
                    let mut rev = 0;
                    if r { rev |= EPOLLIN; }
                    if w { rev |= EPOLLOUT; }
                    rev
                }
                VNode::DrmDevice { .. } => {
                    let mut rev = EPOLLOUT;
                    if crate::drivers::drm::events_pending() {
                        rev |= EPOLLIN;
                    }
                    rev
                }
                VNode::InputEventDevice { minor } | VNode::InputMiceDevice { minor } => {
                    if crate::drivers::keyboard::input_pending(Arc::as_ptr(&v) as usize, *minor) {
                        EPOLLIN
                    } else {
                        0
                    }
                }
                VNode::PtyMaster(pty) => {
                    let (r, w) = crate::fs::pty::pty_master_readiness(pty);
                    let mut rev = 0;
                    if r { rev |= EPOLLIN; }
                    if w { rev |= EPOLLOUT; }
                    rev
                }
                VNode::PtySlave(pty) => {
                    let (r, w) = crate::fs::pty::pty_slave_readiness(pty);
                    let mut rev = 0;
                    if r { rev |= EPOLLIN; }
                    if w { rev |= EPOLLOUT; }
                    rev
                }
                VNode::Epoll(nested) => {
                    // ponytail: nested epoll (fd 12 in mywima's set) fell
                    // into `_` below and reported always-ready, spinning the
                    // compositor's dispatch loop at 17K iters/sec. Propagate
                    // member readiness like Linux (level-triggered).
                    if depth >= 4 {
                        return 0;
                    }
                    match nested.try_lock() {
                        Some(nst) => {
                            let mut rev = 0;
                            for (&mfd, &interest) in nst.interest.iter() {
                                let r = check_fd_readiness_depth(_pid, mfd, depth + 1);
                                rev |= r & (interest.events | EPOLLERR | EPOLLHUP);
                                if rev != 0 {
                                    break;
                                }
                            }
                            rev
                        }
                        None => 0,
                    }
                }
                _ => EPOLLIN | EPOLLOUT,
            }
        }
    }
}

/// Earliest tick deadline among armed TimerFds in an epoll set, for
/// timeout<0 waits that rely on timerfd expiry (calloop/polling pattern:
/// one timerfd armed per wait + epoll_wait(-1)). None when no timerfd is
/// armed — the park keeps deadline MAX.
/// Lock-safe: copies (fd, events) under the instance lock, then resolves
/// each fd with non-blocking try_lock (skips on contention; the re-scan
/// after any wake re-evaluates anyway). Same order as the readiness scan
/// (instance -> process -> vnode, never nested).
fn earliest_timerfd_deadline(epoll_arc: &Arc<SpinLock<EpollInstance>>) -> Option<u64> {
    let targets: alloc::vec::Vec<(i32, u32)> = {
        let ep = epoll_arc.lock();
        ep.interest.iter().map(|(&fd, ev)| (fd, ev.events)).collect()
    };
    let now_ns = monotonic_now_ns();
    let now_tick = crate::time::TICKS.load(core::sync::atomic::Ordering::Relaxed) as u64;
    let mut best: Option<u64> = None;
    for (fd, events) in targets {
        if events & EPOLLIN == 0 {
            continue;
        }
        let t = match get_timerfd_state(fd) {
            Some(t) => t,
            None => continue,
        };
        let mut st = match t.try_lock() {
            Some(g) => g,
            None => continue,
        };
        st.update_expirations(now_ns);
        if st.expirations > 0 {
            return Some(now_tick); // already readable: wake on next tick
        }
        if st.deadline_ns > now_ns {
            let ticks = (st.deadline_ns - now_ns + 999_999) / 1_000_000;
            let d = now_tick.saturating_add(ticks).max(now_tick + 1);
            best = Some(best.map_or(d, |b: u64| b.min(d)));
        }
    }
    best
}

pub fn sys_epoll_wait(epfd: i32, events: *mut EpollEvent, maxevents: i32, timeout: i32) -> u64 {
    if maxevents <= 0 {
        return EINVAL;
    }
    let total_bytes = match (maxevents as usize).checked_mul(core::mem::size_of::<EpollEvent>()) {
        Some(b) => b,
        None => return EINVAL,
    };
    if !validate_user_write_range(events as u64, total_bytes) {
        return EFAULT;
    }

    let (pid, _) = match current_pid_and_thread() {
        Some(pt) => pt,
        None => return ESRCH,
    };

    let epoll_arc = match get_epoll_instance(epfd) {
        Some(ep) => ep,
        None => return EBADF,
    };

    // Deadline computed ONCE (tick units, ~1ms): the resume loop below must
    // not recompute it, or expiry would never arrive. timeout<0 starts at
    // deadline MAX, narrowed per-park from armed timerfds (see park site).
    let deadline = if timeout > 0 {
        let now = crate::time::TICKS.load(core::sync::atomic::Ordering::Relaxed) as u64;
        now.saturating_add((timeout as u64).saturating_add(999) / 1000)
            .max(now + 1)
    } else {
        u64::MAX
    };

    loop {
        let mut count = 0;
        let ep_guard = epoll_arc.lock();
        // Preload the last_reported map (clone to avoid holding lock too long)
        let mut last_reported = ep_guard.last_reported_events.clone();
        drop(ep_guard); // drop lock before calling check_fd_readiness to avoid inversion

        // Take a snapshot of the interest map and DROP the lock before the loop.
        //
        // The obvious form — `for (&fd, &i) in epoll_arc.lock().interest.iter()`
        // — keeps the guard alive for the whole loop body, because a temporary
        // in a `for` iterator expression lives until the end of the `for`. So
        // `check_fd_readiness` ran with the epoll lock held, and any fd whose
        // readiness probe re-enters epoll locked it a second time on the same
        // CPU: `SAME-CPU REENTRY ... epoll.rs:614`, a hard deadlock. It is
        // reproducible whenever a process epolls something whose readiness
        // check touches its own epoll set — zellij's client does.
        let interest_snapshot: Vec<(i32, EpollEvent)> = {
            let g = epoll_arc.lock();
            g.interest.iter().map(|(&fd, &i)| (fd, i)).collect()
        };

        for (target_fd, interest) in interest_snapshot {
            if count >= maxevents as usize {
                break;
            }
            let interest_events = interest.events;
            let interest_data = interest.data;
            let ready = check_fd_readiness(pid, target_fd);
            // Mask readiness with interest BEFORE any reporting decision.
            // This prevents spurious wakeups where a fd is ready for an event
            // the caller didn't register interest in (e.g. EPOLLOUT when only
            // EPOLLIN was requested), which would deliver events==0 to userspace.
            let effective_ready = ready & (interest_events | EPOLLERR | EPOLLHUP);
            let mut report = false;

            if interest_events & EPOLLET != 0 {
                // Edge-triggered: report only if effective readiness has changed since last report
                let mut updated = false;
                {
                    if let Some(last) = last_reported.get_mut(&target_fd) {
                        if effective_ready != 0 && last.events == 0 {
                            // transition from not-ready to ready
                            report = true;
                            updated = true;
                            last.events = effective_ready;
                        } else if effective_ready != 0 && last.events != effective_ready {
                            // readiness changed while still ready
                            report = true;
                            updated = true;
                            last.events = effective_ready;
                        } else if effective_ready == 0 && last.events != 0 {
                            // transition to not-ready: clear last reported
                            updated = true;
                            last.events = 0;
                        }
                    } else {
                        // first time seeing this fd with EPOLLET
                        if effective_ready != 0 {
                            report = true;
                            updated = true;
                        }
                        last_reported.insert(target_fd, EpollEvent { events: effective_ready, data: interest_data });
                    }
                }
                // update the shared map if we changed anything
                if updated {
                    let mut ep = epoll_arc.lock();
                    if let Some(existing) = ep.last_reported_events.get_mut(&target_fd) {
                        existing.events = last_reported[&target_fd].events;
                    } else {
                        ep.last_reported_events.insert(target_fd, last_reported[&target_fd].clone());
                    }
                }
            } else {
                // Level-triggered: report only if effective readiness is non-zero
                if effective_ready != 0 {
                    report = true;
                }
            }

            if report {
                // effective_ready is guaranteed non-zero here (both branches
                // check it before setting report=true), so out_ev.events != 0.
                let out_ev = EpollEvent {
                    events: effective_ready,
                    data: interest_data,
                };
                unsafe {
                    core::ptr::write_unaligned(events.add(count), out_ev);
                }
                count += 1;
            }
        }

        if count > 0 {
            // ponytail: TEMP-DIAG instant-ready hunter (video-CPU case):
            // sample successes 200000..200030 — past boot bursts, in
            // steady-state video playback. Names the always-ready fd.
            static HUNT_N: core::sync::atomic::AtomicU64 =
                core::sync::atomic::AtomicU64::new(0);
            let n = HUNT_N.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            if n >= 200_000 && n < 200_030 {
                // Collect first (epoll guard dropped before fd-table locks:
                // instance -> process would invert lock order).
                let mut hits = [(0i32, 0u16, 0u16); 8];
                let mut nhit = 0usize;
                {
                    let ep = epoll_arc.lock();
                    for (&target_fd, &interest) in ep.interest.iter() {
                        let ready = check_fd_readiness(pid, target_fd);
                        let ev = interest.events;
                        if ready & (ev | 0x018) != 0 && nhit < hits.len() {
                            hits[nhit] = (target_fd, ev as u16, ready as u16);
                            nhit += 1;
                        }
                    }
                }
                for &(target_fd, ev, ready) in hits.iter().take(nhit) {
                    let (vnode_opt, path) = match crate::tasks::current_thread() {
                        Some(t) => {
                            let p = t.process.lock();
                            match p.fd_table.get(target_fd as usize).cloned().flatten() {
                                Some(v) => {
                                    let path = v.lock().path.clone();
                                    (Some(v), path)
                                }
                                None => (None, alloc::string::String::from("?")),
                            }
                        }
                        None => (None, alloc::string::String::from("?")),
                    };
                    let tag = match vnode_opt {
                        None => {
                            crate::dprintln!(
                                "[epoll-hunt] pid={} epfd={} fd={} ev={:#x} rdy={:#x} path={} gone",
                                pid, epfd, target_fd, ev, ready, path
                            );
                            "gone"
                        }
                        Some(v) => match &v.lock().vnode {
                            VNode::Pipe { .. } => "pipe",
                            VNode::EventFd(e) => {
                                let c = e.lock().counter;
                                crate::dprintln!(
                                    "[epoll-hunt] pid={} epfd={} fd={} ev={:#x} rdy={:#x} path={} eventfd counter={}",
                                    pid, epfd, target_fd, ev, ready, path, c
                                );
                                "eventfd"
                            }
                            VNode::TimerFd(t) => {
                                let x = t.lock().expirations;
                                crate::dprintln!(
                                    "[epoll-hunt] pid={} epfd={} fd={} ev={:#x} rdy={:#x} path={} timerfd exp={}",
                                    pid, epfd, target_fd, ev, ready, path, x
                                );
                                "timerfd"
                            }
                            VNode::UnixSocket(s) => {
                                let (r, w) = crate::arch::x86_64::syscalls::net::sock_readiness(s);
                                let (n0, b0, n1, b1) =
                                    crate::arch::x86_64::syscalls::net::sock_queue_depths(s);
                                crate::dprintln!(
                                    "[epoll-hunt] pid={} epfd={} fd={} ev={:#x} rdy={:#x} path={} unix r={} w={} msgs={}/{} bytes={}/{}",
                                    pid, epfd, target_fd, ev, ready, path, r as u8, w as u8, n0, n1, b0, b1
                                );
                                "unix"
                            }
                            VNode::PtyMaster(p) => {
                                let q = p.lock();
                                crate::dprintln!(
                                    "[epoll-hunt] pid={} epfd={} fd={} ev={:#x} rdy={:#x} path={} ptymaster id={} s2m={} m2s={}",
                                    pid, epfd, target_fd, ev, ready, path, q.id, q.s2m.len(), q.m2s.len()
                                );
                                "ptym"
                            }
                            VNode::PtySlave(p) => {
                                let q = p.lock();
                                crate::dprintln!(
                                    "[epoll-hunt] pid={} epfd={} fd={} ev={:#x} rdy={:#x} path={} ptyslave id={} s2m={} m2s={}",
                                    pid, epfd, target_fd, ev, ready, path, q.id, q.s2m.len(), q.m2s.len()
                                );
                                "ptys"
                            }
                            VNode::DrmDevice { .. } => "drm",
                            VNode::TtyDevice { .. } => "tty",
                            VNode::FbDevice { .. } => "fb",
                            VNode::NullDevice => "null",
                            VNode::InputEventDevice { .. } | VNode::InputMiceDevice { .. } => "input",
                            VNode::Epoll(..) => "epoll",
                            VNode::DspAudioDevice | VNode::SndControlDevice | VNode::SndPlaybackDevice | VNode::SndCaptureDevice => "audio",
                            _ => "other",
                        },
                    };
                        if !matches!(tag, "eventfd" | "timerfd" | "unix" | "ptym" | "ptys") {
                            crate::dprintln!(
                                "[epoll-hunt] pid={} epfd={} fd={} ev={:#x} rdy={:#x} path={} {}",
                                pid, epfd, target_fd, ev, ready, path, tag
                            );
                        }
                    }
                }
            return count as u64;
        }
        if timeout == 0 {
            return 0;
        }
        // Park instead of spinning: fd activity wakes early via the broad
        // pipe wakeups, the per-tick sweep wakes deadline expiry.
        // Spinning here on sched_yield burned both cores under mywima's
        // 16ms dispatch loop. Resume always re-scans, so no wakeup-cause
        // bookkeeping is needed.
        // ponytail: timeout<0 parks below derive the deadline from armed
        // timerfds (calloop/polling arms one per wait) — without it the
        // sweep never wakes them and the compositor heartbeat freezes.
        let mut park_deadline = deadline;
        if park_deadline == u64::MAX {
            if let Some(d) = earliest_timerfd_deadline(&epoll_arc) {
                park_deadline = d;
            }
        }
        if timeout > 0 {
            let now = crate::time::TICKS.load(core::sync::atomic::Ordering::Relaxed) as u64;
            if now >= deadline {
                return 0;
            }
        }
        crate::tasks::block_current_thread(crate::tasks::PendingBlock::EpollWait { deadline: park_deadline });
    }
}
pub fn try_read(fd: usize, buf_ptr: *mut u8, count: usize) -> Option<u64> {
    if let Some(efd) = get_eventfd_state(fd as i32) {
        if count < 8 { return Some(EINVAL); }
        if !validate_user_write_range(buf_ptr as u64, 8) { return Some(EFAULT); }
        loop {
            let mut st = efd.lock();
            if st.counter == 0 {
                if st.nonblock { return Some(EAGAIN); }
                drop(st);
                crate::tasks::block_current_thread(PendingBlock::PipeWait);
                continue;
            }
            let val = if st.semaphore {
                st.counter -= 1;
                1u64
            } else {
                let v = st.counter;
                st.counter = 0;
                v
            };
            unsafe { core::ptr::write_unaligned(buf_ptr as *mut u64, val); }
            return Some(8);
        }
    }
    
    if let Some(tfd) = get_timerfd_state(fd as i32) {
        if count < 8 { return Some(EINVAL); }
        if !validate_user_write_range(buf_ptr as u64, 8) { return Some(EFAULT); }
        loop {
            let mut st = tfd.lock();
            let now = monotonic_now_ns();
            st.update_expirations(now);
            if st.expirations == 0 {
                if st.nonblock { return Some(EAGAIN); }
                let deadline = if st.deadline_ns != 0 && st.deadline_ns > now {
                    let diff_ns = st.deadline_ns - now;
                    let ticks = crate::time::TICKS.load(core::sync::atomic::Ordering::Relaxed) as u64;
                    ticks.saturating_add(diff_ns.saturating_add(999_999) / 1_000_000)
                } else {
                    u64::MAX
                };
                drop(st);
                crate::tasks::block_current_thread(PendingBlock::EpollWait { deadline });
                continue;
            }
            let exp = st.expirations;
            st.expirations = 0;
            unsafe { core::ptr::write_unaligned(buf_ptr as *mut u64, exp); }
            return Some(8);
        }
    }
    None
}

pub fn try_write(fd: usize, buf_ptr: *const u8, count: usize) -> Option<u64> {
    if let Some(efd) = get_eventfd_state(fd as i32) {
        if count < 8 { return Some(EINVAL); }
        if !validate_user_read_range(buf_ptr as u64, 8) { return Some(EFAULT); }
        let val = unsafe { core::ptr::read_unaligned(buf_ptr as *const u64) };
        if val == 0xFFFFFFFFFFFFFFFF { return Some(EINVAL); }
        let mut st = efd.lock();
        if st.counter.checked_add(val).is_none() || st.counter + val == 0xFFFFFFFFFFFFFFFF {
            if st.nonblock { return Some(EAGAIN); }
            return Some(EINVAL);
        }
        st.counter += val;
        drop(st);
        wake_pipe_waiters();
        return Some(8);
    }
    None
}

pub fn check_poll_fd(_pid: u64, fd: i32, events: i16) -> Option<i16> {
    if let Some(efd) = get_eventfd_state(fd) {
        let st = efd.lock();
        let mut rev = 0i16;
        if (events & 0x001) != 0 && st.counter > 0 { rev |= 0x001; }
        if (events & 0x004) != 0 { rev |= 0x004; }
        return Some(rev);
    }
    if let Some(tfd) = get_timerfd_state(fd) {
        let mut st = tfd.lock();
        let now = monotonic_now_ns();
        st.update_expirations(now);
        let mut rev = 0i16;
        if (events & 0x001) != 0 && st.expirations > 0 { rev |= 0x001; }
        return Some(rev);
    }
    None
}

pub fn is_timer_fd(fd: i32) -> bool {
    get_timerfd_state(fd).is_some()
}

/// Signalfd — creates a file descriptor for reading pending signals.
/// Returns a valid fd; signal delivery is no-op in DragOS (signals are
/// still delivered via sigreturn path, signalfd only acknowledges fd exists).
pub fn sys_signalfd(fd: i32, mask_ptr: u64, flags: i32) -> u64 {
    if mask_ptr == 0 {
        return super::EFAULT;
    }
    if !validate_user_read_range(mask_ptr, 8) {
        return super::EFAULT;
    }
    if fd == -1 {
        let state = Arc::new(SpinLock::new(EventFdState {
            counter: 0,
            semaphore: (flags & 0x1) != 0,
            nonblock: (flags & 0x800) != 0,
        }));
        let (_pid, new_fd) = match alloc_fd_for_vnode(VNode::EventFd(state), flags as u64) {
            Ok(res) => res,
            Err(e) => return e,
        };
        new_fd as u64
    } else {
        if fd < 0 {
            return super::EBADF;
        }
        fd as u64
    }
}
