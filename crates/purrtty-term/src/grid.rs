//! The terminal grid: a rectangular array of [`Cell`]s, a cursor, a pen,
//! and a ring-buffer scrollback.
//!
//! The grid is pure state — it does not own a parser and does not know about
//! VT escape sequences. The `parser` module wires a `vte::Parser` to it.

use std::collections::VecDeque;

use crate::cell::{Attrs, Cell, Color, Pen};

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
        }
    }

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

    /// Borrow the cell at `(row, col)`. Panics if out of bounds — callers are
    /// expected to clamp first.
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
    ///
    /// `view_idx` is in `[0, rows)`, top-to-bottom within the visible window.
    /// `scroll_offset` is the number of rows the view has been scrolled into
    /// scrollback — `0` is the live bottom, `scrollback_len()` is as far up
    /// as we can go.
    ///
    /// Scrollback rows may have a different column count than the current
    /// grid if a resize happened while they were in history. Callers should
    /// tolerate a row shorter or longer than `self.cols()`.
    pub fn row_at(&self, view_idx: usize, scroll_offset: usize) -> Option<&[Cell]> {
        if view_idx >= self.size.rows {
            return None;
        }
        let sb_len = self.scrollback.len();
        let offset = scroll_offset.min(sb_len);
        // Stream position: scrollback (older) followed by live rows (newer).
        // The view's top is at stream index `sb_len - offset`.
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

    // ---------- mutations ----------

    /// Drop all cells and reset the cursor. Scrollback is preserved.
    pub fn clear_visible(&mut self) {
        for c in &mut self.cells {
            *c = Cell::blank();
        }
        self.cursor = Cursor::default();
    }

    /// Resize the visible grid. This is a non-reflowing resize: content is
    /// truncated or padded with blanks. Real reflow lands later.
    pub fn resize(&mut self, rows: usize, cols: usize) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if rows == self.size.rows && cols == self.size.cols {
            return;
        }
        let mut next = vec![Cell::blank(); rows * cols];
        let copy_rows = rows.min(self.size.rows);
        let copy_cols = cols.min(self.size.cols);
        for r in 0..copy_rows {
            for c in 0..copy_cols {
                next[r * cols + c] = self.cells[r * self.size.cols + c];
            }
        }
        self.cells = next;
        self.size = Size { rows, cols };
        self.cursor.row = self.cursor.row.min(rows - 1);
        self.cursor.col = self.cursor.col.min(cols - 1);
    }

    /// Write `ch` at the cursor and advance. Wraps to next line and scrolls
    /// when reaching the bottom-right.
    pub fn put_char(&mut self, ch: char) {
        if self.cursor.col >= self.size.cols {
            self.wrap();
        }
        let idx = self.index(self.cursor.row, self.cursor.col);
        self.cells[idx] = self.pen.stamp(ch);
        self.cursor.col += 1;
    }

    fn wrap(&mut self) {
        self.cursor.col = 0;
        self.advance_row();
    }

    /// Line feed: move cursor down a row, scrolling if at the bottom.
    pub fn line_feed(&mut self) {
        self.advance_row();
    }

    fn advance_row(&mut self) {
        if self.cursor.row + 1 < self.size.rows {
            self.cursor.row += 1;
        } else {
            self.scroll_up(1);
        }
    }

    /// Shift the visible grid up by `n` rows, pushing the displaced rows into
    /// scrollback and clearing the new bottom rows. The cursor row stays put
    /// (it was already on the last row when this was called from `advance_row`).
    pub fn scroll_up(&mut self, n: usize) {
        let n = n.min(self.size.rows);
        for _ in 0..n {
            let row: Vec<Cell> = self.cells[..self.size.cols].to_vec();
            self.push_scrollback(row);
            self.cells.copy_within(self.size.cols.., 0);
            let tail_start = (self.size.rows - 1) * self.size.cols;
            for c in &mut self.cells[tail_start..] {
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

    /// Carriage return: column back to 0.
    pub fn carriage_return(&mut self) {
        self.cursor.col = 0;
    }

    /// Backspace: move cursor left one column (clamped).
    pub fn backspace(&mut self) {
        if self.cursor.col > 0 {
            self.cursor.col -= 1;
        }
    }

    /// Horizontal tab: advance to the next column that is a multiple of 8.
    pub fn tab(&mut self) {
        let next = ((self.cursor.col / 8) + 1) * 8;
        self.cursor.col = next.min(self.size.cols - 1);
    }

    /// Move the cursor to `(row, col)` (0-indexed), clamped to the grid.
    pub fn move_cursor(&mut self, row: usize, col: usize) {
        self.cursor.row = row.min(self.size.rows - 1);
        self.cursor.col = col.min(self.size.cols - 1);
    }

    /// Cursor column clamped into the valid index range. This hides the
    /// "pending wrap" state where `cursor.col == cols` (legal after printing
    /// the rightmost cell, not yet wrapped) from index-taking operations.
    fn clamped_col(&self) -> usize {
        self.cursor.col.min(self.size.cols - 1)
    }

    /// Erase in display (CSI J).
    ///
    /// - 0: from cursor to end of screen
    /// - 1: from start of screen to cursor
    /// - 2: whole screen
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

    // ---------- SGR ----------

    /// Apply a flat list of SGR parameters to the current pen.
    ///
    /// Handles the common subset: reset, attributes (bold/dim/italic/
    /// underline/reverse/hidden/strike), 8-color, bright 8-color, 256-color
    /// and truecolor foreground/background.
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

/// Parse an extended color spec following an SGR 38/48 code.
///
/// Returns the color and the number of parameters consumed *after* the 38/48.
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
