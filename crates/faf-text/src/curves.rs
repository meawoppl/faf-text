use cosmic_text::{CacheKey, CacheKeyFlags, Command, FontSystem, SwashCache, fontdb};
use rustc_hash::FxHashMap;

/// Width in texels of the RGBA32F curve data texture. Must be even so a
/// curve's two texels never straddle a row, and must match `CURVE_TEX_WIDTH`
/// in `shaders.wgsl`.
pub const CURVE_TEX_WIDTH: u32 = 256;
pub const CURVE_TEX_HEIGHT: u32 = 2048;
/// Two RGBA32F texels (8 floats) per quadratic: [p0.xy p1.xy] [p2.xy pad pad].
const FLOATS_PER_CURVE: usize = 8;
const MAX_CURVES: usize = (CURVE_TEX_WIDTH * CURVE_TEX_HEIGHT / 2) as usize;

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

/// Glyph outlines flattened to quadratic Béziers and packed into a data
/// texture the fragment shader ray-casts against. Curves are stored once per
/// (font, glyph, weight) in em units and reused at every size — scaling is
/// free and needs no re-rasterization.
pub struct CurveStore {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    /// CPU mirror of the packed curve floats; the un-uploaded tail is flushed
    /// to the texture by `flush`.
    data: Vec<f32>,
    entries: FxHashMap<GlyphKey, Option<GlyphCurves>>,
    swash: SwashCache,
    uploaded_rows: u32,
    exhausted: bool,
}

impl CurveStore {
    pub fn new(device: &wgpu::Device) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("faf-text curve data"),
            size: wgpu::Extent3d {
                width: CURVE_TEX_WIDTH,
                height: CURVE_TEX_HEIGHT,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            texture,
            view,
            data: Vec::new(),
            entries: FxHashMap::default(),
            swash: SwashCache::new(),
            uploaded_rows: 0,
            exhausted: false,
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
        if let Some(entry) = self.entries.get(&key) {
            return *entry;
        }
        let entry = self.extract(font_system, font_id, glyph_id, weight, flags);
        self.entries.insert(key, entry);
        entry
    }

    fn extract(
        &mut self,
        font_system: &mut FontSystem,
        font_id: fontdb::ID,
        glyph_id: u16,
        weight: fontdb::Weight,
        flags: CacheKeyFlags,
    ) -> Option<GlyphCurves> {
        // Size 1.0 with hinting off yields outlines in pure em units.
        let (cache_key, _, _) = CacheKey::new(
            font_id,
            glyph_id,
            1.0,
            (0.0, 0.0),
            weight,
            flags | CacheKeyFlags::DISABLE_HINTING,
        );
        let commands = self
            .swash
            .get_outline_commands_uncached(font_system, cache_key)?;

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
            return Some(GlyphCurves {
                first,
                count: 0,
                bbox: [0.0; 4],
            });
        }
        if self.data.len() / FLOATS_PER_CURVE > MAX_CURVES {
            self.data.truncate(first as usize * FLOATS_PER_CURVE);
            if !self.exhausted {
                self.exhausted = true;
                #[cfg(not(target_arch = "wasm32"))]
                eprintln!("faf-text: curve texture exhausted; new glyphs fall back to atlas");
            }
            return None;
        }
        Some(GlyphCurves { first, count, bbox })
    }

    /// Upload any rows of curve data added since the last flush.
    pub fn flush(&mut self, queue: &wgpu::Queue) {
        let floats_per_row = (CURVE_TEX_WIDTH * 4) as usize;
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
                bytes_per_row: Some(CURVE_TEX_WIDTH * 16),
                rows_per_image: None,
            },
            wgpu::Extent3d {
                width: CURVE_TEX_WIDTH,
                height: total_rows - start_row,
                depth_or_array_layers: 1,
            },
        );
        self.uploaded_rows = total_rows;
    }
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
