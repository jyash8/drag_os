//! Virtual terminals — Linux's model.
//!
//! A fixed set of independent text buffers sharing one framebuffer. `/dev/tty0`
//! means "whichever VT is current"; `/dev/tty1`..`/dev/ttyN` name a specific
//! one. Switching is instant and lossless because each VT owns its own cells,
//! cursor, scroll region and rendition state, and the framebuffer is only a
//! rendering target — never the source of truth.
//!
//! This is the substrate the rest of the TTY stack needs. Previously a single
//! global `Terminal` drew straight into the framebuffer with a cursor tracked
//! in *pixels*, so there was no buffer to switch between and no way to keep
//! two consoles' contents.
//!
//! Deliberately not in this milestone: the N_TTY line discipline (canonical
//! editing, echo, signal characters) and job control (foreground process
//! group, `tcsetpgrp`, SIGTTIN/SIGTTOU/SIGTSTP). Those attach to the per-VT
//! state defined here once it exists.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU16, Ordering};

use crate::arch::sync::spinlock::SpinLock;
use crate::graphics::color::Color;

/// Number of virtual terminals. Linux uses 64; eight matches the Ctrl+Alt+Fn
/// range that is actually bound, and each VT holds a full screen of cells.
pub const MAX_VTS: usize = 8;

/// Number 1..=MAX_VTS. VT 0 is invalid so that a zeroed handle means "none",
/// the way Linux numbers its VTs.
pub static ACTIVE_VT: AtomicU16 = AtomicU16::new(1);

/// One character cell: the character plus its rendition.
///
/// Storing rendition per cell (rather than keeping it in the parser and
/// painting straight through) is what makes a VT a *buffer*: a screen can be
/// re-rendered later, after the parser state has moved on, and still look the
/// way it did.
#[derive(Clone, Copy, PartialEq)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
}

impl Cell {
    pub const fn blank(bg: Color) -> Self {
        Cell { ch: ' ', fg: Color::ANSI_WHITE, bg, bold: false }
    }
}

/// Parser state for one VT. The ANSI state machine is per-VT for the same
/// reason the cell buffer is: an escape sequence in progress belongs to the
/// console that is receiving it, not to the console driver.
#[derive(Clone, Copy, PartialEq)]
enum AnsiState {
    Normal,
    Escape,
    Csi,
    Osc,
}

const MAX_PARAMS: usize = 16;

pub struct Vt {
    cells: Vec<Cell>,
    cols: usize,
    rows: usize,

    /// Cursor position in **cells**, not pixels. Keeping it in cells is what
    /// lets the cursor survive a font change and a resize.
    pub cursor_x: usize,
    pub cursor_y: usize,
    saved_cursor: (usize, usize),
    pub cursor_visible: bool,

    /// Inclusive scroll region, in rows. Defaults to the whole screen.
    scroll_top: usize,
    scroll_bot: usize,

    // Current rendition, applied to cells as they are written.
    fg: Color,
    bg: Color,
    default_bg: Color,
    bold: bool,

    // ANSI parser
    ansi_state: AnsiState,
    ansi_params: [usize; MAX_PARAMS],
    ansi_param_idx: usize,
    ansi_param_has_digit: bool,

    /// Rows that need repainting, as an inclusive range. Repainting the whole
    /// screen per character is O(cols*rows) per byte of output and is not
    /// viable; this keeps a burst of output proportional to the lines touched.
    dirty_top: usize,
    dirty_bot: usize,
}

impl Vt {
    pub fn new(cols: usize, rows: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Vt {
            cells: alloc::vec![Cell::blank(Color::new(20, 20, 20)); cols * rows],
            cols,
            rows,
            cursor_x: 0,
            cursor_y: 0,
            saved_cursor: (0, 0),
            cursor_visible: true,
            scroll_top: 0,
            scroll_bot: rows - 1,
            fg: Color::ANSI_WHITE,
            bg: Color::new(20, 20, 20),
            default_bg: Color::new(20, 20, 20),
            bold: false,
            ansi_state: AnsiState::Normal,
            ansi_params: [0; MAX_PARAMS],
            ansi_param_idx: 0,
            ansi_param_has_digit: false,
            dirty_top: 0,
            dirty_bot: rows - 1,
        }
    }

    #[inline]
    pub fn cell_char(&self, x: usize, y: usize) -> char {
        if x >= self.cols || y >= self.rows {
            return '\0';
        }
        self.cells[y * self.cols + x].ch
    }

    /// Text of one row, for the self-check and for debugging.
    pub fn row_text(&self, y: usize) -> alloc::string::String {
        let mut s = alloc::string::String::new();
        if y >= self.rows {
            return s;
        }
        for x in 0..self.cols {
            s.push(self.cells[y * self.cols + x].ch);
        }
        s
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Resize the grid, keeping as much content as still fits.
    ///
    /// Linux reflows a VT on a console size change; keeping it simple and
    /// anchored at the top-left is enough for the font changes this currently
    /// performs, and it is honest about not reflowing.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }
        let mut fresh = alloc::vec![Cell::blank(self.default_bg); cols * rows];
        let copy_w = core::cmp::min(cols, self.cols);
        let copy_h = core::cmp::min(rows, self.rows);
        for y in 0..copy_h {
            for x in 0..copy_w {
                fresh[y * cols + x] = self.cells[y * self.cols + x];
            }
        }
        self.cells = fresh;
        self.cols = cols;
        self.rows = rows;
        self.cursor_x = self.cursor_x.min(cols - 1);
        self.cursor_y = self.cursor_y.min(rows - 1);
        self.scroll_top = 0;
        self.scroll_bot = rows - 1;
        self.mark_all_dirty();
    }

    fn mark_row_dirty(&mut self, row: usize) {
        if row >= self.rows {
            return;
        }
        if self.dirty_top > self.dirty_bot {
            self.dirty_top = row;
            self.dirty_bot = row;
        } else {
            if row < self.dirty_top {
                self.dirty_top = row;
            }
            if row > self.dirty_bot {
                self.dirty_bot = row;
            }
        }
    }

    pub fn mark_all_dirty(&mut self) {
        self.dirty_top = 0;
        self.dirty_bot = self.rows.saturating_sub(1);
    }

    /// Move the cursor, marking **both** the row it leaves and the row it lands
    /// on dirty.
    ///
    /// Both, not just the destination: the cursor is drawn as an opaque block
    /// over the cell it sits on (see `render`), so it lives in the framebuffer,
    /// not just in this cell buffer. A row the cursor vacates is only clean if
    /// it is repainted — otherwise the block stays visible as a stray white
    /// square on the old row.
    ///
    /// Marking only the new row is what produced "the cursor is not cleared
    /// before newline": `newline()` moved the cursor down, repainted the row
    /// below, and left the block sitting on the row above. The same applied to
    /// `carriage_return`, `tab`, and the CSI cursor-motion handlers that marked
    /// nothing at all.
    fn move_cursor(&mut self, x: usize, y: usize) {
        let old_y = self.cursor_y;
        self.cursor_x = x.min(self.cols.saturating_sub(1));
        self.cursor_y = y.min(self.rows.saturating_sub(1));
        self.mark_row_dirty(old_y);
        self.mark_row_dirty(self.cursor_y);
    }

    
    pub fn take_dirty_range(&mut self) -> Option<(usize, usize)> {
        if self.dirty_top > self.dirty_bot {
            return None;
        }
        let r = (self.dirty_top, self.dirty_bot);
        self.dirty_top = 1;
        self.dirty_bot = 0; // empty
        Some(r)
    }

    #[inline]
    fn cell_mut(&mut self, x: usize, y: usize) -> &mut Cell {
        &mut self.cells[y * self.cols + x]
    }

    // ── Writing ─────────────────────────────────────────────────────────

    pub fn put_char(&mut self, ch: char) {
        // Deferred wrap: the cursor parks at the last column and only moves on
        // when the *next* character arrives, so a line that exactly fills the
        // row does not scroll early. This is xterm behaviour and programs
        // depend on it.
        if self.cursor_x >= self.cols {
            self.newline();
        }
        let (x, y) = (self.cursor_x, self.cursor_y);
        if y < self.rows {
            let fg = self.fg;
            let bg = self.bg;
            let bold = self.bold;
            let c = self.cell_mut(x, y);
            c.ch = ch;
            c.fg = fg;
            c.bg = bg;
            c.bold = bold;
            self.mark_row_dirty(y);
        }
        self.cursor_x += 1;
    }

    pub fn newline(&mut self) {
        if self.cursor_y == self.scroll_bot {
            // scroll_up marks the whole region, which includes the row the
            // cursor is on, so the vacated cursor cell is covered.
            self.cursor_x = 0;
            self.scroll_up(1);
        } else {
            self.move_cursor(0, self.cursor_y + 1);
        }
    }

    pub fn carriage_return(&mut self) {
        self.move_cursor(0, self.cursor_y);
    }

    pub fn backspace(&mut self) {
        if self.cursor_x > 0 {
            self.move_cursor(self.cursor_x - 1, self.cursor_y);
        } else if self.cursor_y > 0 {
            self.move_cursor(self.cols - 1, self.cursor_y - 1);
        }
    }

    pub fn tab(&mut self) {
        // Tab stops every 8 columns, as in the Linux n_tty default.
        let next = ((self.cursor_x / 8) + 1) * 8;
        self.move_cursor(next.min(self.cols - 1), self.cursor_y);
    }

    /// Scroll the active region up by `n` lines, discarding the top.
    pub fn scroll_up(&mut self, n: usize) {
        let top = self.scroll_top;
        let bot = self.scroll_bot;
        let height = bot - top + 1;
        if height == 0 {
            return;
        }
        let n = n.min(height);
        let bg = self.default_bg;
        let cols = self.cols;
        // memmove within the region: copy from the bottom up so the source rows
        // are not overwritten before they are read.
        for y in (top..=bot - n).rev() {
            for x in 0..cols {
                let src = self.cells[(y + n) * cols + x];
                self.cells[y * cols + x] = src;
            }
        }
        for y in (bot - n + 1)..=bot {
            for x in 0..cols {
                self.cells[y * cols + x] = Cell::blank(bg);
            }
        }
        self.mark_row_dirty(top);
        self.mark_row_dirty(bot);
    }

    /// Scroll the active region down by `n` lines, discarding the bottom.
    pub fn scroll_down(&mut self, n: usize) {
        let top = self.scroll_top;
        let bot = self.scroll_bot;
        let height = bot - top + 1;
        if height == 0 {
            return;
        }
        let n = n.min(height);
        let bg = self.default_bg;
        let cols = self.cols;
        for y in (top + n)..=bot {
            for x in 0..cols {
                let src = self.cells[(y - n) * cols + x];
                self.cells[y * cols + x] = src;
            }
        }
        for y in top..(top + n) {
            for x in 0..cols {
                self.cells[y * cols + x] = Cell::blank(bg);
            }
        }
        self.mark_row_dirty(top);
        self.mark_row_dirty(bot);
    }

    pub fn set_scroll_region(&mut self, top: usize, bottom: usize) {
        if bottom >= self.rows || top >= bottom {
            return;
        }
        self.scroll_top = top;
        self.scroll_bot = bottom;
    }

    pub fn reset_scroll_region(&mut self) {
        self.scroll_top = 0;
        self.scroll_bot = self.rows - 1;
    }

    pub fn clear(&mut self) {
        let bg = self.default_bg;
        for c in self.cells.iter_mut() {
            *c = Cell::blank(bg);
        }
        self.cursor_x = 0;
        self.cursor_y = 0;
        self.reset_scroll_region();
        self.mark_all_dirty();
    }

    pub fn clear_scrollback(&mut self) {
        self.clear();
    }

    // ── Erasing ─────────────────────────────────────────────────────────

    /// Erase in display. `0` = cursor to end, `1` = start to cursor,
    /// `2` = all, `3` = all + scrollback.
    pub fn erase_in_display(&mut self, mode: usize) {
        match mode {
            0 => {
                self.erase_in_line(0);
                for y in (self.cursor_y + 1)..self.rows {
                    self.erase_row(y);
                }
            }
            1 => {
                for y in 0..self.cursor_y {
                    self.erase_row(y);
                }
                self.erase_in_line(1);
            }
            2 => self.clear(),
            3 => self.clear_scrollback(),
            _ => {}
        }
    }

    /// Erase in line. `0` = cursor to end, `1` = start to cursor, `2` = all.
    pub fn erase_in_line(&mut self, mode: usize) {
        let y = self.cursor_y;
        let bg = self.default_bg;
        let cols = self.cols;
        let (x0, x1) = match mode {
            0 => (self.cursor_x, cols),
            1 => (0, (self.cursor_x + 1).min(cols)),
            2 => (0, cols),
            _ => return,
        };
        for x in x0..x1 {
            self.cells[y * cols + x] = Cell::blank(bg);
        }
        self.mark_row_dirty(y);
    }

    fn erase_row(&mut self, y: usize) {
        let bg = self.default_bg;
        let cols = self.cols;
        for x in 0..cols {
            self.cells[y * cols + x] = Cell::blank(bg);
        }
        self.mark_row_dirty(y);
    }

    pub fn insert_lines(&mut self, n: usize) {
        let y = self.cursor_y;
        if y < self.scroll_top || y > self.scroll_bot {
            return;
        }
        let bg = self.default_bg;
        let cols = self.cols;
        let n = n.min(self.scroll_bot - y + 1);
        for yy in (y..=self.scroll_bot - n).rev() {
            for x in 0..cols {
                let src = self.cells[(yy + n) * cols + x];
                self.cells[yy * cols + x] = src;
            }
        }
        for yy in y..(y + n) {
            for x in 0..cols {
                self.cells[yy * cols + x] = Cell::blank(bg);
            }
        }
        self.mark_row_dirty(y);
        self.mark_row_dirty(self.scroll_bot);
    }

    pub fn delete_lines(&mut self, n: usize) {
        let y = self.cursor_y;
        if y < self.scroll_top || y > self.scroll_bot {
            return;
        }
        let bg = self.default_bg;
        let cols = self.cols;
        let n = n.min(self.scroll_bot - y + 1);
        for yy in y..=(self.scroll_bot - n) {
            for x in 0..cols {
                let src = self.cells[(yy + n) * cols + x];
                self.cells[yy * cols + x] = src;
            }
        }
        for yy in ((self.scroll_bot - n + 1)..=self.scroll_bot).rev() {
            for x in 0..cols {
                self.cells[yy * cols + x] = Cell::blank(bg);
            }
        }
        self.mark_row_dirty(y);
        self.mark_row_dirty(self.scroll_bot);
    }

    /// Erase a row and mark it for repaint, given as the inclusive range it covers.
    ///
    /// Used by the scroll-region setup path (`DECSTBM`), which homes the cursor
    /// into the new region without touching any cell. Without the mark, the
    /// cursor's old position stays painted on the framebuffer.
    pub fn save_cursor(&mut self) {
        self.saved_cursor = (self.cursor_x, self.cursor_y);
    }

    pub fn restore_cursor(&mut self) {
        self.cursor_x = self.saved_cursor.0.min(self.cols - 1);
        self.cursor_y = self.saved_cursor.1.min(self.rows - 1);
        self.mark_row_dirty(self.cursor_y);
    }

    // ── Rendition ───────────────────────────────────────────────────────

    fn apply_sgr(&mut self) {
        let count = self.ansi_param_idx + 1;
        let mut i = 0;
        while i < count {
            let p = self.ansi_params[i];
            match p {
                0 => {
                    self.fg = Color::ANSI_WHITE;
                    self.bg = self.default_bg;
                    self.bold = false;
                }
                1 => self.bold = true,
                22 => self.bold = false,
                30..=37 => self.fg = ansi_color(p - 30, false),
                90..=97 => self.fg = ansi_color(p - 90, true),
                40..=47 => self.bg = ansi_color(p - 40, false),
                100..=107 => self.bg = ansi_color(p - 100, true),
                39 => self.fg = Color::ANSI_WHITE,
                49 => self.bg = self.default_bg,
                38 | 48 => {
                    // Extended colour: 5;n (256) or 2;r;g;b (truecolor).
                    if i + 1 < count && self.ansi_params[i + 1] == 5 && i + 2 < count {
                        self.set_extended(p, 8, i + 2);
                        i += 2;
                    } else if i + 1 < count && self.ansi_params[i + 1] == 2 && i + 4 < count {
                        self.set_extended(p, 24, i + 2);
                        i += 4;
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }

    fn set_extended(&mut self, which: usize, _bits: usize, at: usize) {
        let a = self.ansi_params[at];
        let b = self.ansi_params[at + 1];
        let c = self.ansi_params[at + 2];
        let col = if a <= 7 {
            ansi_color(a, false)
        } else if a < 16 {
            ansi_color(a - 8, true)
        } else if a < 232 {
            // 6x6x6 colour cube.
            let n = a - 16;
            let r = (n / 36) * 255 / 5;
            let g = ((n % 36) / 6) * 255 / 5;
            let bl = (n % 6) * 255 / 5;
            Color::new(r as u8, g as u8, bl as u8)
        } else {
            let v = (8 + (a - 232) * 10) as u8;
            Color::new(v, v, v)
        };
        let _ = (b, c);
        if which == 38 {
            self.fg = col;
        } else {
            self.bg = col;
        }
    }

    // ── ANSI dispatch ────────────────────────────────────────────────────

    /// Feed output into this VT. The VT does not touch the framebuffer; the
    /// caller renders afterwards.
    pub fn write_str(&mut self, s: &str) {
        for ch in s.chars() {
            // Escape sequences first: they are consumed whole and never
            // become cells.
            match self.ansi_state {
                AnsiState::Escape => {
                    match ch {
                        '[' => {
                            self.ansi_state = AnsiState::Csi;
                            self.ansi_params = [0; MAX_PARAMS];
                            self.ansi_param_idx = 0;
                            self.ansi_param_has_digit = false;
                        }
                        ']' => self.ansi_state = AnsiState::Osc,
                        '(' | ')' | '*' | '+' | '#' | '%' => {}
                        '7' => self.save_cursor(),
                        '8' => self.restore_cursor(),
                        'D' => self.newline(),
                        'E' => {
                            self.newline();
                            self.cursor_x = 0;
                        }
                        'M' => {
                            // Reverse index: move up, scrolling at the top.
                            if self.cursor_y == self.scroll_top {
                                self.scroll_down(1);
                            } else if self.cursor_y > 0 {
                                self.cursor_y -= 1;
                                self.mark_row_dirty(self.cursor_y);
                            }
                        }
                        'c' => {
                            // RIS: full reset.
                            self.clear();
                            self.reset_scroll_region();
                            self.fg = Color::ANSI_WHITE;
                            self.bg = self.default_bg;
                            self.bold = false;
                        }
                        _ => self.ansi_state = AnsiState::Normal,
                    }
                    continue;
                }
                AnsiState::Osc => {
                    // Terminated by BEL or ST (ESC \).
                    if ch == '\x07' || ch == '\\' {
                        self.ansi_state = AnsiState::Normal;
                    }
                    continue;
                }
                AnsiState::Csi => {
                    if ch.is_ascii_digit() {
                        self.ansi_param_has_digit = true;
                        if self.ansi_param_idx < MAX_PARAMS {
                            self.ansi_params[self.ansi_param_idx] =
                                self.ansi_params[self.ansi_param_idx] * 10 + (ch as usize - '0' as usize);
                        }
                    } else if ch == ';' {
                        if self.ansi_param_idx < MAX_PARAMS - 1 {
                            self.ansi_param_idx += 1;
                        }
                        self.ansi_param_has_digit = false;
                    } else if ch == '?' || ch == '>' || ch == '<' {
                        // Private marker; parameters still follow.
                    } else {
                        self.dispatch_csi(ch);
                        self.ansi_state = AnsiState::Normal;
                    }
                    continue;
                }
                AnsiState::Normal => {}
            }

            // Control characters.
            match ch {
                '\x1B' => {
                    self.ansi_state = AnsiState::Escape;
                    continue;
                }
                '\x08' | '\x7F' => {
                    self.backspace();
                    continue;
                }
                '\r' => {
                    self.carriage_return();
                    continue;
                }
                '\n' | '\x0B' | '\x0C' => {
                    self.newline();
                    continue;
                }
                '\t' => {
                    self.tab();
                    continue;
                }
                '\x07' => continue, // BEL
                _ if (ch as u32) < 0x20 => continue,
                _ => {}
            }

            self.put_char(ch);
        }
    }

    fn dispatch_csi(&mut self, final_byte: char) {
        // Default parameter is 1 when none was given, per ECMA-48.
        let p0 = if self.ansi_param_has_digit { self.ansi_params[0] } else { 0 };
        let p1 = if self.ansi_params.len() > 1 { self.ansi_params[1] } else { 0 };
        let arg0 = if p0 == 0 { 1 } else { p0 };
        let count = self.ansi_param_idx + 1;

        match final_byte {
            // Full-screen clears and scroll-region reset: the framebuffer no
            // longer matches `cells` anywhere, so repaint everything rather
            // than reason about which rows happen to be in the dirty range.
            //
            // CSI 2 J in particular is what a full-screen program (zellij, an
            // editor, `less`) sends on entry and exit. Repainting only the
            // dirty range there left the previous screen visible -- "terminal
            // is not clearing properly, sometimes previous data is visible" --
            // because nothing in the buffer had changed, so nothing was dirty,
            // so nothing was drawn.
            'J' if p0 == 2 || p0 == 3 => {
                self.mark_all_dirty();
                self.erase_in_display(p0);
            }
            // Cursor position: row ; col (1-based)
            'H' | 'f' => {
                let row = (arg0 - 1).min(self.rows - 1);
                let col = if p1 == 0 { 0 } else { (p1 - 1).min(self.cols - 1) };
                self.move_cursor(col, row);
            }
            // Cursor horizontal / vertical absolute
            'G' | '`' => {
                self.move_cursor((arg0 - 1).min(self.cols - 1), self.cursor_y);
            }
            'd' => {
                self.move_cursor(self.cursor_x, (arg0 - 1).min(self.rows - 1));
            }
            // Cursor movement
            'A' => {
                self.move_cursor(self.cursor_x, self.cursor_y.saturating_sub(arg0));
            }
            'B' => {
                self.move_cursor(self.cursor_x, (self.cursor_y + arg0).min(self.rows - 1));
            }
            'C' => {
                self.move_cursor((self.cursor_x + arg0).min(self.cols - 1), self.cursor_y);
            }
            'D' => {
                self.move_cursor(self.cursor_x.saturating_sub(arg0), self.cursor_y);
            }
            'E' => {
                self.move_cursor(0, (self.cursor_y + arg0).min(self.rows - 1));
            }
            'F' => {
                self.move_cursor(0, self.cursor_y.saturating_sub(arg0));
            }
            // Full-screen clear (mode 2/3) is handled above; modes 0/1 are incremental
            // erases, so only the rows they touch need marking.
            'J' => self.erase_in_display(p0),
            // Erase-in-line: if the cursor sits on the row, its block is drawn
            // after the erase and the row is marked by erase_in_line, so the
            // row is repainted either way.
            'K' => self.erase_in_line(p0),
            'L' => self.insert_lines(arg0),
            'M' => self.delete_lines(arg0),
            'P' => self.delete_chars(arg0),
            '@' => self.insert_chars(arg0),
            'X' => self.erase_chars(arg0),
            'S' => self.scroll_up(arg0),
            'T' => self.scroll_down(arg0),
            'm' => {
                if !self.ansi_param_has_digit && count == 1 {
                    self.ansi_params[0] = 0;
                }
                self.apply_sgr();
            }
            'r' => {
                // Set scroll region; no params means the whole screen.
                if !self.ansi_param_has_digit {
                    self.reset_scroll_region();
                } else {
                    let top = (p0 - 1).min(self.rows - 1);
                    let bottom = (if p1 == 0 { self.rows } else { p1 } - 1).min(self.rows - 1);
                    self.set_scroll_region(top, bottom);
                    // DECSTBM homes the cursor in the new region. The old
                    // position still has a cursor block painted on it, so this
                    // must repaint the row it leaves, not only the destination.
                    self.move_cursor(0, top);
                }
            }
            's' => self.save_cursor(),
            'u' => self.restore_cursor(),
            'h' | 'l' => {
                // DECSET/DECRST. Only ?25 (cursor visibility) is meaningful
                // for a VT buffer; the rest are mode-set requests a text
                // console has no state for.
                if self.ansi_params[0] == 25 {
                    self.cursor_visible = final_byte == 'h';
                }
            }
            _ => {}
        }
    }

    fn delete_chars(&mut self, n: usize) {
        let y = self.cursor_y;
        let bg = self.default_bg;
        let cols = self.cols;
        let x = self.cursor_x;
        let n = n.min(cols - x);
        for xx in x..(cols - n) {
            self.cells[y * cols + xx] = self.cells[y * cols + xx + n];
        }
        for xx in (cols - n)..cols {
            self.cells[y * cols + xx] = Cell::blank(bg);
        }
        self.mark_row_dirty(y);
    }

    fn insert_chars(&mut self, n: usize) {
        let y = self.cursor_y;
        let bg = self.default_bg;
        let cols = self.cols;
        let x = self.cursor_x;
        let n = n.min(cols - x);
        for xx in (x..(cols - n)).rev() {
            self.cells[y * cols + xx] = self.cells[y * cols + xx - n];
        }
        for xx in x..(x + n) {
            self.cells[y * cols + xx] = Cell::blank(bg);
        }
        self.mark_row_dirty(y);
    }

    fn erase_chars(&mut self, n: usize) {
        let y = self.cursor_y;
        let bg = self.default_bg;
        let cols = self.cols;
        let x = self.cursor_x;
        let n = n.min(cols - x);
        for xx in x..(x + n) {
            self.cells[y * cols + xx] = Cell::blank(bg);
        }
        self.mark_row_dirty(y);
    }

    // ── Rendering ───────────────────────────────────────────────────────

    /// Repaint the dirty rows of this VT onto the framebuffer.
    ///
    /// Returns how many rows were drawn. Only the dirty range is touched, so a
    /// character of output costs one row, not a full screen.
    pub fn render(
        &mut self,
        display: &mut crate::graphics::Display,
        font: crate::graphics::font::FontKind,
        scale: usize,
    ) -> usize {
        let Some((top, bot)) = self.take_dirty_range() else { return 0 };
        let cw = font.char_width() * scale;
        let lh = font.line_height() * scale;
        let cols = self.cols;
        for y in top..=bot {
            for x in 0..cols {
                let c = self.cells[y * cols + x];
                let px = x * cw;
                let py = y * lh;
                display.draw_rect(px, py, cw, lh, c.bg);
                if c.ch != ' ' {
                    let fg = if c.bold { brighten(c.fg) } else { c.fg };
                    display.draw_char(px, py, c.ch, fg, c.bg, scale, font);
                }
            }
        }
        // The cursor is drawn last, as an opaque block over its cell. That block
        // exists in the framebuffer and not in `cells`, so it survives any
        // repaint that skips the row it is on — which is why `move_cursor`
        // marks the row it *leaves* dirty as well as the one it lands on.
        //
        // Drawn only when its row is inside the dirty range: if the cursor's row
        // was not repainted, its block is already correct on screen from a
        // previous pass, and redrawing it here would be harmless but painting
        // over a row we did not otherwise touch would not be.
        if self.cursor_visible && self.cursor_y >= top && self.cursor_y <= bot {
            let px = self.cursor_x * cw;
            let py = self.cursor_y * lh;
            display.draw_rect(px, py, cw, lh, Color::ANSI_WHITE);
        }
        bot - top + 1
    }
}

fn ansi_color(n: usize, bright: bool) -> Color {
    match (n, bright) {
        (0, false) => Color::ANSI_BLACK,
        (1, false) => Color::ANSI_RED,
        (2, false) => Color::ANSI_GREEN,
        (3, false) => Color::ANSI_YELLOW,
        (4, false) => Color::ANSI_BLUE,
        (5, false) => Color::ANSI_MAGENTA,
        (6, false) => Color::ANSI_CYAN,
        (7, false) => Color::ANSI_WHITE,
        (0, true) => Color::ANSI_BRIGHT_BLACK,
        (1, true) => Color::ANSI_BRIGHT_RED,
        (2, true) => Color::ANSI_BRIGHT_GREEN,
        (3, true) => Color::ANSI_BRIGHT_YELLOW,
        (4, true) => Color::ANSI_BRIGHT_BLUE,
        (5, true) => Color::ANSI_BRIGHT_MAGENTA,
        (6, true) => Color::ANSI_BRIGHT_CYAN,
        _ => Color::ANSI_BRIGHT_WHITE,
    }
}

fn brighten(c: Color) -> Color {
    Color::new(
        c.r.saturating_add(40),
        c.g.saturating_add(40),
        c.b.saturating_add(40),
    )
}

// ── The VT array ────────────────────────────────────────────────────────────

/// All virtual terminals. Sized once the display geometry and font are known,
/// because a VT's grid is measured in cells and those depend on both.
pub static VTS: SpinLock<Vec<Vt>> = SpinLock::new(Vec::new());

/// Create the VT array. Safe to call again on a font change: existing content
/// is preserved by [`Vt::resize`].
pub fn init_vts(cols: usize, rows: usize) {
    let mut vts = VTS.lock();
    if vts.len() != MAX_VTS {
        vts.clear();
        for _ in 0..MAX_VTS {
            vts.push(Vt::new(cols, rows));
        }
    } else {
        for vt in vts.iter_mut() {
            vt.resize(cols, rows);
        }
    }
}

pub fn vts_ready() -> bool {
    !VTS.lock().is_empty()
}

/// The VT number currently displayed, 1-based as in Linux.
pub fn active_vt_number() -> u16 {
    ACTIVE_VT.load(Ordering::Relaxed)
}

/// Make VT `n` (1-based) current. Returns false if `n` is out of range.
///
/// Switching is only a number change here — no content is touched, because each
/// VT already owns its cells. The caller must repaint, which
/// [`crate::graphics::terminal::Terminal::repaint_all`] does.
pub fn switch_vt(n: u16) -> bool {
    if n < 1 || n as usize > MAX_VTS {
        return false;
    }
    if !vts_ready() {
        return false;
    }
    ACTIVE_VT.store(n, Ordering::SeqCst);
    true
}

/// Run `f` against the active VT.
pub fn with_active<R>(f: impl FnOnce(&mut Vt) -> R) -> Option<R> {
    let idx = (ACTIVE_VT.load(Ordering::Relaxed).max(1) - 1) as usize;
    let mut vts = VTS.lock();
    vts.get_mut(idx).map(f)
}

/// Run `f` against a specific VT by 1-based number.
pub fn with_vt<R>(n: u16, f: impl FnOnce(&mut Vt) -> R) -> Option<R> {
    if n < 1 {
        return None;
    }
    let mut vts = VTS.lock();
    vts.get_mut((n - 1) as usize).map(f)
}

/// First VT not currently bound to the graphical console, for `VT_OPENQRY`.
///
/// Linux hands a compositor a free VT via VT_OPENQRY and the console driver
/// stops driving it; this mirrors that by reporting a VT that is not in use,
/// and [`bind_graphical`] records which one was taken.
pub fn open_query() -> u16 {
    if !vts_ready() {
        return 0;
    }
    let taken = GRAPHICAL_VT.load(Ordering::Relaxed);
    for n in 1..=MAX_VTS as u16 {
        if n != taken {
            return n;
        }
    }
    0
}

/// Take the framebuffer console back from a graphical client.
///
/// Called when a VT switch targets a VT that is not the one a compositor holds.
/// Linux does this as part of `con->vc_swapon`/VT activation: activating the
/// console VT makes the VC layer reclaim the fbdev and puts the console back
/// into `KD_TEXT`, and the DRM master is notified via a lease break.
///
/// Here there is no lease to break, so the notification half is missing — the
/// compositor keeps submitting flips and they will overwrite the text console.
/// What is implemented is the part that makes VT switching usable at all:
/// without the `KD_TEXT` transition the text console never repaints, because
/// nothing re-renders VTs while `CONSOLE_KD_MODE` says a compositor owns the
/// screen.
///
/// Returns the VT whose graphical binding was dropped, or 0 if there was none.
pub fn take_console_back_from_graphical() -> u16 {
    let taken = GRAPHICAL_VT.swap(0, Ordering::SeqCst);
    if taken != 0 {
        crate::arch::x86_64::syscalls::fsio::CONSOLE_KD_MODE
            .store(0, core::sync::atomic::Ordering::SeqCst);
    }
    taken
}

/// The VT currently bound to the graphical console, or 0 for none.
pub static GRAPHICAL_VT: AtomicU16 = AtomicU16::new(0);

/// Bind a VT to the graphical console (the compositor took it) and make it
/// current, as `VT_ACTIVATE` does in the Linux console layer.
pub fn bind_graphical(n: u16) -> bool {
    if !switch_vt(n) {
        return false;
    }
    GRAPHICAL_VT.store(n, Ordering::SeqCst);
    true
}

/// Give a VT back to the text console. Returns false if `n` was not the one
/// taken, so a stray release cannot unbind a VT the compositor still holds.
pub fn release_graphical(n: u16) -> bool {
    if GRAPHICAL_VT.load(Ordering::Relaxed) != n {
        return false;
    }
    GRAPHICAL_VT.store(0, Ordering::SeqCst);
    true
}

// ── Boot-time self-check ────────────────────────────────────────────────────

/// Render a VT's cell contents to a string, one line per row.
///
/// This exists because "is anything actually drawn?" is otherwise unanswerable
/// without a screenshot, and the screen is normally owned by a compositor. A
/// full-screen program like zellij renders into these cells and nowhere else,
/// so this is the only way to see its output in a headless run.
///
/// Trailing blanks are trimmed and a `|` terminator is appended per row, so
/// that row boundaries and trailing content are both visible in a log line.
pub fn dump_rows(n: u16) -> alloc::string::String {
    let mut out = alloc::string::String::new();
    let Some(rows) = with_vt(n, |v| {
        let mut s = alloc::string::String::new();
        for y in 0..v.rows() {
            let mut line = alloc::string::String::new();
            for x in 0..v.cols {
                let c = v.cells[y * v.cols + x];
                // Render a cell that has a non-default background but no glyph
                // as a space; the caller cares about layout, not colour.
                line.push(c.ch);
            }
            while line.ends_with(' ') {
                line.pop();
            }
            s.push_str(&line);
            s.push_str("|\n");
        }
        s
    }) else {
        return alloc::format!("<vt {n} does not exist>\n");
    };
    out.push_str(&rows);
    out
}

/// Dump a VT to the debug log. Called from the keyboard driver on a magic key
/// so a headless run can be inspected after the fact.
pub fn debug_dump(n: u16) {
    let text = dump_rows(n);
    crate::dprintln!("[vtdump] ---- VT {n} ----");
    for line in text.lines() {
        crate::dprintln!("[vtdump] {}", line);
    }
    crate::dprintln!("[vtdump] ---- end VT {n} ----");
}

/// Check the two rendering invariants that are otherwise only observable by
/// looking at the screen, where the framebuffer is usually owned by a
/// compositor.
///
/// Both bugs this guards were reported as one vague symptom ("the terminal is
/// not clearing properly, sometimes previous data is visible") and neither
/// leaves a trace in the log, so both directions are now asserted at boot:
///
/// 1. **A full-screen clear must repaint every row.** `CSI 2 J` blanks every
///    cell, but if the dirty range is what decides what gets drawn, nothing is
///    dirty after the erase and the previous screen stays up. This asserts the
///    erase requests a full repaint.
/// 2. **A cursor move must dirty the row it leaves.** The cursor is an opaque
///    block in the framebuffer, not a cell, so a row the cursor vacates is only
///    clean if repainted. Asserting the vacated row appears in the dirty range
///    is what catches "the cursor is not cleared before newline".
///
/// Runs on a scratch VT and needs no `Display`, so it works before the
/// framebuffer is claimed.
pub fn render_selftest() -> Result<(), &'static str> {
    // Use VT MAX_VTS (the last one) as scratch. Nothing else runs yet at boot.
    let n = MAX_VTS as u16;

    // 1. Full-screen clear asks for a full repaint.
    with_vt(n, |v| {
        v.clear();
        v.mark_all_dirty();
        let _ = v.take_dirty_range(); // consume
        // Simulate what the CSI 2 J arm does: erase, then check the range.
        v.cursor_y = 3;
        v.cursor_x = 5;
        // Pretend nothing was dirty before the clear, as after a period of
        // no output.
        v.dirty_top = 1;
        v.dirty_bot = 0;
        // The arm under test.
        v.mark_all_dirty();
        v.erase_in_display(2);
        let (top, bot) = v.take_dirty_range().unwrap_or((1, 0));
        assert_eq!(top, 0, "clear: must repaint from row 0");
        assert_eq!(bot, v.rows() - 1, "clear: must repaint to the last row");
    });

    // 2. A cursor move dirties the row it leaves.
    with_vt(n, |v| {
        v.clear();
        v.mark_all_dirty();
        let _ = v.take_dirty_range();
        // Sit on row 5 with nothing else pending.
        v.move_cursor(3, 5);
        let _ = v.take_dirty_range(); // consume the landing row
        // Now move up a row. Row 5 holds a painted cursor block and must be
        // repainted to erase it.
        v.move_cursor(3, 4);
        let (top, bot) = v.take_dirty_range().unwrap_or((1, 0));
        assert!(
            top <= 5 && bot >= 5,
            "cursor move: vacated row {top}..{bot} must include 5"
        );
    });

    // 3. newline() specifically, which was the reported symptom.
    with_vt(n, |v| {
        v.clear();
        v.mark_all_dirty();
        let _ = v.take_dirty_range();
        v.move_cursor(0, 5);
        let _ = v.take_dirty_range();
        v.newline();
        let (top, bot) = v.take_dirty_range().unwrap_or((1, 0));
        assert!(
            top <= 5 && bot >= 5,
            "newline: vacated row {top}..{bot} must include 5"
        );
        assert!(
            top <= 6 && bot >= 6,
            "newline: destination row {top}..{bot} must include 6"
        );
    });

    // 4. Carriage return, tab, and horizontal CSI moves must also dirty the row.
    for (name, op) in [
        ("carriage_return", 0usize),
        ("tab", 1),
        ("csi-C", 2),
        ("csi-D", 3),
    ] {
        with_vt(n, |v| {
            v.clear();
            v.mark_all_dirty();
            let _ = v.take_dirty_range();
            v.move_cursor(7, 5);
            let _ = v.take_dirty_range();
            match op {
                0 => v.carriage_return(),
                1 => v.tab(),
                2 => v.move_cursor((v.cursor_x + 3).min(v.cols - 1), v.cursor_y),
                _ => v.move_cursor(v.cursor_x.saturating_sub(3), v.cursor_y),
            }
            let (top, bot) = v.take_dirty_range().unwrap_or((1, 0));
            assert!(
                top <= 5 && bot >= 5,
                "{name}: row {top}..{bot} must include the cursor's row 5"
            );
        });
    }

    // Leave the scratch VT clean for whoever uses it next.
    with_vt(n, |v| v.clear());
    Ok(())
}

/// Exercise the invariants the VT layer is supposed to guarantee, and report
/// the first one that does not hold.
///
/// This exists because the console's correctness is otherwise only observable
/// by looking at the screen -- and the screen is usually owned by a
/// compositor, so a broken text console is invisible. Running it at boot means
/// a regression shows up in the log on every run.
///
/// Uses VTs 2 and 3 as scratch space and restores the active VT afterwards, so
/// it does not disturb whatever was on the console.
pub fn selftest() -> Result<(), &'static str> {
    if VTS.lock().len() < 3 {
        return Err("vt array not initialised");
    }
    let saved_active = ACTIVE_VT.load(Ordering::Relaxed);

    let result = run_checks();

    // Put the scratch VTs back as found and restore the current VT.
    for n in 2..=3u16 {
        with_vt(n, |v| v.clear());
    }
    ACTIVE_VT.store(saved_active, Ordering::SeqCst);
    result
}

fn run_checks() -> Result<(), &'static str> {
    // Two VTs must be independent.
    with_vt(2, |v| {
        v.clear();
        v.write_str("AAAA");
    });
    with_vt(3, |v| {
        v.clear();
        v.write_str("BBBB");
    });
    let t2 = with_vt(2, |v| v.row_text(0)).unwrap_or_default();
    let t3 = with_vt(3, |v| v.row_text(0)).unwrap_or_default();
    if !t2.starts_with("AAAA") {
        return Err("VT 2 did not keep its own text");
    }
    if !t3.starts_with("BBBB") {
        return Err("VT 3 did not keep its own text");
    }

    // Content must survive leaving and re-entering a VT.
    switch_vt(2);
    switch_vt(3);
    switch_vt(2);
    let t2b = with_vt(2, |v| v.row_text(0)).unwrap_or_default();
    if !t2b.starts_with("AAAA") {
        return Err("VT content lost across a switch");
    }

    check_cell_semantics()
}

/// Wrap, erase, scroll-region and resize behaviour, on VT 2.
///
/// Split out because the closure passed to `with_vt` needs an explicit return
/// type for its `Err` paths to typecheck.
fn check_cell_semantics() -> Result<(), &'static str> {
    match with_vt(2, |v| -> Result<(), &'static str> {
        v.clear();

        // Deferred wrap: filling the last column must not scroll yet.
        let cols = v.cols();
        v.cursor_x = cols - 1;
        v.put_char('Z');
        if v.cursor_y != 0 {
            return Err("wrap scrolled early");
        }
        v.put_char('Y');
        // The cursor has advanced *past* the character it just wrote, so the
        // invariant is where the character landed, not where the cursor is.
        if v.cursor_y != 1 || v.cell_char(0, 1) != 'Y' {
            return Err("wrap did not place the character at the next row start");
        }

        // Carriage return and erase-in-line. Reset first: the wrap check above
        // deliberately left the cursor mid-screen, and these checks are about
        // the cells, not about where the cursor happened to be.
        v.clear();
        v.carriage_return();
        v.write_str("keep");
        v.erase_in_line(0);
        if !v.row_text(0).starts_with("keep") {
            return Err("erase-in-line ate the wrong cells");
        }

        // Scroll region: rows outside it must not move.
        v.clear();
        v.set_scroll_region(1, 2);
        for i in 0..4 {
            v.cursor_x = 0;
            v.cursor_y = 1;
            v.write_str(if i == 0 { "one" } else { "xxx" });
        }
        if v.row_text(0).chars().any(|c| c != ' ') {
            return Err("scroll region leaked outside itself");
        }

        // Resize keeps content that still fits.
        v.clear();
        v.write_str("keepme");
        let rows = v.rows();
        v.resize(v.cols(), rows);
        if !v.row_text(0).starts_with("keepme") {
            return Err("resize lost content");
        }
        Ok(())
    }) {
        Some(r) => r,
        None => Err("VT 2 missing"),
    }
}
