//! Introspection into what the renderer actually computes for a glyph.
//!
//! **A debug and tooling surface, not a rendering API.** Documentation
//! figures, tests and explainers want to draw the renderer's real internals —
//! the flattened quadratics, the band tables, the corner clips — rather than
//! plausible-looking fakes. This module runs the *same* code paths production
//! extraction runs (the curve store's flattener, block packer, band builder
//! and clip solver — never a reimplementation) and returns the results as
//! plain owned structs with no GPU types in them.
//!
//! Nothing here is called by the renderer itself, and nothing here touches a
//! GPU: it is the CPU half of extraction, stopped just before upload.
//!
//! ```no_run
//! use faf_text::{fontdb, inspect};
//!
//! let mut fonts = faf_text::font_system_from_fonts(&[faf_text::FONT_DEJAVU_SANS]);
//! let g = inspect::character(&mut fonts, "DejaVu Sans", 'g', fontdb::Weight::NORMAL)
//!     .expect("DejaVu Sans has a 'g'");
//! assert!(g.banded, "'g' has more than 16 curves, so it carries band tables");
//! for q in &g.curves {
//!     // Exactly the control points the fragment shader will read, in em.
//!     let _ = (q.p0, q.p1, q.p2);
//! }
//! ```

use cosmic_text::{CacheKeyFlags, Command, FontSystem, SwashCache, fontdb};

use crate::colr::ColrCache;
use crate::curves::{
    BAND_EPSILON, BAND_MIN_CURVES, BANDS, BuiltBlock, FLOATS_PER_CURVE, FLOATS_PER_TEXEL,
    HEADER_TEXELS, axis_weight, bands, build_block, flatten_with, glyph_outline,
    masters_compatible, spans, weight_blend, wght_axis,
};

/// One quadratic Bézier in em units (y-up, origin at the glyph's baseline
/// origin): start point, control point, end point.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quadratic {
    /// Start point.
    pub p0: [f32; 2],
    /// Control point.
    pub p1: [f32; 2],
    /// End point.
    pub p2: [f32; 2],
}

/// One command of the outline swash hands the flattener, in em units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OutlineCommand {
    /// Start a new contour at a point.
    MoveTo([f32; 2]),
    /// A straight segment.
    LineTo([f32; 2]),
    /// A quadratic Bézier: control point, end point.
    QuadTo([f32; 2], [f32; 2]),
    /// A cubic Bézier: two control points, end point. The flattener splits
    /// each of these into four quadratics.
    CurveTo([f32; 2], [f32; 2], [f32; 2]),
    /// Close the current contour.
    Close,
}

/// One entry of a band's sorted index list: which curve, and the sort key the
/// shader's early-out compares against.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BandEntry {
    /// Index into [`GlyphInspection::curves`] (and `master_b`'s parallel
    /// list — band lists always index master A).
    pub curve: u32,
    /// The entry's sort key: the curve's extreme control-point coordinate
    /// along the ray axis, over *both* masters. Descending lists key on the
    /// maximum, ascending lists on the minimum.
    pub key: f32,
}

/// One band of a banded glyph's table along one axis.
#[derive(Clone, Debug, PartialEq)]
pub struct BandInspection {
    /// The slice of the bbox this band covers along the banding axis, in em:
    /// `[lo, hi]`. Membership is tested against this interval widened by
    /// [`BandTables::epsilon`] of its height.
    pub interval: [f32; 2],
    /// Median of the members' ray-axis midpoints, f16-rounded exactly as
    /// stored: samples past it fire their ray backwards.
    pub split: f32,
    /// Members ordered by decreasing maximum coordinate along the ray axis —
    /// the list a forward (+axis) ray walks.
    pub descending: Vec<BandEntry>,
    /// The same members ordered by increasing minimum — the backward ray's
    /// list.
    pub ascending: Vec<BandEntry>,
}

/// Both axes' band tables of a banded glyph.
///
/// `y` bands are crossed by horizontal rays, `x` bands by vertical ones; the
/// shader picks the axis whose bands are narrower along the ray.
#[derive(Clone, Debug, PartialEq)]
pub struct BandTables {
    /// Bands splitting the bbox's y extent (8 of them, matching the shader).
    pub y: Vec<BandInspection>,
    /// Bands splitting the bbox's x extent.
    pub x: Vec<BandInspection>,
    /// The membership slack, as a fraction of one band's height: a curve joins
    /// every band its control-point range overlaps after widening by this.
    pub epsilon: f32,
}

/// The second master of a glyph from a variable face with a `wght` axis.
#[derive(Clone, Debug, PartialEq)]
pub struct MasterB {
    /// Master B's quadratics as stored (f16-quantized), parallel to
    /// [`GlyphInspection::curves`] index for index.
    pub curves: Vec<Quadratic>,
    /// The same quadratics before f16 rounding.
    pub curves_raw: Vec<Quadratic>,
    /// The `wght` axis minimum (master A's weight), in user units.
    pub axis_min: f32,
    /// The `wght` axis maximum (master B's weight), in user units.
    pub axis_max: f32,
    /// Where the inspected weight falls on the axis: 0 at the minimum, 1 at
    /// the maximum — the blend factor the fragment shader defaults to.
    pub weight_t: f32,
}

/// One layer of a COLRv0 color glyph: its palette color and the full
/// inspection of the ordinary glyph it paints.
#[derive(Clone, Debug, PartialEq)]
pub struct ColorLayerInspection {
    /// The layer's glyph id in the same face.
    pub glyph_id: u16,
    /// Straight RGBA from CPAL palette 0, or `None` for the reserved index
    /// `0xFFFF`, meaning the layer takes the run's text color.
    pub color: Option<[f32; 4]>,
    /// The layer's own inspection — layers are ordinary outline glyphs.
    pub glyph: GlyphInspection,
}

/// Where everything sits inside the glyph's texel block, in texels relative
/// to the block base (which is exactly how the stored offsets are encoded —
/// compaction moves blocks whole and rewrites only the base).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TexelLayout {
    /// Band header texels at the front of the block: `2 * BANDS` entries of
    /// one texel each for a banded block, 0 for a flat one.
    pub header_texels: u32,
    /// Texels the two-per-band sorted index lists occupy (0 for flat).
    pub index_texels: u32,
    /// Base-relative texel where master A's curve records start:
    /// `header_texels + index_texels`.
    pub records_offset: u32,
    /// Texels one master's records occupy — `2 * count` flat (two texels per
    /// curve), `count + contours` rounded up to even when banded (endpoint
    /// sharing). Also the stride from any master-A record to its B twin.
    pub record_texels: u32,
    /// Length of the whole block: what one glyph costs the curve texture.
    pub total_texels: u32,
    /// 1, or 2 when a master-B region follows master A's.
    pub masters: u32,
}

/// Everything the renderer computes for one glyph, stopped just before the
/// texture upload. All coordinates are em units, y-up, baseline origin.
#[derive(Clone, Debug, PartialEq)]
pub struct GlyphInspection {
    /// The face the glyph came from.
    pub font_id: fontdb::ID,
    /// The glyph id inside that face.
    pub glyph_id: u16,
    /// The outline swash extracted (size 1.0, hinting off — pure em), before
    /// flattening. Empty when the glyph has no outline at all (a bitmap emoji,
    /// or a COLR base glyph that only exists as layers).
    pub outline: Vec<OutlineCommand>,
    /// Master A's quadratics exactly as stored: control points rounded to f16
    /// in the flattener, which is the same rounding the bbox, the band tables
    /// and the shader all see.
    pub curves: Vec<Quadratic>,
    /// The same quadratics before the f16 rounding, so quantization deltas
    /// are computable per control point (`curves[i]` pairs with
    /// `curves_raw[i]`).
    pub curves_raw: Vec<Quadratic>,
    /// Curves per contour, in emission order: `curves[0..contours[0]]` is the
    /// first contour, and within a contour `curves[i].p2 == curves[i+1].p0`
    /// (the flattener carries the point, so they are the same float).
    pub contours: Vec<u32>,
    /// Em-space bounds `[min_x, min_y, max_x, max_y]` over both masters — the
    /// extent the instance quad covers and the band tables split.
    pub bbox: [f32; 4],
    /// Whether the block carries band tables (more than 16 curves, and every
    /// stored offset within f16's exact-integer range).
    pub banded: bool,
    /// The band tables, present exactly when `banded`.
    pub bands: Option<BandTables>,
    /// How deep each bbox corner is cut away by the support-plane clip, in em:
    /// legs of the removed isoceles right triangles, in the unit quad's corner
    /// order (0,0), (1,0), (1,1), (0,1) — em-space (min_x,max_y), (max_x,max_y),
    /// (max_x,min_y), (min_x,min_y). 0 where the corner is not worth clipping.
    pub clips: [f32; 4],
    /// The second master, when the face has a `wght` axis and the two masters
    /// are point-compatible.
    pub master_b: Option<MasterB>,
    /// The COLRv0 layer stack, bottom to top, when the glyph is a color glyph.
    /// Each layer is an ordinary glyph and carries its own full inspection.
    pub colr: Option<Vec<ColorLayerInspection>>,
    /// Where everything lands inside the glyph's block of the curve texture.
    pub layout: TexelLayout,
}

/// Inspect a character in a family: resolves the face by family name, maps
/// the character through the face's charmap (no shaping), and inspects the
/// glyph. `None` when no loaded face has that family name or the face has no
/// glyph for the character.
pub fn character(
    font_system: &mut FontSystem,
    family: &str,
    ch: char,
    weight: fontdb::Weight,
) -> Option<GlyphInspection> {
    let font_id = face_by_family(font_system, family)?;
    let font = font_system.get_font(font_id, weight)?;
    let glyph_id = match font.as_swash().charmap().map(ch) {
        0 => return None,
        id => id,
    };
    glyph(font_system, font_id, glyph_id, weight)
}

/// Inspect a glyph by raw id. `None` when the glyph has neither an outline
/// nor COLR layers (a pure bitmap glyph — those live on the atlas path and
/// have no vector internals to show).
pub fn glyph(
    font_system: &mut FontSystem,
    font_id: fontdb::ID,
    glyph_id: u16,
    weight: fontdb::Weight,
) -> Option<GlyphInspection> {
    let mut swash = SwashCache::new();
    let mut colr = ColrCache::default();
    inspect_glyph(
        &mut swash,
        &mut colr,
        font_system,
        font_id,
        glyph_id,
        weight,
        true,
    )
}

/// Family names of every loaded face, in database order, deduplicated. On a
/// blob-only [`FontSystem`] (the wasm case) this is exactly the embedded
/// fonts.
pub fn families(font_system: &FontSystem) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for face in font_system.db().faces() {
        for (name, _) in &face.families {
            if !out.iter().any(|have| have == name) {
                out.push(name.clone());
            }
        }
    }
    out
}

/// The id of the face for a family name. `new_with_fonts` loads system fonts
/// *before* the embedded blobs on native, so when several faces claim the
/// family the last one wins — the same rule the test scaffolding uses.
fn face_by_family(font_system: &FontSystem, family: &str) -> Option<fontdb::ID> {
    font_system
        .db()
        .faces()
        .filter(|face| face.families.iter().any(|(name, _)| name == family))
        .last()
        .map(|face| face.id)
}

fn quads_of(records: &[f32]) -> Vec<Quadratic> {
    records
        .chunks_exact(FLOATS_PER_CURVE)
        .map(|r| Quadratic {
            p0: [r[0], r[1]],
            p1: [r[2], r[3]],
            p2: [r[4], r[5]],
        })
        .collect()
}

fn outline_commands(commands: &[Command]) -> Vec<OutlineCommand> {
    commands
        .iter()
        .map(|command| match *command {
            Command::MoveTo(p) => OutlineCommand::MoveTo([p.x, p.y]),
            Command::LineTo(p) => OutlineCommand::LineTo([p.x, p.y]),
            Command::QuadTo(c, p) => OutlineCommand::QuadTo([c.x, c.y], [p.x, p.y]),
            Command::CurveTo(c1, c2, p) => {
                OutlineCommand::CurveTo([c1.x, c1.y], [c2.x, c2.y], [p.x, p.y])
            }
            Command::Close => OutlineCommand::Close,
        })
        .collect()
}

/// The band tables recomputed through the same [`bands`] the block packer
/// uses, decorated with each entry's sort key and each band's interval.
fn band_tables(built: &BuiltBlock) -> BandTables {
    let axis_bands = |axis: usize, min: f32, max: f32| {
        // Sort keys are the curves' extremes along the *ray* axis (the one
        // rays travel), which is the other axis than the one being banded.
        let spans = spans(&built.records, built.b_records.as_deref(), 1 - axis);
        let height = (max - min) / BANDS as f32;
        bands(&built.records, built.b_records.as_deref(), axis, min, max)
            .into_iter()
            .enumerate()
            .map(|(i, band)| BandInspection {
                interval: [min + i as f32 * height, min + (i + 1) as f32 * height],
                split: band.split,
                descending: band
                    .descending
                    .iter()
                    .map(|&curve| BandEntry {
                        curve,
                        key: spans[curve as usize].1,
                    })
                    .collect(),
                ascending: band
                    .ascending
                    .iter()
                    .map(|&curve| BandEntry {
                        curve,
                        key: spans[curve as usize].0,
                    })
                    .collect(),
            })
            .collect()
    };
    BandTables {
        y: axis_bands(1, built.bbox[1], built.bbox[3]),
        x: axis_bands(0, built.bbox[0], built.bbox[2]),
        epsilon: BAND_EPSILON,
    }
}

/// The worker behind [`glyph`]. `follow_colr` is turned off for the recursive
/// per-layer inspections: layers are ordinary glyphs, and a malformed font
/// that listed a color glyph as its own layer must not recurse forever.
fn inspect_glyph(
    swash: &mut SwashCache,
    colr: &mut ColrCache,
    font_system: &mut FontSystem,
    font_id: fontdb::ID,
    glyph_id: u16,
    weight: fontdb::Weight,
    follow_colr: bool,
) -> Option<GlyphInspection> {
    // Identical to extraction: a variable face is read at both ends of its
    // wght axis and master B is kept only if it is point-compatible.
    let axis = wght_axis(font_system, font_id, weight);
    let (a_weight, b_weight) = match axis {
        Some((min, max)) => (axis_weight(min), axis_weight(max)),
        None => (weight, weight),
    };
    let a_commands = glyph_outline(
        swash,
        font_system,
        font_id,
        glyph_id,
        a_weight,
        weight_flags(),
    );
    let layers = follow_colr
        .then(|| colr.layers(font_system, font_id, glyph_id, weight))
        .flatten();
    if a_commands.is_none() && layers.is_none() {
        return None;
    }

    let colr_layers = layers.map(|layers| {
        layers
            .iter()
            .map(|layer| ColorLayerInspection {
                glyph_id: layer.glyph_id,
                color: layer.color,
                glyph: inspect_glyph(
                    swash,
                    colr,
                    font_system,
                    font_id,
                    layer.glyph_id,
                    weight,
                    false,
                )
                .unwrap_or_else(|| empty_inspection(font_id, layer.glyph_id)),
            })
            .collect()
    });

    let Some(a_commands) = a_commands else {
        let mut empty = empty_inspection(font_id, glyph_id);
        empty.colr = colr_layers;
        return Some(empty);
    };
    let b_commands = match axis {
        Some(_) => glyph_outline(
            swash,
            font_system,
            font_id,
            glyph_id,
            b_weight,
            weight_flags(),
        )
        .filter(|b| masters_compatible(&a_commands, b)),
        None => None,
    };

    // The very packer production runs, plus a second, unquantized flatten of
    // the same commands so the f16 deltas are measurable.
    let built = build_block(&a_commands, b_commands.as_deref(), BAND_MIN_CURVES);
    let (raw_records, _, _) = flatten_with(&a_commands, false);
    let raw_b = b_commands
        .as_deref()
        .map(|commands| flatten_with(commands, false).0);

    let masters = 1 + built.b_records.is_some() as u32;
    let total_texels = (built.block.len() / FLOATS_PER_TEXEL) as u32;
    let records_offset = total_texels - built.record_texels * masters;
    let header_texels = if built.banded {
        HEADER_TEXELS as u32
    } else {
        0
    };
    let master_b = match (&built.b_records, axis) {
        (Some(b), Some((min, max))) => Some(MasterB {
            curves: quads_of(b),
            curves_raw: raw_b.as_deref().map(quads_of).unwrap_or_default(),
            axis_min: min,
            axis_max: max,
            weight_t: weight_blend(weight, min, max),
        }),
        _ => None,
    };

    Some(GlyphInspection {
        font_id,
        glyph_id,
        outline: outline_commands(&a_commands),
        curves: quads_of(&built.records),
        curves_raw: quads_of(&raw_records),
        bands: built.banded.then(|| band_tables(&built)),
        contours: built.contours,
        bbox: built.bbox,
        banded: built.banded,
        clips: built.clips,
        master_b,
        colr: colr_layers,
        layout: TexelLayout {
            header_texels,
            index_texels: records_offset - header_texels,
            records_offset,
            record_texels: built.record_texels,
            total_texels,
            masters,
        },
    })
}

/// The flags production extraction sees for ordinary shaped text.
fn weight_flags() -> CacheKeyFlags {
    CacheKeyFlags::empty()
}

/// The inspection of a glyph with no outline: everything empty, layout zero.
fn empty_inspection(font_id: fontdb::ID, glyph_id: u16) -> GlyphInspection {
    GlyphInspection {
        font_id,
        glyph_id,
        outline: Vec::new(),
        curves: Vec::new(),
        curves_raw: Vec::new(),
        contours: Vec::new(),
        bbox: [0.0; 4],
        banded: false,
        bands: None,
        clips: [0.0; 4],
        master_b: None,
        colr: None,
        layout: TexelLayout {
            header_texels: 0,
            index_texels: 0,
            records_offset: 0,
            record_texels: 0,
            total_texels: 0,
            masters: 1,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curves::CurveStore;
    use crate::testing;

    fn quantize(v: f32) -> f32 {
        half::f16::from_f32(v).to_f32()
    }

    /// The load-bearing property of the whole module: what inspection reports
    /// is what the curve store puts on the GPU, field for field.
    #[test]
    fn inspection_matches_what_the_store_extracts() {
        let (device, _) = testing::gpu();
        let mut fonts = testing::font_system();
        let font_id = testing::font_id_of(&fonts, testing::STATIC_FAMILY);
        let glyph_id = testing::glyph_id_of(&mut fonts, font_id, 'g').unwrap();

        let inspected = glyph(&mut fonts, font_id, glyph_id, fontdb::Weight::NORMAL).unwrap();
        let mut store = CurveStore::new(device);
        let stored = store
            .get_or_insert(
                &mut fonts,
                font_id,
                glyph_id,
                fontdb::Weight::NORMAL,
                CacheKeyFlags::empty(),
            )
            .unwrap();

        assert_eq!(inspected.curves.len() as u32, stored.count);
        assert_eq!(inspected.bbox, stored.bbox);
        assert_eq!(inspected.banded, stored.banded);
        assert_eq!(inspected.clips, stored.clips);
        assert_eq!(inspected.layout.record_texels, stored.record_texels);
    }

    /// 'g' in DejaVu Sans is the canonical banded glyph: two contours, more
    /// than 16 curves, band tables on both axes, and every stored control
    /// point is its raw twin rounded to f16.
    #[test]
    fn g_is_banded_and_quantization_deltas_are_measurable() {
        let mut fonts = testing::font_system();
        let g = character(
            &mut fonts,
            testing::STATIC_FAMILY,
            'g',
            fontdb::Weight::NORMAL,
        )
        .expect("DejaVu Sans has a 'g'");

        assert!(g.curves.len() > 16);
        assert!(g.banded);
        assert_eq!(g.curves.len(), g.curves_raw.len());
        assert_eq!(g.contours.iter().sum::<u32>() as usize, g.curves.len());
        for (stored, raw) in g.curves.iter().zip(&g.curves_raw) {
            for (s, r) in [
                (stored.p0, raw.p0),
                (stored.p1, raw.p1),
                (stored.p2, raw.p2),
            ] {
                assert_eq!(s[0], quantize(r[0]));
                assert_eq!(s[1], quantize(r[1]));
            }
        }

        let bands = g.bands.as_ref().expect("banded means tables");
        assert_eq!(bands.y.len(), BANDS);
        assert_eq!(bands.x.len(), BANDS);
        for band in bands.y.iter().chain(&bands.x) {
            assert_eq!(band.descending.len(), band.ascending.len());
            // Sorted the way the shader's early-out needs them.
            assert!(band.descending.windows(2).all(|w| w[0].key >= w[1].key));
            assert!(band.ascending.windows(2).all(|w| w[0].key <= w[1].key));
        }

        // The layout adds up: header, index lists, then one master's records.
        assert_eq!(g.layout.masters, 1);
        assert_eq!(g.layout.header_texels, HEADER_TEXELS as u32);
        assert_eq!(
            g.layout.total_texels,
            g.layout.records_offset + g.layout.record_texels
        );
    }

    /// A variable face reports a second master, parallel curve for curve.
    #[test]
    fn manrope_carries_a_second_master() {
        let mut fonts = testing::variable_font_system();
        let g = character(
            &mut fonts,
            testing::VARIABLE_FAMILY,
            'g',
            fontdb::Weight::NORMAL,
        )
        .expect("Manrope has a 'g'");
        let b = g.master_b.expect("Manrope has a wght axis");
        assert_eq!(b.curves.len(), g.curves.len());
        assert_eq!(b.axis_min, 200.0);
        assert_eq!(b.axis_max, 800.0);
        assert_eq!(g.layout.masters, 2);
        assert_eq!(
            g.layout.total_texels,
            g.layout.records_offset + 2 * g.layout.record_texels
        );
    }

    /// 🚀 is six COLR layers, each carrying its own full inspection.
    #[test]
    fn the_rocket_reports_its_layer_stack() {
        let mut fonts = testing::color_font_system();
        let rocket = character(
            &mut fonts,
            testing::COLOR_FAMILY,
            '🚀',
            fontdb::Weight::NORMAL,
        )
        .expect("the Twemoji subset has 🚀");
        let layers = rocket.colr.as_ref().expect("🚀 is COLR");
        assert_eq!(layers.len(), 6);
        for layer in layers {
            assert!(!layer.glyph.curves.is_empty(), "layers are real outlines");
            assert!(layer.glyph.colr.is_none(), "layers do not recurse");
        }
        assert_eq!(
            layers[0].color,
            Some([160.0 / 255.0, 4.0 / 255.0, 30.0 / 255.0, 1.0])
        );
    }
}
