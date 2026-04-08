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

    fn csi_dispatch(&mut self, params: &Params, _intermediates: &[u8], _ignore: bool, action: char) {
        match action {
            'H' | 'f' => {
                // CUP: row;col, 1-indexed, defaults to 1.
                let mut it = params.iter();
                let row = first_or(it.next(), 1);
                let col = first_or(it.next(), 1);
                self.grid.move_cursor(
                    row.saturating_sub(1) as usize,
                    col.saturating_sub(1) as usize,
                );
            }
            'J' => {
                let mode = first_or(params.iter().next(), 0);
                self.grid.erase_in_display(mode);
            }
            'K' => {
                let mode = first_or(params.iter().next(), 0);
                self.grid.erase_in_line(mode);
            }
            'm' => {
                // Flatten all sub-params into a single slice for SGR.
                let flat: Vec<u16> = params.iter().flatten().copied().collect();
                self.grid.apply_sgr(&flat);
            }
            _ => {}
        }
    }
}

fn first_or(param: Option<&[u16]>, default: u16) -> u16 {
    param
        .and_then(|p| p.first().copied())
        .filter(|v| *v != 0)
        .unwrap_or(default)
}
