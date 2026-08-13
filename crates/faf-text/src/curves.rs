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
// here, so band tables (#3) and master interleaving (#8) can change it in one
// place.

/// Two RGBA32F texels (8 floats) per quadratic: [p0.xy p1.xy] [p2.xy pad pad].
const FLOATS_PER_CURVE: usize = 8;

/// Floats in one texture row.
const fn floats_per_row(width: u32) -> usize {
    width as usize * 4
}

/// How many curve records fit in a `width × height` texture.
const fn curve_capacity(width: u32, height: u32) -> usize {
    (width as usize * height as usize) * 4 / FLOATS_PER_CURVE
}

/// A glyph's quadratic Bézier set inside the curve texture, in em units
/// (y-up, origin at the glyph's baseline origin).
#[derive(Clone, Copy, Debug)]
pub struct GlyphCurves {
    pub first: u32,
    pub count: u32,
    /// Em-space bounds: [min_x, min_y, max_x, max_y].
    pub bbox: [f32; 4],
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct GlyphKey {
    font_id: fontdb::ID,
    glyph_id: u16,
    weight: u16,
    flags: CacheKeyFlags,
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
            device: device.clone(),
            data: Vec::new(),
            entries: FxHashMap::default(),
            swash: SwashCache::new(),
            uploaded_rows: 0,
            width,
            height,
            max_height,
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

    fn extract(
        &mut self,
        font_system: &mut FontSystem,
        font_id: fontdb::ID,
        glyph_id: u16,
        weight: fontdb::Weight,
        flags: CacheKeyFlags,
    ) -> Extracted {
        // Size 1.0 with hinting off yields outlines in pure em units.
        let (cache_key, _, _) = CacheKey::new(
            font_id,
            glyph_id,
            1.0,
            (0.0, 0.0),
            weight,
            flags | CacheKeyFlags::DISABLE_HINTING,
        );
        let Some(commands) = self
            .swash
            .get_outline_commands_uncached(font_system, cache_key)
        else {
            return Extracted::Done(None);
        };

        let first = (self.data.len() / FLOATS_PER_CURVE) as u32;
        let mut flat = Flattener::new(&mut self.data);
        for command in commands.iter() {
            match *command {
                Command::MoveTo(p) => flat.move_to([p.x, p.y]),
                Command::LineTo(p) => flat.line_to([p.x, p.y]),
                Command::QuadTo(c, p) => flat.quad_to([c.x, c.y], [p.x, p.y]),
                Command::CurveTo(c1, c2, p) => {
                    flat.cubic_to([c1.x, c1.y], [c2.x, c2.y], [p.x, p.y])
                }
                Command::Close => flat.close(),
            }
        }
        flat.close();
        let bbox = flat.bbox;

        let count = (self.data.len() / FLOATS_PER_CURVE) as u32 - first;
        if count == 0 {
            return Extracted::Done(Some(GlyphCurves {
                first,
                count: 0,
                bbox: [0.0; 4],
            }));
        }
        let used = self.data.len() / FLOATS_PER_CURVE;
        if used > self.capacity() && !self.grow_to_fit(used) {
            self.data.truncate(first as usize * FLOATS_PER_CURVE);
            return Extracted::Overflow;
        }
        Extracted::Done(Some(GlyphCurves { first, count, bbox }))
    }

    fn capacity(&self) -> usize {
        curve_capacity(self.width, self.height)
    }

    /// Double the texture height (repeatedly, up to the cap) until `needed`
    /// curves fit, then re-create it and schedule a full re-upload from the
    /// CPU mirror. Returns false — leaving the texture untouched — if the cap
    /// is too small.
    fn grow_to_fit(&mut self, needed: usize) -> bool {
        let mut height = self.height;
        while height < self.max_height && curve_capacity(self.width, height) < needed {
            height = (height * 2).min(self.max_height);
        }
        if curve_capacity(self.width, height) < needed {
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
    fn compact(&mut self) {
        self.needs_compact = false;
        let mut order: Vec<(u64, GlyphKey)> = self
            .entries
            .iter()
            .map(|(k, e)| (e.last_used, *k))
            .collect();
        order.sort_unstable_by_key(|(last_used, _)| *last_used);
        for (_, key) in &order[..order.len() / 2] {
            self.entries.remove(key);
        }

        let Self { entries, data, .. } = self;
        let mut packed = Vec::with_capacity(data.len());
        for entry in entries.values_mut() {
            let Some(curves) = entry.curves.as_mut() else {
                continue;
            };
            if curves.count == 0 {
                curves.first = 0;
                continue;
            }
            let start = curves.first as usize * FLOATS_PER_CURVE;
            let end = start + curves.count as usize * FLOATS_PER_CURVE;
            curves.first = (packed.len() / FLOATS_PER_CURVE) as u32;
            packed.extend_from_slice(&data[start..end]);
        }
        self.data = packed;
        self.uploaded_rows = 0;
        self.generation += 1;
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

    fn slice_of(store: &CurveStore, curves: &GlyphCurves) -> Vec<f32> {
        let start = curves.first as usize * FLOATS_PER_CURVE;
        let end = start + curves.count as usize * FLOATS_PER_CURVE;
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
            live_floats += curves.count as usize * FLOATS_PER_CURVE;
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
        let cap = curve_capacity(CURVE_TEX_WIDTH, 32);

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
                        (curves.first + curves.count) as usize <= cap,
                        "curve range escaped the texture"
                    );
                }
            }
            assert!(
                store.data.len() / FLOATS_PER_CURVE <= cap,
                "the mirror outgrew the texture"
            );
            assert!(store.height <= 32, "growth respects the cap");
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
}
