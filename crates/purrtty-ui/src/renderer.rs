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
use crate::theme::{srgb_to_linear, RendererConfig, Theme};

const PAD_X: f32 = 16.0;
const PAD_Y: f32 = 16.0;

/// Rectangles for a single tab in the tab bar, used for mouse
/// hit-testing. All coordinates are physical pixels relative to the
/// top-left of the window surface.
#[derive(Debug, Clone, Copy)]
pub struct TabLayout {
    pub index: usize,
    /// `(x, y, w, h)` of the whole tab cell.
    pub tab: (f32, f32, f32, f32),
    /// `(x, y, w, h)` of the close "×" button.
    pub close_button: (f32, f32, f32, f32),
}

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
    /// Tab bar state: `(active_index, total_tab_count)`. The bar is
    /// drawn only when `total > 1` so a single-tab session looks the
    /// same as before.
    tab_info: Option<(usize, usize)>,
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
            tab_info: None,
        })
    }

    /// Height reserved for the tab bar, in pixels. Always reserved
    /// when the app has set tab info (even for a single tab), so the
    /// bar becomes a permanent part of the window chrome. Sized around
    /// the line height so it scales with font zoom.
    pub fn tab_bar_height(&self) -> f32 {
        if self.tab_info.is_some() {
            (self.line_height + 12.0).max(32.0)
        } else {
            0.0
        }
    }

    pub fn set_tab_info(&mut self, active: usize, total: usize) {
        self.tab_info = if total >= 1 { Some((active, total)) } else { None };
    }

    /// Compute per-tab rectangles for hit-testing (clicks). All
    /// dimensions scale with `cell_width` / `line_height` so the tab
    /// bar grows proportionally under Cmd+/- font zoom — tab labels
    /// don't overflow when the font gets bigger.
    pub fn tab_layout(&self) -> Vec<TabLayout> {
        let Some((_, tab_count)) = self.tab_info else { return Vec::new() };
        let bar_w = self.config.width as f32;
        let bar_h = self.tab_bar_height();
        let cell_w = self.glyphs.cell_width;
        let line_h = self.line_height;
        // A tab must fit at least "Tab N" (5 chars) + padding + × +
        // padding, all measured in current cell widths so the layout
        // scales with font size.
        let min_tab_w = (cell_w * 12.0).max(100.0);
        let max_tab_w = (cell_w * 22.0).max(220.0);
        let tab_w = (bar_w / tab_count as f32).clamp(min_tab_w, max_tab_w);
        // Close button scales with line height so it stays in
        // proportion with the text.
        let close_size = (line_h * 0.85).max(16.0);
        let close_pad = (cell_w * 0.6).max(8.0);
        let mut out = Vec::with_capacity(tab_count);
        for i in 0..tab_count {
            let x = i as f32 * tab_w;
            let close_x = x + tab_w - close_size - close_pad;
            let close_y = (bar_h - close_size) * 0.5;
            out.push(TabLayout {
                index: i,
                tab: (x, 0.0, tab_w, bar_h),
                close_button: (close_x, close_y, close_size, close_size),
            });
        }
        out
    }

    pub fn grid_dimensions(&self) -> (u16, u16) {
        let tab_h = self.tab_bar_height();
        let w = (self.config.width as f32 - 2.0 * PAD_X).max(0.0);
        let h = (self.config.height as f32 - 2.0 * PAD_Y - tab_h).max(0.0);
        let cols = (w / self.glyphs.cell_width).floor().max(1.0) as u16;
        let rows = (h / self.line_height).floor().max(1.0) as u16;
        (rows, cols)
    }

    /// Layout metrics the app needs for mouse → cell conversion.
    /// `pad_y` already includes the tab bar offset so callers don't
    /// need to know the tab bar exists.
    pub fn cell_metrics(&self) -> (f32, f32, f32, f32) {
        let grid_origin_y = PAD_Y + self.tab_bar_height();
        (PAD_X, grid_origin_y, self.glyphs.cell_width, self.line_height)
    }

    pub fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.device, &self.config);
    }

    pub fn current_font_size(&self) -> f32 {
        self.glyphs.font_size()
    }

    /// Change font size by `delta` pixels. Returns new `(rows, cols)` if
    /// the size actually changed, or `None` if clamped to the same value.
    /// Reuses loaded fonts and GPU pipeline — only the glyph atlas is
    /// cleared (glyphs are re-rasterized lazily on next render).
    pub fn change_font_size(&mut self, delta: f32) -> Option<(u16, u16)> {
        let cur = self.glyphs.font_size();
        let new_size = (cur + delta).clamp(8.0, 72.0);
        if (new_size - cur).abs() < 0.1 {
            return None;
        }
        let new_line_height = (new_size * 1.222).round();
        self.glyphs.rebuild_for_size(&self.queue, new_size, new_line_height);
        self.line_height = new_line_height;
        Some(self.grid_dimensions())
    }

    /// Optional selection highlight range expressed in absolute row
    /// coordinates (scrollback + live rows, row 0 = oldest scrollback).
    /// `end` is exclusive — the last selected cell is `(end_row, end_col - 1)`.
    ///
    /// `hovered_url` highlights a URL currently under the mouse in
    /// view-row coordinates `(view_row, start_col, end_col)` so the
    /// renderer can paint it in a link color with an underline.
    pub fn render_with_selection(
        &mut self,
        grid: &Grid,
        scroll_offset: usize,
        selection: Option<((usize, usize), (usize, usize))>,
        hovered_url: Option<(usize, usize, usize)>,
    ) -> Result<()> {
        self.render_impl(grid, scroll_offset, selection, hovered_url)
    }

    pub fn render(&mut self, grid: &Grid, scroll_offset: usize) -> Result<()> {
        self.render_impl(grid, scroll_offset, None, None)
    }

    fn render_impl(
        &mut self,
        grid: &Grid,
        scroll_offset: usize,
        selection: Option<((usize, usize), (usize, usize))>,
        hovered_url: Option<(usize, usize, usize)>,
    ) -> Result<()> {
        let rows = grid.rows();
        let cols = grid.cols();
        let cell_w = self.glyphs.cell_width;
        let line_h = self.line_height;
        let ascent = self.glyphs.ascent;
        let tab_h = self.tab_bar_height();
        let grid_top = PAD_Y + tab_h;

        let mut glyph_verts: Vec<GlyphVertex> = Vec::with_capacity(rows * cols);
        let mut bg_verts: Vec<QuadVertex> = Vec::new();

        // Link accent color for hovered URLs.
        let link_color = [
            srgb_to_linear(0.36),
            srgb_to_linear(0.63),
            srgb_to_linear(1.0),
            1.0,
        ];

        for view_idx in 0..rows {
            let row = grid.row_at(view_idx, scroll_offset).unwrap_or(&[]);
            // Is this row the one with the hovered URL?
            let hover_in_row = hovered_url.and_then(|(r, s, e)| {
                if r == view_idx { Some((s, e)) } else { None }
            });
            for (col_idx, cell) in row.iter().enumerate() {
                if col_idx >= cols {
                    break;
                }
                if cell.ch == WIDE_CONT {
                    continue;
                }

                let (base_fg, bg_opt) = self.cell_colors(cell);
                // Override fg with link color when this cell is inside
                // the currently-hovered URL span.
                let fg_color = match hover_in_row {
                    Some((s, e)) if col_idx >= s && col_idx < e => link_color,
                    _ => base_fg,
                };
                let cell_x = PAD_X + col_idx as f32 * cell_w;
                let cell_y = grid_top + view_idx as f32 * line_h;

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
                let y = grid_top + cursor.row as f32 * line_h;
                let cursor_gray = srgb_to_linear(0.85);
                QuadRenderer::push_rect(
                    &mut overlay_verts,
                    x,
                    y,
                    cell_w,
                    line_h,
                    [cursor_gray, cursor_gray, cursor_gray, 0.4],
                );
            }
        }

        // Hovered-URL underline. Drawn as a 2px line at the bottom of
        // the hovered cells, using the same link accent color.
        if let Some((view_row, start_col, end_col)) = hovered_url {
            if view_row < rows && start_col < cols && end_col <= cols && end_col > start_col {
                let x = PAD_X + start_col as f32 * cell_w;
                let y = grid_top + view_row as f32 * line_h + line_h - 2.0;
                let w = (end_col - start_col) as f32 * cell_w;
                QuadRenderer::push_rect(
                    &mut overlay_verts,
                    x,
                    y,
                    w,
                    2.0,
                    link_color,
                );
            }
        }

        // Selection highlight. The `selection` range is in absolute row
        // coordinates; we translate each visible row to an absolute row
        // and emit a highlight quad for any range within this row.
        if let Some(((start_row, start_col), (end_row, end_col))) = selection {
            let sb_len = grid.scrollback_len();
            let first_abs = sb_len.saturating_sub(scroll_offset.min(sb_len));
            let select_color = [
                srgb_to_linear(0.26),
                srgb_to_linear(0.44),
                srgb_to_linear(0.78),
                0.42,
            ];
            for view_idx in 0..rows {
                let abs_row = first_abs + view_idx;
                if abs_row < start_row || abs_row > end_row {
                    continue;
                }
                let row_start = if abs_row == start_row { start_col } else { 0 };
                let row_end = if abs_row == end_row { end_col } else { cols };
                let row_start = row_start.min(cols);
                let row_end = row_end.min(cols);
                if row_end <= row_start {
                    continue;
                }
                let x = PAD_X + row_start as f32 * cell_w;
                let y = grid_top + view_idx as f32 * line_h;
                let w = (row_end - row_start) as f32 * cell_w;
                QuadRenderer::push_rect(&mut overlay_verts, x, y, w, line_h, select_color);
            }
        }

        // Tab bar — modeled after VS Code / Warp: a subtle dark strip
        // across the top with an active tab that "sinks" into the main
        // grid (same bg) plus a thin bottom accent line.
        if let Some((active_tab, tab_count)) = self.tab_info {
            let bar_h = tab_h;
            let bar_w = self.config.width as f32;

            // Bar background: slightly lighter than the main bg so
            // the active tab (which uses the main bg) reads as "in
            // front" of the bar.
            let bar_bg = [
                srgb_to_linear(0.145),
                srgb_to_linear(0.145),
                srgb_to_linear(0.150),
                1.0,
            ];
            QuadRenderer::push_rect(&mut bg_verts, 0.0, 0.0, bar_w, bar_h, bar_bg);

            // A single-pixel separator line at the very bottom of the
            // bar, so the active tab's "missing" line reads as a cut
            // into the bar.
            let separator = [
                srgb_to_linear(0.08),
                srgb_to_linear(0.08),
                srgb_to_linear(0.10),
                1.0,
            ];
            QuadRenderer::push_rect(&mut bg_verts, 0.0, bar_h - 1.0, bar_w, 1.0, separator);

            // Tab sizing: flexible up to a cap, min width keeps short
            // labels readable.
            let usable = bar_w.max(0.0);
            let max_tab_w: f32 = 220.0;
            let min_tab_w: f32 = 100.0;
            let tab_w = (usable / tab_count as f32).clamp(min_tab_w, max_tab_w);

            // Main-bg color — used to "cut" the active tab out of the bar.
            let main_bg = self.theme.background.as_array();
            // Accent (link blue) — same as URL hover color for visual unity.
            let accent = [
                srgb_to_linear(0.36),
                srgb_to_linear(0.63),
                srgb_to_linear(1.0),
                1.0,
            ];
            // Active-tab label color: full foreground.
            let active_text = {
                let c = self.theme.foreground;
                [
                    srgb_to_linear(c.r() as f32 / 255.0),
                    srgb_to_linear(c.g() as f32 / 255.0),
                    srgb_to_linear(c.b() as f32 / 255.0),
                    1.0,
                ]
            };
            // Inactive-tab label color: dimmed ~55% gray.
            let inactive_text = [
                srgb_to_linear(0.52),
                srgb_to_linear(0.52),
                srgb_to_linear(0.56),
                1.0,
            ];

            // Precomputed layout so rendering and hit-testing agree
            // on where each tab (and its × button) lives.
            let layouts = self.tab_layout();
            for layout in &layouts {
                let i = layout.index;
                let (x, _, tw, _) = layout.tab;
                if i == active_tab {
                    // Active tab bg = main grid bg, making it look
                    // like the grid "rises through" the bar.
                    QuadRenderer::push_rect(&mut bg_verts, x, 0.0, tw, bar_h, main_bg);
                    // Accent line at the top of the active tab.
                    QuadRenderer::push_rect(&mut bg_verts, x, 0.0, tw, 2.0, accent);
                }

                // Tab label — centered, leaving room for the × button
                // on the right side.
                let (cx, cy, cw, ch) = layout.close_button;
                let label_area_right = cx - 4.0;
                let label_area_left = x + 10.0;
                let label = format!("Tab {}", i + 1);
                let label_w = label.chars().count() as f32 * cell_w;
                let center = (label_area_left + label_area_right) * 0.5;
                let glyph_x_start = (center - label_w * 0.5).max(label_area_left);
                let glyph_y = (bar_h - line_h) * 0.5;
                let text_color = if i == active_tab { active_text } else { inactive_text };
                let mut glyph_x = glyph_x_start;
                for lch in label.chars() {
                    if glyph_x + cell_w > label_area_right {
                        break;
                    }
                    if let Some(entry) =
                        self.glyphs.get_or_insert(lch, &self.device, &self.queue)
                    {
                        GlyphCache::push_glyph(
                            &mut glyph_verts,
                            &entry,
                            glyph_x,
                            glyph_y,
                            ascent,
                            text_color,
                        );
                        glyph_x += cell_w;
                    }
                }

                // Close button: an × glyph centered inside its hit
                // area. Skipped when there's only one tab — you can't
                // close the last one from the bar; use the window
                // close button for that.
                if tab_count > 1 {
                    let close_color = if i == active_tab {
                        active_text
                    } else {
                        inactive_text
                    };
                    let close_glyph_x = cx + (cw - cell_w) * 0.5;
                    let close_glyph_y = cy + (ch - line_h) * 0.5;
                    if let Some(entry) =
                        self.glyphs.get_or_insert('×', &self.device, &self.queue)
                    {
                        GlyphCache::push_glyph(
                            &mut glyph_verts,
                            &entry,
                            close_glyph_x,
                            close_glyph_y,
                            ascent,
                            close_color,
                        );
                    }
                }

                // Thin vertical separator between inactive tabs (not
                // at the edges of the active tab — those are already
                // defined by the bg color contrast).
                if i + 1 < tab_count && i != active_tab && i + 1 != active_tab {
                    let sep_x = x + tw;
                    let sep_color = [
                        srgb_to_linear(0.09),
                        srgb_to_linear(0.09),
                        srgb_to_linear(0.11),
                        1.0,
                    ];
                    QuadRenderer::push_rect(
                        &mut bg_verts,
                        sep_x - 0.5,
                        6.0,
                        1.0,
                        bar_h - 12.0,
                        sep_color,
                    );
                }
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
        srgb_u8_to_linear_rgba(c.r(), c.g(), c.b(), c.a())
    }

    fn resolve_color(&self, c: TermColor, default: [f32; 4]) -> [f32; 4] {
        match c {
            TermColor::Default => default,
            TermColor::Indexed(i) => self.indexed_color(i),
            TermColor::Rgb(r, g, b) => srgb_u8_to_linear_rgba(r, g, b, 255),
        }
    }

    fn indexed_color(&self, i: u8) -> [f32; 4] {
        if (i as usize) < self.theme.palette.len() {
            let c = self.theme.palette[i as usize];
            return srgb_u8_to_linear_rgba(c.r(), c.g(), c.b(), 255);
        }
        if i >= 16 && i <= 231 {
            let n = i - 16;
            let r = n / 36;
            let g = (n % 36) / 6;
            let b = n % 6;
            let lvl = |c: u8| -> u8 {
                if c == 0 { 0 } else { 55 + c * 40 }
            };
            return srgb_u8_to_linear_rgba(lvl(r), lvl(g), lvl(b), 255);
        }
        let g = 8 + (i - 232) * 10;
        srgb_u8_to_linear_rgba(g, g, g, 255)
    }
}

/// Convert a u8 sRGB RGBA tuple into a linear RGBA `[f32; 4]` suitable
/// for an sRGB-encoded wgpu surface. Alpha is never transformed — only
/// the color channels carry the sRGB gamma curve.
fn srgb_u8_to_linear_rgba(r: u8, g: u8, b: u8, a: u8) -> [f32; 4] {
    [
        srgb_to_linear(r as f32 / 255.0),
        srgb_to_linear(g as f32 / 255.0),
        srgb_to_linear(b as f32 / 255.0),
        a as f32 / 255.0,
    ]
}
