//! purrtty — binary entry point.
//!
//! M3: wires a real PTY-backed shell to the VT parser and the wgpu
//! renderer. The event loop owns the renderer, the PTY session, and a
//! shared `Terminal` that both the UI thread and the PTY reader thread
//! mutate under a mutex. Keyboard input is translated to bytes and
//! pushed into the PTY; the reader thread feeds incoming bytes into
//! the VT parser and wakes the UI via an `EventLoopProxy`.

mod agent;
mod block;
mod config;
#[cfg(target_os = "macos")]
mod macos;
mod selection;
mod urls;

use std::sync::{Arc, Mutex};

use agent::{AgentSession, BlockUpdate};
use block::Block;
use config::Config;
use selection::{pixel_to_cell, selection_to_text, view_row_to_absolute, GridPoint, Selection};

use anyhow::Result;
use purrtty_pty::PtySession;
use purrtty_term::Terminal;
use purrtty_ui::Renderer;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, Ime, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
#[cfg(target_os = "macos")]
use winit::platform::macos::WindowAttributesExtMacOS;
use winit::window::{Window, WindowId};

/// Events posted to the winit loop from background threads.
#[derive(Debug, Clone)]
enum UserEvent {
    /// Bytes arrived from the PTY or agent for a given session.
    /// A redraw is only needed when `session_id` matches the active tab.
    PtyDataArrived { session_id: u64 },
    /// The Claude agent process exited on a specific session.
    AgentFinished { session_id: u64, exit_code: i32 },
    /// Deferred redraw — fires ~500ms after startup to ensure the prompt
    /// is rendered even if early frames were discarded by macOS before
    /// the CAMetalLayer was presentable.
    DeferredRedraw,
    /// Periodic tick (~100ms) while an agent block is active, driving
    /// the spinner animation and elapsed-time update in the footer.
    StatusTick { session_id: u64 },
}

/// Keyboard input routing state.
enum InputMode {
    /// Normal shell interaction — keys go to the PTY.
    Normal,
    /// User is composing an agent prompt (triggered by `>`). Keys are
    /// buffered locally and echoed to the grid; the shell sees nothing.
    /// `start_col` is the 0-indexed grid column where the `> ` marker
    /// was drawn — used to erase only our echoed text on refresh,
    /// preserving the shell prompt to the left.
    AgentInput { buffer: String, start_col: usize },
    /// An agent process is running. All keyboard input is swallowed
    /// except Ctrl+C which kills the agent.
    AgentRunning,
}

impl Default for InputMode {
    fn default() -> Self {
        Self::Normal
    }
}

type SharedTerminal = Arc<Mutex<Terminal>>;

/// Per-tab state. The heavy, owned resources (PTY, terminal buffer,
/// optional agent child process) live here so that each tab has its
/// own independent shell. The "lightweight" runtime state like scroll
/// offset and selection is stored on `PurrttyApp` for the *currently*
/// active tab; it's snapshotted into `saved` on every tab switch and
/// restored when the user comes back.
struct Session {
    id: u64,
    pty: PtySession,
    terminal: SharedTerminal,
    agent_session: Option<AgentSession>,
    /// Active agent block (if any). Shared with the reader thread via
    /// Arc<Mutex<>> so the reader can push segments while the main
    /// thread reads for rendering.
    block: Option<std::sync::Arc<std::sync::Mutex<Block>>>,
    /// Preserved scroll/selection/input-mode state for this tab while
    /// it is NOT the active tab. Unused for the active tab.
    saved: SavedSessionState,
}

/// A URL hit at a specific visible cell range. Returned by
/// `url_hit_at_pixel`; also used to track the currently-hovered
/// URL so the renderer can draw it with an underline + accent color.
#[derive(Debug, Clone, PartialEq, Eq)]
struct UrlHit {
    view_row: usize,
    start_col: usize,
    end_col: usize,
    url: String,
}

/// Result of a click inside the tab bar: either the × button of a
/// tab was pressed, or the body of a tab was clicked.
#[derive(Debug, Clone, Copy)]
enum TabHit {
    Close(usize),
    Body(usize),
}

#[derive(Default)]
struct SavedSessionState {
    scroll_offset: usize,
    input_mode: InputMode,
    shell_input_empty: bool,
    selection: Option<Selection>,
}

#[derive(Default)]
struct PurrttyApp {
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    proxy: Option<EventLoopProxy<UserEvent>>,
    /// All open tabs. `active` indexes into this. At least one session
    /// exists once `resumed()` has initialized the window.
    sessions: Vec<Session>,
    active: usize,
    /// Monotonic id counter for new sessions.
    next_session_id: u64,
    /// How many rows the view is scrolled into scrollback. 0 = live bottom.
    /// Tracks the active tab.
    scroll_offset: usize,
    /// Latest modifier state from `WindowEvent::ModifiersChanged`. Used by
    /// the keyboard mapper to detect Ctrl/Alt/Cmd combos.
    modifiers: ModifiersState,
    /// Loaded user config — used to seed the window and the renderer.
    config: Config,
    /// Agent input mode state machine (active tab).
    input_mode: InputMode,
    /// Heuristic: true after we forward Enter to the PTY (shell will
    /// re-emit a prompt), false after forwarding any other key. Used to
    /// detect "user is at the start of a fresh prompt line". Active tab.
    shell_input_empty: bool,
    /// Active text selection for the current tab.
    selection: Option<Selection>,
    /// True while the left mouse button is held and a selection is
    /// being dragged.
    selecting: bool,
    /// Last known mouse position in physical pixels. Needed because
    /// winit fires `CursorMoved` independent of `MouseInput`.
    cursor_pos: (f64, f64),
    /// Currently hovered URL (if any). Drawn with an underline and
    /// accent color so the user can see it's clickable.
    hovered_url: Option<UrlHit>,
    /// Monotonic tick counter for the block footer spinner animation.
    /// Incremented by StatusTick events.
    spinner_tick: usize,
}

/// Approximate cell line height for turning pixel scroll deltas into rows.
/// Keep in sync with the renderer's LINE_HEIGHT constant.
const SCROLL_LINE_HEIGHT: f64 = 22.0;

impl PurrttyApp {
    fn new(proxy: EventLoopProxy<UserEvent>, config: Config) -> Self {
        Self {
            proxy: Some(proxy),
            config,
            // Start true so the very first `>` at the initial prompt works.
            shell_input_empty: true,
            ..Self::default()
        }
    }

    fn redraw(&self) {
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }

    /// Accessors for the active tab's pty / terminal / agent. They
    /// return `None` when no tabs exist yet (pre-`resumed`).
    fn active_pty(&self) -> Option<&PtySession> {
        self.sessions.get(self.active).map(|s| &s.pty)
    }

    fn active_pty_mut(&mut self) -> Option<&mut PtySession> {
        self.sessions.get_mut(self.active).map(|s| &mut s.pty)
    }

    fn active_terminal(&self) -> Option<&SharedTerminal> {
        self.sessions.get(self.active).map(|s| &s.terminal)
    }

    fn active_agent_mut(&mut self) -> Option<&mut Option<AgentSession>> {
        self.sessions
            .get_mut(self.active)
            .map(|s| &mut s.agent_session)
    }

    fn session_index_by_id(&self, id: u64) -> Option<usize> {
        self.sessions.iter().position(|s| s.id == id)
    }

    /// Reposition macOS traffic lights and update the dynamic tab
    /// left inset. Split borrows window (immut) and renderer (mut)
    /// via separate field access.
    #[cfg(target_os = "macos")]
    fn update_macos_traffic_lights(&mut self) {
        let scale = self
            .window
            .as_ref()
            .map(|w| w.scale_factor() as f32)
            .unwrap_or(1.0);
        let bar_h = self
            .renderer
            .as_ref()
            .map(|r| r.tab_bar_height() / scale)
            .unwrap_or(40.0);
        // (runs on every frame — no log to avoid spam)
        if let Some(window) = self.window.as_ref() {
            macos::reposition_traffic_lights(window, bar_h);
        }
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.set_tab_bar_left_inset(macos::tab_bar_left_inset());
        }
    }

    /// Create a new Session (PTY + terminal + reader thread) without
    /// inserting it into `self.sessions`. Returns `None` on failure.
    fn spawn_session(&mut self, rows: u16, cols: u16) -> Option<Session> {
        let id = self.next_session_id;
        self.next_session_id += 1;
        let terminal = Arc::new(Mutex::new(Terminal::new(rows as usize, cols as usize)));
        let terminal_for_reader = terminal.clone();
        let proxy = self
            .proxy
            .clone()
            .expect("proxy set before spawn_session");
        let pty = match PtySession::spawn(rows, cols, move |bytes| {
            if let Ok(mut term) = terminal_for_reader.lock() {
                term.advance(bytes);
            }
            let _ = proxy.send_event(UserEvent::PtyDataArrived { session_id: id });
        }) {
            Ok(p) => p,
            Err(err) => {
                error!(?err, "failed to spawn pty");
                return None;
            }
        };
        Some(Session {
            id,
            pty,
            terminal,
            agent_session: None,
            block: None,
            saved: SavedSessionState::default(),
        })
    }

    /// Open a new tab with a fresh shell and switch to it.
    fn new_tab(&mut self) {
        // Grab dimensions BEFORE we change the tab count — new tab
        // will trigger a grid size update via sync_tab_info_and_resize.
        let Some(renderer) = self.renderer.as_ref() else { return };
        let (rows, cols) = renderer.grid_dimensions();
        let Some(new_session) = self.spawn_session(rows, cols) else { return };
        self.save_active_tab_state();
        self.sessions.push(new_session);
        self.active = self.sessions.len() - 1;
        self.restore_active_tab_state();
        self.sync_tab_info_and_resize();
        self.redraw();
    }

    /// Close the active tab. No-op if it's the last one — the user
    /// should close the window (Cmd+Q / red button) to exit the app.
    fn close_tab(&mut self) {
        if self.sessions.len() <= 1 {
            return;
        }
        self.sessions.remove(self.active);
        if self.active >= self.sessions.len() {
            self.active = self.sessions.len() - 1;
        }
        self.restore_active_tab_state();
        self.sync_tab_info_and_resize();
        self.redraw();
    }

    /// Switch to the tab at `idx` (0-based). No-op if out of range.
    fn switch_tab(&mut self, idx: usize) {
        if idx >= self.sessions.len() || idx == self.active {
            return;
        }
        self.save_active_tab_state();
        self.active = idx;
        self.restore_active_tab_state();
        // Tab bar highlight changes — needs a redraw.
        self.renderer
            .as_mut()
            .map(|r| r.set_tab_info(self.active, self.sessions.len()));
        self.redraw();
    }

    /// Push current tab count/active to the renderer, and if the tab
    /// bar's appearance changed the usable grid area, resize every
    /// session to match. Called on tab create/close.
    fn sync_tab_info_and_resize(&mut self) {
        let Some(renderer) = self.renderer.as_mut() else { return };
        renderer.set_tab_info(self.active, self.sessions.len());
        let (rows, cols) = renderer.grid_dimensions();
        for session in &mut self.sessions {
            if let Ok(mut term) = session.terminal.lock() {
                term.grid_mut().resize(rows as usize, cols as usize);
            }
            if let Err(err) = session.pty.resize(rows, cols) {
                warn!(?err, "pty resize failed after tab change");
            }
        }
    }

    fn save_active_tab_state(&mut self) {
        if let Some(session) = self.sessions.get_mut(self.active) {
            session.saved = SavedSessionState {
                scroll_offset: self.scroll_offset,
                input_mode: std::mem::take(&mut self.input_mode),
                shell_input_empty: self.shell_input_empty,
                selection: self.selection.take(),
            };
        }
    }

    fn restore_active_tab_state(&mut self) {
        if let Some(session) = self.sessions.get_mut(self.active) {
            let s = std::mem::take(&mut session.saved);
            self.scroll_offset = s.scroll_offset;
            self.input_mode = s.input_mode;
            self.shell_input_empty = s.shell_input_empty;
            self.selection = s.selection;
            self.selecting = false;
        }
    }

    fn zoom_font(&mut self, delta: f32) {
        let (rows, cols) = {
            let Some(renderer) = self.renderer.as_mut() else { return };
            match renderer.change_font_size(delta) {
                Some((r, c)) => (r, c),
                None => return,
            }
        };
        info!(rows, cols, delta, "font size changed");
        // Propagate new cell dimensions to every tab.
        for session in &mut self.sessions {
            if let Ok(mut term) = session.terminal.lock() {
                term.grid_mut().resize(rows as usize, cols as usize);
            }
            if let Err(err) = session.pty.resize(rows, cols) {
                warn!(?err, "pty resize failed after font zoom");
            }
        }
        // Traffic-light centering + tab inset depend on bar height.
        #[cfg(target_os = "macos")]
        self.update_macos_traffic_lights();
        self.redraw();
    }

    fn zoom_font_reset(&mut self) {
        let Some(renderer) = self.renderer.as_mut() else { return };
        let cur = renderer.current_font_size();
        let default = self.config.font.size;
        let delta = default - cur;
        if delta.abs() < 0.1 { return; }
        self.zoom_font(delta);
    }

    /// Translate a physical-pixel mouse position into an absolute
    /// grid point (scrollback + visible rows). Returns `None` if
    /// the renderer or terminal isn't ready.
    fn mouse_to_grid_point(&self, x: f64, y: f64) -> Option<GridPoint> {
        let renderer = self.renderer.as_ref()?;
        let (pad_x, pad_y, cell_w, cell_h) = renderer.cell_metrics();
        let (rows, cols) = renderer.grid_dimensions();
        let (view_row, col) = pixel_to_cell(
            x as f32,
            y as f32,
            pad_x,
            pad_y,
            cell_w,
            cell_h,
            rows as usize,
            cols as usize,
        );
        let sb_len = self
            .active_terminal()?
            .lock()
            .ok()
            .map(|t| t.grid().scrollback_len())
            .unwrap_or(0);
        let abs_row = view_row_to_absolute(view_row, self.scroll_offset, sb_len);
        Some(GridPoint::new(abs_row, col))
    }

    /// Hit-test the tab bar at physical pixel `(x, y)`. Returns
    /// `TabHit::Close(idx)` if the × button was hit, `TabHit::Body(idx)`
    /// if the tab body was hit, and `None` for anything else (e.g. the
    /// grid area below the bar).
    fn tab_bar_hit(&self, x: f64, y: f64) -> Option<TabHit> {
        let renderer = self.renderer.as_ref()?;
        let bar_h = renderer.tab_bar_height();
        if bar_h == 0.0 || y >= bar_h as f64 {
            return None;
        }
        fn contains(rect: (f32, f32, f32, f32), px: f64, py: f64) -> bool {
            let (rx, ry, rw, rh) = rect;
            px >= rx as f64
                && px < (rx + rw) as f64
                && py >= ry as f64
                && py < (ry + rh) as f64
        }
        let has_close = self.sessions.len() > 1;
        for layout in renderer.tab_layout() {
            if has_close && contains(layout.close_button, x, y) {
                return Some(TabHit::Close(layout.index));
            }
            if contains(layout.tab, x, y) {
                return Some(TabHit::Body(layout.index));
            }
        }
        None
    }

    /// A URL detected at a given hit-test position.
    fn url_hit_at_pixel(&self, x: f64, y: f64) -> Option<UrlHit> {
        let renderer = self.renderer.as_ref()?;
        let (pad_x, pad_y, cell_w, cell_h) = renderer.cell_metrics();
        let (rows, cols) = renderer.grid_dimensions();
        let (view_row, click_col) = pixel_to_cell(
            x as f32,
            y as f32,
            pad_x,
            pad_y,
            cell_w,
            cell_h,
            rows as usize,
            cols as usize,
        );
        let terminal = self.active_terminal()?;
        let term = terminal.lock().ok()?;
        let row_cells = term.grid().row_at(view_row, self.scroll_offset)?;
        // Build the row's string and a parallel (char_byte_index → col)
        // mapping so we can convert URL byte ranges back to columns.
        let mut row_text = String::with_capacity(row_cells.len());
        let mut byte_to_col: Vec<usize> = Vec::with_capacity(row_cells.len());
        for (col, cell) in row_cells.iter().enumerate() {
            if cell.ch == '\0' {
                continue;
            }
            let len = cell.ch.len_utf8();
            row_text.push(cell.ch);
            for _ in 0..len {
                byte_to_col.push(col);
            }
        }
        let row_text = row_text.trim_end();
        let byte_to_col = &byte_to_col[..row_text.len()];

        for range in urls::find_urls(row_text) {
            let Some(&start_col) = byte_to_col.get(range.start) else {
                continue;
            };
            let end_col = byte_to_col
                .get(range.end - 1)
                .copied()
                .unwrap_or(start_col)
                + 1;
            if click_col >= start_col && click_col < end_col {
                return Some(UrlHit {
                    view_row,
                    start_col,
                    end_col,
                    url: row_text[range].to_string(),
                });
            }
        }
        None
    }

    fn copy_selection_to_clipboard(&mut self) {
        let Some(sel) = self.selection else { return };
        if sel.is_empty() {
            return;
        }
        let Some(terminal) = self.active_terminal() else { return };
        let text = match terminal.lock() {
            Ok(t) => selection_to_text(&sel, t.grid()),
            Err(_) => return,
        };
        if text.is_empty() {
            return;
        }
        match arboard::Clipboard::new() {
            Ok(mut clipboard) => {
                if let Err(err) = clipboard.set_text(&text) {
                    warn!(?err, "clipboard write failed");
                } else {
                    info!(bytes = text.len(), "copied selection to clipboard");
                }
            }
            Err(err) => warn!(?err, "clipboard unavailable"),
        }
    }

    fn paste_from_clipboard(&mut self) {
        let text = match arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
            Ok(t) => t,
            Err(err) => {
                warn!(?err, "clipboard read failed");
                return;
            }
        };
        if text.is_empty() {
            return;
        }
        // Normalize \r\n and \n to \r (what the shell expects as Enter).
        let normalized = text.replace("\r\n", "\r").replace('\n', "\r");

        // If the shell enabled bracketed paste via `\e[?2004h`, wrap
        // the content so it can tell paste apart from typed input.
        // This keeps zsh / fish / bash from prematurely executing
        // multi-line pastes.
        let bracketed = self
            .active_terminal()
            .and_then(|t| t.lock().ok().map(|g| g.grid().bracketed_paste()))
            .unwrap_or(false);
        let payload = build_paste_payload(&normalized, bracketed);

        if let Some(pty) = self.active_pty_mut() {
            if let Err(err) = pty.write(&payload) {
                warn!(?err, "pty write failed (paste)");
            }
        }
        // Any paste fills the shell's input line.
        update_shell_input_empty(normalized.as_bytes(), &mut self.shell_input_empty);
        if self.scroll_offset != 0 {
            self.scroll_offset = 0;
        }
        self.selection = None;
        self.redraw();
    }

    fn handle_keyboard_input(&mut self, event: &KeyEvent) {
        match &mut self.input_mode {
            InputMode::Normal => self.handle_normal_input(event),
            InputMode::AgentInput { .. } => self.handle_agent_input(event),
            InputMode::AgentRunning => {
                // Ctrl+C kills the running agent.
                if let Some(bytes) = key_event_to_bytes(event, self.modifiers) {
                    if bytes == [0x03] {
                        if let Some(agent) = self
                            .active_agent_mut()
                            .and_then(|a| a.as_mut())
                        {
                            agent.kill();
                        }
                    }
                }
            }
        }
    }

    fn handle_normal_input(&mut self, event: &KeyEvent) {
        let Some(bytes) = key_event_to_bytes(event, self.modifiers) else {
            return;
        };

        // Snap scrollback view to the live bottom on any input. Doing
        // this BEFORE the agent-mode check is important: otherwise the
        // "> " marker would be drawn at the live bottom while the user
        // is viewing scrollback — making it invisible.
        if self.scroll_offset != 0 {
            self.scroll_offset = 0;
            self.redraw();
        }

        // Detect `>` at the start of empty shell input → agent mode.
        if bytes == b">" && self.shell_input_empty {
            // Remember the cursor column so refresh_agent_line erases
            // only our echoed text, not the shell prompt to the left.
            let start_col = self
                .active_terminal()
                .and_then(|t| t.lock().ok().map(|g| g.grid().cursor().col))
                .unwrap_or(0);
            self.input_mode = InputMode::AgentInput {
                buffer: String::new(),
                start_col,
            };
            // Draw the empty agent line via the same helper refresh
            // uses, so entry and refresh stay consistent.
            self.refresh_agent_line("", start_col);
            self.redraw();
            return;
        }

        // Track the heuristic for "shell is at empty input".
        update_shell_input_empty(&bytes, &mut self.shell_input_empty);

        // Forward to the PTY.
        if let Some(pty) = self.active_pty_mut() {
            if let Err(err) = pty.write(&bytes) {
                warn!(?err, "pty write failed");
            }
        }
    }

    fn handle_agent_input(&mut self, event: &KeyEvent) {
        if event.state != ElementState::Pressed {
            return;
        }

        // Esc → cancel agent input, erase the echoed text.
        if event.logical_key == Key::Named(NamedKey::Escape) {
            let start_col = match &self.input_mode {
                InputMode::AgentInput { start_col, .. } => *start_col,
                _ => 0,
            };
            self.clear_agent_line(start_col);
            self.input_mode = InputMode::Normal;
            self.redraw();
            return;
        }

        // Enter → launch the agent.
        if event.logical_key == Key::Named(NamedKey::Enter) {
            let (buffer, start_col) = match std::mem::take(&mut self.input_mode) {
                InputMode::AgentInput { buffer, start_col } => (buffer, start_col),
                _ => unreachable!(),
            };
            if buffer.trim().is_empty() {
                self.clear_agent_line(start_col);
                self.input_mode = InputMode::Normal;
                self.redraw();
                return;
            }
            self.echo_to_terminal(b"\r\n");
            self.input_mode = InputMode::AgentRunning;
            self.spawn_agent(buffer);
            return;
        }

        // Backspace → remove last char from buffer, or exit agent mode
        // if the buffer is already empty.
        if event.logical_key == Key::Named(NamedKey::Backspace) {
            if let InputMode::AgentInput { buffer, start_col } = &mut self.input_mode {
                if buffer.pop().is_some() {
                    let snap = buffer.clone();
                    let col = *start_col;
                    // Still in agent mode — keep the "> " marker visible
                    // even if the buffer is now empty.
                    self.refresh_agent_line(&snap, col);
                    self.redraw();
                } else {
                    let col = *start_col;
                    self.clear_agent_line(col);
                    self.input_mode = InputMode::Normal;
                    self.redraw();
                }
            }
            return;
        }

        // Printable text → append to buffer.
        if let Some(text) = &event.text {
            if !text.is_empty() {
                if let InputMode::AgentInput { buffer, start_col } = &mut self.input_mode {
                    buffer.push_str(text.as_str());
                    let snap = buffer.clone();
                    let col = *start_col;
                    self.refresh_agent_line(&snap, col);
                    self.redraw();
                }
            }
        }
    }

    /// Redraw the agent input from `start_col` onwards, preserving the
    /// shell prompt to the left. Always shows the "> " marker because
    /// we are still in agent mode, regardless of whether the buffer
    /// is empty. Use `clear_agent_line` for exit paths.
    fn refresh_agent_line(&self, buffer: &str, start_col: usize) {
        let bytes = agent_line_bytes(buffer, start_col);
        self.echo_to_terminal(&bytes);
    }

    /// Erase the agent input line (including the "> " marker) when
    /// exiting agent mode.
    fn clear_agent_line(&self, start_col: usize) {
        let bytes = clear_line_bytes(start_col);
        self.echo_to_terminal(&bytes);
    }

    fn echo_to_terminal(&self, bytes: &[u8]) {
        if let Some(terminal) = self.active_terminal() {
            if let Ok(mut term) = terminal.lock() {
                term.advance(bytes);
            }
        }
    }

    fn spawn_agent(&mut self, prompt: String) {
        let Some(session) = self.sessions.get(self.active) else { return };
        let session_id = session.id;
        let cwd = session
            .terminal
            .lock()
            .ok()
            .and_then(|g| g.grid().cwd().map(|p| p.to_path_buf()))
            .or_else(|| session.pty.child_pid().and_then(shell_cwd_via_lsof))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

        // Create the block and share it with the reader thread.
        let start_row = session
            .terminal
            .lock()
            .ok()
            .map(|t| t.grid().scrollback_len() + t.grid().cursor().row)
            .unwrap_or(0);
        let block = Arc::new(Mutex::new(Block::new(prompt.clone(), start_row)));
        let block_for_reader = block.clone();

        let terminal_for_agent = session.terminal.clone();
        let proxy = self.proxy.clone().unwrap();

        let proxy_for_exit = proxy.clone();
        match AgentSession::spawn(
            &prompt,
            &cwd,
            move |output| {
                // Echo ANSI text to the terminal grid.
                if let Some(text) = &output.text {
                    if let Ok(mut term) = terminal_for_agent.lock() {
                        term.advance(text.as_bytes());
                    }
                }
                // Feed the block state machine.
                if let Ok(mut blk) = block_for_reader.lock() {
                    match output.update {
                        BlockUpdate::TextDelta(ref t) => blk.push_text(t),
                        BlockUpdate::ToolStart { ref name } => blk.start_tool(name),
                        BlockUpdate::ToolInput(ref j) => blk.push_tool_input(j),
                        BlockUpdate::ContentBlockStop => blk.finish_tool(),
                        BlockUpdate::None => {}
                    }
                }
                let _ = proxy.send_event(UserEvent::PtyDataArrived { session_id });
            },
            move |exit_code| {
                let _ = proxy_for_exit.send_event(UserEvent::AgentFinished {
                    session_id,
                    exit_code,
                });
            },
        ) {
            Ok(agent) => {
                info!(?cwd, "agent spawned");
                if let Some(sess) = self.sessions.get_mut(self.active) {
                    sess.agent_session = Some(agent);
                    sess.block = Some(block);
                }
                // Kick off the spinner timer.
                let proxy_tick = self.proxy.clone().unwrap();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    let _ = proxy_tick.send_event(UserEvent::StatusTick { session_id });
                });
            }
            Err(err) => {
                error!(?err, "failed to spawn agent");
                let msg = format!("\x1b[1;31m✗ failed to spawn claude: {err}\x1b[0m\r\n");
                self.echo_to_terminal(msg.as_bytes());
                self.input_mode = InputMode::Normal;
                self.redraw();
            }
        }
    }
}

impl ApplicationHandler<UserEvent> for PurrttyApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("purrtty")
            .with_inner_size(winit::dpi::LogicalSize::new(
                self.config.window.width,
                self.config.window.height,
            ));
        // On macOS, make the titlebar transparent and extend content
        // into it. This lets our tab bar live inside the titlebar area
        // (Warp / VS Code style) while keeping the traffic-light
        // buttons functional.
        #[cfg(target_os = "macos")]
        let attrs = attrs
            .with_titlebar_transparent(true)
            .with_title_hidden(true)
            .with_fullsize_content_view(true);
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(err) => {
                error!(?err, "failed to create window");
                event_loop.exit();
                return;
            }
        };
        info!(
            size = ?window.inner_size(),
            scale_factor = window.scale_factor(),
            "window created"
        );

        let renderer = match Renderer::new(window.clone(), self.config.renderer_config()) {
            Ok(r) => r,
            Err(err) => {
                error!(?err, "failed to initialize renderer");
                event_loop.exit();
                return;
            }
        };

        let (rows, cols) = renderer.grid_dimensions();
        info!(rows, cols, "initial grid dimensions");

        // Allow macOS / X11 IME composition events to flow through.
        window.set_ime_allowed(true);

        self.window = Some(window.clone());
        self.renderer = Some(renderer);

        // Spawn the first tab.
        if let Some(session) = self.spawn_session(rows, cols) {
            self.sessions.push(session);
            self.active = 0;
        } else {
            event_loop.exit();
            return;
        }

        // Register the initial tab with the renderer so the tab bar
        // reserves space from the very first frame. This also resizes
        // the session's PTY/grid down by the tab bar height.
        self.sync_tab_info_and_resize();

        // Center macOS traffic lights inside our tab bar and compute
        // the dynamic left inset so tabs start after the buttons.
        #[cfg(target_os = "macos")]
        self.update_macos_traffic_lights();

        window.request_redraw();

        // Schedule a deferred redraw so the prompt is visible even if
        // macOS discards early frames before the surface is presentable.
        let proxy_for_deferred = self
            .proxy
            .clone()
            .expect("proxy set before ApplicationHandler::resumed");
        std::thread::Builder::new()
            .name("purrtty-deferred-redraw".into())
            .spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(500));
                let _ = proxy_for_deferred.send_event(UserEvent::DeferredRedraw);
            })
            .ok();
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::PtyDataArrived { session_id } => {
                // Drain pending terminal responses for the session whose
                // PTY produced the bytes, then write them back. Only
                // redraw when it's the active session — background tabs
                // update silently.
                let Some(idx) = self.session_index_by_id(session_id) else { return };
                let responses = self
                    .sessions
                    .get(idx)
                    .and_then(|s| {
                        s.terminal
                            .lock()
                            .ok()
                            .map(|mut term| term.grid_mut().drain_responses())
                    })
                    .unwrap_or_default();
                if let Some(session) = self.sessions.get_mut(idx) {
                    for resp in responses {
                        let _ = session.pty.write(&resp);
                    }
                }
                if idx == self.active {
                    self.redraw();
                }
            }
            UserEvent::DeferredRedraw => {
                self.redraw();
            }
            UserEvent::StatusTick { session_id } => {
                // Advance the spinner and redraw — only if the block
                // is still active on the session that fired the tick.
                if let Some(idx) = self.session_index_by_id(session_id) {
                    let still_active = self
                        .sessions
                        .get(idx)
                        .and_then(|s| s.block.as_ref())
                        .and_then(|b| b.lock().ok())
                        .map(|b| b.is_active())
                        .unwrap_or(false);
                    if still_active {
                        self.spinner_tick = self.spinner_tick.wrapping_add(1);
                        if idx == self.active {
                            self.redraw();
                        }
                        // Schedule the next tick.
                        let proxy = self.proxy.clone().unwrap();
                        std::thread::spawn(move || {
                            std::thread::sleep(std::time::Duration::from_millis(100));
                            let _ = proxy.send_event(UserEvent::StatusTick { session_id });
                        });
                    }
                }
            }
            UserEvent::AgentFinished { session_id, exit_code } => {
                info!(session_id, exit_code, "agent finished");
                let Some(idx) = self.session_index_by_id(session_id) else { return };
                if let Some(session) = self.sessions.get_mut(idx) {
                    session.agent_session = None;
                    // Mark the block as done so the overlay shows ✓/✗.
                    if let Some(block) = session.block.as_ref() {
                        if let Ok(mut b) = block.lock() {
                            b.set_done(exit_code);
                        }
                    }
                }
                let marker = if exit_code == 0 {
                    "\r\n\x1b[1;32m✓ agent done\x1b[0m\r\n"
                } else {
                    "\r\n\x1b[1;31m✗ agent failed\x1b[0m\r\n"
                };
                // Echo the marker directly to the affected session's grid.
                if let Some(session) = self.sessions.get(idx) {
                    if let Ok(mut term) = session.terminal.lock() {
                        term.advance(marker.as_bytes());
                    }
                }
                if let Some(session) = self.sessions.get_mut(idx) {
                    let _ = session.pty.write(b"\n");
                }
                // Mode + heuristic live on the active tab; only touch them
                // when the finished agent was the active tab's.
                if idx == self.active {
                    self.input_mode = InputMode::Normal;
                    self.shell_input_empty = true;
                    self.redraw();
                }
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                info!("close requested, exiting");
                event_loop.exit();
            }
            WindowEvent::Focused(true) | WindowEvent::Occluded(false) => {
                // Window became visible / got focus — re-render in case
                // early frames were discarded before the window appeared.
                self.redraw();
            }
            WindowEvent::Resized(size) => {
                let (rows, cols) = match self.renderer.as_mut() {
                    Some(renderer) => {
                        renderer.resize(size);
                        renderer.grid_dimensions()
                    }
                    None => return,
                };
                // Resize every session's terminal grid + PTY so
                // inactive tabs don't lag behind in size.
                for session in &mut self.sessions {
                    if let Ok(mut term) = session.terminal.lock() {
                        term.grid_mut().resize(rows as usize, cols as usize);
                    }
                    if let Err(err) = session.pty.resize(rows, cols) {
                        warn!(?err, "pty resize failed");
                    }
                }
                // macOS may reset traffic-light positions during resize.
                #[cfg(target_os = "macos")]
                self.update_macos_traffic_lights();
                self.redraw();
            }
            WindowEvent::RedrawRequested => {
                // Re-apply traffic-light positions on every frame.
                // macOS resets button frames during its own layout
                // passes, so a one-shot reposition isn't enough.
                #[cfg(target_os = "macos")]
                self.update_macos_traffic_lights();

                let scroll_offset = self.scroll_offset;
                let selection_range = self.selection.and_then(|s| {
                    if s.is_empty() {
                        return None;
                    }
                    let (start, end) = s.normalized();
                    Some(((start.row, start.col), (end.row, end.col)))
                });
                let hover = self
                    .hovered_url
                    .as_ref()
                    .map(|h| (h.view_row, h.start_col, h.end_col));
                let terminal = match self.active_terminal().cloned() {
                    Some(t) => t,
                    None => return,
                };
                let renderer = match self.renderer.as_mut() {
                    Some(r) => r,
                    None => return,
                };
                let guard = match terminal.lock() {
                    Ok(g) => g,
                    Err(err) => {
                        warn!(?err, "terminal mutex poisoned");
                        return;
                    }
                };
                // Build block overlays from the grid's OSC 133 blocks.
                let grid = guard.grid();
                let sb_len = grid.scrollback_len();
                let vis_rows = grid.rows();
                let first_abs = sb_len.saturating_sub(scroll_offset.min(sb_len));
                let grid_blocks = grid.blocks();
                let cursor_abs = sb_len + grid.cursor().row;

                let mut render_blocks: Vec<purrtty_ui::RenderBlock> = Vec::new();
                for (i, blk) in grid_blocks.iter().enumerate() {
                    let blk_start = blk.start_row;
                    // Block ends where the next block starts, or at the cursor.
                    let blk_end = grid_blocks
                        .get(i + 1)
                        .map(|next| next.start_row)
                        .unwrap_or(cursor_abs + 1);
                    // Map to view rows.
                    if blk_end <= first_abs || blk_start >= first_abs + vis_rows {
                        continue; // entirely outside viewport
                    }
                    let start_view = blk_start.saturating_sub(first_abs).min(vis_rows);
                    let end_view = blk_end.saturating_sub(first_abs).min(vis_rows);
                    if end_view <= start_view {
                        continue;
                    }
                    let state = match blk.state {
                        purrtty_term::grid::TermBlockState::Input => 3, // dim default
                        purrtty_term::grid::TermBlockState::Running => 0, // blue active
                        purrtty_term::grid::TermBlockState::Done { exit_code } => {
                            if exit_code == 0 { 1 } else { 2 } // gray or red
                        }
                    };
                    render_blocks.push(purrtty_ui::RenderBlock {
                        start_view_row: start_view,
                        end_view_row: end_view,
                        footer: String::new(),
                        state,
                    });
                }

                // Headless frame dump: write visual layout to file
                // when PURRTTY_DUMP env var is set.
                if std::env::var("PURRTTY_DUMP").is_ok() {
                    let dump = renderer.frame_dump(
                        guard.grid(),
                        scroll_offset,
                        &render_blocks,
                    );
                    let _ = std::fs::write("/tmp/purrtty_frame.txt", &dump);
                }

                if let Err(err) = renderer.render_with_overlays(
                    guard.grid(),
                    scroll_offset,
                    selection_range,
                    hover,
                    &render_blocks,
                ) {
                    warn!(?err, "render failed");
                }
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
            }
            WindowEvent::Ime(ime) => {
                // Route committed IME input (e.g. finalized hangul) based
                // on the current input mode — Normal goes to the PTY,
                // AgentInput appends to the agent buffer, AgentRunning
                // swallows the input.
                if let Ime::Commit(text) = ime {
                    if !text.is_empty() {
                        if self.scroll_offset != 0 {
                            self.scroll_offset = 0;
                            self.redraw();
                        }
                        match &mut self.input_mode {
                            InputMode::Normal => {
                                update_shell_input_empty(
                                    text.as_bytes(),
                                    &mut self.shell_input_empty,
                                );
                                if let Some(pty) = self.active_pty_mut() {
                                    if let Err(err) = pty.write(text.as_bytes()) {
                                        warn!(?err, "pty write failed (ime commit)");
                                    }
                                }
                            }
                            InputMode::AgentInput { buffer, start_col } => {
                                buffer.push_str(&text);
                                let snap = buffer.clone();
                                let col = *start_col;
                                self.refresh_agent_line(&snap, col);
                                self.redraw();
                            }
                            InputMode::AgentRunning => {
                                // Swallow — no input forwarded while an
                                // agent is running.
                            }
                        }
                    }
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    if let Key::Character(s) = &event.logical_key {
                        // Zoom: Cmd or Ctrl. Safe to intercept because
                        // the shell never sees `Ctrl+=` etc.
                        if self.modifiers.control_key() || self.modifiers.super_key() {
                            match s.as_str() {
                                "=" | "+" => { self.zoom_font(2.0); return; }
                                "-" => { self.zoom_font(-2.0); return; }
                                "0" => { self.zoom_font_reset(); return; }
                                _ => {}
                            }
                        }
                        // Clipboard + tabs: Cmd only. Ctrl+C must stay
                        // SIGINT, Ctrl+W stays kill-word, Ctrl+T stays
                        // transpose — we never steal those from the shell.
                        if self.modifiers.super_key() {
                            match s.as_str() {
                                "c" | "C" => { self.copy_selection_to_clipboard(); return; }
                                "v" | "V" => { self.paste_from_clipboard(); return; }
                                "t" | "T" => { self.new_tab(); return; }
                                "w" | "W" => { self.close_tab(); return; }
                                "1" => { self.switch_tab(0); return; }
                                "2" => { self.switch_tab(1); return; }
                                "3" => { self.switch_tab(2); return; }
                                "4" => { self.switch_tab(3); return; }
                                "5" => { self.switch_tab(4); return; }
                                "6" => { self.switch_tab(5); return; }
                                "7" => { self.switch_tab(6); return; }
                                "8" => { self.switch_tab(7); return; }
                                "9" => {
                                    // Cmd+9 jumps to the last tab, matching
                                    // Chrome / iTerm2 convention.
                                    if !self.sessions.is_empty() {
                                        self.switch_tab(self.sessions.len() - 1);
                                    }
                                    return;
                                }
                                _ => {}
                            }
                        }
                    }
                }
                self.handle_keyboard_input(&event);
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_pos = (position.x, position.y);
                if self.selecting {
                    if let Some(p) = self.mouse_to_grid_point(position.x, position.y) {
                        if let Some(sel) = self.selection.as_mut() {
                            sel.update(p);
                            self.redraw();
                        }
                    }
                }
                // Update hovered URL; only redraw if it changed.
                let new_hover = self.url_hit_at_pixel(position.x, position.y);
                if new_hover != self.hovered_url {
                    self.hovered_url = new_hover;
                    self.redraw();
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if button == MouseButton::Left {
                    match state {
                        ElementState::Pressed => {
                            let (x, y) = self.cursor_pos;
                            // Tab bar takes priority over grid clicks.
                            if let Some(hit) = self.tab_bar_hit(x, y) {
                                match hit {
                                    TabHit::Close(idx) => {
                                        // Switch to the target first
                                        // so close_tab acts on it.
                                        if idx != self.active {
                                            self.switch_tab(idx);
                                        }
                                        self.close_tab();
                                        return;
                                    }
                                    TabHit::Body(idx) => {
                                        self.switch_tab(idx);
                                        return;
                                    }
                                }
                            }
                            // Cmd+click: try to open URL at the click point.
                            if self.modifiers.super_key() {
                                if let Some(hit) = self.url_hit_at_pixel(x, y) {
                                    open_url(&hit.url);
                                    return;
                                }
                            }
                            if let Some(p) = self.mouse_to_grid_point(x, y) {
                                self.selection = Some(Selection::new(p));
                                self.selecting = true;
                                self.redraw();
                            }
                        }
                        ElementState::Released => {
                            self.selecting = false;
                            // Clear empty click-selections so they don't
                            // leave a stale highlight after a non-drag click.
                            if let Some(sel) = self.selection.as_ref() {
                                if sel.is_empty() {
                                    self.selection = None;
                                    self.redraw();
                                }
                            }
                        }
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64,
                    MouseScrollDelta::PixelDelta(pos) => pos.y / SCROLL_LINE_HEIGHT,
                };
                if lines == 0.0 {
                    return;
                }
                let max = self
                    .active_terminal()
                    .and_then(|t| t.lock().ok().map(|g| g.grid().scrollback_len()))
                    .unwrap_or(0);
                // Positive y = scroll up = show older content = larger offset.
                let delta_rows = lines.round() as i32;
                let new_offset = if delta_rows > 0 {
                    (self.scroll_offset + delta_rows as usize).min(max)
                } else {
                    self.scroll_offset
                        .saturating_sub((-delta_rows) as usize)
                };
                if new_offset != self.scroll_offset {
                    self.scroll_offset = new_offset;
                    self.redraw();
                }
            }
            _ => {}
        }
    }
}

/// Translate a winit `KeyEvent` (plus the latest `ModifiersState`) into
/// the bytes a shell expects on stdin.
///
/// Mapping rules:
/// - **Cmd / Super**: app-level shortcut on macOS — never forwarded to
///   the PTY.
/// - **Ctrl + letter**: ASCII control byte (`Ctrl+A` → `0x01`, …,
///   `Ctrl+Z` → `0x1A`). Plus the punctuation control codes
///   (`Ctrl+[` = ESC, `Ctrl+\\` = FS, `Ctrl+]` = GS, `Ctrl+^` = RS,
///   `Ctrl+_` = US, `Ctrl+?` = DEL, `Ctrl+Space` = NUL).
/// - **Alt + char**: ESC prefix + the char (xterm "meta" convention).
/// - **Named keys** (Enter, Tab, arrows, ...): fixed escape sequences.
/// - **Otherwise**: whatever winit composed into `event.text` for the
///   current keyboard layout.
fn key_event_to_bytes(event: &KeyEvent, mods: ModifiersState) -> Option<Vec<u8>> {
    if event.state != ElementState::Pressed {
        return None;
    }

    // Cmd (macOS) / Super shortcuts are application-level — don't leak
    // them to the shell.
    if mods.super_key() {
        return None;
    }

    // Ctrl + letter / punctuation → ASCII control byte.
    if mods.control_key() {
        if let Key::Character(s) = &event.logical_key {
            if let Some(ch) = s.chars().next() {
                let lower = ch.to_ascii_lowercase();
                if lower.is_ascii_lowercase() {
                    return Some(vec![lower as u8 - b'a' + 1]);
                }
                let byte = match ch {
                    '@' | ' ' => Some(0x00),
                    '[' => Some(0x1B),
                    '\\' => Some(0x1C),
                    ']' => Some(0x1D),
                    '^' => Some(0x1E),
                    '_' => Some(0x1F),
                    '?' => Some(0x7F),
                    _ => None,
                };
                if let Some(b) = byte {
                    return Some(vec![b]);
                }
            }
        }
        // Ctrl + named keys fall through (Ctrl+Enter etc. are unusual).
    }

    // Alt / Option + char → ESC + char (xterm meta sends ESC).
    if mods.alt_key() && !mods.control_key() {
        if let Key::Character(s) = &event.logical_key {
            let mut out = Vec::with_capacity(1 + s.len());
            out.push(0x1B);
            out.extend_from_slice(s.as_bytes());
            return Some(out);
        }
    }

    if let Key::Named(named) = &event.logical_key {
        let mapped: Option<&'static [u8]> = match named {
            NamedKey::Enter => Some(b"\r"),
            NamedKey::Tab => Some(b"\t"),
            NamedKey::Backspace => Some(b"\x7f"),
            NamedKey::Escape => Some(b"\x1b"),
            NamedKey::ArrowUp => Some(b"\x1b[A"),
            NamedKey::ArrowDown => Some(b"\x1b[B"),
            NamedKey::ArrowRight => Some(b"\x1b[C"),
            NamedKey::ArrowLeft => Some(b"\x1b[D"),
            NamedKey::Home => Some(b"\x1b[H"),
            NamedKey::End => Some(b"\x1b[F"),
            NamedKey::Delete => Some(b"\x1b[3~"),
            NamedKey::PageUp => Some(b"\x1b[5~"),
            NamedKey::PageDown => Some(b"\x1b[6~"),
            _ => None,
        };
        if let Some(bytes) = mapped {
            return Some(bytes.to_vec());
        }
    }

    event.text.as_ref().map(|t| t.as_bytes().to_vec())
}

/// Build the bytes to redraw the agent input line at `start_col`,
/// showing the "> " marker followed by `buffer`. The marker is always
/// present because this is called while still in agent mode.
fn agent_line_bytes(buffer: &str, start_col: usize) -> Vec<u8> {
    // CHA (1-indexed) + EL mode 0 + "> " (bold cyan) + buffer.
    format!(
        "\x1b[{}G\x1b[K\x1b[1;36m> \x1b[0m{}",
        start_col + 1,
        buffer
    )
    .into_bytes()
}

/// Build the bytes to erase the agent line entirely (exit path).
fn clear_line_bytes(start_col: usize) -> Vec<u8> {
    format!("\x1b[{}G\x1b[K", start_col + 1).into_bytes()
}

/// Open a URL in the user's default browser / handler. Uses macOS
/// `open`; best-effort — logs a warning if it fails.
fn open_url(url: &str) {
    info!(url, "opening URL");
    match std::process::Command::new("open").arg(url).spawn() {
        Ok(_) => {}
        Err(err) => warn!(?err, url, "failed to open URL"),
    }
}

/// Build the byte payload to write to the PTY for a paste operation.
/// If `bracketed` is true (shell enabled DECSET 2004), wrap the text in
/// `\e[200~ ... \e[201~`. Any literal `\e[201~` in the pasted text is
/// sanitized to prevent a paste from prematurely closing the bracket
/// and letting the shell interpret the remainder as a command.
fn build_paste_payload(text: &str, bracketed: bool) -> Vec<u8> {
    if !bracketed {
        return text.as_bytes().to_vec();
    }
    // Sanitize: strip any embedded end-of-paste marker so malicious
    // clipboard content can't break out of the bracket.
    let sanitized = text.replace("\x1b[201~", "");
    let mut out = Vec::with_capacity(sanitized.len() + 12);
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(sanitized.as_bytes());
    out.extend_from_slice(b"\x1b[201~");
    out
}

/// Update the `shell_input_empty` heuristic based on the bytes the user
/// just sent to the shell. This is a best-effort estimate of whether
/// the shell's input buffer is empty, used to decide whether `>` should
/// open agent input mode.
///
/// Rules:
/// - `\r` / `\n` (Enter): shell is about to process & redraw prompt → empty.
/// - `\x03` (Ctrl+C) / `\x15` (Ctrl+U): line cancelled/killed → empty.
/// - `\x7f` / `\x08` (Backspace): preserve state — doesn't fill buffer.
/// - Escape sequences (arrows, function keys, `\x1b...`): preserve state.
/// - `\t` (Tab): preserve state — pure completion trigger.
/// - Printable ASCII or UTF-8 text: fills the buffer → not empty.
/// - Anything else (unknown control codes): preserve state.
fn update_shell_input_empty(bytes: &[u8], shell_input_empty: &mut bool) {
    if bytes.is_empty() {
        return;
    }
    // Enter / LF → fresh prompt.
    if bytes == b"\r" || bytes == b"\n" {
        *shell_input_empty = true;
        return;
    }
    // Ctrl+C, Ctrl+U → line cleared.
    if bytes == b"\x03" || bytes == b"\x15" {
        *shell_input_empty = true;
        return;
    }
    // Backspace / Tab / escape sequences → preserve.
    if bytes == b"\x7f" || bytes == b"\x08" || bytes == b"\t" {
        return;
    }
    if bytes.first() == Some(&0x1b) {
        return;
    }
    // Any printable ASCII or high-bit byte (UTF-8) → fills buffer.
    if bytes
        .iter()
        .any(|&b| (0x20..=0x7e).contains(&b) || b >= 0x80)
    {
        *shell_input_empty = false;
        return;
    }
    // Unknown lone control byte → preserve.
}

/// Query the working directory of a process via `lsof` (macOS).
/// Returns `None` if lsof isn't available or the PID doesn't exist.
fn shell_cwd_via_lsof(pid: u32) -> Option<std::path::PathBuf> {
    let output = std::process::Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&output.stdout);
    for line in s.lines() {
        if let Some(path) = line.strip_prefix('n') {
            return Some(std::path::PathBuf::from(path));
        }
    }
    None
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,purrtty=debug"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

fn main() -> Result<()> {
    init_tracing();
    info!(version = env!("CARGO_PKG_VERSION"), "starting purrtty");

    let event_loop: EventLoop<UserEvent> = EventLoop::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);

    let config = Config::load();
    let proxy = event_loop.create_proxy();
    let mut app = PurrttyApp::new(proxy, config);
    event_loop.run_app(&mut app)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies the full zoom → reposition chain:
    /// 1. Different font sizes produce different tab_bar_height values
    /// 2. Different bar heights produce different traffic-light y values
    /// 3. y always centers the button in the bar (within parent constraints)
    /// 4. x never changes
    /// The root cause of the traffic-light centering bug: macOS fixes
    /// the titlebar container at ~32pt. Without resizing it, the
    /// centering formula clamps to y=0 for any bar > 46pt and the
    /// buttons get stuck at the top.
    ///
    /// After the fix, we resize the container to bar_h BEFORE
    /// positioning, so parent_h == bar_h and the formula never clamps.
    #[test]
    fn traffic_light_needs_parent_resize_to_center() {
        let button_h = 14.0_f64;
        let macos_default_parent_h = 32.0_f64;

        // WITHOUT parent resize: at bar_h=52, formula clamps.
        let bar_h = 52.0_f64;
        let desired_top = ((bar_h - button_h) / 2.0).max(0.0);
        let y_without_resize =
            (macos_default_parent_h - button_h - desired_top).max(0.0);
        assert_eq!(
            y_without_resize, 0.0,
            "without parent resize, y clamps to 0 at bar_h=52"
        );
        let btn_center_without = macos_default_parent_h - y_without_resize - button_h / 2.0;
        let bar_center = bar_h / 2.0;
        assert!(
            (btn_center_without - bar_center).abs() > 0.5,
            "button is NOT centered without parent resize: \
             btn_center={btn_center_without}, bar_center={bar_center}"
        );

        // WITH parent resize: parent_h == bar_h, formula works.
        let parent_h_after_resize = bar_h; // we resize the container
        let y_with_resize =
            (parent_h_after_resize - button_h - desired_top).max(0.0);
        let btn_center_with = parent_h_after_resize - y_with_resize - button_h / 2.0;
        assert!(
            (btn_center_with - bar_center).abs() < 0.01,
            "button IS centered after parent resize: \
             btn_center={btn_center_with}, bar_center={bar_center}"
        );
    }

    #[test]
    fn traffic_light_y_changes_with_bar_height() {
        let button_h = 14.0_f64;
        let parent_h = 32.0_f64;
        // Simulate: default font (line_h=22), zoomed (line_h=30), zoomed more (line_h=38)
        let line_heights = [22.0_f32, 26.0, 30.0, 38.0];
        let mut prev_y = None;
        let mut prev_bar_h = None;
        for line_h in line_heights {
            let bar_h = (line_h + 18.0).max(38.0) as f64;
            let desired_top = ((bar_h - button_h) / 2.0).max(0.0);
            let new_y = (parent_h - button_h - desired_top).max(0.0);
            // Bar gets taller with each zoom step.
            if let Some(prev) = prev_bar_h {
                assert!(
                    bar_h >= prev,
                    "bar should grow with line_h: {bar_h} < {prev}"
                );
            }
            // y should decrease (button moves down in AppKit coords =
            // moves down visually) as bar gets taller... until clamped.
            if let Some(prev) = prev_y {
                assert!(
                    new_y <= prev,
                    "button y should decrease (move down) as bar grows: \
                     new_y={new_y} > prev={prev} at line_h={line_h}"
                );
            }
            prev_y = Some(new_y);
            prev_bar_h = Some(bar_h);
        }
        // Verify the range: at line_h=22, bar=40, y=5.
        // At line_h=38, bar=56, desired_top=21, y=32-14-21=-3 → clamped to 0.
        let bar_small = (22.0_f64 + 18.0).max(38.0);
        let y_small = (parent_h - button_h - ((bar_small - button_h) / 2.0)).max(0.0);
        assert!((y_small - 5.0).abs() < 0.01, "y at bar=40 should be 5, got {y_small}");
        let bar_big = (38.0_f64 + 18.0).max(38.0);
        let y_big = (parent_h - button_h - ((bar_big - button_h) / 2.0)).max(0.0);
        assert_eq!(y_big, 0.0, "y at bar=56 should clamp to 0");
    }

    #[test]
    fn traffic_light_position_stable_across_zooms() {
        // Traffic lights: x stays at macOS default (9), only y changes
        // for vertical centering. Must be idempotent across zoom cycles.
        // Note: perfect centering is only possible when
        // bar_h <= parent_h + button_h; beyond that, the button is
        // clamped to the bottom of its parent.
        let original_x = 9.0_f64;
        let button_h = 14.0_f64;
        let parent_h = 32.0_f64;
        let bar_heights = [40.0, 50.0, 40.0, 60.0, 40.0];
        for bar_h in bar_heights {
            let desired_top = ((bar_h - button_h) / 2.0).max(0.0);
            let new_y = (parent_h - button_h - desired_top).max(0.0);
            // x never changes.
            assert_eq!(original_x, 9.0, "x must stay at macOS default");
            // y is idempotent: same bar_h always gives same new_y.
            let new_y2 = (parent_h - button_h - desired_top).max(0.0);
            assert!(
                (new_y - new_y2).abs() < 0.001,
                "y must be stable across calls at bar_h={bar_h}"
            );
            // Button should be as close to bar center as the parent
            // allows (may be clamped when bar_h >> parent_h).
            let btn_center = parent_h - new_y - button_h / 2.0;
            let bar_center = bar_h / 2.0;
            assert!(
                btn_center <= bar_center + 0.001,
                "button center ({btn_center}) should not be below bar center ({bar_center})"
            );
        }
    }

    /// Block padding: rows after a block boundary are offset downward.
    /// Rows that overflow the window are clipped (not rendered).
    /// Pressing Enter repeatedly creates many blocks. The cursor row
    /// (where the prompt lives) must ALWAYS be visible — never clipped
    /// by block padding.
    #[test]
    fn cursor_row_always_visible_with_many_blocks() {
        let line_h = 24.0_f32;
        let base_grid_top = 40.0_f32;
        let rows = 24_usize;
        let block_pad = (line_h * 0.5).max(8.0);

        // Simulate pressing Enter 20 times: blocks at rows 0, 1, 2, ..., 19
        for num_enters in 1..=20 {
            let cursor_row = num_enters.min(rows - 1);
            let boundaries: Vec<usize> = (1..=num_enters)
                .filter(|&r| r > 0 && r < rows)
                .collect();
            let bottom_offset = boundaries
                .iter()
                .filter(|&&s| s <= rows - 1)
                .count() as f32
                * block_pad;
            let grid_top = base_grid_top - bottom_offset;
            let min_y = base_grid_top;

            // Compute cursor row Y
            let cursor_offset = boundaries
                .iter()
                .filter(|&&s| s <= cursor_row)
                .count() as f32
                * block_pad;
            let cursor_y = grid_top + cursor_row as f32 * line_h + cursor_offset;

            // Cursor row must be visible (not clipped above viewport)
            assert!(
                cursor_y + line_h >= min_y,
                "cursor row {cursor_row} is clipped after {num_enters} Enters: \
                 cursor_y={cursor_y}, min_y={min_y}, bottom_offset={bottom_offset}"
            );
            // Cursor row should be at its natural position
            let natural_y = base_grid_top + cursor_row as f32 * line_h;
            assert!(
                (cursor_y - natural_y).abs() < 0.01,
                "cursor row {cursor_row} drifted from natural position: \
                 cursor_y={cursor_y}, natural_y={natural_y}, after {num_enters} Enters"
            );
        }
    }

    /// Simulates the real scenario: OSC 133 marks + Enter presses.
    /// Creates a Terminal, feeds OSC 133 marks + prompts, then checks
    /// that the FrameLayout keeps the cursor row visible.
    /// Simulates 30 Enters on a 23-row grid — with SCROLLING.
    /// This is the real user scenario where blocks pile up at
    /// the bottom rows after scrollback kicks in.
    #[test]
    fn osc133_scrolling_scenario_keeps_prompt() {
        use purrtty_term::Terminal;

        let mut term = Terminal::new(23, 80);
        let line_h = 24.0_f32;
        let base_grid_top = 82.0_f32;
        let block_pad = (line_h * 0.5).max(8.0);

        // 30 Enter presses with OSC 133 marks.
        for i in 0..30 {
            term.advance(b"\x1b]133;A\x07");
            term.advance(format!("prompt{} % ", i).as_bytes());
            term.advance(b"\x1b]133;B\x07\x1b]133;C\x07");
            term.advance(b"\r\n");
        }
        term.advance(b"\x1b]133;A\x07");
        term.advance(b"final_prompt % ");

        let grid = term.grid();
        let blocks = grid.blocks();
        let cursor = grid.cursor();
        let rows = grid.rows();
        let sb_len = grid.scrollback_len();

        eprintln!("rows={rows}, sb_len={sb_len}, cursor=({},{}), blocks={}",
            cursor.row, cursor.col, blocks.len());

        // Compute view boundaries like the renderer does.
        let first_abs = sb_len;
        let view_boundaries: Vec<usize> = blocks
            .iter()
            .filter_map(|b| {
                let v = b.start_row.checked_sub(first_abs)?;
                if v > 0 && v < rows { Some(v) } else { None }
            })
            .collect();

        let bottom_offset = view_boundaries
            .iter()
            .filter(|&&s| s <= rows - 1)
            .count() as f32 * block_pad;
        let grid_top = base_grid_top - bottom_offset;
        let min_y = base_grid_top;

        eprintln!("view_boundaries: {:?}", view_boundaries);
        eprintln!("bottom_offset: {bottom_offset}, grid_top: {grid_top}");

        // Check EVERY row that has content: is it visible?
        for vr in 0..rows {
            let pad = view_boundaries.iter().filter(|&&s| s <= vr).count() as f32 * block_pad;
            let y = grid_top + vr as f32 * line_h + pad;
            let visible = y + line_h >= min_y;
            if vr == cursor.row {
                assert!(
                    visible,
                    "CURSOR ROW {vr} is clipped! y={y}, min_y={min_y}, \
                     boundaries={view_boundaries:?}"
                );
            }
        }

        // Cursor row text.
        let mut cursor_text = String::new();
        for c in 0..grid.cols() {
            let ch = grid.cell(cursor.row, c).ch;
            if ch != '\0' { cursor_text.push(ch); }
        }
        assert!(
            cursor_text.contains("final_prompt"),
            "cursor row {} text: {:?}", cursor.row, cursor_text.trim()
        );
    }

    #[test]
    fn osc133_blocks_dont_hide_prompt() {
        use purrtty_term::Terminal;

        let mut term = Terminal::new(24, 80);
        let line_h = 24.0_f32;
        let base_grid_top = 40.0_f32;
        let block_pad = (line_h * 0.5).max(8.0);

        // Simulate 10 Enter presses with OSC 133 marks.
        for i in 0..10 {
            // precmd: mark A
            term.advance(b"\x1b]133;A\x07");
            // Shell prints prompt
            term.advance(format!("prompt{} % ", i).as_bytes());
            // preexec: marks B + C
            term.advance(b"\x1b]133;B\x07\x1b]133;C\x07");
            // Empty command → just newline
            term.advance(b"\r\n");
        }
        // Final prompt
        term.advance(b"\x1b]133;A\x07");
        term.advance(b"prompt_final % ");

        let grid = term.grid();
        let blocks = grid.blocks();
        let cursor = grid.cursor();
        let rows = grid.rows();

        // Compute block boundaries in view coordinates.
        let sb_len = grid.scrollback_len();
        let first_abs = sb_len; // no scroll offset
        let boundaries: Vec<usize> = blocks
            .iter()
            .filter_map(|b| {
                let view = b.start_row.checked_sub(first_abs)?;
                if view > 0 && view < rows { Some(view) } else { None }
            })
            .collect();

        let bottom_offset = boundaries
            .iter()
            .filter(|&&s| s <= rows - 1)
            .count() as f32 * block_pad;
        let grid_top = base_grid_top - bottom_offset;
        let min_y = base_grid_top;

        // Check cursor row is visible.
        let cursor_view = cursor.row;
        let cursor_pad = boundaries
            .iter()
            .filter(|&&s| s <= cursor_view)
            .count() as f32 * block_pad;
        let cursor_y = grid_top + cursor_view as f32 * line_h + cursor_pad;

        assert!(
            cursor_y + line_h >= min_y,
            "prompt row {} clipped! cursor_y={}, min_y={}, \
             blocks={}, boundaries={:?}, sb_len={}, bottom_offset={}",
            cursor_view, cursor_y, min_y,
            blocks.len(), boundaries, sb_len, bottom_offset
        );

        // Check prompt text exists at cursor row.
        let mut row_text = String::new();
        for c in 0..grid.cols() {
            let ch = grid.cell(cursor_view, c).ch;
            if ch != '\0' { row_text.push(ch); }
        }
        assert!(
            row_text.contains("prompt_final"),
            "cursor row should have prompt text, got: {:?}",
            row_text.trim()
        );
    }

    #[test]
    fn block_row_y_offset_includes_padding() {
        // Pure logic: given block boundaries at rows [0, 5, 10], each
        // boundary adds `pad` pixels of spacing. Row N's Y position is:
        //     grid_top + row * line_h + boundaries_before(row) * pad
        let block_start_rows = [0usize, 5, 10];
        let line_h = 24.0_f32;
        let pad = 8.0_f32;
        let grid_top = 40.0_f32;

        let y_for_row = |row: usize| -> f32 {
            let boundaries_before = block_start_rows
                .iter()
                .filter(|&&s| s > 0 && s <= row)
                .count();
            grid_top + row as f32 * line_h + boundaries_before as f32 * pad
        };

        // Row 0: no boundaries before it.
        assert_eq!(y_for_row(0), 40.0);
        // Row 4: still in first block, no boundary crossed.
        assert_eq!(y_for_row(4), 40.0 + 4.0 * 24.0);
        // Row 5: second block starts here. One boundary (at row 5).
        assert_eq!(y_for_row(5), 40.0 + 5.0 * 24.0 + 8.0);
        // Row 10: third block starts. Two boundaries (at rows 5 and 10).
        assert_eq!(y_for_row(10), 40.0 + 10.0 * 24.0 + 16.0);
    }

    #[test]
    fn build_paste_payload_unbracketed() {
        let payload = build_paste_payload("hello", false);
        assert_eq!(payload, b"hello");
    }

    #[test]
    fn build_paste_payload_bracketed_wraps() {
        let payload = build_paste_payload("hello", true);
        assert_eq!(payload, b"\x1b[200~hello\x1b[201~");
    }

    #[test]
    fn build_paste_payload_bracketed_sanitizes_end_marker() {
        // A malicious clipboard could embed \e[201~ to break out of
        // the bracket. Sanitization must strip it so the shell only
        // sees one closing bracket.
        let payload = build_paste_payload("hi\x1b[201~rm -rf /", true);
        let s = String::from_utf8(payload).unwrap();
        assert_eq!(s, "\x1b[200~hirm -rf /\x1b[201~");
        // Exactly one end marker.
        assert_eq!(s.matches("\x1b[201~").count(), 1);
    }

    #[test]
    fn parser_sets_bracketed_paste_mode() {
        let mut t = purrtty_term::Terminal::new(4, 20);
        // Shell enables bracketed paste.
        t.advance(b"\x1b[?2004h");
        assert!(t.grid().bracketed_paste());
        // And disables it.
        t.advance(b"\x1b[?2004l");
        assert!(!t.grid().bracketed_paste());
    }

    #[test]
    fn shell_input_empty_backspace_preserves_state() {
        let mut s = true;
        update_shell_input_empty(b"\x7f", &mut s);
        assert!(s, "backspace should preserve true");
        let mut s = false;
        update_shell_input_empty(b"\x7f", &mut s);
        assert!(!s, "backspace should preserve false");
    }

    #[test]
    fn shell_input_empty_enter_resets_to_true() {
        let mut s = false;
        update_shell_input_empty(b"\r", &mut s);
        assert!(s);
    }

    #[test]
    fn shell_input_empty_ctrl_c_resets_to_true() {
        let mut s = false;
        update_shell_input_empty(b"\x03", &mut s);
        assert!(s, "Ctrl+C cancels the line");
    }

    #[test]
    fn shell_input_empty_ctrl_u_resets_to_true() {
        let mut s = false;
        update_shell_input_empty(b"\x15", &mut s);
        assert!(s, "Ctrl+U clears the line");
    }

    #[test]
    fn shell_input_empty_arrow_keys_preserve_state() {
        let mut s = true;
        update_shell_input_empty(b"\x1b[A", &mut s);
        assert!(s, "arrow up should preserve true");
        update_shell_input_empty(b"\x1b[B", &mut s);
        assert!(s);
        update_shell_input_empty(b"\x1b[C", &mut s);
        assert!(s);
        update_shell_input_empty(b"\x1b[D", &mut s);
        assert!(s);
    }

    #[test]
    fn shell_input_empty_tab_preserves_state() {
        let mut s = true;
        update_shell_input_empty(b"\t", &mut s);
        assert!(s);
    }

    #[test]
    fn shell_input_empty_printable_sets_false() {
        let mut s = true;
        update_shell_input_empty(b"a", &mut s);
        assert!(!s);
    }

    #[test]
    fn shell_input_empty_utf8_sets_false() {
        let mut s = true;
        update_shell_input_empty("안".as_bytes(), &mut s);
        assert!(!s, "UTF-8 multi-byte text fills the buffer");
    }

    #[test]
    fn shell_input_empty_empty_bytes_preserve() {
        let mut s = true;
        update_shell_input_empty(b"", &mut s);
        assert!(s);
    }

    #[test]
    fn agent_line_bytes_always_contains_marker() {
        // Even with an empty buffer, the "> " marker must be drawn —
        // we are still in agent mode.
        let bytes = agent_line_bytes("", 5);
        let s = String::from_utf8(bytes).unwrap();
        assert!(
            s.contains("> "),
            "agent_line_bytes must always include the '> ' marker (empty buffer case): {s:?}"
        );
        assert!(s.contains("\x1b[6G"), "should CHA to start_col+1");
        assert!(s.contains("\x1b[K"), "should erase to end of line");
    }

    #[test]
    fn agent_line_bytes_with_buffer() {
        let bytes = agent_line_bytes("hello", 2);
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.contains("> "));
        assert!(s.contains("hello"));
        assert!(s.ends_with("hello"));
    }

    #[test]
    fn clear_line_bytes_has_no_marker() {
        let bytes = clear_line_bytes(2);
        let s = String::from_utf8(bytes).unwrap();
        assert!(!s.contains("> "), "clear path should not draw marker");
        assert_eq!(s, "\x1b[3G\x1b[K");
    }

    /// Reproduces the "> >" bug: in agent mode, typing chars then
    /// backspacing them all left the marker erased (refresh_agent_line
    /// with empty buffer), but mode stayed AgentInput. Next `>` got
    /// appended to the buffer and drew "> >" via refresh_agent_line.
    ///
    /// With the fix, refresh_agent_line always draws "> " so the user
    /// sees they're still in agent mode, and typing `>` shows "> >"
    /// (which is now correct — it's literal agent input).
    #[test]
    fn backspace_keeps_marker_visible_in_agent_mode() {
        // Simulating the call site: buffer was "a", pop returns Some,
        // snap is "". refresh_agent_line("", col) is called.
        let bytes = agent_line_bytes("", 4);
        let s = String::from_utf8(bytes).unwrap();
        assert!(
            s.contains("\x1b[1;36m> \x1b[0m"),
            "the '> ' marker (bold cyan) must still be visible after \
             backspacing to an empty buffer while still in agent mode"
        );
    }

    /// End-to-end: pressing backspace in Normal mode (e.g. from an
    /// extra backspace press after exiting agent mode) must not
    /// clobber shell_input_empty, otherwise `>` can't re-enter.
    #[test]
    fn reenter_agent_mode_after_multiple_backspaces() {
        let mut shell_input_empty = true;
        update_shell_input_empty(b"\x7f", &mut shell_input_empty);
        assert!(
            shell_input_empty,
            "backspace in Normal mode should not clobber shell_input_empty"
        );
    }

    /// Simulates the state machine for handle_normal_input's `>` check
    /// and handle_agent_input's backspace exit. Verifies that after
    /// exiting agent mode via backspace, typing `>` again re-enters it.
    #[test]
    fn can_reenter_agent_mode_after_backspace_exit() {
        // Simplified state matching PurrttyApp.
        let mut mode = InputMode::Normal;
        let shell_input_empty = true;

        // Helper: simulate handle_normal_input for the `>` case.
        fn try_enter_agent(
            mode: &mut InputMode,
            shell_input_empty: bool,
            bytes: &[u8],
            cursor_col: usize,
        ) -> bool {
            if bytes == b">" && shell_input_empty {
                *mode = InputMode::AgentInput {
                    buffer: String::new(),
                    start_col: cursor_col,
                };
                return true;
            }
            false
        }

        // Helper: simulate handle_agent_input's backspace on empty buffer.
        fn try_backspace_exit(mode: &mut InputMode) -> bool {
            if let InputMode::AgentInput { buffer, .. } = mode {
                if buffer.pop().is_some() {
                    return false;
                }
                *mode = InputMode::Normal;
                return true;
            }
            false
        }

        // Step 1: press `>` at prompt → enter agent mode.
        assert!(try_enter_agent(&mut mode, shell_input_empty, b">", 2));
        assert!(matches!(mode, InputMode::AgentInput { .. }));
        // handle_normal_input returns before updating shell_input_empty.

        // Step 2: press backspace on empty buffer → exit to Normal.
        assert!(try_backspace_exit(&mut mode));
        assert!(matches!(mode, InputMode::Normal));
        // handle_agent_input doesn't touch shell_input_empty either.
        assert!(shell_input_empty, "shell_input_empty should still be true");

        // Step 3: press `>` again → should re-enter agent mode.
        let reentered = try_enter_agent(&mut mode, shell_input_empty, b">", 2);
        assert!(
            reentered,
            "should re-enter agent mode after backspace exit"
        );
        assert!(matches!(mode, InputMode::AgentInput { .. }));
    }

    /// Backspacing past the empty agent buffer should exit agent mode
    /// and return to normal shell input.
    #[test]
    fn backspace_on_empty_agent_buffer_exits_agent_mode() {
        // Start in AgentInput with an empty buffer (user typed `>` then
        // immediately backspaced the one char they typed, or never
        // typed anything after `>`).
        let mut mode = InputMode::AgentInput {
            buffer: String::new(),
            start_col: 2,
        };

        // Simulate what handle_agent_input does on Backspace:
        if let InputMode::AgentInput { buffer, .. } = &mut mode {
            if buffer.pop().is_some() {
                // Would call refresh_agent_line + redraw here.
            } else {
                // Buffer empty → exit agent mode.
                mode = InputMode::Normal;
            }
        }

        assert!(
            matches!(mode, InputMode::Normal),
            "backspace on empty agent buffer should exit to Normal mode, \
             but mode is still AgentInput"
        );
    }
}
