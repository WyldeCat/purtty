//! A single terminal cell: character + foreground/background color + attributes.

use bitflags::bitflags;

/// Color slot for foreground or background.
///
/// `Default` means "use the terminal's configured default", which the renderer
/// resolves to concrete RGB at paint time. `Indexed` covers the ANSI 16-color
/// palette (0-15) and the 256-color cube (16-255). `Rgb` is truecolor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl Default for Color {
    fn default() -> Self {
        Self::Default
    }
}

bitflags! {
    /// Per-cell text attributes. Matches the common SGR set we care about in v0.1.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct Attrs: u8 {
        const BOLD      = 1 << 0;
        const DIM       = 1 << 1;
        const ITALIC    = 1 << 2;
        const UNDERLINE = 1 << 3;
        const REVERSE   = 1 << 4;
        const HIDDEN    = 1 << 5;
        const STRIKE    = 1 << 6;
    }
}

/// One cell in the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub attrs: Attrs,
}

impl Cell {
    /// An empty cell using default colors — equivalent to a space.
    pub const fn blank() -> Self {
        Self {
            ch: ' ',
            fg: Color::Default,
            bg: Color::Default,
            attrs: Attrs::empty(),
        }
    }
}

impl Default for Cell {
    fn default() -> Self {
        Self::blank()
    }
}

/// The drawing "pen" currently held by the terminal: color + attrs that will
/// be stamped onto every newly printed cell. SGR sequences mutate this.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Pen {
    pub fg: Color,
    pub bg: Color,
    pub attrs: Attrs,
}

impl Pen {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn stamp(&self, ch: char) -> Cell {
        Cell {
            ch,
            fg: self.fg,
            bg: self.bg,
            attrs: self.attrs,
        }
    }
}
