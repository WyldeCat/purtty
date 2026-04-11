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

/// One row of scrollback, carrying both the cells and a `wrapped` flag
/// that marks whether this row's content continues onto the next row in
/// the same logical line. The flag is set when put_char auto-wraps past
/// the right margin; it stays `false` for rows that end with an explicit
/// newline. `resize` uses the flag to reflow long lines without data
/// loss when the column count changes.
#[derive(Debug, Clone)]
pub struct ScrollbackRow {
    pub cells: Vec<Cell>,
    pub wrapped: bool,
}

/// The primary-screen state we stash away when entering the alt screen.
#[derive(Debug)]
struct PrimarySnapshot {
    cells: Vec<Cell>,
    /// Parallel wrap flags for the snapshot rows, so reflow during an
    /// alt-screen resize still preserves the primary buffer's logical lines.
    wrapped: Vec<bool>,
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
    /// Per-row "soft wrap" flags: `true` means this row's content
    /// continues on the next row in the same logical line. Used by
    /// resize reflow to preserve long lines without data loss.
    row_wrapped: Vec<bool>,
    cursor: Cursor,
    pen: Pen,
    scrollback: VecDeque<ScrollbackRow>,
    scrollback_limit: usize,

    /// Top of the scroll region, inclusive. Default 0.
    scroll_top: usize,
    /// Bottom of the scroll region, exclusive. Default `rows`.
    scroll_bot: usize,

    /// Saved cursor from ESC 7 / CSI s.
    saved_cursor: Option<SavedCursor>,

    /// DEC mode 25 visibility — tracked here, honored by the renderer.
    cursor_visible: bool,

    /// DEC mode 2004 — bracketed paste. The shell toggles this via
    /// `\e[?2004h`/`\e[?2004l` to ask the terminal to wrap pasted text
    /// in `\e[200~ ... \e[201~`, distinguishing paste from typed input.
    bracketed_paste: bool,

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
            row_wrapped: vec![false; rows],
            cursor: Cursor::default(),
            pen: Pen::default(),
            scrollback: VecDeque::new(),
            scrollback_limit: DEFAULT_SCROLLBACK,
            scroll_top: 0,
            scroll_bot: rows,
            saved_cursor: None,
            cursor_visible: true,
            bracketed_paste: false,
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
            self.scrollback.get(abs).map(|r| r.cells.as_slice())
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

    /// Resize the visible grid, preserving content via reflow.
    ///
    /// Reflow treats consecutive wrap-flagged rows as one logical line
    /// and re-lays logical lines at the new width. Content that doesn't
    /// fit the new visible rows spills into scrollback; content that
    /// used to be in scrollback is pulled back onto the visible grid
    /// when growing. The alt screen is NOT reflowed (apps redraw on
    /// SIGWINCH); the alt buffer is truncated/padded like before.
    pub fn resize(&mut self, rows: usize, cols: usize) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if rows == self.size.rows && cols == self.size.cols {
            return;
        }
        let old_cols = self.size.cols;

        if self.in_alt_screen {
            // Alt screen: no reflow. Just truncate/pad the alt cells.
            self.cells = resize_buffer(&self.cells, self.size, rows, cols);
            self.row_wrapped = resize_wrapped(&self.row_wrapped, rows);
            // But reflow the primary snapshot underneath so the user's
            // shell state is intact when they exit alt screen.
            if let Some(snap) = self.primary_snapshot.as_mut() {
                let result = reflow(
                    &snap.cells,
                    &snap.wrapped,
                    &VecDeque::new(),
                    old_cols,
                    rows,
                    cols,
                    self.scrollback_limit,
                    snap.cursor,
                );
                // Snapshot doesn't participate in scrollback reflow during
                // alt-screen — any scrollback history stays in self.scrollback.
                snap.cells = result.new_cells;
                snap.wrapped = result.new_wrapped;
                snap.cursor = result.new_cursor;
                snap.scroll_top = snap.scroll_top.min(rows.saturating_sub(1));
                snap.scroll_bot = snap.scroll_bot.min(rows).max(snap.scroll_top + 1);
            }
        } else {
            let result = reflow(
                &self.cells,
                &self.row_wrapped,
                &self.scrollback,
                old_cols,
                rows,
                cols,
                self.scrollback_limit,
                self.cursor,
            );
            self.cells = result.new_cells;
            self.row_wrapped = result.new_wrapped;
            self.scrollback = result.new_scrollback;
            self.cursor = result.new_cursor;
        }

        self.size = Size { rows, cols };
        self.cursor.row = self.cursor.row.min(rows - 1);
        self.cursor.col = self.cursor.col.min(cols.saturating_sub(1));
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
            self.row_wrapped[self.cursor.row] = true;
            self.wrap();
        }
        if width == 2 && self.cursor.col + 1 >= self.size.cols {
            self.row_wrapped[self.cursor.row] = true;
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
                let cells: Vec<Cell> = self.cells[0..cols].to_vec();
                let wrapped = self.row_wrapped[0];
                self.push_scrollback(ScrollbackRow { cells, wrapped });
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
            // Shift wrap flags up in lockstep with the cells.
            for r in top..(bot - 1) {
                self.row_wrapped[r] = self.row_wrapped[r + 1];
            }
            self.row_wrapped[bot - 1] = false;
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
            // Shift wrap flags down in lockstep.
            for r in (top + 1..bot).rev() {
                self.row_wrapped[r] = self.row_wrapped[r - 1];
            }
            self.row_wrapped[top] = false;
        }
    }

    fn push_scrollback(&mut self, row: ScrollbackRow) {
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
            // Shift wrap flags down alongside cells.
            for r in (top + n..bot).rev() {
                self.row_wrapped[r] = self.row_wrapped[r - n];
            }
        }
        for r in top..(top + n) {
            let row_start = r * cols;
            let row_end = row_start + cols;
            for c in &mut self.cells[row_start..row_end] {
                *c = Cell::blank();
            }
            self.row_wrapped[r] = false;
        }
        // Any line-structure change invalidates the wrap flag on the row
        // above the insertion point, because the old logical line is now
        // broken.
        if top > 0 {
            self.row_wrapped[top - 1] = false;
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
            // Shift wrap flags up alongside cells.
            for r in top..(bot - n) {
                self.row_wrapped[r] = self.row_wrapped[r + n];
            }
        }
        for r in (bot - n)..bot {
            let row_start = r * cols;
            let row_end = row_start + cols;
            for c in &mut self.cells[row_start..row_end] {
                *c = Cell::blank();
            }
            self.row_wrapped[r] = false;
        }
        if top > 0 {
            self.row_wrapped[top - 1] = false;
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
        let blank_cells = vec![Cell::blank(); self.size.rows * self.size.cols];
        let blank_wrapped = vec![false; self.size.rows];
        let primary_cells = std::mem::replace(&mut self.cells, blank_cells);
        let primary_wrapped = std::mem::replace(&mut self.row_wrapped, blank_wrapped);
        self.primary_snapshot = Some(PrimarySnapshot {
            cells: primary_cells,
            wrapped: primary_wrapped,
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
            self.row_wrapped = snap.wrapped;
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

    pub fn set_bracketed_paste(&mut self, enabled: bool) {
        self.bracketed_paste = enabled;
    }

    pub fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
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

fn resize_wrapped(src: &[bool], rows: usize) -> Vec<bool> {
    let mut out = vec![false; rows];
    let copy = rows.min(src.len());
    out[..copy].copy_from_slice(&src[..copy]);
    out
}

/// Result of a reflow pass.
struct ReflowResult {
    new_cells: Vec<Cell>,
    new_wrapped: Vec<bool>,
    new_scrollback: VecDeque<ScrollbackRow>,
    new_cursor: Cursor,
}

/// Reflow the scrollback + primary cells into a new `(rows, cols)` layout.
///
/// The algorithm:
///   1. Collect logical lines from scrollback (oldest first) + primary
///      rows. A logical line is one or more consecutive rows linked by
///      the wrap flag.
///   2. Lay out each logical line into physical rows of `new_cols`
///      width, wrapping when the logical content exceeds the new width
///      and leaving blank padding on the last row of the line.
///   3. Split the resulting physical row list: the last `new_rows`
///      become the visible grid; everything before that becomes the
///      new scrollback (capped at `scrollback_limit`).
///   4. Track the cursor's position through the reflow by remembering
///      its logical offset and looking it up in the new layout.
fn reflow(
    primary: &[Cell],
    primary_wrapped: &[bool],
    scrollback: &VecDeque<ScrollbackRow>,
    old_cols: usize,
    new_rows: usize,
    new_cols: usize,
    scrollback_limit: usize,
    cursor: Cursor,
) -> ReflowResult {
    // ---- Step 1: collect logical lines ----
    // Each logical line is a Vec<Cell> whose length is the raw cell
    // count (wide chars already contribute 2 cells). We also track
    // whether the cursor's position maps into this line, and if so
    // the cell index within the line.
    let mut logical: Vec<Vec<Cell>> = Vec::new();
    let mut current: Vec<Cell> = Vec::new();
    // The cursor's absolute logical offset: (line_index, cell_index).
    // Computed only from the primary rows; scrollback rows have no
    // live cursor. Default to the origin of the first primary logical
    // line — we fix it up below.
    let mut cursor_line: Option<usize> = None;
    let mut cursor_col_in_line: usize = 0;

    // Scrollback first.
    for row in scrollback {
        append_trimmed_row(&mut current, &row.cells, row.wrapped);
        if !row.wrapped {
            logical.push(std::mem::take(&mut current));
        }
    }
    // Then primary rows. Remember where the cursor sat.
    let cursor_row = cursor.row.min(primary.len() / old_cols.max(1));
    for r in 0..primary_wrapped.len() {
        let start = r * old_cols;
        let end = start + old_cols;
        let row_cells = &primary[start..end];
        let wrapped = primary_wrapped[r];

        // If the cursor is on this row, record its logical position
        // BEFORE we trim trailing blanks.
        if r == cursor_row {
            cursor_line = Some(logical.len());
            cursor_col_in_line = current.len() + cursor.col.min(old_cols);
        }

        append_trimmed_row(&mut current, row_cells, wrapped);
        if !wrapped {
            logical.push(std::mem::take(&mut current));
        }
    }
    // If the last primary line was mid-wrap, flush it so it becomes its
    // own logical line (without a wrapped flag on its last physical row).
    if !current.is_empty() {
        logical.push(std::mem::take(&mut current));
    }

    // Trim trailing empty logical lines. These come from blank rows at
    // the bottom of the primary grid that represent "unused space",
    // not meaningful blank lines the user typed. Keeping them would
    // inflate the physical row count and push real content into
    // scrollback on shrink. We only trim past the cursor's line so
    // that the cursor's logical position stays valid.
    let cursor_line_idx = cursor_line.unwrap_or(0);
    while logical.len() > cursor_line_idx + 1
        && logical
            .last()
            .map(|l| l.is_empty())
            .unwrap_or(false)
    {
        logical.pop();
    }

    // ---- Step 2: lay out logical lines at new_cols ----
    let mut phys_cells: Vec<Vec<Cell>> = Vec::new();
    let mut phys_wrapped: Vec<bool> = Vec::new();
    // After layout, we need to know which physical row the cursor's
    // logical line starts on so we can map the cursor offset.
    let mut cursor_phys_row_start: Option<usize> = None;

    for (li, line) in logical.iter().enumerate() {
        if cursor_line == Some(li) {
            cursor_phys_row_start = Some(phys_cells.len());
        }

        if line.is_empty() {
            phys_cells.push(vec![Cell::blank(); new_cols]);
            phys_wrapped.push(false);
            continue;
        }

        let mut pos = 0usize;
        while pos < line.len() {
            let remaining = line.len() - pos;
            let take = remaining.min(new_cols);
            // Don't split a wide-char pair across rows: if the last cell
            // we'd place is a wide char (non-zero width) and its WIDE_CONT
            // follows in the next position, back off by 1.
            let take = if take < remaining && is_wide_head(&line[pos + take - 1], &line[pos + take]) {
                take - 1
            } else {
                take
            };
            if take == 0 {
                // Pathological: new_cols is 1 and a wide char is next.
                // Emit it as a single cell (renderer will clip) and advance.
                break;
            }

            let mut row = vec![Cell::blank(); new_cols];
            row[..take].copy_from_slice(&line[pos..pos + take]);
            phys_cells.push(row);
            pos += take;
            // If more of the same logical line remains, this row is
            // "soft-wrapped".
            phys_wrapped.push(pos < line.len());
        }
    }

    // ---- Step 3: split into scrollback + visible ----
    //
    // Layout rules:
    //   * If there's more content than fits on screen, the OLDEST rows
    //     go into scrollback and the newest `new_rows` become visible.
    //   * If content fits (total <= new_rows), it sits at the TOP of
    //     the visible grid with blank padding below — this matches how
    //     a fresh shell looks and what the user sees after a normal
    //     window resize.
    let total = phys_cells.len();
    let (scroll_count, content_visible) = if total > new_rows {
        (total - new_rows, new_rows)
    } else {
        (0, total)
    };

    let mut new_scrollback: VecDeque<ScrollbackRow> = VecDeque::new();
    let scroll_keep = scroll_count.min(scrollback_limit);
    let scroll_start = scroll_count - scroll_keep;
    for i in scroll_start..scroll_count {
        new_scrollback.push_back(ScrollbackRow {
            cells: std::mem::take(&mut phys_cells[i]),
            wrapped: phys_wrapped[i],
        });
    }

    let mut new_cells: Vec<Cell> = Vec::with_capacity(new_rows * new_cols);
    let mut new_wrapped: Vec<bool> = Vec::with_capacity(new_rows);
    // Visible content starts at row 0 (content-at-top) and runs for
    // `content_visible` rows.
    for i in scroll_count..(scroll_count + content_visible) {
        new_cells.extend_from_slice(&phys_cells[i]);
        new_wrapped.push(phys_wrapped[i]);
    }
    // Bottom padding: blank rows after the content.
    for _ in content_visible..new_rows {
        new_cells.extend(std::iter::repeat(Cell::blank()).take(new_cols));
        new_wrapped.push(false);
    }
    debug_assert_eq!(new_cells.len(), new_rows * new_cols);
    debug_assert_eq!(new_wrapped.len(), new_rows);

    // ---- Step 4: cursor mapping ----
    let new_cursor = if let Some(phys_start) = cursor_phys_row_start {
        let mut remaining = cursor_col_in_line;
        let mut phys_row = phys_start;
        while phys_row < total && remaining >= new_cols && phys_wrapped[phys_row] {
            remaining -= new_cols;
            phys_row += 1;
        }
        let col = remaining.min(new_cols.saturating_sub(1));

        // Map phys_row into the new visible grid. Content starts at
        // row 0 (scroll_count rows live in scrollback and don't count).
        let visible_row = if phys_row < scroll_count {
            0
        } else {
            phys_row - scroll_count
        };
        let visible_row = visible_row.min(new_rows - 1);
        Cursor { row: visible_row, col }
    } else {
        Cursor { row: 0, col: 0 }
    };

    ReflowResult {
        new_cells,
        new_wrapped,
        new_scrollback,
        new_cursor,
    }
}

/// Append a row's cells into the logical-line accumulator, trimming
/// trailing blank cells only when the row is NOT soft-wrapped (trailing
/// blanks on a soft-wrapped row are still part of the logical line).
fn append_trimmed_row(dst: &mut Vec<Cell>, row: &[Cell], wrapped: bool) {
    if wrapped {
        dst.extend_from_slice(row);
    } else {
        let trim = row
            .iter()
            .rposition(|c| !is_blank_cell(c))
            .map(|p| p + 1)
            .unwrap_or(0);
        dst.extend_from_slice(&row[..trim]);
    }
}

fn is_blank_cell(c: &Cell) -> bool {
    c.ch == ' ' || c.ch == '\0'
}

/// Returns true if `head` is a wide-char head (any non-WIDE_CONT char)
/// and `tail` is its continuation (`WIDE_CONT`). Used by reflow to
/// avoid splitting a wide glyph across rows.
fn is_wide_head(head: &Cell, tail: &Cell) -> bool {
    head.ch != WIDE_CONT && tail.ch == WIDE_CONT
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
