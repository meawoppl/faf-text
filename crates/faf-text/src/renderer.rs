use bytemuck::{Pod, Zeroable};
use cosmic_text::{Buffer, CacheKey, FontSystem};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::Color;
use crate::arena::{Arena, REPACK_FRAGMENTATION, Span};
use crate::atlas::Atlas;
use crate::curves::{CurveStore, GlyphKey};
use crate::view::Rect;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    screen_size: [f32; 2],
    _pad: [f32; 2],
}

/// Per-block data, one copy per block in a single uniform buffer bound at a
/// dynamic offset. Uniform buffers with dynamic offsets are WebGL2-clean;
/// storage buffers are not, so this stays a uniform however big it gets.
///
/// Headroom: [`UNIFORM_MIN_STRIDE`] is 256 bytes and this is 16, so issue #9
/// can replace `offset` with a mat4 (a pane placed in 3D) without changing the
/// buffer layout, the bind group, or the draw loop.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, PartialEq)]
struct BlockUniform {
    offset: [f32; 2],
    _pad: [f32; 2],
}

/// Stride between per-block uniforms. The real stride is this raised to the
/// device's `min_uniform_buffer_offset_alignment`; 256 is that alignment's
/// worst case (and what WebGL2 reports), so blocks cost the same everywhere.
const UNIFORM_MIN_STRIDE: u32 = 256;

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
    /// Curve count, tagged with `BANDED_FLAG` when band tables lead the block.
    count: u32,
    /// Maps the unit-quad corner to band space: 0..1 across the glyph's em
    /// bbox, which is the extent `curves.rs` splits into bands. The quad is
    /// the bbox plus a 1.5 px pad, so the two only differ by the pad — but the
    /// pad is a size-dependent number of em, and the bands are baked once per
    /// glyph, so the vertex shader divides it back out.
    band_scale: [f32; 2],
    band_bias: [f32; 2],
    /// Blend between the glyph's two variable-font masters: 0 = the `wght`
    /// axis minimum, 1 = its maximum. Ignored when `b_first` is 0.
    weight_t: f32,
    /// Base texel of the master-B records, 0 for a single-master glyph — the
    /// fast path every static font takes.
    b_first: u32,
}

/// Which side of the text a rect layer renders on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RectLayer {
    /// Behind the glyphs — selection backgrounds.
    Under,
    /// In front of the glyphs, alpha-blended — highlight overlays.
    Over,
}

// The draw layers a block owns, in the order they render. Layering is
// *within* a block: blocks themselves composite in insertion order, so a
// selection underlay for text in block B belongs in a block created before B.
const UNDER: usize = 0;
const VECTOR: usize = 1;
const BLEND: usize = 2;
const ATLAS: usize = 3;
const OVER: usize = 4;
const LAYERS: usize = 5;

/// Handle to a retained block of content. Blocks composite in creation order,
/// so creation order is z-order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockId(u32);

/// What to put in a block. Positions are block-local: the block's offset (see
/// [`TextRenderer::set_block_offset`]) places them on screen, and moving a
/// block never re-uploads a single instance.
pub struct BlockContent<'a> {
    /// Shaped text to draw, if any.
    pub buffer: Option<&'a Buffer>,
    /// Top-left corner of `buffer`, block-local.
    pub pos: [f32; 2],
    /// Color for glyphs that carry none of their own.
    pub default_color: Color,
    /// GPU weight blend for every glyph, as in
    /// [`TextRenderer::text_with_weight`]. `None` keeps each glyph's shaped
    /// weight.
    pub weight: Option<f32>,
    /// Rects drawn under this block's glyphs — selection backgrounds.
    pub under_rects: &'a [(Rect, Color)],
    /// Rects drawn over this block's glyphs — highlight overlays, carets.
    pub over_rects: &'a [(Rect, Color)],
}

impl Default for BlockContent<'_> {
    fn default() -> Self {
        Self {
            buffer: None,
            pos: [0.0, 0.0],
            default_color: Color::WHITE,
            weight: None,
            under_rects: &[],
            over_rects: &[],
        }
    }
}

/// What the last [`TextRenderer::finish`] actually re-uploaded. Damage
/// tracking is invisible when it works, so it is worth being able to assert on
/// it: an idle frame should report zeros, and a frame that changed one block
/// should report only that block's instances.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UploadStats {
    /// `queue.write_buffer` calls that carried instance data.
    pub content_writes: u32,
    /// Instances those calls carried.
    pub instances: u32,
    /// `queue.write_buffer` calls that carried block uniforms.
    pub uniform_writes: u32,
}

/// One instance-range upload, for the damage tests.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct UploadRecord {
    /// The block whose range was written; `None` for a whole-arena upload
    /// after growth or a repack.
    block: Option<u32>,
    layer: usize,
    span: Span,
}

/// A retained block: ranges in each instance arena, a uniform slot, and the
/// damage flags that decide what `finish` re-uploads.
struct Block {
    spans: [Span; LAYERS],
    offset: [f32; 2],
    visible: bool,
    /// Index of this block's uniform in the per-block uniform buffer.
    slot: u32,
    content_dirty: bool,
    uniform_dirty: bool,
    /// The content was built while a glyph store was out of room, or a glyph
    /// it referenced has since been evicted: re-setting it would draw more.
    stale: bool,
    /// Glyph keys this block's instances reference, deduplicated. Touched at
    /// every frame edge so the stores' LRU does not mistake retained content
    /// for cold content.
    curve_keys: Vec<GlyphKey>,
    atlas_keys: Vec<CacheKey>,
}

impl Block {
    fn new(slot: u32) -> Self {
        Self {
            spans: [Span::default(); LAYERS],
            offset: [0.0, 0.0],
            visible: true,
            slot,
            content_dirty: false,
            // The buffer's contents are undefined until written.
            uniform_dirty: true,
            stale: false,
            curve_keys: Vec::new(),
            atlas_keys: Vec::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.spans.iter().all(Span::is_empty)
    }
}

/// Instances being built, before they are committed into a block's arena
/// ranges. One shared staging area: only one block is built at a time.
#[derive(Default)]
struct Scratch {
    under: Vec<RectInstance>,
    over: Vec<RectInstance>,
    atlas: Vec<AtlasGlyphInstance>,
    vector: Vec<VectorGlyphInstance>,
    blend: Vec<VectorGlyphInstance>,
    curve_keys: FxHashSet<GlyphKey>,
    atlas_keys: FxHashSet<CacheKey>,
    /// Whether to collect the keys above. The immediate-mode block re-queries
    /// the stores every frame, so it needs no keep-alive list and should not
    /// pay a hash insert per glyph for one.
    collect_keys: bool,
    stale: bool,
}

impl Scratch {
    fn clear(&mut self) {
        self.under.clear();
        self.over.clear();
        self.atlas.clear();
        self.vector.clear();
        self.blend.clear();
        self.curve_keys.clear();
        self.atlas_keys.clear();
        self.collect_keys = false;
        self.stale = false;
    }

    fn counts(&self) -> [u32; LAYERS] {
        let mut counts = [0; LAYERS];
        counts[UNDER] = self.under.len() as u32;
        counts[VECTOR] = self.vector.len() as u32;
        counts[BLEND] = self.blend.len() as u32;
        counts[ATLAS] = self.atlas.len() as u32;
        counts[OVER] = self.over.len() as u32;
        counts
    }
}

/// GPU text renderer: vector glyphs evaluated per-pixel from Bézier outlines,
/// a bitmap atlas fallback for color emoji, and rect layers for selection and
/// highlight overlays.
///
/// Content lives in **retained blocks**. A block owns a range of each instance
/// arena and a per-block uniform; setting its content re-uploads only that
/// range, moving it re-uploads only 16 bytes, and a frame in which nothing
/// changed reports [`TextRenderer::damaged`] false so the host can skip
/// rendering and presenting entirely.
///
/// The immediate-mode API ([`TextRenderer::begin`], [`TextRenderer::rect`],
/// [`TextRenderer::text`], [`TextRenderer::finish`]) is a thin wrapper over one
/// internal block that is rebuilt every frame.
pub struct TextRenderer {
    rect_pipeline: wgpu::RenderPipeline,
    atlas_pipeline: wgpu::RenderPipeline,
    vector_pipeline: wgpu::RenderPipeline,
    /// Same shaders with `BLEND_MASTERS` on, for the glyphs that have a
    /// second variable-font master.
    blend_pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    globals_buffer: wgpu::Buffer,
    globals_dirty: bool,
    screen_size: [f32; 2],

    block_layout: wgpu::BindGroupLayout,
    block_bind_group: wgpu::BindGroup,
    block_uniforms: wgpu::Buffer,
    uniform_stride: u32,
    /// Uniform slots the buffer holds, and the bump pointer / free list that
    /// hand them out.
    uniform_capacity: u32,
    slot_top: u32,
    free_slots: Vec<u32>,

    atlas: Atlas,
    curves: CurveStore,
    /// Curve-store generations the bind group and the retained instances were
    /// built against.
    curves_generation: u64,
    curves_layout: u64,
    /// Atlas generation the retained instances' UVs were built against.
    atlas_generation: u64,
    frame: u64,

    arenas: [Arena; LAYERS],
    blocks: FxHashMap<u32, Block>,
    /// Block ids in creation order, which is z-order.
    order: Vec<u32>,
    next_id: u32,
    /// The block the immediate-mode API draws into.
    transient: BlockId,
    transient_pending: bool,

    scratch: Scratch,
    damaged: bool,
    uploads: UploadStats,
    #[cfg(test)]
    upload_log: Vec<UploadRecord>,
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

        // Screen size stays in its own global rather than being duplicated into
        // every block's uniform: a resize then writes 16 bytes instead of
        // dirtying every block, and the block uniform holds nothing but the
        // block's own placement — which is exactly what #9 wants to widen.
        let block_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("faf-text block layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: wgpu::BufferSize::new(
                        std::mem::size_of::<BlockUniform>() as u64
                    ),
                },
                count: None,
            }],
        });
        let uniform_stride = device
            .limits()
            .min_uniform_buffer_offset_alignment
            .max(UNIFORM_MIN_STRIDE);
        let uniform_capacity = 8;
        let block_uniforms = make_uniform_buffer(device, uniform_stride, uniform_capacity);
        let block_bind_group = make_block_bind_group(device, &block_layout, &block_uniforms);

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("faf-text pipeline layout"),
            bind_group_layouts: &[Some(&bind_group_layout), Some(&block_layout)],
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

        // `constants` specialize the fragment shader: the vector pipelines
        // differ only in whether BLEND_MASTERS compiles the master-B fetch in.
        let make_pipeline = |label: &str,
                             vs: &str,
                             fs: &str,
                             stride: u64,
                             attrs: &[wgpu::VertexAttribute],
                             constants: &[(&str, f64)]| {
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
                    compilation_options: wgpu::PipelineCompilationOptions {
                        constants,
                        ..Default::default()
                    },
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

        let rect_stride = std::mem::size_of::<RectInstance>();
        let atlas_stride = std::mem::size_of::<AtlasGlyphInstance>();
        let vector_stride = std::mem::size_of::<VectorGlyphInstance>();

        let rect_pipeline = make_pipeline(
            "faf-text rects",
            "rect_vs",
            "rect_fs",
            rect_stride as u64,
            &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32x4],
            &[],
        );
        let atlas_pipeline = make_pipeline(
            "faf-text atlas glyphs",
            "glyph_vs",
            "glyph_fs",
            atlas_stride as u64,
            &wgpu::vertex_attr_array![
                0 => Float32x2, 1 => Float32x2, 2 => Float32x2,
                3 => Float32x2, 4 => Float32x4, 5 => Uint32
            ],
            &[],
        );
        let vector_attrs = wgpu::vertex_attr_array![
            0 => Float32x2, 1 => Float32x2, 2 => Float32x2,
            3 => Float32x2, 4 => Float32x4, 5 => Uint32, 6 => Uint32,
            7 => Float32x2, 8 => Float32x2, 9 => Float32, 10 => Uint32
        ];
        let vector_pipeline = make_pipeline(
            "faf-text vector glyphs",
            "vector_vs",
            "vector_fs",
            vector_stride as u64,
            &vector_attrs,
            &[],
        );
        let blend_pipeline = make_pipeline(
            "faf-text vector glyphs (two masters)",
            "vector_vs",
            "vector_fs",
            vector_stride as u64,
            &vector_attrs,
            &[("BLEND_MASTERS", 1.0)],
        );

        let mut renderer = Self {
            rect_pipeline,
            atlas_pipeline,
            vector_pipeline,
            blend_pipeline,
            bind_group,
            bind_group_layout,
            sampler,
            globals_buffer,
            globals_dirty: true,
            screen_size: [0.0, 0.0],
            block_layout,
            block_bind_group,
            block_uniforms,
            uniform_stride,
            uniform_capacity,
            slot_top: 0,
            free_slots: Vec::new(),
            curves_generation: curves.generation,
            curves_layout: curves.layout_generation,
            atlas_generation: atlas.generation,
            frame: 0,
            atlas,
            curves,
            arenas: [
                Arena::new("faf-text under rects", rect_stride),
                Arena::new("faf-text vector glyphs", vector_stride),
                Arena::new("faf-text blended glyphs", vector_stride),
                Arena::new("faf-text atlas glyphs", atlas_stride),
                Arena::new("faf-text over rects", rect_stride),
            ],
            blocks: FxHashMap::default(),
            order: Vec::new(),
            next_id: 0,
            transient: BlockId(0),
            transient_pending: false,
            scratch: Scratch::default(),
            damaged: false,
            uploads: UploadStats::default(),
            #[cfg(test)]
            upload_log: Vec::new(),
        };
        // The immediate-mode block is created first, so immediate-mode content
        // composites under every block the host goes on to create.
        renderer.transient = renderer.create_block();
        renderer
    }

    // ---- Retained blocks ----

    /// Create an empty block. Blocks composite in creation order.
    pub fn create_block(&mut self) -> BlockId {
        let slot = self.free_slots.pop().unwrap_or_else(|| {
            self.slot_top += 1;
            self.slot_top - 1
        });
        let id = self.next_id;
        self.next_id += 1;
        self.blocks.insert(id, Block::new(slot));
        self.order.push(id);
        self.damaged = true;
        BlockId(id)
    }

    /// Drop a block, returning its arena ranges and uniform slot. Dropping
    /// enough blocks eventually repacks the arenas.
    pub fn drop_block(&mut self, id: BlockId) {
        let Some(mut block) = self.blocks.remove(&id.0) else {
            return;
        };
        self.order.retain(|&other| other != id.0);
        for (layer, span) in block.spans.iter_mut().enumerate() {
            self.arenas[layer].release(span);
        }
        self.free_slots.push(block.slot);
        self.damaged = true;
        self.repack_fragmented();
    }

    /// Reshape a block's content: shapes the text, packs the rects, and marks
    /// the block dirty so the next [`TextRenderer::finish`] re-uploads its
    /// ranges — and only its ranges.
    pub fn set_block_content(
        &mut self,
        queue: &wgpu::Queue,
        font_system: &mut FontSystem,
        id: BlockId,
        content: &BlockContent<'_>,
    ) {
        if !self.blocks.contains_key(&id.0) {
            return;
        }
        self.scratch.clear();
        self.scratch.collect_keys = true;
        for &(rect, color) in content.under_rects {
            self.push_rect(rect, color, RectLayer::Under);
        }
        for &(rect, color) in content.over_rects {
            self.push_rect(rect, color, RectLayer::Over);
        }
        if let Some(buffer) = content.buffer {
            self.push_text(
                queue,
                font_system,
                buffer,
                content.pos,
                content.default_color,
                content.weight,
            );
        }
        self.commit(id);
    }

    /// Move a block. No instance is touched — this is 16 bytes of uniform.
    pub fn set_block_offset(&mut self, id: BlockId, offset: [f32; 2]) {
        let Some(block) = self.blocks.get_mut(&id.0) else {
            return;
        };
        if block.offset == offset {
            return;
        }
        block.offset = offset;
        block.uniform_dirty = true;
        self.damaged = true;
    }

    /// Show or hide a block. Hidden blocks keep their arena ranges, so showing
    /// one again costs nothing.
    pub fn set_block_visible(&mut self, id: BlockId, visible: bool) {
        let Some(block) = self.blocks.get_mut(&id.0) else {
            return;
        };
        if block.visible == visible {
            return;
        }
        block.visible = visible;
        self.damaged = true;
    }

    /// True if anything changed since the last [`TextRenderer::finish`]. A host
    /// that gets `false` can skip `finish`, the render pass, and the present
    /// entirely — the last frame is still on screen and still correct.
    pub fn damaged(&self) -> bool {
        self.damaged
    }

    /// True if a block's content was built while a glyph store was out of room,
    /// or if a glyph it drew has since been evicted. Its instances are still
    /// safe to draw (missing glyphs draw nothing); re-setting the content
    /// brings them back.
    pub fn block_stale(&self, id: BlockId) -> bool {
        self.blocks.get(&id.0).is_some_and(|block| block.stale)
    }

    /// What the last [`TextRenderer::finish`] re-uploaded.
    pub fn upload_stats(&self) -> UploadStats {
        self.uploads
    }

    // ---- Immediate mode ----

    /// Start a new frame in immediate mode: advances the frame and clears the
    /// content queued by [`TextRenderer::rect`] and [`TextRenderer::text`].
    ///
    /// Retained hosts call [`TextRenderer::begin_frame`] instead.
    pub fn begin(&mut self) {
        self.begin_frame();
        self.scratch.clear();
        self.transient_pending = true;
    }

    /// Advance to the next frame without disturbing any block.
    ///
    /// This is the only point at which the glyph stores may evict — no instance
    /// drawn in the frame just ended can survive it — so a retained host must
    /// still call it once per frame, before setting any content.
    pub fn begin_frame(&mut self) {
        self.frame += 1;

        // Retained content is live but silent: it never queries the stores
        // again, so it is stamped as used for the frame about to start *before*
        // the stores decide what to evict.
        let Self {
            blocks,
            curves,
            atlas,
            frame,
            ..
        } = self;
        for block in blocks.values() {
            for key in &block.curve_keys {
                curves.touch(key, *frame);
            }
            for key in &block.atlas_keys {
                atlas.touch(key, *frame);
            }
        }

        self.curves.begin_frame(self.frame);
        self.atlas.begin_frame(self.frame);

        // Compaction moved glyphs the retained instances point at. Patch them
        // rather than blanking the blocks: the data is all still there, just
        // somewhere else.
        if self.curves_layout != self.curves.layout_generation {
            self.curves_layout = self.curves.layout_generation;
            self.relocate_glyph_instances();
        }
        // The atlas's last resort throws every shelf away, so a retained UV
        // could land on someone else's glyph. Nothing survives that; blank the
        // instances and tell the host the block wants re-setting.
        if self.atlas_generation != self.atlas.generation {
            self.atlas_generation = self.atlas.generation;
            self.blank_atlas_instances();
        }
    }

    /// Queue a solid rectangle on the given layer.
    pub fn rect(&mut self, pos: [f32; 2], size: [f32; 2], color: Color, layer: RectLayer) {
        self.push_rect([pos[0], pos[1], size[0], size[1]], color, layer);
        self.transient_pending = true;
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
        self.text_with_weight(queue, font_system, buffer, pos, default_color, None);
    }

    /// [`TextRenderer::text`] with the GPU weight blend overridden for every
    /// glyph: 0 draws the font's `wght` axis minimum, 1 its maximum, and
    /// anything between interpolates the outlines in the fragment shader. Free
    /// to animate — the curve data never changes.
    ///
    /// `None` keeps each glyph's default blend, the one matching the weight it
    /// was shaped at. Glyphs from a static font ignore this entirely.
    ///
    /// Caveat: advances come from shaping, which happened at the attrs weight.
    /// Blending far from it makes the text look tight or loose, so animate
    /// around the shaped weight rather than across the whole axis.
    pub fn text_with_weight(
        &mut self,
        queue: &wgpu::Queue,
        font_system: &mut FontSystem,
        buffer: &Buffer,
        pos: [f32; 2],
        default_color: Color,
        weight_t: Option<f32>,
    ) {
        self.push_text(queue, font_system, buffer, pos, default_color, weight_t);
        self.transient_pending = true;
    }

    // ---- Frame ----

    /// Upload everything dirty — instance ranges, block uniforms, new glyph
    /// data — and clear the damage. Call once per frame, after the last content
    /// change and before `render`.
    pub fn finish(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, screen_size: [f32; 2]) {
        self.uploads = UploadStats::default();
        #[cfg(test)]
        self.upload_log.clear();

        if self.transient_pending {
            self.transient_pending = false;
            self.commit(self.transient);
        }

        if self.screen_size != screen_size || self.globals_dirty {
            self.screen_size = screen_size;
            self.globals_dirty = false;
            queue.write_buffer(
                &self.globals_buffer,
                0,
                bytemuck::bytes_of(&Globals {
                    screen_size,
                    _pad: [0.0; 2],
                }),
            );
        }

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

        self.upload_uniforms(device, queue);
        self.upload_content(device, queue);
        self.damaged = false;
    }

    /// Record draws into an open render pass: every visible block in creation
    /// order, and within a block its selection rects, vector glyphs, atlas
    /// glyphs and overlay rects.
    pub fn render(&self, pass: &mut wgpu::RenderPass<'_>) {
        pass.set_bind_group(0, &self.bind_group, &[]);
        let pipelines = [
            &self.rect_pipeline,
            &self.vector_pipeline,
            &self.blend_pipeline,
            &self.atlas_pipeline,
            &self.rect_pipeline,
        ];
        for id in &self.order {
            let Some(block) = self.blocks.get(id) else {
                continue;
            };
            if !block.visible || block.is_empty() {
                continue;
            }
            pass.set_bind_group(
                1,
                &self.block_bind_group,
                &[block.slot * self.uniform_stride],
            );
            for (layer, &span) in block.spans.iter().enumerate() {
                if span.is_empty() {
                    continue;
                }
                let Some(buffer) = self.arenas[layer].buffer() else {
                    continue;
                };
                // The span is addressed by offsetting the vertex buffer, not by
                // a first-instance draw: base-instance draws are not a WebGL2
                // feature, buffer offsets are.
                pass.set_pipeline(pipelines[layer]);
                pass.set_vertex_buffer(0, buffer.slice(self.arenas[layer].byte_offset(span)..));
                pass.draw(0..6, 0..span.len);
            }
        }
    }

    // ---- Building ----

    fn push_rect(&mut self, rect: Rect, color: Color, layer: RectLayer) {
        let inst = RectInstance {
            pos: [rect[0], rect[1]],
            size: [rect[2], rect[3]],
            color: color.0,
        };
        match layer {
            RectLayer::Under => self.scratch.under.push(inst),
            RectLayer::Over => self.scratch.over.push(inst),
        }
    }

    fn push_text(
        &mut self,
        queue: &wgpu::Queue,
        font_system: &mut FontSystem,
        buffer: &Buffer,
        pos: [f32; 2],
        default_color: Color,
        weight_t: Option<f32>,
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
                    if self.scratch.collect_keys {
                        self.scratch.curve_keys.insert(GlyphKey::new(
                            glyph.font_id,
                            glyph.glyph_id,
                            glyph.font_weight,
                            glyph.cache_key_flags,
                        ));
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

                    // Band space is the unpadded bbox, y-up like the em coords.
                    let band_w = (gc.bbox[2] - gc.bbox[0]).max(f32::MIN_POSITIVE);
                    let band_h = (gc.bbox[3] - gc.bbox[1]).max(f32::MIN_POSITIVE);

                    let instance = VectorGlyphInstance {
                        pos: [origin_x + min_x * font_size, origin_y - max_y * font_size],
                        size: [(max_x - min_x) * font_size, (max_y - min_y) * font_size],
                        em_pos: [min_x, max_y],
                        em_size: [max_x - min_x, -(max_y - min_y)],
                        color: color.0,
                        first: gc.first,
                        count: gc.instance_count(),
                        band_scale: [(max_x - min_x) / band_w, -(max_y - min_y) / band_h],
                        band_bias: [(min_x - gc.bbox[0]) / band_w, (max_y - gc.bbox[1]) / band_h],
                        weight_t: weight_t.unwrap_or(gc.weight_t),
                        b_first: gc.b_first(),
                    };
                    // Two masters means the blending pipeline; everything else
                    // draws with a shader that never looks for one.
                    if instance.b_first == 0 {
                        self.scratch.vector.push(instance);
                    } else {
                        self.scratch.blend.push(instance);
                    }
                    continue;
                }

                // No outline: rasterize via swash into the bitmap atlas. If the
                // curve store only *ran out of room*, this glyph is a fallback
                // and the block wants re-setting once the store has compacted.
                self.scratch.stale |= self.curves.overflowed();
                let physical = glyph.physical((pos[0], baseline_y), 1.0);
                if let Some(entry) =
                    self.atlas
                        .get_or_insert(queue, font_system, physical.cache_key)
                {
                    if self.scratch.collect_keys {
                        self.scratch.atlas_keys.insert(physical.cache_key);
                    }
                    self.scratch.atlas.push(AtlasGlyphInstance {
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

    /// Move the staged instances into a block's arena ranges.
    fn commit(&mut self, id: BlockId) {
        {
            let Self {
                arenas,
                blocks,
                scratch,
                ..
            } = self;
            let Some(block) = blocks.get_mut(&id.0) else {
                return;
            };
            let counts = scratch.counts();
            // Nothing queued for a block that was already empty: not a change,
            // and an immediate-mode host that drew nothing should not damage
            // the frame.
            if block.is_empty() && counts.iter().all(|&count| count == 0) {
                return;
            }
            for (layer, &count) in counts.iter().enumerate() {
                arenas[layer].alloc(&mut block.spans[layer], count);
            }
            arenas[UNDER].write(block.spans[UNDER], &scratch.under);
            arenas[VECTOR].write(block.spans[VECTOR], &scratch.vector);
            arenas[BLEND].write(block.spans[BLEND], &scratch.blend);
            arenas[ATLAS].write(block.spans[ATLAS], &scratch.atlas);
            arenas[OVER].write(block.spans[OVER], &scratch.over);
            block.content_dirty = true;
            block.stale = scratch.stale;
            if scratch.collect_keys {
                block.curve_keys.clear();
                block.curve_keys.extend(scratch.curve_keys.iter().copied());
                block.atlas_keys.clear();
                block.atlas_keys.extend(scratch.atlas_keys.iter().copied());
            }
        }
        self.damaged = true;
        self.repack_fragmented();
    }

    /// Repack any arena whose free list has outgrown its live content.
    fn repack_fragmented(&mut self) {
        for layer in 0..LAYERS {
            if self.arenas[layer].fragmentation() > REPACK_FRAGMENTATION {
                self.repack(layer);
            }
        }
    }

    /// Copy every live span of one arena back contiguously, in draw order. The
    /// whole prefix is re-uploaded afterwards, so this costs one write of the
    /// live instances and nothing else.
    fn repack(&mut self, layer: usize) {
        let Self {
            arenas,
            blocks,
            order,
            ..
        } = self;
        let arena = &mut arenas[layer];
        let mut packed = arena.repack_buffer();
        for id in order.iter() {
            let Some(block) = blocks.get_mut(id) else {
                continue;
            };
            block.spans[layer] = arena.repack_span(&mut packed, block.spans[layer]);
        }
        arena.repack_finish(packed);
    }

    /// Rewrite the curve-texture addresses baked into retained instances after
    /// the store compacted. A glyph that did not survive draws nothing (count
    /// 0) and marks its block stale.
    fn relocate_glyph_instances(&mut self) {
        let Self {
            arenas,
            blocks,
            curves,
            ..
        } = self;
        for block in blocks.values_mut() {
            let mut touched = false;
            for layer in [VECTOR, BLEND] {
                let span = block.spans[layer];
                if span.is_empty() {
                    continue;
                }
                touched = true;
                for inst in arenas[layer].slice_mut::<VectorGlyphInstance>(span) {
                    match curves.relocations.get(&inst.first) {
                        Some(&moved) => {
                            if inst.b_first != 0 {
                                inst.b_first = moved + (inst.b_first - inst.first);
                            }
                            inst.first = moved;
                        }
                        None => {
                            // Evicted. Zero curves means zero coverage, and it
                            // clears BANDED_FLAG with it, so the shader reads
                            // nothing at the stale address.
                            inst.count = 0;
                            block.stale = true;
                        }
                    }
                }
            }
            if touched {
                block.content_dirty = true;
            }
        }
        self.damaged = true;
    }

    /// Collapse retained atlas quads to nothing after the atlas reset wholesale
    /// and their UVs stopped meaning anything.
    fn blank_atlas_instances(&mut self) {
        let Self { arenas, blocks, .. } = self;
        for block in blocks.values_mut() {
            let span = block.spans[ATLAS];
            if span.is_empty() {
                continue;
            }
            for inst in arenas[ATLAS].slice_mut::<AtlasGlyphInstance>(span) {
                inst.size = [0.0, 0.0];
            }
            block.stale = true;
            block.content_dirty = true;
        }
        self.damaged = true;
    }

    // ---- Uploads ----

    fn upload_uniforms(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        if self.slot_top > self.uniform_capacity {
            self.uniform_capacity = self.slot_top.next_power_of_two();
            self.block_uniforms =
                make_uniform_buffer(device, self.uniform_stride, self.uniform_capacity);
            self.block_bind_group =
                make_block_bind_group(device, &self.block_layout, &self.block_uniforms);
            for block in self.blocks.values_mut() {
                block.uniform_dirty = true;
            }
        }
        for block in self.blocks.values_mut() {
            if !block.uniform_dirty {
                continue;
            }
            block.uniform_dirty = false;
            queue.write_buffer(
                &self.block_uniforms,
                (block.slot * self.uniform_stride) as u64,
                bytemuck::bytes_of(&BlockUniform {
                    offset: block.offset,
                    _pad: [0.0; 2],
                }),
            );
            self.uploads.uniform_writes += 1;
        }
    }

    fn upload_content(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        let mut uploaded_whole = [false; LAYERS];
        for (layer, arena) in self.arenas.iter_mut().enumerate() {
            arena.reserve(device);
            if !arena.needs_full_upload() {
                continue;
            }
            let instances = arena.upload_all(queue);
            uploaded_whole[layer] = true;
            self.uploads.content_writes += 1;
            self.uploads.instances += instances;
            #[cfg(test)]
            self.upload_log.push(UploadRecord {
                block: None,
                layer,
                span: Span {
                    start: 0,
                    cap: instances,
                    len: instances,
                },
            });
        }

        for id in &self.order {
            let Some(block) = self.blocks.get_mut(id) else {
                continue;
            };
            if !block.content_dirty {
                continue;
            }
            block.content_dirty = false;
            for (layer, &span) in block.spans.iter().enumerate() {
                if uploaded_whole[layer] || span.is_empty() {
                    continue;
                }
                self.uploads.content_writes += 1;
                self.uploads.instances += self.arenas[layer].upload(queue, span);
                #[cfg(test)]
                self.upload_log.push(UploadRecord {
                    block: Some(*id),
                    layer,
                    span,
                });
            }
        }
    }
}

fn make_uniform_buffer(device: &wgpu::Device, stride: u32, slots: u32) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("faf-text block uniforms"),
        size: (stride * slots) as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn make_block_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    buffer: &wgpu::Buffer,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("faf-text block bind group"),
        layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            // Bind one block's worth; the dynamic offset picks which one.
            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                buffer,
                offset: 0,
                size: wgpu::BufferSize::new(std::mem::size_of::<BlockUniform>() as u64),
            }),
        }],
    })
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
        view_in(font_system, text, size, pos, Family::SansSerif)
    }

    fn view_in(
        font_system: &mut FontSystem,
        text: &str,
        size: f32,
        pos: [f32; 2],
        family: Family,
    ) -> TextView {
        let mut view = TextView::new(font_system, Metrics::new(size, size * 1.25));
        view.pos = pos;
        view.set_text(font_system, text, &Attrs::new().family(family));
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

    /// Draw one view at a fixed GPU weight blend and read the frame back.
    fn weighted_frame(
        renderer: &mut TextRenderer,
        font_system: &mut FontSystem,
        view: &TextView,
        weight_t: Option<f32>,
    ) -> Vec<u8> {
        let (_, queue) = testing::gpu();
        renderer.begin();
        renderer.text_with_weight(
            queue,
            font_system,
            &view.buffer,
            view.pos,
            Color::WHITE,
            weight_t,
        );
        testing::render_pixels(renderer, W, H)
    }

    /// Total coverage in a frame — how much ink the glyphs put down.
    fn ink(pixels: &[u8]) -> u64 {
        pixels.iter().map(|&p| p as u64).sum()
    }

    /// True if anything at all landed on the black clear color. (Alpha comes
    /// back 255 everywhere, so a plain "any non-zero byte" test would pass on
    /// an empty frame.)
    fn drew(pixels: &[u8]) -> bool {
        pixels
            .chunks_exact(4)
            .any(|px| px[..3].iter().any(|&c| c != 0))
    }

    #[test]
    fn the_gpu_weight_blend_grades_a_variable_font_between_its_masters() {
        let mut font_system = testing::variable_font_system();
        let sample = view_in(
            &mut font_system,
            "weight",
            40.0,
            [6.0, 6.0],
            Family::Name(testing::VARIABLE_FAMILY),
        );
        let mut renderer = renderer(2048, 2048);

        let steps: Vec<u64> = [0.0, 0.25, 0.5, 0.75, 1.0]
            .iter()
            .map(|&t| {
                ink(&weighted_frame(
                    &mut renderer,
                    &mut font_system,
                    &sample,
                    Some(t),
                ))
            })
            .collect();
        assert!(steps[0] > 0, "the sample drew something");
        assert!(
            renderer.scratch.vector.is_empty() && !renderer.scratch.blend.is_empty(),
            "a variable face draws on the blending pipeline"
        );
        assert!(
            renderer.scratch.blend.iter().all(|g| g.b_first != 0),
            "every glyph of a variable face carries a second master"
        );
        // Heavier all the way up the axis, and no step is a jump to a
        // completely different shape: the outline moves continuously.
        for pair in steps.windows(2) {
            assert!(pair[1] > pair[0], "weight steps must add ink: {steps:?}");
            assert!(
                pair[1] < pair[0] * 2,
                "a step should nudge the outline, not replace it: {steps:?}"
            );
        }

        // The default blend is the one matching the shaped weight (400 of
        // 200..800), so it lands inside the range the overrides span.
        let default = ink(&weighted_frame(
            &mut renderer,
            &mut font_system,
            &sample,
            None,
        ));
        assert!(default > steps[0] && default < steps[4]);
    }

    #[test]
    fn a_static_font_ignores_the_weight_blend_entirely() {
        let mut font_system = testing::font_system();
        let sample = view_in(
            &mut font_system,
            "weight",
            40.0,
            [6.0, 6.0],
            Family::Name(testing::STATIC_FAMILY),
        );
        let mut renderer = renderer(2048, 2048);

        let plain = weighted_frame(&mut renderer, &mut font_system, &sample, None);
        assert!(
            renderer.scratch.vector.iter().all(|g| g.b_first == 0),
            "a static face has no second master to blend to"
        );
        assert!(ink(&plain) > 0, "the sample drew something");
        for t in [0.0, 0.5, 1.0] {
            let forced = weighted_frame(&mut renderer, &mut font_system, &sample, Some(t));
            assert_eq!(plain, forced, "weight_t {t} disturbed a static font");
        }
    }

    #[test]
    fn band_tables_do_not_move_a_single_pixel() {
        let (device, _) = testing::gpu();
        let mut font_system = testing::font_system();
        // Every one of these glyphs clears the 16-curve banding threshold. 11px
        // takes the three-tap path, where a tap's ray offset can land in
        // another band; 30px takes the single-ray one.
        for size in [11.0, 30.0] {
            let sample = view(&mut font_system, "Q@g&%8", size, [6.0, 6.0]);
            let mut with_bands = renderer(2048, 2048);
            let banded = frame(&mut with_bands, &mut font_system, &sample, None);

            let mut store = CurveStore::with_size(device, CURVE_TEX_WIDTH, 2048, 2048);
            store.band_min_curves = u32::MAX; // every glyph keeps the flat layout
            let mut without =
                TextRenderer::with_stores(device, testing::FORMAT, Atlas::new(device), store);
            let flat = frame(&mut without, &mut font_system, &sample, None);

            assert!(banded.iter().any(|&b| b != 0), "the sample drew something");
            assert!(
                with_bands
                    .scratch
                    .vector
                    .iter()
                    .any(|g| g.count & crate::curves::BANDED_FLAG != 0),
                "the sample should exercise the banded path at {size}px"
            );
            assert!(
                without
                    .scratch
                    .vector
                    .iter()
                    .all(|g| g.count & crate::curves::BANDED_FLAG == 0),
                "the reference render must be unbanded"
            );
            assert_eq!(banded, flat, "banding changed coverage at {size}px");
        }
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
            renderer.scratch.atlas.is_empty(),
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
            !renderer.scratch.atlas.is_empty(),
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

    // ---- Retained blocks ----

    /// Set one block's content to a shaped view at its position.
    fn set_text_block(
        renderer: &mut TextRenderer,
        font_system: &mut FontSystem,
        id: BlockId,
        view: &TextView,
    ) {
        let (_, queue) = testing::gpu();
        renderer.set_block_content(
            queue,
            font_system,
            id,
            &BlockContent {
                buffer: Some(&view.buffer),
                pos: view.pos,
                default_color: Color::WHITE,
                ..Default::default()
            },
        );
    }

    fn set_rect_block(renderer: &mut TextRenderer, id: BlockId, rects: &[(Rect, Color)]) {
        let (_, queue) = testing::gpu();
        let mut font_system = testing::font_system();
        renderer.set_block_content(
            queue,
            &mut font_system,
            id,
            &BlockContent {
                under_rects: rects,
                ..Default::default()
            },
        );
    }

    /// Pixels of a frame drawn from whatever the blocks currently hold.
    fn retained_frame(renderer: &mut TextRenderer) -> Vec<u8> {
        testing::render_pixels(renderer, W, H)
    }

    #[test]
    fn re_setting_one_block_leaves_the_other_alone() {
        let mut font_system = testing::font_system();
        let mut renderer = renderer(2048, 2048);
        let top = view(&mut font_system, "retained", 24.0, [6.0, 6.0]);
        let bottom = view(&mut font_system, "damage", 24.0, [6.0, 120.0]);

        let a = renderer.create_block();
        let b = renderer.create_block();
        renderer.begin_frame();
        set_text_block(&mut renderer, &mut font_system, a, &top);
        set_text_block(&mut renderer, &mut font_system, b, &bottom);
        let both = retained_frame(&mut renderer);
        assert!(drew(&both), "the blocks drew something");

        // A frame that changes nothing uploads nothing and asks for nothing.
        renderer.begin_frame();
        assert!(!renderer.damaged(), "an untouched scene is not damaged");
        let idle = retained_frame(&mut renderer);
        assert_eq!(idle, both, "an idle frame renders the same pixels");
        assert_eq!(renderer.upload_stats(), UploadStats::default());

        // Re-set block B with the same content: the pixels stay identical, and
        // only B's ranges are written.
        renderer.begin_frame();
        set_text_block(&mut renderer, &mut font_system, b, &bottom);
        assert!(renderer.damaged());
        let again = retained_frame(&mut renderer);
        assert_eq!(again, both, "re-setting a block changed the frame");
        assert!(
            renderer.upload_log.iter().all(|up| up.block == Some(b.0)),
            "only the dirty block's ranges may upload: {:?}",
            renderer.upload_log
        );
        assert_eq!(
            renderer.upload_stats().content_writes,
            1,
            "one vector range"
        );
        assert_eq!(renderer.upload_stats().uniform_writes, 0);

        // And block A's pixels are untouched by a *different* content in B.
        let empty = view(&mut font_system, "", 24.0, [6.0, 120.0]);
        renderer.begin_frame();
        set_text_block(&mut renderer, &mut font_system, b, &empty);
        let without_b = retained_frame(&mut renderer);
        assert_eq!(
            without_b[..CROP],
            both[..CROP],
            "block A must not move when block B is re-set"
        );
        assert!(!drew(&without_b[CROP..]), "block B should be gone");
    }

    #[test]
    fn offset_only_frames_upload_no_content() {
        let mut font_system = testing::font_system();
        let mut renderer = renderer(2048, 2048);
        let sample = view(&mut font_system, "scroll", 20.0, [6.0, 6.0]);

        let block = renderer.create_block();
        renderer.begin_frame();
        set_text_block(&mut renderer, &mut font_system, block, &sample);
        let base = retained_frame(&mut renderer);
        assert!(drew(&base));

        let mut content_writes = 0;
        for step in 1..=100 {
            renderer.begin_frame();
            renderer.set_block_offset(block, [0.0, step as f32]);
            assert!(renderer.damaged(), "a moved block still needs re-recording");
            retained_frame(&mut renderer);
            content_writes += renderer.upload_stats().content_writes;
            assert_eq!(renderer.upload_stats().uniform_writes, 1);
        }
        assert_eq!(content_writes, 0, "scrolling must not re-upload instances");

        // Back where it started: the offset uniform is the only difference, so
        // the pixels have to match to the byte.
        renderer.begin_frame();
        renderer.set_block_offset(block, [0.0, 0.0]);
        let home = retained_frame(&mut renderer);
        assert_eq!(home, base, "a block's offset must not disturb its raster");

        // A block moved by 30 px draws what a block built 30 px lower draws.
        renderer.begin_frame();
        renderer.set_block_offset(block, [0.0, 30.0]);
        let moved = retained_frame(&mut renderer);
        let lower = view(&mut font_system, "scroll", 20.0, [6.0, 36.0]);
        renderer.begin_frame();
        renderer.set_block_offset(block, [0.0, 0.0]);
        set_text_block(&mut renderer, &mut font_system, block, &lower);
        assert_eq!(moved, retained_frame(&mut renderer));
    }

    #[test]
    fn dropping_blocks_repacks_the_arena_without_disturbing_the_survivors() {
        const KEPT: [usize; 3] = [1, 4, 7];
        let mut renderer = renderer(2048, 2048);
        let color = Color::rgba(0.2, 0.6, 0.9, 1.0);
        let row = |i: usize| ([6.0, 6.0 + i as f32 * 20.0, 60.0, 12.0], color);

        let blocks: Vec<BlockId> = (0..8).map(|_| renderer.create_block()).collect();
        renderer.begin_frame();
        for (i, &block) in blocks.iter().enumerate() {
            set_rect_block(&mut renderer, block, &[row(i)]);
        }
        retained_frame(&mut renderer);
        let filled = renderer.arenas[UNDER].top();
        assert!(filled > 0);

        // Five of eight spans freed is past the repack threshold, so the arena
        // compacts itself around what is left.
        for (i, &block) in blocks.iter().enumerate() {
            if !KEPT.contains(&i) {
                renderer.drop_block(block);
            }
        }
        assert_eq!(renderer.arenas[UNDER].freed(), 0, "the arena repacked");
        assert!(renderer.arenas[UNDER].top() < filled);
        let survivors = retained_frame(&mut renderer);

        // The same scene built from scratch must render identically — the
        // repack moved instances, not geometry.
        let mut fresh = self::renderer(2048, 2048);
        for i in KEPT {
            let block = fresh.create_block();
            fresh.begin_frame();
            set_rect_block(&mut fresh, block, &[row(i)]);
        }
        assert_eq!(survivors, retained_frame(&mut fresh));
        assert!(drew(&survivors), "rects drew something");
    }

    #[test]
    fn hiding_a_block_costs_no_upload() {
        let mut font_system = testing::font_system();
        let mut renderer = renderer(2048, 2048);
        let sample = view(&mut font_system, "visible", 20.0, [6.0, 6.0]);
        let block = renderer.create_block();
        renderer.begin_frame();
        set_text_block(&mut renderer, &mut font_system, block, &sample);
        let shown = retained_frame(&mut renderer);

        renderer.begin_frame();
        renderer.set_block_visible(block, false);
        assert!(renderer.damaged());
        let hidden = retained_frame(&mut renderer);
        assert!(!drew(&hidden), "a hidden block draws nothing");
        assert_eq!(renderer.upload_stats(), UploadStats::default());

        renderer.begin_frame();
        renderer.set_block_visible(block, true);
        assert_eq!(retained_frame(&mut renderer), shown);
    }

    #[test]
    fn retained_blocks_survive_a_curve_store_compaction() {
        let mut font_system = testing::font_system();
        let sample = view(&mut font_system, "Abg", 30.0, [6.0, 6.0]);
        let mut filler = view(&mut font_system, FILLER, 13.0, [6.0, 120.0]);
        filler.set_size(&mut font_system, Some(W as f32 - 12.0), None);

        // A capped store: the filler cannot fit, which forces a compaction.
        let mut renderer = renderer(2, 2);
        let block = renderer.create_block();
        renderer.begin_frame();
        set_text_block(&mut renderer, &mut font_system, block, &sample);
        let before = retained_frame(&mut renderer);
        assert!(drew(&before));

        // Immediate-mode filler in the same frame overflows the store.
        let (_, queue) = testing::gpu();
        renderer.begin();
        renderer.text(
            queue,
            &mut font_system,
            &filler.buffer,
            filler.pos,
            Color::WHITE,
        );
        retained_frame(&mut renderer);
        assert!(
            renderer.curves.overflowed(),
            "the store should be out of room"
        );

        // The compaction runs here and relocates what survived. The retained
        // block never re-set its content, so only the patch can save it — and
        // its glyphs are the ones the frame edge just touched.
        renderer.begin();
        let after = retained_frame(&mut renderer);
        assert!(renderer.curves.generation > 0, "compaction ran");
        assert_eq!(
            before, after,
            "a retained block must survive a curve relocation"
        );
    }

    #[test]
    fn immediate_mode_and_a_block_draw_the_same_pixels() {
        let mut font_system = testing::font_system();
        let sample = view(&mut font_system, "compat", 28.0, [6.0, 6.0]);

        let mut immediate = renderer(2048, 2048);
        let direct = frame(&mut immediate, &mut font_system, &sample, None);

        let mut retained = renderer(2048, 2048);
        let block = retained.create_block();
        retained.begin_frame();
        set_text_block(&mut retained, &mut font_system, block, &sample);
        assert_eq!(direct, retained_frame(&mut retained));
        assert!(direct.iter().any(|&p| p != 0));
    }
}
