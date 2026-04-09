//! wgpu + glyphon renderer.
//!
//! Renders a [`Grid`] using a single `cosmic_text::Buffer` shared across
//! the whole grid. Per-line updates go through `buffer.lines[i].set_text`
//! with an `AttrsList` carrying per-cell colors and attributes — this is
//! the same pattern cosmic-term uses, and it lets cosmic-text's
//! line-level shaping reuse what's already shaped from previous frames.
//!
//! See `docs/perf.md` for the research that led here.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use glyphon::cosmic_text::{Attrs, AttrsList, BufferLine, LineEnding};
use glyphon::{
    Buffer, Cache, Color as GlyphColor, Family, FontSystem, Metrics, Resolution, Shaping,
    SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport, Wrap,
};
use purrtty_term::cell::Color as TermColor;
use purrtty_term::grid::WIDE_CONT;
use purrtty_term::{Attrs as CellAttrs, Cell, Grid};
use wgpu::{
    CompositeAlphaMode, DeviceDescriptor, Instance, InstanceDescriptor, LoadOp, MultisampleState,
    Operations, PresentMode, RenderPassColorAttachment, RenderPassDescriptor,
    RequestAdapterOptions, StoreOp, SurfaceConfiguration, TextureUsages, TextureViewDescriptor,
};
use winit::{dpi::PhysicalSize, window::Window};

const FONT_SIZE: f32 = 18.0;
const LINE_HEIGHT: f32 = 22.0;
const CELL_WIDTH: f32 = 10.0;
const PAD_X: f32 = 16.0;
const PAD_Y: f32 = 16.0;

const DEFAULT_FG: GlyphColor = GlyphColor::rgb(220, 220, 220);

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

    /// Single buffer for the entire grid; we mutate `buffer.lines[i]` per
    /// row instead of rebuilding it from scratch each frame.
    buffer: Buffer,
    /// Cached content hash per row, used as a fast pre-check before going
    /// into cosmic-text. Avoids the per-frame allocation of building a
    /// String + AttrsList for unchanged rows.
    row_hashes: Vec<u64>,
    last_grid_rows: usize,
    last_grid_cols: usize,
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
        buffer.set_wrap(&mut font_system, Wrap::None);
        buffer.set_size(
            &mut font_system,
            Some(width as f32),
            Some(height as f32),
        );

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
            row_hashes: Vec::new(),
            last_grid_rows: 0,
            last_grid_cols: 0,
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

        let rows = grid.rows();
        let cols = grid.cols();

        // On grid dimension change, reset the buffer's lines vec to exactly
        // `rows` empty lines and invalidate all row hashes.
        if rows != self.last_grid_rows || cols != self.last_grid_cols {
            self.buffer.lines.clear();
            for _ in 0..rows {
                self.buffer.lines.push(BufferLine::new(
                    "",
                    LineEnding::default(),
                    AttrsList::new(default_attrs()),
                    Shaping::Advanced,
                ));
            }
            self.row_hashes.clear();
            self.row_hashes.resize(rows, u64::MAX);
            self.last_grid_rows = rows;
            self.last_grid_cols = cols;
        }

        // For each visible row, hash and update only on change. cosmic-text
        // tracks shaping per BufferLine internally, so untouched lines
        // don't get re-shaped.
        for view_idx in 0..rows {
            let row = grid.row_at(view_idx, scroll_offset).unwrap_or(&[]);
            let hash = row_hash(row);
            if self.row_hashes[view_idx] == hash {
                continue;
            }
            let (text, attrs_list) = build_line(row);
            self.buffer.lines[view_idx].set_text(text, LineEnding::default(), attrs_list);
            self.row_hashes[view_idx] = hash;
        }

        self.buffer
            .shape_until_scroll(&mut self.font_system, false);

        let bounds = TextBounds {
            left: 0,
            top: 0,
            right: self.config.width as i32,
            bottom: self.config.height as i32,
        };
        let text_area = TextArea {
            buffer: &self.buffer,
            left: PAD_X,
            top: PAD_Y,
            scale: 1.0,
            bounds,
            default_color: DEFAULT_FG,
            custom_glyphs: &[],
        };

        self.text_renderer
            .prepare(
                &self.device,
                &self.queue,
                &mut self.font_system,
                &mut self.atlas,
                &self.viewport,
                [text_area],
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
        let _ = &self.window;
        Ok(())
    }
}

fn default_attrs() -> Attrs<'static> {
    Attrs::new().family(Family::Monospace)
}

/// Build the cosmic-text line text and the matching `AttrsList` from a
/// grid row. Cells with identical fg/attrs are compacted into one span.
fn build_line(row: &[Cell]) -> (String, AttrsList) {
    let mut attrs_list = AttrsList::new(default_attrs());
    let mut text = String::with_capacity(row.len());

    let mut run_start: Option<usize> = None;
    let mut run_fg = TermColor::Default;
    let mut run_attrs = CellAttrs::empty();

    for cell in row {
        if cell.ch == WIDE_CONT {
            continue;
        }
        let started_new_run = match run_start {
            Some(_) => cell.fg != run_fg || cell.attrs != run_attrs,
            None => true,
        };
        if started_new_run {
            if let Some(start) = run_start {
                if start < text.len() {
                    attrs_list.add_span(start..text.len(), make_attrs(run_fg, run_attrs));
                }
            }
            run_start = Some(text.len());
            run_fg = cell.fg;
            run_attrs = cell.attrs;
        }
        text.push(cell.ch);
    }
    if let Some(start) = run_start {
        if start < text.len() {
            attrs_list.add_span(start..text.len(), make_attrs(run_fg, run_attrs));
        }
    }

    (text, attrs_list)
}

/// Convert a terminal cell's foreground + attrs into a cosmic-text Attrs
/// suitable for an `AttrsList` span. Background colors and reverse-video
/// will be a separate wgpu quad pass in the next stage.
fn make_attrs(fg: TermColor, attrs: CellAttrs) -> Attrs<'static> {
    use glyphon::cosmic_text::{Style, Weight};

    let mut a = default_attrs();
    if let Some(color) = term_color_to_glyph(fg) {
        a = a.color(color);
    }
    if attrs.contains(CellAttrs::BOLD) {
        a = a.weight(Weight::BOLD);
    }
    if attrs.contains(CellAttrs::ITALIC) {
        a = a.style(Style::Italic);
    }
    a
}

fn term_color_to_glyph(c: TermColor) -> Option<GlyphColor> {
    match c {
        TermColor::Default => None,
        TermColor::Indexed(i) => Some(indexed_color(i)),
        TermColor::Rgb(r, g, b) => Some(GlyphColor::rgb(r, g, b)),
    }
}

/// 16-color ANSI palette + xterm 256-color cube + grayscale ramp.
fn indexed_color(i: u8) -> GlyphColor {
    const ANSI_16: [(u8, u8, u8); 16] = [
        (0, 0, 0),       // black
        (205, 49, 49),   // red
        (13, 188, 121),  // green
        (229, 229, 16),  // yellow
        (36, 114, 200),  // blue
        (188, 63, 188),  // magenta
        (17, 168, 205),  // cyan
        (229, 229, 229), // white (light gray)
        (102, 102, 102), // bright black (dark gray)
        (241, 76, 76),   // bright red
        (35, 209, 139),  // bright green
        (245, 245, 67),  // bright yellow
        (59, 142, 234),  // bright blue
        (214, 112, 214), // bright magenta
        (41, 184, 219),  // bright cyan
        (255, 255, 255), // bright white
    ];
    if (i as usize) < ANSI_16.len() {
        let (r, g, b) = ANSI_16[i as usize];
        return GlyphColor::rgb(r, g, b);
    }
    if i >= 16 && i <= 231 {
        // 6×6×6 color cube
        let n = i - 16;
        let r = n / 36;
        let g = (n % 36) / 6;
        let b = n % 6;
        let lvl = |c: u8| -> u8 {
            if c == 0 {
                0
            } else {
                55 + c * 40
            }
        };
        return GlyphColor::rgb(lvl(r), lvl(g), lvl(b));
    }
    // 232..=255 grayscale ramp
    let g = 8 + (i - 232) * 10;
    GlyphColor::rgb(g, g, g)
}

/// Content hash for one grid row, including every cell's char/fg/bg/attrs.
fn row_hash(row: &[Cell]) -> u64 {
    let mut h = DefaultHasher::new();
    for cell in row {
        cell.hash(&mut h);
    }
    h.finish()
}
