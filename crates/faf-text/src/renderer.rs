use bytemuck::{Pod, Zeroable};
use cosmic_text::{Buffer, CacheKey, FontSystem, LayoutRun, UnderlineStyle};
use glam::Mat4;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::Color;
use crate::arena::{Arena, REPACK_FRAGMENTATION, Span};
use crate::atlas::Atlas;
use crate::curves::{CurveStore, GlyphKey};
use crate::math;
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
/// [`UNIFORM_MIN_STRIDE`] is 256 bytes and this is 80, so the mat4 fits the
/// layout #4 shipped without touching the bind group or the draw loop.
///
/// `transform` maps block-local px to *homogeneous screen pixels*, not to clip
/// space: the shader's own last step is the divide by the screen size, so a
/// host's view-projection is folded in here as `px_from_clip * vp * model`.
/// The pay-off is that a 2D block's transform is a plain translation and the
/// vertex math stays bit-for-bit what it was before this matrix existed.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, PartialEq, Debug)]
struct BlockUniform {
    transform: [[f32; 4]; 4],
    flags: u32,
    _pad: [u32; 3],
}

/// [`BlockUniform::flags`]: the placement is an axis-aligned scale plus
/// translation, so the atlas path can snap its quads to the pixel grid. Must
/// match `BLOCK_SNAP` in shaders.wgsl.
const BLOCK_SNAP: u32 = 1;

/// How far off axis a placement may be and still count as axis-aligned. A
/// hand-built ortho matrix has exact zeros here; this is slack for one that
/// came out of a matrix product.
const AXIS_EPSILON: f32 = 1e-6;

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

/// A decoration's shape. Every kind draws from one pipeline, so a block's
/// decorations cost two draws however many shapes they mix.
///
/// Geometry comes from [`crate::TextView::decoration_rects`], which anchors
/// the line kinds to the baseline using the font's own metrics.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DecorationKind {
    /// A solid bar, drawn at the font's underline offset.
    Underline,
    /// A solid bar, drawn at the font's strikeout offset.
    Strikethrough,
    /// A sine wave — the diagnostics squiggle. Its stroke thickness,
    /// amplitude and wavelength all follow from the height of the rect it is
    /// given, so it scales with the text without a second set of knobs.
    Squiggle,
    /// A rounded-rect background: inline code, pills, tags. Drawn *under* the
    /// glyphs, unlike the line kinds.
    Chip { radius_px: f32 },
}

impl DecorationKind {
    /// True for the kinds that draw behind the glyphs.
    fn is_chip(self) -> bool {
        matches!(self, DecorationKind::Chip { .. })
    }
}

/// One decoration instance: a rect, a shape, and the shape's parameters.
///
/// `pos`/`size` are the box the shape is drawn in — for a solid bar that is
/// the bar itself, for a squiggle the band its wave sweeps, for a chip the
/// chip. `params` carries what the fragment shader needs beyond the box.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq)]
struct DecorationInstance {
    pos: [f32; 2],
    size: [f32; 2],
    color: [f32; 4],
    /// Squiggle: amplitude, wavelength, stroke thickness (px). Chip: corner
    /// radius (px). Unused by the solid kinds.
    params: [f32; 4],
    kind: u32,
    _pad: [u32; 3],
}

// Shape codes, matching `DECO_KIND_*` in shaders.wgsl.
const DECO_SOLID: u32 = 0;
const DECO_SQUIGGLE: u32 = 1;
const DECO_CHIP: u32 = 2;

/// A squiggle's stroke thickness as a fraction of the band it is given, which
/// leaves the same fraction again for the amplitude above and below the
/// centerline: `thickness + 2 * amplitude` is exactly the band.
const SQUIGGLE_THICKNESS: f32 = 1.0 / 3.0;
/// A squiggle's wavelength, in band heights. Six stroke widths per period is
/// the diagnostics-underline look.
const SQUIGGLE_WAVELENGTH: f32 = 2.0;

impl DecorationInstance {
    fn new(rect: Rect, kind: DecorationKind, color: Color) -> Self {
        let (code, params) = match kind {
            DecorationKind::Underline | DecorationKind::Strikethrough => (DECO_SOLID, [0.0; 4]),
            DecorationKind::Squiggle => {
                let thickness = rect[3] * SQUIGGLE_THICKNESS;
                let amplitude = (rect[3] - thickness) * 0.5;
                (
                    DECO_SQUIGGLE,
                    [
                        amplitude,
                        rect[3] * SQUIGGLE_WAVELENGTH,
                        thickness.max(1.0),
                        0.0,
                    ],
                )
            }
            DecorationKind::Chip { radius_px } => (DECO_CHIP, [radius_px, 0.0, 0.0, 0.0]),
        };
        Self {
            pos: [rect[0], rect[1]],
            size: [rect[2], rect[3]],
            color: color.0,
            params,
            kind: code,
            _pad: [0; 3],
        }
    }
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
//
// Chips sit between the selection underlay and the glyphs (an inline-code
// background belongs behind its text), and line decorations between the glyphs
// and the highlight overlay (an underline belongs in front of the descenders
// it crosses). Both feed the same pipeline, in two draws.
const UNDER: usize = 0;
const CHIPS: usize = 1;
const VECTOR: usize = 2;
const BLEND: usize = 3;
const ATLAS: usize = 4;
const LINE_DECOS: usize = 5;
const OVER: usize = 6;
const LAYERS: usize = 7;

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
    /// Rounded-rect chips drawn under this block's glyphs, after the
    /// under-rects: inline-code backgrounds, pills, tags. Each entry is a
    /// rect, a corner radius in px, and a color.
    pub chips: &'a [(Rect, f32, Color)],
    /// Underlines, strikethroughs and squiggles drawn over this block's
    /// glyphs. A [`DecorationKind::Chip`] listed here still draws in the chip
    /// layer, under the glyphs, where a background belongs.
    pub decorations: &'a [(Rect, DecorationKind, Color)],
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
            chips: &[],
            decorations: &[],
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
    /// Block-local px to world. A plain translation for a 2D block; anything
    /// at all for a pane in 3D.
    model: Mat4,
    /// Painter's-order sort key, low to high. Ties keep insertion order.
    z: f32,
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
            model: Mat4::IDENTITY,
            z: 0.0,
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
    chips: Vec<DecorationInstance>,
    line_decos: Vec<DecorationInstance>,
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
        self.chips.clear();
        self.line_decos.clear();
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
        counts[CHIPS] = self.chips.len() as u32;
        counts[VECTOR] = self.vector.len() as u32;
        counts[BLEND] = self.blend.len() as u32;
        counts[ATLAS] = self.atlas.len() as u32;
        counts[LINE_DECOS] = self.line_decos.len() as u32;
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
/// range, moving it (or turning it to face a camera —
/// [`TextRenderer::set_block_transform`]) re-uploads only that uniform, and a
/// frame in which nothing changed reports [`TextRenderer::damaged`] false so
/// the host can skip rendering and presenting entirely.
///
/// The immediate-mode API ([`TextRenderer::begin`], [`TextRenderer::rect`],
/// [`TextRenderer::text`], [`TextRenderer::finish`]) is a thin wrapper over one
/// internal block that is rebuilt every frame.
pub struct TextRenderer {
    rect_pipeline: wgpu::RenderPipeline,
    /// Chips and line decorations both draw with this one.
    deco_pipeline: wgpu::RenderPipeline,
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
    /// World to clip, as the host set it. `None` is the default — the screen
    /// ortho, which is the projection the shader applies anyway, so blocks
    /// upload their model matrix untouched and 2D stays exact.
    view_projection: Option<Mat4>,
    /// `math::px_from_clip(screen_size) * view_projection`, the matrix a
    /// block's model is composed with on its way into the uniform. Cached
    /// because it changes only when the projection or the surface does.
    px_from_world: Option<Mat4>,

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
    /// Block ids in draw order: by [`TextRenderer::set_block_z`], then by
    /// creation order (block ids are handed out in sequence, so sorting by id
    /// *is* the insertion-order tiebreak).
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
        // every block's uniform: a resize writes 16 bytes instead of dirtying
        // every block, and the block uniform holds nothing but the block's own
        // placement. That still holds with the mat4 in it, because the block
        // matrix lands in *pixel* space — only a host that has set its own
        // view-projection pays for a resize.
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
        let deco_stride = std::mem::size_of::<DecorationInstance>();
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
        let deco_pipeline = make_pipeline(
            "faf-text decorations",
            "deco_vs",
            "deco_fs",
            deco_stride as u64,
            &wgpu::vertex_attr_array![
                0 => Float32x2, 1 => Float32x2, 2 => Float32x4, 3 => Float32x4, 4 => Uint32
            ],
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
            deco_pipeline,
            atlas_pipeline,
            vector_pipeline,
            blend_pipeline,
            bind_group,
            bind_group_layout,
            sampler,
            globals_buffer,
            globals_dirty: true,
            screen_size: [0.0, 0.0],
            view_projection: None,
            px_from_world: None,
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
                Arena::new("faf-text chips", deco_stride),
                Arena::new("faf-text vector glyphs", vector_stride),
                Arena::new("faf-text blended glyphs", vector_stride),
                Arena::new("faf-text atlas glyphs", atlas_stride),
                Arena::new("faf-text line decorations", deco_stride),
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

    /// Create an empty block. Blocks composite in creation order, until a
    /// [`TextRenderer::set_block_z`] says otherwise.
    pub fn create_block(&mut self) -> BlockId {
        let slot = self.free_slots.pop().unwrap_or_else(|| {
            self.slot_top += 1;
            self.slot_top - 1
        });
        let id = self.next_id;
        self.next_id += 1;
        self.blocks.insert(id, Block::new(slot));
        self.order.push(id);
        // Ids ascend, so this only ever matters once someone has set a z.
        self.sort_order();
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
        for &(rect, radius_px, color) in content.chips {
            self.push_decoration(rect, DecorationKind::Chip { radius_px }, color);
        }
        for &(rect, kind, color) in content.decorations {
            self.push_decoration(rect, kind, color);
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

    /// Move a block. No instance is touched — this is one uniform write.
    ///
    /// Shorthand for [`TextRenderer::set_block_transform`] with a translation:
    /// with the default view-projection, world space *is* screen pixels.
    pub fn set_block_offset(&mut self, id: BlockId, offset: [f32; 2]) {
        self.place_block(
            id,
            Mat4::from_translation(glam::vec3(offset[0], offset[1], 0.0)),
        );
    }

    /// Place a block in 3D: `model` maps block-local px to world space, and
    /// [`TextRenderer::set_view_projection`] maps world to clip.
    ///
    /// Column-major, the layout `glam`'s `Mat4::to_cols_array_2d` produces. As
    /// with [`TextRenderer::set_block_offset`] no instance is rebuilt — a pane
    /// can be re-oriented every frame for the price of one uniform write, which
    /// is the whole point of putting the matrix here rather than in the
    /// geometry.
    ///
    /// Coverage needs nothing from this: antialiasing comes from screen-space
    /// derivatives of an interpolated varying, so it is already measured on the
    /// projected, foreshortened glyph. The atlas path is the one exception —
    /// see [`TextRenderer::set_view_projection`].
    ///
    /// A model that leaves the z = 0 plane wants a projection to go with it:
    /// the default one passes depth straight through, so a rotated pane's
    /// corners land outside the 0..1 clip range and get clipped away.
    /// [`crate::math::screen_perspective`] is the one to reach for.
    pub fn set_block_transform(&mut self, id: BlockId, model: [[f32; 4]; 4]) {
        self.place_block(id, Mat4::from_cols_array_2d(&model));
    }

    fn place_block(&mut self, id: BlockId, model: Mat4) {
        let Some(block) = self.blocks.get_mut(&id.0) else {
            return;
        };
        if block.model == model {
            return;
        }
        block.model = model;
        block.uniform_dirty = true;
        self.damaged = true;
    }

    /// Set the shared world-to-clip matrix — a camera for every block at once.
    /// [`crate::math::screen_perspective`] builds one that leaves 2D content
    /// exactly where it was.
    ///
    /// There is no depth buffer and text does not z-write, so **blocks
    /// composite in the order they are drawn**: hosts sort back to front with
    /// [`TextRenderer::set_block_z`]. Two things follow from the matrix:
    ///
    /// - A block whose placement is no longer an axis-aligned scale and
    ///   translation stops snapping its **atlas** quads (color emoji, bitmap
    ///   fallbacks) to the pixel grid, because there is no grid to snap them
    ///   to. Those glyphs go a touch soft in 3D. Vector glyphs are unaffected
    ///   at any angle.
    /// - A non-default projection makes every block's uniform depend on the
    ///   surface size, so a resize re-uploads all of them (a few dozen bytes
    ///   each). The default projection costs nothing on resize.
    pub fn set_view_projection(&mut self, vp: [[f32; 4]; 4]) {
        let vp = Mat4::from_cols_array_2d(&vp);
        if self.view_projection == Some(vp) {
            return;
        }
        self.view_projection = Some(vp);
        self.projection_changed();
    }

    /// Back to the default projection: the screen ortho, which is the one the
    /// shader applies itself. 2D content is then placed by the exact same
    /// arithmetic it was before any of this existed.
    pub fn clear_view_projection(&mut self) {
        if self.view_projection.is_none() {
            return;
        }
        self.view_projection = None;
        self.projection_changed();
    }

    /// Painter's-order sort key, low to high: a block with a lower `z` draws
    /// first and everything after composites over it. Blocks that share a `z`
    /// keep creation order, which is what every block has by default.
    ///
    /// This is a sort key, not a coordinate — it does not move the block. With
    /// no depth buffer and alpha-blended text, hosts that place blocks in 3D
    /// are expected to sort them back to front themselves (say, by the model
    /// matrix's distance from the eye) and feed the result to this.
    pub fn set_block_z(&mut self, id: BlockId, z: f32) {
        let Some(block) = self.blocks.get_mut(&id.0) else {
            return;
        };
        if block.z == z {
            return;
        }
        block.z = z;
        self.sort_order();
        self.damaged = true;
    }

    /// Re-derive the projection blocks are composed with, and dirty every
    /// block's uniform since all of them just changed.
    fn projection_changed(&mut self) {
        self.px_from_world = self
            .view_projection
            .map(|vp| math::px_from_clip(self.screen_size) * vp);
        for block in self.blocks.values_mut() {
            block.uniform_dirty = true;
        }
        self.damaged = true;
    }

    /// Draw order: z first, then creation order — block ids are handed out in
    /// sequence, so the id is the insertion index.
    fn sort_order(&mut self) {
        let blocks = &self.blocks;
        let z = |id: &u32| blocks.get(id).map_or(0.0, |block| block.z);
        self.order
            .sort_by(|a, b| z(a).total_cmp(&z(b)).then(a.cmp(b)));
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

    /// Queue a decoration. Chips draw under the frame's glyphs, the line kinds
    /// over them; [`crate::TextView::decoration_rects`] produces the geometry.
    pub fn decoration(&mut self, rect: Rect, kind: DecorationKind, color: Color) {
        self.push_decoration(rect, kind, color);
        self.transient_pending = true;
    }

    /// Queue a rounded-rect chip under the glyphs — an inline-code background
    /// or a pill. Shorthand for [`TextRenderer::decoration`] with
    /// [`DecorationKind::Chip`].
    pub fn chip(&mut self, rect: Rect, radius_px: f32, color: Color) {
        self.decoration(rect, DecorationKind::Chip { radius_px }, color);
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
            let resized = self.screen_size != screen_size;
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
            // A host projection is folded into pixel space against the surface
            // size, so it (and every block composed with it) has to be redone.
            // The default projection has no such dependency, which is why a
            // plain 2D resize still touches nothing but these 16 bytes.
            if resized && self.view_projection.is_some() {
                self.projection_changed();
            }
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

    /// Record draws into an open render pass: every visible block in draw
    /// order (z, then creation order — nothing z-writes, so this *is* the
    /// compositing order), and within a block the seven layers listed on
    /// [`TextRenderer`] — under-rects, chips, vector glyphs, weight-blended
    /// glyphs, atlas glyphs, line decorations, over-rects.
    pub fn render(&self, pass: &mut wgpu::RenderPass<'_>) {
        pass.set_bind_group(0, &self.bind_group, &[]);
        // One entry per layer, in draw order: under-rects, chips, vector
        // glyphs, weight-blended glyphs, atlas glyphs, line decorations,
        // over-rects.
        let pipelines = [
            &self.rect_pipeline,
            &self.deco_pipeline,
            &self.vector_pipeline,
            &self.blend_pipeline,
            &self.atlas_pipeline,
            &self.deco_pipeline,
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

    fn push_decoration(&mut self, rect: Rect, kind: DecorationKind, color: Color) {
        let inst = DecorationInstance::new(rect, kind, color);
        if kind.is_chip() {
            self.scratch.chips.push(inst);
        } else {
            self.scratch.line_decos.push(inst);
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
            self.push_run_decorations(&run, pos, default_color);
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

    /// Emit the solid lines the *attributes* asked for. cosmic-text 0.19 shapes
    /// `Attrs::underline`/`strikethrough`/`overline` into per-run
    /// [`DecorationSpan`]s carrying the face's own offset and thickness in em,
    /// so styled text underlines itself with no help from the caller — the
    /// manual [`TextRenderer::decoration`] path is for what attrs cannot say
    /// (squiggles, chips, spans that are not attribute runs).
    fn push_run_decorations(&mut self, run: &LayoutRun<'_>, pos: [f32; 2], default_color: Color) {
        for span in run.decorations {
            let Some(glyphs) = run.glyphs.get(span.glyph_range.clone()) else {
                continue;
            };
            // Glyph x is already visual, so the extent is right under BiDi.
            let (mut x0, mut x1) = (f32::INFINITY, f32::NEG_INFINITY);
            for glyph in glyphs {
                x0 = x0.min(glyph.x);
                x1 = x1.max(glyph.x + glyph.w);
            }
            if x1 <= x0 {
                continue;
            }
            let (x, w) = (pos[0] + x0, x1 - x0);
            let baseline_y = pos[1] + run.line_y;
            let size = span.font_size;
            let deco = &span.data;
            let color = |explicit: Option<cosmic_text::Color>| {
                explicit
                    .or(span.color_opt)
                    .map(Color::from_cosmic)
                    .unwrap_or(default_color)
            };
            // Offsets are em, y-up from the baseline; thickness is em too, and
            // never rounds away to nothing.
            let bar = |metrics: &cosmic_text::DecorationMetrics| {
                let thickness = (metrics.thickness * size).max(1.0);
                (baseline_y - metrics.offset * size, thickness)
            };

            let underlines = match deco.text_decoration.underline {
                UnderlineStyle::None => 0,
                UnderlineStyle::Single => 1,
                UnderlineStyle::Double => 2,
            };
            if underlines > 0 {
                let (y, thickness) = bar(&deco.underline_metrics);
                let color = color(deco.text_decoration.underline_color_opt);
                for i in 0..underlines {
                    // The gap between a double underline's two bars is one
                    // bar, which is what cosmic-text's own renderer draws.
                    let y = y + i as f32 * thickness * 2.0;
                    self.push_solid_line([x, y, w, thickness], color);
                }
            }
            if deco.text_decoration.strikethrough {
                let (y, thickness) = bar(&deco.strikethrough_metrics);
                self.push_solid_line(
                    [x, y, w, thickness],
                    color(deco.text_decoration.strikethrough_color_opt),
                );
            }
            if deco.text_decoration.overline {
                let thickness = (deco.underline_metrics.thickness * size).max(1.0);
                // Clamped into the line box, like cosmic-text's renderer: a
                // tall ascent would otherwise overline the row above.
                let y = (baseline_y - deco.ascent * size).max(pos[1] + run.line_top);
                self.push_solid_line(
                    [x, y, w, thickness],
                    color(deco.text_decoration.overline_color_opt),
                );
            }
        }
    }

    /// A solid bar in the line-decoration layer. Underline, strikeout and
    /// overline differ only in where the rect is.
    fn push_solid_line(&mut self, rect: Rect, color: Color) {
        self.scratch.line_decos.push(DecorationInstance::new(
            rect,
            DecorationKind::Underline,
            color,
        ));
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
            arenas[CHIPS].write(block.spans[CHIPS], &scratch.chips);
            arenas[VECTOR].write(block.spans[VECTOR], &scratch.vector);
            arenas[BLEND].write(block.spans[BLEND], &scratch.blend);
            arenas[ATLAS].write(block.spans[ATLAS], &scratch.atlas);
            arenas[LINE_DECOS].write(block.spans[LINE_DECOS], &scratch.line_decos);
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
            let uniform = block_uniform(self.px_from_world, block.model);
            queue.write_buffer(
                &self.block_uniforms,
                (block.slot * self.uniform_stride) as u64,
                bytemuck::bytes_of(&uniform),
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

/// A block's uniform: its model matrix carried into homogeneous pixel space,
/// plus what the vertex shader needs to know about the result.
///
/// `px_from_world` is `None` for the default projection, and then the model
/// matrix goes in untouched rather than through a multiply by something that
/// is only *numerically* the identity — which is what keeps a 2D scene
/// bit-for-bit identical to the pre-matrix renderer.
fn block_uniform(px_from_world: Option<Mat4>, model: Mat4) -> BlockUniform {
    let m = match px_from_world {
        Some(px_from_world) => px_from_world * model,
        None => model,
    };
    BlockUniform {
        transform: m.to_cols_array_2d(),
        flags: if is_axis_aligned(m) { BLOCK_SNAP } else { 0 },
        _pad: [0; 3],
    }
}

/// Whether a placement is an axis-aligned scale plus translation, i.e. whether
/// block-local x and y still land on screen x and y with no rotation, shear or
/// perspective. Only the columns a z = 0 point touches matter: `m * (x, y, 0,
/// 1)` is `col0 * x + col1 * y + col3`.
fn is_axis_aligned(m: Mat4) -> bool {
    let (x, y, t) = (m.x_axis, m.y_axis, m.w_axis);
    x.y.abs() <= AXIS_EPSILON
        && y.x.abs() <= AXIS_EPSILON
        && x.w.abs() <= AXIS_EPSILON
        && y.w.abs() <= AXIS_EPSILON
        && (t.w - 1.0).abs() <= AXIS_EPSILON
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
    use crate::{Attrs, Cursor, Family, Metrics, TextView, UnderlineStyle};

    const RED: Color = Color::rgba(1.0, 0.0, 0.0, 1.0);

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

    // ---- Placement ----

    /// Turn `model` into the matrix the shader would receive, under the
    /// default projection.
    fn placed(model: Mat4) -> BlockUniform {
        block_uniform(None, model)
    }

    #[test]
    fn a_2d_placement_is_a_translation_and_nothing_else() {
        // The whole reason the block matrix lands in pixel space rather than
        // clip space: an offset block's transform is *exactly* a translation,
        // so the vertex shader adds and never scales, and 2D content renders
        // bit-for-bit as it did before the matrix existed.
        let moved = placed(Mat4::from_translation(glam::vec3(30.0, -12.5, 0.0)));
        assert_eq!(
            moved.transform,
            [
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [30.0, -12.5, 0.0, 1.0],
            ]
        );
        assert_eq!(moved.flags, BLOCK_SNAP, "a moved block still snaps");
        assert_eq!(placed(Mat4::IDENTITY).flags, BLOCK_SNAP);
    }

    #[test]
    fn a_host_projection_composes_into_pixel_space() {
        let size = [W as f32, H as f32];
        // A host that hands over the very projection the renderer applies
        // itself must get the identity composition back — otherwise every
        // "3D" scene would pay a scale it did not ask for.
        let px_from_world = Some(math::px_from_clip(size) * math::screen_ortho(size));
        let model = Mat4::from_translation(glam::vec3(12.0, 34.0, 0.0));
        let composed = block_uniform(px_from_world, model);
        for (a, b) in composed
            .transform
            .iter()
            .flatten()
            .zip(placed(model).transform.iter().flatten())
        {
            assert!((a - b).abs() < 1e-4, "{composed:?} should be the model");
        }

        // A real camera keeps the block's own placement and adds perspective:
        // the corner of a block at z = 0 still lands on its 2D pixel.
        let perspective = Some(math::px_from_clip(size) * math::screen_perspective(size, 0.7));
        let m = Mat4::from_cols_array_2d(&block_uniform(perspective, model).transform);
        let corner = m * glam::vec4(60.0, 20.0, 0.0, 1.0);
        assert!(
            (corner.x / corner.w - 72.0).abs() < 1e-2 && (corner.y / corner.w - 54.0).abs() < 1e-2,
            "{corner} should be the 2D pixel (72, 54)"
        );
    }

    #[test]
    fn the_snap_flag_follows_the_placement() {
        // Snapping means "there is a pixel grid to snap to": scale and
        // translation keep one, rotation and perspective do not.
        let axis_aligned = [
            Mat4::IDENTITY,
            Mat4::from_translation(glam::vec3(4.0, 9.0, 0.0)),
            Mat4::from_scale(glam::vec3(2.0, 3.0, 1.0)),
            Mat4::from_scale(glam::vec3(-1.0, 1.0, 1.0)), // mirrored, still on grid
        ];
        for model in axis_aligned {
            assert_eq!(placed(model).flags, BLOCK_SNAP, "{model} is axis aligned");
        }
        // A turn in the screen plane leaves no grid at all.
        assert_eq!(placed(Mat4::from_rotation_z(0.01)).flags, 0);
        // A turn *out* of it does, as long as nothing projects it: with no
        // camera, rotating about y is only a horizontal squash. (It also
        // pushes the block's z out of the clip range, which is why a block
        // placed in 3D wants a view-projection — see `set_block_transform`.)
        assert_eq!(placed(Mat4::from_rotation_y(0.4)).flags, BLOCK_SNAP);
        let size = [W as f32, H as f32];
        let perspective = Some(math::px_from_clip(size) * math::screen_perspective(size, 0.7));
        assert_eq!(
            block_uniform(perspective, Mat4::from_rotation_y(0.4)).flags,
            0,
            "a perspective divisor is not a pixel grid"
        );
    }

    #[test]
    fn a_transform_and_an_offset_place_a_block_the_same_way() {
        let mut font_system = testing::font_system();
        let sample = view(&mut font_system, "placed", 24.0, [6.0, 6.0]);
        let offset = [37.0, 61.5];

        let mut moved = renderer(2048, 2048);
        let block = moved.create_block();
        moved.begin_frame();
        set_text_block(&mut moved, &mut font_system, block, &sample);
        moved.set_block_offset(block, offset);
        let by_offset = retained_frame(&mut moved);
        assert!(drew(&by_offset));

        let mut transformed = renderer(2048, 2048);
        let block = transformed.create_block();
        transformed.begin_frame();
        set_text_block(&mut transformed, &mut font_system, block, &sample);
        transformed.set_block_transform(
            block,
            Mat4::from_translation(glam::vec3(offset[0], offset[1], 0.0)).to_cols_array_2d(),
        );
        assert_eq!(by_offset, retained_frame(&mut transformed));
    }

    #[test]
    fn a_pane_turned_away_from_the_camera_still_draws_its_text() {
        let mut font_system = testing::font_system();
        let sample = view(&mut font_system, "oblique", 22.0, [10.0, 90.0]);
        let mut renderer = renderer(2048, 2048);
        let block = renderer.create_block();
        renderer.begin_frame();
        set_text_block(&mut renderer, &mut font_system, block, &sample);
        let flat = retained_frame(&mut renderer);

        let size = [W as f32, H as f32];
        let center = glam::vec3(size[0] * 0.5, size[1] * 0.5, 0.0);
        renderer.begin_frame();
        renderer.set_view_projection(math::screen_perspective(size, 0.7).to_cols_array_2d());
        renderer.set_block_transform(
            block,
            (Mat4::from_translation(center)
                * Mat4::from_rotation_y(50f32.to_radians())
                * Mat4::from_translation(-center))
            .to_cols_array_2d(),
        );
        let tilted = retained_frame(&mut renderer);
        assert!(drew(&tilted), "a tilted pane still draws");
        assert_ne!(tilted, flat, "and it does not draw the flat pane");
        assert_eq!(
            renderer.upload_stats().content_writes,
            0,
            "turning a pane in 3D is a uniform write, not a rebuild"
        );

        // Back to the default projection and placement: the same pixels as
        // before anything 3D happened.
        renderer.begin_frame();
        renderer.clear_view_projection();
        renderer.set_block_offset(block, [0.0, 0.0]);
        assert_eq!(retained_frame(&mut renderer), flat);
    }

    #[test]
    fn the_z_key_decides_which_block_composites_on_top() {
        let mut renderer = renderer(2048, 2048);
        let over_the_same_pixels = [20.0, 20.0, 80.0, 40.0];
        let first = renderer.create_block();
        let second = renderer.create_block();
        renderer.begin_frame();
        set_rect_block(&mut renderer, first, &[(over_the_same_pixels, RED)]);
        set_rect_block(
            &mut renderer,
            second,
            &[(over_the_same_pixels, Color::WHITE)],
        );

        // Creation order is the default z-order: the later block wins.
        let px = retained_frame(&mut renderer);
        assert_eq!(at(&px, 60, 40), 255, "white over red");
        assert_eq!(px[((40 * W + 60) * 4 + 1) as usize], 255);

        // Sorting the first block in front flips that, and nothing re-uploads.
        renderer.begin_frame();
        renderer.set_block_z(first, 1.0);
        let px = retained_frame(&mut renderer);
        assert_eq!(px[((40 * W + 60) * 4 + 1) as usize], 0, "red over white");
        assert_eq!(renderer.upload_stats(), UploadStats::default());

        // Equal keys fall back to creation order, and the transient block
        // (created first, so id 0) stays underneath everything.
        renderer.begin_frame();
        renderer.set_block_z(first, 0.0);
        retained_frame(&mut renderer);
        assert_eq!(
            renderer.order,
            vec![renderer.transient.0, first.0, second.0]
        );
    }

    // ---- Decorations ----

    /// Red channel of one pixel.
    fn at(pixels: &[u8], x: u32, y: u32) -> u8 {
        pixels[((y * W + x) * 4) as usize]
    }

    /// Pixels that came out fully lit — glyph interiors, and nothing that has
    /// been blended over.
    fn pure_white(pixels: &[u8]) -> usize {
        pixels
            .chunks_exact(4)
            .filter(|px| px[..3] == [255, 255, 255])
            .count()
    }

    #[test]
    fn decoration_instances_encode_their_shape_parameters() {
        let mut renderer = renderer(2048, 2048);
        renderer.begin();
        renderer.chip([10.0, 20.0, 60.0, 24.0], 6.0, Color::WHITE);
        renderer.decoration(
            [10.0, 50.0, 60.0, 2.0],
            DecorationKind::Underline,
            Color::WHITE,
        );
        renderer.decoration([10.0, 60.0, 60.0, 9.0], DecorationKind::Squiggle, RED);

        assert_eq!(renderer.scratch.chips.len(), 1, "a chip is not a line");
        assert_eq!(renderer.scratch.line_decos.len(), 2);

        let chip = renderer.scratch.chips[0];
        assert_eq!(chip.kind, DECO_CHIP);
        assert_eq!([chip.pos, chip.size], [[10.0, 20.0], [60.0, 24.0]]);
        assert_eq!(chip.params, [6.0, 0.0, 0.0, 0.0], "corner radius, px");

        let underline = renderer.scratch.line_decos[0];
        assert_eq!(underline.kind, DECO_SOLID);
        assert_eq!(underline.params, [0.0; 4], "a bar is just its rect");

        // A 9 px band is a 3 px stroke swinging 3 px either side of the
        // centerline, over an 18 px period.
        let squiggle = renderer.scratch.line_decos[1];
        assert_eq!(squiggle.kind, DECO_SQUIGGLE);
        assert_eq!(squiggle.params, [3.0, 18.0, 3.0, 0.0]);
        assert_eq!(squiggle.color, RED.0);
    }

    #[test]
    fn chips_draw_under_the_glyphs_and_line_decorations_over_them() {
        let mut font_system = testing::font_system();
        let sample = view(&mut font_system, "Ag", 40.0, [6.0, 6.0]);
        let mut renderer = renderer(2048, 2048);
        let (_, queue) = testing::gpu();
        // Comfortably around the glyphs.
        let over_the_text = [0.0, 0.0, 120.0, 60.0];

        let mut draw = |chip: bool, line: bool, renderer: &mut TextRenderer| {
            renderer.begin();
            if chip {
                renderer.chip(over_the_text, 0.0, RED);
            }
            renderer.text(
                queue,
                &mut font_system,
                &sample.buffer,
                sample.pos,
                Color::WHITE,
            );
            if line {
                renderer.decoration(over_the_text, DecorationKind::Underline, RED);
            }
            testing::render_pixels(renderer, W, H)
        };

        let text_only = draw(false, false, &mut renderer);
        let chipped = draw(true, false, &mut renderer);
        let covered = draw(false, true, &mut renderer);

        assert!(pure_white(&text_only) > 0, "the glyphs drew something");
        assert_eq!(
            pure_white(&chipped),
            pure_white(&text_only),
            "a chip goes behind the glyphs, so their interiors stay white"
        );
        assert!(
            at(&chipped, 118, 58) > 128,
            "and the chip itself covers the rest of its rect"
        );
        assert_eq!(
            pure_white(&covered),
            0,
            "an opaque line decoration goes in front of the glyphs"
        );
    }

    #[test]
    fn a_chip_rounds_its_corners_and_a_squiggle_waves() {
        let mut renderer = renderer(2048, 2048);
        renderer.begin();
        // 100x60 at (20, 20), corners rounded by a third of the height.
        renderer.chip([20.0, 20.0, 100.0, 60.0], 20.0, Color::WHITE);
        // A 12 px band at (20, 100): a 4 px stroke, 4 px of swing, 24 px period.
        renderer.decoration([20.0, 100.0, 120.0, 12.0], DecorationKind::Squiggle, RED);
        let px = testing::render_pixels(&mut renderer, W, H);

        assert_eq!(at(&px, 70, 50), 255, "the middle of a chip is solid");
        assert_eq!(at(&px, 70, 21), 255, "so is its edge between the corners");
        assert_eq!(at(&px, 22, 50), 255, "and its side");
        for (x, y) in [(21, 21), (118, 21), (21, 78), (118, 78)] {
            assert_eq!(at(&px, x, y), 0, "corner ({x}, {y}) should be rounded off");
        }

        // A quarter period in, the wave is at the bottom of its band; three
        // quarters in, the top. A solid bar would light both rows at both x.
        assert!(at(&px, 26, 110) > 128 && at(&px, 26, 102) < 128, "crest");
        assert!(at(&px, 38, 102) > 128 && at(&px, 38, 110) < 128, "trough");
    }

    #[test]
    fn attribute_decorations_shape_themselves() {
        let mut font_system = testing::font_system();
        let plain = Attrs::new().family(Family::SansSerif);
        let mut sample = TextView::new(&mut font_system, Metrics::new(40.0, 50.0));
        sample.pos = [6.0, 6.0];
        sample.set_rich_text(
            &mut font_system,
            [
                ("plain ", plain.clone()),
                ("under", plain.clone().underline(UnderlineStyle::Single)),
                (" struck", plain.clone().strikethrough()),
            ],
            &plain,
        );

        let mut renderer = renderer(2048, 2048);
        let (_, queue) = testing::gpu();
        renderer.begin();
        renderer.text(
            queue,
            &mut font_system,
            &sample.buffer,
            sample.pos,
            Color::WHITE,
        );

        assert_eq!(
            renderer.scratch.line_decos.len(),
            2,
            "one bar per decorated span, none for the plain one"
        );
        let (underline, strike) = (
            renderer.scratch.line_decos[0],
            renderer.scratch.line_decos[1],
        );
        let baseline = sample.pos[1] + sample.buffer.layout_runs().next().unwrap().line_y;
        assert!(
            underline.pos[1] > baseline,
            "underlines go below the baseline"
        );
        assert!(strike.pos[1] < baseline, "strikeouts cross the glyphs");
        assert!(underline.pos[0] < strike.pos[0], "spans keep their order");
        assert!(
            underline.pos[0] > sample.pos[0],
            "the plain span is not underlined"
        );

        // The attribute path (cosmic-text's shaped metrics) and the manual one
        // (this crate's swash cache) must land the bar in the same place.
        let manual = sample.decoration_rects(
            Cursor::new(0, 6),
            Cursor::new(0, 11),
            DecorationKind::Underline,
        );
        assert_eq!(manual.len(), 1);
        assert!(
            (manual[0][1] - underline.pos[1]).abs() < 0.01
                && (manual[0][0] - underline.pos[0]).abs() < 0.01
                && (manual[0][2] - underline.size[0]).abs() < 0.01,
            "manual {manual:?} vs attribute {underline:?}"
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
