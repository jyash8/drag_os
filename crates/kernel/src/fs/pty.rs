//! Pseudo-terminal (PTY) pair subsystem for DragOS.
//!
//! Provides master/slave PTY pairs for terminal multiplexers, SSH daemons
//! (Dropbear, OpenSSH), and interactive shell sessions.
//!
//! Master: allocated by opening `/dev/ptmx`.
//! Slave: opened at `/dev/pts/<id>` once unlocked via `TIOCSPTLCK`.

#![allow(unused)]

use alloc::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
};
use crate::arch::sync::spinlock::SpinLock;
use crate::tasks::{block_current_thread, wake_pipe_waiters, PendingBlock};

pub const PTY_BUF_CAPACITY: usize = 16384; // 16 KiB buffer per direction

/// Linux x86_64 struct termios layout (36 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Termios {
    pub c_iflag: u32,
    pub c_oflag: u32,
    pub c_cflag: u32,
    pub c_lflag: u32,
    pub c_line: u8,
    pub c_cc: [u8; 19],
}

impl Default for Termios {
    fn default() -> Self {
        let mut cc = [0u8; 19];
        // Standard ASCII control characters
        cc[0] = 3;   // VINTR = ^C
        cc[1] = 28;  // VQUIT = ^\
        cc[2] = 127; // VERASE = DEL / Backspace
        cc[3] = 21;  // VKILL = ^U
        cc[4] = 4;   // VEOF = ^D
        cc[5] = 0;   // VTIME
        cc[6] = 1;   // VMIN = 1
        cc[7] = 0;   // VSWTC
        cc[8] = 17;  // VSTART = ^Q
        cc[9] = 19;  // VSTOP = ^S
        cc[10] = 26; // VSUSP = ^Z
        cc[11] = 0;  // VEOL
        cc[12] = 18; // VREPRINT = ^R
        cc[13] = 15; // VDISCARD = ^O
        cc[14] = 23; // VWERASE = ^W
        cc[15] = 22; // VLNEXT = ^V
        cc[16] = 0;  // VEOL2

        Self {
            c_iflag: 0o422,       // ICRNL | IXON
            c_oflag: 0o5,         // OPOST | ONLCR
            c_cflag: 0o2261,      // B38400 | CS8 | CREAD | HUPCL
            c_lflag: 0o105073,    // ISIG | ICANON | ECHO | ECHOE | ECHOK | IEXTEN
            c_line: 0,
            c_cc: cc,
        }
    }
}

/// Linux x86_64 struct winsize (8 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct WinSize {
    pub ws_row: u16,
    pub ws_col: u16,
    pub ws_xpixel: u16,
    pub ws_ypixel: u16,
}

impl Default for WinSize {
    fn default() -> Self {
        Self {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }
    }
}

pub struct PtyPair {
    pub id: u32,
    /// Owner/mode of the slave node (grantpt/sshd chown/chmod these;
    /// stat reports them so ttyname's dev/ino+perm checks pass).
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
    pub locked: bool,
    pub packet_mode: bool,
    pub pgrp: i32,
    pub winsize: WinSize,
    pub termios: Termios,

    /// Master writes into `m2s`, slave reads from `m2s`.
    pub m2s: VecDeque<u8>,
    /// Canonical line edit buffer (ICANON mode).
    pub canon_buf: VecDeque<u8>,
    /// Slave writes into `s2m`, master reads from `s2m`.
    pub s2m: VecDeque<u8>,

    /// True after VEOF (^D) pressed on an empty canonical line. The next
    /// slave read returns 0 (EOF) instead of blocking. Cleared when the
    /// EOF is actually returned to the slave.
    pub eof_pending: bool,

    pub master_open: bool,
    pub slave_open: bool,
    /// Open-description counts per side. dup()/fork() share one OpenFile
    /// (one description) so only open()/last-close move these. The bools
    /// above are derived from the counters.
    pub master_count: u32,
    pub slave_count: u32,
}

impl PtyPair {
    pub fn new(id: u32) -> Self {
        Self {
            id,
            uid: 0,
            gid: 0,
            mode: 0o620,
            locked: true, // PTY slaves start locked per Unix98 spec
            packet_mode: false,
            pgrp: 0,
            winsize: WinSize::default(),
            termios: Termios::default(),
            m2s: VecDeque::with_capacity(1024),
            canon_buf: VecDeque::with_capacity(256),
            s2m: VecDeque::with_capacity(1024),
            eof_pending: false,
            master_open: true,
            slave_open: false,
            master_count: 0,
            slave_count: 0,
        }
    }

    /// Store winsize; returns the foreground pgrp to signal. The caller must
    /// send SIGWINCH itself AFTER dropping all locks: sys_kill takes
    /// PROCESS_TABLE → SCHEDULER, and its exit_group path re-takes the
    /// OpenFile/pty locks (non-reentrant) — signaling under them deadlocks.
    pub fn set_winsize(&mut self, ws: WinSize) -> i32 {
        self.winsize = ws;
        self.pgrp
    }

    pub fn master_read(&mut self, buf: &mut [u8], nonblock: bool) -> Result<usize, u64> {
        loop {
            if !self.s2m.is_empty() {
                let n = core::cmp::min(buf.len(), self.s2m.len());
                for b in buf[..n].iter_mut() {
                    *b = self.s2m.pop_front().unwrap();
                }
                wake_pipe_waiters();
                return Ok(n);
            }
            if !self.slave_open {
                // Slave has closed: EOF
                return Ok(0);
            }
            if nonblock {
                return Err(crate::arch::x86_64::syscalls::EAGAIN);
            }
            return Err(crate::arch::x86_64::syscalls::EAGAIN);
        }
    }

    pub fn master_write(&mut self, buf: &[u8], nonblock: bool) -> Result<usize, u64> {
        loop {
            let space = PTY_BUF_CAPACITY.saturating_sub(self.m2s.len());
            if space > 0 {
                const ICRNL: u32 = 0o400;
                const ISIG: u32 = 0o1;
                const ICANON: u32 = 0o2;
                const ECHO: u32 = 0o10;
                const ECHOE: u32 = 0o20;
                const ECHOK: u32 = 0o40;
                const ONLCR: u32 = 0o4;
                const OPOST: u32 = 0o1;

                let is_canon = self.termios.c_lflag & ICANON != 0;
                let is_isig = self.termios.c_lflag & ISIG != 0;
                let do_echo = self.termios.c_lflag & ECHO != 0;
                let do_echoe = self.termios.c_lflag & ECHOE != 0;
                let do_echok = self.termios.c_lflag & ECHOK != 0;
                let out_nl = self.termios.c_oflag & (OPOST | ONLCR) == (OPOST | ONLCR);

                let v_intr = self.termios.c_cc[0];
                let v_quit = self.termios.c_cc[1];
                let v_erase = self.termios.c_cc[2];
                let v_kill = self.termios.c_cc[3];
                let v_eof = self.termios.c_cc[4];
                let v_susp = self.termios.c_cc[10];

                let to_write = core::cmp::min(buf.len(), space);
                let mut committed = false;

                for &b in &buf[..to_write] {
                    // 1. Check signal characters (ISIG)
                    if is_isig {
                        if b == v_intr && v_intr != 0 {
                            if self.pgrp > 0 {
                                crate::arch::x86_64::syscalls::sys_kill(self.pgrp as i64, 2); // SIGINT
                            }
                            continue;
                        }
                        if b == v_quit && v_quit != 0 {
                            if self.pgrp > 0 {
                                crate::arch::x86_64::syscalls::sys_kill(self.pgrp as i64, 3); // SIGQUIT
                            }
                            continue;
                        }
                        if b == v_susp && v_susp != 0 {
                            if self.pgrp > 0 {
                                crate::arch::x86_64::syscalls::sys_kill(self.pgrp as i64, 20); // SIGTSTP
                            }
                            continue;
                        }
                    }

                    // 2. Input mapping: ICRNL
                    let mut c = b;
                    if self.termios.c_iflag & ICRNL != 0 && c == b'\r' {
                        c = b'\n';
                    }

                    // 3. Line discipline: ICANON vs Raw
                    if is_canon {
                        if c == v_erase && v_erase != 0 {
                            if self.canon_buf.pop_back().is_some() && do_echoe {
                                if self.s2m.len() + 3 <= PTY_BUF_CAPACITY {
                                    self.s2m.push_back(b'\x08');
                                    self.s2m.push_back(b' ');
                                    self.s2m.push_back(b'\x08');
                                }
                            }
                        } else if c == v_kill && v_kill != 0 {
                            self.canon_buf.clear();
                            if do_echok && self.s2m.len() + 2 <= PTY_BUF_CAPACITY {
                                self.s2m.push_back(b'\r');
                                self.s2m.push_back(b'\n');
                            }
                        } else if c == b'\n' {
                            self.canon_buf.push_back(b'\n');
                            while let Some(ch) = self.canon_buf.pop_front() {
                                self.m2s.push_back(ch);
                            }
                            committed = true;
                            if do_echo {
                                if out_nl {
                                    if self.s2m.len() + 2 <= PTY_BUF_CAPACITY {
                                        self.s2m.push_back(b'\r');
                                        self.s2m.push_back(b'\n');
                                    }
                                } else if self.s2m.len() < PTY_BUF_CAPACITY {
                                    self.s2m.push_back(b'\n');
                                }
                            }
                        } else if c == v_eof && v_eof != 0 {
                            if self.canon_buf.is_empty() {
                                self.eof_pending = true;
                            } else {
                                while let Some(ch) = self.canon_buf.pop_front() {
                                    self.m2s.push_back(ch);
                                }
                                committed = true;
                            }
                        } else {
                            if self.canon_buf.len() < PTY_BUF_CAPACITY {
                                self.canon_buf.push_back(c);
                            }
                            if do_echo && self.s2m.len() < PTY_BUF_CAPACITY {
                                self.s2m.push_back(c);
                            }
                        }
                    } else {
                        // Raw mode
                        self.m2s.push_back(c);
                        committed = true;
                        if do_echo {
                            if c == b'\n' && out_nl {
                                if self.s2m.len() + 2 <= PTY_BUF_CAPACITY {
                                    self.s2m.push_back(b'\r');
                                    self.s2m.push_back(b'\n');
                                }
                            } else if self.s2m.len() < PTY_BUF_CAPACITY {
                                self.s2m.push_back(c);
                            }
                        }
                    }
                }

                if committed {
                    wake_pipe_waiters();
                }
                return Ok(to_write);
            }
            if nonblock {
                return Err(crate::arch::x86_64::syscalls::EAGAIN);
            }
            return Err(crate::arch::x86_64::syscalls::EAGAIN);
        }
    }

    pub fn slave_read(&mut self, buf: &mut [u8], nonblock: bool) -> Result<usize, u64> {
        loop {
            if !self.m2s.is_empty() {
                let n = core::cmp::min(buf.len(), self.m2s.len());
                for b in buf[..n].iter_mut() {
                    *b = self.m2s.pop_front().unwrap();
                }
                wake_pipe_waiters();
                return Ok(n);
            }
            // VEOF (Ctrl+D) on empty line: signal EOF to reader
            if self.eof_pending {
                self.eof_pending = false;
                return Ok(0);
            }
            if !self.master_open {
                // Master has closed: EOF
                return Ok(0);
            }
            if nonblock {
                return Err(crate::arch::x86_64::syscalls::EAGAIN);
            }
            return Err(crate::arch::x86_64::syscalls::EAGAIN);
        }
    }

    pub fn slave_write(&mut self, buf: &[u8], nonblock: bool) -> Result<usize, u64> {
        loop {
            if !self.master_open {
                return Err(crate::arch::x86_64::syscalls::EPIPE);
            }
            let space = PTY_BUF_CAPACITY.saturating_sub(self.s2m.len());
            if space > 0 {
                // ponytail: OPOST|ONLCR for shell output (matches echo path).
                const ONLCR: u32 = 0o4;
                const OPOST: u32 = 0o1;
                let out_nl = self.termios.c_oflag & (OPOST | ONLCR) == (OPOST | ONLCR);
                let to_write = core::cmp::min(buf.len(), space);
                for &b in &buf[..to_write] {
                    if b == b'\n' && out_nl {
                        if self.s2m.len() + 1 >= PTY_BUF_CAPACITY {
                            break;
                        }
                        self.s2m.push_back(b'\r');
                        self.s2m.push_back(b'\n');
                    } else if self.s2m.len() < PTY_BUF_CAPACITY {
                        self.s2m.push_back(b);
                    } else {
                        break;
                    }
                }
                wake_pipe_waiters();
                return Ok(to_write);
            }
            if nonblock {
                return Err(crate::arch::x86_64::syscalls::EAGAIN);
            }
            return Err(crate::arch::x86_64::syscalls::EAGAIN);
        }
    }
}

pub static PTY_TABLE: SpinLock<BTreeMap<u32, Arc<SpinLock<PtyPair>>>> =
    SpinLock::new(BTreeMap::new());

/// Allocate a new PTY pair and return `(id, Arc<SpinLock<PtyPair>>)`.
pub fn alloc_pty() -> (u32, Arc<SpinLock<PtyPair>>) {
    let mut table = PTY_TABLE.lock();
    let mut id = 0u32;
    while table.contains_key(&id) {
        id += 1;
    }
    let pair = Arc::new(SpinLock::new(PtyPair::new(id)));
    table.insert(id, pair.clone());
    (id, pair)
}

pub fn get_pty(id: u32) -> Option<Arc<SpinLock<PtyPair>>> {
    PTY_TABLE.lock().get(&id).cloned()
}

// ── The POSIX pty API (`posix_openpt` and friends) ──────────────────────────
//
// These are the entry points a program reaches through glibc's `openpty(3)`,
// which is how zellij obtains a pty for each pane. `/dev/ptmx` and the
// `TIOCGPTN`/`TIOCSPTLCK` ioctl pair are the older interface; the POSIX calls
// are not implemented in terms of them here, they are implemented directly,
// because the syscall numbers (622/624/625/626) must be dispatched at all.
//
// Previously they were absent, so `openpty` failed with ENOSYS and zellij
// stalled after "session initialized, spawning tabs" with no panes and no
// diagnostic. `ptyrun` kept working because it opens `/dev/ptmx` directly,
// which is exactly why PTYs looked healthy while zellij could not get one.

/// Install a freshly allocated master into the process's fd table and return
/// the fd, mirroring what `open_path` does for `/dev/ptmx`.
pub fn register_master_fd(id: u32, flags: u32, pty: Arc<SpinLock<PtyPair>>) -> u64 {
    let vnode = crate::fs::vfs::VNode::PtyMaster(pty.clone());
    let arc_vnode = Arc::new(SpinLock::new(crate::fs::vfs::OpenFile {
        vnode,
        flags: flags as u64,
        path: alloc::format!("/dev/ptmx ({})", id),
    }));
    pty_acquire(&pty, true);

    let thread = match crate::tasks::current_thread() {
        Some(t) => t,
        None => return crate::arch::x86_64::syscalls::EBADF,
    };
    let mut p = thread.process.lock();
    for (i, slot) in p.fd_table.iter_mut().enumerate() {
        if slot.is_none() {
            *slot = Some(arc_vnode);
            return i as u64;
        }
    }
    p.fd_table.push(Some(arc_vnode));
    (p.fd_table.len() - 1) as u64
}

/// The pty id behind an open master fd, or `None` if it is not one.
pub fn master_id(fd: u32) -> Option<u32> {
    let proc = crate::tasks::current_process()?;
    let p = proc.lock();
    let f = p.fd_table.get(fd as usize)?.as_ref()?;
    let g = f.lock();
    match &g.vnode {
        crate::fs::vfs::VNode::PtyMaster(pty) => Some(pty.lock().id),
        _ => None,
    }
}

/// Clear the lock on the pair behind `fd` so `/dev/pts/<n>` can be opened.
pub fn unlock_master(fd: u32) -> u64 {
    let proc = match crate::tasks::current_process() {
        Some(p) => p,
        None => return crate::arch::x86_64::syscalls::EBADF,
    };
    let vnode_arc = {
        let p = proc.lock();
        match p.fd_table.get(fd as usize) {
            Some(Some(f)) => f.clone(),
            _ => return crate::arch::x86_64::syscalls::EBADF,
        }
    };
    let g = vnode_arc.lock();
    match &g.vnode {
        crate::fs::vfs::VNode::PtyMaster(pty) => {
            pty.lock().locked = false;
            0
        }
        _ => crate::arch::x86_64::syscalls::ENOTTY,
    }
}

pub fn remove_pty(id: u32) {
    PTY_TABLE.lock().remove(&id);
}

/// A new open description for one side of `pair` (from open(), never from
/// dup()/fork() which share the OpenFile). Call with no locks held.
pub fn pty_acquire(pair: &Arc<SpinLock<PtyPair>>, is_master: bool) {
    let mut p = pair.lock();
    if is_master {
        p.master_count += 1;
        p.master_open = true;
    } else {
        p.slave_count += 1;
        p.slave_open = true;
    }
}

/// The last fd reference to an open description closed (caller verified via
/// `Arc::strong_count`). Clears the side, wakes PipeWait blockers sleeping in
/// pty_master/slave_read, and frees the pair when both sides are done.
/// Call with no locks held: takes pty lock (leaf), then SCHEDULER via the
/// wake (same as the existing pipe-close path), then PTY_TABLE (leaf).
/// Established: tasks.rs exit teardown / fsio.rs sys_close hold no locks here.
pub fn pty_release(pair: &Arc<SpinLock<PtyPair>>, is_master: bool) {
    let (empty, id) = {
        let mut p = pair.lock();
        if is_master {
            p.master_count = p.master_count.saturating_sub(1);
            if p.master_count == 0 {
                p.master_open = false;
            }
        } else {
            p.slave_count = p.slave_count.saturating_sub(1);
            if p.slave_count == 0 {
                p.slave_open = false;
            }
        }
        (p.master_count == 0 && p.slave_count == 0, p.id)
    };
    crate::tasks::wake_pipe_waiters();
    if empty {
        remove_pty(id);
    }
}

/// Read from master channel with thread blocking if empty and not nonblocking.
pub fn pty_master_read(
    pty: &Arc<SpinLock<PtyPair>>,
    buf: &mut [u8],
    nonblock: bool,
) -> Result<usize, u64> {
    loop {
        {
            let mut pair = pty.lock();
            match pair.master_read(buf, nonblock) {
                Ok(n) => return Ok(n),
                Err(e) if e == crate::arch::x86_64::syscalls::EAGAIN && !nonblock => {}
                Err(e) => return Err(e),
            }
        }
        block_current_thread(PendingBlock::PipeWait);
    }
}

/// Write to master channel with thread blocking if full and not nonblocking.
pub fn pty_master_write(
    pty: &Arc<SpinLock<PtyPair>>,
    buf: &[u8],
    nonblock: bool,
) -> Result<usize, u64> {
    loop {
        {
            let mut pair = pty.lock();
            match pair.master_write(buf, nonblock) {
                Ok(n) => return Ok(n),
                Err(e) if e == crate::arch::x86_64::syscalls::EAGAIN && !nonblock => {}
                Err(e) => return Err(e),
            }
        }
        block_current_thread(PendingBlock::PipeWait);
    }
}

/// Read from slave channel with thread blocking if empty and not nonblocking.
pub fn pty_slave_read(
    pty: &Arc<SpinLock<PtyPair>>,
    buf: &mut [u8],
    nonblock: bool,
) -> Result<usize, u64> {
    // Job control on the pty slave: a process in a background process group
    // reading the terminal gets SIGTTIN. tmux and zellij both hold the slave
    // and spawn panes into other groups, so this is the check that stops a
    // backgrounded pane from consuming the terminal's input.
    //
    // `wait_until_foreground` blocks *inside* the check rather than returning
    // to the caller. A plain park here races with SIGCONT: the kernel marks
    // the process stopped and the parent's `kill` sweeps the wait queues, but
    // this thread only reaches its wait queue afterwards, so the sweep finds
    // nothing and the thread sleeps forever with a pending stop.
    if !nonblock {
        wait_until_foreground(pty, crate::arch::x86_64::abi::SIGTTIN);
    }
    loop {
        {
            let mut pair = pty.lock();
            match pair.slave_read(buf, nonblock) {
                Ok(n) => return Ok(n),
                Err(e) if e == crate::arch::x86_64::syscalls::EAGAIN && !nonblock => {}
                Err(e) => return Err(e),
            }
        }
        // A blocking read must be interruptible by a signal.
        //
        // Otherwise the loop parks forever: delivery happens on return to user
        // mode (invariant 8), and a thread asleep here never gets there. So
        // `kill(child, SIGTERM)` on a process blocked reading a pty would never
        // take effect and the parent would wait in waitpid() indefinitely.
        //
        // `check_any_fatal` exits the process for a pending SIG_DFL-fatal
        // signal and returns immediately otherwise, leaving a
        // handler/ignored/blocked signal for the normal tick path.
        crate::tasks::check_any_fatal();
        block_current_thread(PendingBlock::PipeWait);
    }
}

/// `TOSTOP` (c_oflag): stop a background process group that writes the
/// terminal. Off by default, which is why a background write is normally
/// allowed and only `tcsetpgrp` raises SIGTTOU unconditionally.
///
/// The value is Linux's `<asm-generic/termbits.h>` `TOSTOP` = `0x00100`
/// (octal `0400`). It is spelled in hex here to defeat the octal/hex trap that
/// produced `0o100` (`0x40` = `OFILL`) previously: the bit-match against
/// `c_oflag` therefore never fired and a background write under TOSTOP was let
/// through. Clients pass the libc constant verbatim, so the kernel must agree.
const TOSTOP: u32 = 0x00100;

/// Block until this process is in the pty's foreground group, stopping it with
/// `sig` (SIGTTIN for reads, SIGTTOU for writes) the first time it is found in
/// the background.
///
/// The stop is applied *inline* rather than queued as a signal: the thread is
/// inside a syscall, so its saved frame is a kernel frame and `can_deliver`
/// refuses signals to those (invariant 8). A queued SIGTTIN would sit
/// undelivered. See `stop_self_for_tty_io`.
///
/// Parks as `PendingBlock::JobControl` and re-tests on every wake, so a
/// `SIGCONT` that races the park is caught on the next dispatch rather than
/// being lost — see `park_until_stopped_cleared`.
fn wait_until_foreground(pty: &Arc<SpinLock<PtyPair>>, sig: i32) {
    // A process that ignores or blocks the stop signal is not stopped, exactly
    // as in Linux (`tty_check_change` honours the disposition): the background
    // read/write is allowed to proceed. Shells and job-control clients set
    // SIGTTOU to SIG_IGN around tcsetpgrp for precisely this reason.
    if crate::tasks::signal_is_ignored_or_blocked(sig) {
        return;
    }
    let mut stopped_once = false;
    crate::tasks::park_until_stopped_cleared(|| {
        if is_foreground(pty) {
            return true;
        }
        if !stopped_once {
            stopped_once = true;
            crate::tasks::stop_self_for_tty_io(sig);
        }
        false
    });
}

/// Is the calling process in this pty's foreground group?
///
/// A pty with no foreground group recorded is treated as "yes": a lone shell,
/// or one whose owner never called `tcsetpgrp`, must not be stopped on its
/// first read.
fn is_foreground(pty: &Arc<SpinLock<PtyPair>>) -> bool {
    let fg = pty.lock().pgrp;
    if fg <= 0 {
        return true;
    }
    let pgid = match crate::tasks::current_thread() {
        Some(t) => t.process.lock().pgid as i32,
        None => return true,
    };
    fg == pgid
}

/// Write to slave channel with thread blocking if full and not nonblocking.
pub fn pty_slave_write(
    pty: &Arc<SpinLock<PtyPair>>,
    buf: &[u8],
    nonblock: bool,
) -> Result<usize, u64> {
    // Background write => SIGTTOU. The process is stopped and the bytes are
    // dropped, so a background job cannot paint over the foreground pane's
    // screen. Park until foregrounded, exactly as the read path does, so the
    // stop behaves the same way in both directions.
    // SIGTTOU on a background write is gated on TOSTOP, exactly as in Linux.
    //
    // Blocking every background write regardless of TOSTOP deadlocks ordinary
    // shell output: a shell that has backgrounded a job still writes its own
    // prompt and diagnostics, and gating those on the foreground group stops
    // the shell itself the moment it stops caring. Linux only raises SIGTTOU
    // from `write(2)` when the terminal has TOSTOP set; the *unconditional*
    // SIGTTOU applies to `tcsetpgrp` from a background group, which is a
    // different call and is handled in the ioctl path.
    if !nonblock && pty.lock().termios.c_oflag & TOSTOP != 0 {
        wait_until_foreground(pty, crate::arch::x86_64::abi::SIGTTOU);
    }
    loop {
        {
            let mut pair = pty.lock();
            match pair.slave_write(buf, nonblock) {
                Ok(n) => return Ok(n),
                Err(e) if e == crate::arch::x86_64::syscalls::EAGAIN && !nonblock => {}
                Err(e) => return Err(e),
            }
        }
        block_current_thread(PendingBlock::PipeWait);
    }
}

/// Readiness for epoll / poll: `(readable, writable)`.
pub fn pty_master_readiness(pty: &Arc<SpinLock<PtyPair>>) -> (bool, bool) {
    let pair = pty.lock();
    let readable = !pair.s2m.is_empty() || !pair.slave_open;
    let writable = pair.m2s.len() < PTY_BUF_CAPACITY;
    (readable, writable)
}

pub fn pty_slave_readiness(pty: &Arc<SpinLock<PtyPair>>) -> (bool, bool) {
    let pair = pty.lock();
    let readable = !pair.m2s.is_empty() || !pair.master_open;
    let writable = pair.s2m.len() < PTY_BUF_CAPACITY;
    (readable, writable)
}
