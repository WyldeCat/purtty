//! wgpu renderer using platform-native font rasterization.
//!
//! Each grid cell is rendered as a textured quad sampled from a glyph
//! atlas populated via `font-kit` (Core Text on macOS). Backgrounds
//! and the cursor use the solid-color quad pipeline from `quad.rs`.
//!
//! This replaces the previous glyphon/cosmic-text pipeline and produces
//! font quality matching native macOS terminal apps.

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use purrtty_term::cell::Color as TermColor;
use purrtty_term::grid::WIDE_CONT;
use purrtty_term::{Attrs as CellAttrs, Cell, Grid};
use wgpu::{
    CompositeAlphaMode, DeviceDescriptor, Instance, InstanceDescriptor, LoadOp,
    Operations, PresentMode, RenderPassColorAttachment, RenderPassDescriptor,
    RequestAdapterOptions, StoreOp, SurfaceConfiguration, TextureUsages, TextureViewDescriptor,
};
use winit::{dpi::PhysicalSize, window::Window};

use crate::glyph_cache::{GlyphCache, GlyphVertex};
use crate::quad::{QuadRenderer, QuadVertex};
use crate::theme::{RendererConfig, Theme};

const PAD_X: f32 = 16.0;
const PAD_Y: f32 = 16.0;

pub struct Renderer {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: SurfaceConfiguration,

    glyphs: GlyphCache,
    quads: QuadRenderer,

    line_height: f32,
    theme: Theme,
}

impl Renderer {
    pub fn new(window: Arc<Window>, cfg: RendererConfig) -> Result<Self> {
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

        let surface_config = SurfaceConfiguration {
            usage: TextureUsages::RENDER_ATTACHMENT,
            format,
            width,
            height,
            present_mode: PresentMode::Fifo,
            alpha_mode: CompositeAlphaMode::Auto,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &surface_config);

        let glyphs = GlyphCache::new(
            &device,
            &queue,
            format,
            cfg.font_family.as_deref(),
            cfg.font_size,
            cfg.line_height,
        )?;

        let quads = QuadRenderer::new(&device, format)?;

        tracing::info!(
            cell_width = glyphs.cell_width,
            line_height = glyphs.line_height,
            ascent = glyphs.ascent,
            "renderer ready (Core Text)"
        );

        Ok(Self {
            window,
            surface,
            device,
            queue,
            config: surface_config,
            glyphs,
            quads,
            line_height: cfg.line_height,
            theme: cfg.theme,
        })
    }

    pub fn grid_dimensions(&self) -> (u16, u16) {
        let w = (self.config.width as f32 - 2.0 * PAD_X).max(0.0);
        let h = (self.config.height as f32 - 2.0 * PAD_Y).max(0.0);
        let cols = (w / self.glyphs.cell_width).floor().max(1.0) as u16;
        let rows = (h / self.line_height).floor().max(1.0) as u16;
        (rows, cols)
    }

    pub fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.device, &self.config);
    }

    pub fn render(&mut self, grid: &Grid, scroll_offset: usize) -> Result<()> {
        let rows = grid.rows();
        let cols = grid.cols();
        let cell_w = self.glyphs.cell_width;
        let line_h = self.line_height;
        let ascent = self.glyphs.ascent;

        let mut glyph_verts: Vec<GlyphVertex> = Vec::with_capacity(rows * cols);
        let mut bg_verts: Vec<QuadVertex> = Vec::new();

        for view_idx in 0..rows {
            let row = grid.row_at(view_idx, scroll_offset).unwrap_or(&[]);
            for (col_idx, cell) in row.iter().enumerate() {
                if col_idx >= cols {
                    break;
                }
                if cell.ch == WIDE_CONT {
                    continue;
                }

                let (fg_color, bg_opt) = self.cell_colors(cell);
                let cell_x = PAD_X + col_idx as f32 * cell_w;
                let cell_y = PAD_Y + view_idx as f32 * line_h;

                // Background quad.
                if let Some(bg) = bg_opt {
                    let next_is_cont = col_idx + 1 < cols
                        && row
                            .get(col_idx + 1)
                            .map(|c| c.ch == WIDE_CONT)
                            .unwrap_or(false);
                    let w = if next_is_cont { 2.0 * cell_w } else { cell_w };
                    QuadRenderer::push_rect(&mut bg_verts, cell_x, cell_y, w, line_h, bg);
                }

                // Glyph quad.
                if cell.ch != ' ' {
                    if let Some(entry) =
                        self.glyphs
                            .get_or_insert(cell.ch, &self.device, &self.queue)
                    {
                        GlyphCache::push_glyph(
                            &mut glyph_verts,
                            &entry,
                            cell_x,
                            cell_y,
                            ascent,
                            fg_color,
                        );
                    }
                }
            }
        }

        // Cursor quad.
        let mut overlay_verts: Vec<QuadVertex> = Vec::new();
        if grid.cursor_visible() && scroll_offset == 0 {
            let cursor = grid.cursor();
            if cursor.row < rows && cols > 0 {
                let col = cursor.col.min(cols - 1);
                let x = PAD_X + col as f32 * cell_w;
                let y = PAD_Y + cursor.row as f32 * line_h;
                QuadRenderer::push_rect(
                    &mut overlay_verts,
                    x,
                    y,
                    cell_w,
                    line_h,
                    [0.85, 0.85, 0.85, 0.4],
                );
            }
        }

        // Upload.
        self.glyphs
            .update_resolution(&self.queue, self.config.width, self.config.height);
        self.glyphs
            .upload(&self.device, &self.queue, &glyph_verts);
        self.quads
            .update_resolution(&self.queue, self.config.width, self.config.height);
        self.quads.upload_bg(&self.device, &self.queue, &bg_verts);
        self.quads
            .upload_overlay(&self.device, &self.queue, &overlay_verts);

        // Render pass: clear → bg quads → glyph quads → cursor overlay.
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
                        load: LoadOp::Clear(self.theme.background.as_wgpu()),
                        store: StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            self.quads.render_bg(&mut pass);
            self.glyphs.render(&mut pass);
            self.quads.render_overlay(&mut pass);
        }

        self.queue.submit(Some(encoder.finish()));
        frame.present();
        let _ = &self.window;
        Ok(())
    }

    fn cell_colors(&self, cell: &Cell) -> ([f32; 4], Option<[f32; 4]>) {
        let fg = self.resolve_color(cell.fg, self.fg_rgba());
        let bg_opt = match cell.bg {
            TermColor::Default => None,
            other => Some(self.resolve_color(other, self.fg_rgba())),
        };

        if cell.attrs.contains(CellAttrs::REVERSE) {
            let new_fg = bg_opt.unwrap_or(self.theme.background.as_array());
            let new_bg = fg;
            return (new_fg, Some(new_bg));
        }

        (fg, bg_opt)
    }

    fn fg_rgba(&self) -> [f32; 4] {
        let c = self.theme.foreground;
        [
            c.r() as f32 / 255.0,
            c.g() as f32 / 255.0,
            c.b() as f32 / 255.0,
            c.a() as f32 / 255.0,
        ]
    }

    fn resolve_color(&self, c: TermColor, default: [f32; 4]) -> [f32; 4] {
        match c {
            TermColor::Default => default,
            TermColor::Indexed(i) => self.indexed_color(i),
            TermColor::Rgb(r, g, b) => [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0],
        }
    }

    fn indexed_color(&self, i: u8) -> [f32; 4] {
        if (i as usize) < self.theme.palette.len() {
            let c = self.theme.palette[i as usize];
            return [
                c.r() as f32 / 255.0,
                c.g() as f32 / 255.0,
                c.b() as f32 / 255.0,
                1.0,
            ];
        }
        if i >= 16 && i <= 231 {
            let n = i - 16;
            let r = n / 36;
            let g = (n % 36) / 6;
            let b = n % 6;
            let lvl = |c: u8| -> f32 {
                if c == 0 { 0.0 } else { (55.0 + c as f32 * 40.0) / 255.0 }
            };
            return [lvl(r), lvl(g), lvl(b), 1.0];
        }
        let g = (8 + (i - 232) * 10) as f32 / 255.0;
        [g, g, g, 1.0]
    }
}
