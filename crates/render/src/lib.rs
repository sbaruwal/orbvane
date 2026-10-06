//! GPU renderer for the workbench: rounded/bordered quads, text, vector icons and images, drawn
//! in layers.
//!
//! Callers work in logical pixels through [`Canvas`]; everything is converted to physical pixels
//! for the GPU.

mod atlas;
mod text;

use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
pub use text::{char_cells, Font, Icon, TextStyle, ICON_TURNS};
use text::TextSystem;
pub use theme::Color;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }

    pub fn inset(&self, dx: f32, dy: f32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, (self.w - 2.0 * dx).max(0.0), (self.h - 2.0 * dy).max(0.0))
    }

    pub fn intersect(&self, o: &Rect) -> Rect {
        let x = self.x.max(o.x);
        let y = self.y.max(o.y);
        Rect::new(x, y, (self.right().min(o.right()) - x).max(0.0), (self.bottom().min(o.bottom()) - y).max(0.0))
    }

    /// Splits off `h` from the top, returning (top, rest).
    pub fn cut_top(&self, h: f32) -> (Rect, Rect) {
        let h = h.min(self.h);
        (Rect::new(self.x, self.y, self.w, h), Rect::new(self.x, self.y + h, self.w, self.h - h))
    }

    pub fn cut_bottom(&self, h: f32) -> (Rect, Rect) {
        let h = h.min(self.h);
        (Rect::new(self.x, self.y, self.w, self.h - h), Rect::new(self.x, self.bottom() - h, self.w, h))
    }

    pub fn cut_left(&self, w: f32) -> (Rect, Rect) {
        let w = w.min(self.w);
        (Rect::new(self.x, self.y, w, self.h), Rect::new(self.x + w, self.y, self.w - w, self.h))
    }

    pub fn cut_right(&self, w: f32) -> (Rect, Rect) {
        let w = w.min(self.w);
        (Rect::new(self.x, self.y, self.w - w, self.h), Rect::new(self.right() - w, self.y, w, self.h))
    }

    pub fn center_y(&self) -> f32 {
        self.y + self.h / 2.0
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct QuadInstance {
    rect: [f32; 4],
    color: [f32; 4],
    clip: [f32; 4],
    params: [f32; 4],
    border_color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GlyphInstance {
    rect: [f32; 4],
    uv: [f32; 4],
    color: [f32; 4],
    clip: [f32; 4],
    kind: u32,
    _pad: [u32; 3],
}

/// An RGBA picture (straight alpha, rows top to bottom). `id` identifies its pixels: the
/// renderer keeps one texture per id while it's being drawn.
pub struct Image {
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Image {
    /// An image with a fresh id.
    pub fn new(width: u32, height: u32, rgba: Vec<u8>) -> Self {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self { id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed), width, height, rgba }
    }
}

#[derive(Default)]
struct Layer {
    quads: Vec<QuadInstance>,
    glyphs: Vec<GlyphInstance>,
    /// Images, drawn after the layer's quads and before its text.
    images: Vec<(Arc<Image>, GlyphInstance)>,
}

/// Immediate-mode drawing surface for one frame.
pub struct Canvas<'a> {
    layers: &'a mut Vec<Layer>,
    text: &'a mut TextSystem,
    scale: f32,
    clip_stack: Vec<[f32; 4]>,
    size: (f32, f32),
}

impl Canvas<'_> {
    /// Logical size of the window.
    pub fn size(&self) -> (f32, f32) {
        self.size
    }

    pub fn scale(&self) -> f32 {
        self.scale
    }

    fn clip(&self) -> [f32; 4] {
        *self.clip_stack.last().unwrap()
    }

    fn layer(&mut self) -> &mut Layer {
        self.layers.last_mut().unwrap()
    }

    /// Starts a new layer drawn above everything so far (menus, popups, the command palette).
    pub fn push_layer(&mut self) {
        self.layers.push(Layer::default());
    }

    pub fn push_clip(&mut self, r: Rect) {
        let s = self.scale;
        let [x0, y0, x1, y1] = self.clip();
        self.clip_stack.push([
            (r.x * s).max(x0),
            (r.y * s).max(y0),
            (r.right() * s).min(x1),
            (r.bottom() * s).min(y1),
        ]);
    }

    pub fn pop_clip(&mut self) {
        if self.clip_stack.len() > 1 {
            self.clip_stack.pop();
        }
    }

    fn quad(&mut self, r: Rect, color: Color, radius: f32, border: f32, border_color: Color) {
        if color.a <= 0.0 && (border <= 0.0 || border_color.a <= 0.0) {
            return;
        }
        let s = self.scale;
        let clip = self.clip();
        self.layer().quads.push(QuadInstance {
            rect: [r.x * s, r.y * s, r.w * s, r.h * s],
            color: color.to_array(),
            clip,
            params: [radius * s, border * s, 0.0, 0.0],
            border_color: border_color.to_array(),
        });
    }

    pub fn fill(&mut self, r: Rect, color: Color) {
        self.quad(r, color, 0.0, 0.0, Color::TRANSPARENT);
    }

    /// Draws `image` stretched over `r`.
    pub fn image(&mut self, r: Rect, image: &Arc<Image>) {
        let s = self.scale;
        let clip = self.clip();
        let inst = GlyphInstance { rect: [r.x * s, r.y * s, r.w * s, r.h * s], uv: [0.0, 0.0, 1.0, 1.0], color: [1.0; 4], clip, kind: 2, _pad: [0; 3] };
        self.layer().images.push((image.clone(), inst));
    }

    pub fn fill_rounded(&mut self, r: Rect, color: Color, radius: f32) {
        self.quad(r, color, radius, 0.0, Color::TRANSPARENT);
    }

    /// A rectangle with a border drawn inside its bounds.
    pub fn bordered(&mut self, r: Rect, fill: Color, border: Color, width: f32, radius: f32) {
        self.quad(r, fill, radius, width, border);
    }

    /// A drop shadow approximated with a few expanding translucent rounded rects.
    pub fn shadow(&mut self, r: Rect, radius: f32, color: Color) {
        for i in 1..=6 {
            let spread = i as f32 * 2.0;
            let rr = Rect::new(r.x - spread, r.y - spread + 2.0, r.w + spread * 2.0, r.h + spread * 2.0);
            self.fill_rounded(rr, color.with_alpha(color.a / 6.0), radius + spread);
        }
    }

    /// Draws the interface in the editor's font (true) or the system's.
    pub fn set_ui_mono(&mut self, mono: bool) {
        self.text.set_ui_mono(mono);
    }

    /// Sets the monospace font from a family list like `editor.fontFamily`. Returns the first
    /// family if it isn't installed (when the list changed).
    pub fn set_mono_font(&mut self, families: &str) -> Vec<String> {
        self.text.set_mono_families(families)
    }

    pub fn measure(&mut self, text: &str, style: &TextStyle) -> f32 {
        self.text.shape(text, &[], style).width
    }

    /// Draws one line of text with its line box starting at (x, y). Returns the advance width.
    pub fn text(&mut self, x: f32, y: f32, text: &str, style: &TextStyle) -> f32 {
        self.rich_text(x, y, text, &[], style)
    }

    /// Draws text vertically centered in `r`, starting at its left edge.
    pub fn text_in(&mut self, r: Rect, text: &str, style: &TextStyle) -> f32 {
        let y = r.y + ((r.h - style.line_height) / 2.0).round();
        self.text(r.x, y, text, style)
    }

    /// Like `text_in`, but text wider than `r` is cut short with an ellipsis. Returns the
    /// drawn width.
    pub fn text_fit(&mut self, r: Rect, text: &str, style: &TextStyle) -> f32 {
        if r.w <= 0.0 {
            return 0.0;
        }
        if self.measure(text, style) <= r.w {
            return self.text_in(r, text, style);
        }
        // The longest prefix that fits with the ellipsis (binary search over char counts).
        let ends: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
        let (mut lo, mut hi) = (0, ends.len());
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            if self.measure(&format!("{}…", text[..ends[mid]].trim_end()), style) <= r.w {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        if lo == 0 {
            return 0.0;
        }
        self.text_in(r, &format!("{}…", text[..ends[lo]].trim_end()), style)
    }

    /// Draws text where byte ranges `spans` get their own colors (syntax highlighting).
    pub fn rich_text(&mut self, x: f32, y: f32, text: &str, spans: &[(usize, usize, Color)], style: &TextStyle) -> f32 {
        if text.is_empty() {
            return 0.0;
        }
        let s = self.scale;
        let clip = self.clip();
        let line = self.text.shape(text, spans, style);
        let width = line.width;
        let glyphs = line.glyphs.clone();
        let baseline = line.baseline;
        let origin = ((x * s).round(), ((y + baseline) * s).round());
        let inv = 1.0 / atlas::ATLAS_SIZE as f32;
        let mut out = Vec::with_capacity(glyphs.len());
        for g in &glyphs {
            let pg = g.physical(origin, s);
            let gx = pg.x as f32;
            // Skip glyphs fully outside the clip rect.
            if gx > clip[2] || gx + g.w * s * 2.0 < clip[0] {
                continue;
            }
            let Some((slot, [left, top], is_color)) = self.text.glyph(pg.cache_key) else { continue };
            let color = g.color_opt.map_or(style.color, |c| {
                Color::rgba8(c.r(), c.g(), c.b(), c.a())
            });
            out.push(GlyphInstance {
                rect: [(pg.x + left) as f32, (pg.y - top) as f32, slot.w as f32, slot.h as f32],
                uv: [slot.x as f32 * inv, slot.y as f32 * inv, slot.w as f32 * inv, slot.h as f32 * inv],
                color: color.to_array(),
                clip,
                kind: is_color as u32,
                _pad: [0; 3],
            });
        }
        self.layer().glyphs.extend(out);
        width
    }

    /// Draws a vector icon filling a `size`×`size` box at (x, y).
    pub fn icon(&mut self, icon: &Icon, x: f32, y: f32, size: f32, color: Color) {
        self.icon_turned(icon, x, y, size, color, 0);
    }

    /// Draws an icon rotated by `turn` of `ICON_TURNS` steps (a spinner advances `turn` over time).
    pub fn icon_turned(&mut self, icon: &Icon, x: f32, y: f32, size: f32, color: Color, turn: u32) {
        let s = self.scale;
        let px = (size * s).round() as u32;
        let clip = self.clip();
        let Some(slot) = self.text.icon(icon, px, turn) else { return };
        let inv = 1.0 / atlas::ATLAS_SIZE as f32;
        let glyph = GlyphInstance {
            rect: [(x * s).round(), (y * s).round(), slot.w as f32, slot.h as f32],
            uv: [slot.x as f32 * inv, slot.y as f32 * inv, slot.w as f32 * inv, slot.h as f32 * inv],
            color: color.to_array(),
            clip,
            kind: 0,
            _pad: [0; 3],
        };
        self.layer().glyphs.push(glyph);
    }

    /// Draws an icon centered in `r`.
    pub fn icon_in(&mut self, icon: &Icon, r: Rect, size: f32, color: Color) {
        self.icon(icon, (r.x + (r.w - size) / 2.0).round(), (r.y + (r.h - size) / 2.0).round(), size, color);
    }
}

pub struct Renderer {
    /// The window's surface; None when drawing offscreen (`Renderer::offscreen`).
    surface: Option<wgpu::Surface<'static>>,
    /// The last offscreen frame, RGBA (offscreen only).
    pixels: Vec<u8>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    quad_pipeline: wgpu::RenderPipeline,
    glyph_pipeline: wgpu::RenderPipeline,
    globals_buffer: wgpu::Buffer,
    globals_group: wgpu::BindGroup,
    atlas_group: wgpu::BindGroup,
    quad_buffer: wgpu::Buffer,
    glyph_buffer: wgpu::Buffer,
    image_pipeline: wgpu::RenderPipeline,
    image_layout: wgpu::BindGroupLayout,
    image_sampler: wgpu::Sampler,
    image_buffer: wgpu::Buffer,
    /// A texture per image id, with the frame it was last drawn in.
    textures: std::collections::HashMap<u64, (wgpu::BindGroup, u64)>,
    frame_count: u64,
    text: TextSystem,
    layers: Vec<Layer>,
    scale: f32,
}

impl Renderer {
    /// `target` is the window (anything wgpu can make a surface for); `size` is in physical pixels.
    pub fn new(
        target: impl Into<wgpu::SurfaceTarget<'static>>,
        size: (u32, u32),
        scale: f32,
    ) -> Result<Self, String> {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = wgpu::Backends::METAL;
        let instance = wgpu::Instance::new(desc);
        let surface = instance.create_surface(target).map_err(|e| e.to_string())?;
        let adapter = pollster_block(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))
        .map_err(|e| e.to_string())?;
        let (device, queue) = pollster_block(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("orbvane device"),
            ..Default::default()
        }))
        .map_err(|e| e.to_string())?;

        let mut config = surface
            .get_default_config(&adapter, size.0.max(1), size.1.max(1))
            .ok_or("surface not supported by adapter")?;
        // Blend in sRGB space, like browsers do.
        let caps = surface.get_capabilities(&adapter);
        if let Some(f) = caps.formats.iter().find(|f| !f.is_srgb()) {
            config.format = *f;
        }
        config.present_mode = wgpu::PresentMode::AutoVsync;
        surface.configure(&device, &config);
        Ok(Self::build(Some(surface), device, queue, config, scale))
    }

    /// A renderer that draws into a texture instead of a window, for snapshots of the UI
    /// (`pixels` after each frame). `size` is in physical pixels.
    pub fn offscreen(size: (u32, u32), scale: f32) -> Result<Self, String> {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = wgpu::Backends::METAL;
        let instance = wgpu::Instance::new(desc);
        let adapter = pollster_block(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))
        .map_err(|e| e.to_string())?;
        let (device, queue) = pollster_block(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("orbvane offscreen device"),
            ..Default::default()
        }))
        .map_err(|e| e.to_string())?;
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            format: wgpu::TextureFormat::Bgra8Unorm,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: size.0.max(1),
            height: size.1.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            view_formats: Vec::new(),
        };
        Ok(Self::build(None, device, queue, config, scale))
    }

    /// The last offscreen frame: (width, height, RGBA rows).
    pub fn pixels(&self) -> (u32, u32, &[u8]) {
        (self.config.width, self.config.height, &self.pixels)
    }

    fn build(surface: Option<wgpu::Surface<'static>>, device: wgpu::Device, queue: wgpu::Queue, config: wgpu::SurfaceConfiguration, scale: f32) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("workbench shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });

        let globals_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("globals"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let texture_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let atlas_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("atlas"),
            entries: &[
                texture_entry(0),
                texture_entry(1),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let globals_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globals"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let globals_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("globals"),
            layout: &globals_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: globals_buffer.as_entire_binding() }],
        });

        let text = TextSystem::new(&device, &queue);
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("atlas sampler"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let mask_view = text.mask_atlas.texture.create_view(&Default::default());
        let color_view = text.color_atlas.texture.create_view(&Default::default());
        let atlas_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("atlas"),
            layout: &atlas_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&mask_view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&color_view) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&sampler) },
            ],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("workbench"),
            bind_group_layouts: &[Some(&globals_layout), Some(&atlas_layout)],
            immediate_size: 0,
        });
        // Images: one texture each, in a third bind group.
        let image_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("image"),
            entries: &[
                texture_entry(0),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let image_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("images"),
            bind_group_layouts: &[Some(&globals_layout), Some(&atlas_layout), Some(&image_layout)],
            immediate_size: 0,
        });
        let image_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("image sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let quad_attrs = wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4, 3 => Float32x4, 4 => Float32x4];
        let glyph_attrs = wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4, 3 => Float32x4, 4 => Uint32];
        let pipeline = |label, vs, fs, stride, attrs: &[wgpu::VertexAttribute], layout: &wgpu::PipelineLayout| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some(vs),
                    compilation_options: Default::default(),
                    buffers: &[Some(wgpu::VertexBufferLayout {
                        array_stride: stride,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes: attrs,
                    })],
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: config.format,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let quad_pipeline =
            pipeline("quads", "vs_quad", "fs_quad", size_of::<QuadInstance>() as u64, &quad_attrs, &layout);
        let glyph_pipeline =
            pipeline("glyphs", "vs_glyph", "fs_glyph", size_of::<GlyphInstance>() as u64, &glyph_attrs, &layout);
        let image_pipeline =
            pipeline("images", "vs_glyph", "fs_image", size_of::<GlyphInstance>() as u64, &glyph_attrs, &image_pipeline_layout);

        let quad_buffer = instance_buffer(&device, "quads", 1 << 20);
        let glyph_buffer = instance_buffer(&device, "glyphs", 1 << 20);
        let image_buffer = instance_buffer(&device, "images", 1 << 12);

        Self {
            surface,
            pixels: Vec::new(),
            device,
            queue,
            config,
            quad_pipeline,
            glyph_pipeline,
            globals_buffer,
            globals_group,
            atlas_group,
            quad_buffer,
            glyph_buffer,
            image_pipeline,
            image_layout,
            image_sampler,
            image_buffer,
            textures: Default::default(),
            frame_count: 0,
            text,
            layers: Vec::new(),
            scale,
        }
    }

    /// The size being drawn at, in physical pixels.
    pub fn physical_size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    pub fn resize(&mut self, width: u32, height: u32, scale: f32) {
        self.scale = scale;
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        if let Some(surface) = &self.surface {
            surface.configure(&self.device, &self.config);
        }
    }

    /// Builds a frame with `draw`, then submits it. Returns true if the frame should be
    /// redrawn (the glyph atlas was reset mid-frame).
    pub fn frame(&mut self, clear: Color, draw: impl FnOnce(&mut Canvas)) -> bool {
        self.layers.clear();
        self.layers.push(Layer::default());
        let (w, h) = (self.config.width as f32, self.config.height as f32);
        {
            let mut canvas = Canvas {
                layers: &mut self.layers,
                text: &mut self.text,
                scale: self.scale,
                clip_stack: vec![[0.0, 0.0, w, h]],
                size: (w / self.scale, h / self.scale),
            };
            draw(&mut canvas);
        }
        self.text.end_frame();
        let overflowed = self.text.mask_atlas.overflowed || self.text.color_atlas.overflowed;
        self.text.mask_atlas.overflowed = false;
        self.text.color_atlas.overflowed = false;
        self.submit(clear);
        overflowed
    }

    fn submit(&mut self, clear: Color) {
        let (frame, offscreen) = match &self.surface {
            Some(surface) => match surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => (Some(t), None),
                wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                    surface.configure(&self.device, &self.config);
                    return;
                }
                _ => return,
            },
            None => {
                let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("offscreen frame"),
                    size: wgpu::Extent3d { width: self.config.width, height: self.config.height, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: self.config.format,
                    usage: self.config.usage,
                    view_formats: &[],
                });
                (None, Some(texture))
            }
        };
        let view = match (&frame, &offscreen) {
            (Some(f), _) => f.texture.create_view(&Default::default()),
            (_, Some(t)) => t.create_view(&Default::default()),
            _ => return,
        };

        let globals = [self.config.width as f32, self.config.height as f32, 0.0, 0.0];
        self.queue.write_buffer(&self.globals_buffer, 0, bytemuck::cast_slice(&globals));

        let quads: Vec<QuadInstance> = self.layers.iter().flat_map(|l| l.quads.iter().copied()).collect();
        let glyphs: Vec<GlyphInstance> = self.layers.iter().flat_map(|l| l.glyphs.iter().copied()).collect();
        // Images: upload new ones, drop textures not drawn for a while.
        self.frame_count += 1;
        let images: Vec<GlyphInstance> = self.layers.iter().flat_map(|l| l.images.iter().map(|(_, g)| *g)).collect();
        for layer in &self.layers {
            for (img, _) in &layer.images {
                let frame = self.frame_count;
                if let Some(entry) = self.textures.get_mut(&img.id) {
                    entry.1 = frame;
                    continue;
                }
                let group = self.upload_image(img);
                self.textures.insert(img.id, (group, frame));
            }
        }
        let now = self.frame_count;
        self.textures.retain(|_, (_, last)| now - *last < 600);
        ensure_capacity(&self.device, &mut self.image_buffer, "images", bytemuck::cast_slice::<_, u8>(&images).len());
        self.queue.write_buffer(&self.image_buffer, 0, bytemuck::cast_slice(&images));
        ensure_capacity(&self.device, &mut self.quad_buffer, "quads", bytemuck::cast_slice::<_, u8>(&quads).len());
        ensure_capacity(&self.device, &mut self.glyph_buffer, "glyphs", bytemuck::cast_slice::<_, u8>(&glyphs).len());
        self.queue.write_buffer(&self.quad_buffer, 0, bytemuck::cast_slice(&quads));
        self.queue.write_buffer(&self.glyph_buffer, 0, bytemuck::cast_slice(&glyphs));

        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("workbench"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: clear.r as f64,
                            g: clear.g as f64,
                            b: clear.b as f64,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_bind_group(0, &self.globals_group, &[]);
            pass.set_bind_group(1, &self.atlas_group, &[]);
            pass.set_vertex_buffer(0, self.quad_buffer.slice(..));
            let (mut q, mut g, mut im) = (0u32, 0u32, 0u32);
            for layer in &self.layers {
                let (nq, ng) = (layer.quads.len() as u32, layer.glyphs.len() as u32);
                if nq > 0 {
                    pass.set_pipeline(&self.quad_pipeline);
                    pass.set_vertex_buffer(0, self.quad_buffer.slice(..));
                    pass.draw(0..4, q..q + nq);
                }
                for (img, _) in &layer.images {
                    if let Some((group, _)) = self.textures.get(&img.id) {
                        pass.set_pipeline(&self.image_pipeline);
                        pass.set_bind_group(2, group, &[]);
                        pass.set_vertex_buffer(0, self.image_buffer.slice(..));
                        pass.draw(0..4, im..im + 1);
                    }
                    im += 1;
                }
                if ng > 0 {
                    pass.set_pipeline(&self.glyph_pipeline);
                    pass.set_vertex_buffer(0, self.glyph_buffer.slice(..));
                    pass.draw(0..4, g..g + ng);
                }
                q += nq;
                g += ng;
            }
        }
        self.queue.submit([encoder.finish()]);
        if let Some(frame) = frame {
            self.queue.present(frame);
        }
        if let Some(texture) = offscreen {
            self.read_back(&texture);
        }
    }

    /// Copies an offscreen frame into `pixels` (as RGBA).
    fn read_back(&mut self, texture: &wgpu::Texture) {
        let (w, h) = (self.config.width, self.config.height);
        let row = (w * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("offscreen readback"),
            size: row as u64 * h as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self.device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo { buffer: &buffer, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(h) } },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.queue.submit([encoder.finish()]);
        buffer.map_async(wgpu::MapMode::Read, .., |_| {});
        let _ = self.device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None });
        let Ok(data) = buffer.get_mapped_range(..) else { return };
        self.pixels.clear();
        for y in 0..h as usize {
            let line = &data[y * row as usize..y * row as usize + w as usize * 4];
            // BGRA → RGBA.
            for px in line.chunks_exact(4) {
                self.pixels.extend_from_slice(&[px[2], px[1], px[0], 255]);
            }
        }
    }
}

impl Renderer {
    /// Makes a texture of `img` and the bind group that samples it.
    fn upload_image(&self, img: &Image) -> wgpu::BindGroup {
        let size = wgpu::Extent3d { width: img.width.max(1), height: img.height.max(1), depth_or_array_layers: 1 };
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("image"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        if img.rgba.len() as u64 >= size.width as u64 * size.height as u64 * 4 {
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo { texture: &texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                &img.rgba,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4 * size.width), rows_per_image: Some(size.height) },
                size,
            );
        }
        let view = texture.create_view(&Default::default());
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("image"),
            layout: &self.image_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.image_sampler) },
            ],
        })
    }
}

fn instance_buffer(device: &wgpu::Device, label: &str, size: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn ensure_capacity(device: &wgpu::Device, buffer: &mut wgpu::Buffer, label: &str, needed: usize) {
    if buffer.size() < needed as u64 {
        *buffer = instance_buffer(device, label, (needed as u64).next_power_of_two());
    }
}

/// Minimal executor for wgpu's setup futures, which resolve immediately on native backends.
fn pollster_block<F: std::future::Future>(fut: F) -> F::Output {
    use std::task::{Context, Poll, Wake, Waker};
    struct ThreadWaker(std::thread::Thread);
    impl Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut fut = std::pin::pin!(fut);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => std::thread::park(),
        }
    }
}

/// Writes RGBA pixels as a PNG (uncompressed deflate: big, but needs no encoder).
pub fn write_png(path: &std::path::Path, width: u32, height: u32, rgba: &[u8]) -> std::io::Result<()> {
    fn crc32(data: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in data {
            crc ^= b as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            }
        }
        !crc
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32(&body).to_be_bytes());
    }
    // Scanlines, each with filter byte 0.
    let mut raw = Vec::with_capacity((width as usize * 4 + 1) * height as usize);
    for y in 0..height as usize {
        raw.push(0);
        raw.extend_from_slice(&rgba[y * width as usize * 4..(y + 1) * width as usize * 4]);
    }
    // zlib: stored blocks of up to 65535 bytes, then the Adler-32 of the data.
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65535).collect();
    for (i, block) in blocks.iter().enumerate() {
        z.push(u8::from(i + 1 == blocks.len()));
        let len = block.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    std::fs::write(path, out)
}
