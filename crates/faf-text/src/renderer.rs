use bytemuck::{Pod, Zeroable};
use cosmic_text::{Buffer, FontSystem};

use crate::Color;
use crate::atlas::Atlas;
use crate::curves::CurveStore;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    screen_size: [f32; 2],
    _pad: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct RectInstance {
    pos: [f32; 2],
    size: [f32; 2],
    color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct AtlasGlyphInstance {
    pos: [f32; 2],
    size: [f32; 2],
    uv_pos: [f32; 2],
    uv_size: [f32; 2],
    color: [f32; 4],
    kind: u32,
    _pad: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct VectorGlyphInstance {
    pos: [f32; 2],
    size: [f32; 2],
    em_pos: [f32; 2],
    em_size: [f32; 2],
    color: [f32; 4],
    first: u32,
    count: u32,
    _pad: [u32; 2],
}

/// Which side of the text a rect layer renders on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RectLayer {
    /// Behind the glyphs — selection backgrounds.
    Under,
    /// In front of the glyphs, alpha-blended — highlight overlays.
    Over,
}

struct InstanceBuffer {
    buffer: Option<wgpu::Buffer>,
    capacity: u64,
    len: u32,
}

impl InstanceBuffer {
    fn new() -> Self {
        Self {
            buffer: None,
            capacity: 0,
            len: 0,
        }
    }

    fn upload<T: Pod>(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, data: &[T]) {
        self.len = data.len() as u32;
        if data.is_empty() {
            return;
        }
        let bytes: &[u8] = bytemuck::cast_slice(data);
        if self.buffer.is_none() || self.capacity < bytes.len() as u64 {
            let capacity = (bytes.len() as u64).next_power_of_two().max(4096);
            self.buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("faf-text instances"),
                size: capacity,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
            self.capacity = capacity;
        }
        queue.write_buffer(self.buffer.as_ref().unwrap(), 0, bytes);
    }
}

/// GPU text renderer: vector glyphs evaluated per-pixel from Bézier outlines,
/// a bitmap atlas fallback for color emoji, and rect layers for selection and
/// highlight overlays. One instanced draw per layer.
pub struct TextRenderer {
    rect_pipeline: wgpu::RenderPipeline,
    atlas_pipeline: wgpu::RenderPipeline,
    vector_pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    globals_buffer: wgpu::Buffer,

    atlas: Atlas,
    curves: CurveStore,

    under_rects: Vec<RectInstance>,
    over_rects: Vec<RectInstance>,
    atlas_glyphs: Vec<AtlasGlyphInstance>,
    vector_glyphs: Vec<VectorGlyphInstance>,

    under_buf: InstanceBuffer,
    over_buf: InstanceBuffer,
    atlas_buf: InstanceBuffer,
    vector_buf: InstanceBuffer,
}

impl TextRenderer {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("faf-text shaders"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders.wgsl").into()),
        });

        let atlas = Atlas::new(device);
        let curves = CurveStore::new(device);

        let globals_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("faf-text globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("faf-text atlas sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("faf-text bind group layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
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
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("faf-text bind group"),
            layout: &bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: globals_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&atlas.view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&curves.view),
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("faf-text pipeline layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let blend = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::SrcAlpha,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
        };

        let make_pipeline =
            |label: &str, vs: &str, fs: &str, stride: u64, attrs: &[wgpu::VertexAttribute]| {
                device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some(label),
                    layout: Some(&pipeline_layout),
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
                    fragment: Some(wgpu::FragmentState {
                        module: &shader,
                        entry_point: Some(fs),
                        compilation_options: Default::default(),
                        targets: &[Some(wgpu::ColorTargetState {
                            format,
                            blend: Some(blend),
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                    }),
                    primitive: wgpu::PrimitiveState::default(),
                    depth_stencil: None,
                    multisample: wgpu::MultisampleState::default(),
                    multiview_mask: None,
                    cache: None,
                })
            };

        let rect_pipeline = make_pipeline(
            "faf-text rects",
            "rect_vs",
            "rect_fs",
            std::mem::size_of::<RectInstance>() as u64,
            &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32x4],
        );
        let atlas_pipeline = make_pipeline(
            "faf-text atlas glyphs",
            "glyph_vs",
            "glyph_fs",
            std::mem::size_of::<AtlasGlyphInstance>() as u64,
            &wgpu::vertex_attr_array![
                0 => Float32x2, 1 => Float32x2, 2 => Float32x2,
                3 => Float32x2, 4 => Float32x4, 5 => Uint32
            ],
        );
        let vector_pipeline = make_pipeline(
            "faf-text vector glyphs",
            "vector_vs",
            "vector_fs",
            std::mem::size_of::<VectorGlyphInstance>() as u64,
            &wgpu::vertex_attr_array![
                0 => Float32x2, 1 => Float32x2, 2 => Float32x2,
                3 => Float32x2, 4 => Float32x4, 5 => Uint32, 6 => Uint32
            ],
        );

        Self {
            rect_pipeline,
            atlas_pipeline,
            vector_pipeline,
            bind_group,
            globals_buffer,
            atlas,
            curves,
            under_rects: Vec::new(),
            over_rects: Vec::new(),
            atlas_glyphs: Vec::new(),
            vector_glyphs: Vec::new(),
            under_buf: InstanceBuffer::new(),
            over_buf: InstanceBuffer::new(),
            atlas_buf: InstanceBuffer::new(),
            vector_buf: InstanceBuffer::new(),
        }
    }

    /// Start a new frame; clears all queued instances.
    pub fn begin(&mut self) {
        self.under_rects.clear();
        self.over_rects.clear();
        self.atlas_glyphs.clear();
        self.vector_glyphs.clear();
    }

    /// Queue a solid rectangle on the given layer.
    pub fn rect(&mut self, pos: [f32; 2], size: [f32; 2], color: Color, layer: RectLayer) {
        let inst = RectInstance {
            pos,
            size,
            color: color.0,
        };
        match layer {
            RectLayer::Under => self.under_rects.push(inst),
            RectLayer::Over => self.over_rects.push(inst),
        }
    }

    /// Queue every glyph of a shaped buffer at `pos` (its top-left corner).
    ///
    /// Glyphs with outlines take the GPU vector path at exact fractional
    /// positions; bitmap-only glyphs (color emoji) land in the atlas.
    pub fn text(
        &mut self,
        queue: &wgpu::Queue,
        font_system: &mut FontSystem,
        buffer: &Buffer,
        pos: [f32; 2],
        default_color: Color,
    ) {
        for run in buffer.layout_runs() {
            let baseline_y = pos[1] + run.line_y;
            for glyph in run.glyphs.iter() {
                let color = glyph
                    .color_opt
                    .map(Color::from_cosmic)
                    .unwrap_or(default_color);
                let font_size = glyph.font_size;

                if let Some(gc) = self.curves.get_or_insert(
                    font_system,
                    glyph.font_id,
                    glyph.glyph_id,
                    glyph.font_weight,
                    glyph.cache_key_flags,
                ) {
                    if gc.count == 0 {
                        continue; // whitespace
                    }
                    // Unsnapped glyph origin: the analytic coverage handles
                    // fractional positions exactly.
                    let origin_x = pos[0] + glyph.x + glyph.x_offset * font_size;
                    let origin_y = baseline_y + glyph.y - glyph.y_offset * font_size;

                    let pad = 1.5 / font_size;
                    let min_x = gc.bbox[0] - pad;
                    let min_y = gc.bbox[1] - pad;
                    let max_x = gc.bbox[2] + pad;
                    let max_y = gc.bbox[3] + pad;

                    self.vector_glyphs.push(VectorGlyphInstance {
                        pos: [origin_x + min_x * font_size, origin_y - max_y * font_size],
                        size: [(max_x - min_x) * font_size, (max_y - min_y) * font_size],
                        em_pos: [min_x, max_y],
                        em_size: [max_x - min_x, -(max_y - min_y)],
                        color: color.0,
                        first: gc.first,
                        count: gc.count,
                        _pad: [0; 2],
                    });
                    continue;
                }

                // No outline: rasterize via swash into the bitmap atlas.
                let physical = glyph.physical((pos[0], baseline_y), 1.0);
                if let Some(entry) =
                    self.atlas
                        .get_or_insert(queue, font_system, physical.cache_key)
                {
                    self.atlas_glyphs.push(AtlasGlyphInstance {
                        pos: [
                            (physical.x + entry.left) as f32,
                            (physical.y - entry.top) as f32,
                        ],
                        size: entry.size,
                        uv_pos: entry.uv_pos,
                        uv_size: entry.uv_size,
                        color: color.0,
                        kind: entry.is_color as u32,
                        _pad: [0; 3],
                    });
                }
            }
        }
    }

    /// Upload all queued instances and new glyph data. Call once per frame,
    /// after the last `text`/`rect` call and before `render`.
    pub fn finish(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, screen_size: [f32; 2]) {
        queue.write_buffer(
            &self.globals_buffer,
            0,
            bytemuck::bytes_of(&Globals {
                screen_size,
                _pad: [0.0; 2],
            }),
        );
        self.curves.flush(queue);
        self.under_buf.upload(device, queue, &self.under_rects);
        self.over_buf.upload(device, queue, &self.over_rects);
        self.atlas_buf.upload(device, queue, &self.atlas_glyphs);
        self.vector_buf.upload(device, queue, &self.vector_glyphs);
    }

    /// Record draws into an open render pass:
    /// selection rects, vector glyphs, atlas glyphs, then overlay rects.
    pub fn render(&self, pass: &mut wgpu::RenderPass<'_>) {
        pass.set_bind_group(0, &self.bind_group, &[]);

        let draws = [
            (&self.rect_pipeline, &self.under_buf),
            (&self.vector_pipeline, &self.vector_buf),
            (&self.atlas_pipeline, &self.atlas_buf),
            (&self.rect_pipeline, &self.over_buf),
        ];
        for (pipeline, instances) in draws {
            if instances.len == 0 {
                continue;
            }
            let Some(buffer) = &instances.buffer else {
                continue;
            };
            pass.set_pipeline(pipeline);
            pass.set_vertex_buffer(0, buffer.slice(..));
            pass.draw(0..6, 0..instances.len);
        }
    }
}
