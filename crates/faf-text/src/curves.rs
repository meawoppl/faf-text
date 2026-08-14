use cosmic_text::{CacheKey, CacheKeyFlags, Command, FontSystem, SwashCache, fontdb};
use rustc_hash::FxHashMap;

/// Width in texels of the RGBA32F curve data texture. Must be even so a
/// curve's two texels never straddle a row, and must match `CURVE_TEX_WIDTH`
/// in `shaders.wgsl`.
pub const CURVE_TEX_WIDTH: u32 = 256;
/// Height the curve texture starts at; it doubles on overflow up to
/// [`CURVE_TEX_MAX_HEIGHT`] (or the device's limit, whichever is smaller).
pub const CURVE_TEX_HEIGHT: u32 = 2048;
/// Ceiling on curve-texture height. 256 × 8192 RGBA32F is 32 MiB and holds
/// ~1M quadratics; past that we evict instead of growing.
pub const CURVE_TEX_MAX_HEIGHT: u32 = 8192;

// --- Record layout. Everything that knows how a curve maps to texels lives
// here, so band tables and master pairs can change it in one place.
//
// A glyph owns one contiguous, even-texel-aligned block of the texture;
// `GlyphCurves::first` is the block's base texel and every offset stored inside
// it is relative to that base, so compaction relocates a glyph by moving its
// texels and rewriting `first` alone.
//
// Flat block (`count` <= `BAND_MIN_CURVES`):
//
//     [curve record 0][curve record 1]…            2 texels each
//
// Banded block (`count` > `BAND_MIN_CURVES`):
//
//     [header]              BANDS texels: BANDS y-band then BANDS x-band
//                           entries, each (list offset in texels from the
//                           block base, curve count) — 2 entries per texel
//     [index lists]         the 2*BANDS lists in header order, each starting
//                           on a texel boundary, region padded to even
//     [curve record 0]…     2 texels each
//
// A list entry is the texel offset of that curve's record from the block base
// (float, exact to 2^24 — a glyph would need 8M texels to lose precision), so
// the shader resolves a curve without knowing where the record region starts.
// The two shapes are told apart by [`BANDED_FLAG`] in the instance's `count`.
//
// A glyph from a variable font with a `wght` axis stores a second master: the
// same records extracted at the axis maximum, in a parallel region right after
// master A's, in the same order. [`GlyphCurves::b_offset`] is the texel
// distance from any A record to its B twin (`count * TEXELS_PER_CURVE`), so
// band lists keep indexing A alone and the shader reaches B by adding it.

/// Floats per RGBA32F texel.
const FLOATS_PER_TEXEL: usize = 4;
/// Two RGBA32F texels (8 floats) per quadratic: [p0.xy p1.xy] [p2.xy pad pad].
const TEXELS_PER_CURVE: usize = 2;
const FLOATS_PER_CURVE: usize = TEXELS_PER_CURVE * FLOATS_PER_TEXEL;

/// Bands per axis. Must match `BANDS` in `shaders.wgsl`.
pub const BANDS: usize = 8;
/// Texels the band header occupies: 2*BANDS entries × 2 floats, 4 floats per
/// texel.
const HEADER_TEXELS: usize = BANDS * 2 * 2 / FLOATS_PER_TEXEL;
/// Glyphs with more curves than this get band tables; at or below it the flat
/// loop is cheaper than the indirection.
const BAND_MIN_CURVES: u32 = 16;
/// A band's interval is widened by this fraction of a band's height before
/// curve overlap is tested. Control-point ranges already bound the curve, so
/// the test is conservative; the slack only has to cover fp disagreement
/// between these boundaries and the shader's interpolated band coordinate.
const BAND_EPSILON: f32 = 0.05;
/// Set in the vector instance's `count` field when the glyph's block is
/// banded. Must match `BANDED_FLAG` in `shaders.wgsl`.
pub const BANDED_FLAG: u32 = 0x8000_0000;
/// The variation axis a glyph's two masters are extracted at the ends of.
const WGHT: swash::Tag = u32::from_be_bytes(*b"wght");

/// Floats in one texture row.
const fn floats_per_row(width: u32) -> usize {
    width as usize * FLOATS_PER_TEXEL
}

/// How many texels a `width × height` texture holds.
const fn texel_capacity(width: u32, height: u32) -> usize {
    width as usize * height as usize
}

/// A glyph's quadratic Bézier set inside the curve texture, in em units
/// (y-up, origin at the glyph's baseline origin).
#[derive(Clone, Copy, Debug)]
pub struct GlyphCurves {
    /// Base texel of the glyph's block.
    pub first: u32,
    /// Quadratics per master in the block. Never carries [`BANDED_FLAG`] — the
    /// shader wants [`GlyphCurves::instance_count`].
    pub count: u32,
    /// Em-space bounds: [min_x, min_y, max_x, max_y], the union over both
    /// masters so the quad covers the whole animation range. Also the extent
    /// the bands split, so the renderer must map quad-local coordinates into
    /// it.
    pub bbox: [f32; 4],
    /// Whether the block starts with band tables.
    pub banded: bool,
    /// Texels from a master-A record to its master-B twin, or 0 when the glyph
    /// has a single master (a static face, or masters that do not interpolate).
    pub b_offset: u32,
    /// Blend factor matching the weight the text was shaped at: 0 = axis
    /// minimum, 1 = axis maximum. Meaningless when `b_offset` is 0.
    pub weight_t: f32,
    /// Length of the whole block in texels — what compaction moves.
    texels: u32,
}

impl GlyphCurves {
    /// The `count` field for the vector instance: the curve count, tagged with
    /// [`BANDED_FLAG`] when the shader should read band tables first.
    pub fn instance_count(&self) -> u32 {
        if self.banded {
            self.count | BANDED_FLAG
        } else {
            self.count
        }
    }

    /// Base texel of the master-B records, or 0 for a single-master glyph —
    /// the `b_first` the vector instance carries. A block always begins with
    /// master A, so a dual-master glyph can never land on 0.
    pub fn b_first(&self) -> u32 {
        if self.b_offset == 0 {
            0
        } else {
            self.first + self.b_offset
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlyphKey {
    font_id: fontdb::ID,
    glyph_id: u16,
    weight: u16,
    flags: CacheKeyFlags,
}

impl GlyphKey {
    pub fn new(
        font_id: fontdb::ID,
        glyph_id: u16,
        weight: fontdb::Weight,
        flags: CacheKeyFlags,
    ) -> Self {
        Self {
            font_id,
            glyph_id,
            weight: weight.0,
            flags,
        }
    }
}

/// A cached extraction: the curve range (or `None` for an outline-less glyph)
/// plus the frame it was last drawn on, which drives LRU compaction.
struct Entry {
    curves: Option<GlyphCurves>,
    last_used: u64,
}

/// Outcome of an extraction attempt.
enum Extracted {
    /// Settled: either a curve range or a definitive "this glyph has no
    /// outline". Safe to cache.
    Done(Option<GlyphCurves>),
    /// The store is at its cap and out of room. Nothing is cached — the glyph
    /// falls back to the bitmap atlas for this frame and is retried after the
    /// next compaction.
    Overflow,
}

/// Glyph outlines flattened to quadratic Béziers and packed into a data
/// texture the fragment shader ray-casts against. Curves are stored once per
/// (font, glyph, weight) in em units and reused at every size — scaling is
/// free and needs no re-rasterization.
///
/// The texture grows (height doubling, full re-upload from the CPU mirror)
/// until it hits its cap, then evicts the least recently used half of its
/// entries. Growth and eviction both replace or rewrite the texture, so both
/// bump [`CurveStore::generation`]; the renderer rebuilds its bind group when
/// that changes. Eviction never happens mid-frame — queued instances hold raw
/// `first` indices — it is deferred to the next `begin_frame`.
pub struct CurveStore {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    /// Bumped whenever `texture`/`view` are replaced or the packing is
    /// rewritten. Renderers compare it to decide on a bind-group rebuild.
    pub generation: u64,
    /// Bumped only when compaction *moves* glyphs, which is the event that
    /// invalidates a base texel a retained instance is holding. Growth
    /// re-uploads the same packing to a taller texture and leaves every `first`
    /// alone, so it deliberately does not bump this.
    pub layout_generation: u64,
    /// Old base texel → new base texel, for the glyphs that survived the last
    /// compaction. Retained blocks patch their instances through this; a base
    /// that is missing belonged to an evicted glyph.
    pub relocations: FxHashMap<u32, u32>,
    device: wgpu::Device,
    /// CPU mirror of the packed curve floats; the un-uploaded tail is flushed
    /// to the texture by `flush`.
    data: Vec<f32>,
    entries: FxHashMap<GlyphKey, Entry>,
    swash: SwashCache,
    uploaded_rows: u32,
    width: u32,
    height: u32,
    max_height: u32,
    /// Curve count past which a glyph gets band tables. Tests raise it to
    /// render the same glyph both ways.
    pub(crate) band_min_curves: u32,
    frame: u64,
    /// An allocation failed at cap this frame; compact at the next frame edge.
    needs_compact: bool,
}

impl CurveStore {
    pub fn new(device: &wgpu::Device) -> Self {
        Self::with_size(
            device,
            CURVE_TEX_WIDTH,
            CURVE_TEX_HEIGHT,
            CURVE_TEX_MAX_HEIGHT,
        )
    }

    /// Same as [`CurveStore::new`] with explicit dimensions (tests use tiny
    /// textures to exercise growth and eviction). `max_height` is clamped to
    /// what the device supports.
    pub(crate) fn with_size(
        device: &wgpu::Device,
        width: u32,
        height: u32,
        max_height: u32,
    ) -> Self {
        let limit = device.limits().max_texture_dimension_2d;
        let max_height = max_height.min(limit).max(height);
        let texture = create_texture(device, width, height);
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            texture,
            view,
            generation: 0,
            layout_generation: 0,
            relocations: FxHashMap::default(),
            device: device.clone(),
            data: Vec::new(),
            entries: FxHashMap::default(),
            swash: SwashCache::new(),
            uploaded_rows: 0,
            width,
            height,
            max_height,
            band_min_curves: BAND_MIN_CURVES,
            frame: 0,
            needs_compact: false,
        }
    }

    /// Start a frame: stamp new lookups with `frame` and run any eviction that
    /// last frame deferred.
    pub fn begin_frame(&mut self, frame: u64) {
        self.frame = frame;
        if self.needs_compact {
            self.compact();
        }
    }

    /// Fetch (extracting on first use) the curve range for a glyph.
    /// `None` means no outline exists (e.g. a bitmap emoji) — callers should
    /// fall back to the bitmap atlas. `Some` with `count == 0` is a blank
    /// glyph (whitespace): nothing to draw, nothing to fall back to.
    pub fn get_or_insert(
        &mut self,
        font_system: &mut FontSystem,
        font_id: fontdb::ID,
        glyph_id: u16,
        weight: fontdb::Weight,
        flags: CacheKeyFlags,
    ) -> Option<GlyphCurves> {
        let key = GlyphKey {
            font_id,
            glyph_id,
            weight: weight.0,
            flags,
        };
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.last_used = self.frame;
            return entry.curves;
        }
        match self.extract(font_system, font_id, glyph_id, weight, flags) {
            Extracted::Done(curves) => {
                self.entries.insert(
                    key,
                    Entry {
                        curves,
                        last_used: self.frame,
                    },
                );
                curves
            }
            Extracted::Overflow => {
                // Not cached: after compaction this glyph gets another chance.
                self.needs_compact = true;
                None
            }
        }
    }

    /// Stamp a glyph as used on `frame` without looking anything up.
    ///
    /// Retained blocks bake `first` into their instances once and never call
    /// [`CurveStore::get_or_insert`] again, so to the LRU their glyphs would
    /// look colder every frame. The renderer touches the keys its live blocks
    /// reference with the frame about to start — before [`CurveStore::
    /// begin_frame`] runs the deferred compaction — so retained content is
    /// never among the coldest half that compaction drops.
    pub fn touch(&mut self, key: &GlyphKey, frame: u64) {
        if let Some(entry) = self.entries.get_mut(key) {
            entry.last_used = frame;
        }
    }

    /// True once an allocation has failed at the cap: glyphs are falling back
    /// to the bitmap atlas until the next frame edge compacts the store.
    pub fn overflowed(&self) -> bool {
        self.needs_compact
    }

    /// A glyph's outline in em units at one point on the `wght` axis. Size 1.0
    /// with hinting off yields pure em coordinates.
    fn outline(
        &mut self,
        font_system: &mut FontSystem,
        font_id: fontdb::ID,
        glyph_id: u16,
        weight: fontdb::Weight,
        flags: CacheKeyFlags,
    ) -> Option<Box<[Command]>> {
        let (cache_key, _, _) = CacheKey::new(
            font_id,
            glyph_id,
            1.0,
            (0.0, 0.0),
            weight,
            flags | CacheKeyFlags::DISABLE_HINTING,
        );
        self.swash
            .get_outline_commands_uncached(font_system, cache_key)
    }

    fn extract(
        &mut self,
        font_system: &mut FontSystem,
        font_id: fontdb::ID,
        glyph_id: u16,
        weight: fontdb::Weight,
        flags: CacheKeyFlags,
    ) -> Extracted {
        // A variable face is extracted twice — at both ends of its wght axis —
        // and the fragment shader lerps between the two control-point sets.
        let axis = wght_axis(font_system, font_id, weight);
        let (a_weight, b_weight) = match axis {
            Some((min, max)) => (axis_weight(min), axis_weight(max)),
            None => (weight, weight),
        };
        let Some(a_commands) = self.outline(font_system, font_id, glyph_id, a_weight, flags) else {
            return Extracted::Done(None);
        };
        // Master interpolation is per control point, so the two outlines must
        // be point-compatible. Variable-font construction guarantees it; a font
        // that breaks the promise renders from one master instead of garbage.
        let b_commands = match axis {
            Some(_) => {
                let b = self
                    .outline(font_system, font_id, glyph_id, b_weight, flags)
                    .filter(|b| masters_compatible(&a_commands, b));
                if b.is_none() {
                    warn_incompatible_masters(font_id);
                }
                b
            }
            None => None,
        };

        // Flattened into scratch buffers first: the band tables go in front of
        // the records, and the curve count decides whether there are any.
        let (records, mut bbox) = flatten(&a_commands);
        let b_records = b_commands.map(|commands| {
            let (b_records, b_bbox) = flatten(&commands);
            // The quad has to cover every weight the glyph can be drawn at.
            for i in 0..2 {
                bbox[i] = bbox[i].min(b_bbox[i]);
                bbox[2 + i] = bbox[2 + i].max(b_bbox[2 + i]);
            }
            b_records
        });
        // Same commands in, same records out — this only guards the layout
        // invariant the shader's fixed A→B stride depends on.
        let b_records = b_records.filter(|b| b.len() == records.len());

        let count = (records.len() / FLOATS_PER_CURVE) as u32;
        if count == 0 {
            return Extracted::Done(Some(GlyphCurves {
                first: 0,
                count: 0,
                bbox: [0.0; 4],
                banded: false,
                b_offset: 0,
                weight_t: 0.0,
                texels: 0,
            }));
        }

        let (b_offset, weight_t) = match (&b_records, axis) {
            (Some(_), Some((min, max))) => (
                count * TEXELS_PER_CURVE as u32,
                weight_blend(weight, min, max),
            ),
            _ => (0, 0.0),
        };

        let banded = count > self.band_min_curves;
        let block = if banded {
            banded_block(&records, b_records.as_deref(), bbox)
        } else {
            let mut block = records;
            if let Some(b) = &b_records {
                block.extend_from_slice(b);
            }
            block
        };
        debug_assert_eq!(block.len() % FLOATS_PER_CURVE, 0, "blocks are even texels");

        // Every block is an even number of texels long, so this base is even
        // and no curve record ever straddles a texture row.
        let first = (self.data.len() / FLOATS_PER_TEXEL) as u32;
        let texels = (block.len() / FLOATS_PER_TEXEL) as u32;
        self.data.extend_from_slice(&block);
        let used = self.data.len() / FLOATS_PER_TEXEL;
        if used > self.capacity() && !self.grow_to_fit(used) {
            self.data.truncate(first as usize * FLOATS_PER_TEXEL);
            return Extracted::Overflow;
        }
        Extracted::Done(Some(GlyphCurves {
            first,
            count,
            bbox,
            banded,
            b_offset,
            weight_t,
            texels,
        }))
    }

    fn capacity(&self) -> usize {
        texel_capacity(self.width, self.height)
    }

    /// Double the texture height (repeatedly, up to the cap) until `needed`
    /// texels fit, then re-create it and schedule a full re-upload from the
    /// CPU mirror. Returns false — leaving the texture untouched — if the cap
    /// is too small.
    fn grow_to_fit(&mut self, needed: usize) -> bool {
        let mut height = self.height;
        while height < self.max_height && texel_capacity(self.width, height) < needed {
            height = (height * 2).min(self.max_height);
        }
        if texel_capacity(self.width, height) < needed {
            return false;
        }
        self.height = height;
        self.texture = create_texture(&self.device, self.width, height);
        self.view = self
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        // Nothing of the old texture survives; the mirror is re-uploaded whole.
        self.uploaded_rows = 0;
        self.generation += 1;
        true
    }

    /// Drop the least recently used half of the entries and repack the CPU
    /// mirror densely, rewriting `first` in the survivors. Only ever called at
    /// a frame edge, so no queued instance can reference what we move.
    ///
    /// Retained instances *do* outlive a frame edge, so every move is recorded
    /// in [`CurveStore::relocations`] for the renderer to patch them through.
    fn compact(&mut self) {
        self.needs_compact = false;
        self.relocations.clear();
        let mut order: Vec<(u64, GlyphKey)> = self
            .entries
            .iter()
            .map(|(k, e)| (e.last_used, *k))
            .collect();
        order.sort_unstable_by_key(|(last_used, _)| *last_used);
        for (_, key) in &order[..order.len() / 2] {
            self.entries.remove(key);
        }

        let Self {
            entries,
            data,
            relocations,
            ..
        } = self;
        let mut packed = Vec::with_capacity(data.len());
        for entry in entries.values_mut() {
            let Some(curves) = entry.curves.as_mut() else {
                continue;
            };
            if curves.count == 0 {
                curves.first = 0;
                continue;
            }
            // Whole blocks move, band tables and all; the offsets inside them
            // are base-relative, so only `first` needs rewriting.
            let start = curves.first as usize * FLOATS_PER_TEXEL;
            let end = start + curves.texels as usize * FLOATS_PER_TEXEL;
            let moved = (packed.len() / FLOATS_PER_TEXEL) as u32;
            relocations.insert(curves.first, moved);
            curves.first = moved;
            packed.extend_from_slice(&data[start..end]);
        }
        self.data = packed;
        self.uploaded_rows = 0;
        self.generation += 1;
        self.layout_generation += 1;
    }

    /// Upload any rows of curve data added since the last flush.
    pub fn flush(&mut self, queue: &wgpu::Queue) {
        let floats_per_row = floats_per_row(self.width);
        let total_rows = self.data.len().div_ceil(floats_per_row) as u32;
        if total_rows <= self.uploaded_rows {
            return;
        }
        // Re-upload from the last complete row so a partially-filled row that
        // gained new curves is refreshed too.
        let start_row = self.uploaded_rows.saturating_sub(1);
        let mut rows = Vec::from(&self.data[start_row as usize * floats_per_row..]);
        rows.resize((total_rows - start_row) as usize * floats_per_row, 0.0);

        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: 0,
                    y: start_row,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&rows),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(self.width * 16),
                rows_per_image: None,
            },
            wgpu::Extent3d {
                width: self.width,
                height: total_rows - start_row,
                depth_or_array_layers: 1,
            },
        );
        self.uploaded_rows = total_rows;
    }
}

/// The `wght` variation axis of a face, in user units (min, max). `None` for a
/// static face — or one whose axis is a single point — which keeps the
/// single-master path and the `b_first == 0` fast path in the shader.
fn wght_axis(
    font_system: &mut FontSystem,
    font_id: fontdb::ID,
    weight: fontdb::Weight,
) -> Option<(f32, f32)> {
    let font = font_system.get_font(font_id, weight)?;
    let axis = font.as_swash().variations().find_by_tag(WGHT)?;
    let (min, max) = (axis.min_value(), axis.max_value());
    (max > min).then_some((min, max))
}

/// The `Weight` that pins the `wght` axis to `value`; cosmic-text clamps the
/// weight into the axis range when it builds the scaler, so the ends land
/// exactly on the masters.
fn axis_weight(value: f32) -> fontdb::Weight {
    fontdb::Weight(value.clamp(0.0, u16::MAX as f32) as u16)
}

/// Where a shaped weight falls on the `min..max` axis: 0 at the minimum,
/// 1 at the maximum. This is the blend the GPU defaults to, so a glyph looks
/// like the weight it was shaped with unless a caller overrides it.
fn weight_blend(weight: fontdb::Weight, min: f32, max: f32) -> f32 {
    ((f32::from(weight.0) - min) / (max - min)).clamp(0.0, 1.0)
}

/// Whether two masters interpolate: same commands, same order. The blend is
/// per control point, so a differing command sequence would pair up unrelated
/// points and produce a shape belonging to neither master.
fn masters_compatible(a: &[Command], b: &[Command]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(a, b)| std::mem::discriminant(a) == std::mem::discriminant(b))
}

/// Said once per process, not once per glyph — a broken font would say it
/// thousands of times a frame.
fn warn_incompatible_masters(font_id: fontdb::ID) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        eprintln!(
            "faf-text: {font_id:?} has point-incompatible wght masters; \
             falling back to single-master glyphs"
        );
    });
}

/// Flatten one master's path into padded quadratic records, with its bbox.
fn flatten(commands: &[Command]) -> (Vec<f32>, [f32; 4]) {
    let mut records = Vec::new();
    let mut flat = Flattener::new(&mut records);
    for command in commands {
        match *command {
            Command::MoveTo(p) => flat.move_to([p.x, p.y]),
            Command::LineTo(p) => flat.line_to([p.x, p.y]),
            Command::QuadTo(c, p) => flat.quad_to([c.x, c.y], [p.x, p.y]),
            Command::CurveTo(c1, c2, p) => flat.cubic_to([c1.x, c1.y], [c2.x, c2.y], [p.x, p.y]),
            Command::Close => flat.close(),
        }
    }
    flat.close();
    let bbox = flat.bbox;
    (records, bbox)
}

fn create_texture(device: &wgpu::Device, width: u32, height: u32) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("faf-text curve data"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba32Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

/// A curve record's control-point range along one axis (0 = x, 1 = y).
fn control_span(record: &[f32], axis: usize) -> (f32, f32) {
    let coords = [record[axis], record[2 + axis], record[4 + axis]];
    (
        coords.iter().copied().fold(f32::INFINITY, f32::min),
        coords.iter().copied().fold(f32::NEG_INFINITY, f32::max),
    )
}

/// Curve indices per band along one axis of `[min, max]` (`axis` 0 = x,
/// 1 = y), for curves already packed as flat records.
///
/// A curve joins every band its control-point range overlaps, widened by
/// [`BAND_EPSILON`] of a band's height. The control points bound the curve
/// (convex hull), so a curve left out of band `i` cannot cross any ray inside
/// band `i` — the shader's winding sum is unchanged, term for term.
///
/// With a second master the range spans both: a blended control point is a
/// lerp of the two masters' and so never leaves their per-coordinate hull, so
/// the same "left out means exactly zero" argument holds at every blend.
fn band_lists(
    records: &[f32],
    b_records: Option<&[f32]>,
    axis: usize,
    min: f32,
    max: f32,
) -> Vec<Vec<u32>> {
    let height = ((max - min) / BANDS as f32).max(f32::MIN_POSITIVE);
    let eps = height * BAND_EPSILON;
    let last_band = (BANDS - 1) as f32;
    let mut lists = vec![Vec::new(); BANDS];
    for (index, record) in records.chunks_exact(FLOATS_PER_CURVE).enumerate() {
        let (mut lo, mut hi) = control_span(record, axis);
        if let Some(b) = b_records {
            let (b_lo, b_hi) = control_span(&b[index * FLOATS_PER_CURVE..], axis);
            lo = lo.min(b_lo);
            hi = hi.max(b_hi);
        }
        let first = (((lo - eps - min) / height).floor()).clamp(0.0, last_band) as usize;
        let last = (((hi + eps - min) / height).floor()).clamp(0.0, last_band) as usize;
        for list in &mut lists[first..=last] {
            list.push(index as u32);
        }
    }
    lists
}

/// Lay out a banded block: header, index lists, then master A's curve records
/// and master B's parallel copy. See the record-layout comment at the top of
/// this file.
fn banded_block(records: &[f32], b_records: Option<&[f32]>, bbox: [f32; 4]) -> Vec<f32> {
    let mut lists = band_lists(records, b_records, 1, bbox[1], bbox[3]);
    lists.extend(band_lists(records, b_records, 0, bbox[0], bbox[2]));

    // Lists start on texel boundaries (the shader walks them four at a time)
    // and the region as a whole is padded to an even texel count, which keeps
    // the records that follow two-texel aligned.
    let list_texels: usize = lists
        .iter()
        .map(|list| list.len().div_ceil(FLOATS_PER_TEXEL))
        .sum::<usize>()
        .next_multiple_of(TEXELS_PER_CURVE);
    let records_offset = HEADER_TEXELS + list_texels;

    let mut block = Vec::with_capacity(records_offset * FLOATS_PER_TEXEL + records.len());
    let mut offset = HEADER_TEXELS;
    for list in &lists {
        block.push(offset as f32);
        block.push(list.len() as f32);
        offset += list.len().div_ceil(FLOATS_PER_TEXEL);
    }
    debug_assert_eq!(block.len(), HEADER_TEXELS * FLOATS_PER_TEXEL);
    for list in &lists {
        for &index in list {
            block.push((records_offset + index as usize * TEXELS_PER_CURVE) as f32);
        }
        block.resize(block.len().next_multiple_of(FLOATS_PER_TEXEL), 0.0);
    }
    block.resize(records_offset * FLOATS_PER_TEXEL, 0.0);
    block.extend_from_slice(records);
    if let Some(b) = b_records {
        block.extend_from_slice(b);
    }
    block
}

/// Flattens a zeno path into padded quadratic records, tracking the bbox.
struct Flattener<'a> {
    out: &'a mut Vec<f32>,
    start: [f32; 2],
    current: [f32; 2],
    open: bool,
    bbox: [f32; 4],
}

impl<'a> Flattener<'a> {
    fn new(out: &'a mut Vec<f32>) -> Self {
        Self {
            out,
            start: [0.0; 2],
            current: [0.0; 2],
            open: false,
            bbox: [f32::MAX, f32::MAX, f32::MIN, f32::MIN],
        }
    }

    fn push(&mut self, p0: [f32; 2], p1: [f32; 2], p2: [f32; 2]) {
        for p in [p0, p1, p2] {
            self.bbox[0] = self.bbox[0].min(p[0]);
            self.bbox[1] = self.bbox[1].min(p[1]);
            self.bbox[2] = self.bbox[2].max(p[0]);
            self.bbox[3] = self.bbox[3].max(p[1]);
        }
        self.out
            .extend_from_slice(&[p0[0], p0[1], p1[0], p1[1], p2[0], p2[1], 0.0, 0.0]);
    }

    fn move_to(&mut self, p: [f32; 2]) {
        self.close();
        self.start = p;
        self.current = p;
        self.open = true;
    }

    fn line_to(&mut self, p: [f32; 2]) {
        let mid = [
            (self.current[0] + p[0]) * 0.5,
            (self.current[1] + p[1]) * 0.5,
        ];
        self.push(self.current, mid, p);
        self.current = p;
    }

    fn quad_to(&mut self, c: [f32; 2], p: [f32; 2]) {
        self.push(self.current, c, p);
        self.current = p;
    }

    /// Approximate a cubic with four quadratics (split at quarters, midpoint
    /// control rule) — comfortably below visible error at text sizes.
    fn cubic_to(&mut self, c1: [f32; 2], c2: [f32; 2], p: [f32; 2]) {
        let cubic = [self.current, c1, c2, p];
        let (a, b) = split_cubic(&cubic, 0.5);
        for half in [a, b] {
            let (q0, q1) = split_cubic(&half, 0.5);
            for seg in [q0, q1] {
                let control = [
                    (3.0 * (seg[1][0] + seg[2][0]) - seg[0][0] - seg[3][0]) * 0.25,
                    (3.0 * (seg[1][1] + seg[2][1]) - seg[0][1] - seg[3][1]) * 0.25,
                ];
                self.push(seg[0], control, seg[3]);
            }
        }
        self.current = p;
    }

    fn close(&mut self) {
        if self.open && self.current != self.start {
            self.line_to(self.start);
        }
        self.open = false;
    }
}

fn lerp(a: [f32; 2], b: [f32; 2], t: f32) -> [f32; 2] {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
}

fn split_cubic(c: &[[f32; 2]; 4], t: f32) -> ([[f32; 2]; 4], [[f32; 2]; 4]) {
    let ab = lerp(c[0], c[1], t);
    let bc = lerp(c[1], c[2], t);
    let cd = lerp(c[2], c[3], t);
    let abbc = lerp(ab, bc, t);
    let bccd = lerp(bc, cd, t);
    let mid = lerp(abbc, bccd, t);
    ([c[0], ab, abbc, mid], [mid, bccd, cd, c[3]])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;

    /// Extract one glyph by raw id, stamping it with the current frame.
    fn get(
        store: &mut CurveStore,
        font_system: &mut FontSystem,
        font_id: fontdb::ID,
        glyph_id: u16,
    ) -> Option<GlyphCurves> {
        store.get_or_insert(
            font_system,
            font_id,
            glyph_id,
            fontdb::Weight::NORMAL,
            CacheKeyFlags::empty(),
        )
    }

    fn key_of(font_id: fontdb::ID, glyph_id: u16) -> GlyphKey {
        GlyphKey {
            font_id,
            glyph_id,
            weight: fontdb::Weight::NORMAL.0,
            flags: CacheKeyFlags::empty(),
        }
    }

    /// The glyph's whole block — band tables included.
    fn slice_of(store: &CurveStore, curves: &GlyphCurves) -> Vec<f32> {
        let start = curves.first as usize * FLOATS_PER_TEXEL;
        let end = start + curves.texels as usize * FLOATS_PER_TEXEL;
        store.data[start..end].to_vec()
    }

    #[test]
    fn texture_doubles_on_overflow_and_preserves_existing_curves() {
        let (device, _) = testing::gpu();
        let mut font_system = testing::font_system();
        let font_id = testing::font_id(&font_system);
        // Two rows hold 256 curves — a handful of glyphs.
        let mut store = CurveStore::with_size(device, CURVE_TEX_WIDTH, 2, 64);
        store.begin_frame(1);

        let first = get(&mut store, &mut font_system, font_id, 36)
            .expect("glyph 36 ('A') should have an outline");
        assert!(first.count > 0);
        let snapshot = slice_of(&store, &first);

        for glyph_id in 37..120 {
            get(&mut store, &mut font_system, font_id, glyph_id);
        }

        assert!(store.height > 2, "texture should have grown");
        assert_eq!(store.height % 2, 0, "height doubles, so stays even");
        assert!(store.generation > 0, "growth bumps the generation");
        assert_eq!(store.uploaded_rows, 0, "growth schedules a full re-upload");
        // The mirror is untouched by growth, so the first glyph is where it was.
        let same = get(&mut store, &mut font_system, font_id, 36).unwrap();
        assert_eq!(same.first, first.first);
        assert_eq!(slice_of(&store, &same), snapshot);
    }

    #[test]
    fn growth_stops_at_the_cap_and_defers_compaction_to_the_frame_edge() {
        let (device, _) = testing::gpu();
        let mut font_system = testing::font_system();
        let font_id = testing::font_id(&font_system);
        // max_height == height: no growth headroom at all.
        let mut store = CurveStore::with_size(device, CURVE_TEX_WIDTH, 8, 8);
        store.begin_frame(1);

        let mut overflowed = None;
        for glyph_id in 36..500 {
            get(&mut store, &mut font_system, font_id, glyph_id);
            if store.needs_compact {
                overflowed = Some(glyph_id);
                break;
            }
        }
        let overflowed = overflowed.expect("the store should have filled up");
        assert_eq!(store.height, 8, "capped stores never grow");
        assert_eq!(store.generation, 0, "no eviction happens mid-frame");
        assert!(
            !store.entries.contains_key(&key_of(font_id, overflowed)),
            "an overflowing glyph must not be cached as outline-less"
        );
        let filled = store.data.len();

        store.begin_frame(2);
        assert!(!store.needs_compact);
        assert!(store.generation > 0, "compaction bumps the generation");
        assert!(store.data.len() < filled, "compaction frees room");
        assert_eq!(store.uploaded_rows, 0, "compaction re-uploads everything");
        assert!(
            get(&mut store, &mut font_system, font_id, overflowed).is_some_and(|c| c.count > 0),
            "the glyph that overflowed fits after compaction"
        );
    }

    #[test]
    fn compaction_drops_the_lru_half_and_repacks_survivors() {
        let (device, _) = testing::gpu();
        let mut font_system = testing::font_system();
        let font_id = testing::font_id(&font_system);
        let mut store = CurveStore::with_size(device, CURVE_TEX_WIDTH, 32, 32);

        // Fill over three frames so `last_used` actually orders the entries.
        let mut before: FxHashMap<GlyphKey, Vec<f32>> = FxHashMap::default();
        let mut glyph_id = 36;
        for frame in 1..=3 {
            store.begin_frame(frame);
            let target = store.entries.len() + 12;
            while store.entries.len() < target && !store.needs_compact {
                if let Some(curves) = get(&mut store, &mut font_system, font_id, glyph_id)
                    && curves.count > 0
                {
                    before.insert(key_of(font_id, glyph_id), slice_of(&store, &curves));
                }
                glyph_id += 1;
            }
        }
        let populated = store.entries.len();
        assert!(populated > 6, "need entries on both sides of the LRU split");
        let oldest: Vec<GlyphKey> = store
            .entries
            .iter()
            .filter(|(_, e)| e.last_used == 1)
            .map(|(k, _)| *k)
            .collect();
        assert!(!oldest.is_empty());

        store.needs_compact = true;
        store.begin_frame(4);

        assert_eq!(store.entries.len(), populated - populated / 2);
        for key in &oldest {
            assert!(
                !store.entries.contains_key(key),
                "frame-1 entries are the least recently used"
            );
        }
        // Every survivor's `first` was rewritten to point at its own, unmoved
        // curve bytes, and the mirror is dense with no gaps or overlap.
        let mut live_floats = 0;
        for (key, entry) in &store.entries {
            let Some(curves) = entry.curves else { continue };
            if curves.count == 0 {
                continue;
            }
            live_floats += curves.texels as usize * FLOATS_PER_TEXEL;
            assert_eq!(
                &slice_of(&store, &curves),
                before.get(key).expect("survivor was extracted earlier"),
                "curve data moved but stayed byte-identical"
            );
        }
        assert_eq!(live_floats, store.data.len(), "mirror is densely packed");
    }

    #[test]
    fn churning_thousands_of_glyph_and_weight_combos_stays_bounded() {
        let (device, _) = testing::gpu();
        let mut font_system = testing::font_system();
        let font_id = testing::font_id(&font_system);
        let mut store = CurveStore::with_size(device, CURVE_TEX_WIDTH, 8, 32);
        let cap = texel_capacity(CURVE_TEX_WIDTH, 32);

        // 100 glyphs × 20 weights, a frame per weight. Every combination is a
        // distinct entry, so this churns many times the texture's worth.
        for (frame, weight) in (1..=20u64).zip((100..).step_by(50)) {
            store.begin_frame(frame);
            for glyph_id in 36..136 {
                let curves = store.get_or_insert(
                    &mut font_system,
                    font_id,
                    glyph_id,
                    fontdb::Weight(weight),
                    CacheKeyFlags::empty(),
                );
                if let Some(curves) = curves {
                    assert!(
                        (curves.first + curves.texels) as usize <= cap,
                        "curve block escaped the texture"
                    );
                }
            }
            assert!(
                store.data.len() / FLOATS_PER_TEXEL <= cap,
                "the mirror outgrew the texture"
            );
            assert!(store.height <= 32, "growth respects the cap");
        }
    }

    /// Records for curves with the given (x-range, y-range), as flat quads
    /// whose control points span exactly that box.
    fn records_spanning(ranges: &[([f32; 2], [f32; 2])]) -> Vec<f32> {
        let mut out = Vec::new();
        for ([x0, x1], [y0, y1]) in ranges {
            let mid = [(x0 + x1) * 0.5, (y0 + y1) * 0.5];
            out.extend_from_slice(&[*x0, *y0, mid[0], mid[1], *x1, *y1, 0.0, 0.0]);
        }
        out
    }

    #[test]
    fn bands_take_every_curve_overlapping_them_plus_an_epsilon_margin() {
        // Eight bands over [0, 8]: band i is [i, i+1], epsilon 0.05.
        let records = records_spanning(&[
            ([0.0, 1.0], [0.0, 0.4]),  // 0: inside band 0
            ([0.0, 1.0], [2.5, 4.5]),  // 1: straddles bands 2, 3, 4
            ([0.0, 1.0], [3.0, 3.0]),  // 2: exactly on the 2|3 boundary
            ([0.0, 1.0], [6.96, 7.9]), // 3: within epsilon of band 6
            ([0.0, 1.0], [0.0, 8.0]),  // 4: spans everything
        ]);
        let lists = band_lists(&records, None, 1, 0.0, 8.0);

        assert_eq!(lists[0], vec![0, 4]);
        assert_eq!(lists[1], vec![4]);
        assert_eq!(lists[2], vec![1, 2, 4], "the boundary curve joins band 2");
        assert_eq!(lists[3], vec![1, 2, 4], "…and band 3");
        assert_eq!(lists[4], vec![1, 4]);
        assert_eq!(lists[5], vec![4]);
        assert_eq!(lists[6], vec![3, 4], "6.96 is inside band 7 but within eps");
        assert_eq!(lists[7], vec![3, 4]);

        // The x-ranges are identical, so every x-band holds every curve.
        for list in band_lists(&records, None, 0, 0.0, 1.0) {
            assert_eq!(list, vec![0, 1, 2, 3, 4]);
        }
    }

    #[test]
    fn a_curve_outside_a_band_cannot_cross_any_ray_inside_it() {
        // The property the shader relies on: what a band leaves out has all
        // three control points on one side of every ray through that band.
        let mut records = Vec::new();
        for i in 0..40u32 {
            let y = i as f32 * 0.1;
            records.extend(records_spanning(&[([0.0, 1.0], [y, y + 0.25])]));
        }
        let lists = band_lists(&records, None, 1, 0.0, 4.0);
        let height = 4.0 / BANDS as f32;
        for (band, list) in lists.iter().enumerate() {
            let (lo, hi) = (band as f32 * height, (band as f32 + 1.0) * height);
            for (index, record) in records.chunks_exact(FLOATS_PER_CURVE).enumerate() {
                if list.contains(&(index as u32)) {
                    continue;
                }
                let ys = [record[1], record[3], record[5]];
                let below = ys.iter().all(|y| *y < lo);
                let above = ys.iter().all(|y| *y > hi);
                assert!(below || above, "curve {index} straddles band {band}");
            }
        }
    }

    /// Decode a banded block the way the shader does: header entry per band,
    /// index list, curve records. Returns each band's resolved curve indices.
    fn decode_bands(block: &[f32], count: u32, bbox: [f32; 4]) -> Vec<Vec<u32>> {
        let records_offset = block.len() / FLOATS_PER_TEXEL - count as usize * TEXELS_PER_CURVE;
        let mut decoded = Vec::new();
        for slot in 0..2 * BANDS {
            let list = block[slot * 2] as usize;
            let len = block[slot * 2 + 1] as usize;
            assert!(list >= HEADER_TEXELS && list + len.div_ceil(4) <= records_offset);
            let axis = if slot < BANDS { 1 } else { 0 };
            let band = slot % BANDS;
            let height = (bbox[axis + 2] - bbox[axis]) / BANDS as f32;
            let (lo, hi) = (
                bbox[axis] + band as f32 * height,
                bbox[axis] + (band as f32 + 1.0) * height,
            );
            let eps = height * BAND_EPSILON;

            let mut indices = Vec::new();
            for i in 0..len {
                let texel = block[list * FLOATS_PER_TEXEL + i] as usize;
                assert!(texel >= records_offset, "list entry points into the header");
                assert_eq!((texel - records_offset) % TEXELS_PER_CURVE, 0);
                let start = texel * FLOATS_PER_TEXEL;
                let record = &block[start..start + FLOATS_PER_CURVE];
                let coords = [record[axis], record[2 + axis], record[4 + axis]];
                let curve_lo = coords.iter().copied().fold(f32::INFINITY, f32::min);
                let curve_hi = coords.iter().copied().fold(f32::NEG_INFINITY, f32::max);
                assert!(
                    curve_hi >= lo - eps && curve_lo <= hi + eps,
                    "band {slot} lists a curve it does not overlap"
                );
                indices.push(((texel - records_offset) / TEXELS_PER_CURVE) as u32);
            }
            decoded.push(indices);
        }
        decoded
    }

    #[test]
    fn only_glyphs_past_the_threshold_are_banded_and_their_headers_round_trip() {
        let (device, _) = testing::gpu();
        let mut font_system = testing::font_system();
        let font_id = testing::font_id(&font_system);
        let mut store = CurveStore::new(device);
        store.begin_frame(1);

        let mut banded: Option<GlyphCurves> = None;
        let mut flat: Option<GlyphCurves> = None;
        for glyph_id in 36..200 {
            let Some(curves) = get(&mut store, &mut font_system, font_id, glyph_id) else {
                continue;
            };
            if curves.count == 0 {
                continue;
            }
            if curves.banded {
                banded.get_or_insert(curves);
            } else {
                flat.get_or_insert(curves);
            }
        }
        let banded = banded.expect("some glyph exceeds the banding threshold");
        let flat = flat.expect("some glyph stays under it");

        assert!(flat.count <= BAND_MIN_CURVES);
        assert_eq!(
            flat.instance_count(),
            flat.count,
            "flat glyphs are unflagged"
        );
        assert_eq!(
            flat.texels as usize,
            flat.count as usize * TEXELS_PER_CURVE,
            "a flat block is nothing but records"
        );

        assert!(banded.count > BAND_MIN_CURVES);
        assert_eq!(banded.instance_count(), banded.count | BANDED_FLAG);
        assert_eq!(banded.first % 2, 0, "records must not straddle a row");
        assert_eq!(banded.texels % 2, 0);

        // Every band resolves to curves it overlaps (checked in decode_bands),
        // and no curve that overlaps a band is missing from it.
        let block = slice_of(&store, &banded);
        let decoded = decode_bands(&block, banded.count, banded.bbox);
        let records = &block[block.len() - banded.count as usize * FLOATS_PER_CURVE..];
        for (slot, indices) in decoded.iter().enumerate() {
            let axis = if slot < BANDS { 1 } else { 0 };
            let expected = &band_lists(
                records,
                None,
                axis,
                banded.bbox[axis],
                banded.bbox[axis + 2],
            )[slot % BANDS];
            assert_eq!(indices, expected, "band {slot} lost curves");
        }
        assert!(
            decoded
                .iter()
                .any(|band| band.len() < banded.count as usize),
            "banding should cut some band's loop below the full curve set"
        );
    }

    #[test]
    fn shader_constants_match_the_record_layout() {
        let src = include_str!("shaders.wgsl");
        for expected in [
            format!("const CURVE_TEX_WIDTH: u32 = {CURVE_TEX_WIDTH}u;"),
            format!("const BANDS: u32 = {BANDS}u;"),
            format!("const BANDED_FLAG: u32 = 0x{BANDED_FLAG:08X}u;"),
        ] {
            assert!(
                src.contains(&expected),
                "shaders.wgsl is missing `{expected}`"
            );
        }
    }

    fn quads(data: &[f32]) -> Vec<[[f32; 2]; 3]> {
        data.chunks_exact(FLOATS_PER_CURVE)
            .map(|c| [[c[0], c[1]], [c[2], c[3]], [c[4], c[5]]])
            .collect()
    }

    #[test]
    fn lines_become_degenerate_quads_and_contours_close() {
        let mut data = Vec::new();
        let mut f = Flattener::new(&mut data);
        f.move_to([0.0, 0.0]);
        f.line_to([1.0, 0.0]);
        f.line_to([1.0, 1.0]);
        f.close(); // triangle: closing edge back to (0,0) must be emitted
        let bbox = f.bbox;

        let q = quads(&data);
        assert_eq!(q.len(), 3);
        // control point of a line is its midpoint
        assert_eq!(q[0][1], [0.5, 0.0]);
        // closing edge returns to the contour start
        assert_eq!(q[2][2], [0.0, 0.0]);
        assert_eq!(bbox, [0.0, 0.0, 1.0, 1.0]);
    }

    #[test]
    fn unterminated_contour_is_closed_by_move_to() {
        let mut data = Vec::new();
        let mut f = Flattener::new(&mut data);
        f.move_to([0.0, 0.0]);
        f.line_to([1.0, 0.0]);
        f.move_to([5.0, 5.0]); // implicit close of the open contour
        f.line_to([6.0, 5.0]);
        f.close();

        let q = quads(&data);
        assert_eq!(q.len(), 4);
        assert_eq!(q[1][2], [0.0, 0.0]);
    }

    #[test]
    fn cubic_splits_into_four_quads_hitting_endpoints() {
        let mut data = Vec::new();
        let mut f = Flattener::new(&mut data);
        f.move_to([0.0, 0.0]);
        f.cubic_to([0.0, 1.0], [1.0, 1.0], [1.0, 0.0]);

        let q = quads(&data);
        assert_eq!(q.len(), 4);
        assert_eq!(q[0][0], [0.0, 0.0]);
        assert_eq!(q[3][2], [1.0, 0.0]);
        // pieces are contiguous
        for w in q.windows(2) {
            assert_eq!(w[0][2], w[1][0]);
        }
        // quadratic approximation of this symmetric cubic peaks at y = 0.75
        let apex = q[1][2][1];
        assert!((apex - 0.75).abs() < 0.01, "apex {apex}");
    }

    // ---- Variable fonts: a second master per glyph ----

    /// Manrope's axis, the one the variable-font tests interpolate over.
    const AXIS: (f32, f32) = (200.0, 800.0);

    /// A glyph's block split into (band tables, master A records, master B
    /// records). B is empty for a single-master glyph.
    fn masters(store: &CurveStore, curves: &GlyphCurves) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
        let block = slice_of(store, curves);
        let records = curves.count as usize * FLOATS_PER_CURVE;
        let masters = if curves.b_offset == 0 { 1 } else { 2 };
        let head = block.len() - masters * records;
        (
            block[..head].to_vec(),
            block[head..head + records].to_vec(),
            block[head + records..].to_vec(),
        )
    }

    #[test]
    fn the_wght_axis_is_found_on_a_variable_face_and_absent_on_a_static_one() {
        let mut variable = testing::variable_font_system();
        let id = testing::font_id_of(&variable, testing::VARIABLE_FAMILY);
        assert_eq!(
            wght_axis(&mut variable, id, fontdb::Weight::NORMAL),
            Some(AXIS)
        );

        let mut static_font = testing::font_system();
        let id = testing::font_id_of(&static_font, testing::STATIC_FAMILY);
        assert_eq!(
            wght_axis(&mut static_font, id, fontdb::Weight::NORMAL),
            None
        );
    }

    #[test]
    fn a_static_face_stores_one_master_and_keeps_the_b_first_fast_path() {
        let (device, _) = testing::gpu();
        let mut font_system = testing::font_system();
        let font_id = testing::font_id_of(&font_system, testing::STATIC_FAMILY);
        let mut store = CurveStore::new(device);
        store.begin_frame(1);

        let curves = get(&mut store, &mut font_system, font_id, 36).expect("'A' has an outline");
        assert_eq!(curves.b_offset, 0);
        assert_eq!(curves.b_first(), 0, "the shader must skip the second fetch");
        assert_eq!(curves.weight_t, 0.0);
        let (_, a, b) = masters(&store, &curves);
        assert!(b.is_empty(), "no second master to store");
        assert_eq!(a.len(), curves.count as usize * FLOATS_PER_CURVE);
    }

    #[test]
    fn a_variable_face_stores_both_masters_in_parallel_regions() {
        let (device, _) = testing::gpu();
        let mut font_system = testing::variable_font_system();
        let font_id = testing::font_id_of(&font_system, testing::VARIABLE_FAMILY);
        let mut store = CurveStore::new(device);
        store.begin_frame(1);

        let mut seen_flat = false;
        let mut seen_banded = false;
        for glyph_id in 1..200 {
            let Some(curves) = get(&mut store, &mut font_system, font_id, glyph_id) else {
                continue;
            };
            if curves.count == 0 {
                continue;
            }
            seen_flat |= !curves.banded;
            seen_banded |= curves.banded;

            // B's records sit exactly one A-region past A's, so a single
            // constant offset takes the shader from any record to its twin.
            assert_eq!(curves.b_offset, curves.count * TEXELS_PER_CURVE as u32);
            assert_eq!(curves.b_first(), curves.first + curves.b_offset);
            assert_eq!(curves.first % 2, 0, "records must not straddle a row");
            assert_eq!(curves.texels % 2, 0);
            assert!(
                (curves.weight_t - 1.0 / 3.0).abs() < 1e-6,
                "wght 400 of 200..800"
            );

            let (_, a, b) = masters(&store, &curves);
            assert_eq!(a.len(), b.len(), "the masters are point-for-point parallel");
            assert_ne!(a, b, "the axis ends are different shapes");
            // The bbox is the union: every control point of either master is
            // inside it, so the padded quad covers the whole blend range.
            let records = a.chunks_exact(FLOATS_PER_CURVE);
            for record in records.chain(b.chunks_exact(FLOATS_PER_CURVE)) {
                // The trailing two floats of a record are padding, not a point.
                for point in record[..6].chunks_exact(2) {
                    assert!(point[0] >= curves.bbox[0] && point[0] <= curves.bbox[2]);
                    assert!(point[1] >= curves.bbox[1] && point[1] <= curves.bbox[3]);
                }
            }
        }
        assert!(seen_flat && seen_banded, "both block shapes should appear");
    }

    #[test]
    fn band_lists_of_a_dual_master_glyph_index_master_a_and_cover_master_b() {
        let (device, _) = testing::gpu();
        let mut font_system = testing::variable_font_system();
        let font_id = testing::font_id_of(&font_system, testing::VARIABLE_FAMILY);
        let mut store = CurveStore::new(device);
        store.begin_frame(1);

        let banded = (1..200)
            .filter_map(|glyph_id| get(&mut store, &mut font_system, font_id, glyph_id))
            .find(|c| c.banded)
            .expect("some Manrope glyph exceeds the banding threshold");

        let block = slice_of(&store, &banded);
        let (head, a, b) = masters(&store, &banded);
        let records_offset = head.len() / FLOATS_PER_TEXEL;
        let a_texels = banded.count as usize * TEXELS_PER_CURVE;

        for slot in 0..2 * BANDS {
            let list = block[slot * 2] as usize;
            let len = block[slot * 2 + 1] as usize;
            for i in 0..len {
                let texel = block[list * FLOATS_PER_TEXEL + i] as usize;
                assert!(
                    (records_offset..records_offset + a_texels).contains(&texel),
                    "band {slot} points outside master A's records"
                );
            }
        }
        // Membership is decided over both masters, so a blended curve can
        // never cross a ray in a band that left it out.
        for (slot, list) in band_lists(&a, Some(&b), 1, banded.bbox[1], banded.bbox[3])
            .iter()
            .enumerate()
        {
            let expected = block[slot * 2 + 1] as usize;
            assert_eq!(list.len(), expected, "y-band {slot} lost curves");
        }
    }

    #[test]
    fn masters_pair_up_only_when_their_command_sequences_match() {
        use cosmic_text::Command;
        use swash::zeno::Point;

        let p = |x: f32| Point::new(x, x);
        let a = [
            Command::MoveTo(p(0.0)),
            Command::LineTo(p(1.0)),
            Command::QuadTo(p(2.0), p(3.0)),
            Command::Close,
        ];
        let bolder = [
            Command::MoveTo(p(0.1)),
            Command::LineTo(p(1.4)),
            Command::QuadTo(p(2.2), p(3.9)),
            Command::Close,
        ];
        assert!(
            masters_compatible(&a, &bolder),
            "same commands, moved points"
        );

        // A curve where the other master has a line: the blend would pair a
        // control point with an endpoint and produce a shape from neither.
        let retyped = [
            Command::MoveTo(p(0.0)),
            Command::QuadTo(p(1.0), p(1.5)),
            Command::QuadTo(p(2.0), p(3.0)),
            Command::Close,
        ];
        assert!(!masters_compatible(&a, &retyped));
        assert!(!masters_compatible(&a, &a[..3]), "a dropped contour");
        assert!(!masters_compatible(&a[..1], &a));

        // The real font keeps its promise at both ends of the axis.
        let (device, _) = testing::gpu();
        let mut font_system = testing::variable_font_system();
        let font_id = testing::font_id_of(&font_system, testing::VARIABLE_FAMILY);
        let mut store = CurveStore::new(device);
        let flags = CacheKeyFlags::empty();
        for glyph_id in 1..200 {
            let light = store.outline(
                &mut font_system,
                font_id,
                glyph_id,
                axis_weight(AXIS.0),
                flags,
            );
            let bold = store.outline(
                &mut font_system,
                font_id,
                glyph_id,
                axis_weight(AXIS.1),
                flags,
            );
            if let (Some(light), Some(bold)) = (light, bold) {
                assert!(
                    masters_compatible(&light, &bold),
                    "glyph {glyph_id} masters do not interpolate"
                );
            }
        }
    }

    #[test]
    fn the_shaped_weight_maps_onto_the_axis_ends() {
        let (min, max) = AXIS;
        let t = |w: u16| weight_blend(fontdb::Weight(w), min, max);
        assert_eq!(t(200), 0.0, "axis minimum is master A");
        assert_eq!(t(500), 0.5, "the midpoint blends half and half");
        assert_eq!(t(800), 1.0, "axis maximum is master B");
        assert!((t(400) - 1.0 / 3.0).abs() < 1e-6, "regular sits a third in");
        // Outside the axis the font itself clamps, so the blend has to as well.
        assert_eq!(t(100), 0.0);
        assert_eq!(t(950), 1.0);
        assert_eq!(axis_weight(min), fontdb::Weight(200));
        assert_eq!(axis_weight(max), fontdb::Weight(800));
    }
}
