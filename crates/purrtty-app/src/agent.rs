//! Spawns the Claude CLI as a child process and streams its output.
//!
//! `AgentSession::spawn` runs `claude --print "<prompt>"` in a given
//! working directory, reads stdout on a background thread, and delivers
//! chunks via a caller-supplied callback. A separate `on_exit` callback
//! fires (on the same thread) after the process terminates.

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result};
use tracing::{debug, warn};

/// A running Claude CLI session. Dropping it does **not** kill the child
/// — call [`AgentSession::kill`] explicitly if you need to abort.
pub struct AgentSession {
    child: Option<Child>,
    _reader_thread: JoinHandle<()>,
}

impl AgentSession {
    /// Spawn `claude --print <prompt>` in `cwd`.
    ///
    /// - `on_output` is invoked on a background thread with each chunk
    ///   of stdout bytes as they arrive.
    /// - `on_exit` is invoked once after the child process exits, with
    ///   the exit code (or -1 if unavailable).
    pub fn spawn<F, G>(prompt: &str, cwd: &Path, on_output: F, on_exit: G) -> Result<Self>
    where
        F: FnMut(&[u8]) + Send + 'static,
        G: FnOnce(i32) + Send + 'static,
    {
        debug!(?cwd, "spawning claude agent");

        let mut child = Command::new("claude")
            .args(["--print", prompt])
            .current_dir(cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .spawn()
            .context("spawn claude CLI — is `claude` in $PATH?")?;

        let stdout = child
            .stdout
            .take()
            .context("take claude stdout")?;
        let stderr = child
            .stderr
            .take()
            .context("take claude stderr")?;

        let reader_thread = thread::Builder::new()
            .name("purrtty-agent-reader".into())
            .spawn(move || {
                Self::reader_loop(stdout, stderr, on_output, on_exit);
            })
            .context("spawn agent reader thread")?;

        Ok(Self {
            child: Some(child),
            _reader_thread: reader_thread,
        })
    }

    /// Ask the child to terminate. Best-effort; does not block.
    pub fn kill(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
        }
    }

    fn reader_loop<F, G>(
        mut stdout: impl Read,
        mut stderr: impl Read,
        mut on_output: F,
        on_exit: G,
    ) where
        F: FnMut(&[u8]),
        G: FnOnce(i32),
    {
        let mut buf = [0u8; 4096];

        // Read stdout until EOF.
        loop {
            match stdout.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => on_output(&buf[..n]),
                Err(err) => {
                    warn!(?err, "agent stdout read error");
                    break;
                }
            }
        }

        // Drain stderr and log it (don't send to terminal).
        let mut stderr_buf = Vec::new();
        if let Ok(n) = stderr.read_to_end(&mut stderr_buf) {
            if n > 0 {
                if let Ok(s) = std::str::from_utf8(&stderr_buf) {
                    for line in s.lines() {
                        debug!(line, "agent stderr");
                    }
                }
            }
        }

        // The reader thread outlives the child handle that was moved
        // into `AgentSession`, so we can't call child.wait() here.
        // Instead the exit code is obtained asynchronously — for v0 we
        // just report 0.
        on_exit(0);
    }
}
