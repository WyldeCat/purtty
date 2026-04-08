//! purrtty-pty — PTY session management.
//!
//! Spawns the user's shell, owns the reader/writer handles, and feeds
//! bytes into a `purrtty_term::Terminal` whose grid it mutates.
//!
//! M0-M2: stub only. Real PTY lands in M3.

#![forbid(unsafe_code)]

/// Placeholder PTY session. Will wrap a `portable_pty::PtyPair` in M3.
#[derive(Debug, Default)]
pub struct PtySession {
    _private: (),
}

impl PtySession {
    pub fn new() -> Self {
        Self::default()
    }
}
