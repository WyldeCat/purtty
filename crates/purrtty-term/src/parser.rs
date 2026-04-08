//! Glue between `vte::Parser` and [`Grid`].
//!
//! [`Terminal`] owns both a grid and a parser. Feed it bytes via
//! [`Terminal::advance`] and the grid mutates in place.

use vte::{Params, Parser, Perform};

use crate::grid::Grid;

/// A terminal: a grid + a VT state machine driving it.
pub struct Terminal {
    grid: Grid,
    parser: Parser,
}

impl Terminal {
    pub fn new(rows: usize, cols: usize) -> Self {
        Self {
            grid: Grid::new(rows, cols),
            parser: Parser::new(),
        }
    }

    pub fn grid(&self) -> &Grid {
        &self.grid
    }

    pub fn grid_mut(&mut self) -> &mut Grid {
        &mut self.grid
    }

    /// Feed a slice of bytes through the parser.
    pub fn advance(&mut self, bytes: &[u8]) {
        let mut performer = GridPerformer { grid: &mut self.grid };
        for &b in bytes {
            self.parser.advance(&mut performer, b);
        }
    }

    /// Convenience for tests / REPL-style code.
    pub fn advance_str(&mut self, s: &str) {
        self.advance(s.as_bytes());
    }
}

struct GridPerformer<'a> {
    grid: &'a mut Grid,
}

impl Perform for GridPerformer<'_> {
    fn print(&mut self, c: char) {
        self.grid.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\r' => self.grid.carriage_return(),
            b'\n' | 0x0B | 0x0C => self.grid.line_feed(),
            0x08 => self.grid.backspace(),
            b'\t' => self.grid.tab(),
            _ => {}
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &Params,
        intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        // DEC private modes: `CSI ? Pn [; Pn...] h/l`
        if intermediates.first() == Some(&b'?') {
            match action {
                'h' => {
                    for p in params.iter().flatten().copied() {
                        self.set_dec_mode(p, true);
                    }
                }
                'l' => {
                    for p in params.iter().flatten().copied() {
                        self.set_dec_mode(p, false);
                    }
                }
                _ => {}
            }
            return;
        }

        // Intermediates other than `?` (e.g. `>` for secondary DA) are
        // ignored in v0.1.
        if !intermediates.is_empty() {
            return;
        }

        match action {
            // Cursor motion
            'A' => self.grid.cursor_up(first_nonzero(params, 1) as usize),
            'B' | 'e' => self.grid.cursor_down(first_nonzero(params, 1) as usize),
            'C' | 'a' => self.grid.cursor_forward(first_nonzero(params, 1) as usize),
            'D' => self.grid.cursor_back(first_nonzero(params, 1) as usize),
            'G' | '`' => self
                .grid
                .cursor_horizontal_absolute((first_nonzero(params, 1) as usize).saturating_sub(1)),
            'd' => self
                .grid
                .cursor_vertical_absolute((first_nonzero(params, 1) as usize).saturating_sub(1)),
            'H' | 'f' => {
                let mut it = params.iter();
                let row = first_or(it.next(), 1);
                let col = first_or(it.next(), 1);
                self.grid.move_cursor(
                    row.saturating_sub(1) as usize,
                    col.saturating_sub(1) as usize,
                );
            }

            // Erase
            'J' => {
                let mode = first_or(params.iter().next(), 0);
                self.grid.erase_in_display(mode);
            }
            'K' => {
                let mode = first_or(params.iter().next(), 0);
                self.grid.erase_in_line(mode);
            }

            // Line insert / delete
            'L' => self.grid.insert_lines(first_nonzero(params, 1) as usize),
            'M' => self.grid.delete_lines(first_nonzero(params, 1) as usize),

            // Scroll
            'S' => self.grid.scroll_up(first_nonzero(params, 1) as usize),
            'T' => self.grid.scroll_down(first_nonzero(params, 1) as usize),

            // Character insert / delete / erase
            '@' => self.grid.insert_chars(first_nonzero(params, 1) as usize),
            'P' => self.grid.delete_chars(first_nonzero(params, 1) as usize),
            'X' => self.grid.erase_chars(first_nonzero(params, 1) as usize),

            // Scroll region (DECSTBM)
            'r' => {
                let mut it = params.iter();
                let top = first_or(it.next(), 1);
                let bot = first_or(it.next(), self.grid.rows() as u16);
                self.grid.set_scroll_region(
                    top.saturating_sub(1) as usize,
                    bot as usize,
                );
            }

            // Cursor save / restore (ANSI variant)
            's' => self.grid.save_cursor(),
            'u' => self.grid.restore_cursor(),

            // SGR
            'm' => {
                let flat: Vec<u16> = params.iter().flatten().copied().collect();
                self.grid.apply_sgr(&flat);
            }

            _ => {}
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        match byte {
            // DECSC / DECRC — save / restore cursor
            b'7' => self.grid.save_cursor(),
            b'8' => self.grid.restore_cursor(),
            // RI — reverse index
            b'M' => self.grid.reverse_line_feed(),
            // NEL — next line
            b'E' => {
                self.grid.carriage_return();
                self.grid.line_feed();
            }
            // IND — index (line feed)
            b'D' => self.grid.line_feed(),
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {
        // Title sets (0/1/2) and friends are accepted silently for now.
    }
}

impl GridPerformer<'_> {
    fn set_dec_mode(&mut self, mode: u16, enable: bool) {
        match mode {
            25 => self.grid.set_cursor_visible(enable),
            1049 | 1047 | 47 => {
                if enable {
                    self.grid.enter_alt_screen();
                } else {
                    self.grid.leave_alt_screen();
                }
            }
            // Autowrap, bracketed paste, mouse tracking, etc. — accept
            // silently so sending them doesn't garble state, but don't do
            // anything with them yet.
            7 | 2004 | 1000 | 1002 | 1003 | 1006 | 1015 | 12 => {}
            _ => {}
        }
    }
}

/// Extract the first parameter, applying a default if missing **or** 0.
/// Used where the VT spec says "0 is treated as 1" (cursor motion, IL, DL,
/// etc.).
fn first_nonzero(params: &Params, default: u16) -> u16 {
    params
        .iter()
        .next()
        .and_then(|p| p.first().copied())
        .filter(|v| *v != 0)
        .unwrap_or(default)
}

/// Extract the first parameter with a default for missing/0.
fn first_or(param: Option<&[u16]>, default: u16) -> u16 {
    param
        .and_then(|p| p.first().copied())
        .filter(|v| *v != 0)
        .unwrap_or(default)
}
