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

use std::sync::{Arc, Mutex};

use agent::AgentSession;
use config::Config;

use anyhow::Result;
use purrtty_pty::PtySession;
use purrtty_term::Terminal;
use purrtty_ui::Renderer;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, Ime, KeyEvent, MouseScrollDelta, WindowEvent};
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
            // Echo a bold-cyan `> ` marker into the terminal grid.
            self.echo_to_terminal(b"\x1b[1;36m> \x1b[0m");
            self.redraw();
            return;
        }

        // Snap scrollback view to the live bottom on any input.
        if self.scroll_offset != 0 {
            self.scroll_offset = 0;
            self.redraw();
        }

        // Track the heuristic for "shell is at empty input".
        if bytes == b"\r" {
            self.shell_input_empty = true;
        } else {
            self.shell_input_empty = false;
        }

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
            self.refresh_agent_line("", start_col);
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
                self.refresh_agent_line("", start_col);
                self.input_mode = InputMode::Normal;
                self.redraw();
                return;
            }
            self.echo_to_terminal(b"\r\n");
            self.input_mode = InputMode::AgentRunning;
            self.spawn_agent(buffer);
            return;
        }

        // Backspace → remove last char from buffer.
        if event.logical_key == Key::Named(NamedKey::Backspace) {
            if let InputMode::AgentInput { buffer, start_col } = &mut self.input_mode {
                if buffer.pop().is_some() {
                    let snap = buffer.clone();
                    let col = *start_col;
                    self.refresh_agent_line(&snap, col);
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
    /// shell prompt to the left. Uses CHA (cursor horizontal absolute)
    /// to jump to the saved column and EL mode 0 to erase only from
    /// that point to end of line.
    fn refresh_agent_line(&self, buffer: &str, start_col: usize) {
        // CHA is 1-indexed.
        let line = if buffer.is_empty() {
            // Erase the agent marker entirely.
            format!("\x1b[{}G\x1b[K", start_col + 1)
        } else {
            format!(
                "\x1b[{}G\x1b[K\x1b[1;36m> \x1b[0m{}",
                start_col + 1,
                buffer
            )
        };
        self.echo_to_terminal(line.as_bytes());
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
                if let Err(err) = renderer.render(guard.grid(), self.scroll_offset) {
                    warn!(?err, "render failed");
                }
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
            }
            WindowEvent::Ime(ime) => {
                // Forward committed IME input (e.g. finalized hangul) to the
                // shell. Preedit composition is currently not displayed —
                // overlay rendering is a follow-up.
                if let Ime::Commit(text) = ime {
                    if !text.is_empty() {
                        if self.scroll_offset != 0 {
                            self.scroll_offset = 0;
                            self.redraw();
                        }
                        if let Some(pty) = self.pty.as_mut() {
                            if let Err(err) = pty.write(text.as_bytes()) {
                                warn!(?err, "pty write failed (ime commit)");
                            }
                        }
                    }
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                self.handle_keyboard_input(&event);
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
