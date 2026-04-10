//! The terminal grid: a rectangular array of [`Cell`]s, a cursor, a pen,
//! a ring-buffer scrollback, an optional scroll region, and an optional
//! alternate-screen back-buffer.
//!
//! The grid is pure state — it does not own a parser and does not know
//! about VT escape sequences. The `parser` module wires a `vte::Parser`
//! to it.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use unicode_width::UnicodeWidthChar;

use crate::cell::{Attrs, Cell, Color, Pen};

/// Sentinel character used in the right-hand cell of a wide (CJK, emoji)
/// glyph. The renderer skips cells with this `ch` when building its text
/// run so the wide glyph isn't followed by an extraneous space.
pub const WIDE_CONT: char = '\0';

/// Cursor position within the grid (0-indexed, `(row, col)`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
}

/// Grid dimensions in cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub rows: usize,
    pub cols: usize,
}

/// Snapshot of cursor state saved by `ESC 7` / `CSI s` and restored by
/// `ESC 8` / `CSI u`. Pen is saved with the cursor so that color/attrs
/// round-trip across alt-screen switches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SavedCursor {
    pub cursor: Cursor,
    pub pen: Pen,
}

/// The primary-screen state we stash away when entering the alt screen.
#[derive(Debug)]
struct PrimarySnapshot {
    cells: Vec<Cell>,
    cursor: Cursor,
    pen: Pen,
    saved_cursor: Option<SavedCursor>,
    scroll_top: usize,
    scroll_bot: usize,
}

/// Default scrollback capacity in rows.
pub const DEFAULT_SCROLLBACK: usize = 10_000;

/// The core terminal model.
#[derive(Debug)]
pub struct Grid {
    size: Size,
    /// Row-major cells, length `rows * cols`.
    cells: Vec<Cell>,
    cursor: Cursor,
    pen: Pen,
    scrollback: VecDeque<Vec<Cell>>,
    scrollback_limit: usize,

    /// Top of the scroll region, inclusive. Default 0.
    scroll_top: usize,
    /// Bottom of the scroll region, exclusive. Default `rows`.
    scroll_bot: usize,

    /// Saved cursor from ESC 7 / CSI s.
    saved_cursor: Option<SavedCursor>,

    /// DEC mode 25 visibility — tracked here, honored by the renderer.
    cursor_visible: bool,

    /// True while the alt screen is active. Scrollback is skipped in this
    /// mode; the primary buffer lives inside `primary_snapshot`.
    in_alt_screen: bool,
    primary_snapshot: Option<PrimarySnapshot>,

    /// Shell's current working directory, parsed from OSC 7
    /// (`\e]7;file://host/path\a`). `None` if no OSC 7 has been
    /// received yet.
    cwd: Option<PathBuf>,

    /// Pending responses to terminal queries (DA, DSR, etc.) that the
    /// parser queued. The app layer drains this after each advance()
    /// and writes the bytes back to the PTY.
    response_queue: Vec<Vec<u8>>,
}

impl Grid {
    pub fn new(rows: usize, cols: usize) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        Self {
            size: Size { rows, cols },
            cells: vec![Cell::blank(); rows * cols],
            cursor: Cursor::default(),
            pen: Pen::default(),
            scrollback: VecDeque::new(),
            scrollback_limit: DEFAULT_SCROLLBACK,
            scroll_top: 0,
            scroll_bot: rows,
            saved_cursor: None,
            cursor_visible: true,
            in_alt_screen: false,
            primary_snapshot: None,
            cwd: None,
            response_queue: Vec::new(),
        }
    }

    // ---------- accessors ----------

    pub fn size(&self) -> Size {
        self.size
    }

    pub fn rows(&self) -> usize {
        self.size.rows
    }

    pub fn cols(&self) -> usize {
        self.size.cols
    }

    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    pub fn pen(&self) -> Pen {
        self.pen
    }

    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    pub fn scroll_region(&self) -> (usize, usize) {
        (self.scroll_top, self.scroll_bot)
    }

    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    pub fn is_alt_screen(&self) -> bool {
        self.in_alt_screen
    }

    /// Shell cwd as reported by OSC 7. `None` if no OSC 7 has been received.
    pub fn cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    pub fn set_cwd(&mut self, path: PathBuf) {
        self.cwd = Some(path);
    }

    /// Queue a response to be sent back to the PTY (e.g. DA1, DSR).
    pub fn queue_response(&mut self, bytes: Vec<u8>) {
        self.response_queue.push(bytes);
    }

    /// Drain all pending responses. The caller writes them to the PTY.
    pub fn drain_responses(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.response_queue)
    }

    /// Borrow the cell at `(row, col)`. Panics if out of bounds — callers
    /// are expected to clamp first.
    pub fn cell(&self, row: usize, col: usize) -> &Cell {
        &self.cells[self.index(row, col)]
    }

    fn index(&self, row: usize, col: usize) -> usize {
        debug_assert!(row < self.size.rows && col < self.size.cols);
        row * self.size.cols + col
    }

    /// Return the visible rows as slices, top-to-bottom.
    pub fn rows_iter(&self) -> impl Iterator<Item = &[Cell]> {
        let cols = self.size.cols;
        self.cells.chunks(cols)
    }

    /// Resolve a visible-view row index to the backing row slice, honoring
    /// a scrollback offset.
    pub fn row_at(&self, view_idx: usize, scroll_offset: usize) -> Option<&[Cell]> {
        if view_idx >= self.size.rows {
            return None;
        }
        let sb_len = self.scrollback.len();
        let offset = scroll_offset.min(sb_len);
        let abs = (sb_len - offset) + view_idx;
        if abs < sb_len {
            self.scrollback.get(abs).map(|v| v.as_slice())
        } else {
            let r = abs - sb_len;
            let start = r * self.size.cols;
            let end = start + self.size.cols;
            Some(&self.cells[start..end])
        }
    }

    // ---------- structural mutations ----------

    /// Drop all cells and reset the cursor. Scrollback is preserved.
    pub fn clear_visible(&mut self) {
        for c in &mut self.cells {
            *c = Cell::blank();
        }
        self.cursor = Cursor::default();
    }

    /// Resize the visible grid. Non-reflowing: content truncated or padded.
    pub fn resize(&mut self, rows: usize, cols: usize) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if rows == self.size.rows && cols == self.size.cols {
            return;
        }

        self.cells = resize_buffer(&self.cells, self.size, rows, cols);

        // Resize the primary snapshot too, if we're in alt screen.
        if let Some(snap) = self.primary_snapshot.as_mut() {
            snap.cells = resize_buffer(&snap.cells, self.size, rows, cols);
            snap.cursor.row = snap.cursor.row.min(rows - 1);
            snap.cursor.col = snap.cursor.col.min(cols - 1);
            snap.scroll_top = snap.scroll_top.min(rows - 1);
            snap.scroll_bot = snap.scroll_bot.min(rows).max(snap.scroll_top + 1);
        }

        self.size = Size { rows, cols };
        self.cursor.row = self.cursor.row.min(rows - 1);
        self.cursor.col = self.cursor.col.min(cols - 1);
        // Reset scroll region to full screen; apps re-issue DECSTBM after
        // resize.
        self.scroll_top = 0;
        self.scroll_bot = rows;
    }

    // ---------- printing ----------

    /// Write `ch` at the cursor and advance. Wraps to the next line and
    /// scrolls when reaching the bottom-right of the scroll region. Wide
    /// characters (CJK, emoji) occupy two cells; the right-hand cell is a
    /// sentinel that the renderer skips.
    pub fn put_char(&mut self, ch: char) {
        let width = UnicodeWidthChar::width(ch).unwrap_or(1);
        if width == 0 {
            return;
        }
        if self.cursor.col >= self.size.cols {
            self.wrap();
        }
        if width == 2 && self.cursor.col + 1 >= self.size.cols {
            self.wrap();
        }
        let idx = self.index(self.cursor.row, self.cursor.col);
        self.cells[idx] = self.pen.stamp(ch);
        if width == 2 {
            let cont_idx = self.index(self.cursor.row, self.cursor.col + 1);
            self.cells[cont_idx] = Cell {
                ch: WIDE_CONT,
                fg: self.pen.fg,
                bg: self.pen.bg,
                attrs: self.pen.attrs,
            };
        }
        self.cursor.col += width;
    }

    fn wrap(&mut self) {
        self.cursor.col = 0;
        self.advance_row();
    }

    /// Line feed: move cursor down a row, scrolling if at the bottom of
    /// the scroll region.
    pub fn line_feed(&mut self) {
        self.advance_row();
    }

    /// Reverse line feed: move cursor up, scrolling the region down if at
    /// the top.
    pub fn reverse_line_feed(&mut self) {
        if self.cursor.row == self.scroll_top {
            self.scroll_down(1);
        } else if self.cursor.row > 0 {
            self.cursor.row -= 1;
        }
    }

    fn advance_row(&mut self) {
        if self.cursor.row + 1 == self.scroll_bot {
            self.scroll_up(1);
        } else if self.cursor.row + 1 < self.size.rows {
            self.cursor.row += 1;
        }
    }

    // ---------- scrolling ----------

    /// Scroll the current scroll region up by `n` rows (text moves up,
    /// cursor-visible content shifts up). Rows leaving the top are pushed
    /// to scrollback only when the region begins at the screen top AND the
    /// alt screen is not active — matching xterm.
    pub fn scroll_up(&mut self, n: usize) {
        let top = self.scroll_top;
        let bot = self.scroll_bot;
        if top >= bot {
            return;
        }
        let region_rows = bot - top;
        let n = n.min(region_rows);
        let cols = self.size.cols;
        for _ in 0..n {
            if top == 0 && !self.in_alt_screen {
                let row: Vec<Cell> = self.cells[0..cols].to_vec();
                self.push_scrollback(row);
            }
            let src_start = (top + 1) * cols;
            let src_end = bot * cols;
            let dst_start = top * cols;
            self.cells.copy_within(src_start..src_end, dst_start);
            let blank_start = (bot - 1) * cols;
            let blank_end = bot * cols;
            for c in &mut self.cells[blank_start..blank_end] {
                *c = Cell::blank();
            }
        }
    }

    /// Scroll the current scroll region down by `n` rows. Rows leaving
    /// the bottom are discarded (no scrollback).
    pub fn scroll_down(&mut self, n: usize) {
        let top = self.scroll_top;
        let bot = self.scroll_bot;
        if top >= bot {
            return;
        }
        let region_rows = bot - top;
        let n = n.min(region_rows);
        let cols = self.size.cols;
        for _ in 0..n {
            // Shift [top, bot-1) down to [top+1, bot).
            let src_start = top * cols;
            let src_end = (bot - 1) * cols;
            let dst_start = (top + 1) * cols;
            self.cells.copy_within(src_start..src_end, dst_start);
            let blank_start = top * cols;
            let blank_end = (top + 1) * cols;
            for c in &mut self.cells[blank_start..blank_end] {
                *c = Cell::blank();
            }
        }
    }

    fn push_scrollback(&mut self, row: Vec<Cell>) {
        if self.scrollback.len() == self.scrollback_limit {
            self.scrollback.pop_front();
        }
        self.scrollback.push_back(row);
    }

    // ---------- simple C0 ops ----------

    pub fn carriage_return(&mut self) {
        self.cursor.col = 0;
    }

    pub fn backspace(&mut self) {
        if self.cursor.col > 0 {
            self.cursor.col -= 1;
        }
    }

    pub fn tab(&mut self) {
        let next = ((self.cursor.col / 8) + 1) * 8;
        self.cursor.col = next.min(self.size.cols - 1);
    }

    // ---------- cursor motion ----------

    /// Move the cursor to `(row, col)` (0-indexed), clamped to the grid.
    pub fn move_cursor(&mut self, row: usize, col: usize) {
        self.cursor.row = row.min(self.size.rows - 1);
        self.cursor.col = col.min(self.size.cols - 1);
    }

    pub fn cursor_up(&mut self, n: usize) {
        let n = n.max(1);
        // Clamp to scroll region top if the cursor starts inside it.
        let floor = if self.cursor.row >= self.scroll_top {
            self.scroll_top
        } else {
            0
        };
        self.cursor.row = self.cursor.row.saturating_sub(n).max(floor);
    }

    pub fn cursor_down(&mut self, n: usize) {
        let n = n.max(1);
        let ceil = if self.cursor.row < self.scroll_bot {
            self.scroll_bot - 1
        } else {
            self.size.rows - 1
        };
        self.cursor.row = (self.cursor.row + n).min(ceil);
    }

    pub fn cursor_forward(&mut self, n: usize) {
        let n = n.max(1);
        self.cursor.col = (self.cursor.col + n).min(self.size.cols - 1);
    }

    pub fn cursor_back(&mut self, n: usize) {
        let n = n.max(1);
        self.cursor.col = self.cursor.col.saturating_sub(n);
    }

    /// Cursor horizontal absolute (CHA, 0-indexed).
    pub fn cursor_horizontal_absolute(&mut self, col: usize) {
        self.cursor.col = col.min(self.size.cols - 1);
    }

    /// Vertical position absolute (VPA, 0-indexed).
    pub fn cursor_vertical_absolute(&mut self, row: usize) {
        self.cursor.row = row.min(self.size.rows - 1);
    }

    // ---------- line / character insert/delete ----------

    fn cursor_in_region(&self) -> bool {
        self.cursor.row >= self.scroll_top && self.cursor.row < self.scroll_bot
    }

    /// Insert `n` blank lines at the cursor, pushing lines below down
    /// within the scroll region. Rows pushed past `scroll_bot` are lost.
    pub fn insert_lines(&mut self, n: usize) {
        if !self.cursor_in_region() {
            return;
        }
        let top = self.cursor.row;
        let bot = self.scroll_bot;
        let region_rows = bot - top;
        let n = n.max(1).min(region_rows);
        let cols = self.size.cols;

        if region_rows > n {
            let src_start = top * cols;
            let src_end = (bot - n) * cols;
            let dst_start = (top + n) * cols;
            self.cells.copy_within(src_start..src_end, dst_start);
        }
        for r in top..(top + n) {
            let row_start = r * cols;
            let row_end = row_start + cols;
            for c in &mut self.cells[row_start..row_end] {
                *c = Cell::blank();
            }
        }
        self.cursor.col = 0;
    }

    /// Delete `n` lines at the cursor, pulling lines below up within the
    /// scroll region. The exposed rows at the bottom are blanked.
    pub fn delete_lines(&mut self, n: usize) {
        if !self.cursor_in_region() {
            return;
        }
        let top = self.cursor.row;
        let bot = self.scroll_bot;
        let region_rows = bot - top;
        let n = n.max(1).min(region_rows);
        let cols = self.size.cols;

        if region_rows > n {
            let src_start = (top + n) * cols;
            let src_end = bot * cols;
            let dst_start = top * cols;
            self.cells.copy_within(src_start..src_end, dst_start);
        }
        for r in (bot - n)..bot {
            let row_start = r * cols;
            let row_end = row_start + cols;
            for c in &mut self.cells[row_start..row_end] {
                *c = Cell::blank();
            }
        }
        self.cursor.col = 0;
    }

    /// Insert `n` blank characters at the cursor, pushing the remainder of
    /// the line right. Characters pushed past the right edge are lost.
    pub fn insert_chars(&mut self, n: usize) {
        let row = self.cursor.row;
        let col = self.clamped_col();
        let cols = self.size.cols;
        let available = cols - col;
        let n = n.max(1).min(available);
        let row_start = row * cols;
        if available > n {
            self.cells.copy_within(
                row_start + col..row_start + cols - n,
                row_start + col + n,
            );
        }
        for c in &mut self.cells[row_start + col..row_start + col + n] {
            *c = Cell::blank();
        }
    }

    /// Delete `n` characters at the cursor, pulling the remainder of the
    /// line left. Blanks are inserted at the right edge.
    pub fn delete_chars(&mut self, n: usize) {
        let row = self.cursor.row;
        let col = self.clamped_col();
        let cols = self.size.cols;
        let available = cols - col;
        let n = n.max(1).min(available);
        let row_start = row * cols;
        if available > n {
            self.cells.copy_within(
                row_start + col + n..row_start + cols,
                row_start + col,
            );
        }
        for c in &mut self.cells[row_start + cols - n..row_start + cols] {
            *c = Cell::blank();
        }
    }

    /// Erase `n` characters at the cursor in place (no shift).
    pub fn erase_chars(&mut self, n: usize) {
        let row = self.cursor.row;
        let col = self.clamped_col();
        let cols = self.size.cols;
        let available = cols - col;
        let n = n.max(1).min(available);
        let row_start = row * cols;
        for c in &mut self.cells[row_start + col..row_start + col + n] {
            *c = Cell::blank();
        }
    }

    /// Column clamped into the valid index range, hiding the pending-wrap
    /// state where `cursor.col == cols` from index math.
    fn clamped_col(&self) -> usize {
        self.cursor.col.min(self.size.cols - 1)
    }

    /// Erase in display (CSI J).
    pub fn erase_in_display(&mut self, mode: u16) {
        match mode {
            0 => {
                let cursor_idx = self.cursor.row * self.size.cols + self.clamped_col();
                self.blank_range(cursor_idx..self.cells.len());
            }
            1 => {
                let cursor_idx = self.cursor.row * self.size.cols + self.clamped_col();
                self.blank_range(0..=cursor_idx);
            }
            2 | 3 => self.blank_range(0..self.cells.len()),
            _ => {}
        }
    }

    /// Erase in line (CSI K).
    pub fn erase_in_line(&mut self, mode: u16) {
        let row_start = self.cursor.row * self.size.cols;
        let row_end = row_start + self.size.cols;
        let cursor_idx = row_start + self.clamped_col();
        match mode {
            0 => self.blank_range(cursor_idx..row_end),
            1 => self.blank_range(row_start..=cursor_idx),
            2 => self.blank_range(row_start..row_end),
            _ => {}
        }
    }

    fn blank_range(&mut self, range: impl std::slice::SliceIndex<[Cell], Output = [Cell]>) {
        for c in &mut self.cells[range] {
            *c = Cell::blank();
        }
    }

    // ---------- scroll region (DECSTBM) ----------

    /// Set the scroll region. `top` and `bot` are 0-indexed half-open
    /// `[top, bot)`. Empty or invalid range resets to full screen.
    /// DECSTBM homes the cursor per spec.
    pub fn set_scroll_region(&mut self, top: usize, bot: usize) {
        let bot = bot.min(self.size.rows);
        if top >= bot || bot - top < 2 {
            // Invalid or degenerate — reset to full screen.
            self.scroll_top = 0;
            self.scroll_bot = self.size.rows;
        } else {
            self.scroll_top = top;
            self.scroll_bot = bot;
        }
        self.cursor = Cursor::default();
    }

    /// Reset the scroll region to the full visible grid.
    pub fn reset_scroll_region(&mut self) {
        self.scroll_top = 0;
        self.scroll_bot = self.size.rows;
    }

    // ---------- cursor save / restore ----------

    pub fn save_cursor(&mut self) {
        self.saved_cursor = Some(SavedCursor {
            cursor: self.cursor,
            pen: self.pen,
        });
    }

    pub fn restore_cursor(&mut self) {
        if let Some(sc) = self.saved_cursor {
            self.cursor.row = sc.cursor.row.min(self.size.rows - 1);
            self.cursor.col = sc.cursor.col.min(self.size.cols - 1);
            self.pen = sc.pen;
        }
    }

    // ---------- alt screen ----------

    /// Enter the alternate screen buffer. Saves the primary buffer +
    /// cursor + pen + saved-cursor + scroll region, then swaps in a blank
    /// alt buffer with a fresh default state. Idempotent.
    pub fn enter_alt_screen(&mut self) {
        if self.in_alt_screen {
            return;
        }
        let blank = vec![Cell::blank(); self.size.rows * self.size.cols];
        let primary_cells = std::mem::replace(&mut self.cells, blank);
        self.primary_snapshot = Some(PrimarySnapshot {
            cells: primary_cells,
            cursor: self.cursor,
            pen: self.pen,
            saved_cursor: self.saved_cursor.take(),
            scroll_top: self.scroll_top,
            scroll_bot: self.scroll_bot,
        });
        self.cursor = Cursor::default();
        self.pen = Pen::default();
        self.scroll_top = 0;
        self.scroll_bot = self.size.rows;
        self.in_alt_screen = true;
    }

    /// Leave the alt screen and restore the primary buffer + state.
    pub fn leave_alt_screen(&mut self) {
        if !self.in_alt_screen {
            return;
        }
        if let Some(snap) = self.primary_snapshot.take() {
            self.cells = snap.cells;
            self.cursor = snap.cursor;
            self.pen = snap.pen;
            self.saved_cursor = snap.saved_cursor;
            self.scroll_top = snap.scroll_top;
            self.scroll_bot = snap.scroll_bot;
        }
        self.in_alt_screen = false;
    }

    // ---------- DEC modes ----------

    pub fn set_cursor_visible(&mut self, visible: bool) {
        self.cursor_visible = visible;
    }

    // ---------- SGR ----------

    /// Apply a flat list of SGR parameters to the current pen.
    pub fn apply_sgr(&mut self, params: &[u16]) {
        if params.is_empty() {
            self.pen.reset();
            return;
        }
        let mut i = 0;
        while i < params.len() {
            let p = params[i];
            match p {
                0 => self.pen.reset(),
                1 => self.pen.attrs.insert(Attrs::BOLD),
                2 => self.pen.attrs.insert(Attrs::DIM),
                3 => self.pen.attrs.insert(Attrs::ITALIC),
                4 => self.pen.attrs.insert(Attrs::UNDERLINE),
                7 => self.pen.attrs.insert(Attrs::REVERSE),
                8 => self.pen.attrs.insert(Attrs::HIDDEN),
                9 => self.pen.attrs.insert(Attrs::STRIKE),
                22 => self.pen.attrs.remove(Attrs::BOLD | Attrs::DIM),
                23 => self.pen.attrs.remove(Attrs::ITALIC),
                24 => self.pen.attrs.remove(Attrs::UNDERLINE),
                27 => self.pen.attrs.remove(Attrs::REVERSE),
                28 => self.pen.attrs.remove(Attrs::HIDDEN),
                29 => self.pen.attrs.remove(Attrs::STRIKE),
                30..=37 => self.pen.fg = Color::Indexed((p - 30) as u8),
                39 => self.pen.fg = Color::Default,
                40..=47 => self.pen.bg = Color::Indexed((p - 40) as u8),
                49 => self.pen.bg = Color::Default,
                90..=97 => self.pen.fg = Color::Indexed((p - 90 + 8) as u8),
                100..=107 => self.pen.bg = Color::Indexed((p - 100 + 8) as u8),
                38 => {
                    if let Some((color, advance)) = parse_extended_color(&params[i + 1..]) {
                        self.pen.fg = color;
                        i += advance;
                    }
                }
                48 => {
                    if let Some((color, advance)) = parse_extended_color(&params[i + 1..]) {
                        self.pen.bg = color;
                        i += advance;
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }
}

/// Resize a row-major cell buffer, truncating or padding with blanks.
fn resize_buffer(src: &[Cell], from: Size, rows: usize, cols: usize) -> Vec<Cell> {
    let mut out = vec![Cell::blank(); rows * cols];
    let copy_rows = rows.min(from.rows);
    let copy_cols = cols.min(from.cols);
    for r in 0..copy_rows {
        for c in 0..copy_cols {
            out[r * cols + c] = src[r * from.cols + c];
        }
    }
    out
}

/// Parse an extended color spec following an SGR 38/48 code.
fn parse_extended_color(rest: &[u16]) -> Option<(Color, usize)> {
    match rest.first()? {
        5 => {
            let n = *rest.get(1)? as u8;
            Some((Color::Indexed(n), 2))
        }
        2 => {
            let r = *rest.get(1)? as u8;
            let g = *rest.get(2)? as u8;
            let b = *rest.get(3)? as u8;
            Some((Color::Rgb(r, g, b), 4))
        }
        _ => None,
    }
}
