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
    fn wide_character_occupies_two_cells() {
        let mut t = Terminal::new(2, 10);
        t.advance_str("a안b");
        assert_eq!(t.grid().cell(0, 0).ch, 'a');
        assert_eq!(t.grid().cell(0, 1).ch, '안');
        assert_eq!(t.grid().cell(0, 2).ch, grid::WIDE_CONT);
        assert_eq!(t.grid().cell(0, 3).ch, 'b');
        assert_eq!(t.grid().cursor(), Cursor { row: 0, col: 4 });
    }

    #[test]
    fn wide_character_wraps_when_no_room() {
        let mut t = Terminal::new(2, 3);
        // "aa" fills cols 0..2, then 안 (width 2) can't fit in col 2 alone
        // (it would span 2..4 but cols=3). Must wrap to next line.
        t.advance_str("aa안");
        assert_eq!(t.grid().cell(0, 0).ch, 'a');
        assert_eq!(t.grid().cell(0, 1).ch, 'a');
        assert_eq!(t.grid().cell(1, 0).ch, '안');
        assert_eq!(t.grid().cell(1, 1).ch, grid::WIDE_CONT);
    }

    #[test]
    fn sgr_empty_params_resets_pen() {
        let mut t = Terminal::new(2, 10);
        t.advance_str("\x1b[1;31mX\x1b[mY");
        assert!(t.grid().cell(0, 0).attrs.contains(Attrs::BOLD));
        assert_eq!(t.grid().cell(0, 1).fg, Color::Default);
        assert!(!t.grid().cell(0, 1).attrs.contains(Attrs::BOLD));
    }

    // ---------- M3.5 VT hardening ----------

    #[test]
    fn cursor_up_down_forward_back() {
        let mut t = Terminal::new(5, 10);
        // Move down 3, right 4, then up 2, back 1.
        t.advance_str("\x1b[3B\x1b[4C\x1b[2A\x1b[1D*");
        assert_eq!(t.grid().cell(1, 3).ch, '*');
    }

    #[test]
    fn cursor_horizontal_absolute_is_one_indexed() {
        let mut t = Terminal::new(2, 10);
        t.advance_str("\x1b[5G*");
        assert_eq!(t.grid().cell(0, 4).ch, '*');
    }

    #[test]
    fn vertical_position_absolute_is_one_indexed() {
        let mut t = Terminal::new(5, 10);
        t.advance_str("\x1b[3d*");
        assert_eq!(t.grid().cell(2, 0).ch, '*');
    }

    #[test]
    fn insert_line_pushes_rows_down() {
        let mut t = Terminal::new(4, 3);
        t.advance_str("aaa\r\nbbb\r\nccc\r\nddd");
        // Home, then insert a line.
        t.advance_str("\x1b[1;1H\x1b[L");
        assert_eq!(row_text(t.grid(), 0), "   ");
        assert_eq!(row_text(t.grid(), 1), "aaa");
        assert_eq!(row_text(t.grid(), 2), "bbb");
        assert_eq!(row_text(t.grid(), 3), "ccc");
    }

    #[test]
    fn delete_line_pulls_rows_up() {
        let mut t = Terminal::new(4, 3);
        t.advance_str("aaa\r\nbbb\r\nccc\r\nddd");
        t.advance_str("\x1b[1;1H\x1b[M");
        assert_eq!(row_text(t.grid(), 0), "bbb");
        assert_eq!(row_text(t.grid(), 1), "ccc");
        assert_eq!(row_text(t.grid(), 2), "ddd");
        assert_eq!(row_text(t.grid(), 3), "   ");
    }

    #[test]
    fn insert_chars_within_line() {
        let mut t = Terminal::new(2, 6);
        t.advance_str("abcdef");
        // CUP to col 3, insert 2 blanks.
        t.advance_str("\x1b[1;3H\x1b[2@");
        assert_eq!(row_text(t.grid(), 0), "ab  cd");
    }

    #[test]
    fn delete_chars_within_line() {
        let mut t = Terminal::new(2, 6);
        t.advance_str("abcdef");
        t.advance_str("\x1b[1;3H\x1b[2P");
        assert_eq!(row_text(t.grid(), 0), "abef  ");
    }

    #[test]
    fn erase_chars_blanks_in_place() {
        let mut t = Terminal::new(2, 6);
        t.advance_str("abcdef");
        t.advance_str("\x1b[1;3H\x1b[2X");
        assert_eq!(row_text(t.grid(), 0), "ab  ef");
    }

    #[test]
    fn scroll_region_limits_line_feed_scroll() {
        let mut t = Terminal::new(5, 3);
        t.advance_str("aaa\r\nbbb\r\nccc\r\nddd\r\neee");
        // Set scroll region rows 2..=4 (1-indexed in VT, so rows 1..4
        // zero-indexed). DECSTBM homes cursor to (0,0).
        t.advance_str("\x1b[2;4r");
        // CUP to bottom of region (row 4 in 1-index = idx 3).
        t.advance_str("\x1b[4;1H");
        // Line feed: should scroll the region [1,4) up by one, losing
        // row 1 content, row 0 untouched.
        t.advance_str("\n");
        assert_eq!(row_text(t.grid(), 0), "aaa"); // untouched (outside region)
        assert_eq!(row_text(t.grid(), 1), "ccc"); // was row 2
        assert_eq!(row_text(t.grid(), 2), "ddd"); // was row 3
        assert_eq!(row_text(t.grid(), 3), "   "); // new blank row at region bottom
        assert_eq!(row_text(t.grid(), 4), "eee"); // untouched (outside region)
    }

    #[test]
    fn cursor_save_and_restore_via_decsc() {
        let mut t = Terminal::new(4, 10);
        t.advance_str("\x1b[2;5H"); // row 2, col 5 (1-indexed)
        t.advance_str("\x1b7"); // DECSC
        t.advance_str("\x1b[4;1HXX"); // move and write
        t.advance_str("\x1b8"); // DECRC
        t.advance_str("*");
        assert_eq!(t.grid().cell(1, 4).ch, '*');
    }

    #[test]
    fn cursor_save_restore_preserves_pen() {
        let mut t = Terminal::new(2, 10);
        // Set red fg, save. Reset pen. Restore — pen should be red again.
        t.advance_str("\x1b[31m\x1b7\x1b[0m\x1b8R");
        assert_eq!(t.grid().cell(0, 0).ch, 'R');
        assert_eq!(t.grid().cell(0, 0).fg, Color::Indexed(1));
    }

    #[test]
    fn alt_screen_enter_and_leave_swaps_buffers() {
        let mut t = Terminal::new(3, 5);
        t.advance_str("hello");
        // Enter alt screen; buffer should be blank and cursor at origin.
        t.advance_str("\x1b[?1049h");
        assert!(t.grid().is_alt_screen());
        for r in 0..3 {
            assert_eq!(row_text(t.grid(), r), "     ");
        }
        t.advance_str("WORLD");
        assert_eq!(row_text(t.grid(), 0), "WORLD");
        // Leave alt screen; primary should be restored.
        t.advance_str("\x1b[?1049l");
        assert!(!t.grid().is_alt_screen());
        assert_eq!(row_text(t.grid(), 0), "hello");
    }

    #[test]
    fn alt_screen_does_not_push_to_scrollback() {
        let mut t = Terminal::new(2, 3);
        t.advance_str("\x1b[?1049h");
        // Fill both rows and force a scroll within the alt buffer.
        t.advance_str("aaa\r\nbbb\r\nccc");
        assert_eq!(t.grid().scrollback_len(), 0);
        // Exit and the primary scrollback should still be empty.
        t.advance_str("\x1b[?1049l");
        assert_eq!(t.grid().scrollback_len(), 0);
    }

    #[test]
    fn dec_mode_25_toggles_cursor_visibility() {
        let mut t = Terminal::new(2, 3);
        assert!(t.grid().cursor_visible());
        t.advance_str("\x1b[?25l");
        assert!(!t.grid().cursor_visible());
        t.advance_str("\x1b[?25h");
        assert!(t.grid().cursor_visible());
    }

    #[test]
    fn osc7_sets_grid_cwd() {
        let mut t = Terminal::new(2, 10);
        t.advance_str("\x1b]7;file://localhost/Users/foo/bar\x07");
        assert_eq!(
            t.grid().cwd(),
            Some(std::path::Path::new("/Users/foo/bar"))
        );
    }

    #[test]
    fn osc7_percent_decodes_path() {
        let mut t = Terminal::new(2, 10);
        t.advance_str("\x1b]7;file:///tmp/my%20dir\x07");
        assert_eq!(
            t.grid().cwd(),
            Some(std::path::Path::new("/tmp/my dir"))
        );
    }

    #[test]
    fn reverse_index_at_top_scrolls_down() {
        let mut t = Terminal::new(3, 3);
        t.advance_str("aaa\r\nbbb\r\nccc\x1b[1;1H");
        // RI at row 0 — should scroll region down by 1, blanking top row.
        t.advance_str("\x1bM");
        assert_eq!(row_text(t.grid(), 0), "   ");
        assert_eq!(row_text(t.grid(), 1), "aaa");
        assert_eq!(row_text(t.grid(), 2), "bbb");
    }
}
