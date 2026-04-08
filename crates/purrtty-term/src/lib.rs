//! purrtty-term — terminal grid model and VT parser.
//!
//! This crate is pure domain logic: no GPU, no windowing, no PTY I/O.
//! It can be unit-tested in isolation. The `purrtty-ui` crate reads
//! from a [`Grid`] to render, and `purrtty-pty` feeds bytes into a
//! parser that mutates a [`Grid`].
//!
//! M0: stubs only. Real model lands in M2.

#![forbid(unsafe_code)]

/// Placeholder terminal grid. Will grow real fields in M2.
#[derive(Debug, Default)]
pub struct Grid {
    _private: (),
}

impl Grid {
    pub fn new() -> Self {
        Self::default()
    }
}
