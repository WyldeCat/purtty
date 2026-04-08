//! purrtty-term — terminal grid model and VT parser.
//!
//! This crate is pure domain logic: no GPU, no windowing, no PTY I/O.
//! It can be unit-tested in isolation. The `purrtty-ui` crate reads
//! from a [`Grid`] to render, and `purrtty-pty` feeds bytes into a
//! [`Terminal`] whose grid it mutates.

#![forbid(unsafe_code)]

pub mod cell;
pub mod grid;
pub mod parser;

pub use cell::{Attrs, Cell, Color, Pen};
pub use grid::{Cursor, Grid, Size, DEFAULT_SCROLLBACK};
pub use parser::Terminal;

#[cfg(test)]
mod tests {
    use super::*;

    fn row_text(grid: &Grid, row: usize) -> String {
        (0..grid.cols())
            .map(|c| grid.cell(row, c).ch)
            .collect::<String>()
    }

    #[test]
    fn plain_text_writes_left_to_right() {
        let mut t = Terminal::new(4, 10);
        t.advance_str("hello");
        assert_eq!(row_text(t.grid(), 0).trim_end(), "hello");
        assert_eq!(t.grid().cursor(), Cursor { row: 0, col: 5 });
    }

    #[test]
    fn crlf_moves_to_next_line() {
        let mut t = Terminal::new(4, 10);
        t.advance_str("ab\r\ncd");
        assert_eq!(row_text(t.grid(), 0).trim_end(), "ab");
        assert_eq!(row_text(t.grid(), 1).trim_end(), "cd");
        assert_eq!(t.grid().cursor(), Cursor { row: 1, col: 2 });
    }

    #[test]
    fn backspace_moves_left_without_erasing() {
        let mut t = Terminal::new(4, 10);
        t.advance_str("ab\x08c");
        // 'c' overwrites 'b'
        assert_eq!(row_text(t.grid(), 0).trim_end(), "ac");
    }

    #[test]
    fn tab_advances_to_next_eight() {
        let mut t = Terminal::new(4, 20);
        t.advance_str("a\tb");
        assert_eq!(t.grid().cell(0, 0).ch, 'a');
        assert_eq!(t.grid().cell(0, 8).ch, 'b');
    }

    #[test]
    fn wrap_moves_to_next_line_after_rightmost_col() {
        let mut t = Terminal::new(4, 3);
        t.advance_str("abcd");
        assert_eq!(row_text(t.grid(), 0), "abc");
        assert_eq!(row_text(t.grid(), 1).chars().next(), Some('d'));
    }

    #[test]
    fn line_feed_at_bottom_scrolls_and_fills_scrollback() {
        let mut t = Terminal::new(2, 3);
        t.advance_str("abc\r\ndef\r\nghi");
        // After scroll: row0="def", row1="ghi"; scrollback has "abc".
        assert_eq!(row_text(t.grid(), 0), "def");
        assert_eq!(row_text(t.grid(), 1), "ghi");
        assert_eq!(t.grid().scrollback_len(), 1);
    }

    #[test]
    fn cup_moves_cursor_one_indexed() {
        let mut t = Terminal::new(5, 10);
        t.advance_str("\x1b[3;5H*");
        assert_eq!(t.grid().cell(2, 4).ch, '*');
    }

    #[test]
    fn cup_with_no_params_goes_to_origin() {
        let mut t = Terminal::new(5, 10);
        t.advance_str("abc\x1b[H*");
        assert_eq!(t.grid().cell(0, 0).ch, '*');
    }

    #[test]
    fn erase_in_display_mode_2_blanks_everything() {
        let mut t = Terminal::new(3, 4);
        t.advance_str("abcd\r\nefgh\x1b[2J");
        for r in 0..3 {
            assert_eq!(row_text(t.grid(), r), "    ");
        }
    }

    #[test]
    fn erase_in_line_mode_0_clears_from_cursor() {
        let mut t = Terminal::new(2, 6);
        t.advance_str("abcdef\x1b[1;4H\x1b[K");
        assert_eq!(row_text(t.grid(), 0), "abc   ");
    }

    #[test]
    fn sgr_sets_foreground_and_attrs_on_new_cells() {
        let mut t = Terminal::new(2, 10);
        t.advance_str("\x1b[1;31mR\x1b[0mG");
        let r = t.grid().cell(0, 0);
        let g = t.grid().cell(0, 1);
        assert_eq!(r.ch, 'R');
        assert_eq!(r.fg, Color::Indexed(1));
        assert!(r.attrs.contains(Attrs::BOLD));
        assert_eq!(g.ch, 'G');
        assert_eq!(g.fg, Color::Default);
        assert!(!g.attrs.contains(Attrs::BOLD));
    }

    #[test]
    fn sgr_truecolor_foreground() {
        let mut t = Terminal::new(2, 10);
        t.advance_str("\x1b[38;2;10;20;30mX");
        assert_eq!(t.grid().cell(0, 0).fg, Color::Rgb(10, 20, 30));
    }

    #[test]
    fn sgr_256_color_background() {
        let mut t = Terminal::new(2, 10);
        t.advance_str("\x1b[48;5;200mY");
        assert_eq!(t.grid().cell(0, 0).bg, Color::Indexed(200));
    }

    #[test]
    fn sgr_empty_params_resets_pen() {
        let mut t = Terminal::new(2, 10);
        t.advance_str("\x1b[1;31mX\x1b[mY");
        assert!(t.grid().cell(0, 0).attrs.contains(Attrs::BOLD));
        assert_eq!(t.grid().cell(0, 1).fg, Color::Default);
        assert!(!t.grid().cell(0, 1).attrs.contains(Attrs::BOLD));
    }
}
