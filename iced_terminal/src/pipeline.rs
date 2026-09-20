//! The GPU side: one custom primitive per terminal surface.
//!
//! A terminal is not a pile of text widgets. Eighty columns of forty rows is
//! 3200 cells, every one of which may have its own colours, and all of them
//! may change in one frame. So the widget submits a single
//! [`iced_wgpu::primitive::Primitive`] and this module turns a frame
//! description into four instanced draws: backgrounds, the selection, glyphs
//! from a shared atlas, and decorations plus the cursor, in that order.
//!
//! The work that must not happen per frame is shaping. A row is shaped once
//! per distinct content (see [`crate::cache`]), its GPU instances are built
//! once per shaped row and palette, and a frame that changed one row uploads
//! only from that row onwards. A frame that changed only the cursor or the
//! selection touches neither: those live in their own small buffers.
//!
//! One [`TerminalPipeline`] is shared by every surface -- iced keys pipeline
//! storage by primitive type -- so the glyph atlas and both caches are shared
//! too, and the per-surface buffers hang off it under the widget's id.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};

use bytemuck::{Pod, Zeroable};
use iced::advanced::graphics::text::{cosmic_text, font_system};
use iced::{Color, Rectangle};
use iced_graphics::Viewport;
use iced_wgpu::primitive::{Pipeline, Primitive};
use zeughaus_mux::{CellSpan, CellStyle, CursorShape, Palette, StyleFlags, Underline, WireColor};

use crate::cache::{InstanceKey, Lru, RowKey, Shelf, char_columns};
use crate::geometry::CellMetrics;

/// Rows kept shaped. A deep scrollback scrolled quickly is the worst case;
/// beyond this the oldest go and are reshaped if they come back.
const SHAPE_CACHE_ROWS: usize = 4096;
/// Built instance rows kept. Smaller than the shape cache: instances are
/// invalidated by a palette or atlas change, shaping is not.
const INSTANCE_CACHE_ROWS: usize = 2048;
/// The glyph atlas is a fixed square. Full means rebuild, never grow.
const ATLAS_SIZE: u32 = 2048;

const KIND_SOLID: u32 = 0;
const KIND_CURLY: u32 = 1;
const KIND_DOTTED: u32 = 2;
const KIND_DASHED: u32 = 3;

const FLAG_COLOR_GLYPH: u32 = 1;

/// How strongly a selection tints the cells under it.
const SELECTION_ALPHA: f32 = 0.30;
/// A block cursor is drawn over the glyph rather than swapping its colours,
/// so it has to let the glyph through.
const CURSOR_ALPHA: f32 = 0.65;

// ---------------------------------------------------------------- frame ----

/// Everything the GPU needs about one frame of one terminal.
///
/// Built by the widget from the shared [`zeughaus_mux::view::TerminalView`]
/// while the lock is held, because a primitive outlives it. Copying the
/// visible text is the only per-frame allocation the renderer makes; shaping
/// and instance building are both behind content-addressed caches.
#[derive(Debug)]
pub(crate) struct Frame {
    pub cols: u16,
    pub rows: u16,
    pub metrics: CellMetrics,
    pub font_size: f32,
    pub palette: Palette,
    pub palette_generation: u64,
    pub reverse_video: bool,
    pub lines: Vec<FrameRow>,
    pub selection: Vec<Highlight>,
    pub cursor: Option<CursorSpec>,
}

#[derive(Debug)]
pub(crate) struct FrameRow {
    pub key: RowKey,
    pub spans: Vec<CellSpan>,
}

/// A run of cells on one screen row to tint.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Highlight {
    pub row: u16,
    pub from: u16,
    pub to: u16,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CursorSpec {
    pub col: u16,
    pub row: u16,
    pub shape: CursorShape,
    /// A focused pane draws a solid cursor, an unfocused one an outline.
    pub focused: bool,
}

/// One terminal surface, drawn from `frame`, with GPU state kept under `id`.
#[derive(Debug, Clone)]
pub(crate) struct TerminalPrimitive {
    pub id: u64,
    pub frame: Arc<Frame>,
}

impl Primitive for TerminalPrimitive {
    type Pipeline = TerminalPipeline;

    fn prepare(
        &self,
        pipeline: &mut Self::Pipeline,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bounds: &Rectangle,
        viewport: &Viewport,
    ) {
        pipeline.prepare(self.id, &self.frame, device, queue, bounds, viewport);
    }

    fn draw(&self, pipeline: &Self::Pipeline, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        let Some(view) = pipeline.views.get(&self.id) else {
            return true;
        };

        render_pass.set_pipeline(&pipeline.quad);
        render_pass.set_bind_group(0, &view.bind_group, &[]);
        view.base.draw(render_pass);
        view.backgrounds.draw(render_pass);
        view.selection.draw(render_pass);

        render_pass.set_pipeline(&pipeline.glyph);
        render_pass.set_bind_group(0, &view.bind_group, &[]);
        render_pass.set_bind_group(1, &pipeline.atlas.bind_group, &[]);
        view.glyphs.draw(render_pass);

        render_pass.set_pipeline(&pipeline.quad);
        render_pass.set_bind_group(0, &view.bind_group, &[]);
        view.decorations.draw(render_pass);
        view.cursor.draw(render_pass);

        true
    }
}

// ------------------------------------------------------------ instances ----

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct QuadInstance {
    rect: [f32; 4],
    color: [f32; 4],
    kind: u32,
    padding: [u32; 3],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct GlyphInstance {
    rect: [f32; 4],
    uv: [f32; 4],
    color: [f32; 4],
    flags: u32,
    padding: [u32; 3],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct Uniforms {
    size: [f32; 2],
    padding: [f32; 2],
}

/// A row's instances, positioned with the row's top edge at y = 0.
#[derive(Debug, Default)]
struct RowInstances {
    backgrounds: Vec<QuadInstance>,
    glyphs: Vec<GlyphInstance>,
    decorations: Vec<QuadInstance>,
}

#[derive(Debug, Clone, Copy)]
struct ShapedGlyph {
    /// Index into the row's spans: where the colour comes from.
    span: u32,
    col: u16,
    key: cosmic_text::CacheKey,
    x: i32,
    y: i32,
}

#[derive(Debug, Default)]
struct ShapedRow {
    glyphs: Vec<ShapedGlyph>,
}

// --------------------------------------------------------------- atlas -----

#[derive(Debug, Clone, Copy)]
struct AtlasGlyph {
    uv: [f32; 4],
    size: [f32; 2],
    left: f32,
    top: f32,
    color: bool,
}

struct Atlas {
    texture: wgpu::Texture,
    sampler: wgpu::Sampler,
    layout: wgpu::BindGroupLayout,
    bind_group: wgpu::BindGroup,
    shelf: Shelf,
    glyphs: HashMap<cosmic_text::CacheKey, Option<AtlasGlyph>>,
    generation: u64,
    staging: Vec<u8>,
}

/// sRGB to linear, once, for colour glyph pixels. Mask coverage is not a
/// colour and is uploaded untouched.
static SRGB_TO_LINEAR: LazyLock<[u8; 256]> = LazyLock::new(|| {
    let mut table = [0u8; 256];
    for (value, slot) in table.iter_mut().enumerate() {
        let srgb = value as f32 / 255.0;
        let linear = if srgb <= 0.04045 {
            srgb / 12.92
        } else {
            ((srgb + 0.055) / 1.055).powf(2.4)
        };
        *slot = (linear * 255.0).round().clamp(0.0, 255.0) as u8;
    }
    table
});

impl Atlas {
    fn new(device: &wgpu::Device) -> Self {
        let texture = Self::texture(device);
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("iced_terminal glyph sampler"),
            // Glyphs are rasterized at exactly the size they are drawn, so
            // any filtering would only blur them.
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..wgpu::SamplerDescriptor::default()
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("iced_terminal atlas layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                    count: None,
                },
            ],
        });
        let bind_group = Self::bind_group(device, &layout, &texture, &sampler);

        Atlas {
            texture,
            sampler,
            layout,
            bind_group,
            shelf: Shelf::new(ATLAS_SIZE),
            glyphs: HashMap::new(),
            generation: 1,
            staging: Vec::new(),
        }
    }

    fn texture(device: &wgpu::Device) -> wgpu::Texture {
        device.create_texture(&wgpu::TextureDescriptor {
            label: Some("iced_terminal glyph atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_SIZE,
                height: ATLAS_SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        })
    }

    fn bind_group(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        texture: &wgpu::Texture,
        sampler: &wgpu::Sampler,
    ) -> wgpu::BindGroup {
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("iced_terminal atlas"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
            ],
        })
    }

    /// Throws the atlas away. Every texture coordinate handed out before is
    /// now wrong, which is why the instance cache is keyed by `generation`.
    fn reset(&mut self, device: &wgpu::Device) {
        log::debug!("iced_terminal: glyph atlas full, rebuilding");
        self.texture = Self::texture(device);
        self.bind_group = Self::bind_group(device, &self.layout, &self.texture, &self.sampler);
        self.shelf.clear();
        self.glyphs.clear();
        self.generation += 1;
    }

    /// Rasterizes and uploads one glyph. `None` means the glyph draws
    /// nothing (a space) or could not be rendered; either way it is
    /// remembered so it is not attempted again.
    fn insert(
        &mut self,
        queue: &wgpu::Queue,
        image: &cosmic_text::SwashImage,
    ) -> Result<Option<AtlasGlyph>, Full> {
        let width = image.placement.width;
        let height = image.placement.height;
        if width == 0 || height == 0 {
            return Ok(None);
        }
        let color = match image.content {
            cosmic_text::SwashContent::Mask => false,
            cosmic_text::SwashContent::Color => true,
            cosmic_text::SwashContent::SubpixelMask => return Ok(None),
        };

        // One pixel of slack so neighbouring glyphs cannot bleed into each
        // other when the sampler lands exactly on an edge.
        let (x, y) = self.shelf.allocate(width + 1, height + 1).ok_or(Full)?;

        let pixels = (width as usize) * (height as usize) * 4;
        self.staging.clear();
        self.staging.reserve(pixels);
        if color {
            let table = &*SRGB_TO_LINEAR;
            for chunk in image.data.as_chunks::<4>().0 {
                self.staging.push(table[chunk[0] as usize]);
                self.staging.push(table[chunk[1] as usize]);
                self.staging.push(table[chunk[2] as usize]);
                self.staging.push(chunk[3]);
            }
        } else {
            for coverage in &image.data {
                self.staging
                    .extend_from_slice(&[0xff, 0xff, 0xff, *coverage]);
            }
        }
        if self.staging.len() < pixels {
            // A truncated image is a malformed font, not a reason to panic.
            self.staging.resize(pixels, 0);
        }

        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            &self.staging[..pixels],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        let size = self.shelf.size() as f32;
        Ok(Some(AtlasGlyph {
            uv: [
                x as f32 / size,
                y as f32 / size,
                width as f32 / size,
                height as f32 / size,
            ],
            size: [width as f32, height as f32],
            left: image.placement.left as f32,
            top: image.placement.top as f32,
            color,
        }))
    }
}

/// The atlas ran out of room.
struct Full;

// ------------------------------------------------------- instance buffer ---

/// A GPU buffer that grows to fit and is written from the first changed
/// instance onwards.
struct InstanceBuffer {
    buffer: Option<wgpu::Buffer>,
    capacity: usize,
    len: u32,
    stride: u64,
    label: &'static str,
}

impl InstanceBuffer {
    fn new(stride: usize, label: &'static str) -> Self {
        InstanceBuffer {
            buffer: None,
            capacity: 0,
            len: 0,
            stride: stride as u64,
            label,
        }
    }

    /// Uploads `data`, writing only from index `from`. Reallocating discards
    /// that optimisation and writes everything, which is correct either way.
    fn write<T: Pod>(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        data: &[T],
        from: usize,
    ) {
        self.len = data.len() as u32;
        if data.is_empty() {
            return;
        }

        let mut from = from.min(data.len());
        if data.len() > self.capacity {
            let capacity = data.len().next_power_of_two().max(64);
            self.buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(self.label),
                size: capacity as u64 * self.stride,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
            self.capacity = capacity;
            from = 0;
        }

        let Some(buffer) = &self.buffer else {
            return;
        };
        if from < data.len() {
            queue.write_buffer(
                buffer,
                from as u64 * self.stride,
                bytemuck::cast_slice(&data[from..]),
            );
        }
    }

    fn draw(&self, render_pass: &mut wgpu::RenderPass<'_>) {
        let Some(buffer) = &self.buffer else {
            return;
        };
        if self.len == 0 {
            return;
        }
        render_pass.set_vertex_buffer(0, buffer.slice(..));
        render_pass.draw(0..6, 0..self.len);
    }
}

// ----------------------------------------------------------- view state ----

/// What decides whether every row has to be rebuilt: the grid, the cell size
/// in device pixels, and where the baseline sits in a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct LayoutKey {
    cols: u16,
    rows: u16,
    cell_width: u32,
    cell_height: u32,
    baseline: u32,
}

struct CachedRow {
    key: InstanceKey,
    instances: Arc<RowInstances>,
    backgrounds: usize,
    glyphs: usize,
    decorations: usize,
}

struct ViewState {
    uniform: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    uniform_size: [f32; 2],
    base: InstanceBuffer,
    backgrounds: InstanceBuffer,
    glyphs: InstanceBuffer,
    decorations: InstanceBuffer,
    selection: InstanceBuffer,
    cursor: InstanceBuffer,
    rows: Vec<CachedRow>,
    cpu_backgrounds: Vec<QuadInstance>,
    cpu_glyphs: Vec<GlyphInstance>,
    cpu_decorations: Vec<QuadInstance>,
    layout: LayoutKey,
}

impl ViewState {
    fn new(device: &wgpu::Device, layout: &wgpu::BindGroupLayout) -> Self {
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("iced_terminal uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("iced_terminal uniforms"),
            layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform.as_entire_binding(),
            }],
        });

        ViewState {
            uniform,
            bind_group,
            // No real size, so the first frame always writes the uniform.
            uniform_size: [-1.0, -1.0],
            base: InstanceBuffer::new(std::mem::size_of::<QuadInstance>(), "iced_terminal base"),
            backgrounds: InstanceBuffer::new(
                std::mem::size_of::<QuadInstance>(),
                "iced_terminal backgrounds",
            ),
            glyphs: InstanceBuffer::new(
                std::mem::size_of::<GlyphInstance>(),
                "iced_terminal glyphs",
            ),
            decorations: InstanceBuffer::new(
                std::mem::size_of::<QuadInstance>(),
                "iced_terminal decorations",
            ),
            selection: InstanceBuffer::new(
                std::mem::size_of::<QuadInstance>(),
                "iced_terminal selection",
            ),
            cursor: InstanceBuffer::new(
                std::mem::size_of::<QuadInstance>(),
                "iced_terminal cursor",
            ),
            rows: Vec::new(),
            cpu_backgrounds: Vec::new(),
            cpu_glyphs: Vec::new(),
            cpu_decorations: Vec::new(),
            layout: LayoutKey::default(),
        }
    }
}

// -------------------------------------------------------------- pipeline ---

/// The wgpu state every terminal surface shares.
pub(crate) struct TerminalPipeline {
    quad: wgpu::RenderPipeline,
    glyph: wgpu::RenderPipeline,
    uniform_layout: wgpu::BindGroupLayout,
    atlas: Atlas,
    shapes: Lru<RowKey, Arc<ShapedRow>>,
    instances: Lru<InstanceKey, Arc<RowInstances>>,
    views: HashMap<u64, ViewState>,
    swash: Mutex<cosmic_text::SwashCache>,
}

impl Pipeline for TerminalPipeline {
    fn new(device: &wgpu::Device, _queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        crate::font::ensure_registered();

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("iced_terminal shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });

        let uniform_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("iced_terminal uniform layout"),
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

        let atlas = Atlas::new(device);

        let quad_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("iced_terminal quad layout"),
            bind_group_layouts: &[&uniform_layout],
            push_constant_ranges: &[],
        });
        let glyph_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("iced_terminal glyph layout"),
            bind_group_layouts: &[&uniform_layout, &atlas.layout],
            push_constant_ranges: &[],
        });

        let targets = [Some(wgpu::ColorTargetState {
            format,
            blend: Some(wgpu::BlendState::ALPHA_BLENDING),
            write_mask: wgpu::ColorWrites::ALL,
        })];

        let quad_attributes = wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Uint32];
        let quad_buffers = [wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<QuadInstance>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &quad_attributes,
        }];

        let glyph_attributes = wgpu::vertex_attr_array![
            0 => Float32x4,
            1 => Float32x4,
            2 => Float32x4,
            3 => Uint32
        ];
        let glyph_buffers = [wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<GlyphInstance>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &glyph_attributes,
        }];

        let quad = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("iced_terminal quads"),
            layout: Some(&quad_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("quad_vertex"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &quad_buffers,
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("quad_fragment"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &targets,
            }),
            multiview: None,
            cache: None,
        });

        let glyph = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("iced_terminal glyphs"),
            layout: Some(&glyph_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("glyph_vertex"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &glyph_buffers,
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("glyph_fragment"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &targets,
            }),
            multiview: None,
            cache: None,
        });

        TerminalPipeline {
            quad,
            glyph,
            uniform_layout,
            atlas,
            shapes: Lru::new(SHAPE_CACHE_ROWS),
            instances: Lru::new(INSTANCE_CACHE_ROWS),
            views: HashMap::new(),
            swash: Mutex::new(cosmic_text::SwashCache::new()),
        }
    }
}

impl TerminalPipeline {
    fn prepare(
        &mut self,
        id: u64,
        frame: &Frame,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bounds: &Rectangle,
        viewport: &Viewport,
    ) {
        let scale = viewport.scale_factor().max(0.01);
        let cell_width = (frame.metrics.width * scale).round().max(1.0);
        let cell_height = (frame.metrics.height * scale).round().max(1.0);
        let baseline = (frame.metrics.ascent * scale).round();

        let layout = LayoutKey {
            cols: frame.cols,
            rows: frame.rows,
            cell_width: cell_width as u32,
            cell_height: cell_height as u32,
            baseline: baseline as u32,
        };

        let shaped = self.shape(frame, scale);
        let atlas_before = self.atlas.generation;
        self.stage_glyphs(&shaped, device, queue);
        if self.atlas.generation != atlas_before {
            // Every cached texture coordinate belongs to the old atlas.
            self.instances.clear();
        }

        if !self.views.contains_key(&id) {
            let state = ViewState::new(device, &self.uniform_layout);
            let _ = self.views.insert(id, state);
        }

        let mut built: Vec<(InstanceKey, Arc<RowInstances>)> = Vec::with_capacity(shaped.len());
        for (index, row) in frame.lines.iter().enumerate() {
            let key = InstanceKey {
                row: row.key.with_scale(scale),
                palette: frame.palette_generation,
                atlas: self.atlas.generation,
                reverse_video: frame.reverse_video,
            };
            let instances = match self.instances.get(&key).cloned() {
                Some(hit) => hit,
                None => {
                    let empty = ShapedRow::default();
                    let shaped_row = shaped.get(index).map_or(&empty, |row| row.as_ref());
                    let built = Arc::new(self.build_row(frame, row, shaped_row, &layout, scale));
                    self.instances.insert(key, Arc::clone(&built));
                    built
                }
            };
            built.push((key, instances));
        }

        let Some(view) = self.views.get_mut(&id) else {
            return;
        };

        let relayout = view.layout != layout;
        view.layout = layout;

        let first_dirty = if relayout {
            0
        } else {
            let mut dirty = built.len().min(view.rows.len());
            for (index, (key, _)) in built.iter().enumerate() {
                if view.rows.get(index).is_none_or(|row| row.key != *key) {
                    dirty = index;
                    break;
                }
            }
            if view.rows.len() != built.len() {
                dirty = dirty.min(built.len());
            }
            dirty
        };

        if first_dirty < built.len() || view.rows.len() != built.len() {
            let (mut bg, mut gl, mut de) = match view.rows.get(first_dirty) {
                Some(row) => (row.backgrounds, row.glyphs, row.decorations),
                None => view
                    .rows
                    .last()
                    .map(|row| {
                        (
                            row.backgrounds + row.instances.backgrounds.len(),
                            row.glyphs + row.instances.glyphs.len(),
                            row.decorations + row.instances.decorations.len(),
                        )
                    })
                    .unwrap_or((0, 0, 0)),
            };

            view.rows.truncate(first_dirty);
            view.cpu_backgrounds.truncate(bg);
            view.cpu_glyphs.truncate(gl);
            view.cpu_decorations.truncate(de);

            for (index, (key, instances)) in built.iter().enumerate().skip(first_dirty) {
                let top = index as f32 * cell_height;
                view.cpu_backgrounds
                    .extend(instances.backgrounds.iter().map(|quad| quad.offset_y(top)));
                view.cpu_glyphs
                    .extend(instances.glyphs.iter().map(|glyph| glyph.offset_y(top)));
                view.cpu_decorations
                    .extend(instances.decorations.iter().map(|quad| quad.offset_y(top)));
                view.rows.push(CachedRow {
                    key: *key,
                    instances: Arc::clone(instances),
                    backgrounds: bg,
                    glyphs: gl,
                    decorations: de,
                });
                bg += instances.backgrounds.len();
                gl += instances.glyphs.len();
                de += instances.decorations.len();
            }

            let (from_bg, from_gl, from_de) = view.rows.get(first_dirty).map_or((0, 0, 0), |row| {
                (row.backgrounds, row.glyphs, row.decorations)
            });
            let backgrounds = std::mem::take(&mut view.cpu_backgrounds);
            view.backgrounds.write(device, queue, &backgrounds, from_bg);
            view.cpu_backgrounds = backgrounds;
            let glyphs = std::mem::take(&mut view.cpu_glyphs);
            view.glyphs.write(device, queue, &glyphs, from_gl);
            view.cpu_glyphs = glyphs;
            let decorations = std::mem::take(&mut view.cpu_decorations);
            view.decorations.write(device, queue, &decorations, from_de);
            view.cpu_decorations = decorations;
        }

        // Base, selection and cursor never reshape a row: they are their own
        // small buffers so a blink or a drag uploads a handful of quads.
        let size = [bounds.width * scale, bounds.height * scale];
        let (_, default_bg) = default_colors(frame);
        let base = [QuadInstance {
            rect: [0.0, 0.0, size[0].max(1.0), size[1].max(1.0)],
            color: shader_color(default_bg, 1.0),
            kind: KIND_SOLID,
            padding: [0; 3],
        }];
        view.base.write(device, queue, &base, 0);

        let selection: Vec<QuadInstance> = frame
            .selection
            .iter()
            .filter(|highlight| highlight.to > highlight.from)
            .map(|highlight| QuadInstance {
                rect: [
                    f32::from(highlight.from) * cell_width,
                    f32::from(highlight.row) * cell_height,
                    f32::from(highlight.to - highlight.from) * cell_width,
                    cell_height,
                ],
                color: shader_color(frame.palette.foreground, SELECTION_ALPHA),
                kind: KIND_SOLID,
                padding: [0; 3],
            })
            .collect();
        view.selection.write(device, queue, &selection, 0);

        let cursor = cursor_quads(frame, cell_width, cell_height);
        view.cursor.write(device, queue, &cursor, 0);

        if view.uniform_size != size {
            view.uniform_size = size;
            queue.write_buffer(
                &view.uniform,
                0,
                bytemuck::bytes_of(&Uniforms {
                    size,
                    padding: [0.0; 2],
                }),
            );
        }
    }

    /// Shapes every visible row that is not already shaped.
    fn shape(&mut self, frame: &Frame, scale: f32) -> Vec<Arc<ShapedRow>> {
        let mut out: Vec<Option<Arc<ShapedRow>>> = Vec::with_capacity(frame.lines.len());
        let mut missing = false;
        for row in &frame.lines {
            let hit = self.shapes.get(&row.key.with_scale(scale)).cloned();
            missing |= hit.is_none();
            out.push(hit);
        }

        if missing && let Ok(mut fonts) = font_system().write() {
            let fonts = fonts.raw();
            let mut buffer = cosmic_text::Buffer::new(
                fonts,
                cosmic_text::Metrics::new(frame.font_size, frame.metrics.height.max(1.0)),
            );
            buffer.set_wrap(fonts, cosmic_text::Wrap::None);
            buffer.set_size(fonts, None, None);

            for (slot, row) in out.iter_mut().zip(&frame.lines) {
                if slot.is_some() {
                    continue;
                }
                let shaped = Arc::new(shape_row(&mut buffer, fonts, &row.spans, scale));
                self.shapes
                    .insert(row.key.with_scale(scale), Arc::clone(&shaped));
                *slot = Some(shaped);
            }
        }

        out.into_iter()
            .map(|shaped| shaped.unwrap_or_else(|| Arc::new(ShapedRow::default())))
            .collect()
    }

    /// Makes sure every glyph of the frame has an atlas entry, rebuilding the
    /// atlas at most once if it filled up.
    fn stage_glyphs(
        &mut self,
        shaped: &[Arc<ShapedRow>],
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) {
        let mut wanted: HashSet<cosmic_text::CacheKey> = HashSet::new();
        for row in shaped {
            for glyph in &row.glyphs {
                if !self.atlas.glyphs.contains_key(&glyph.key) {
                    let _ = wanted.insert(glyph.key);
                }
            }
        }
        if wanted.is_empty() {
            return;
        }

        let Ok(mut fonts) = font_system().write() else {
            return;
        };
        let Ok(mut swash) = self.swash.lock() else {
            return;
        };
        let fonts = fonts.raw();

        let mut retried = false;
        let mut pending: Vec<cosmic_text::CacheKey> = wanted.into_iter().collect();
        while let Some(key) = pending.pop() {
            let image = swash.get_image_uncached(fonts, key);
            let entry = match image {
                Some(image) => match self.atlas.insert(queue, &image) {
                    Ok(entry) => entry,
                    Err(Full) if !retried => {
                        retried = true;
                        self.atlas.reset(device);
                        // Everything placed so far belonged to the old atlas.
                        pending.clear();
                        for row in shaped {
                            for glyph in &row.glyphs {
                                pending.push(glyph.key);
                            }
                        }
                        continue;
                    }
                    Err(Full) => {
                        log::warn!("iced_terminal: glyph atlas cannot hold one screen of text");
                        None
                    }
                },
                None => None,
            };
            let _ = self.atlas.glyphs.insert(key, entry);
        }
    }

    fn build_row(
        &self,
        frame: &Frame,
        row: &FrameRow,
        shaped: &ShapedRow,
        layout: &LayoutKey,
        scale: f32,
    ) -> RowInstances {
        let cell_width = layout.cell_width as f32;
        let cell_height = layout.cell_height as f32;
        let baseline = layout.baseline as f32;

        let (default_fg, default_bg) = default_colors(frame);
        let mut instances = RowInstances::default();

        for span in &row.spans {
            let (fg, bg) = span_colors(&span.style, &frame.palette, default_fg, default_bg);
            let x = f32::from(span.start_col) * cell_width;
            let width = f32::from(span.cell_count) * cell_width;

            if bg != default_bg {
                instances.backgrounds.push(QuadInstance {
                    rect: [x, 0.0, width, cell_height],
                    color: shader_color(bg, 1.0),
                    kind: KIND_SOLID,
                    padding: [0; 3],
                });
            }

            if span.style.flags.has(StyleFlags::INVISIBLE) {
                continue;
            }

            let line_color = match span.style.underline_color {
                WireColor::Default => fg,
                other => resolve(other, &frame.palette, fg),
            };
            let thickness = (frame.metrics.underline_thickness * scale).round().max(1.0);

            let underline_top = (frame.metrics.underline_offset * scale).round();
            match span.style.flags.underline() {
                Underline::None => {}
                Underline::Single => instances.decorations.push(QuadInstance {
                    rect: [x, underline_top, width, thickness],
                    color: shader_color(line_color, 1.0),
                    kind: KIND_SOLID,
                    padding: [0; 3],
                }),
                Underline::Double => {
                    for offset in [0.0, thickness * 2.0] {
                        instances.decorations.push(QuadInstance {
                            rect: [
                                x,
                                (underline_top + offset).min(cell_height - thickness),
                                width,
                                thickness,
                            ],
                            color: shader_color(line_color, 1.0),
                            kind: KIND_SOLID,
                            padding: [0; 3],
                        });
                    }
                }
                Underline::Curly => instances.decorations.push(QuadInstance {
                    rect: [
                        x,
                        (underline_top - thickness).max(0.0),
                        width,
                        (thickness * 3.0).min(cell_height),
                    ],
                    color: shader_color(line_color, 1.0),
                    kind: KIND_CURLY,
                    padding: [0; 3],
                }),
                Underline::Dotted => instances.decorations.push(QuadInstance {
                    rect: [x, underline_top, width, thickness],
                    color: shader_color(line_color, 1.0),
                    kind: KIND_DOTTED,
                    padding: [0; 3],
                }),
                Underline::Dashed => instances.decorations.push(QuadInstance {
                    rect: [x, underline_top, width, thickness],
                    color: shader_color(line_color, 1.0),
                    kind: KIND_DASHED,
                    padding: [0; 3],
                }),
            }

            if span.style.flags.has(StyleFlags::STRIKETHROUGH) {
                instances.decorations.push(QuadInstance {
                    rect: [
                        x,
                        (frame.metrics.strikethrough_offset * scale).round(),
                        width,
                        thickness,
                    ],
                    color: shader_color(fg, 1.0),
                    kind: KIND_SOLID,
                    padding: [0; 3],
                });
            }
            if span.style.flags.has(StyleFlags::OVERLINE) {
                instances.decorations.push(QuadInstance {
                    rect: [x, 0.0, width, thickness],
                    color: shader_color(fg, 1.0),
                    kind: KIND_SOLID,
                    padding: [0; 3],
                });
            }
        }

        for glyph in &shaped.glyphs {
            let Some(span) = row.spans.get(glyph.span as usize) else {
                continue;
            };
            if span.style.flags.has(StyleFlags::INVISIBLE) {
                continue;
            }
            let Some(Some(entry)) = self.atlas.glyphs.get(&glyph.key) else {
                continue;
            };
            let (fg, _) = span_colors(&span.style, &frame.palette, default_fg, default_bg);

            let left = f32::from(glyph.col) * cell_width + glyph.x as f32 + entry.left;
            let top = baseline + glyph.y as f32 - entry.top;
            instances.glyphs.push(GlyphInstance {
                rect: [left, top, entry.size[0], entry.size[1]],
                uv: entry.uv,
                color: shader_color(fg, 1.0),
                flags: if entry.color { FLAG_COLOR_GLYPH } else { 0 },
                padding: [0; 3],
            });
        }

        instances
    }
}

impl QuadInstance {
    fn offset_y(&self, top: f32) -> QuadInstance {
        QuadInstance {
            rect: [self.rect[0], self.rect[1] + top, self.rect[2], self.rect[3]],
            ..*self
        }
    }
}

impl GlyphInstance {
    fn offset_y(&self, top: f32) -> GlyphInstance {
        GlyphInstance {
            rect: [self.rect[0], self.rect[1] + top, self.rect[2], self.rect[3]],
            ..*self
        }
    }
}

// --------------------------------------------------------------- shaping ---

fn shape_row(
    buffer: &mut cosmic_text::Buffer,
    fonts: &mut cosmic_text::FontSystem,
    spans: &[CellSpan],
    scale: f32,
) -> ShapedRow {
    if spans.is_empty() {
        return ShapedRow::default();
    }

    // Byte offset in the concatenated line -> grid column, so a glyph lands
    // in the cell the runner counted for it rather than wherever the shaper's
    // advances happened to put it.
    let mut columns: Vec<u16> = Vec::new();
    let mut pieces: Vec<(&str, cosmic_text::Attrs<'_>)> = Vec::with_capacity(spans.len());
    for (index, span) in spans.iter().enumerate() {
        let base = columns.len();
        columns.resize(base + span.text.len(), 0);
        for (offset, ch, col) in char_columns(span) {
            for slot in columns
                .iter_mut()
                .skip(base + offset)
                .take(ch.len_utf8().max(1))
            {
                *slot = col;
            }
        }
        pieces.push((span.text.as_str(), attrs_for(&span.style).metadata(index)));
    }

    buffer.set_rich_text(
        fonts,
        pieces,
        &attrs_for(&CellStyle::default()),
        cosmic_text::Shaping::Advanced,
        None,
    );

    let mut shaped = ShapedRow::default();
    for run in buffer.layout_runs() {
        for glyph in run.glyphs {
            let Some(col) = columns.get(glyph.start).copied() else {
                continue;
            };
            // Placement is ours: only the glyph's own offsets within its
            // cluster survive, never the shaper's running advance.
            let physical = glyph.physical((-glyph.x * scale, 0.0), scale);
            shaped.glyphs.push(ShapedGlyph {
                span: glyph.metadata as u32,
                col,
                key: physical.cache_key,
                x: physical.x,
                y: physical.y,
            });
        }
    }
    shaped
}

fn attrs_for(style: &CellStyle) -> cosmic_text::Attrs<'static> {
    let mut attrs =
        cosmic_text::Attrs::new().family(cosmic_text::Family::Name(crate::font::FAMILY));
    if style.flags.has(StyleFlags::BOLD) {
        attrs = attrs.weight(cosmic_text::Weight::BOLD);
    }
    if style.flags.has(StyleFlags::ITALIC) {
        attrs = attrs.style(cosmic_text::Style::Italic);
    }
    attrs
}

// ---------------------------------------------------------------- colour ---

fn default_colors(frame: &Frame) -> ([u8; 3], [u8; 3]) {
    if frame.reverse_video {
        (frame.palette.background, frame.palette.foreground)
    } else {
        (frame.palette.foreground, frame.palette.background)
    }
}

fn span_colors(
    style: &CellStyle,
    palette: &Palette,
    default_fg: [u8; 3],
    default_bg: [u8; 3],
) -> ([u8; 3], [u8; 3]) {
    let mut fg = resolve(style.fg, palette, default_fg);
    let mut bg = resolve(style.bg, palette, default_bg);
    if style.flags.has(StyleFlags::REVERSE) {
        std::mem::swap(&mut fg, &mut bg);
    }
    if style.flags.has(StyleFlags::DIM) {
        fg = [fg[0] / 2, fg[1] / 2, fg[2] / 2];
    }
    (fg, bg)
}

/// Indexed colours below 16 come from the terminal's live palette; the cube
/// and the greys are fixed by convention and never travel.
fn resolve(color: WireColor, palette: &Palette, default: [u8; 3]) -> [u8; 3] {
    match color {
        WireColor::Default => default,
        WireColor::Rgb(rgb) => rgb,
        WireColor::Indexed(index) => match index {
            0..=15 => palette.ansi[index as usize],
            16..=231 => {
                let index = index - 16;
                [
                    cube_level(index / 36),
                    cube_level((index % 36) / 6),
                    cube_level(index % 6),
                ]
            }
            _ => {
                let level = 8 + u16::from(index - 232) * 10;
                let level = level.min(255) as u8;
                [level, level, level]
            }
        },
    }
}

fn cube_level(step: u8) -> u8 {
    if step == 0 { 0 } else { 55 + step * 40 }
}

/// iced renders in linear space by default, so a terminal's sRGB colours have
/// to be converted the same way iced converts its own.
fn shader_color(rgb: [u8; 3], alpha: f32) -> [f32; 4] {
    Color::from_rgba8(rgb[0], rgb[1], rgb[2], alpha).into_linear()
}

fn cursor_quads(frame: &Frame, cell_width: f32, cell_height: f32) -> Vec<QuadInstance> {
    let Some(cursor) = frame.cursor else {
        return Vec::new();
    };
    let color = shader_color(
        frame.palette.cursor,
        if cursor.focused { CURSOR_ALPHA } else { 1.0 },
    );
    let x = f32::from(cursor.col) * cell_width;
    let y = f32::from(cursor.row) * cell_height;
    let thickness = (cell_height / 10.0).round().max(1.0);

    let solid = |rect: [f32; 4]| QuadInstance {
        rect,
        color,
        kind: KIND_SOLID,
        padding: [0; 3],
    };

    if !cursor.focused {
        // An unfocused pane shows where the cursor is without claiming it.
        return vec![
            solid([x, y, cell_width, thickness]),
            solid([x, y + cell_height - thickness, cell_width, thickness]),
            solid([x, y, thickness, cell_height]),
            solid([x + cell_width - thickness, y, thickness, cell_height]),
        ];
    }

    match cursor.shape {
        CursorShape::Block => vec![solid([x, y, cell_width, cell_height])],
        CursorShape::Underline => vec![solid([
            x,
            y + cell_height - thickness,
            cell_width,
            thickness,
        ])],
        CursorShape::Bar => vec![solid([x, y, thickness, cell_height])],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexed_colours_resolve_to_the_conventional_cube_and_greys() {
        let palette = Palette::default();
        let fallback = [1, 2, 3];
        assert_eq!(
            resolve(WireColor::Indexed(1), &palette, fallback),
            palette.ansi[1]
        );
        assert_eq!(
            resolve(WireColor::Indexed(16), &palette, fallback),
            [0, 0, 0]
        );
        assert_eq!(
            resolve(WireColor::Indexed(231), &palette, fallback),
            [255, 255, 255]
        );
        assert_eq!(
            resolve(WireColor::Indexed(232), &palette, fallback),
            [8, 8, 8]
        );
        assert_eq!(
            resolve(WireColor::Indexed(255), &palette, fallback),
            [238, 238, 238]
        );
        assert_eq!(resolve(WireColor::Default, &palette, fallback), fallback);
        assert_eq!(
            resolve(WireColor::Rgb([9, 9, 9]), &palette, fallback),
            [9, 9, 9]
        );
    }

    #[test]
    fn reverse_swaps_and_dim_halves() {
        let palette = Palette::default();
        let style = CellStyle {
            fg: WireColor::Rgb([200, 100, 50]),
            bg: WireColor::Rgb([10, 20, 30]),
            underline_color: WireColor::Default,
            flags: StyleFlags::default().with(StyleFlags::REVERSE),
        };
        let (fg, bg) = span_colors(&style, &palette, [0, 0, 0], [0, 0, 0]);
        assert_eq!(fg, [10, 20, 30]);
        assert_eq!(bg, [200, 100, 50]);

        let dim = CellStyle {
            flags: StyleFlags::default().with(StyleFlags::DIM),
            ..style
        };
        let (fg, _) = span_colors(&dim, &palette, [0, 0, 0], [0, 0, 0]);
        assert_eq!(fg, [100, 50, 25]);
    }

    #[test]
    fn global_reverse_video_swaps_the_defaults() {
        let mut frame = test_frame();
        let (fg, bg) = default_colors(&frame);
        assert_eq!(fg, frame.palette.foreground);
        assert_eq!(bg, frame.palette.background);

        frame.reverse_video = true;
        let (fg, bg) = default_colors(&frame);
        assert_eq!(fg, frame.palette.background);
        assert_eq!(bg, frame.palette.foreground);
    }

    #[test]
    fn an_unfocused_cursor_is_an_outline_and_a_focused_block_is_one_quad() {
        let mut frame = test_frame();
        frame.cursor = Some(CursorSpec {
            col: 2,
            row: 1,
            shape: CursorShape::Block,
            focused: true,
        });
        assert_eq!(cursor_quads(&frame, 8.0, 16.0).len(), 1);

        frame.cursor = Some(CursorSpec {
            col: 2,
            row: 1,
            shape: CursorShape::Block,
            focused: false,
        });
        assert_eq!(cursor_quads(&frame, 8.0, 16.0).len(), 4);

        frame.cursor = None;
        assert!(cursor_quads(&frame, 8.0, 16.0).is_empty());
    }

    fn test_frame() -> Frame {
        Frame {
            cols: 8,
            rows: 2,
            metrics: CellMetrics {
                width: 8.0,
                height: 16.0,
                ascent: 12.0,
                descent: 4.0,
                underline_offset: 14.0,
                underline_thickness: 1.0,
                strikethrough_offset: 8.0,
            },
            font_size: 14.0,
            palette: Palette::default(),
            palette_generation: 0,
            reverse_video: false,
            lines: Vec::new(),
            selection: Vec::new(),
            cursor: None,
        }
    }

    /// The one thing CPU tests cannot answer: does the WGSL compile, do the
    /// two vertex layouts match their entry points, and does a frame of real
    /// text end up as pixels?
    ///
    /// Runs against a headless adapter -- no window, no surface. A machine
    /// without one skips it rather than failing.
    #[test]
    fn a_frame_of_text_reaches_the_target_texture() {
        let Some((device, queue)) = headless_device() else {
            eprintln!("iced_terminal: no wgpu adapter, skipping the GPU smoke test");
            return;
        };

        let format = wgpu::TextureFormat::Rgba8Unorm;
        let mut pipeline = TerminalPipeline::new(&device, &queue, format);

        let metrics = crate::font::cell_metrics(16.0);
        let spans = vec![CellSpan {
            start_col: 0,
            cell_count: 5,
            text: "Hello".to_string(),
            style: CellStyle {
                fg: WireColor::Rgb([255, 0, 0]),
                ..CellStyle::default()
            },
            link: None,
        }];
        let build = |cursor_col: u16| Frame {
            cols: 8,
            rows: 2,
            metrics,
            font_size: 16.0,
            palette: Palette::default(),
            palette_generation: 1,
            reverse_video: false,
            lines: vec![
                FrameRow {
                    key: crate::cache::row_key(&spans, 16.0),
                    spans: spans.clone(),
                },
                FrameRow {
                    key: crate::cache::row_key(&[], 16.0),
                    spans: Vec::new(),
                },
            ],
            selection: vec![Highlight {
                row: 1,
                from: 0,
                to: 4,
            }],
            cursor: Some(CursorSpec {
                col: cursor_col,
                row: 0,
                shape: CursorShape::Block,
                focused: true,
            }),
        };
        let frame = build(5);

        let width = (metrics.width * 8.0).ceil() as u32;
        let height = (metrics.height * 2.0).ceil() as u32;
        let bounds = Rectangle {
            x: 0.0,
            y: 0.0,
            width: width as f32,
            height: height as f32,
        };
        let viewport = Viewport::with_physical_size(iced::Size::new(width, height), 1.0);

        let primitive = TerminalPrimitive {
            id: 1,
            frame: Arc::new(frame),
        };
        primitive.prepare(&mut pipeline, &device, &queue, &bounds, &viewport);

        assert!(
            pipeline.atlas.glyphs.values().any(Option::is_some),
            "shaping and rasterizing 'Hello' must have filled the atlas"
        );

        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("smoke target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());

        // 256-byte row alignment, as `copy_texture_to_buffer` requires.
        let bytes_per_row = (width * 4).div_ceil(256) * 256;
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("smoke readback"),
            size: u64::from(bytes_per_row) * u64::from(height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("smoke pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            // iced sets the viewport to the widget's bounds before `draw`.
            pass.set_viewport(0.0, 0.0, width as f32, height as f32, 0.0, 1.0);
            assert!(
                primitive.draw(&pipeline, &mut pass),
                "the primitive draws in the pass it is given"
            );
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        let _ = queue.submit([encoder.finish()]);

        readback.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        let _ = device.poll(wgpu::PollType::wait_indefinitely());
        let pixels = readback.slice(..).get_mapped_range().to_vec();

        let background = shader_color(Palette::default().background, 1.0);
        let expected = [
            (background[0] * 255.0).round() as u8,
            (background[1] * 255.0).round() as u8,
            (background[2] * 255.0).round() as u8,
        ];
        let mut painted = 0usize;
        let mut foreign = 0usize;
        let mut red = 0usize;
        for row in 0..height as usize {
            let start = row * bytes_per_row as usize;
            for pixel in pixels[start..start + width as usize * 4].as_chunks::<4>().0 {
                if pixel[3] == 0 {
                    continue;
                }
                painted += 1;
                if pixel[..3] != expected {
                    foreign += 1;
                }
                if pixel[0] > 128 && pixel[1] < 64 && pixel[2] < 64 {
                    red += 1;
                }
            }
        }
        assert_eq!(
            painted,
            (width * height) as usize,
            "the pane background covers every pixel of the bounds"
        );
        assert!(
            foreign > 0,
            "glyphs, the selection and the cursor must differ from the background"
        );
        assert!(
            red > 0,
            "the red foreground of 'Hello' must reach the target texture"
        );

        // A cursor-only change must not reshape a row: that is the whole
        // point of keeping the cursor in its own instance buffer.
        let shaped_rows = pipeline.shapes.len();
        let moved = TerminalPrimitive {
            id: 1,
            frame: Arc::new(build(6)),
        };
        moved.prepare(&mut pipeline, &device, &queue, &bounds, &viewport);
        assert_eq!(
            pipeline.shapes.len(),
            shaped_rows,
            "moving the cursor shaped a row again"
        );
    }

    /// A pane whose view has not arrived yet still has to be a pane: its
    /// frame carries no rows at all, and the pipeline has to paint the
    /// background across the whole surface instead of walking them.
    #[test]
    fn a_pane_without_a_view_paints_only_its_background() {
        let Some((device, queue)) = headless_device() else {
            eprintln!("iced_terminal: no wgpu adapter, skipping the GPU smoke test");
            return;
        };

        let format = wgpu::TextureFormat::Rgba8Unorm;
        let mut pipeline = TerminalPipeline::new(&device, &queue, format);

        let terminal: crate::widget::Terminal<'_, ()> =
            crate::widget::Terminal::new(Arc::new(std::sync::Mutex::new(None)), 7);
        let (width, height) = (96u32, 48u32);
        let frame = terminal.empty_frame(iced::Size::new(width as f32, height as f32));
        assert!(frame.lines.is_empty(), "no view is no rows");

        let bounds = Rectangle {
            x: 0.0,
            y: 0.0,
            width: width as f32,
            height: height as f32,
        };
        let viewport = Viewport::with_physical_size(iced::Size::new(width, height), 1.0);
        let primitive = TerminalPrimitive {
            id: 7,
            frame: Arc::new(frame),
        };
        primitive.prepare(&mut pipeline, &device, &queue, &bounds, &viewport);

        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("empty target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());

        let bytes_per_row = (width * 4).div_ceil(256) * 256;
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("empty readback"),
            size: u64::from(bytes_per_row) * u64::from(height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("empty pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_viewport(0.0, 0.0, width as f32, height as f32, 0.0, 1.0);
            assert!(
                primitive.draw(&pipeline, &mut pass),
                "an empty pane still draws its background"
            );
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        let _ = queue.submit([encoder.finish()]);

        readback.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        let _ = device.poll(wgpu::PollType::wait_indefinitely());
        let pixels = readback.slice(..).get_mapped_range().to_vec();

        let background = shader_color(Palette::default().background, 1.0);
        let expected = [
            (background[0] * 255.0).round() as u8,
            (background[1] * 255.0).round() as u8,
            (background[2] * 255.0).round() as u8,
        ];
        for row in 0..height as usize {
            let start = row * bytes_per_row as usize;
            for pixel in pixels[start..start + width as usize * 4].as_chunks::<4>().0 {
                assert_eq!(pixel[3], 255, "the background is opaque everywhere");
                assert_eq!(pixel[..3], expected, "nothing but the background is drawn");
            }
        }
    }

    fn headless_device() -> Option<(wgpu::Device, wgpu::Queue)> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let adapter =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
                .ok()?;
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).ok()
    }
}
