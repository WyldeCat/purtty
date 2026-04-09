//! purrtty-pty — spawn and drive a child shell via a pseudoterminal.
//!
//! [`PtySession::spawn`] opens a PTY, spawns `$SHELL` (or zsh as a
//! fallback) connected to the slave side, and starts a background
//! thread that reads bytes from the master and invokes a caller-
//! supplied callback. The caller owns decoding/VT parsing.

#![forbid(unsafe_code)]

use std::io::{Read, Write};
use std::thread::{self, JoinHandle};

use anyhow::{anyhow, Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use tracing::{debug, warn};

/// A running shell session bound to a pseudoterminal.
pub struct PtySession {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    _reader_thread: JoinHandle<()>,
}

impl PtySession {
    /// Spawn a new shell in a fresh PTY. `on_read` is invoked on a
    /// background thread whenever bytes arrive from the child.
    pub fn spawn<F>(rows: u16, cols: u16, on_read: F) -> Result<Self>
    where
        F: FnMut(&[u8]) + Send + 'static,
    {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: rows.max(1),
                cols: cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| anyhow!("openpty failed: {e}"))?;

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
        debug!(%shell, rows, cols, "spawning shell");

        let mut cmd = CommandBuilder::new(&shell);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        if let Ok(home) = std::env::var("HOME") {
            cmd.cwd(home);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| anyhow!("spawn_command({shell}): {e}"))?;

        // Release the slave end in this process so the child becomes the
        // sole owner — otherwise the reader never sees EOF on child exit.
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| anyhow!("clone reader: {e}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| anyhow!("take writer: {e}"))?;

        let reader_thread = thread::Builder::new()
            .name("purrtty-pty-reader".into())
            .spawn(move || {
                let mut buf = [0u8; 4096];
                let mut on_read = on_read;
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => {
                            debug!("pty reader: EOF");
                            break;
                        }
                        Ok(n) => on_read(&buf[..n]),
                        Err(err) => {
                            warn!(?err, "pty read error");
                            break;
                        }
                    }
                }
            })
            .context("spawn pty reader thread")?;

        Ok(Self {
            master: pair.master,
            writer,
            child,
            _reader_thread: reader_thread,
        })
    }

    /// Write bytes to the child's stdin.
    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        self.writer
            .write_all(data)
            .context("write to pty master")?;
        self.writer.flush().context("flush pty master")?;
        Ok(())
    }

    /// The child shell's process ID, if available.
    pub fn child_pid(&self) -> Option<u32> {
        self.child.process_id()
    }

    /// Tell the PTY about a new window size (in cells).
    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        self.master
            .resize(PtySize {
                rows: rows.max(1),
                cols: cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| anyhow!("pty resize: {e}"))?;
        Ok(())
    }
}
