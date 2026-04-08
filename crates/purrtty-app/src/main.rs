//! purrtty — binary entry point.
//!
//! M3: wires a real PTY-backed shell to the VT parser and the wgpu
//! renderer. The event loop owns the renderer, the PTY session, and a
//! shared `Terminal` that both the UI thread and the PTY reader thread
//! mutate under a mutex. Keyboard input is translated to bytes and
//! pushed into the PTY; the reader thread feeds incoming bytes into
//! the VT parser and wakes the UI via an `EventLoopProxy`.

#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex};

use anyhow::Result;
use purrtty_pty::PtySession;
use purrtty_term::Terminal;
use purrtty_ui::Renderer;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

/// Events posted to the winit loop from background threads.
#[derive(Debug, Clone, Copy)]
enum UserEvent {
    /// Bytes arrived from the PTY; redraw is needed.
    PtyDataArrived,
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
}

/// Approximate cell line height for turning pixel scroll deltas into rows.
/// Keep in sync with the renderer's LINE_HEIGHT constant.
const SCROLL_LINE_HEIGHT: f64 = 22.0;

impl PurrttyApp {
    fn with_proxy(proxy: EventLoopProxy<UserEvent>) -> Self {
        Self {
            proxy: Some(proxy),
            ..Self::default()
        }
    }

    fn redraw(&self) {
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
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
            .with_inner_size(winit::dpi::LogicalSize::new(960.0, 600.0));
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

        let renderer = match Renderer::new(window.clone()) {
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

        self.window = Some(window.clone());
        self.renderer = Some(renderer);
        self.pty = Some(pty);
        self.terminal = Some(terminal);
        window.request_redraw();
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::PtyDataArrived => self.redraw(),
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
            WindowEvent::KeyboardInput { event, .. } => {
                if let Some(bytes) = key_event_to_bytes(&event) {
                    // Any real input snaps the view back to the live bottom.
                    if self.scroll_offset != 0 {
                        self.scroll_offset = 0;
                        self.redraw();
                    }
                    if let Some(pty) = self.pty.as_mut() {
                        if let Err(err) = pty.write(&bytes) {
                            warn!(?err, "pty write failed");
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

/// Translate a winit `KeyEvent` into the bytes a shell expects on stdin.
///
/// This is the minimum usable set: Enter, Tab, Backspace, Escape, arrow
/// keys, and whatever characters winit gives us in `text` (which already
/// accounts for the keyboard layout and modifiers for printable input).
fn key_event_to_bytes(event: &KeyEvent) -> Option<Vec<u8>> {
    if event.state != ElementState::Pressed {
        return None;
    }

    // Named keys we translate to explicit escape sequences. For any other
    // named key (Space, letters composed with modifiers, etc.) we fall
    // through to the text fallback below, which winit fills in with the
    // already-composed character(s).
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

    let proxy = event_loop.create_proxy();
    let mut app = PurrttyApp::with_proxy(proxy);
    event_loop.run_app(&mut app)?;
    Ok(())
}
