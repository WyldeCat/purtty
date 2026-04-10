//! Integration smoke tests using a real PTY + our VT parser.
//!
//! These spawn a shell, feed it commands, and assert on the resulting
//! grid state. They're slower than unit tests (~3s each) but verify the
//! full stack from PTY through the parser to the grid model.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use purrtty_pty::PtySession;
use purrtty_term::Terminal;

/// Helper: spawn a PTY shell, run it for `duration`, drain DA/DSR
/// responses, and return the terminal for assertions.
fn run_shell(
    rows: u16,
    cols: u16,
    commands: &[&str],
    duration: Duration,
) -> Arc<Mutex<Terminal>> {
    let terminal = Arc::new(Mutex::new(Terminal::new(rows as usize, cols as usize)));
    let responses: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));

    let term_for_reader = terminal.clone();
    let resp_for_reader = responses.clone();

    let mut pty = PtySession::spawn(rows, cols, move |bytes| {
        let mut term = term_for_reader.lock().unwrap();
        term.advance(bytes);
        let resps = term.grid_mut().drain_responses();
        if !resps.is_empty() {
            resp_for_reader.lock().unwrap().extend(resps);
        }
    })
    .expect("failed to spawn pty");

    // Wait for shell to initialize.
    thread::sleep(Duration::from_millis(500));

    // Drain initial responses (DA, DSR).
    let resps: Vec<Vec<u8>> = std::mem::take(&mut *responses.lock().unwrap());
    for resp in resps {
        let _ = pty.write(&resp);
    }

    // Send commands.
    for cmd in commands {
        thread::sleep(Duration::from_millis(200));
        let _ = pty.write(cmd.as_bytes());
        let _ = pty.write(b"\r");
    }

    // Wait for output and drain responses.
    let polls = (duration.as_millis() / 100) as usize;
    for _ in 0..polls {
        thread::sleep(Duration::from_millis(100));
        let resps: Vec<Vec<u8>> = std::mem::take(&mut *responses.lock().unwrap());
        for resp in resps {
            let _ = pty.write(&resp);
        }
    }

    terminal
}

/// Read row `r` of the grid as a trimmed string.
fn row_text(terminal: &Arc<Mutex<Terminal>>, r: usize) -> String {
    let term = terminal.lock().unwrap();
    let grid = term.grid();
    let cols = grid.cols();
    let mut s = String::with_capacity(cols);
    for c in 0..cols {
        let ch = grid.cell(r, c).ch;
        if ch != '\0' {
            s.push(ch);
        }
    }
    s.trim_end().to_string()
}

// ──────────────────────────────────────────────────────────

#[test]
fn shell_prompt_appears() {
    let terminal = run_shell(24, 80, &[], Duration::from_secs(2));
    let r0 = row_text(&terminal, 0);
    assert!(
        r0.contains('%') || r0.contains('$') || r0.contains('#'),
        "expected a shell prompt on row 0, got: {r0:?}"
    );
}

#[test]
fn ls_produces_output() {
    let terminal = run_shell(24, 80, &["ls"], Duration::from_secs(2));
    // Row 0 should have the prompt + "ls", and subsequent rows should
    // have directory listing content.
    let r0 = row_text(&terminal, 0);
    assert!(r0.contains("ls"), "expected 'ls' in row 0, got: {r0:?}");
    // At least one output row should be non-empty.
    let has_output = (1..24).any(|r| !row_text(&terminal, r).is_empty());
    assert!(has_output, "expected ls output in rows 1+");
}

#[test]
fn echo_renders_text() {
    let terminal = run_shell(24, 80, &["echo hello_purrtty"], Duration::from_secs(2));
    let all_text: String = (0..24).map(|r| row_text(&terminal, r)).collect::<Vec<_>>().join("\n");
    assert!(
        all_text.contains("hello_purrtty"),
        "expected 'hello_purrtty' in grid, got:\n{all_text}"
    );
}

#[test]
fn cursor_position_after_prompt() {
    let terminal = run_shell(24, 80, &[], Duration::from_secs(2));
    let term = terminal.lock().unwrap();
    let cursor = term.grid().cursor();
    // Cursor should be on row 0 (or nearby) at a reasonable column
    // (after the prompt text).
    assert!(cursor.row < 3, "cursor row {} too far down", cursor.row);
    assert!(cursor.col > 0, "cursor col should be > 0 (after prompt)");
}

#[test]
fn da_response_is_generated() {
    // Feed a DA1 query directly into the terminal and check that a
    // response is queued.
    let mut terminal = purrtty_term::Terminal::new(24, 80);
    terminal.advance(b"\x1b[c");
    let responses = terminal.grid_mut().drain_responses();
    assert!(
        !responses.is_empty(),
        "expected a DA1 response to be queued"
    );
    let resp = String::from_utf8_lossy(&responses[0]);
    assert!(
        resp.starts_with("\x1b[?"),
        "DA1 response should start with ESC[?, got: {resp:?}"
    );
}

#[test]
fn dsr_cursor_position_report() {
    let mut terminal = purrtty_term::Terminal::new(24, 80);
    // Move cursor to row 5, col 10 (1-indexed: 6, 11).
    terminal.advance(b"\x1b[6;11H");
    terminal.advance(b"\x1b[6n");
    let responses = terminal.grid_mut().drain_responses();
    assert!(!responses.is_empty(), "expected a DSR response");
    let resp = String::from_utf8_lossy(&responses[0]);
    assert_eq!(resp, "\x1b[6;11R", "CPR should report row=6, col=11");
}

#[test]
fn osc7_sets_cwd() {
    let mut terminal = purrtty_term::Terminal::new(24, 80);
    terminal.advance(b"\x1b]7;file:///tmp/test_dir\x07");
    let cwd = terminal.grid().cwd();
    assert_eq!(
        cwd,
        Some(std::path::Path::new("/tmp/test_dir")),
        "OSC 7 should set cwd"
    );
}

#[test]
fn alt_screen_round_trip() {
    let mut terminal = purrtty_term::Terminal::new(4, 10);
    terminal.advance(b"hello");
    terminal.advance(b"\x1b[?1049h"); // enter alt screen
    assert!(terminal.grid().is_alt_screen());
    assert_eq!(terminal.grid().cell(0, 0).ch, ' '); // alt is blank
    terminal.advance(b"world");
    terminal.advance(b"\x1b[?1049l"); // leave alt screen
    assert!(!terminal.grid().is_alt_screen());
    assert_eq!(terminal.grid().cell(0, 0).ch, 'h'); // primary restored
}

#[test]
fn wide_char_cursor_advance() {
    let mut terminal = purrtty_term::Terminal::new(2, 20);
    terminal.advance("한글".as_bytes());
    let cursor = terminal.grid().cursor();
    // 2 wide chars × 2 cells each = 4.
    assert_eq!(cursor.col, 4, "cursor should advance by 4 for two wide chars");
}
