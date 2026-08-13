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
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    globals_buffer: wgpu::Buffer,

    atlas: Atlas,
    curves: CurveStore,
    /// Curve-store generation the current bind group was built against.
    curves_generation: u64,
    frame: u64,

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
        Self::with_stores(device, format, Atlas::new(device), CurveStore::new(device))
    }

    /// Build a renderer around pre-sized glyph stores (tests use tiny ones to
    /// exercise growth and eviction).
    pub(crate) fn with_stores(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        atlas: Atlas,
        curves: CurveStore,
    ) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("faf-text shaders"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders.wgsl").into()),
        });

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

        let bind_group = make_bind_group(
            device,
            &bind_group_layout,
            &globals_buffer,
            &sampler,
            &atlas,
            &curves,
        );

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
            bind_group_layout,
            sampler,
            globals_buffer,
            curves_generation: curves.generation,
            frame: 0,
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
    ///
    /// This is also the only point at which the glyph stores may evict — no
    /// instance queued for the frame just ended can survive it.
    pub fn begin(&mut self) {
        self.frame += 1;
        self.curves.begin_frame(self.frame);
        self.atlas.begin_frame(self.frame);
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
        // Growth or compaction replaced the curve texture this frame; the old
        // bind group still points at the retired one.
        if self.curves_generation != self.curves.generation {
            self.curves_generation = self.curves.generation;
            self.bind_group = make_bind_group(
                device,
                &self.bind_group_layout,
                &self.globals_buffer,
                &self.sampler,
                &self.atlas,
                &self.curves,
            );
        }
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

fn make_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    globals: &wgpu::Buffer,
    sampler: &wgpu::Sampler,
    atlas: &Atlas,
    curves: &CurveStore,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("faf-text bind group"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: globals.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(&atlas.view),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: wgpu::BindingResource::TextureView(&curves.view),
            },
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curves::CURVE_TEX_WIDTH;
    use crate::testing;
    use crate::{Attrs, Family, Metrics, TextView};

    const W: u32 = 220;
    const H: u32 = 220;
    /// Rows the sample text occupies; the filler text is drawn well below.
    const CROP: usize = (W * 4 * 60) as usize;
    /// A pile of distinct glyphs — enough to blow past a two-row curve texture.
    const FILLER: &str = "quick brown fox jumps over the lazy dog 0123456789 ΓΔΘΞΣΦΨΩ";

    fn renderer(curve_height: u32, curve_max_height: u32) -> TextRenderer {
        let (device, _) = testing::gpu();
        TextRenderer::with_stores(
            device,
            testing::FORMAT,
            Atlas::new(device),
            CurveStore::with_size(device, CURVE_TEX_WIDTH, curve_height, curve_max_height),
        )
    }

    fn view(font_system: &mut FontSystem, text: &str, size: f32, pos: [f32; 2]) -> TextView {
        let mut view = TextView::new(font_system, Metrics::new(size, size * 1.25));
        view.pos = pos;
        view.set_text(font_system, text, &Attrs::new().family(Family::SansSerif));
        view
    }

    /// Draw `sample` alone, plus optionally `filler` far below it, and read the
    /// frame back.
    fn frame(
        renderer: &mut TextRenderer,
        font_system: &mut FontSystem,
        sample: &TextView,
        filler: Option<&TextView>,
    ) -> Vec<u8> {
        let (_, queue) = testing::gpu();
        renderer.begin();
        renderer.text(queue, font_system, &sample.buffer, sample.pos, Color::WHITE);
        if let Some(filler) = filler {
            renderer.text(queue, font_system, &filler.buffer, filler.pos, Color::WHITE);
        }
        testing::render_pixels(renderer, W, H)
    }

    #[test]
    fn glyphs_render_identically_across_a_curve_texture_growth() {
        let mut font_system = testing::font_system();
        let sample = view(&mut font_system, "Abg", 30.0, [6.0, 6.0]);
        let mut filler = view(&mut font_system, FILLER, 13.0, [6.0, 120.0]);
        filler.set_size(&mut font_system, Some(W as f32 - 12.0), None);

        // Two rows to start with; "Abg" fits, the filler does not.
        let mut renderer = renderer(2, 256);
        let before = frame(&mut renderer, &mut font_system, &sample, None);
        assert_eq!(renderer.curves.generation, 0, "no growth yet");

        frame(&mut renderer, &mut font_system, &sample, Some(&filler));
        assert!(renderer.curves.generation > 0, "the texture should grow");
        assert!(
            renderer.atlas_glyphs.is_empty(),
            "growth means nothing falls back to the atlas"
        );

        let after = frame(&mut renderer, &mut font_system, &sample, None);
        assert!(before.iter().any(|&b| b != 0), "sample text drew something");
        assert_eq!(before, after, "growth must not disturb existing glyphs");
    }

    #[test]
    fn overflow_at_the_cap_falls_back_without_corrupting_queued_glyphs() {
        let mut font_system = testing::font_system();
        let sample = view(&mut font_system, "Abg", 30.0, [6.0, 6.0]);
        let mut filler = view(&mut font_system, FILLER, 13.0, [6.0, 120.0]);
        filler.set_size(&mut font_system, Some(W as f32 - 12.0), None);

        // Capped at its initial size: the filler cannot be made to fit.
        let mut renderer = renderer(2, 2);
        let before = frame(&mut renderer, &mut font_system, &sample, None);

        let overflowed = frame(&mut renderer, &mut font_system, &sample, Some(&filler));
        assert_eq!(renderer.curves.generation, 0, "no eviction mid-frame");
        assert!(
            !renderer.atlas_glyphs.is_empty(),
            "overflowing glyphs fall back to the bitmap atlas"
        );
        assert_eq!(
            before[..CROP],
            overflowed[..CROP],
            "already-queued glyphs must keep rendering exactly as before"
        );

        // The deferred compaction lands at the next frame edge and rewrites
        // every surviving `first`; the same text must still render the same.
        let after = frame(&mut renderer, &mut font_system, &sample, None);
        assert!(renderer.curves.generation > 0, "compaction ran at begin()");
        assert_eq!(before, after, "compaction must not disturb rendering");
    }
}
