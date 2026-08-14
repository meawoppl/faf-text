use cosmic_text::{CacheKey, CacheKeyFlags, Command, FontSystem, SwashCache, fontdb};
use half::f16;
use rustc_hash::FxHashMap;

/// Width in texels of the RGBA16F curve data texture. Must be even so a
/// curve's two texels never straddle a row, and must match `CURVE_TEX_WIDTH`
/// in `shaders.wgsl`.
pub const CURVE_TEX_WIDTH: u32 = 256;
/// Height the curve texture starts at; it doubles on overflow up to
/// [`CURVE_TEX_MAX_HEIGHT`] (or the device's limit, whichever is smaller).
pub const CURVE_TEX_HEIGHT: u32 = 2048;
/// Ceiling on curve-texture height. 256 × 8192 RGBA16F is 16 MiB and holds
/// ~2M quadratics; past that we evict instead of growing.
pub const CURVE_TEX_MAX_HEIGHT: u32 = 8192;

// --- Record layout. Everything that knows how a curve maps to texels lives
// here, so band tables and master pairs can change it in one place.
//
// Texels are RGBA16F: four halves, 8 bytes. Control points are em coordinates
// in roughly [-0.2, 1.2], where f16's 11-bit significand quantizes to at worst
// 2^-11 = 4.9e-4 em (0.031 px at 64 px/em, 0.15 px at 300); outlines are
// *rounded to f16 as they are flattened*, so bboxes, band membership and the
// early-out sort keys are all computed from the very numbers the shader reads.
// Integers stored in a texel — texel offsets and curve counts — are exact only
// up to [`F16_EXACT_INT`] = 2048, which is what bounds a banded block's size.
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
//     [header]              2*BANDS texels: BANDS y-band then BANDS x-band
//                           entries, one texel each — (descending list offset,
//                           curve count, split coordinate, ascending list
//                           offset), offsets in texels from the block base
//     [index lists]         two lists per band (descending then ascending) in
//                           header order, each starting on a texel boundary,
//                           region padded to even
//     [curve texels]        one per curve plus one per contour, see below
//
// A list entry is the texel offset of that curve's record from the block base,
// so the shader resolves a curve without knowing where the record region
// starts. The two shapes are told apart by [`BANDED_FLAG`] in the instance's
// `count`.
//
// **Endpoint sharing (banded blocks).** Within a contour p2 of curve i is p0
// of curve i+1 — the flattener emits contours contiguously and carries the
// point across, so the two are the same float, not merely equal — and the
// texel [p0.xy p1.xy] of the next curve therefore already holds this curve's
// p2 in its first two lanes. A contour of n curves is n+1 texels: n curve
// texels plus a terminator holding the last p2 (the contour's own first point
// again, since contours close). The shader's two-texel fetch is unchanged: it
// reads t0 = [p0 p1] and takes t1.xy as p2, which is the next curve's texel or
// the terminator. That halves the record region, 2 texels/curve → ~1.1.
//
// Flat blocks keep the unshared 2-texel record. Sharing needs an index list to
// step over contour terminators — or a per-texel branch in the innermost loop,
// which this shader has measured at 25% — and the flat path walks records by
// arithmetic alone (`i * TEXELS_PER_CURVE`). A flat block is at most
// `BAND_MIN_CURVES` curves, so what that leaves on the table is ~130 bytes a
// glyph. Both shapes hold the same f16 values, so a glyph rendered flat and
// banded is still bit-identical (`band_tables_do_not_move_a_single_pixel`).
//
// A band's two lists hold the *same* curves in opposite orders: descending by
// the member's maximum coordinate along the ray axis for rays fired in the
// +axis direction, ascending by its minimum for rays fired backwards. Control
// points bound the curve (convex hull), so the shader can stop walking a list
// the moment a fetched curve's bound has fallen behind the antialiasing
// window — every curve after it is further behind still. `split` is the median
// of the members' ray-axis midpoints: samples past it fire backwards, so
// neither side of a glyph pays for the curves on the other.
//
// A glyph from a variable font with a `wght` axis stores a second master: the
// same records extracted at the axis maximum, in a parallel region right after
// master A's, packed the same way (same commands, so the same contours and the
// same texel for every curve). [`GlyphCurves::record_texels`] is the texel
// distance from any A record to its B twin, so band lists keep indexing A
// alone and the shader reaches B by adding it.

/// Halves per RGBA16F texel.
const FLOATS_PER_TEXEL: usize = 4;
/// Bytes one texel occupies in the texture.
const BYTES_PER_TEXEL: usize = FLOATS_PER_TEXEL * 2;
/// Two RGBA16F texels (8 halves) per quadratic in a flat block:
/// [p0.xy p1.xy] [p2.xy pad pad].
const TEXELS_PER_CURVE: usize = 2;
const FLOATS_PER_CURVE: usize = TEXELS_PER_CURVE * FLOATS_PER_TEXEL;
/// Largest integer an f16 lane holds exactly (2^11; 2049 is not
/// representable). Band headers and index lists are integers in lanes, so a
/// block whose offsets would pass this cannot be banded — it falls back to the
/// flat layout, which addresses records by arithmetic on a u32 and has no
/// integers in the texture at all. Real glyphs are an order of magnitude
/// under: the largest offset DejaVu Sans or Manrope encodes is 254
/// (`banded_blocks_only_encode_integers_f16_holds_exactly` prints it).
const F16_EXACT_INT: usize = 2048;

/// Bands per axis. Must match `BANDS` in `shaders.wgsl`.
pub const BANDS: usize = 8;
/// Texels per band header entry: (descending list offset, curve count, split
/// coordinate, ascending list offset) — one texel. Must match
/// `BAND_ENTRY_TEXELS` in `shaders.wgsl`.
pub const BAND_ENTRY_TEXELS: usize = 1;
/// Texels the band header occupies: 2*BANDS entries, one texel each.
const HEADER_TEXELS: usize = BANDS * 2 * BAND_ENTRY_TEXELS;
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
    /// Whether a master-B region follows master A's. False for a static face,
    /// or for masters that do not interpolate.
    pub dual_master: bool,
    /// Blend factor matching the weight the text was shaped at: 0 = axis
    /// minimum, 1 = axis maximum. Meaningless without a second master.
    pub weight_t: f32,
    /// Texels one master's records occupy: `2 * count` in a flat block,
    /// `count + contours` (rounded up to even) in a banded one, where endpoint
    /// sharing applies. Also the A→B stride of a dual-master glyph.
    pub(crate) record_texels: u32,
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
        if self.dual_master {
            self.first + self.record_texels
        } else {
            0
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
    /// CPU mirror of the packed curve halves; the un-uploaded tail is flushed
    /// to the texture by `flush`.
    data: Vec<f16>,
    entries: FxHashMap<GlyphKey, Entry>,
    swash: SwashCache,
    /// How much of `data` the texture already holds. Tracked in halves, not
    /// rows: new glyphs often fit inside the current partial row, and a
    /// row-granular high-water mark would skip them forever.
    uploaded_floats: usize,
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
            uploaded_floats: 0,
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
        let (records, contours, mut bbox) = flatten(&a_commands);
        let b_records = b_commands.map(|commands| {
            let (b_records, _, b_bbox) = flatten(&commands);
            // The quad has to cover every weight the glyph can be drawn at.
            for i in 0..2 {
                bbox[i] = bbox[i].min(b_bbox[i]);
                bbox[2 + i] = bbox[2 + i].max(b_bbox[2 + i]);
            }
            b_records
        });
        // Same commands in, same records out — this only guards the layout
        // invariant the shader's fixed A→B stride depends on. The contours are
        // the same for the same reason, so one packing serves both masters.
        let b_records = b_records.filter(|b| b.len() == records.len());

        let count = (records.len() / FLOATS_PER_CURVE) as u32;
        if count == 0 {
            return Extracted::Done(Some(GlyphCurves {
                first: 0,
                count: 0,
                bbox: [0.0; 4],
                banded: false,
                dual_master: false,
                weight_t: 0.0,
                record_texels: 0,
                texels: 0,
            }));
        }

        // A block whose offsets outgrow f16's exact integers keeps the flat
        // layout, which stores no integers at all.
        let block = (count > self.band_min_curves)
            .then(|| banded_block(&records, b_records.as_deref(), &contours, bbox))
            .flatten();
        let (block, banded, record_texels) = match block {
            Some(block) => {
                let record_texels = shared_texels(count as usize, contours.len());
                (block, true, record_texels)
            }
            None => (
                flat_block(&records, b_records.as_deref()),
                false,
                count * TEXELS_PER_CURVE as u32,
            ),
        };
        let (dual_master, weight_t) = match (&b_records, axis) {
            (Some(_), Some((min, max))) => (true, weight_blend(weight, min, max)),
            _ => (false, 0.0),
        };
        debug_assert_eq!(block.len() % FLOATS_PER_CURVE, 0, "blocks are even texels");

        // Every block is an even number of texels long, so this base is even
        // and no curve record ever straddles a texture row.
        let first = (self.data.len() / FLOATS_PER_TEXEL) as u32;
        let texels = (block.len() / FLOATS_PER_TEXEL) as u32;
        // Control points were rounded to f16 as they were flattened, so this
        // conversion is exact for them; the integers the band tables carry are
        // exact by construction (see F16_EXACT_INT).
        self.data.extend(block.iter().map(|&v| f16::from_f32(v)));
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
            dual_master,
            weight_t,
            record_texels,
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
        self.uploaded_floats = 0;
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
        self.uploaded_floats = 0;
        self.generation += 1;
        self.layout_generation += 1;
    }

    /// (Bytes of packed curve data, bytes the texture occupies).
    pub fn memory(&self) -> (usize, usize) {
        (
            self.data.len() / FLOATS_PER_TEXEL * BYTES_PER_TEXEL,
            self.capacity() * BYTES_PER_TEXEL,
        )
    }

    /// Upload any rows of curve data added since the last flush.
    pub fn flush(&mut self, queue: &wgpu::Queue) {
        if self.data.len() <= self.uploaded_floats {
            return;
        }
        let floats_per_row = floats_per_row(self.width);
        let total_rows = self.data.len().div_ceil(floats_per_row) as u32;
        // Restart from the row holding the first new float, refreshing the
        // partially-filled row the new curves may share with old ones.
        let start_row = (self.uploaded_floats / floats_per_row) as u32;
        let mut rows = Vec::from(&self.data[start_row as usize * floats_per_row..]);
        rows.resize(
            (total_rows - start_row) as usize * floats_per_row,
            f16::ZERO,
        );

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
                bytes_per_row: Some(self.width * BYTES_PER_TEXEL as u32),
                rows_per_image: None,
            },
            wgpu::Extent3d {
                width: self.width,
                height: total_rows - start_row,
                depth_or_array_layers: 1,
            },
        );
        self.uploaded_floats = self.data.len();
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

/// Flatten one master's path into padded quadratic records, with the curve
/// count of each contour (in emission order) and the bbox. The contours are
/// what endpoint sharing packs against: within one, the next record's p0 *is*
/// this record's p2.
fn flatten(commands: &[Command]) -> (Vec<f32>, Vec<u32>, [f32; 4]) {
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
    let contours = flat.contours;
    debug_assert_eq!(
        contours.iter().sum::<u32>() as usize,
        records.len() / FLOATS_PER_CURVE,
        "every curve belongs to exactly one contour"
    );
    (records, contours, bbox)
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
        // RGBA16F, not RGBA32F: half the bytes and half the fetch bandwidth,
        // and the *better* supported of the two on WebGL2 — wgpu's GLES
        // backend gives Rgba16Float TEXTURE_BINDING plus filtering with no
        // extension at all, where Rgba32Float is sampled-but-unfilterable.
        format: wgpu::TextureFormat::Rgba16Float,
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

/// Every curve's control-point range along one axis, spanning both masters.
///
/// A blended control point is a lerp of the two masters' and so never leaves
/// their per-coordinate hull: one span bounds the glyph at every `weight_t`.
/// Both band membership and the early-out sort keys ride on that.
fn spans(records: &[f32], b_records: Option<&[f32]>, axis: usize) -> Vec<(f32, f32)> {
    records
        .chunks_exact(FLOATS_PER_CURVE)
        .enumerate()
        .map(|(index, record)| {
            let (mut lo, mut hi) = control_span(record, axis);
            if let Some(b) = b_records {
                let (b_lo, b_hi) = control_span(&b[index * FLOATS_PER_CURVE..], axis);
                lo = lo.min(b_lo);
                hi = hi.max(b_hi);
            }
            (lo, hi)
        })
        .collect()
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
    for (index, (lo, hi)) in spans(records, b_records, axis).into_iter().enumerate() {
        let first = (((lo - eps - min) / height).floor()).clamp(0.0, last_band) as usize;
        let last = (((hi + eps - min) / height).floor()).clamp(0.0, last_band) as usize;
        for list in &mut lists[first..=last] {
            list.push(index as u32);
        }
    }
    lists
}

/// One band's membership, ordered for both ray directions, plus the
/// coordinate that decides which direction a sample fires in.
struct Band {
    /// Members ordered by *decreasing* maximum coordinate along the ray axis.
    /// A ray fired in the +axis direction meets them far end first, so the
    /// first curve whose maximum has dropped behind the sample's antialiasing
    /// window ends the loop: every curve after it is further behind still.
    descending: Vec<u32>,
    /// The same members ordered by *increasing* minimum coordinate, for the
    /// rays the median split fires in the -axis direction.
    ascending: Vec<u32>,
    /// Median of the members' ray-axis midpoints; samples past it fire
    /// backwards.
    split: f32,
}

/// The bands along one axis of `[min, max]`, with each band's list sorted for
/// the shader's early-out and its median split computed.
///
/// The sort keys span both masters (see [`spans`]), which is what keeps the
/// early-out conservative at every `weight_t`: the shader compares the same
/// two-master extreme, and the blended outline can only sit inside it. Sorting
/// on master A alone would let a blend toward B put a curve that still crosses
/// the ray behind the curve that ends the loop.
fn bands(records: &[f32], b_records: Option<&[f32]>, axis: usize, min: f32, max: f32) -> Vec<Band> {
    // Rays run across the banding axis: y-bands are crossed by horizontal
    // rays, x-bands by vertical ones.
    let spans = spans(records, b_records, 1 - axis);
    band_lists(records, b_records, axis, min, max)
        .into_iter()
        .map(|members| {
            let mut descending = members.clone();
            descending.sort_by(|a, b| spans[*b as usize].1.total_cmp(&spans[*a as usize].1));
            let mut ascending = members;
            ascending.sort_by(|a, b| spans[*a as usize].0.total_cmp(&spans[*b as usize].0));
            Band {
                split: median_split(&ascending, &spans),
                descending,
                ascending,
            }
        })
        .collect()
}

/// Where to split a band: the median of its members' ray-axis midpoints. A
/// sample sitting there has about half the band's curves ahead of it and half
/// behind, so the early-out fires after a similar number of fetches whichever
/// way the ray goes. An empty band never reaches the shader, and 0.0 is as
/// good a coordinate as any for it.
fn median_split(members: &[u32], spans: &[(f32, f32)]) -> f32 {
    if members.is_empty() {
        return 0.0;
    }
    let mut mids: Vec<f32> = members
        .iter()
        .map(|&i| (spans[i as usize].0 + spans[i as usize].1) * 0.5)
        .collect();
    mids.sort_by(f32::total_cmp);
    // Rounded here rather than on the way into the texture, so the coordinate
    // the shader compares a sample against is the one this file reasoned
    // about. Rounding is monotone, so a split inside its band's extremes —
    // themselves already f16 — stays inside them.
    quantize(mids[mids.len() / 2])
}

/// A coordinate as the texture will hold it. Everything downstream of the
/// flattener works in these, so band membership, the early-out sort keys and
/// the shader all see one set of numbers.
fn quantize(v: f32) -> f32 {
    f16::from_f32(v).to_f32()
}

/// Texels one master's records occupy in a banded block: one per curve, one
/// per contour terminator, rounded up so the region — and therefore the block
/// and the next master's region — stays even.
fn shared_texels(count: usize, contours: usize) -> u32 {
    (count + contours).next_multiple_of(TEXELS_PER_CURVE) as u32
}

/// Master A's records followed by master B's, unshared: two texels a curve,
/// which is what the flat path's `i * TEXELS_PER_CURVE` addressing wants.
fn flat_block(records: &[f32], b_records: Option<&[f32]>) -> Vec<f32> {
    let mut block = records.to_vec();
    if let Some(b) = b_records {
        block.extend_from_slice(b);
    }
    block
}

/// One master's records with shared endpoints: a contour of n curves becomes
/// n + 1 texels, [p0.xy p1.xy] per curve plus a terminator holding the last
/// curve's p2. Returns the lanes (padded to [`shared_texels`]) and each
/// curve's texel offset within the region.
///
/// The next curve's texel already holds this curve's p2 in its first two
/// lanes, which is what makes the shader's unchanged two-texel fetch land on
/// the right point.
fn shared_records(records: &[f32], contours: &[u32]) -> (Vec<f32>, Vec<u32>) {
    let count = records.len() / FLOATS_PER_CURVE;
    let mut lanes = Vec::with_capacity((count + contours.len()) * FLOATS_PER_TEXEL);
    let mut texel_of = Vec::with_capacity(count);
    let mut curve = 0;
    for &n in contours {
        let n = n as usize;
        debug_assert!(n > 0, "the flattener never emits an empty contour");
        for j in 0..n {
            let record = &records[(curve + j) * FLOATS_PER_CURVE..];
            debug_assert!(
                j == 0 || record[..2] == records[(curve + j - 1) * FLOATS_PER_CURVE + 4..][..2],
                "endpoint sharing needs p2 of a curve to be p0 of the next"
            );
            texel_of.push((lanes.len() / FLOATS_PER_TEXEL) as u32);
            lanes.extend_from_slice(&record[..FLOATS_PER_TEXEL]);
        }
        // The terminator: the contour's last p2, which endpoint sharing has
        // nowhere else to put (and which is the contour's first point again —
        // contours close).
        let last = &records[(curve + n - 1) * FLOATS_PER_CURVE..];
        lanes.extend_from_slice(&[last[4], last[5], 0.0, 0.0]);
        curve += n;
    }
    lanes.resize(
        shared_texels(count, contours.len()) as usize * FLOATS_PER_TEXEL,
        0.0,
    );
    (lanes, texel_of)
}

/// Lay out a banded block: header, index lists, then master A's shared-endpoint
/// curve texels and master B's parallel copy. See the record-layout comment at
/// the top of this file.
///
/// `None` when the block would encode a texel offset past [`F16_EXACT_INT`],
/// where an f16 lane stops holding integers exactly — the caller falls back to
/// the flat layout, which stores no integers at all. A curve costs ~1.1 texels
/// of records and ~2.5 of index lists, so it takes on the order of 550 curves
/// in a single glyph to get there.
fn banded_block(
    records: &[f32],
    b_records: Option<&[f32]>,
    contours: &[u32],
    bbox: [f32; 4],
) -> Option<Vec<f32>> {
    let mut table = bands(records, b_records, 1, bbox[1], bbox[3]);
    table.extend(bands(records, b_records, 0, bbox[0], bbox[2]));

    // Two lists per band — same members, opposite orders. Each starts on a
    // texel boundary (the shader walks them four at a time) and the region as
    // a whole is padded to an even texel count, which keeps the records that
    // follow two-texel aligned.
    let list_texels: usize = table
        .iter()
        .map(|band| 2 * band.descending.len().div_ceil(FLOATS_PER_TEXEL))
        .sum::<usize>()
        .next_multiple_of(TEXELS_PER_CURVE);
    let records_offset = HEADER_TEXELS + list_texels;

    let (a_lanes, texel_of) = shared_records(records, contours);
    // The largest integer any lane of this block would hold: the offset of the
    // last curve texel in master A's region. Everything else — header offsets,
    // band member counts, the other list entries — is smaller.
    if records_offset + a_lanes.len() / FLOATS_PER_TEXEL > F16_EXACT_INT {
        return None;
    }

    let mut block = Vec::with_capacity(records_offset * FLOATS_PER_TEXEL + a_lanes.len());
    let mut offset = HEADER_TEXELS;
    for band in &table {
        let texels = band.descending.len().div_ceil(FLOATS_PER_TEXEL);
        block.push(offset as f32);
        block.push(band.descending.len() as f32);
        block.push(band.split);
        block.push((offset + texels) as f32);
        offset += 2 * texels;
    }
    debug_assert_eq!(block.len(), HEADER_TEXELS * FLOATS_PER_TEXEL);
    for band in &table {
        for list in [&band.descending, &band.ascending] {
            for &index in list {
                block.push((records_offset + texel_of[index as usize] as usize) as f32);
            }
            block.resize(block.len().next_multiple_of(FLOATS_PER_TEXEL), 0.0);
        }
    }
    block.resize(records_offset * FLOATS_PER_TEXEL, 0.0);
    block.extend_from_slice(&a_lanes);
    if let Some(b) = b_records {
        // Same commands, so the same contours: master B packs to the same
        // texels, which is what keeps the A→B stride a single constant.
        let (b_lanes, b_texel_of) = shared_records(b, contours);
        debug_assert_eq!(b_texel_of, texel_of);
        block.extend_from_slice(&b_lanes);
    }
    Some(block)
}

/// Flattens a zeno path into padded quadratic records, tracking the bbox and
/// the curve count of each contour.
struct Flattener<'a> {
    out: &'a mut Vec<f32>,
    start: [f32; 2],
    current: [f32; 2],
    open: bool,
    bbox: [f32; 4],
    /// Curves per contour, in emission order.
    contours: Vec<u32>,
    /// Curves pushed since the last contour ended.
    in_contour: u32,
}

impl<'a> Flattener<'a> {
    fn new(out: &'a mut Vec<f32>) -> Self {
        Self {
            out,
            start: [0.0; 2],
            current: [0.0; 2],
            open: false,
            bbox: [f32::MAX, f32::MAX, f32::MIN, f32::MIN],
            contours: Vec::new(),
            in_contour: 0,
        }
    }

    /// Control points are rounded to f16 here — the one place a coordinate
    /// enters the pipeline — so the bbox, band membership, the sort keys and
    /// the texture all describe the same outline. Rounding is a function of the
    /// value, so a point carried from one record's p2 to the next record's p0
    /// rounds to the same number in both, and endpoint sharing stays exact.
    fn push(&mut self, p0: [f32; 2], p1: [f32; 2], p2: [f32; 2]) {
        let points = [p0, p1, p2].map(|p| [quantize(p[0]), quantize(p[1])]);
        for p in points {
            self.bbox[0] = self.bbox[0].min(p[0]);
            self.bbox[1] = self.bbox[1].min(p[1]);
            self.bbox[2] = self.bbox[2].max(p[0]);
            self.bbox[3] = self.bbox[3].max(p[1]);
        }
        let [p0, p1, p2] = points;
        self.out
            .extend_from_slice(&[p0[0], p0[1], p1[0], p1[1], p2[0], p2[1], 0.0, 0.0]);
        self.in_contour += 1;
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
        if self.in_contour > 0 {
            self.contours.push(self.in_contour);
            self.in_contour = 0;
        }
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

    /// A glyph extracted after a flush, small enough to land inside the
    /// texture's current partial row, must still be uploaded by the next
    /// flush. A row-granular high-water mark skipped it forever: the store
    /// held the curves, the texture never saw them, and the glyph rendered
    /// as nothing from then on.
    #[test]
    fn flush_uploads_glyphs_that_fit_inside_a_partial_row() {
        let (device, queue) = testing::gpu();
        let mut font_system = testing::font_system();
        let font_id = testing::font_id(&font_system);
        let mut store = CurveStore::new(device);

        let a = get(&mut store, &mut font_system, font_id, 20).expect("glyph 20");
        store.flush(queue);
        let after_first = store.uploaded_floats;
        assert_eq!(after_first, store.data.len());

        // A second small glyph: with 256-texel rows this stays in the same
        // partial row the first flush already touched.
        let b = get(&mut store, &mut font_system, font_id, 21).expect("glyph 21");
        assert!(
            (b.first as usize * FLOATS_PER_TEXEL)
                < after_first.next_multiple_of(floats_per_row(store.width)),
            "repro requires the new glyph to start inside the flushed partial row"
        );
        store.flush(queue);
        assert_eq!(
            store.uploaded_floats,
            store.data.len(),
            "flush must upload data appended within a partial row"
        );
        assert_ne!(a.first, b.first);
    }

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

    /// The glyph's whole block — band tables included — decoded back to f32,
    /// which is what the shader's `textureLoad` hands its own math.
    fn slice_of(store: &CurveStore, curves: &GlyphCurves) -> Vec<f32> {
        let start = curves.first as usize * FLOATS_PER_TEXEL;
        let end = start + curves.texels as usize * FLOATS_PER_TEXEL;
        store.data[start..end].iter().map(|v| v.to_f32()).collect()
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
        assert_eq!(
            store.uploaded_floats, 0,
            "growth schedules a full re-upload"
        );
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
        assert_eq!(store.uploaded_floats, 0, "compaction re-uploads everything");
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

    /// One band as the shader reads it back out of a block.
    struct DecodedBand {
        /// Members in the order the forward ray walks them.
        descending: Vec<u32>,
        /// Members in the order the backward ray walks them.
        ascending: Vec<u32>,
        split: f32,
    }

    /// A banded block, decoded.
    struct DecodedBlock {
        bands: Vec<DecodedBand>,
        /// Curve index → texel offset of its first texel in the block.
        texel_of: Vec<u32>,
        /// First texel of master A's records.
        records_offset: usize,
    }

    /// Decode a banded block the way the shader does: header entry per band,
    /// its two index lists, curve texels.
    ///
    /// Endpoint sharing makes a curve's texel offset depend on how many
    /// contours came before it, so curve *indices* are recovered from the
    /// lists themselves: every curve overlaps at least one band, and the
    /// records are packed in curve order, so sorting the union of every list
    /// numbers the curves.
    fn decode_bands(block: &[f32], curves: &GlyphCurves) -> DecodedBlock {
        let masters = if curves.dual_master { 2 } else { 1 };
        let records_offset =
            block.len() / FLOATS_PER_TEXEL - masters * curves.record_texels as usize;
        let bbox = curves.bbox;

        let mut decoded = Vec::new();
        let mut texels = Vec::new();
        for slot in 0..2 * BANDS {
            let entry = &block[slot * FLOATS_PER_TEXEL..][..FLOATS_PER_TEXEL];
            let len = entry[1] as usize;
            let axis = if slot < BANDS { 1 } else { 0 };
            let band = slot % BANDS;
            let height = (bbox[axis + 2] - bbox[axis]) / BANDS as f32;
            let (lo, hi) = (
                bbox[axis] + band as f32 * height,
                bbox[axis] + (band as f32 + 1.0) * height,
            );
            let eps = height * BAND_EPSILON;

            let mut lists = Vec::new();
            for list in [entry[0] as usize, entry[3] as usize] {
                assert!(list >= HEADER_TEXELS && list + len.div_ceil(4) <= records_offset);
                let mut entries = Vec::new();
                for i in 0..len {
                    let texel = block[list * FLOATS_PER_TEXEL + i] as usize;
                    assert!(texel >= records_offset, "list entry points into the header");
                    assert!(
                        texel + 1 < records_offset + curves.record_texels as usize,
                        "a curve's second texel must stay inside master A"
                    );
                    // Membership spans both masters, so a curve master A puts
                    // outside the band can still be listed — a blend toward B
                    // may pull it in.
                    let (mut curve_lo, mut curve_hi) = span_at(block, texel, axis);
                    if curves.dual_master {
                        let twin = span_at(block, texel + curves.record_texels as usize, axis);
                        curve_lo = curve_lo.min(twin.0);
                        curve_hi = curve_hi.max(twin.1);
                    }
                    assert!(
                        curve_hi >= lo - eps && curve_lo <= hi + eps,
                        "band {slot} lists a curve it does not overlap"
                    );
                    entries.push(texel as u32);
                    texels.push(texel as u32);
                }
                lists.push(entries);
            }
            decoded.push((lists, entry[2]));
        }

        texels.sort_unstable();
        texels.dedup();
        assert_eq!(
            texels.len(),
            curves.count as usize,
            "every curve should appear in some band"
        );
        let index_of = |texel: u32| texels.binary_search(&texel).unwrap() as u32;
        let bands = decoded
            .into_iter()
            .map(|(lists, split)| DecodedBand {
                descending: lists[0].iter().map(|&t| index_of(t)).collect(),
                ascending: lists[1].iter().map(|&t| index_of(t)).collect(),
                split,
            })
            .collect();
        DecodedBlock {
            bands,
            texel_of: texels.iter().map(|t| t - records_offset as u32).collect(),
            records_offset,
        }
    }

    /// The unshared 8-lane records a region packs, in curve order — the three
    /// points the shader reads for each curve, which is what the layout
    /// helpers in this file take.
    fn records_at(block: &[f32], base: usize, texel_of: &[u32]) -> Vec<f32> {
        let mut records = Vec::with_capacity(texel_of.len() * FLOATS_PER_CURVE);
        for &texel in texel_of {
            let start = (base + texel as usize) * FLOATS_PER_TEXEL;
            records.extend_from_slice(&block[start..start + 6]);
            records.extend_from_slice(&[0.0, 0.0]);
        }
        records
    }

    /// A curve's control-point span along `axis`, read at a block texel.
    fn span_at(block: &[f32], texel: usize, axis: usize) -> (f32, f32) {
        control_span(&block[texel * FLOATS_PER_TEXEL..], axis)
    }

    /// A curve's control-point span along `axis`, straight out of a record.
    fn record_span(records: &[f32], index: u32, axis: usize) -> (f32, f32) {
        control_span(&records[index as usize * FLOATS_PER_CURVE..], axis)
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
            "a flat block is nothing but unshared records"
        );

        assert!(banded.count > BAND_MIN_CURVES);
        assert_eq!(banded.instance_count(), banded.count | BANDED_FLAG);
        assert_eq!(banded.first % 2, 0, "records must not straddle a row");
        assert_eq!(banded.texels % 2, 0);
        assert!(
            banded.record_texels < banded.count * TEXELS_PER_CURVE as u32,
            "endpoint sharing must beat the unshared record"
        );

        // Every band resolves to curves it overlaps (checked in decode_bands),
        // and no curve that overlaps a band is missing from either of its two
        // lists.
        let block = slice_of(&store, &banded);
        let decoded = decode_bands(&block, &banded);
        let records = records_at(&block, decoded.records_offset, &decoded.texel_of);
        for (slot, band) in decoded.bands.iter().enumerate() {
            let axis = if slot < BANDS { 1 } else { 0 };
            let expected = &band_lists(
                &records,
                None,
                axis,
                banded.bbox[axis],
                banded.bbox[axis + 2],
            )[slot % BANDS];
            for list in [&band.descending, &band.ascending] {
                let mut sorted = list.clone();
                sorted.sort_unstable();
                assert_eq!(&sorted, expected, "band {slot} lost curves");
            }
        }
        assert!(
            decoded
                .bands
                .iter()
                .any(|band| band.descending.len() < banded.count as usize),
            "banding should cut some band's loop below the full curve set"
        );
    }

    /// The property the shader's early-out rests on: walking a band's list
    /// forwards, a curve's maximum along the ray axis never rises again, so
    /// the first curve whose maximum has fallen behind the sample proves every
    /// curve after it has too (and the mirror image for the backward ray).
    #[test]
    fn band_lists_are_sorted_by_the_ray_axis_extremes_the_shader_breaks_on() {
        let (device, _) = testing::gpu();
        let mut font_system = testing::font_system();
        let font_id = testing::font_id(&font_system);
        let mut store = CurveStore::new(device);
        store.begin_frame(1);

        let banded = (36..200)
            .filter_map(|glyph_id| get(&mut store, &mut font_system, font_id, glyph_id))
            .find(|c| c.banded)
            .expect("some glyph exceeds the banding threshold");
        let block = slice_of(&store, &banded);
        let decoded = decode_bands(&block, &banded);
        let records = records_at(&block, decoded.records_offset, &decoded.texel_of);

        let mut split_inside = 0;
        for (slot, band) in decoded.bands.iter().enumerate() {
            // Rays run across the banding axis: y-bands are crossed by
            // horizontal rays, x-bands by vertical ones.
            let ray = if slot < BANDS { 0 } else { 1 };
            for pair in band.descending.windows(2) {
                let (a, b) = (
                    record_span(&records, pair[0], ray).1,
                    record_span(&records, pair[1], ray).1,
                );
                assert!(a >= b, "band {slot} is not descending by maximum");
            }
            for pair in band.ascending.windows(2) {
                let (a, b) = (
                    record_span(&records, pair[0], ray).0,
                    record_span(&records, pair[1], ray).0,
                );
                assert!(a <= b, "band {slot} is not ascending by minimum");
            }
            // The split is a coordinate the band's own curves reach, and it
            // leaves work on both sides: neither direction is a no-op.
            if let (Some(&first), Some(&last)) = (band.ascending.first(), band.descending.first()) {
                let lo = record_span(&records, first, ray).0;
                let hi = record_span(&records, last, ray).1;
                assert!(
                    band.split >= lo && band.split <= hi,
                    "band {slot} split {} is outside [{lo}, {hi}]",
                    band.split
                );
                split_inside += 1;
            }
        }
        assert!(split_inside > 0, "the glyph should have populated bands");
    }

    /// Endpoint sharing is what makes a curve one texel instead of two, and
    /// the shader only reads a curve's p2 out of the *next* texel, so the
    /// packing has to put it there: consecutive curves of a contour are
    /// consecutive texels, and the texel after a contour's last curve repeats
    /// the point that contour started from.
    #[test]
    fn shared_curve_texels_hand_p2_to_the_next_texel_and_close_each_contour() {
        let (device, _) = testing::gpu();
        let mut font_system = testing::font_system();
        let font_id = testing::font_id(&font_system);
        let mut store = CurveStore::new(device);
        store.begin_frame(1);

        let mut multi_contour = 0;
        let mut banded_glyphs = 0;
        for glyph_id in 36..200 {
            let Some(banded) = get(&mut store, &mut font_system, font_id, glyph_id) else {
                continue;
            };
            if !banded.banded {
                continue;
            }
            banded_glyphs += 1;
            let block = slice_of(&store, &banded);
            let decoded = decode_bands(&block, &banded);
            let contours = check_shared_region(&block, &banded, &decoded);
            if contours > 1 {
                multi_contour += 1;
            }
        }
        assert!(
            banded_glyphs > 0,
            "some glyph exceeds the banding threshold"
        );
        assert!(
            multi_contour > 0,
            "some glyph has a counter to share around"
        );
    }

    /// The whole shared-endpoint invariant for one glyph, returning its contour
    /// count.
    fn check_shared_region(block: &[f32], banded: &GlyphCurves, decoded: &DecodedBlock) -> usize {
        let point = |texel: u32| {
            let start = (decoded.records_offset + texel as usize) * FLOATS_PER_TEXEL;
            [block[start], block[start + 1]]
        };
        let p2_of = |texel: u32| {
            let start = (decoded.records_offset + texel as usize) * FLOATS_PER_TEXEL;
            [block[start + 4], block[start + 5]]
        };

        let mut contours = 1;
        let mut contour_start = decoded.texel_of[0];
        assert_eq!(contour_start, 0, "the first contour starts the region");
        for pair in decoded.texel_of.windows(2) {
            let (here, next) = (pair[0], pair[1]);
            assert_eq!(
                p2_of(here),
                point(here + 1),
                "curve {here}'s p2 must be the texel the shader reads next"
            );
            match next - here {
                // Same contour: the next curve *is* this curve's p2 texel.
                1 => {}
                // A terminator sits between them, holding the closing point.
                2 => {
                    assert_eq!(
                        point(here + 1),
                        point(contour_start),
                        "a contour's terminator repeats its first point"
                    );
                    contours += 1;
                    contour_start = next;
                }
                gap => panic!("curves are one or two texels apart, not {gap}"),
            }
        }
        let last = *decoded.texel_of.last().unwrap();
        assert_eq!(
            p2_of(last),
            point(contour_start),
            "the last contour closes too"
        );
        assert_eq!(
            banded.record_texels,
            shared_texels(banded.count as usize, contours),
            "a texel per curve, a texel per contour"
        );
        contours
    }

    /// Every integer a banded block puts in a texel — list offsets, member
    /// counts, curve texel offsets — has to survive f16, which stops counting
    /// exactly at 2048. Real fonts are nowhere near it; the assertion is what
    /// keeps the fallback in `banded_block` honest.
    #[test]
    fn banded_blocks_only_encode_integers_f16_holds_exactly() {
        let (device, _) = testing::gpu();
        let mut biggest = 0.0f32;
        for (mut font_system, family) in [
            (testing::font_system(), testing::STATIC_FAMILY),
            (testing::variable_font_system(), testing::VARIABLE_FAMILY),
        ] {
            let font_id = testing::font_id_of(&font_system, family);
            let mut store = CurveStore::new(device);
            store.begin_frame(1);
            for glyph_id in 1..400 {
                let Some(curves) = get(&mut store, &mut font_system, font_id, glyph_id) else {
                    continue;
                };
                if !curves.banded {
                    continue;
                }
                let block = slice_of(&store, &curves);
                let decoded = decode_bands(&block, &curves);
                let mut integers: Vec<f32> = Vec::new();
                for slot in 0..2 * BANDS {
                    let entry = &block[slot * FLOATS_PER_TEXEL..][..FLOATS_PER_TEXEL];
                    integers.extend([entry[0], entry[1], entry[3]]);
                }
                integers.extend(
                    decoded
                        .texel_of
                        .iter()
                        .map(|t| (decoded.records_offset + *t as usize) as f32),
                );
                for value in integers {
                    assert_eq!(
                        value,
                        f16::from_f32(value).to_f32(),
                        "{value} does not survive the texture"
                    );
                    assert!(value <= F16_EXACT_INT as f32);
                    biggest = biggest.max(value);
                }
            }
        }
        println!("largest integer encoded in a block: {biggest}");
        assert!(biggest > 0.0, "some glyph should have been banded");
    }

    /// The escape hatch: a glyph with more curves than f16 can address falls
    /// back to the flat layout rather than encoding an offset it cannot hold.
    #[test]
    fn a_glyph_too_big_to_address_in_f16_gives_up_its_band_tables() {
        // Curves strung along the diagonal, each in its own contour, so every
        // one lands in a single band on both axes and the block grows with the
        // record region rather than with the lists.
        let ranges: Vec<([f32; 2], [f32; 2])> = (0..2100)
            .map(|i| {
                let t = i as f32 / 2100.0;
                ([t, t + 0.001], [t, t + 0.001])
            })
            .collect();
        let all = records_spanning(&ranges);
        let contours = vec![1u32; ranges.len()];

        let fits = ranges.len() / 4;
        assert!(
            banded_block(
                &all[..fits * FLOATS_PER_CURVE],
                None,
                &contours[..fits],
                [0.0, 0.0, 1.0, 1.0]
            )
            .is_some(),
            "a few hundred curves address fine"
        );
        assert!(
            banded_block(&all, None, &contours, [0.0, 0.0, 1.0, 1.0]).is_none(),
            "past 2048 texels the block cannot be banded"
        );
    }

    /// Control points are rounded to f16 by the flattener, not on the way to
    /// the GPU, so what the CPU bands and sorts is what the shader reads — and
    /// rounding a point that two records share leaves it shared.
    #[test]
    fn flattened_control_points_are_already_f16_and_stay_shared() {
        let (device, _) = testing::gpu();
        let mut font_system = testing::font_system();
        let font_id = testing::font_id(&font_system);
        let mut store = CurveStore::new(device);

        let mut checked = 0;
        for glyph_id in 36..200 {
            let Some(commands) = store.outline(
                &mut font_system,
                font_id,
                glyph_id,
                fontdb::Weight::NORMAL,
                CacheKeyFlags::empty(),
            ) else {
                continue;
            };
            let (records, contours, bbox) = flatten(&commands);
            for value in records.iter().chain(bbox.iter()) {
                assert_eq!(*value, f16::from_f32(*value).to_f32(), "not an f16 value");
            }
            let mut curve = 0;
            for n in contours {
                for j in 1..n as usize {
                    let previous = &records[(curve + j - 1) * FLOATS_PER_CURVE..];
                    let next = &records[(curve + j) * FLOATS_PER_CURVE..];
                    assert_eq!(
                        previous[4..6],
                        next[0..2],
                        "p2 of a curve is p0 of the next"
                    );
                    checked += 1;
                }
                curve += n as usize;
            }
        }
        assert!(checked > 100, "the font should have supplied contours");
    }

    #[test]
    fn shader_constants_match_the_record_layout() {
        let src = include_str!("shaders.wgsl");
        for expected in [
            format!("const CURVE_TEX_WIDTH: u32 = {CURVE_TEX_WIDTH}u;"),
            format!("const BANDS: u32 = {BANDS}u;"),
            format!("const BAND_ENTRY_TEXELS: u32 = {BAND_ENTRY_TEXELS}u;"),
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

    /// A glyph's block split into (band tables, master A's records region,
    /// master B's). B is empty for a single-master glyph. The regions are raw
    /// lanes — unshared 8-lane records in a flat block, shared curve texels in
    /// a banded one.
    fn masters(store: &CurveStore, curves: &GlyphCurves) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
        let block = slice_of(store, curves);
        let region = curves.record_texels as usize * FLOATS_PER_TEXEL;
        let masters = if curves.dual_master { 2 } else { 1 };
        let head = block.len() - masters * region;
        (
            block[..head].to_vec(),
            block[head..head + region].to_vec(),
            block[head + region..].to_vec(),
        )
    }

    /// Every control point a records region holds. Lanes come in (x, y) pairs;
    /// a record's padding and a contour terminator's second pair are zeros,
    /// and a genuine control point at the origin costs nothing to skip.
    fn points_in(region: &[f32]) -> Vec<[f32; 2]> {
        region
            .chunks_exact(2)
            .map(|p| [p[0], p[1]])
            .filter(|p| *p != [0.0, 0.0])
            .collect()
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
        assert!(!curves.dual_master);
        assert_eq!(curves.b_first(), 0, "the shader must skip the second fetch");
        assert_eq!(curves.weight_t, 0.0);
        let (_, a, b) = masters(&store, &curves);
        assert!(b.is_empty(), "no second master to store");
        assert_eq!(a.len(), curves.record_texels as usize * FLOATS_PER_TEXEL);
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
            // constant offset takes the shader from any record to its twin —
            // whether that region is unshared records or shared curve texels.
            assert!(curves.dual_master);
            if curves.banded {
                assert!(
                    (curves.count + 1..=curves.count * TEXELS_PER_CURVE as u32)
                        .contains(&curves.record_texels),
                    "a shared region is a texel per curve plus one per contour"
                );
            } else {
                assert_eq!(
                    curves.record_texels as usize,
                    curves.count as usize * TEXELS_PER_CURVE
                );
            }
            assert_eq!(curves.b_first(), curves.first + curves.record_texels);
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
            for point in points_in(&a).into_iter().chain(points_in(&b)) {
                assert!(point[0] >= curves.bbox[0] && point[0] <= curves.bbox[2]);
                assert!(point[1] >= curves.bbox[1] && point[1] <= curves.bbox[3]);
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
        let decoded = decode_bands(&block, &banded);
        let records_offset = decoded.records_offset;
        let a_texels = banded.record_texels as usize;
        // The two masters, unpacked back into the three-point records the
        // layout helpers reason about.
        let a = records_at(&block, records_offset, &decoded.texel_of);
        let b = records_at(&block, records_offset + a_texels, &decoded.texel_of);

        for slot in 0..2 * BANDS {
            let entry = &block[slot * FLOATS_PER_TEXEL..][..FLOATS_PER_TEXEL];
            let len = entry[1] as usize;
            for list in [entry[0] as usize, entry[3] as usize] {
                for i in 0..len {
                    let texel = block[list * FLOATS_PER_TEXEL + i] as usize;
                    assert!(
                        (records_offset..records_offset + a_texels).contains(&texel),
                        "band {slot} points outside master A's records"
                    );
                }
            }
        }
        // Membership is decided over both masters, so a blended curve can
        // never cross a ray in a band that left it out.
        for (slot, list) in band_lists(&a, Some(&b), 1, banded.bbox[1], banded.bbox[3])
            .iter()
            .enumerate()
        {
            let expected = block[slot * FLOATS_PER_TEXEL + 1] as usize;
            assert_eq!(list.len(), expected, "y-band {slot} lost curves");
        }

        // …and the sort keys span both masters too. The shader's early-out
        // compares the same two-master extreme, so a list ordered by master A
        // alone could break off in front of a curve that a blend toward B has
        // pushed back across the ray.
        let ray_spans = spans(&a, Some(&b), 0);
        for (slot, band) in bands(&a, Some(&b), 1, banded.bbox[1], banded.bbox[3])
            .iter()
            .enumerate()
        {
            for pair in band.descending.windows(2) {
                assert!(
                    ray_spans[pair[0] as usize].1 >= ray_spans[pair[1] as usize].1,
                    "y-band {slot} is not descending by the two-master maximum"
                );
            }
            for pair in band.ascending.windows(2) {
                assert!(
                    ray_spans[pair[0] as usize].0 <= ray_spans[pair[1] as usize].0,
                    "y-band {slot} is not ascending by the two-master minimum"
                );
            }
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
