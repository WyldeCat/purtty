//! Spawns the Claude CLI with streaming JSON output and parses events.
//!
//! `AgentSession::spawn` runs `claude -p "<prompt>"` with
//! `--output-format stream-json` so we get structured events (text
//! deltas, tool use, results) instead of raw markdown. Each event is
//! formatted into clean ANSI text and delivered to the caller via
//! `on_output(&[u8])`, which feeds `terminal.advance()`.

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result};
use serde_json::Value;
use tracing::{debug, warn};

/// A running Claude CLI session. Call [`AgentSession::kill`] to abort.
pub struct AgentSession {
    child: Option<Child>,
    _reader_thread: JoinHandle<()>,
}

impl AgentSession {
    /// Spawn `claude -p <prompt>` with streaming JSON output and full
    /// tool access. Parsed events are formatted into ANSI text and
    /// delivered via `on_output`. `on_exit` fires after the process
    /// terminates.
    pub fn spawn<F, G>(prompt: &str, cwd: &Path, on_output: F, on_exit: G) -> Result<Self>
    where
        F: FnMut(&[u8]) + Send + 'static,
        G: FnOnce(i32) + Send + 'static,
    {
        debug!(?cwd, "spawning claude agent (stream-json)");

        let mut child = Command::new("claude")
            .args([
                "-p",
                prompt,
                "--output-format",
                "stream-json",
                "--verbose",
                "--include-partial-messages",
                "--allowedTools",
                "Bash,Read,Edit,Write,Grep,Glob",
            ])
            .current_dir(cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .spawn()
            .context("spawn claude CLI — is `claude` in $PATH?")?;

        let stdout = child.stdout.take().context("take claude stdout")?;
        let stderr = child.stderr.take().context("take claude stderr")?;

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
        stdout: impl Read,
        mut stderr: impl Read,
        mut on_output: F,
        on_exit: G,
    ) where
        F: FnMut(&[u8]),
        G: FnOnce(i32),
    {
        let reader = BufReader::new(stdout);

        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(err) => {
                    warn!(?err, "agent stdout read error");
                    break;
                }
            };
            if line.is_empty() {
                continue;
            }
            let json: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => {
                    // Not valid JSON — might be a stray log line. Skip.
                    continue;
                }
            };
            if let Some(formatted) = format_event(&json) {
                on_output(formatted.as_bytes());
            }
        }

        // Drain stderr and log.
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

        on_exit(0);
    }
}

/// Turn a streaming JSON event into formatted ANSI text for the
/// terminal grid. Returns `None` for events we don't want to render.
fn format_event(json: &Value) -> Option<String> {
    let event_type = json.get("type")?.as_str()?;

    match event_type {
        // Text streaming: `.event.delta.type == "text_delta"`
        "stream_event" => {
            let event = json.get("event")?;

            // Text delta — the main content stream.
            if let Some(delta) = event.get("delta") {
                if delta.get("type").and_then(|t| t.as_str()) == Some("text_delta") {
                    return delta.get("text").and_then(|t| t.as_str()).map(String::from);
                }
            }

            // Content block start — detect tool use.
            if event.get("type").and_then(|t| t.as_str()) == Some("content_block_start") {
                if let Some(block) = event.get("content_block") {
                    if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                        let tool_name = block
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("tool");
                        return Some(format!(
                            "\r\n\x1b[1;33m⚡ {}\x1b[0m ",
                            tool_name
                        ));
                    }
                }
            }

            // Input JSON delta for tool use — show what's being passed.
            if delta_is_input_json(event) {
                if let Some(partial) = event
                    .get("delta")
                    .and_then(|d| d.get("partial_json"))
                    .and_then(|p| p.as_str())
                {
                    return Some(format!("\x1b[2m{}\x1b[0m", partial));
                }
            }

            // Content block stop after tool use — newline.
            if event.get("type").and_then(|t| t.as_str()) == Some("content_block_stop") {
                return Some("\r\n".to_string());
            }

            None
        }

        // Result message — final output. Text was already streamed via
        // deltas, so we just add a trailing newline if needed.
        "result" => Some("\r\n".to_string()),

        // System init — log session ID.
        "system" => {
            if json.get("subtype").and_then(|s| s.as_str()) == Some("init") {
                if let Some(sid) = json
                    .get("data")
                    .and_then(|d| d.get("session_id"))
                    .and_then(|s| s.as_str())
                {
                    debug!(session_id = sid, "agent session started");
                }
            }
            None
        }

        _ => None,
    }
}

fn delta_is_input_json(event: &Value) -> bool {
    event
        .get("delta")
        .and_then(|d| d.get("type"))
        .and_then(|t| t.as_str())
        == Some("input_json_delta")
}
