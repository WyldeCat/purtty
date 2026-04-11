//! Glyph cache backed by platform-native font rasterization (Core Text
//! on macOS, FreeType on Linux) via `font-kit`. Rasterized glyph bitmaps
//! are uploaded to a wgpu texture atlas and drawn as textured quads.
//!
//! This replaces glyphon/cosmic-text/swash for text rendering, producing
//! font quality that matches native macOS apps.

use std::collections::HashMap;

use anyhow::{anyhow, Context, Result};
use bytemuck::{Pod, Zeroable};
use font_kit::canvas::{Canvas, Format, RasterizationOptions};
use font_kit::family_name::FamilyName;
use font_kit::font::Font;
use font_kit::hinting::HintingOptions;
use font_kit::metrics::Metrics as FKMetrics;
use font_kit::properties::Properties;
use font_kit::source::SystemSource;
use pathfinder_geometry::transform2d::Transform2F;
use pathfinder_geometry::vector::Vector2I;
use wgpu::util::DeviceExt;

const ATLAS_SIZE: u32 = 2048;

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct GlyphVertex {
    pub pos: [f32; 2],
    pub uv: [f32; 2],
    pub color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct GlyphUniform {
    resolution: [f32; 2],
    atlas_size: [f32; 2],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct GlyphKey(char);

#[derive(Debug, Clone, Copy)]
pub struct GlyphEntry {
    pub atlas_x: u32,
    pub atlas_y: u32,
    pub width: u32,
    pub height: u32,
    pub bearing_x: f32,
    pub bearing_y: f32,
}

const GLYPH_SHADER: &str = r#"
struct Uniform {
    resolution: vec2<f32>,
    atlas_size: vec2<f32>,
};

@group(0) @binding(0) var<uniform> u: Uniform;
@group(0) @binding(1) var glyph_tex: texture_2d<f32>;
@group(0) @binding(2) var glyph_samp: sampler;

struct VIn {
    @location(0) pos: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec4<f32>,
};

struct VOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
};

@vertex
fn vs_main(in: VIn) -> VOut {
    var out: VOut;
    let ndc = vec2<f32>(
        in.pos.x / u.resolution.x * 2.0 - 1.0,
        1.0 - in.pos.y / u.resolution.y * 2.0
    );
    out.clip_pos = vec4<f32>(ndc, 0.0, 1.0);
    out.uv = in.uv / u.atlas_size;
    out.color = in.color;
    return out;
}

@fragment
fn fs_main(in: VOut) -> @location(0) vec4<f32> {
    let alpha = textureSample(glyph_tex, glyph_samp, in.uv).r;
    return vec4<f32>(in.color.rgb, in.color.a * alpha);
}
"#;

pub struct GlyphCache {
    font: Font,
    /// Fallback fonts tried when the primary font lacks a glyph (CJK, emoji, etc.)
    fallback_fonts: Vec<Font>,
    font_size: f32,

    atlas_texture: wgpu::Texture,
    atlas_view: wgpu::TextureView,
    entries: HashMap<GlyphKey, GlyphEntry>,
    pack_x: u32,
    pack_y: u32,
    row_h: u32,

    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    uniform_buffer: wgpu::Buffer,
    vertex_buffer: wgpu::Buffer,
    vertex_cap: u64,
    vertex_count: u32,

    pub cell_width: f32,
    pub line_height: f32,
    pub ascent: f32,
}

impl GlyphCache {
    pub fn font_size(&self) -> f32 {
        self.font_size
    }

    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        surface_format: wgpu::TextureFormat,
        font_family: Option<&str>,
        font_size: f32,
        line_height: f32,
    ) -> Result<Self> {
        let families: Vec<FamilyName> = match font_family {
            Some(name) => vec![FamilyName::Title(name.to_string()), FamilyName::Monospace],
            None => vec![FamilyName::Monospace],
        };
        let font = SystemSource::new()
            .select_best_match(&families, &Properties::new())
            .map_err(|e| anyhow!("font select: {e}"))?
            .load()
            .map_err(|e| anyhow!("font load: {e}"))?;

        let fk_metrics: FKMetrics = font.metrics();
        let scale = font_size / fk_metrics.units_per_em as f32;
        let ascent = fk_metrics.ascent * scale;
        let cell_width = Self::measure_advance(&font, font_size);

        // Load fallback fonts for glyphs missing from the primary (CJK, emoji, etc.)
        let fallback_names = [
            "Apple SD Gothic Neo",  // Korean
            "PingFang SC",          // Chinese Simplified
            "Hiragino Sans",        // Japanese
            "Apple Color Emoji",    // Emoji
            "Arial Unicode MS",     // Broad Unicode coverage
        ];
        let mut fallback_fonts = Vec::new();
        let source = SystemSource::new();
        for name in &fallback_names {
            if let Ok(handle) = source.select_best_match(
                &[FamilyName::Title(name.to_string())],
                &Properties::new(),
            ) {
                if let Ok(f) = handle.load() {
                    tracing::debug!(fallback = name, "loaded fallback font");
                    fallback_fonts.push(f);
                }
            }
        }

        tracing::debug!(
            cell_width,
            ascent,
            line_height,
            font_name = ?font.full_name(),
            fallbacks = fallback_fonts.len(),
            "glyph cache initialized"
        );

        // Atlas texture (R8, grayscale alpha).
        let atlas_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("purrtty.glyph_atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_SIZE,
                height: ATLAS_SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let atlas_view = atlas_texture.create_view(&Default::default());

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("purrtty.glyph_sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("purrtty.glyph.shader"),
            source: wgpu::ShaderSource::Wgsl(GLYPH_SHADER.into()),
        });

        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("purrtty.glyph.bgl"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::VERTEX,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("purrtty.glyph.layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("purrtty.glyph.pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<GlyphVertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &[
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x2,
                            offset: 0,
                            shader_location: 0,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x2,
                            offset: 8,
                            shader_location: 1,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: 16,
                            shader_location: 2,
                        },
                    ],
                }],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let uniform_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("purrtty.glyph.uniform"),
            contents: bytemuck::bytes_of(&GlyphUniform {
                resolution: [1.0, 1.0],
                atlas_size: [ATLAS_SIZE as f32, ATLAS_SIZE as f32],
            }),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("purrtty.glyph.bg"),
            layout: &bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });

        let initial_cap = 1024u64 * std::mem::size_of::<GlyphVertex>() as u64;
        let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("purrtty.glyph.vbo"),
            size: initial_cap,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Ok(Self {
            font,
            fallback_fonts,
            font_size,
            atlas_texture,
            atlas_view,
            entries: HashMap::new(),
            pack_x: 0,
            pack_y: 0,
            row_h: 0,
            pipeline,
            bind_group,
            uniform_buffer,
            vertex_buffer,
            vertex_cap: initial_cap,
            vertex_count: 0,
            cell_width,
            line_height,
            ascent,
        })
    }

    /// Change the font size without reloading fonts or recreating the GPU
    /// pipeline. Clears cached glyphs and zeroes the atlas texture so new
    /// glyphs don't bleed into each other through stale pixels.
    pub fn rebuild_for_size(&mut self, queue: &wgpu::Queue, new_size: f32, new_line_height: f32) {
        let fk_metrics = self.font.metrics();
        let scale = new_size / fk_metrics.units_per_em as f32;
        self.ascent = fk_metrics.ascent * scale;
        self.cell_width = Self::measure_advance(&self.font, new_size);
        self.font_size = new_size;
        self.line_height = new_line_height;

        // Clear atlas packing state — glyphs will be re-rasterized lazily.
        self.entries.clear();
        self.pack_x = 0;
        self.pack_y = 0;
        self.row_h = 0;

        // Zero the atlas texture so stale pixels from the old size don't
        // bleed into the new glyphs via linear sampling at cell edges.
        let zero = vec![0u8; (ATLAS_SIZE * ATLAS_SIZE) as usize];
        queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &self.atlas_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &zero,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(ATLAS_SIZE),
                rows_per_image: None,
            },
            wgpu::Extent3d {
                width: ATLAS_SIZE,
                height: ATLAS_SIZE,
                depth_or_array_layers: 1,
            },
        );
    }

    fn measure_advance(font: &Font, size: f32) -> f32 {
        if let Some(gid) = font.glyph_for_char('M') {
            if let Ok(adv) = font.advance(gid) {
                let scale = size / font.metrics().units_per_em as f32;
                return adv.x() * scale;
            }
        }
        size * 0.6
    }

    /// Get or rasterize a glyph. Returns `None` for whitespace / missing
    /// glyphs (the caller just skips them).
    pub fn get_or_insert(
        &mut self,
        ch: char,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Option<GlyphEntry> {
        let key = GlyphKey(ch);
        if let Some(&entry) = self.entries.get(&key) {
            return Some(entry);
        }
        if ch == ' ' || ch == '\0' {
            return None;
        }
        // Try primary font first, then each fallback.
        if let Some(entry) = self.rasterize_with_font(&self.font.clone(), ch, queue) {
            return Some(entry);
        }
        for i in 0..self.fallback_fonts.len() {
            let font = self.fallback_fonts[i].clone();
            if let Some(entry) = self.rasterize_with_font(&font, ch, queue) {
                return Some(entry);
            }
        }
        None
    }

    fn rasterize_with_font(
        &mut self,
        font: &Font,
        ch: char,
        queue: &wgpu::Queue,
    ) -> Option<GlyphEntry> {
        let glyph_id = font.glyph_for_char(ch)?;

        let hinting = HintingOptions::Full(self.font_size);
        let raster_opts = RasterizationOptions::GrayscaleAa;

        let bounds = font
            .raster_bounds(
                glyph_id,
                self.font_size,
                Transform2F::default(),
                hinting,
                raster_opts,
            )
            .ok()?;

        let w = bounds.width() as u32;
        let h = bounds.height() as u32;
        if w == 0 || h == 0 {
            return None;
        }

        // Pack into atlas.
        if self.pack_x + w > ATLAS_SIZE {
            self.pack_x = 0;
            self.pack_y += self.row_h;
            self.row_h = 0;
        }
        if self.pack_y + h > ATLAS_SIZE {
            tracing::warn!("glyph atlas full");
            return None;
        }

        let origin = bounds.origin();
        let transform = Transform2F::from_translation(-origin.to_f32());
        let mut canvas = Canvas::new(Vector2I::new(w as i32, h as i32), Format::A8);
        font.rasterize_glyph(
                &mut canvas,
                glyph_id,
                self.font_size,
                transform,
                hinting,
                raster_opts,
            )
            .ok()?;

        // Upload to atlas texture.
        queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &self.atlas_texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: self.pack_x,
                    y: self.pack_y,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            &canvas.pixels,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(w),
                rows_per_image: None,
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );

        let entry = GlyphEntry {
            atlas_x: self.pack_x,
            atlas_y: self.pack_y,
            width: w,
            height: h,
            bearing_x: origin.x() as f32,
            bearing_y: origin.y() as f32,
        };

        self.pack_x += w + 1; // 1px gap to avoid bleed
        self.row_h = self.row_h.max(h + 1);
        self.entries.insert(GlyphKey(ch), entry);
        Some(entry)
    }

    /// Push a glyph quad into the vertex list at the given screen
    /// position. `x, y` is the top-left of the cell.
    pub fn push_glyph(
        verts: &mut Vec<GlyphVertex>,
        entry: &GlyphEntry,
        cell_x: f32,
        cell_y: f32,
        ascent: f32,
        color: [f32; 4],
    ) {
        // Position the glyph bitmap relative to the cell's baseline.
        let x = cell_x + entry.bearing_x;
        let y = cell_y + ascent + entry.bearing_y;
        let x2 = x + entry.width as f32;
        let y2 = y + entry.height as f32;

        let u = entry.atlas_x as f32;
        let v = entry.atlas_y as f32;
        let u2 = u + entry.width as f32;
        let v2 = v + entry.height as f32;

        let vtx = |px, py, tu, tv| GlyphVertex {
            pos: [px, py],
            uv: [tu, tv],
            color,
        };

        verts.push(vtx(x, y, u, v));
        verts.push(vtx(x2, y, u2, v));
        verts.push(vtx(x, y2, u, v2));
        verts.push(vtx(x, y2, u, v2));
        verts.push(vtx(x2, y, u2, v));
        verts.push(vtx(x2, y2, u2, v2));
    }

    pub fn update_resolution(&self, queue: &wgpu::Queue, width: u32, height: u32) {
        queue.write_buffer(
            &self.uniform_buffer,
            0,
            bytemuck::bytes_of(&GlyphUniform {
                resolution: [width as f32, height as f32],
                atlas_size: [ATLAS_SIZE as f32, ATLAS_SIZE as f32],
            }),
        );
    }

    pub fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, verts: &[GlyphVertex]) {
        if verts.is_empty() {
            self.vertex_count = 0;
            return;
        }
        let needed = (verts.len() * std::mem::size_of::<GlyphVertex>()) as u64;
        if needed > self.vertex_cap {
            let new_cap = needed.next_power_of_two();
            self.vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("purrtty.glyph.vbo"),
                size: new_cap,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.vertex_cap = new_cap;
        }
        queue.write_buffer(&self.vertex_buffer, 0, bytemuck::cast_slice(verts));
        self.vertex_count = verts.len() as u32;
    }

    pub fn render<'a>(&'a self, pass: &mut wgpu::RenderPass<'a>) {
        if self.vertex_count == 0 {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
        pass.draw(0..self.vertex_count, 0..1);
    }
}
