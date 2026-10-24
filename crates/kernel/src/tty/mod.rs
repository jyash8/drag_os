//! Per-VT input queues and the `N_TTY` line discipline.
//!
//! # What was wrong before
//!
//! VTs had independent *output* (each owns a cell buffer) but shared one
//! global input queue: `interrupts::STDIN_BUFFER`, a single `VecDeque<u8>` that
//! every `read(2)` on `/dev/tty`, `/dev/ttyN` and fd 0 drained. So typing on
//! VT 2 while VT 1 was displayed put the characters in the one queue, and
//! whichever reader got there first consumed them. Worse, there was no line
//! discipline at all: no canonical editing, no echo, no signal characters, and
//! termios ioctls on a `TtyDevice` fell through to a `Termios::default()`
//! that was thrown away — `TCSETS` on a VT was accepted and had no effect.
//!
//! # The model
//!
//! Each VT owns a [`Tty`]: a raw input queue, a canonical line buffer, its own
//! termios and winsize. Keystrokes are routed to the *active* VT, which is what
//! makes per-VT input work — VT 2 keeps its half-typed line while VT 1 is on
//! screen, exactly as on Linux.
//!
//! Bytes arriving from the keyboard go through [`Tty::input`], which is the line
//! discipline: it interprets signal characters, does canonical editing, echoes
//! when asked, and appends to whichever buffer `read(2)` will drain.
//!
//! # Locking
//!
//! `TTYS` is a leaf lock in the producer direction: the keyboard ISR takes it,
//! pushes, releases it, and only then takes `SCHEDULER` to wake readers (the
//! asymmetric pattern that makes the producer/park pair deadlock-free — see the
//! data-queue note in `smp.rs`). Nothing takes `SCHEDULER` while holding
//! `TTYS`, so it cannot invert.
//!
//! Echo does reach `vt::VTS`, so the order `TTYS -> VTS` is taken when echo is
//! emitted inline. The reverse never happens: rendering and `Vt::write_str`
//! touch no tty state.

#![allow(unused)]

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;

use crate::arch::sync::spinlock::SpinLock;
use crate::fs::pty::{Termios, WinSize};

/// Sentinel meaning "the reader should see EOF", pushed into the read queue
/// when `VEOF` arrives on an empty line. Kept distinct from a real 0 byte
/// because `read(2)` returning 0 *is* EOF and no data byte may be 0 here.
pub const EOF_SENTINEL: u8 = 0xFF;

// ── termios flag bits ───────────────────────────────────────────────────────
//
// Named rather than written as octal literals at each use site: these are the
// bits clients actually flip, and a bare `& 0o2` in a termios context is
// unreadable.
pub const ICANON: u32 = 0o0000002;
pub const ECHO: u32 = 0o0000010;
pub const ECHOE: u32 = 0o0000020;
pub const ECHOK: u32 = 0o0000040;
pub const ECHONL: u32 = 0o0000100;
pub const ISIG: u32 = 0o0000001;
pub const IEXTEN: u32 = 0o0100000;
pub const ICRNL: u32 = 0o0000400;
pub const IGNCR: u32 = 0o0000200;
pub const INLCR: u32 = 0o0000100;
pub const IXON: u32 = 0o0002000;
pub const IXOFF: u32 = 0o0010000;
pub const OPOST: u32 = 0o0000001;
pub const ONLCR: u32 = 0o0000004;
pub const OCRNL: u32 = 0o0000010;
pub const ONLRET: u32 = 0o0000040;

// c_cc indices, matching the Linux struct termios layout.
const VINTR: usize = 0;
const VQUIT: usize = 1;
const VERASE: usize = 2;
const VKILL: usize = 3;
const VEOF: usize = 4;
const VTIME: usize = 5;
const VMIN: usize = 6;
const VSTART: usize = 8;
const VSTOP: usize = 9;
const VSUSP: usize = 10;
const VEOL: usize = 11;
const VREPRINT: usize = 12;
const VWERASE: usize = 14;
const VLNEXT: usize = 15;

/// What a line-discipline step asks the caller to do.
///
/// Returned rather than performed inline so that signalling (which reaches
/// `PROCESS_TABLE` and `SCHEDULER`) never happens while a tty lock is held.
/// The ISR caller drops its locks first, exactly as the existing
/// `signal_ctrl_c_from_isr` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Nothing further.
    None,
    /// Deliver `sig` to the VT's foreground process group.
    Signal(i32),
}

pub struct Tty {
    /// Bytes ready for `read(2)`. In canonical mode these are whole lines; in
    /// non-canonical mode they are keystrokes as typed.
    pub queue: VecDeque<u8>,
    /// The line currently being typed, in canonical mode. `read(2)` does not
    /// see it until a line terminator arrives — that is what makes ^U and ^W
    /// able to retract it.
    pub line: Vec<u8>,
    pub termios: Termios,
    pub winsize: WinSize,
    /// Foreground process group for `ISIG`. 0 means "the process that opened
    /// this VT", recorded in `open_pgid`. Job control (`tcsetpgrp`) can set it.
    pub fg_pgid: i32,
    /// pid to signal when `fg_pgid` is still 0.
    pub open_pgid: i32,
    /// `IXOFF` output flow control is engaged (`^S` seen).
    pub stopped: bool,
    /// `^V` (VLNEXT) was seen: the next byte is literal.
    pub lnext: bool,
    /// Echo bookkeeping: how many characters of `line` are currently on
    /// screen. Without this, ERASE and KILL cannot know how much to rub out.
    pub echoed: usize,
}

impl Tty {
    pub fn new() -> Self {
        Tty {
            queue: VecDeque::new(),
            line: Vec::new(),
            termios: Termios::default(),
            winsize: WinSize::default(),
            fg_pgid: 0,
            open_pgid: 0,
            stopped: false,
            lnext: false,
            echoed: 0,
        }
    }

    #[inline]
    fn cc(&self, idx: usize) -> u8 {
        self.termios.c_cc.get(idx).copied().unwrap_or(0)
    }

    #[inline]
    fn lflag(&self, bit: u32) -> bool {
        self.termios.c_lflag & bit != 0
    }

    #[inline]
    fn iflag(&self, bit: u32) -> bool {
        self.termios.c_iflag & bit != 0
    }

    /// Is a byte readable right now? Used by the park-time re-check in `smp.rs`
    /// and by epoll readiness, so it must not block.
    #[inline]
    pub fn has_input(&self) -> bool {
        !self.queue.is_empty()
    }

    /// Feed one byte from the keyboard through the line discipline.
    ///
    /// Returns what the caller still has to do (usually nothing). Echo is
    /// written here, under the tty lock, so that a `^U` racing another key
    /// cannot interleave its erasure with the other key's echo.
    pub fn input(&mut self, b: u8, vt: u16) -> Action {
        // VLNEXT escapes the very next byte from all interpretation.
        if self.lnext {
            self.lnext = false;
            if self.lflag(ICANON) {
                self.line.push(b);
                self.echoed += 1;
                self.echo(vt, &[b]);
            } else {
                self.queue.push_back(b);
            }
            return Action::None;
        }

        // Input flow control (IXON): ^S stops, ^Q restarts. Handled before
        // signal chars so a terminal in IXON cannot be frozen by ^C.
        if self.iflag(IXON) {
            if b == self.cc(VSTOP) && self.cc(VSTOP) != 0 {
                self.stopped = true;
                return Action::None;
            }
            if b == self.cc(VSTART) && self.cc(VSTART) != 0 {
                self.stopped = false;
                return Action::None;
            }
            if self.stopped {
                return Action::None;
            }
        }

        // Input flag fixups before anything looks at the byte.
        let mut b = b;
        if self.iflag(ICRNL) && b == b'\r' {
            b = b'\n';
        } else if self.iflag(IGNCR) && b == b'\r' {
            return Action::None; // drop it
        } else if self.iflag(INLCR) && b == b'\n' {
            b = b'\r';
        }

        // Signal characters.
        if self.lflag(ISIG) {
            if b == self.cc(VINTR) && self.cc(VINTR) != 0 {
                self.flush_for_signal(vt);
                return Action::Signal(2); // SIGINT
            }
            if b == self.cc(VQUIT) && self.cc(VQUIT) != 0 {
                self.flush_for_signal(vt);
                return Action::Signal(3); // SIGQUIT
            }
            if b == self.cc(VSUSP) && self.cc(VSUSP) != 0 {
                self.flush_for_signal(vt);
                return Action::Signal(20); // SIGTSTP
            }
        }

        if b == 0 || b == EOF_SENTINEL {
            // 0 cannot cross the tty boundary (read(2) would call it EOF) and
            // the EOF sentinel must not be smuggled in as data.
            return Action::None;
        }

        if !self.lflag(ICANON) {
            // Non-canonical: straight through.
            self.queue.push_back(b);
            return Action::None;
        }

        // ── Canonical mode ───────────────────────────────────────────────
        let erase = self.cc(VERASE);
        if erase != 0 && (b == erase || (erase == 0x7f && b == 0x08)) {
            if self.line.pop().is_some() {
                if self.echoed > 0 {
                    self.echoed -= 1;
                }
                // ECHOE: rub the character out with "backspace, space,
                // backspace" rather than trusting the cursor to move left.
                if self.lflag(ECHOE) {
                    self.echo(vt, b"\x08 \x08");
                }
            }
            return Action::None;
        }

        if self.cc(VKILL) != 0 && b == self.cc(VKILL) {
            if self.lflag(ECHO) && self.echoed > 0 {
                // ECHOK: ^U then a newline resets the column the way a fresh
                // prompt would, which is what the user expects from ^U.
                self.echo(vt, b"\x15\n");
            }
            self.line.clear();
            self.echoed = 0;
            return Action::None;
        }

        if self.cc(VWERASE) != 0 && b == self.cc(VWERASE) {
            // Erase the last whitespace-delimited word.
            while self.line.last() == Some(&b' ') {
                self.line.pop();
            }
            while let Some(&c) = self.line.last() {
                if c == b' ' {
                    break;
                }
                self.line.pop();
            }
            if self.echoed > 0 {
                let n = (self.line.len()).min(self.echoed);
                let mut out = Vec::new();
                for _ in 0..(self.echoed - n) {
                    out.extend_from_slice(b"\x08 \x08");
                }
                self.echoed = n;
                self.echo(vt, &out);
            }
            return Action::None;
        }

        if self.cc(VREPRINT) != 0 && b == self.cc(VREPRINT) {
            if self.lflag(ECHO) {
                let mut out = Vec::new();
                out.push(b'\n');
                out.extend_from_slice(&self.line);
                self.echoed = self.line.len();
                self.echo(vt, &out);
            }
            return Action::None;
        }

        if self.cc(VLNEXT) != 0 && b == self.cc(VLNEXT) {
            self.lnext = true;
            if self.lflag(ECHO) {
                self.echo(vt, b"^V");
                self.echoed += 2;
            }
            return Action::None;
        }

        let eof = self.cc(VEOF);
        if eof != 0 && b == eof {
            if self.line.is_empty() {
                // ^D on an empty line: end of input.
                self.queue.push_back(EOF_SENTINEL);
                if self.lflag(ECHO) {
                    self.echo(vt, b"\n");
                }
                return Action::None;
            }
            // ^D with text pending: the text is delivered without waiting for
            // a newline. This is the documented Linux behaviour and the reason
            // `cat` can be ended mid-line.
            self.deliver_line(vt, false);
            return Action::None;
        }

        let terminator = b == b'\n' || (self.cc(VEOL) != 0 && b == self.cc(VEOL));
        if terminator {
            self.deliver_line(vt, true);
            return Action::None;
        }

        self.line.push(b);
        self.echoed += 1;
        if self.lflag(ECHO) {
            self.echo(vt, &[b]);
        }
        Action::None
    }

    /// End of line: move the pending line into the read queue.
    ///
    /// `append_newline` distinguishes the two ways a line ends. A real
    /// terminator (`\n`, `VEOL`) is *part of* the data the reader gets, so it
    /// is appended. `VEOF` is not a terminator — it submits what is pending and
    /// stops — so nothing is appended.
    ///
    /// Getting this wrong hands the program a byte it never typed. For `cat`,
    /// whose whole job is byte-exact copying, that is a spurious extra line at
    /// the end of its output, and it is invisible to a casual test because the
    /// text still looks right.
    fn deliver_line(&mut self, vt: u16, append_newline: bool) {
        if append_newline {
            let lflag_echo = self.lflag(ECHO);
            if lflag_echo {
                // Echo the terminator. With ONLCR a bare \n would leave the cursor
                // at column 0 of the *same* row, so echo CRLF.
                self.echo(vt, b"\r\n");
            } else if self.lflag(ECHONL) {
                self.echo(vt, b"\n");
            }
        }
        let line = core::mem::take(&mut self.line);
        self.queue.extend(line);
        if append_newline {
            self.queue.push_back(b'\n');
        }
        self.echoed = 0;
    }

    /// A signal character discards pending input, as Linux does: the line
    /// half-typed so far is abandoned rather than delivered to the next reader.
    fn flush_for_signal(&mut self, vt: u16) {
        self.line.clear();
        self.queue.clear();
        self.echoed = 0;
        // Echo the caret notation, then a newline so the next prompt starts
        // clean.
        let mut out = Vec::new();
        out.push(b'^');
        out.push(self.cc(VINTR).wrapping_add(0x40));
        out.extend_from_slice(b"\r\n");
        self.echo(vt, &out);
    }

    /// Write echo bytes to this VT's cell buffer.
    fn echo(&self, vt: u16, bytes: &[u8]) {
        // Escape bytes that would otherwise be eaten by the ANSI parser:
        // echoing a literal ESC or CR must not start a sequence.
        let mut text = String::with_capacity(bytes.len());
        for &b in bytes {
            match b {
                0x1b => text.push_str("\x1b\\"),
                b'\r' => text.push('\r'),
                b'\n' => text.push('\n'),
                0x08 => text.push('\x08'),
                _ => {
                    // Every remaining echo byte is printable ASCII or a control
                    // we deliberately pass through; anything else is dropped
                    // rather than turned into a replacement char, because a
                    // NUL or a high byte would corrupt the cell buffer.
                    if (0x20..0x7f).contains(&b) {
                        text.push(b as char);
                    }
                }
            }
        }
        if text.is_empty() {
            return;
        }
        crate::graphics::vt::with_vt(vt, |v| v.write_str(&text));
        // Repaint *that* VT, not the active one: echo belongs to the tty's own
        // VT, which need not be the one currently on screen.
        crate::graphics::terminal::WRITER.lock().flush_vt(vt);
    }

    /// Drain readable bytes into `buf`.
    ///
    /// The three-way result matters: `read(2)` must return 0 for EOF, but a
    /// bare `usize` cannot distinguish "0 bytes, and that is EOF" from "0
    /// bytes, and more may come". The second case is the caller's cue to block.
    pub fn read(&mut self, buf: &mut [u8]) -> ReadOutcome {
        let mut n = 0;
        let mut saw_eof = false;
        while n < buf.len() {
            match self.queue.pop_front() {
                Some(EOF_SENTINEL) => {
                    saw_eof = true;
                    break;
                }
                Some(b) => {
                    buf[n] = b;
                    n += 1;
                }
                None => break,
            }
        }
        if n > 0 {
            // Data wins over a pending EOF: return the bytes now and let the
            // next read report EOF. Draining them together would be wrong for
            // a program that reads a line at a time.
            ReadOutcome::Data(n)
        } else if saw_eof {
            ReadOutcome::Eof
        } else {
            ReadOutcome::Empty
        }
    }

    /// Minimum bytes a non-canonical read must gather before returning
    /// (`VMIN`), or 0 if it may return whatever is present.
    #[inline]
    pub fn vmin(&self) -> usize {
        if self.lflag(ICANON) {
            // Canonical reads are line-at-a-time; a partial line is not
            // readable at all, so the effective minimum is "one whole line",
            // which the queue's emptiness already expresses.
            0
        } else {
            self.cc(VMIN) as usize
        }
    }

    //// Interbyte timeout for a non-canonical read, in deciseconds (`VTIME`).
    #[inline]
    pub fn vtime(&self) -> usize {
        if self.lflag(ICANON) {
            0
        } else {
            self.cc(VTIME) as usize
        }
    }

    // ── Self-check ──────────────────────────────────────────────────────
    //
    // The line discipline is almost entirely invisible from the outside: a
    // broken canonical buffer looks like a shell that "eats" the first
    // character, and a broken signal char looks like ^C doing nothing. Both are
    // easy to miss and hard to attribute, so the invariants are checked at boot
    // and logged on every run.

    /// Exercise the invariants, returning the first one that does not hold.
    ///
    /// Uses a scratch VT so it cannot disturb the console.
    pub fn selftest() -> Result<(), &'static str> {
        let scratch = (crate::graphics::vt::MAX_VTS as u16).min(6);
        if scratch == 0 {
            return Err("no vt available for selftest");
        }
        let mut ttys = TTYS.lock();
        let Some(t) = ttys.get_mut(scratch as usize - 1) else {
            return Err("tty array smaller than MAX_VTS");
        };
        // Leave the scratch VT as we found it.
        let saved_termios = t.termios;
        *t = Tty::new();

        let mut out = [0u8; 64];

        // 1. Canonical mode withholds a partial line until the terminator.
        t.termios.c_lflag |= ICANON;
        assert_eq!(t.input(b'a', scratch), Action::None);
        assert_eq!(t.input(b'b', scratch), Action::None);
        assert!(
            matches!(t.read(&mut out), ReadOutcome::Empty),
            "canonical: partial line must not be readable"
        );
        t.input(b'\n', scratch);
        match t.read(&mut out) {
            ReadOutcome::Data(n) => {
                assert_eq!(&out[..n], b"ab\n", "canonical: line must read back verbatim");
            }
            other => return Err("canonical: expected data after newline"),
        }

        // 2. VERASE retracts the last character rather than submitting it.
        t.input(b'x', scratch);
        t.input(b'y', scratch);
        t.input(t.termios.c_cc[VERASE], scratch);
        t.input(b'\n', scratch);
        match t.read(&mut out) {
            ReadOutcome::Data(n) => assert_eq!(&out[..n], b"x\n", "erase: wrong line"),
            _ => return Err("erase: expected data"),
        }

        // 3. VKILL discards the whole line.
        t.input(b'g', scratch);
        t.input(b'a', scratch);
        t.input(b'r', scratch);
        t.input(t.termios.c_cc[VKILL], scratch);
        t.input(b'\n', scratch);
        match t.read(&mut out) {
            ReadOutcome::Data(n) => assert_eq!(&out[..n], b"\n", "kill: line not discarded"),
            _ => return Err("kill: expected data"),
        }

        // 4. VINTR raises SIGINT and discards pending input.
        t.input(b'z', scratch);
        assert_eq!(
            t.input(t.termios.c_cc[VINTR], scratch),
            Action::Signal(2),
            "isig: ^C must request SIGINT"
        );
        assert!(
            matches!(t.read(&mut out), ReadOutcome::Empty),
            "isig: pending input must be flushed"
        );

        // 5. VEOF on an empty line reports EOF.
        t.input(t.termios.c_cc[VEOF], scratch);
        match t.read(&mut out) {
            ReadOutcome::Eof => {}
            _ => return Err("eof: expected Eof"),
        }
        // ...and EOF must not be sticky: it is consumed by the read that saw it.
        assert!(
            matches!(t.read(&mut out), ReadOutcome::Empty),
            "eof: must not repeat after being consumed"
        );

        // 6. VEOF with a partial line delivers that line without a newline.
        t.input(b'o', scratch);
        t.input(b'k', scratch);
        t.input(t.termios.c_cc[VEOF], scratch);
        match t.read(&mut out) {
            ReadOutcome::Data(n) => assert_eq!(&out[..n], b"ok", "eof-partial: wrong data"),
            _ => return Err("eof-partial: expected data"),
        }

        // 7. Non-canonical mode passes keystrokes straight through.
        t.termios.c_lflag &= !ICANON;
        t.input(b'p', scratch);
        t.input(b'q', scratch);
        match t.read(&mut out) {
            ReadOutcome::Data(n) => assert_eq!(&out[..n], b"pq", "raw: wrong data"),
            _ => return Err("raw: expected data"),
        }

        // 8. A NUL byte must not reach the reader, because read(2) would
        //    report it as EOF.
        t.input(b'w', scratch);
        t.input(0, scratch);
        t.input(b'x', scratch);
        match t.read(&mut out) {
            ReadOutcome::Data(n) => assert_eq!(&out[..n], b"wx", "nul: leaked into the queue"),
            _ => return Err("nul: expected data"),
        }

        // 9. IXON: ^S stops input, ^Q resumes it.
        t.termios.c_lflag |= ICANON;
        t.termios.c_iflag |= IXON;
        t.input(t.termios.c_cc[VSTOP], scratch);
        t.input(b'a', scratch);
        assert!(
            matches!(t.read(&mut out), ReadOutcome::Empty),
            "ixon: ^S must stop input"
        );
        t.input(t.termios.c_cc[VSTART], scratch);
        t.input(b'a', scratch);
        t.input(b'\n', scratch);
        match t.read(&mut out) {
            ReadOutcome::Data(n) => assert_eq!(&out[..n], b"a\n", "ixon: ^Q did not resume"),
            _ => return Err("ixon: expected data"),
        }

        // 10. Restore, then drop the borrow so the isolation check below can
        //     take the lock itself.
        t.termios.c_iflag &= !IXON;
        ttys.get_mut(scratch as usize - 1).unwrap().termios = saved_termios;
        ttys.get_mut(scratch as usize - 1).unwrap().queue.clear();
        ttys.get_mut(scratch as usize - 1).unwrap().line.clear();
        drop(ttys);

        // 11. Per-VT isolation is the whole point of this layer: a byte fed to
        //     one VT must be invisible to another. Checked outside the borrow
        //     above because it needs the array lock itself.
        let other = scratch - 1;
        {
            let mut g = TTYS.lock();
            if let Some(o) = g.get_mut(other as usize - 1) {
                o.queue.clear();
                o.line.clear();
            }
        }
        // The VT is back in canonical mode (its termios was restored above), so a bare
        // byte stays in the line buffer and is not readable until a terminator
        // arrives. Feeding just 'L' and expecting to read it back would be
        // testing the wrong thing -- and would fail for correct behaviour.
        if crate::tty::input_byte(scratch, b'L') != Action::None {
            return Err("isolation: unexpected signal from a plain char");
        }
        if crate::tty::input_byte(scratch, b'\n') != Action::None {
            return Err("isolation: unexpected signal from a newline");
        }
        let mut probe = [0u8; 8];
        if !matches!(crate::tty::read(other, &mut probe), ReadOutcome::Empty) {
            return Err("isolation: another VT saw this VT's input");
        }
        // The bytes must be readable from their own VT, or the check above is
        // vacuous because nothing was queued anywhere.
        match crate::tty::read(scratch, &mut probe) {
            ReadOutcome::Data(n) if &probe[..n] == b"L\n" => {}
            ReadOutcome::Data(n) => return Err("isolation: wrong data in the owning VT"),
            _ => return Err("isolation: byte did not land in its own VT"),
        }
        // Drain the line buffer the echo test left behind.
        let _ = crate::tty::read(scratch, &mut probe);
        Ok(())
    }
}

impl Default for Tty {
    fn default() -> Self {
        Self::new()
    }
}

// ── The per-VT array ────────────────────────────────────────────────────────

/// One [`Tty`] per VT, indexed by VT number - 1.
pub static TTYS: SpinLock<Vec<Tty>> = SpinLock::new(Vec::new());

/// Size the array to match `MAX_VTS`. Called with the VT array at boot.
pub fn init_ttys() {
    let n = crate::graphics::vt::MAX_VTS;
    let mut ttys = TTYS.lock();
    if ttys.len() != n {
        ttys.clear();
        for _ in 0..n {
            ttys.push(Tty::new());
        }
    }
}

#[inline]
fn idx(vt: u16) -> Option<usize> {
    if vt == 0 {
        Some(crate::graphics::vt::active_vt_number().max(1) as usize - 1)
    } else if (1..=crate::graphics::vt::MAX_VTS as u16).contains(&vt) {
        Some(vt as usize - 1)
    } else {
        None
    }
}

/// True once [`init_ttys`] has run.
pub fn ready() -> bool {
    !TTYS.lock().is_empty()
}

/// Route one keystroke to `vt`, returning any signal the caller must deliver.
///
/// The keyboard ISR calls this. It performs the line discipline and the echo;
/// the returned `Action` is acted on *after* the tty lock is released, so no
/// signal path runs under `TTYS`.
pub fn input_byte(vt: u16, b: u8) -> Action {
    let mut ttys = TTYS.lock();
    let Some(i) = idx(vt) else {
        return Action::None;
    };
    let Some(tty) = ttys.get_mut(i) else {
        return Action::None;
    };
    tty.input(b, vt)
}

/// Feed a byte to the active VT. Used by the keyboard and serial ISRs.
pub fn input_active(b: u8) -> Action {
    let vt = crate::graphics::vt::active_vt_number();
    input_byte(vt, b)
}

/// Perform the follow-up a line-discipline step asked for. Separate from
/// [`input_byte`] so the caller can drop every lock first.
pub fn dispatch(action: Action, vt: u16) {
    let Action::Signal(sig) = action else { return };
    let pgid = TTYS
        .lock()
        .get(idx(vt).unwrap_or(0))
        .map(|t| {
            if t.fg_pgid > 0 {
                t.fg_pgid
            } else {
                t.open_pgid
            }
        })
        .unwrap_or(0);
    if pgid > 0 {
        crate::arch::x86_64::syscalls::proc::sys_kill(pgid as i64, sig);
    }
    // Readers blocked in read(2) must re-check, since the flush may have
    // removed their data and the signal may have killed them.
    crate::tasks::wake_keyboard_waiters();
}

/// Module-level entry point for [`Tty::selftest`], so boot code can call
/// `tty::selftest()` without naming the type.
pub fn selftest() -> Result<(), &'static str> {
    if !ready() {
        return Err("tty array not initialised");
    }
    Tty::selftest()
}

/// Outcome of a read attempt, distinguishing EOF from "nothing yet".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOutcome {
    Data(usize),
    Eof,
    Empty,
}

/// Read from VT `vt` into `buf`.
pub fn read(vt: u16, buf: &mut [u8]) -> ReadOutcome {
    let mut ttys = TTYS.lock();
    match idx(vt).and_then(|i| ttys.get_mut(i)) {
        Some(tty) => tty.read(buf),
        None => ReadOutcome::Empty,
    }
}

/// Non-blocking readiness, for epoll and for the park-time re-check.
pub fn has_input(vt: u16) -> bool {
    // try_lock, never block. This is called from the park path while holding
    // SCHEDULER, and the keyboard ISR takes TTYS before SCHEDULER — blocking
    // here would invert that order and deadlock. A failed try_lock reports "no
    // data", which is the safe answer: the holder is mid-push and its wake-up
    // will find whoever is parking already in the wait queue.
    match TTYS.try_lock() {
        Some(mut g) => idx(vt).and_then(|i| g.get(i)).map(|t| t.has_input()).unwrap_or(false),
        None => false,
    }
}

/// Record the pid that opened this VT, so `ISIG` has someone to signal before
/// any `tcsetpgrp`.
pub fn set_open_pgid(vt: u16, pid: i32) {
    let mut ttys = TTYS.lock();
    if let Some(t) = idx(vt).and_then(|i| ttys.get_mut(i)) {
        if t.open_pgid == 0 {
            t.open_pgid = pid;
        }
    }
}

pub fn set_fg_pgid(vt: u16, pgid: i32) {
    let mut ttys = TTYS.lock();
    if let Some(t) = idx(vt).and_then(|i| ttys.get_mut(i)) {
        t.fg_pgid = pgid;
    }
}

pub fn get_fg_pgid(vt: u16) -> i32 {
    let mut ttys = TTYS.lock();
    idx(vt)
        .and_then(|i| ttys.get(i))
        .map(|t| {
            if t.fg_pgid > 0 {
                t.fg_pgid
            } else {
                t.open_pgid
            }
        })
        .unwrap_or(0)
}

pub fn termios(vt: u16) -> Termios {
    let mut ttys = TTYS.lock();
    idx(vt)
        .and_then(|i| ttys.get(i))
        .map(|t| t.termios)
        .unwrap_or_default()
}

pub fn set_termios(vt: u16, t: Termios) {
    let mut ttys = TTYS.lock();
    if let Some(tt) = idx(vt).and_then(|i| ttys.get_mut(i)) {
        // Turning off canonical mode must not strand a half-typed line in a
        // buffer the reader will never look at; hand it over immediately.
        if t.c_lflag & ICANON == 0 && !tt.line.is_empty() {
            let line = core::mem::take(&mut tt.line);
            tt.queue.extend(line);
            tt.queue.push_back(b'\n');
            tt.echoed = 0;
        }
        tt.termios = t;
    }
}

pub fn winsize(vt: u16) -> WinSize {
    let mut ttys = TTYS.lock();
    idx(vt)
        .and_then(|i| ttys.get(i))
        .map(|t| t.winsize)
        .unwrap_or_default()
}

pub fn set_winsize(vt: u16, ws: WinSize) {
    let mut ttys = TTYS.lock();
    if let Some(t) = idx(vt).and_then(|i| ttys.get_mut(i)) {
        t.winsize = ws;
    }
}

/// Apply output post-processing (`OPOST`) to bytes on their way to the VT.
///
/// Kept here rather than in `Vt::write_str` because these are termios bits:
/// they belong to the tty the descriptor names, and two VTs can legitimately
/// disagree about whether a lone `\n` becomes `\r\n`.
pub fn output_postprocess(vt: u16, buf: &[u8]) -> Vec<u8> {
    let t = termios(vt);
    if t.c_oflag & OPOST == 0 {
        return buf.to_vec();
    }
    let mut out = Vec::with_capacity(buf.len() + 8);
    for &b in buf {
        if b == b'\n' && t.c_oflag & ONLCR != 0 {
            out.push(b'\r');
            out.push(b'\n');
        } else if b == b'\r' && t.c_oflag & OCRNL != 0 {
            out.push(b'\n');
        } else if b == b'\n' && t.c_oflag & ONLRET != 0 {
            out.push(b'\r');
        } else {
            out.push(b);
        }
    }
    out
}
