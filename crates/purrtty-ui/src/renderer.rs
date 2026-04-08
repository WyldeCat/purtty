//! wgpu + glyphon renderer.
//!
//! Renders a [`Grid`] as monochrome text on a dark background via
//! glyphon. Color attributes and a blinking cursor land in M4 polish.

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use glyphon::{
    Attrs, Buffer, Cache, Color as GlyphColor, Family, FontSystem, Metrics, Resolution, Shaping,
    SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport, Wrap,
};
use purrtty_term::Grid;
use wgpu::{
    CompositeAlphaMode, DeviceDescriptor, Instance, InstanceDescriptor, LoadOp, MultisampleState,
    Operations, PresentMode, RenderPassColorAttachment, RenderPassDescriptor,
    RequestAdapterOptions, StoreOp, SurfaceConfiguration, TextureUsages, TextureViewDescriptor,
};
use winit::{dpi::PhysicalSize, window::Window};

/// Font size in physical pixels.
const FONT_SIZE: f32 = 18.0;
/// Line height in physical pixels (font size * ~1.22).
const LINE_HEIGHT: f32 = 22.0;
/// Approximate monospace advance width in physical pixels. Slightly over a
/// half em for most monospace fonts at 18px.
const CELL_WIDTH: f32 = 10.0;
/// Inner window padding (physical pixels).
const PAD_X: f32 = 16.0;
const PAD_Y: f32 = 16.0;

/// Owns wgpu + glyphon state tied to a single window/surface.
pub struct Renderer {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: SurfaceConfiguration,

    font_system: FontSystem,
    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    text_renderer: TextRenderer,
    buffer: Buffer,
}

impl Renderer {
    pub fn new(window: Arc<Window>) -> Result<Self> {
        let size = window.inner_size();
        let width = size.width.max(1);
        let height = size.height.max(1);

        let instance = Instance::new(InstanceDescriptor::default());
        let surface = instance
            .create_surface(window.clone())
            .context("create wgpu surface")?;

        let adapter = pollster::block_on(instance.request_adapter(&RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
        }))
        .ok_or_else(|| anyhow!("no suitable wgpu adapter found"))?;

        let (device, queue) =
            pollster::block_on(adapter.request_device(&DeviceDescriptor::default(), None))
                .context("request wgpu device")?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);

        let config = SurfaceConfiguration {
            usage: TextureUsages::RENDER_ATTACHMENT,
            format,
            width,
            height,
            present_mode: PresentMode::Fifo,
            alpha_mode: CompositeAlphaMode::Auto,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let mut font_system = FontSystem::new();
        let swash_cache = SwashCache::new();
        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        let mut atlas = TextAtlas::new(&device, &queue, &cache, format);
        let text_renderer =
            TextRenderer::new(&mut atlas, &device, MultisampleState::default(), None);

        let mut buffer = Buffer::new(&mut font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
        // Terminals are column-oriented: one grid row = one visual line.
        // Disable cosmic-text's soft-wrap so a row wider than the buffer
        // just gets clipped instead of flowing into extra visual lines
        // that would push the cursor off the bottom of the window.
        buffer.set_wrap(&mut font_system, Wrap::None);
        buffer.set_size(&mut font_system, Some(width as f32), Some(height as f32));

        Ok(Self {
            window,
            surface,
            device,
            queue,
            config,
            font_system,
            swash_cache,
            viewport,
            atlas,
            text_renderer,
            buffer,
        })
    }

    /// Terminal grid dimensions, in cells, that fit the current surface.
    pub fn grid_dimensions(&self) -> (u16, u16) {
        let w = (self.config.width as f32 - 2.0 * PAD_X).max(0.0);
        let h = (self.config.height as f32 - 2.0 * PAD_Y).max(0.0);
        let cols = (w / CELL_WIDTH).floor().max(1.0) as u16;
        let rows = (h / LINE_HEIGHT).floor().max(1.0) as u16;
        (rows, cols)
    }

    pub fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.device, &self.config);
        self.buffer.set_size(
            &mut self.font_system,
            Some(size.width as f32),
            Some(size.height as f32),
        );
    }

    pub fn render(&mut self, grid: &Grid, scroll_offset: usize) -> Result<()> {
        self.viewport.update(
            &self.queue,
            Resolution {
                width: self.config.width,
                height: self.config.height,
            },
        );

        // Build the frame text by walking the visible view. Scroll offset
        // pulls rows out of scrollback into the top of the view; 0 is the
        // live bottom.
        let rows = grid.rows();
        let cols = grid.cols();
        let mut text = String::with_capacity(rows * (cols + 1));
        for view_idx in 0..rows {
            if let Some(row) = grid.row_at(view_idx, scroll_offset) {
                for cell in row {
                    // Skip right-hand continuation cells of wide glyphs —
                    // the wide char in the preceding cell already covers
                    // that visual column.
                    if cell.ch == purrtty_term::grid::WIDE_CONT {
                        continue;
                    }
                    text.push(cell.ch);
                }
            }
            text.push('\n');
        }
        // drop trailing newline so cosmic-text doesn't reserve an extra line
        if text.ends_with('\n') {
            text.pop();
        }

        self.buffer.set_text(
            &mut self.font_system,
            &text,
            Attrs::new().family(Family::Monospace),
            Shaping::Advanced,
        );
        self.buffer
            .shape_until_scroll(&mut self.font_system, false);

        self.text_renderer
            .prepare(
                &self.device,
                &self.queue,
                &mut self.font_system,
                &mut self.atlas,
                &self.viewport,
                [TextArea {
                    buffer: &self.buffer,
                    left: PAD_X,
                    top: PAD_Y,
                    scale: 1.0,
                    bounds: TextBounds {
                        left: 0,
                        top: 0,
                        right: self.config.width as i32,
                        bottom: self.config.height as i32,
                    },
                    default_color: GlyphColor::rgb(220, 220, 220),
                    custom_glyphs: &[],
                }],
                &mut self.swash_cache,
            )
            .context("glyphon prepare")?;

        let frame = self
            .surface
            .get_current_texture()
            .context("acquire surface texture")?;
        let view = frame.texture.create_view(&TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("purrtty.encoder"),
            });

        {
            let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("purrtty.main"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: Operations {
                        load: LoadOp::Clear(wgpu::Color {
                            r: 0.05,
                            g: 0.05,
                            b: 0.08,
                            a: 1.0,
                        }),
                        store: StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            self.text_renderer
                .render(&self.atlas, &self.viewport, &mut pass)
                .context("glyphon render")?;
        }

        self.queue.submit(Some(encoder.finish()));
        frame.present();
        self.atlas.trim();
        let _ = &self.window; // keep window reference live for surface 'static
        Ok(())
    }
}
