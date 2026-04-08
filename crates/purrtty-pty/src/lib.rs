//! purrtty-pty — PTY session management.
//!
//! Spawns the user's shell, owns the reader/writer handles, and feeds
//! bytes into a VT parser that mutates a `purrtty_term::Grid`.
//!
//! M0: stubs only. Real PTY lands in M3.

#![forbid(unsafe_code)]

use purrtty_term::Grid;

/// Placeholder PTY session. Will wrap a `portable_pty::PtyPair` in M3.
#[derive(Debug, Default)]
pub struct PtySession {
    _grid: Grid,
}

impl PtySession {
    pub fn new() -> Self {
        Self::default()
    }
}
