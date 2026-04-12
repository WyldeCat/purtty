//! Agent interaction block — groups a user prompt, streamed agent
//! output, tool invocations, and a status footer into one visual unit.
//!
//! The block tracks its lifecycle via `BlockState` and accumulates
//! output segments in order. The renderer reads the block's state to
//! draw a border overlay, header, and live status footer.

use std::time::{Duration, Instant};

/// One segment of output inside a block.
#[derive(Debug, Clone)]
pub enum BlockSegment {
    /// Agent text output (may contain ANSI for terminal rendering).
    Text(String),
    /// A tool invocation with its name and optional streamed I/O.
    Tool {
        name: String,
        input: String,
        output: Option<String>,
    },
}

/// Lifecycle phase of a block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockState {
    /// Agent spawned, no output yet.
    Thinking,
    /// Receiving text_delta events.
    Streaming,
    /// A tool_use content block is active.
    ToolRunning { name: String },
    /// Agent process exited.
    Done { exit_code: i32 },
}

/// Visual state passed to the renderer for the block overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockOverlayState {
    Active,
    Done,
    Error,
}

/// A single agent interaction block.
#[derive(Debug)]
pub struct Block {
    /// The user's prompt (what they typed after `>`).
    pub prompt: String,
    /// Output segments in chronological order.
    pub segments: Vec<BlockSegment>,
    /// Current lifecycle state.
    pub state: BlockState,
    /// Timestamp when the agent was spawned.
    pub started_at: Instant,
    /// Grid row (absolute) where the block starts. Updated on reflow.
    pub start_row: usize,
    /// Number of grid rows the block occupies. Updated each render.
    pub row_count: usize,
}

/// Braille spinner frames for the live status footer.
const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

impl Block {
    pub fn new(prompt: String, start_row: usize) -> Self {
        Self {
            prompt,
            segments: Vec::new(),
            state: BlockState::Thinking,
            started_at: Instant::now(),
            start_row,
            row_count: 1,
        }
    }

    /// Append (or extend) a text segment. Consecutive text_delta
    /// events are merged into one segment.
    pub fn push_text(&mut self, text: &str) {
        if self.state == BlockState::Thinking {
            self.state = BlockState::Streaming;
        }
        match self.segments.last_mut() {
            Some(BlockSegment::Text(buf)) => buf.push_str(text),
            _ => self.segments.push(BlockSegment::Text(text.to_string())),
        }
    }

    /// Begin a new tool invocation. Transitions state to ToolRunning.
    pub fn start_tool(&mut self, name: &str) {
        self.state = BlockState::ToolRunning {
            name: name.to_string(),
        };
        self.segments.push(BlockSegment::Tool {
            name: name.to_string(),
            input: String::new(),
            output: None,
        });
    }

    /// Append streamed JSON input to the current tool segment.
    pub fn push_tool_input(&mut self, json: &str) {
        if let Some(BlockSegment::Tool { input, .. }) = self.segments.last_mut() {
            input.push_str(json);
        }
    }

    /// Mark the current tool as finished. Returns to Streaming since
    /// more text or tools may follow.
    pub fn finish_tool(&mut self) {
        self.state = BlockState::Streaming;
    }

    /// Mark the block as done (agent exited).
    pub fn set_done(&mut self, exit_code: i32) {
        self.state = BlockState::Done { exit_code };
    }

    pub fn is_active(&self) -> bool {
        !matches!(self.state, BlockState::Done { .. })
    }

    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    /// The overlay state for the renderer.
    pub fn overlay_state(&self) -> BlockOverlayState {
        match &self.state {
            BlockState::Done { exit_code } if *exit_code != 0 => BlockOverlayState::Error,
            BlockState::Done { .. } => BlockOverlayState::Done,
            _ => BlockOverlayState::Active,
        }
    }

    /// Build the footer text for the current state.
    pub fn footer_text(&self, spinner_tick: usize) -> String {
        let elapsed = format_elapsed(self.elapsed());
        match &self.state {
            BlockState::Thinking => {
                let s = SPINNER[spinner_tick % SPINNER.len()];
                format!("{s} Thinking... — {elapsed}")
            }
            BlockState::Streaming => {
                let s = SPINNER[spinner_tick % SPINNER.len()];
                format!("{s} Streaming... — {elapsed}")
            }
            BlockState::ToolRunning { name } => {
                let s = SPINNER[spinner_tick % SPINNER.len()];
                format!("{s} Running: {name} ��� {elapsed}")
            }
            BlockState::Done { exit_code } => {
                if *exit_code == 0 {
                    format!("✓ Done — {elapsed}")
                } else {
                    format!("✗ Failed (exit {exit_code}) — {elapsed}")
                }
            }
        }
    }
}

/// Format a Duration as human-friendly elapsed time.
pub fn format_elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        let mins = secs / 60;
        let rem = secs % 60;
        format!("{mins}m {rem}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_block_starts_as_thinking() {
        let b = Block::new("hello".into(), 0);
        assert_eq!(b.state, BlockState::Thinking);
        assert!(b.is_active());
        assert_eq!(b.segments.len(), 0);
    }

    #[test]
    fn push_text_transitions_to_streaming() {
        let mut b = Block::new("hi".into(), 0);
        b.push_text("hello ");
        assert_eq!(b.state, BlockState::Streaming);
        b.push_text("world");
        // Consecutive text is merged.
        assert_eq!(b.segments.len(), 1);
        match &b.segments[0] {
            BlockSegment::Text(t) => assert_eq!(t, "hello world"),
            _ => panic!("expected Text segment"),
        }
    }

    #[test]
    fn tool_lifecycle() {
        let mut b = Block::new("fix bug".into(), 0);
        b.push_text("Let me check...\n");
        b.start_tool("Bash");
        assert_eq!(
            b.state,
            BlockState::ToolRunning {
                name: "Bash".into()
            }
        );
        b.push_tool_input("ls -la");
        b.finish_tool();
        assert_eq!(b.state, BlockState::Streaming);
        b.push_text("Found the issue.");
        // 3 segments: text, tool, text.
        assert_eq!(b.segments.len(), 3);
    }

    #[test]
    fn text_after_tool_is_new_segment() {
        let mut b = Block::new("q".into(), 0);
        b.push_text("a");
        b.start_tool("Read");
        b.finish_tool();
        b.push_text("b");
        // text, tool, text — not merged across tool boundary.
        assert_eq!(b.segments.len(), 3);
    }

    #[test]
    fn done_state() {
        let mut b = Block::new("q".into(), 0);
        b.set_done(0);
        assert!(!b.is_active());
        assert_eq!(b.overlay_state(), BlockOverlayState::Done);
    }

    #[test]
    fn error_state() {
        let mut b = Block::new("q".into(), 0);
        b.set_done(1);
        assert_eq!(b.overlay_state(), BlockOverlayState::Error);
    }

    #[test]
    fn footer_text_varies_by_state() {
        let mut b = Block::new("q".into(), 0);
        let f = b.footer_text(0);
        assert!(f.contains("Thinking"), "got: {f}");

        b.push_text("hi");
        let f = b.footer_text(1);
        assert!(f.contains("Streaming"), "got: {f}");

        b.start_tool("Bash");
        let f = b.footer_text(2);
        assert!(f.contains("Running: Bash"), "got: {f}");

        b.finish_tool();
        b.set_done(0);
        let f = b.footer_text(0);
        assert!(f.contains("✓ Done"), "got: {f}");
    }

    #[test]
    fn footer_error_shows_exit_code() {
        let mut b = Block::new("q".into(), 0);
        b.set_done(127);
        let f = b.footer_text(0);
        assert!(f.contains("✗ Failed"), "got: {f}");
        assert!(f.contains("127"), "got: {f}");
    }

    #[test]
    fn format_elapsed_seconds() {
        assert_eq!(format_elapsed(Duration::from_secs(5)), "5s");
        assert_eq!(format_elapsed(Duration::from_secs(59)), "59s");
    }

    #[test]
    fn format_elapsed_minutes() {
        assert_eq!(format_elapsed(Duration::from_secs(60)), "1m 0s");
        assert_eq!(format_elapsed(Duration::from_secs(90)), "1m 30s");
        assert_eq!(format_elapsed(Duration::from_secs(125)), "2m 5s");
    }

    #[test]
    fn spinner_cycles() {
        let b = Block::new("q".into(), 0);
        let f0 = b.footer_text(0);
        let f1 = b.footer_text(1);
        // Different spinner frames.
        assert_ne!(f0.chars().next(), f1.chars().next());
    }
}
