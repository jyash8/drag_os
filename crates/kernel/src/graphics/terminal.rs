//! The framebuffer side of the console: it owns the `Display` and renders
//! virtual terminals onto it.
//!
//! All terminal *state* lives in [`crate::graphics::vt`]. This file deliberately
//! holds no cursor, no colour and no escape-sequence state — that used to be
//! here, drawn straight into the framebuffer with the cursor tracked in pixels,
//! which is why there was no way to have two consoles or to switch between
//! them without losing the screen.
//!
//! The split matters for correctness, not tidiness: a VT is a buffer of cells,
//! and the framebuffer is a rendering target that can be rebuilt from scratch
//! at any time. `Ctrl+Alt+F1` and `F2` now work because switching is a number
//! change followed by a repaint.

use core::fmt::{self, Write};

use crate::{
    arch::sync::spinlock::SpinLock,
    graphics::{color::Color, font::FontKind, vt},
};

pub struct Terminal {
    display: Option<crate::graphics::Display>,
    font: FontKind,
    scale: usize,
}

impl Terminal {
    pub const fn new() -> Self {
        Terminal { display: None, font: FontKind::Noto20, scale: 1 }
    }

    pub fn font(&self) -> FontKind {
        self.font
    }

    /// Adopt the framebuffer and size every VT to match it.
    pub fn init(&mut self, display: crate::graphics::Display) {
        let cols = display.width() / (self.font.char_width() * self.scale);
        let rows = display.height() / (self.font.line_height() * self.scale);
        self.display = Some(display);
        vt::init_vts(cols.max(1), rows.max(1));
        self.repaint_all();
    }

    pub fn cols(&self) -> usize {
        vt::with_active(|v| v.cols()).unwrap_or(80)
    }

    pub fn rows(&self) -> usize {
        vt::with_active(|v| v.rows()).unwrap_or(25)
    }

    /// Change font and rebuild every VT's grid to the new metrics.
    pub fn set_font(&mut self, font: FontKind) {
        self.font = font;
        if let Some(d) = self.display.as_ref() {
            let cols = d.width() / (self.font.char_width() * self.scale);
            let rows = d.height() / (self.font.line_height() * self.scale);
            vt::init_vts(cols.max(1), rows.max(1));
        }
        self.repaint_all();
    }

    pub fn cycle_font(&mut self) {
        let next = self.font.next();
        self.set_font(next);
    }

    /// Repaint every VT in full. Used after a VT switch, where the new VT's
    /// cells have never been on this framebuffer.
    pub fn repaint_all(&mut self) {
        if let Some(_d) = self.display.as_mut() {
            if let Some(rows) = vt::with_active(|v| {
                v.mark_all_dirty();
                v.rows()
            }) {
                let _ = rows;
            }
            // Mark every VT dirty, then render the active one.
            let active = vt::active_vt_number();
            for n in 1..=vt::MAX_VTS as u16 {
                if let Some(rows) = vt::with_vt(n, |v| {
                    v.mark_all_dirty();
                    v.rows()
                }) {
                    if n == active {
                        let _ = rows;
                    }
                }
            }
            if let Some(d) = self.display.as_mut() {
                let font = self.font;
                let scale = self.scale;
                vt::with_active(|v| v.render(d, font, scale));
            }
        }
    }

    /// Repaint the active VT's dirty rows. This is the hot path: called after
    /// every write, and cheap because only touched rows are drawn.
    pub fn flush_dirty(&mut self) {
        self.flush();
    }

    /// Repaint a named VT's dirty rows.
    ///
    /// The line discipline echoes into whichever VT owns the tty, which is not
    /// necessarily the one on screen — a process writing to /dev/tty3 must have
    /// its echo land on VT 3 even while VT 1 is displayed.
    pub fn flush_vt(&mut self, n: u16) {
        if let Some(d) = self.display.as_mut() {
            let font = self.font;
            let scale = self.scale;
            vt::with_vt(n, |v| v.render(d, font, scale));
        }
    }

    fn flush(&mut self) {
        if let Some(d) = self.display.as_mut() {
            let font = self.font;
            let scale = self.scale;
            vt::with_active(|v| v.render(d, font, scale));
        }
    }

    // ── Public API used by the rest of the kernel ───────────────────────
    //
    // These keep the names callers already use. `draw_cursor`/`erase_cursor`
    // used to implement a blink by painting and un-painting a block; with a
    // cell buffer the cursor is drawn as part of a row repaint, so they now
    // simply repaint the cursor's row. The blink is gone — deliberately, rather
    // than faked by painting over a row that also holds text.

    pub fn draw_cursor(&mut self) {
        self.flush();
    }

    pub fn erase_cursor(&mut self) {
        self.flush();
    }

    pub fn backspace(&mut self) {
        vt::with_active(|v| {
            v.backspace();
        });
        self.flush();
    }

    /// Clear the active VT. Note this is a *per-VT* clear now: it no longer
    /// wipes the framebuffer, so another console keeps its contents.
    pub fn clear(&mut self) {
        vt::with_active(|v| v.clear());
        self.flush();
    }

    /// Wipe every VT. Used when the display is handed over wholesale.
    pub fn clear_all(&mut self) {
        for n in 1..=vt::MAX_VTS as u16 {
            vt::with_vt(n, |v| v.clear());
        }
        self.flush();
    }
}

impl fmt::Write for Terminal {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        vt::with_active(|v| v.write_str(s));
        self.flush();
        Ok(())
    }
}

// ── Global writer ───────────────────────────────────────────────────────────

pub static WRITER: SpinLock<Terminal> = SpinLock::new(Terminal::new());

#[doc(hidden)]
pub fn _print(args: core::fmt::Arguments) {
    if crate::arch::x86_64::syscalls::fsio::CONSOLE_KD_MODE.load(core::sync::atomic::Ordering::Relaxed) == 0 {
        WRITER.lock().write_fmt(args).unwrap();
    }
}

#[macro_export]
macro_rules! tprint {
    ($($arg:tt)*) => ($crate::graphics::terminal::_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! tprintln {
    () => ($crate::tprint!("\n"));
    ($($arg:tt)*) => ($crate::tprint!("{}\n", format_args!($($arg)*)));
}

/// Default background, shared with the VT layer so a new VT and the renderer
/// agree on what "blank" looks like.
pub const DEFAULT_BG: Color = Color::new(20, 20, 20);
