//! purrtty — binary entry point.
//!
//! M1: opens a winit window, initializes a wgpu surface, and renders
//! a fixed greeting via glyphon. Closing the window exits the app.

#![forbid(unsafe_code)]

use std::sync::Arc;

use anyhow::Result;
use purrtty_ui::Renderer;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

/// Top-level application state held across the winit event loop.
#[derive(Default)]
struct PurttyApp {
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
}

impl ApplicationHandler for PurttyApp {
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
                warn!(?err, "failed to create window");
                event_loop.exit();
                return;
            }
        };
        info!(
            size = ?window.inner_size(),
            scale_factor = window.scale_factor(),
            "window created"
        );

        match Renderer::new(window.clone()) {
            Ok(r) => {
                self.renderer = Some(r);
                info!("renderer ready");
                window.request_redraw();
            }
            Err(err) => {
                error!(?err, "failed to initialize renderer");
                event_loop.exit();
                return;
            }
        }

        self.window = Some(window);
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
                info!(?size, "resized");
                if let Some(renderer) = self.renderer.as_mut() {
                    renderer.resize(size);
                }
                if let Some(window) = self.window.as_ref() {
                    window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(renderer) = self.renderer.as_mut() {
                    if let Err(err) = renderer.render() {
                        warn!(?err, "render failed");
                    }
                }
            }
            _ => {}
        }
    }
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

    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut app = PurttyApp::default();
    event_loop.run_app(&mut app)?;
    Ok(())
}
