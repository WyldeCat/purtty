//! purrtty — binary entry point.
//!
//! M3: wires a real PTY-backed shell to the VT parser and the wgpu
//! renderer. The event loop owns the renderer, the PTY session, and a
//! shared `Terminal` that both the UI thread and the PTY reader thread
//! mutate under a mutex. Keyboard input is translated to bytes and
//! pushed into the PTY; the reader thread feeds incoming bytes into
//! the VT parser and wakes the UI via an `EventLoopProxy`.

#![forbid(unsafe_code)]

mod agent;
mod config;
mod selection;
mod urls;

use std::sync::{Arc, Mutex};

use agent::AgentSession;
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
use winit::window::{Window, WindowId};

/// Events posted to the winit loop from background threads.
#[derive(Debug, Clone)]
enum UserEvent {
    /// Bytes arrived from the PTY or agent; redraw is needed.
    PtyDataArrived,
    /// The Claude agent process exited.
    AgentFinished { exit_code: i32 },
    /// Deferred redraw — fires ~500ms after startup to ensure the prompt
    /// is rendered even if early frames were discarded by macOS before
    /// the CAMetalLayer was presentable.
    DeferredRedraw,
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

#[derive(Default)]
struct PurrttyApp {
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    pty: Option<PtySession>,
    terminal: Option<SharedTerminal>,
    proxy: Option<EventLoopProxy<UserEvent>>,
    /// How many rows the view is scrolled into scrollback. 0 = live bottom.
    scroll_offset: usize,
    /// Latest modifier state from `WindowEvent::ModifiersChanged`. Used by
    /// the keyboard mapper to detect Ctrl/Alt/Cmd combos.
    modifiers: ModifiersState,
    /// Loaded user config — used to seed the window and the renderer.
    config: Config,
    /// Agent input mode state machine.
    input_mode: InputMode,
    /// Heuristic: true after we forward Enter to the PTY (shell will
    /// re-emit a prompt), false after forwarding any other key. Used to
    /// detect "user is at the start of a fresh prompt line".
    shell_input_empty: bool,
    /// Handle to a running agent child process (if any).
    agent_session: Option<AgentSession>,
    /// Active text selection, if any. Present while the user is
    /// dragging or has a finalized selection they can copy.
    selection: Option<Selection>,
    /// True while the left mouse button is held and a selection is
    /// being dragged.
    selecting: bool,
    /// Last known mouse position in physical pixels. Needed because
    /// winit fires `CursorMoved` independent of `MouseInput`.
    cursor_pos: (f64, f64),
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

    fn zoom_font(&mut self, delta: f32) {
        let Some(renderer) = self.renderer.as_mut() else { return };
        let Some((rows, cols)) = renderer.change_font_size(delta) else { return };
        info!(rows, cols, delta, "font size changed");
        if let Some(terminal) = self.terminal.as_ref() {
            if let Ok(mut term) = terminal.lock() {
                term.grid_mut().resize(rows as usize, cols as usize);
            }
        }
        if let Some(pty) = self.pty.as_ref() {
            if let Err(err) = pty.resize(rows, cols) {
                warn!(?err, "pty resize failed after font zoom");
            }
        }
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
            .terminal
            .as_ref()?
            .lock()
            .ok()
            .map(|t| t.grid().scrollback_len())
            .unwrap_or(0);
        let abs_row = view_row_to_absolute(view_row, self.scroll_offset, sb_len);
        Some(GridPoint::new(abs_row, col))
    }

    /// Return the URL at the given physical pixel position, if any.
    /// Scans the row under the cursor for `http://`, `https://`, and
    /// `file://` schemes and checks which (if any) contains the click.
    fn url_at_pixel(&self, x: f64, y: f64) -> Option<String> {
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
        let terminal = self.terminal.as_ref()?;
        let term = terminal.lock().ok()?;
        let row_cells = term.grid().row_at(view_row, self.scroll_offset)?;
        // Build the row's string and a parallel (char_byte_index → col)
        // mapping so we can convert URL byte ranges back to columns.
        let mut row_text = String::with_capacity(row_cells.len());
        let mut byte_to_col: Vec<usize> = Vec::with_capacity(row_cells.len());
        for (col, cell) in row_cells.iter().enumerate() {
            if cell.ch == '\0' {
                continue; // wide-char continuation
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
                return Some(row_text[range].to_string());
            }
        }
        None
    }

    fn copy_selection_to_clipboard(&mut self) {
        let Some(sel) = self.selection else { return };
        if sel.is_empty() {
            return;
        }
        let Some(terminal) = self.terminal.as_ref() else { return };
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
            .terminal
            .as_ref()
            .and_then(|t| t.lock().ok().map(|g| g.grid().bracketed_paste()))
            .unwrap_or(false);
        let payload = build_paste_payload(&normalized, bracketed);

        if let Some(pty) = self.pty.as_mut() {
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
                        // Ctrl+C
                        if let Some(session) = self.agent_session.as_mut() {
                            session.kill();
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
                .terminal
                .as_ref()
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
        if let Some(pty) = self.pty.as_mut() {
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
        if let Some(terminal) = self.terminal.as_ref() {
            if let Ok(mut term) = terminal.lock() {
                term.advance(bytes);
            }
        }
    }

    fn spawn_agent(&mut self, prompt: String) {
        // Try OSC 7 first, then lsof on the shell PID, then purrtty's own cwd.
        let cwd = self
            .terminal
            .as_ref()
            .and_then(|t| t.lock().ok().and_then(|g| g.grid().cwd().map(|p| p.to_path_buf())))
            .or_else(|| {
                self.pty
                    .as_ref()
                    .and_then(|pty| pty.child_pid())
                    .and_then(shell_cwd_via_lsof)
            })
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

        let terminal_for_agent = self.terminal.clone().unwrap();
        let proxy = self.proxy.clone().unwrap();

        let proxy_for_exit = proxy.clone();
        match AgentSession::spawn(
            &prompt,
            &cwd,
            move |bytes| {
                if let Ok(mut term) = terminal_for_agent.lock() {
                    term.advance(bytes);
                }
                let _ = proxy.send_event(UserEvent::PtyDataArrived);
            },
            move |exit_code| {
                let _ = proxy_for_exit.send_event(UserEvent::AgentFinished { exit_code });
            },
        ) {
            Ok(session) => {
                info!(?cwd, "agent spawned");
                self.agent_session = Some(session);
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

        let terminal = Arc::new(Mutex::new(Terminal::new(rows as usize, cols as usize)));

        let terminal_for_reader = terminal.clone();
        let proxy_for_reader = self
            .proxy
            .clone()
            .expect("proxy set before ApplicationHandler::resumed");

        let pty = match PtySession::spawn(rows, cols, move |bytes| {
            if let Ok(mut term) = terminal_for_reader.lock() {
                term.advance(bytes);
            }
            let _ = proxy_for_reader.send_event(UserEvent::PtyDataArrived);
        }) {
            Ok(p) => p,
            Err(err) => {
                error!(?err, "failed to spawn pty");
                event_loop.exit();
                return;
            }
        };

        // Allow macOS / X11 IME composition events to flow through.
        window.set_ime_allowed(true);

        self.window = Some(window.clone());
        self.renderer = Some(renderer);
        self.pty = Some(pty);
        self.terminal = Some(terminal);
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
            UserEvent::PtyDataArrived => {
                // Drain any pending terminal responses (DA, DSR, etc.)
                // that were queued during the last advance() call.
                let responses = self
                    .terminal
                    .as_ref()
                    .and_then(|t| {
                        t.lock()
                            .ok()
                            .map(|mut term| term.grid_mut().drain_responses())
                    })
                    .unwrap_or_default();
                if let Some(pty) = self.pty.as_mut() {
                    for resp in responses {
                        let _ = pty.write(&resp);
                    }
                }
                self.redraw();
            }
            UserEvent::DeferredRedraw => {
                self.redraw();
            }
            UserEvent::AgentFinished { exit_code } => {
                info!(exit_code, "agent finished");
                self.input_mode = InputMode::Normal;
                self.agent_session = None;
                // Print a status marker into the terminal grid.
                let marker = if exit_code == 0 {
                    "\r\n\x1b[1;32m✓ agent done\x1b[0m\r\n"
                } else {
                    "\r\n\x1b[1;31m✗ agent failed\x1b[0m\r\n"
                };
                self.echo_to_terminal(marker.as_bytes());
                // Nudge the shell to re-display its prompt by sending
                // a newline. The shell sees an empty command and just
                // prints a fresh prompt.
                if let Some(pty) = self.pty.as_mut() {
                    let _ = pty.write(b"\n");
                }
                self.shell_input_empty = true;
                self.redraw();
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
                if let Some(renderer) = self.renderer.as_mut() {
                    renderer.resize(size);
                    let (rows, cols) = renderer.grid_dimensions();
                    if let Some(terminal) = self.terminal.as_ref() {
                        if let Ok(mut term) = terminal.lock() {
                            term.grid_mut().resize(rows as usize, cols as usize);
                        }
                    }
                    if let Some(pty) = self.pty.as_ref() {
                        if let Err(err) = pty.resize(rows, cols) {
                            warn!(?err, "pty resize failed");
                        }
                    }
                }
                self.redraw();
            }
            WindowEvent::RedrawRequested => {
                let renderer = match self.renderer.as_mut() {
                    Some(r) => r,
                    None => return,
                };
                let terminal = match self.terminal.as_ref() {
                    Some(t) => t,
                    None => return,
                };
                let guard = match terminal.lock() {
                    Ok(g) => g,
                    Err(err) => {
                        warn!(?err, "terminal mutex poisoned");
                        return;
                    }
                };
                let selection_range = self.selection.and_then(|s| {
                    if s.is_empty() {
                        return None;
                    }
                    let (start, end) = s.normalized();
                    Some(((start.row, start.col), (end.row, end.col)))
                });
                if let Err(err) = renderer.render_with_selection(
                    guard.grid(),
                    self.scroll_offset,
                    selection_range,
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
                                if let Some(pty) = self.pty.as_mut() {
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
                        // Clipboard: Cmd only. Ctrl+C must stay SIGINT
                        // and Ctrl+V must stay a control byte.
                        if self.modifiers.super_key() {
                            match s.as_str() {
                                "c" | "C" => { self.copy_selection_to_clipboard(); return; }
                                "v" | "V" => { self.paste_from_clipboard(); return; }
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
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if button == MouseButton::Left {
                    match state {
                        ElementState::Pressed => {
                            // Cmd+click: try to open URL at the click point.
                            if self.modifiers.super_key() {
                                let (x, y) = self.cursor_pos;
                                if let Some(url) = self.url_at_pixel(x, y) {
                                    open_url(&url);
                                    return;
                                }
                            }
                            let (x, y) = self.cursor_pos;
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
                    .terminal
                    .as_ref()
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
