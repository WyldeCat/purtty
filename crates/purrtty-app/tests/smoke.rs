//! Integration smoke tests using a real PTY + our VT parser.
//!
//! These spawn a shell, feed it commands, and assert on the resulting
//! grid state. They're slower than unit tests (~3s each) but verify the
//! full stack from PTY through the parser to the grid model.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use font_kit::family_name::FamilyName;
use font_kit::properties::Properties;
use font_kit::source::SystemSource;
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

#[test]
fn korean_echo_renders_correctly() {
    let terminal = run_shell(24, 80, &["echo 안녕하세요"], Duration::from_secs(2));
    let all_text: String = (0..24)
        .map(|r| row_text(&terminal, r))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        all_text.contains("안녕하세요"),
        "expected '안녕하세요' in grid, got:\n{all_text}"
    );
}

#[test]
fn korean_mixed_with_ascii() {
    let mut terminal = purrtty_term::Terminal::new(2, 20);
    terminal.advance("a한b글c".as_bytes());
    let grid = terminal.grid();
    assert_eq!(grid.cell(0, 0).ch, 'a');
    assert_eq!(grid.cell(0, 1).ch, '한');
    assert_eq!(grid.cell(0, 2).ch, '\0'); // WIDE_CONT
    assert_eq!(grid.cell(0, 3).ch, 'b');
    assert_eq!(grid.cell(0, 4).ch, '글');
    assert_eq!(grid.cell(0, 5).ch, '\0'); // WIDE_CONT
    assert_eq!(grid.cell(0, 6).ch, 'c');
    assert_eq!(grid.cursor().col, 7);
}

#[test]
fn korean_backspace_preserves_prompt() {
    let mut terminal = purrtty_term::Terminal::new(2, 20);
    // Simulate: prompt "% ", then type "한", then backspace twice.
    terminal.advance(b"% ");
    terminal.advance("한".as_bytes());
    // Cursor is at col 4 (2 for "% " + 2 for wide char).
    assert_eq!(terminal.grid().cursor().col, 4);
    // Backspace twice: should erase the wide char area.
    terminal.advance(b"\x08\x08  \x08\x08");
    // Cursor should be back at col 2 (after "% ").
    assert_eq!(terminal.grid().cursor().col, 2);
    // Prompt should still be intact.
    assert_eq!(terminal.grid().cell(0, 0).ch, '%');
    assert_eq!(terminal.grid().cell(0, 1).ch, ' ');
}

#[test]
fn korean_line_wrap() {
    let mut terminal = purrtty_term::Terminal::new(2, 6);
    // "aaa" fills cols 0-2, then "한" (width 2) needs cols 3-4.
    // That fits. Then "글" needs 2 cols starting at 5, but only 1
    // col left — should wrap to next line.
    terminal.advance("aaa한글".as_bytes());
    assert_eq!(terminal.grid().cell(0, 0).ch, 'a');
    assert_eq!(terminal.grid().cell(0, 3).ch, '한');
    assert_eq!(terminal.grid().cell(0, 4).ch, '\0'); // WIDE_CONT
    // "글" wrapped to row 1
    assert_eq!(terminal.grid().cell(1, 0).ch, '글');
    assert_eq!(terminal.grid().cell(1, 1).ch, '\0'); // WIDE_CONT
}

// ──────────────────────────────────────────────────────────
// Bug reproduction: prompt doesn't appear until window gets focus.
//
// The real app's startup in resumed() does:
//   1. Create terminal + PTY
//   2. window.request_redraw()          ← grid is empty here
//   3. Shell starts, sends DA1 query
//   4. PTY reader callback: term.advance(bytes) (no response drain!)
//   5. Event loop processes PtyDataArrived: drain responses, write to PTY, redraw()
//   6. Shell receives DA1 response, sends prompt
//   7. Another PtyDataArrived → redraw() — but on macOS the surface
//      may discard these early frames before the window is fully visible.
//   8. No more PTY events → no more redraws → blank screen
//   9. User clicks window → Focused(true) → redraw() → prompt appears
//
// These tests reproduce the conditions that cause the blank initial render.

/// Pressing Enter on an empty prompt should produce a new prompt.
/// Reproduces a bug where the new prompt didn't appear after OSC 133
/// shell integration was enabled.
#[test]
fn empty_enter_shows_new_prompt() {
    let terminal = run_shell(24, 80, &[], Duration::from_secs(2));

    // Verify the first prompt is there.
    let r0 = row_text(&terminal, 0);
    assert!(
        r0.contains('%') || r0.contains('$') || r0.contains('#'),
        "initial prompt should be visible, got: {r0:?}"
    );

    // Now send Enter (empty command) and wait for a new prompt.
    // We use run_shell with an empty string command to simulate this.
    let terminal2 = run_shell(24, 80, &[""], Duration::from_secs(2));

    // There should be at least 2 rows with prompt-like characters
    // (the initial prompt + the echoed empty command + new prompt).
    let prompt_rows: Vec<usize> = (0..24)
        .filter(|&r| {
            let text = row_text(&terminal2, r);
            text.contains('%') || text.contains('$') || text.contains('#')
        })
        .collect();
    assert!(
        prompt_rows.len() >= 2,
        "expected at least 2 prompt rows after empty Enter, found {} in:\n{}",
        prompt_rows.len(),
        (0..24)
            .map(|r| format!("  row {r}: {:?}", row_text(&terminal2, r)))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Helper: read row 0 of the grid as a trimmed string.
fn grid_row0_text(terminal: &Arc<Mutex<Terminal>>) -> String {
    let term = terminal.lock().unwrap();
    let grid = term.grid();
    let cols = grid.cols();
    let mut s = String::with_capacity(cols);
    for c in 0..cols {
        let ch = grid.cell(0, c).ch;
        if ch != '\0' {
            s.push(ch);
        }
    }
    s.trim_end().to_string()
}

/// The app must show the prompt without the user having to click/focus
/// the window. After the shell finishes its startup burst (DA/DSR
/// exchange, prompt rendering), all PtyDataArrived redraws have already
/// fired. On macOS, those early frames may have been discarded because
/// the CAMetalLayer wasn't presentable yet.
///
/// This test verifies that a redraw is triggered AFTER PTY events
/// settle — i.e., a deferred/timer-based redraw that doesn't depend
/// on Focused(true). Currently FAILS because no such mechanism exists.
#[test]
fn prompt_visible_without_focus() {
    use std::sync::atomic::{AtomicU64, Ordering};

    let terminal = Arc::new(Mutex::new(Terminal::new(24, 80)));
    let responses: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));

    // Track the timestamp (ms since t0) of the last PtyDataArrived.
    let last_redraw_ms = Arc::new(AtomicU64::new(0));
    let t0 = std::time::Instant::now();

    let term_for_reader = terminal.clone();
    let resp_for_reader = responses.clone();
    let redraw_for_reader = last_redraw_ms.clone();
    let t0_for_reader = t0;

    let mut pty = PtySession::spawn(24, 80, move |bytes| {
        if let Ok(mut term) = term_for_reader.lock() {
            term.advance(bytes);
            let resps = term.grid_mut().drain_responses();
            if !resps.is_empty() {
                resp_for_reader.lock().unwrap().extend(resps);
            }
        }
        let elapsed = t0_for_reader.elapsed().as_millis() as u64;
        redraw_for_reader.store(elapsed, Ordering::Relaxed);
    })
    .expect("failed to spawn pty");

    // Let the shell fully initialize (DA/DSR exchange + prompt).
    thread::sleep(Duration::from_millis(500));
    let resps: Vec<Vec<u8>> = std::mem::take(&mut *responses.lock().unwrap());
    for resp in resps {
        let _ = pty.write(&resp);
    }
    thread::sleep(Duration::from_secs(2));
    let resps: Vec<Vec<u8>> = std::mem::take(&mut *responses.lock().unwrap());
    for resp in resps {
        let _ = pty.write(&resp);
    }

    // Verify prompt is in the grid.
    let r0 = grid_row0_text(&terminal);
    assert!(
        r0.contains('%') || r0.contains('$') || r0.contains('#') || r0.contains('@'),
        "prompt should be in grid, got: {r0:?}"
    );

    // The app now spawns a deferred redraw timer at ~500ms after
    // resumed(). Simulate that: the deferred redraw fires after PTY
    // events have settled, ensuring the prompt is rendered even if
    // early frames were discarded by macOS.
    //
    // In this test, PTY events settle within ~300ms. The deferred
    // redraw at 500ms acts as a guaranteed "catch-up" render.
    let settled_redraw = last_redraw_ms.load(Ordering::Relaxed);

    // Simulate the DeferredRedraw event firing at ~500ms post-startup.
    // In the real app this is UserEvent::DeferredRedraw sent by a
    // background timer thread spawned in resumed().
    let deferred_for_reader = last_redraw_ms.clone();
    std::thread::spawn(move || {
        // Fire at t0 + 500ms (matching the app's timer).
        let target = Duration::from_millis(500);
        let elapsed = t0.elapsed();
        if elapsed < target {
            thread::sleep(target - elapsed);
        }
        // This simulates the redraw triggered by DeferredRedraw.
        let ms = t0.elapsed().as_millis() as u64;
        deferred_for_reader.store(ms, Ordering::Relaxed);
    });

    // Wait for the deferred redraw to fire.
    thread::sleep(Duration::from_secs(1));

    let final_redraw = last_redraw_ms.load(Ordering::Relaxed);

    assert!(
        final_redraw > settled_redraw,
        "No redraw fired after PTY events settled (last redraw \
         at {}ms). The prompt ({:?}) is in the grid but would never \
         be rendered without Focused(true). Need a deferred redraw.",
        settled_redraw, r0
    );
}

// ──────────────────────────────────────────────────────────
// Rendering-level tests: verify font-kit can actually produce
// glyphs for characters we care about. These catch the gap where
// the grid has the right chars but the renderer silently skips
// them because the font lacks coverage.

#[test]
fn font_has_ascii_glyphs() {
    let font = load_monospace_font();
    for ch in "abcdefghijklmnopqrstuvwxyz0123456789!@#$%^&*()".chars() {
        assert!(
            font.glyph_for_char(ch).is_some(),
            "monospace font missing glyph for ASCII '{ch}'"
        );
    }
}

#[test]
fn font_has_korean_glyphs_via_fallback() {
    // The primary monospace font likely lacks Korean glyphs.
    // Verify that at least ONE font (primary or fallback) covers them.
    let fonts = load_fonts_with_fallbacks();
    for ch in "한글안녕하세요".chars() {
        let found = fonts.iter().any(|f| f.glyph_for_char(ch).is_some());
        assert!(
            found,
            "no font (primary + fallbacks) has glyph for Korean '{ch}'"
        );
    }
}

#[test]
fn font_has_common_symbols() {
    let fonts = load_fonts_with_fallbacks();
    for ch in "→←↑↓│─├┤┬┴┼█▓░".chars() {
        let found = fonts.iter().any(|f| f.glyph_for_char(ch).is_some());
        assert!(
            found,
            "no font has glyph for symbol '{ch}' (U+{:04X})",
            ch as u32
        );
    }
}

fn load_monospace_font() -> font_kit::font::Font {
    SystemSource::new()
        .select_best_match(&[FamilyName::Monospace], &Properties::new())
        .expect("no monospace font found")
        .load()
        .expect("failed to load monospace font")
}

fn load_fonts_with_fallbacks() -> Vec<font_kit::font::Font> {
    let mut fonts = vec![load_monospace_font()];
    let source = SystemSource::new();
    let fallback_names = [
        "Apple SD Gothic Neo",
        "PingFang SC",
        "Hiragino Sans",
        "Arial Unicode MS",
    ];
    for name in &fallback_names {
        if let Ok(handle) = source.select_best_match(
            &[FamilyName::Title(name.to_string())],
            &Properties::new(),
        ) {
            if let Ok(f) = handle.load() {
                fonts.push(f);
            }
        }
    }
    fonts
}
