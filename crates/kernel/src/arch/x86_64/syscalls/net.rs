//! Minimal AF_UNIX SOCK_STREAM + AF_INET via smoltcp.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::arch::sync::spinlock::SpinLock;
use crate::fs::vfs::{OpenFile, VNode};
use crate::tasks::{block_current_thread, PendingBlock};

use super::{
    EAFNOSUPPORT, EAGAIN, EALREADY, EBADF, ECONNREFUSED, EDESTADDRREQ, EINPROGRESS, EINVAL,
    ENOENT, ENOTSOCK, EOPNOTSUPP, EPROTONOSUPPORT,
};

const AF_UNIX: u64 = 1;
const AF_INET: u64 = 2;
const AF_INET6: u64 = 10;
const SOCK_STREAM: u64 = 1;
const SOCK_DGRAM: u64 = 2;
const SOCK_NONBLOCK: u64 = 0o4000;
const SOCK_CLOEXEC: u64 = 0o2000000;

pub const SOL_SOCKET: u32 = 1;
pub const SCM_RIGHTS: u32 = 1;
const MSG_DONTWAIT: i32 = 0x40;
const MSG_PEEK: i32 = 0x2;

/// Byte cap per direction; wayland messages are small.
const SOCK_BUF_LIMIT: usize = 512 * 1024;

pub struct UnixMsg {
    pub bytes: Vec<u8>,
    pub fds: Vec<Arc<SpinLock<OpenFile>>>,
}

pub struct UnixStream {
    /// bufs[0]: written by end 0, read by end 1. bufs[1]: reverse.
    pub bufs: [SpinLock<VecDeque<UnixMsg>>; 2],
    pub buffered: [core::sync::atomic::AtomicUsize; 2],
    /// 1 while the owning end lives, 0 once its last SockInner dropped.
    pub opened: [core::sync::atomic::AtomicUsize; 2],
}

impl UnixStream {
    fn new() -> Self {
        use core::sync::atomic::AtomicUsize;
        Self {
            bufs: [SpinLock::new(VecDeque::new()), SpinLock::new(VecDeque::new())],
            buffered: [AtomicUsize::new(0), AtomicUsize::new(0)],
            opened: [AtomicUsize::new(1), AtomicUsize::new(1)],
        }
    }

    fn peer_open(&self, my_end: usize) -> bool {
        self.opened[my_end ^ 1]
            .load(core::sync::atomic::Ordering::Acquire)
            > 0
    }
}

pub enum SockKind {
    Unbound,
    Listening {
        path: String,
        backlog: usize,
        pending: VecDeque<(Arc<UnixStream>, usize)>,
    },
    Connected(Arc<UnixStream>, usize),
}

pub struct SockInner {
    pub kind: SockKind,
    pub nonblock: bool,
    pub is_netlink: bool,
}

impl Drop for SockInner {
    fn drop(&mut self) {
        match &mut self.kind {
            SockKind::Connected(stream, my_end) => {
                stream.opened[*my_end]
                    .fetch_sub(1, core::sync::atomic::Ordering::Release);
                // Drain our queue so buffered counts stay honest.
                let mut q = stream.bufs[*my_end].lock();
                while q.pop_front().is_some() {}
            }
            SockKind::Listening { path, .. } => {
                if let Some(mut l) = LISTENERS.try_lock() {
                    l.remove(path);
                }
            }
            _ => {}
        }
        crate::tasks::wake_pipe_waiters();
    }
}

static LISTENERS: SpinLock<BTreeMap<String, Arc<SpinLock<SockInner>>>> =
    SpinLock::new(BTreeMap::new());

pub fn remove_listener(path: &str) {
    let mut l = LISTENERS.lock();
    l.remove(path);
    let trimmed = path.trim_start_matches('/');
    l.remove(trimmed);
}

fn get_sock(fd: i32) -> Option<Arc<SpinLock<SockInner>>> {
    let thread = crate::tasks::current_thread()?;
    let p = thread.process.lock();
    let open_file = (*p.fd_table.get(fd.max(0) as usize)?).clone()?;
    match &open_file.lock().vnode {
        VNode::UnixSocket(s) => Some(s.clone()),
        _ => None,
    }
}

fn get_sock_and_file(fd: i32) -> Option<(Arc<SpinLock<SockInner>>, Arc<SpinLock<OpenFile>>)> {
    let thread = crate::tasks::current_thread()?;
    let p = thread.process.lock();
    let open_file = (*p.fd_table.get(fd.max(0) as usize)?).clone()?;
    let sock = match &open_file.lock().vnode {
        VNode::UnixSocket(s) => s.clone(),
        _ => return None,
    };
    Some((sock, open_file))
}

// ----- Inet -----

use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp::{Socket as TcpSocket, SocketBuffer};
use smoltcp::wire::{IpAddress, IpEndpoint, Ipv4Address};

pub struct InetInner {
    pub handle: SocketHandle,
    pub nonblock: bool,
    pub bound_port: Option<u16>,
    pub is_listener: bool,
    pub backlog: usize,
}

impl Drop for InetInner {
    fn drop(&mut self) {
        // Remove socket from set; ignore if already removed or not init.
        if let Some(mut guard) = crate::net::SOCKETS.try_lock() {
            if let Some(set) = guard.as_mut() {
                // smoltcp SocketSet::remove is fallible but we ignore errors.
                let _ = set.remove(self.handle);
            }
        }
        crate::tasks::wake_pipe_waiters();
    }
}

pub struct UdpInner {
    pub handle: SocketHandle,
    pub nonblock: bool,
    pub peer: SpinLock<Option<(Ipv4Address, u16)>>,
}

impl Drop for UdpInner {
    fn drop(&mut self) {
        if let Some(mut guard) = crate::net::SOCKETS.try_lock() {
            if let Some(set) = guard.as_mut() {
                let _ = set.remove(self.handle);
            }
        }
        crate::tasks::wake_pipe_waiters();
    }
}

fn get_udp(fd: i32) -> Option<Arc<SpinLock<UdpInner>>> {
    let thread = crate::tasks::current_thread()?;
    let p = thread.process.lock();
    let open_file = (*p.fd_table.get(fd.max(0) as usize)?).clone()?;
    match &open_file.lock().vnode {
        VNode::UdpSocket(s) => Some(s.clone()),
        _ => None,
    }
}

pub fn udp_readiness(udp: &Arc<SpinLock<UdpInner>>) -> (bool, bool) {
    let handle = udp.lock().handle;
    let readable = crate::net::SOCKETS
        .try_lock()
        .and_then(|mut guard| {
            guard.as_mut().map(|set| {
                set.get::<smoltcp::socket::udp::Socket>(handle)
                    .can_recv()
            })
        })
        .unwrap_or(false);
    (readable, true)
}

static NEXT_EPHEMERAL: core::sync::atomic::AtomicU16 = core::sync::atomic::AtomicU16::new(49152);

fn get_inet(fd: i32) -> Option<Arc<SpinLock<InetInner>>> {
    let thread = crate::tasks::current_thread()?;
    let p = thread.process.lock();
    let open_file = (*p.fd_table.get(fd.max(0) as usize)?).clone()?;
    match &open_file.lock().vnode {
        VNode::InetSocket(s) => Some(s.clone()),
        _ => None,
    }
}

fn parse_sockaddr_in(addr: u64, len: u64) -> Option<(Ipv4Address, u16)> {
    if addr == 0 || len < 8 {
        return None;
    }
    if !crate::memory::space::validate_user_read_range(addr, 8) {
        return None;
    }
    unsafe {
        let family = *(addr as *const u16);
        if family != AF_INET as u16 {
            return None;
        }
        let port_be = *(addr.wrapping_add(2) as *const u16);
        let port = u16::from_be(port_be);
        let ip_raw = *(addr.wrapping_add(4) as *const u32);
        let ip_host = u32::from_be(ip_raw);
        let ipv4 = Ipv4Address::from_bytes(&ip_host.to_be_bytes());
        Some((ipv4, port))
    }
}

pub fn inet_read(inet: &Arc<SpinLock<InetInner>>, buf: &mut [u8], nonblock_flag: bool) -> Result<usize, u64> {
    let handle = inet.lock().handle;
    let nonblock = inet.lock().nonblock || nonblock_flag;
    loop {
        crate::net::poll();
        // Hold SOCKETS lock only briefly per iteration.
        let result = {
            let mut guard = crate::net::SOCKETS.lock();
            let set = match guard.as_mut() {
                Some(s) => s,
                None => return Err(EINVAL),
            };
            let sock = set.get_mut::<TcpSocket>(handle);
            if sock.can_recv() {
                match sock.recv_slice(buf) {
                    Ok(n) => Some(Ok(n)),
                    Err(smoltcp::socket::tcp::RecvError::Finished) => Some(Ok(0)),
                    Err(e) => {
                        crate::dprintln!("[inet-read] RECV_ERR state={:?}", sock.state());
                        let _ = e;
                        Some(Err(EINVAL))
                    }
                }
            } else if !sock.is_open()
                || matches!(
                    sock.state(),
                    smoltcp::socket::tcp::State::CloseWait
                        | smoltcp::socket::tcp::State::LastAck
                        | smoltcp::socket::tcp::State::Closing
                )
            {
                // Closed and no data -> EOF. CloseWait/LastAck/Closing with an
                // empty buffer means the remote FIN arrived and all data was
                // consumed (is_open stays true there, so match states too).
                Some(Ok(0))
            } else {
                None
            }
        };
        if let Some(r) = result {
            return r;
        }
        if nonblock {
            return Err(EAGAIN);
        }
        block_current_thread(PendingBlock::PipeWait);
    }
}

pub fn inet_write(inet: &Arc<SpinLock<InetInner>>, buf: &[u8], nonblock_flag: bool) -> Result<usize, u64> {
    if buf.is_empty() {
        return Ok(0);
    }
    let handle = inet.lock().handle;
    let nonblock = inet.lock().nonblock || nonblock_flag;
    loop {
        crate::net::poll();
        let result = {
            let mut guard = crate::net::SOCKETS.lock();
            let set = match guard.as_mut() {
                Some(s) => s,
                None => return Err(EINVAL),
            };
            let sock = set.get_mut::<TcpSocket>(handle);
            if sock.can_send() {
                match sock.send_slice(buf) {
                    Ok(n) => {
                        // n may be less than buf.len() if buffer limited; smoltcp send_slice returns enqueued amount
                        // If it enqueued 0 but can_send was true, treat as not ready
                        if n == 0 {
                            None
                        } else {
                            Some(Ok(n))
                        }
                    }
                    Err(_) => Some(Err(super::EPIPE)),
                }
            } else if !sock.may_send() {
                // Not open for sending (SynSent, Closed, etc). For blocking, wait; for nonblock EAGAIN.
                // If socket is closed, EPIPE.
                if !sock.is_open() {
                    Some(Err(super::EPIPE))
                } else {
                    None
                }
            } else {
                // may_send true but buffer full
                None
            }
        };
        if let Some(r) = result {
            if r.is_ok() {
                crate::tasks::wake_pipe_waiters();
            }
            return r;
        }
        if nonblock {
            return Err(EAGAIN);
        }
        block_current_thread(PendingBlock::PipeWait);
    }
}

pub fn udp_read(udp: &Arc<SpinLock<UdpInner>>, buf: &mut [u8], nonblock_flag: bool) -> Result<usize, u64> {
    let (handle, nonblock) = {
        let u = udp.lock();
        (u.handle, u.nonblock || nonblock_flag)
    };
    loop {
        crate::net::poll();
        let got = {
            let mut guard = crate::net::SOCKETS.lock();
            let set = match guard.as_mut() {
                Some(s) => s,
                None => return Err(super::ENOMEM),
            };
            let sock = set.get_mut::<smoltcp::socket::udp::Socket>(handle);
            if sock.can_recv() {
                match sock.recv() {
                    Ok((payload, _meta)) => {
                        let n = payload.len().min(buf.len());
                        buf[..n].copy_from_slice(&payload[..n]);
                        Some(Ok(n))
                    }
                    Err(_) => None,
                }
            } else {
                None
            }
        };
        if let Some(r) = got {
            return r;
        }
        if nonblock {
            return Err(EAGAIN);
        }
        block_current_thread(PendingBlock::PipeWait);
    }
}

pub fn udp_write(udp: &Arc<SpinLock<UdpInner>>, buf: &[u8], nonblock_flag: bool) -> Result<usize, u64> {
    if buf.len() > 65535 {
        return Err(EINVAL);
    }
    let (handle, nonblock, peer) = {
        let u = udp.lock();
        (u.handle, u.nonblock || nonblock_flag, *u.peer.lock())
    };
    let peer = match peer {
        Some(p) => p,
        None => return Err(EDESTADDRREQ),
    };
    let remote = IpEndpoint::new(IpAddress::Ipv4(peer.0), peer.1);
    loop {
        crate::net::poll();
        let sent = {
            let mut guard = crate::net::SOCKETS.lock();
            let set = match guard.as_mut() {
                Some(s) => s,
                None => return Err(super::ENOMEM),
            };
            let sock = set.get_mut::<smoltcp::socket::udp::Socket>(handle);
            if !sock.is_open() {
                let port = NEXT_EPHEMERAL.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                let port = if port == 0 { 49152 } else { port };
                if sock.bind(port).is_err() {
                    return Err(EINVAL);
                }
            }
            match sock.send_slice(buf, remote) {
                Ok(()) => true,
                Err(_) => false,
            }
        };
        if sent {
            return Ok(buf.len());
        }
        if nonblock {
            return Err(EAGAIN);
        }
        block_current_thread(PendingBlock::PipeWait);
    }
}


pub fn inet_readiness(inet: &Arc<SpinLock<InetInner>>) -> (bool, bool) {
    // Use try_lock to avoid deadlock in poll context (fsio.rs holds process lock then calls readiness).
    let (handle, is_listener) = {
        // try to copy fields without blocking if inet lock contended
        if let Some(g) = inet.try_lock() {
            (g.handle, g.is_listener)
        } else {
            return (false, false);
        }
    };
    let Some(mut guard) = crate::net::SOCKETS.try_lock() else { return (false, false); };
    let Some(set) = guard.as_mut() else { return (false, false); };
    let sock = set.get_mut::<TcpSocket>(handle);
    if is_listener {
        let readable = sock.state() == smoltcp::socket::tcp::State::Established
            || (sock.is_active()
                && sock.state() != smoltcp::socket::tcp::State::Listen
                && sock.state() != smoltcp::socket::tcp::State::SynReceived);
        return (readable, false);
    }
    let state = sock.state();
    let readable = sock.can_recv() || !sock.is_open();
    let writable = sock.can_send()
        || state == smoltcp::socket::tcp::State::Established
        || state == smoltcp::socket::tcp::State::Closed
        || state == smoltcp::socket::tcp::State::TimeWait;
    (readable, writable)
}

fn user_str(ptr: u64) -> Option<String> {
    if ptr == 0 {
        return None;
    }
    let mut bytes = Vec::new();
    unsafe {
        let mut p = ptr as *const u8;
        loop {
            if !crate::memory::space::validate_user_read_range(p as u64, 1usize) {
                return None;
            }
            let b = *p;
            if b == 0 {
                break;
            }
            bytes.push(b);
            if bytes.len() >= 108 {
                break;
            }
            p = p.add(1);
        }
    }
    String::from_utf8(bytes).ok()
}

const AF_NETLINK: u64 = 16;

/// 41
pub fn sys_socket(domain: u64, sock_type: u64, _protocol: u64) -> u64 {
    // Allow Unix/Netlink as before, plus Inet.
    if domain == AF_UNIX || domain == AF_NETLINK {
        let inner = SockInner {
            kind: SockKind::Unbound,
            nonblock: sock_type & SOCK_NONBLOCK != 0,
            is_netlink: domain == AF_NETLINK,
        };
        match super::epoll::alloc_fd_for_vnode(
            VNode::UnixSocket(Arc::new(SpinLock::new(inner))),
            if sock_type & SOCK_CLOEXEC != 0 { SOCK_CLOEXEC } else { 0 },
        ) {
            Ok((_, fd)) => return fd as u64,
            Err(e) => return e,
        }
    }
    if domain == AF_INET6 {
        return EAFNOSUPPORT;
    }
    if domain == AF_INET {
        let stype = sock_type & 0xF;
        if stype == SOCK_DGRAM {
            use smoltcp::socket::udp::Socket as UdpSocket;
            use smoltcp::storage::{PacketBuffer, PacketMetadata};
            let rx = PacketBuffer::new(
                alloc::vec![PacketMetadata::EMPTY; 8],
                alloc::vec![0u8; 16384],
            );
            let tx = PacketBuffer::new(
                alloc::vec![PacketMetadata::EMPTY; 8],
                alloc::vec![0u8; 16384],
            );
            let udp = UdpSocket::new(rx, tx);
            let handle = {
                let mut guard = crate::net::SOCKETS.lock();
                let set = match guard.as_mut() {
                    Some(s) => s,
                    None => return super::ENOMEM,
                };
                set.add(udp)
            };
            let inner = UdpInner {
                handle,
                nonblock: sock_type & SOCK_NONBLOCK != 0,
                peer: SpinLock::new(None),
            };
            match super::epoll::alloc_fd_for_vnode(
                VNode::UdpSocket(Arc::new(SpinLock::new(inner))),
                if sock_type & SOCK_CLOEXEC != 0 { SOCK_CLOEXEC } else { 0 },
            ) {
                Ok((_, fd)) => return fd as u64,
                Err(e) => return e,
            }
        }
        // ponytail: 16K rx/tx per socket, enough for HTTP headers
        let rx = SocketBuffer::new(alloc::vec![0u8; 16384]);
        let tx = SocketBuffer::new(alloc::vec![0u8; 16384]);
        let mut tcp = TcpSocket::new(rx, tx);
        tcp.set_nagle_enabled(false);
        // Add to global set
        let handle = {
            let mut guard = crate::net::SOCKETS.lock();
            let set = match guard.as_mut() {
                Some(s) => s,
                None => return super::ENOMEM,
            };
            set.add(tcp)
        };
        let inner = InetInner {
            handle,
            nonblock: sock_type & SOCK_NONBLOCK != 0,
            bound_port: None,
            is_listener: false,
            backlog: 16,
        };
        match super::epoll::alloc_fd_for_vnode(
            VNode::InetSocket(Arc::new(SpinLock::new(inner))),
            if sock_type & SOCK_CLOEXEC != 0 { SOCK_CLOEXEC } else { 0 },
        ) {
            Ok((_, fd)) => return fd as u64,
            Err(e) => {
                // clean up handle on failure
                if let Some(mut guard) = crate::net::SOCKETS.try_lock() {
                    if let Some(set) = guard.as_mut() {
                        let _ = set.remove(handle);
                    }
                }
                return e;
            }
        }
    }
    EPROTONOSUPPORT
}

fn sockaddr_path(addr_ptr: u64, len: u64) -> Option<String> {
    if addr_ptr == 0 || len < 3 {
        return None;
    }
    user_str(addr_ptr + 2)
}

/// 49
pub fn sys_bind(fd: u64, addr: u64, len: u64) -> u64 {
    // Inet bind: parse sockaddr_in and record bound port
    if let Some(inet) = get_inet(fd as i32) {
        if addr == 0 || len < 2 {
            return EINVAL;
        }
        let family = unsafe { *(addr as *const u16) };
        if family != AF_INET as u16 && family != AF_INET6 as u16 {
            return EINVAL;
        }
        if let Some((_ip, port)) = parse_sockaddr_in(addr, len) {
            inet.lock().bound_port = Some(port);
            return 0;
        }
        return EINVAL;
    }
    if let Some(udp) = get_udp(fd as i32) {
        if addr == 0 || len < 2 {
            return EINVAL;
        }
        let family = unsafe { *(addr as *const u16) };
        if family != AF_INET as u16 && family != AF_INET6 as u16 {
            return EINVAL;
        }
        if let Some((_ip, port)) = parse_sockaddr_in(addr, len) {
            let handle = udp.lock().handle;
            let mut guard = crate::net::SOCKETS.lock();
            let set = match guard.as_mut() {
                Some(s) => s,
                None => return super::ENOMEM,
            };
            let sock = set.get_mut::<smoltcp::socket::udp::Socket>(handle);
            return match sock.bind(port) {
                Ok(()) => 0,
                Err(_) => EINVAL,
            };
        }
        return EINVAL;
    }
    let sock = match get_sock(fd as i32) {
        Some(s) => s,
        None => return ENOTSOCK,
    };
    if addr == 0 || len < 2 {
        return EINVAL;
    }
    let family = unsafe { *(addr as *const u16) };
    if family == AF_NETLINK as u16 {
        return 0;
    }
    let path = match sockaddr_path(addr, len) {
        Some(p) => p,
        None => return EINVAL,
    };
    {
        let mut s = sock.lock();
        if !matches!(s.kind, SockKind::Unbound) {
            return EINVAL;
        }
        s.kind = SockKind::Listening {
            path: path.clone(),
            backlog: 16,
            pending: VecDeque::new(),
        };
    }
    let old = {
        let mut l = LISTENERS.lock();
        l.insert(path.clone(), sock.clone())
    };
    drop(old);
    crate::dprintln!("[bind-tmp] pid={} fd={} path={}", crate::tasks::current_thread().map(|t| t.process.lock().pid).unwrap_or(0), fd, path); // TEMP
    0
}

/// 50
pub fn sys_listen(fd: u64, backlog: i32) -> u64 {
    if let Some(inet) = get_inet(fd as i32) {
        let mut inner = inet.lock();
        let port = inner.bound_port.unwrap_or_else(|| {
            let p = NEXT_EPHEMERAL.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            inner.bound_port = Some(p);
            p
        });
        let handle = inner.handle;
        let mut sockets_guard = match crate::net::SOCKETS.lock() {
            guard if guard.is_some() => guard,
            _ => return super::ENOMEM,
        };
        let sockets = sockets_guard.as_mut().unwrap();
        let sock = sockets.get_mut::<TcpSocket>(handle);
        if let Err(_) = sock.listen(port) {
            return super::EADDRINUSE;
        }
        inner.is_listener = true;
        inner.backlog = backlog.clamp(1, 128) as usize;
        return 0;
    }
    if get_udp(fd as i32).is_some() {
        return EOPNOTSUPP;
    }
    let sock = match get_sock(fd as i32) {
        Some(s) => s,
        None => return ENOTSOCK,
    };
    let mut s = sock.lock();
    match &mut s.kind {
        SockKind::Listening { backlog: b, .. } => {
            *b = backlog.clamp(1, 128) as usize;
            0
        }
        _ => EOPNOTSUPP,
    }
}

/// 42
pub fn sys_connect(fd: u64, addr: u64, len: u64) -> u64 {
    if let Some(udp) = get_udp(fd as i32) {
        if addr == 0 || len < 2 {
            return EINVAL;
        }
        if !crate::memory::space::validate_user_read_range(addr, 2) {
            return super::EFAULT;
        }
        let family = unsafe { *(addr as *const u16) };
        if family == 0 {
            // AF_UNSPEC: disconnect
            *udp.lock().peer.lock() = None;
            return 0;
        }
        let (ipv4, port) = match parse_sockaddr_in(addr, len) {
            Some(v) => v,
            None => return EINVAL,
        };
        *udp.lock().peer.lock() = Some((ipv4, port));
        return 0;
    }

    // Try Inet first
    if let Some(inet) = get_inet(fd as i32) {
        if addr != 0 && len >= 2 && crate::memory::space::validate_user_read_range(addr, 2) {
            let family = unsafe { *(addr as *const u16) };
            if family == AF_INET6 as u16 {
                return EAFNOSUPPORT;
            }
        }
        let (ipv4, port) = match parse_sockaddr_in(addr, len) {
            Some(v) => v,
            None => return EINVAL,
        };
        let (handle, nonblock) = {
            let inner = inet.lock();
            (inner.handle, inner.nonblock)
        };
        let local_port = NEXT_EPHEMERAL.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let local_port = if local_port == 0 { 49152 } else { local_port };
        let remote = IpEndpoint::new(IpAddress::Ipv4(ipv4), port);

        // Initiate connection if not already connecting/connected
        let mut initiated = false;
        {
            let mut net_guard = crate::net::NET.lock();
            let iface = match net_guard.as_mut() {
                Some(i) => i,
                None => return ECONNREFUSED,
            };
            let mut sockets_guard = crate::net::SOCKETS.lock();
            let sockets = match sockets_guard.as_mut() {
                Some(s) => s,
                None => return ECONNREFUSED,
            };
            let sock = sockets.get_mut::<TcpSocket>(handle);
            match sock.state() {
                smoltcp::socket::tcp::State::Closed => {
                    match sock.connect(iface.context(), remote, local_port) {
                        Ok(()) => {
                            initiated = true;
                        }
                        Err(_) => return EINVAL,
                    }
                }
                smoltcp::socket::tcp::State::SynSent => {
                    if nonblock {
                        return EALREADY;
                    }
                }
                smoltcp::socket::tcp::State::Established => {
                    return 0;
                }
                _ => return EINVAL,
            }
        }
        if initiated {
            crate::net::poll();
        }

        if nonblock {
            return EINPROGRESS;
        }

        // Blocking wait until Established or Closed
        loop {
            crate::net::poll();
            let state = {
                let mut sockets_guard = crate::net::SOCKETS.lock();
                let sockets = match sockets_guard.as_mut() {
                    Some(s) => s,
                    None => return ECONNREFUSED,
                };
                let sock = sockets.get_mut::<TcpSocket>(handle);
                sock.state()
            };
            match state {
                smoltcp::socket::tcp::State::Established => return 0,
                smoltcp::socket::tcp::State::Closed | smoltcp::socket::tcp::State::TimeWait => return ECONNREFUSED,
                _ => {}
            }
            block_current_thread(PendingBlock::PipeWait);
        }
    }
    let sock = match get_sock(fd as i32) {
        Some(s) => s,
        None => return ENOTSOCK,
    };
    let path = match sockaddr_path(addr, len) {
        Some(p) => p,
        None => return EINVAL,
    };
    let listener = match LISTENERS.lock().get(&path).cloned() {
        Some(l) => l,
        None => return ENOENT,
    };

    let stream = Arc::new(UnixStream::new());
    {
        let mut l = listener.lock();
        match &mut l.kind {
            SockKind::Listening { pending, backlog, .. } => {
                if pending.len() >= *backlog {
                    return ECONNREFUSED;
                }
                pending.push_back((stream.clone(), 0));
            }
            _ => return ECONNREFUSED,
        }
    }
    crate::tasks::wake_pipe_waiters();

    sock.lock().kind = SockKind::Connected(stream, 0);
    0
}

/// 43 / 288 (accept4)
pub fn sys_accept4(fd: u64, addr: u64, addr_len_ptr: u64, flags: u64) -> u64 {
    let (ap, at) = crate::tasks::current_thread().map(|t| (t.process.lock().pid, t.id)).unwrap_or((0, 0));
    crate::dprintln!("[accept-tmp] pid={} tid={} accept(fd={})...", ap, at, fd); // TEMP
    if let Some(inet) = get_inet(fd as i32) {
        let is_listener = inet.lock().is_listener;
        if !is_listener {
            return EINVAL;
        }
        let port = inet.lock().bound_port.unwrap_or(0);
        loop {
            crate::net::poll();
            let mut accept_res: Option<(SocketHandle, Option<IpEndpoint>)> = None;
            {
                let mut sockets_guard = match crate::net::SOCKETS.lock() {
                    guard if guard.is_some() => guard,
                    _ => return super::ENOMEM,
                };
                let sockets = sockets_guard.as_mut().unwrap();
                let current_handle = inet.lock().handle;
                let sock = sockets.get_mut::<TcpSocket>(current_handle);
                let is_connected = sock.state() == smoltcp::socket::tcp::State::Established
                    || (sock.is_active()
                        && sock.state() != smoltcp::socket::tcp::State::Listen
                        && sock.state() != smoltcp::socket::tcp::State::SynReceived);
                if is_connected {
                    let remote = sock.remote_endpoint();
                    // Allocate a replacement TcpSocket to keep listening on this port
                    let rx = SocketBuffer::new(alloc::vec![0u8; 16384]);
                    let tx = SocketBuffer::new(alloc::vec![0u8; 16384]);
                    let mut new_listener = TcpSocket::new(rx, tx);
                    new_listener.set_nagle_enabled(false);
                    let _ = new_listener.listen(port);
                    let new_handle = sockets.add(new_listener);

                    // Update the listener's active handle
                    inet.lock().handle = new_handle;
                    accept_res = Some((current_handle, remote));
                }
            }

            if let Some((client_handle, remote_opt)) = accept_res {
                // If caller requested peer address, write sockaddr_in
                if addr != 0 && addr_len_ptr != 0 {
                    if crate::memory::space::validate_user_write_range(addr, 16)
                        && crate::memory::space::validate_user_write_range(addr_len_ptr, 4)
                    {
                        let remote_ep = remote_opt.unwrap_or(IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED), 0));
                        let ip_octets: [u8; 4] = match remote_ep.addr {
                            IpAddress::Ipv4(ip) => ip.0,
                        };
                        unsafe {
                            let need = 16u32;
                            let avail = *(addr_len_ptr as *const u32);
                            let w = need.min(avail) as usize;
                            if w >= 2 {
                                *(addr as *mut u16) = AF_INET as u16;
                            }
                            if w >= 4 {
                                *(addr.wrapping_add(2) as *mut u16) = remote_ep.port.to_be();
                            }
                            if w >= 8 {
                                core::ptr::copy_nonoverlapping(ip_octets.as_ptr(), addr.wrapping_add(4) as *mut u8, 4);
                                if w > 8 {
                                    core::ptr::write_bytes(addr.wrapping_add(8) as *mut u8, 0, w - 8);
                                }
                            }
                            *(addr_len_ptr as *mut u32) = need;
                        }
                    }
                }

                let client_inner = InetInner {
                    handle: client_handle,
                    nonblock: flags & SOCK_NONBLOCK != 0,
                    bound_port: Some(port),
                    is_listener: false,
                    backlog: 0,
                };
                match super::epoll::alloc_fd_for_vnode(
                    VNode::InetSocket(Arc::new(SpinLock::new(client_inner))),
                    if flags & SOCK_CLOEXEC != 0 { SOCK_CLOEXEC } else { 0 },
                ) {
                    Ok((_, nfd)) => return nfd as u64,
                    Err(e) => return e,
                }
            }

            if flags & SOCK_NONBLOCK != 0 || inet.lock().nonblock {
                return EAGAIN;
            }
            block_current_thread(PendingBlock::PipeWait);
        }
    }
    let sock = match get_sock(fd as i32) {
        Some(s) => s,
        None => return ENOTSOCK,
    };
    loop {
        {
            let lpath = match &sock.lock().kind {
                SockKind::Listening { path, .. } => path.clone(),
                _ => alloc::string::String::from("?"),
            };
            let mut s = sock.lock();
            if let SockKind::Listening { pending, .. } = &mut s.kind {
                if let Some((stream, client_end)) = pending.pop_front() {
                    let inner = SockInner {
                        kind: SockKind::Connected(stream, client_end ^ 1),
                        nonblock: flags & SOCK_NONBLOCK != 0,
                        is_netlink: false,
                    };
                    drop(s);
                    // Write peer address if caller requested it (Rust std UnixListener::accept requires AF_UNIX)
                    if addr != 0 && addr_len_ptr != 0 {
                        if crate::memory::space::validate_user_write_range(addr_len_ptr, 4) {
                            let max_len = unsafe { *(addr_len_ptr as *const u32) } as usize;
                            if max_len >= 2 && crate::memory::space::validate_user_write_range(addr, 2) {
                                unsafe {
                                    *(addr as *mut u16) = AF_UNIX as u16;
                                    if max_len > 2 && crate::memory::space::validate_user_write_range(addr.wrapping_add(2), 1) {
                                        *(addr.wrapping_add(2) as *mut u8) = 0;
                                    }
                                    *(addr_len_ptr as *mut u32) = 2;
                                }
                            }
                        }
                    }
                    let of_flags = (if flags & SOCK_CLOEXEC != 0 { SOCK_CLOEXEC } else { 0 })
                        | (if flags & SOCK_NONBLOCK != 0 { 0x800 } else { 0 });
                    match super::epoll::alloc_fd_for_vnode(
                        VNode::UnixSocket(Arc::new(SpinLock::new(inner))),
                        of_flags,
                    ) {
                        Ok((_, nfd)) => {
                            crate::dprintln!("[accept-tmp] pid={} path={} -> nfd={}", crate::tasks::current_thread().map(|t| t.process.lock().pid).unwrap_or(0), lpath, nfd); // TEMP
                            return nfd as u64;
                        }
                        Err(e) => return e,
                    }
                }
            } else {
                return EINVAL;
            }
        }
        // ponytail: honor the listener's own O_NONBLOCK (fcntl), not just accept4 flags.
        if flags & SOCK_NONBLOCK != 0 || sock.lock().nonblock {
            return EAGAIN;
        }
        block_current_thread(PendingBlock::PipeWait);
    }
}

/// 44
pub fn sys_sendto(fd: u64, buf: u64, len: u64, flags: u64, dest_addr: u64, addr_len: u64) -> u64 {
    // Real UDP via smoltcp.
    if let Some(udp) = get_udp(fd as i32) {
        if len > 65535 {
            return EINVAL;
        }
        if !crate::memory::space::validate_user_read_range(buf, len as usize) {
            return super::EFAULT;
        }
        let data: Vec<u8> = unsafe { core::slice::from_raw_parts(buf as *const u8, len as usize).to_vec() };
        let peer = if dest_addr != 0 {
            match parse_sockaddr_in(dest_addr, addr_len) {
                Some(p) => p,
                None => return EINVAL,
            }
        } else {
            match *udp.lock().peer.lock() {
                Some(p) => p,
                None => return EINVAL,
            }
        };
        *udp.lock().peer.lock() = Some(peer);
        let nonblock = udp.lock().nonblock || (flags as i32 & MSG_DONTWAIT != 0);
        let remote = IpEndpoint::new(IpAddress::Ipv4(peer.0), peer.1);
        loop {
            crate::net::poll();
            let sent = {
                let mut guard = crate::net::SOCKETS.lock();
                let set = match guard.as_mut() {
                    Some(s) => s,
                    None => return super::ENOMEM,
                };
                let sock = set.get_mut::<smoltcp::socket::udp::Socket>(udp.lock().handle);
                if !sock.is_open() {
                    // Bind an ephemeral port on first use.
                    let port = NEXT_EPHEMERAL.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    let port = if port == 0 { 49152 } else { port };
                    if sock.bind(port).is_err() {
                        return EINVAL;
                    }
                }
                match sock.send_slice(&data, remote) {
                    Ok(()) => true,
                    Err(_) => false,
                }
            };
            if sent {
                return len;
            }
            if nonblock {
                return EAGAIN;
            }
            block_current_thread(PendingBlock::PipeWait);
        }
    }
    // Inet fallback: treat as write
    if let Some(inet) = get_inet(fd as i32) {
        if !crate::memory::space::validate_user_read_range(buf, len as usize) {
            return super::EFAULT;
        }
        let slice = unsafe { core::slice::from_raw_parts(buf as *const u8, len as usize) };
        let nb = inet.lock().nonblock || (flags as i32 & MSG_DONTWAIT != 0);
        match inet_write(&inet, slice, nb) {
            Ok(n) => return n as u64,
            Err(e) => return e,
        }
    }
    // ponytail: AF_UNIX connected send (send() == sendto with NULL addr, e.g. wayland).
    if let Some((sock, _)) = get_sock_and_file(fd as i32) {
        if !crate::memory::space::validate_user_read_range(buf, len as usize) {
            return super::EFAULT;
        }
        let slice = unsafe { core::slice::from_raw_parts(buf as *const u8, len as usize) };
        let nb = sock.lock().nonblock || (flags as i32 & MSG_DONTWAIT != 0);
        match sock_write(&sock, slice, nb) {
            Ok(n) => return n as u64,
            Err(e) => return e,
        }
    }
    ENOTSOCK
}

/// 45
pub fn sys_recvfrom(fd: u64, buf: u64, len: u64, flags: u64, src_addr: u64, addr_len_ptr: u64) -> u64 {
    if let Some(udp) = get_udp(fd as i32) {
        let nonblock = udp.lock().nonblock || (flags as i32 & MSG_DONTWAIT != 0);
        if !crate::memory::space::validate_user_write_range(buf, len as usize) {
            return super::EFAULT;
        }
        loop {
            crate::net::poll();
            let got = {
                let mut guard = crate::net::SOCKETS.lock();
                let set = match guard.as_mut() {
                    Some(s) => s,
                    None => return super::ENOMEM,
                };
                let sock = set.get_mut::<smoltcp::socket::udp::Socket>(udp.lock().handle);
                if sock.can_recv() {
                    match sock.recv() {
                        Ok((payload, meta)) => {
                            let n = (payload.len() as u64).min(len);
                            unsafe {
                                core::ptr::copy_nonoverlapping(
                                    payload.as_ptr(),
                                    buf as *mut u8,
                                    n as usize,
                                );
                            }
                            // Report the true source endpoint.
                            if src_addr != 0 && addr_len_ptr != 0
                                && crate::memory::space::validate_user_write_range(src_addr, 16)
                                && crate::memory::space::validate_user_write_range(addr_len_ptr, 4)
                            {
                                // Irrefutable: `RecvMeta.endpoint` is an
                                // `IpEndpoint`, and `Ipv4` is its only address
                                // variant, so this used to be an `if let` the
                                // compiler could prove always matches.
                                let IpEndpoint {
                                    addr: IpAddress::Ipv4(ip),
                                    port,
                                } = meta.endpoint;
                                let ip_bytes = ip.as_bytes();
                                unsafe {
                                    *(src_addr as *mut u16) = AF_INET as u16;
                                    *(src_addr.wrapping_add(2) as *mut u16) = port.to_be();
                                    core::ptr::copy_nonoverlapping(
                                        ip_bytes.as_ptr(),
                                        src_addr.wrapping_add(4) as *mut u8,
                                        4,
                                    );
                                    core::ptr::write_bytes(
                                        src_addr.wrapping_add(8) as *mut u8,
                                        0,
                                        8,
                                    );
                                    *(addr_len_ptr as *mut u32) = 16;
                                }
                            }
                            Some(n)
                        }
                        Err(_) => None,
                    }
                } else {
                    None
                }
            };
            if let Some(n) = got {
                return n;
            }
            if nonblock {
                return EAGAIN;
            }
            block_current_thread(PendingBlock::PipeWait);
        }
    }
    if let Some(inet) = get_inet(fd as i32) {
        if !crate::memory::space::validate_user_write_range(buf, len as usize) {
            return super::EFAULT;
        }
        let slice = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, len as usize) };
        let nb = inet.lock().nonblock || (flags as i32 & MSG_DONTWAIT != 0);
        match inet_read(&inet, slice, nb) {
            Ok(n) => {
                if src_addr != 0 && addr_len_ptr != 0
                    && crate::memory::space::validate_user_write_range(src_addr, 16)
                    && crate::memory::space::validate_user_write_range(addr_len_ptr, 4)
                {
                    unsafe {
                        *(src_addr as *mut u16) = AF_INET as u16;
                        *(addr_len_ptr as *mut u32) = 16;
                    }
                }
                return n as u64;
            }
            Err(e) => return e,
        }
    }
    // ponytail: NETLINK_KOBJECT_UEVENT (protocol 15) recv — socket() keeps
    // no per-socket queue, so all bound AF_NETLINK sockets share UEVENT_QUEUE
    // (protocol is not stored on SockInner; the kernel only speaks uevents on
    // AF_NETLINK, so every netlink socket is a protocol-15 listener).
    // Readers block on PipeWait, woken by uevent broadcast.
    if let Some(sock) = get_sock(fd as i32) {
        if sock.lock().is_netlink {
            if !crate::memory::space::validate_user_write_range(buf, len as usize) {
                return super::EFAULT;
            }
            let nonblock = sock.lock().nonblock || (flags as i32 & MSG_DONTWAIT != 0);
            loop {
                if let Some(msg) = crate::drivers::uevent::dequeue() {
                    // ponytail: datagram truncated to fit, like Linux netlink
                    // (no MSG_TRUNC — the P1T4 listener sizes its buffer).
                    let n = (msg.len() as u64).min(len);
                    unsafe {
                        core::ptr::copy_nonoverlapping(msg.as_ptr(), buf as *mut u8, n as usize);
                    }
                    if src_addr != 0 && addr_len_ptr != 0
                        && crate::memory::space::validate_user_write_range(src_addr, 12)
                        && crate::memory::space::validate_user_write_range(addr_len_ptr, 4)
                    {
                        unsafe {
                            *(src_addr as *mut u16) = AF_NETLINK as u16;
                            *(src_addr.wrapping_add(2) as *mut u16) = 0;
                            *(src_addr.wrapping_add(4) as *mut u32) = 0; // kernel pid 0
                            *(src_addr.wrapping_add(8) as *mut u32) = 0; // no groups
                            *(addr_len_ptr as *mut u32) = 12;
                        }
                    }
                    return n;
                }
                if nonblock {
                    return EAGAIN;
                }
                block_current_thread(PendingBlock::PipeWait);
            }
        }
    }
    // ponytail: AF_UNIX connected recv (recv() == recvfrom with NULL addr).
    if let Some((sock, _)) = get_sock_and_file(fd as i32) {
        if !crate::memory::space::validate_user_write_range(buf, len as usize) {
            return super::EFAULT;
        }
        let slice = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, len as usize) };
        let nb = sock.lock().nonblock || (flags as i32 & MSG_DONTWAIT != 0);
        match sock_read(&sock, slice, nb) {
            Ok(n) => return n as u64,
            Err(e) => return e,
        }
    }
    ENOTSOCK
}

/// Blocking read used by vfs read paths. Returns Ok(0) on EOF.
pub fn sock_read(
    sock: &Arc<SpinLock<SockInner>>,
    buf: &mut [u8],
    nonblock: bool,
) -> Result<usize, u64> {
    loop {
        {
            let s = sock.lock();
            let (stream, my_end) = match &s.kind {
                SockKind::Connected(st, e) => (st.clone(), *e),
                _ => return Err(ENOTSOCK),
            };
            let mut q = stream.bufs[my_end].lock();
            if let Some(front) = q.front_mut() {
                let n = front.bytes.len().min(buf.len());
                buf[..n].copy_from_slice(&front.bytes[..n]);
                front.bytes.drain(..n);
                stream.buffered[my_end]
                    .fetch_sub(n, core::sync::atomic::Ordering::Release);
                if front.bytes.is_empty() {
                    q.pop_front();
                }
                return Ok(n);
            }
            if !stream.peer_open(my_end) {
                return Ok(0);
            }
        }
        if nonblock {
            return Err(EAGAIN);
        }
        block_current_thread(PendingBlock::PipeWait);
    }
}

/// Blocking write used by vfs write paths.
pub fn sock_write(
    sock: &Arc<SpinLock<SockInner>>,
    buf: &[u8],
    nonblock: bool,
) -> Result<usize, u64> {
    loop {
        {
            let s = sock.lock();
            let (stream, my_end) = match &s.kind {
                SockKind::Connected(st, e) => (st.clone(), *e),
                _ => return Err(ENOTSOCK),
            };
            let peer = my_end ^ 1;
            if !stream.peer_open(my_end) {
                return Err(super::EPIPE);
            }
            let total = stream.buffered[peer]
                .load(core::sync::atomic::Ordering::Acquire);
            if total + buf.len() <= SOCK_BUF_LIMIT {
                stream.bufs[peer].lock().push_back(UnixMsg {
                    bytes: buf.to_vec(),
                    fds: Vec::new(),
                });
                stream.buffered[peer]
                    .store(total + buf.len(), core::sync::atomic::Ordering::Release);
                crate::tasks::wake_pipe_waiters();
                return Ok(buf.len());
            }
        }
        if nonblock {
            return Err(EAGAIN);
        }
        block_current_thread(PendingBlock::PipeWait);
    }
}

/// Readiness bits for poll/epoll: EPOLLIN | EPOLLOUT.
/// TEMP-DIAG: queued (messages, bytes) per stream direction for epoll-hunt.
/// (MAX, 0, 0, 0) when not a connected stream.
pub fn sock_queue_depths(sock: &Arc<SpinLock<SockInner>>) -> (usize, usize, usize, usize) {
    let s = sock.lock();
    match &s.kind {
        SockKind::Connected(stream, _) => {
            let q0 = stream.bufs[0].lock();
            let q1 = stream.bufs[1].lock();
            (
                q0.len(),
                q0.iter().map(|m| m.bytes.len()).sum::<usize>(),
                q1.len(),
                q1.iter().map(|m| m.bytes.len()).sum::<usize>(),
            )
        }
        _ => (usize::MAX, 0, 0, 0),
    }
}

pub fn sock_readiness(sock: &Arc<SpinLock<SockInner>>) -> (bool, bool) {    let s = sock.lock();
    if s.is_netlink {
        // ponytail: shared uevent queue; netlink is always writable.
        return (crate::drivers::uevent::has_pending(), true);
    }
    match &s.kind {
        SockKind::Connected(stream, my_end) => {
            let readable = !stream.bufs[*my_end].lock().is_empty()
                || !stream.peer_open(*my_end);
            let writable = stream.peer_open(*my_end)
                && stream.buffered[my_end ^ 1]
                    .load(core::sync::atomic::Ordering::Acquire)
                    < SOCK_BUF_LIMIT;
            (readable, writable)
        }
        SockKind::Listening { pending, .. } => (!pending.is_empty(), false),
        _ => (false, false),
    }
}

fn parse_msghdr(msg_ptr: u64) -> Result<(u64, u64, u64, u64, u64), u64> {
    const MSGHDR_SZ: u64 = 56;
    if msg_ptr == 0 || !crate::memory::space::validate_user_read_range(msg_ptr, MSGHDR_SZ as usize) {
        return Err(EINVAL);
    }
    unsafe {
        let iov = *(msg_ptr.wrapping_add(16) as *const u64);
        let iovlen = *(msg_ptr.wrapping_add(24) as *const u64);
        let control = *(msg_ptr.wrapping_add(32) as *const u64);
        let controllen = *(msg_ptr.wrapping_add(40) as *const u64);
        if iov == 0 || iovlen == 0 || iovlen > 1024 {
            return Err(EINVAL);
        }
        if !crate::memory::space::validate_user_read_range(iov, (iovlen * 16) as usize) {
            return Err(EINVAL);
        }
        Ok((iov, iovlen, control, controllen, msg_ptr))
    }
}

/// 46
pub fn sys_sendmsg(fd: u64, msg_ptr: u64, flags: u64) -> u64 {
    let (sock, open_file) = match get_sock_and_file(fd as i32) {
        Some(v) => v,
        None => {
            // Try Inet fallback: use inet_write for dgram? For Tcp, sendmsg via write.
            if let Some(inet) = get_inet(fd as i32) {
                let (iov, iovlen, _control, _controllen, _) = match parse_msghdr(msg_ptr) {
                    Ok(v) => v,
                    Err(e) => return e,
                };
                let mut payload = Vec::new();
                unsafe {
                    for i in 0..iovlen as usize {
                        let b = *(iov.wrapping_add(i as u64 * 16) as *const u64);
                        let l = *(iov.wrapping_add(i as u64 * 16).wrapping_add(8) as *const u64);
                        if l == 0 { continue; }
                        if b == 0 || !crate::memory::space::validate_user_read_range(b, l as usize) {
                            return EINVAL;
                        }
                        payload.extend_from_slice(core::slice::from_raw_parts(b as *const u8, l as usize));
                    }
                }
                if payload.is_empty() { return 0; }
                let nonblock = inet.lock().nonblock || (flags as i32 & MSG_DONTWAIT != 0);
                match inet_write(&inet, &payload, nonblock) {
                    Ok(n) => return n as u64,
                    Err(e) => return e,
                }
            }
            return ENOTSOCK;
        }
    };
    let (iov, iovlen, control, controllen, _) = match parse_msghdr(msg_ptr) {
        Ok(v) => v,
        Err(e) => return e,
    };

    let mut payload = Vec::new();
    unsafe {
        for i in 0..iovlen as usize {
            let b = *(iov.wrapping_add(i as u64 * 16) as *const u64);
            let l = *(iov.wrapping_add(i as u64 * 16).wrapping_add(8) as *const u64);
            if l == 0 {
                continue;
            }
            if b == 0 || !crate::memory::space::validate_user_read_range(b, l as usize) {
                return EINVAL;
            }
            payload.extend_from_slice(core::slice::from_raw_parts(b as *const u8, l as usize));
        }
    }
    if payload.is_empty() {
        return 0;
    }

    // Control data: SCM_RIGHTS only. Store OpenFile clones so they survive
    // until the receiver collects them.
    let mut fds: Vec<Arc<SpinLock<OpenFile>>> = Vec::new();
    let sender_table: Vec<Option<Arc<SpinLock<OpenFile>>>> = {
        let t = crate::tasks::current_thread().unwrap();
        let p = t.process.lock();
        p.fd_table.clone()
    };
    let mut c = control;
    let cend = control.wrapping_add(controllen);
    while controllen > 0 && c + 16 <= cend {
        if !crate::memory::space::validate_user_read_range(c, 16) {
            break;
        }
        let clen = unsafe { *(c as *const u64) };
        let clevel = unsafe { *((c + 8) as *const u32) };
        let ctype = unsafe { *((c + 12) as *const u32) };
        if clen < 16 || c + clen > cend {
            break;
        }
        if clevel == SOL_SOCKET && ctype == SCM_RIGHTS && clen > 16 {
            let nfds = ((clen - 16) / 4) as usize;
            if crate::memory::space::validate_user_read_range(c + 16, nfds * 4) {
                unsafe {
                    for i in 0..nfds {
                        let sfd = *((c + 16 + (i * 4) as u64) as *const i32);
                        if sfd >= 0 {
                            if let Some(Some(of)) = sender_table.get(sfd as usize) {
                                fds.push(of.clone());
                            }
                        }
                    }
                }
            }
        }
        c += clen.div_ceil(8) * 8;
    }
    let _ = open_file;

    loop {
        {
            let s = sock.lock();
            let (stream, my_end) = match &s.kind {
                SockKind::Connected(st, e) => (st.clone(), *e),
                _ => return super::EPIPE,
            };
            let peer = my_end ^ 1;
            let total =
                stream.buffered[peer].load(core::sync::atomic::Ordering::Acquire);
            if total + payload.len() > SOCK_BUF_LIMIT {
                let nonblock =
                    s.nonblock || (flags as i32 & MSG_DONTWAIT != 0);
                drop(s);
                if nonblock {
                    return EAGAIN;
                }
                block_current_thread(PendingBlock::PipeWait);
                continue;
            }
            let plen = payload.len();
            stream.bufs[peer].lock().push_back(UnixMsg {
                bytes: payload,
                fds,
            });
            stream.buffered[peer].store(
                total + plen,
                core::sync::atomic::Ordering::Release,
            );
            crate::tasks::wake_pipe_waiters();
            return plen as u64;
        }
    }
}

/// 47
pub fn sys_recvmsg(fd: u64, msg_ptr: u64, flags: u64) -> u64 {
    // Inet path
    if let Some(inet) = get_inet(fd as i32) {
        let (iov, iovlen, control, controllen, _) = match parse_msghdr(msg_ptr) {
            Ok(v) => v,
            Err(e) => return e,
        };
        // For Tcp, recv into iovecs via inet_read
        // We need to collect total to read: try to read into temp then copy to iovecs.
        // Use a temporary buffer sized to sum of iov lens (cap 64K)
        let mut total_len: usize = 0;
        unsafe {
            for i in 0..iovlen as usize {
                let l = *(iov.wrapping_add(i as u64 * 16).wrapping_add(8) as *const u64) as usize;
                total_len = total_len.saturating_add(l);
            }
        }
        if total_len == 0 { return 0; }
        let mut tmp = alloc::vec![0u8; total_len.min(65536)];
        let nonblock = inet.lock().nonblock || (flags as i32 & MSG_DONTWAIT != 0);
        let n = match inet_read(&inet, &mut tmp, nonblock) {
            Ok(n) => n,
            Err(e) => return e,
        };
        // copy out to iovecs
        let mut remaining = &tmp[..n];
        let mut written = 0usize;
        for i in 0..iovlen as usize {
            if remaining.is_empty() { break; }
            let b = unsafe { *(iov.wrapping_add(i as u64 * 16) as *const u64) };
            let l = unsafe { *(iov.wrapping_add(i as u64 * 16).wrapping_add(8) as *const u64) } as usize;
            if b == 0 || l == 0 { continue; }
            if !crate::memory::space::validate_user_write_range(b, l) { return EINVAL; }
            let take = remaining.len().min(l);
            unsafe { core::ptr::copy_nonoverlapping(remaining.as_ptr(), b as *mut u8, take); }
            remaining = &remaining[take..];
            written += take;
        }
        unsafe {
            (msg_ptr.wrapping_add(8) as *mut u64).write(0);
            (msg_ptr.wrapping_add(40) as *mut u64).write(0);
            (msg_ptr.wrapping_add(48) as *mut i64).write(0);
            let _ = control; let _ = controllen;
        }
        return written as u64;
    }
    let sock = match get_sock(fd as i32) {
        Some(s) => s,
        None => return ENOTSOCK,
    };
    let (iov, iovlen, control, controllen, _) = match parse_msghdr(msg_ptr) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let peek = flags as i32 & MSG_PEEK != 0;

    loop {
        let outcome;
        {
            let s = sock.lock();
            let (stream, my_end) = match &s.kind {
                SockKind::Connected(st, e) => (st.clone(), *e),
                _ => return ENOTSOCK,
            };
            let q = stream.bufs[my_end].lock();
            if let Some(front) = q.front().map(|m| (m.bytes.clone(), m.fds.clone())) {
                outcome = Ok(front);
            } else if !stream.peer_open(my_end) {
                outcome = Ok((Vec::new(), Vec::new()));
            } else {
                outcome = Err(EAGAIN);
            }
        }

        match outcome {
            Ok((bytes, fds)) => {
                // Copy out into caller iovecs.
                let mut written = 0usize;
                let mut data: &[u8] = &bytes;
                for i in 0..iovlen as usize {
                    if data.is_empty() {
                        break;
                    }
                    let b = unsafe { *(iov.wrapping_add(i as u64 * 16) as *const u64) };
                    let l = unsafe {
                        *(iov.wrapping_add(i as u64 * 16).wrapping_add(8) as *const u64)
                    } as usize;
                    if b == 0 || l == 0 {
                        continue;
                    }
                    if !crate::memory::space::validate_user_write_range(b, l) {
                        return EINVAL;
                    }
                    let n = data.len().min(l);
                    unsafe {
                        core::ptr::copy_nonoverlapping(data.as_ptr(), b as *mut u8, n);
                    }
                    data = &data[n..];
                    written += n;
                }
                if !peek && written > 0 {
                    let s = sock.lock();
                    if let SockKind::Connected(stream, my_end) = &s.kind {
                        let mut q = stream.bufs[*my_end].lock();
                        if let Some(mut m) = q.pop_front() {
                            let total = m.bytes.len();
                            m.bytes.drain(..written.min(total));
                            stream.buffered[*my_end].fetch_sub(
                                written.min(total),
                                core::sync::atomic::Ordering::Release,
                            );
                            if !m.bytes.is_empty() {
                                q.push_front(m);
                            }
                        }
                    }
                    crate::tasks::wake_pipe_waiters();
                }
                // SCM_RIGHTS attach.
                let mut ctrl_used = 0u64;
                if !fds.is_empty() && control != 0 && controllen >= 20 {
                    let mut new_fds: Vec<i32> = Vec::new();
                    for of in fds.iter() {
                        let (vnode, path, oflags) = {
                            let file = of.lock();
                            (file.clone_vnode(), file.path.clone(), file.flags)
                        };
                        if let Ok((_pid, nfd)) = super::epoll::alloc_fd_for_vnode(vnode, 0) {
                            if let Some(t) = crate::tasks::current_thread() {
                                let p = t.process.lock();
                                if let Some(Some(slot)) = p.fd_table.get(nfd) {
                                    // ponytail: Linux shares the whole open file
                                    // description over SCM_RIGHTS — flags (notably
                                    // O_NONBLOCK) and path must follow the clone,
                                    // or the receiver spuriously blocks forever.
                                    let mut s = slot.lock();
                                    s.flags = oflags;
                                    if !path.is_empty() {
                                        s.path = path.clone();
                                    }
                                }
                            }
                            // ponytail: SCM clones the description (not shared
                            // like Linux); the clone still needs its own evdev
                            // stream, minor riding in the cloned VNode.
                            //
                            // Collect the key+minor under process.lock(), then
                            // drop it before registering. `input_register` reads
                            // current_thread().process.lock() for its diagnostic
                            // pid, and SpinLock is non-reentrant: holding
                            // process.lock() across that call panicked the whole
                            // kernel the first time a seatd-style compositor
                            // received an input fd over SCM_RIGHTS.
                            let input_stream = {
                                let t = crate::tasks::current_thread();
                                t.and_then(|t| {
                                    let p = t.process.lock();
                                    let slot = p.fd_table.get(nfd)?.as_ref()?;
                                    let minor = match slot.lock().vnode {
                                        crate::fs::vfs::VNode::InputEventDevice { minor }
                                        | crate::fs::vfs::VNode::InputMiceDevice { minor } => minor,
                                        _ => return None,
                                    };
                                    Some((alloc::sync::Arc::as_ptr(slot) as usize, minor))
                                })
                            };
                            if let Some((key, minor)) = input_stream {
                                crate::drivers::keyboard::input_register(key, minor);
                            }
                            crate::dprintln!("sys_recvmsg: SCM_RIGHTS installed fd={} path='{}'", nfd, path);
                            new_fds.push(nfd as i32);
                        }
                    }
                    let space = controllen.min(4096);
                    let cmsg_len = 16 + new_fds.len() * 4;
                    let clen = cmsg_len.div_ceil(8) * 8;
                    if !new_fds.is_empty() && clen as u64 <= space {
                        unsafe {
                            (control as *mut u64).write(cmsg_len as u64);
                            ((control.wrapping_add(8)) as *mut u32).write(SOL_SOCKET);
                            ((control.wrapping_add(12)) as *mut u32).write(SCM_RIGHTS);
                            for (i, f) in new_fds.iter().enumerate() {
                                ((control.wrapping_add(16 + (i * 4) as u64)) as *mut i32)
                                    .write(*f);
                            }
                        }
                        ctrl_used = clen as u64;
                    }
                }
                unsafe {
                    (msg_ptr.wrapping_add(8) as *mut u64).write(0);
                    (msg_ptr.wrapping_add(40) as *mut u64).write(ctrl_used);
                    (msg_ptr.wrapping_add(48) as *mut i64).write(0);
                }
                return written as u64;
            }
            Err(EAGAIN) => {
                let nonblock = sock.lock().nonblock || (flags as i32 & MSG_DONTWAIT != 0);
                if nonblock {
                    return EAGAIN;
                }
                block_current_thread(PendingBlock::PipeWait);
            }
            Err(e) => return e,
        }
    }
}

/// 53 — real bidirectional socketpair (replaces the pipe mock).
pub fn sys_socketpair(dom: u64, typ: u64, _proto: u64, sv: u64) -> u64 {
    if dom != AF_UNIX || typ & !(SOCK_NONBLOCK | SOCK_CLOEXEC) != SOCK_STREAM {
        return EPROTONOSUPPORT;
    }
    if sv == 0 || !crate::memory::space::validate_user_write_range(sv, 8usize) {
        return EINVAL;
    }
    let stream = Arc::new(UnixStream::new());
    let mk = |end: usize| {
        Arc::new(SpinLock::new(SockInner {
            kind: SockKind::Connected(stream.clone(), end),
            nonblock: typ & SOCK_NONBLOCK != 0,
            is_netlink: false,
        }))
    };
    let r = super::epoll::alloc_fd_for_vnode(VNode::UnixSocket(mk(0)), 0);
    let w = super::epoll::alloc_fd_for_vnode(VNode::UnixSocket(mk(1)), 0);
    match (r, w) {
        (Ok((_, a)), Ok((_, b))) => unsafe {
            (sv as *mut i32).write(a as i32);
            ((sv + 4) as *mut i32).write(b as i32);
            0
        },
        (Err(e), _) | (_, Err(e)) => e,
    }
}

/// 87
pub fn sys_unlink(path_ptr: u64) -> u64 {
    let path = match user_str(path_ptr) {
        Some(p) => p,
        None => return EINVAL,
    };
    let canonical = crate::fs::canonicalize_path("/", &path);
    let clean = canonical.trim_start_matches('/');
    if clean.is_empty() {
        return EBADF;
    }
    let (parent_rel, name) = match clean.rfind('/') {
        Some(pos) => (&clean[..pos], &clean[pos + 1..]),
        None => ("", clean),
    };
    let parent_vn = if parent_rel.is_empty() {
        crate::fs::lookup("/")
    } else {
        crate::fs::lookup_relative_to("/", parent_rel)
    };
    if name.is_empty() {
        return ENOENT;
    }
    match parent_vn {
        Some(VNode::FsNode { node, .. }) => match node.unlink(name) {
            Ok(()) => 0,
            Err(e) => e,
        },
        Some(_) => enotdir_err(),
        None => ENOENT,
    }
}

fn enotdir_err() -> u64 {
    super::ENOTDIR
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct UCred {
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
}

/// 55: `getsockopt(fd, level, optname, optval, optlen)`
pub fn sys_getsockopt(fd: u64, level: u64, optname: u64, optval: u64, optlen: u64) -> u64 {
    if !crate::memory::space::validate_user_write_range(optlen, 4) {
        return super::EFAULT;
    }
    let current_len = unsafe { *(optlen as *const u32) } as usize;

    const SOL_SOCKET_U64: u64 = 1;
    const SO_TYPE: u64 = 3;
    const SO_ERROR: u64 = 4;
    const SO_PEERCRED: u64 = 17;

    if level == SOL_SOCKET_U64 {
        if optname == SO_PEERCRED {
            if current_len < core::mem::size_of::<UCred>() {
                return super::EINVAL;
            }
            if !crate::memory::space::validate_user_write_range(optval, core::mem::size_of::<UCred>()) {
                return super::EFAULT;
            }
            let (peer_pid, peer_uid, peer_gid) = {
                let thread = match crate::tasks::current_thread() {
                    Some(t) => t,
                    None => return super::EBADF,
                };
                let p = thread.process.lock();
                (p.pid as u32, p.uid, p.gid)
            };
            let cred = UCred {
                pid: peer_pid,
                uid: peer_uid,
                gid: peer_gid,
            };
            unsafe {
                core::ptr::write(optval as *mut UCred, cred);
                *(optlen as *mut u32) = core::mem::size_of::<UCred>() as u32;
            }
            return 0;
        } else if optname == SO_ERROR {
            if current_len >= 4 {
                if !crate::memory::space::validate_user_write_range(optval, 4) {
                    return super::EFAULT;
                }
                let mut err = 0i32;
                if let Some(inet) = get_inet(fd as i32) {
                    let handle = inet.lock().handle;
                    if let Some(mut guard) = crate::net::SOCKETS.try_lock() {
                        if let Some(set) = guard.as_mut() {
                            let sock = set.get_mut::<TcpSocket>(handle);
                            if sock.state() == smoltcp::socket::tcp::State::Closed {
                                err = 111; // ECONNREFUSED
                            }
                        }
                    }
                }
                unsafe {
                    *(optval as *mut i32) = err;
                    *(optlen as *mut u32) = 4;
                }
            }
            return 0;
        } else if optname == SO_TYPE {
            if current_len >= 4 {
                if !crate::memory::space::validate_user_write_range(optval, 4) {
                    return super::EFAULT;
                }
                let stype = if get_udp(fd as i32).is_some() {
                    2 // SOCK_DGRAM
                } else {
                    1 // SOCK_STREAM
                };
                unsafe {
                    *(optval as *mut i32) = stype;
                    *(optlen as *mut u32) = 4;
                }
            }
            return 0;
        }
    }

    // ponytail: no IP source-routing options are ever set — report zero
    // length (sshd fatals on nonzero, killing every inbound connection).
    const IPPROTO_IP_U64: u64 = 0;
    const IP_OPTIONS: u64 = 4;
    if level == IPPROTO_IP_U64 && optname == IP_OPTIONS {
        unsafe {
            *(optlen as *mut u32) = 0;
        }
        return 0;
    }

    if current_len >= 4 && optval != 0 {
        if crate::memory::space::validate_user_write_range(optval, 4) {
            unsafe {
                *(optval as *mut i32) = 0;
                *(optlen as *mut u32) = 4;
            }
        }
    }
    0
}

/// 54: `setsockopt(fd, level, optname, optval, optlen)`
pub fn sys_setsockopt(fd: u64, level: u64, optname: u64, optval: u64, optlen: u64) -> u64 {
    const IPPROTO_TCP: u64 = 6;
    const TCP_NODELAY: u64 = 1;
    const SOL_SOCKET_U64: u64 = 1;
    const SO_REUSEADDR: u64 = 2;

    if optval != 0 && optlen >= 4 {
        if !crate::memory::space::validate_user_read_range(optval, 4) {
            return super::EFAULT;
        }
        let val = unsafe { *(optval as *const i32) };
        if level == IPPROTO_TCP && optname == TCP_NODELAY {
            if let Some(inet) = get_inet(fd as i32) {
                let handle = inet.lock().handle;
                if let Some(mut guard) = crate::net::SOCKETS.try_lock() {
                    if let Some(set) = guard.as_mut() {
                        let sock = set.get_mut::<TcpSocket>(handle);
                        sock.set_nagle_enabled(val == 0);
                    }
                }
            }
            return 0;
        }
        if level == SOL_SOCKET_U64 && optname == SO_REUSEADDR {
            // No-op success for SO_REUSEADDR
            return 0;
        }
    }
    0
}

/// 48: `shutdown(fd, how)`
pub fn sys_shutdown(fd: u64, how: u64) -> u64 {
    if let Some(inet) = get_inet(fd as i32) {
        let handle = inet.lock().handle;
        if let Some(mut guard) = crate::net::SOCKETS.try_lock() {
            if let Some(set) = guard.as_mut() {
                let sock = set.get_mut::<TcpSocket>(handle);
                // how: 0 = SHUT_RD, 1 = SHUT_WR, 2 = SHUT_RDWR
                if how == 1 || how == 2 {
                    sock.close();
                }
            }
        }
        crate::net::poll();
        return 0;
    }
    if let Some((_sock, _)) = get_sock_and_file(fd as i32) {
        return 0;
    }
    super::ENOTSOCK
}

/// 51: `getsockname(fd, addr, addrlen)`
pub fn sys_getsockname(fd: u64, addr: u64, addrlen: u64) -> u64 {
    if addrlen == 0 || !crate::memory::space::validate_user_write_range(addrlen, 4) {
        return super::EFAULT;
    }
    if let Some(inet) = get_inet(fd as i32) {
        let max_len = unsafe { *(addrlen as *const u32) } as usize;
        if addr != 0 && max_len >= 16 && crate::memory::space::validate_user_write_range(addr, 16) {
            let handle = inet.lock().handle;
            let mut ep_opt = None;
            if let Some(mut guard) = crate::net::SOCKETS.try_lock() {
                if let Some(set) = guard.as_mut() {
                    let sock = set.get_mut::<TcpSocket>(handle);
                    ep_opt = sock.local_endpoint();
                }
            }
            let bound_port = inet.lock().bound_port.unwrap_or(0);
            let port = ep_opt.map(|ep| ep.port).unwrap_or(bound_port);
            unsafe {
                *(addr as *mut u16) = AF_INET as u16;
                *(addr.wrapping_add(2) as *mut u16) = port.to_be();
                *(addr.wrapping_add(4) as *mut u32) = 0; // 0.0.0.0
                core::ptr::write_bytes(addr.wrapping_add(8) as *mut u8, 0, 8);
                *(addrlen as *mut u32) = 16;
            }
        }
        return 0;
    }
    // udp AF_INET
    if let Some(_udp) = get_udp(fd as i32) {
        let max_len = unsafe { *(addrlen as *const u32) } as usize;
        if addr != 0 && max_len >= 16 && crate::memory::space::validate_user_write_range(addr, 16) {
            unsafe {
                *(addr as *mut u16) = AF_INET as u16;
                *(addr.wrapping_add(2) as *mut u16) = 0;
                *(addr.wrapping_add(4) as *mut u32) = 0;
                core::ptr::write_bytes(addr.wrapping_add(8) as *mut u8, 0, 8);
                *(addrlen as *mut u32) = 16;
            }
        }
        return 0;
    }
    let sock = match get_sock(fd as i32) {
        Some(s) => s,
        None => return super::ENOTSOCK,
    };
    let max_len = unsafe { *(addrlen as *const u32) } as usize;
    if addr != 0 && max_len >= 2 {
        if !crate::memory::space::validate_user_write_range(addr, 2.min(max_len)) {
            return super::EFAULT;
        }
        let s = sock.lock();
        if s.is_netlink {
            if max_len >= 12 && crate::memory::space::validate_user_write_range(addr, 12) {
                let pid = crate::tasks::current_thread()
                    .map(|t| t.process.lock().pid as u32)
                    .unwrap_or(0);
                unsafe {
                    *(addr as *mut u16) = AF_NETLINK as u16;
                    *(addr.wrapping_add(2) as *mut u16) = 0;
                    *(addr.wrapping_add(4) as *mut u32) = pid;
                    *(addr.wrapping_add(8) as *mut u32) = 1;
                    *(addrlen as *mut u32) = 12;
                }
                return 0;
            }
        }
        match &s.kind {
            SockKind::Listening { path, .. } => {
                let path_bytes = path.as_bytes();
                let need_len = 2 + path_bytes.len() + 1;
                unsafe {
                    *(addr as *mut u16) = AF_UNIX as u16;
                }
                if max_len > 2 {
                    let copy_len = path_bytes.len().min(max_len - 3);
                    if crate::memory::space::validate_user_write_range(addr.wrapping_add(2), copy_len + 1) {
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                path_bytes.as_ptr(),
                                addr.wrapping_add(2) as *mut u8,
                                copy_len,
                            );
                            *(addr.wrapping_add((2 + copy_len) as u64) as *mut u8) = 0;
                        }
                    }
                }
                unsafe {
                    *(addrlen as *mut u32) = need_len.min(max_len) as u32;
                }
            }
            _ => {
                unsafe {
                    *(addr as *mut u16) = AF_UNIX as u16;
                    if max_len > 2 && crate::memory::space::validate_user_write_range(addr.wrapping_add(2), 1) {
                        *(addr.wrapping_add(2) as *mut u8) = 0;
                    }
                    *(addrlen as *mut u32) = 2;
                }
            }
        }
    }
    0
}

/// 52: `getpeername(fd, addr, addrlen)`
pub fn sys_getpeername(fd: u64, addr: u64, addrlen: u64) -> u64 {
    if addrlen == 0 || !crate::memory::space::validate_user_write_range(addrlen, 4) {
        return super::EFAULT;
    }
    if let Some(inet) = get_inet(fd as i32) {
        let max_len = unsafe { *(addrlen as *const u32) } as usize;
        if addr != 0 && max_len >= 16 && crate::memory::space::validate_user_write_range(addr, 16) {
            let handle = inet.lock().handle;
            let mut ep_opt = None;
            if let Some(mut guard) = crate::net::SOCKETS.try_lock() {
                if let Some(set) = guard.as_mut() {
                    let sock = set.get_mut::<TcpSocket>(handle);
                    ep_opt = sock.remote_endpoint();
                }
            }
            if let Some(remote) = ep_opt {
                let ip_octets: [u8; 4] = match remote.addr {
                    IpAddress::Ipv4(ip) => ip.0,
                };
                unsafe {
                    *(addr as *mut u16) = AF_INET as u16;
                    *(addr.wrapping_add(2) as *mut u16) = remote.port.to_be();
                    core::ptr::copy_nonoverlapping(ip_octets.as_ptr(), addr.wrapping_add(4) as *mut u8, 4);
                    core::ptr::write_bytes(addr.wrapping_add(8) as *mut u8, 0, 8);
                    *(addrlen as *mut u32) = 16;
                }
                return 0;
            } else {
                return super::ENOTCONN;
            }
        }
        return 0;
    }
    if get_udp(fd as i32).is_some() {
        let max_len = unsafe { *(addrlen as *const u32) } as usize;
        if addr != 0 && max_len >= 16 && crate::memory::space::validate_user_write_range(addr, 16) {
            unsafe {
                *(addr as *mut u16) = AF_INET as u16;
                *(addr.wrapping_add(2) as *mut u16) = 0;
                *(addr.wrapping_add(4) as *mut u32) = 0;
                core::ptr::write_bytes(addr.wrapping_add(8) as *mut u8, 0, 8);
                *(addrlen as *mut u32) = 16;
            }
        }
        return 0;
    }
    let _sock = match get_sock(fd as i32) {
        Some(s) => s,
        None => return super::ENOTSOCK,
    };
    let max_len = unsafe { *(addrlen as *const u32) } as usize;
    if addr != 0 && max_len >= 2 {
        if !crate::memory::space::validate_user_write_range(addr, 2) {
            return super::EFAULT;
        }
        unsafe {
            *(addr as *mut u16) = AF_UNIX as u16;
            if max_len > 2 && crate::memory::space::validate_user_write_range(addr.wrapping_add(2), 1) {
                *(addr.wrapping_add(2) as *mut u8) = 0;
            }
            *(addrlen as *mut u32) = 2;
        }
    }
    0
}
