//! COLRv0 layered color glyphs.
//!
//! A COLRv0 color glyph is not a picture: it is a stack of *ordinary outline
//! glyphs*, each painted in one flat color out of the font's CPAL palette
//! (Lengyel 2017 §6). Nothing about that needs a rasterizer — the layers go
//! through [`crate::curves::CurveStore`] like any other glyph, and the renderer
//! emits one vector instance per layer in COLR order, so a color emoji is as
//! crisp at 300 px as a letter is and costs no atlas space at all.
//!
//! This module is only the table reading: given a (font, base glyph) it hands
//! back the layer list, resolved against palette 0, and caches it. What it
//! deliberately does *not* cover is COLRv1 — gradients, transforms and
//! compositing modes, none of which the winding shader can express — so a v1
//! font is reported as having no layers and its glyphs take the bitmap atlas
//! path, exactly like CBDT/sbix/SVG color fonts do.
//!
//! ttf-parser is used for the parts where a byte-level mistake is expensive
//! (locating tables inside an sfnt or a collection, and CPAL, whose color
//! records are BGRA behind two levels of indirection). The COLRv0 base-glyph
//! and layer arrays are read here directly: ttf-parser models them through a
//! `Painter` trait built for v1, which cannot say *which* layers asked for the
//! reserved "use the text color" palette index.

use std::sync::Arc;

use cosmic_text::{FontSystem, fontdb};
use rustc_hash::FxHashMap;
use ttf_parser::{RawFace, Tag, cpal};

const COLR: Tag = Tag::from_bytes(b"COLR");
const CPAL: Tag = Tag::from_bytes(b"CPAL");

/// The palette entry COLR reserves for "paint this layer in the text color",
/// which is how a font ships a glyph that takes the run's own color.
const FOREGROUND_PALETTE_INDEX: u16 = 0xFFFF;

/// The palette layers resolve against. Every COLRv0 font in the wild ships
/// exactly one; picking another would be a per-font host setting, not a
/// per-glyph decision, so it is fixed here rather than threaded through the
/// draw calls.
const PALETTE: u16 = 0;

/// One layer of a COLRv0 color glyph: an ordinary glyph id, plus the flat
/// color it is painted in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColorLayer {
    /// The layer's glyph id — a normal outline glyph in the same face.
    pub glyph_id: u16,
    /// Straight (non-premultiplied) RGBA from palette 0, or `None` for the
    /// reserved index `0xFFFF`, meaning the layer takes the text color.
    pub color: Option<[f32; 4]>,
}

/// Layer lists per (font, base glyph), read once and kept.
///
/// Two levels of caching, and the outer one matters: a font is probed for a
/// usable COLRv0 + CPAL pair the first time any of its glyphs is looked up, and
/// a font that has none is remembered as such — otherwise every letter of every
/// ordinary text run would leave a "not a color glyph" entry behind.
#[derive(Default)]
pub struct ColrCache {
    /// Whether a font has a COLRv0 table this renderer can use at all. False
    /// for plain outline fonts, for CBDT/sbix/SVG color fonts, and for COLRv1.
    fonts: FxHashMap<fontdb::ID, bool>,
    /// Layer lists, only for fonts that passed the probe. `None` is a glyph
    /// with no base record in a font that does have color glyphs.
    layers: FxHashMap<(fontdb::ID, u16), Option<Arc<[ColorLayer]>>>,
}

impl ColrCache {
    /// The layers of a COLRv0 color glyph, bottom to top, or `None` when the
    /// glyph is not one — which is the signal to render it the ordinary way
    /// (outline if it has one, bitmap atlas if it does not).
    ///
    /// `weight` only picks which instance of the face cosmic-text hands back;
    /// the tables read here are the file's, and identical across weights.
    pub fn layers(
        &mut self,
        font_system: &mut FontSystem,
        font_id: fontdb::ID,
        glyph_id: u16,
        weight: fontdb::Weight,
    ) -> Option<Arc<[ColorLayer]>> {
        if let Some(cached) = self.layers.get(&(font_id, glyph_id)) {
            return cached.clone();
        }
        if self.fonts.get(&font_id) == Some(&false) {
            return None;
        }
        let (usable, layers) = read_layers(font_system, font_id, glyph_id, weight);
        self.fonts.insert(font_id, usable);
        if usable {
            self.layers.insert((font_id, glyph_id), layers.clone());
        }
        layers
    }

    /// Whether a font carries color glyphs this renderer draws as vectors.
    /// Only meaningful after [`ColrCache::layers`] has probed the font.
    #[cfg(test)]
    pub fn font_is_colr(&self, font_id: fontdb::ID) -> Option<bool> {
        self.fonts.get(&font_id).copied()
    }
}

/// Read one glyph's layers straight out of the font's COLR/CPAL tables.
///
/// Returns `(font has usable COLRv0, this glyph's layers)`; the first half is
/// what [`ColrCache`] remembers so a non-color font is probed exactly once.
fn read_layers(
    font_system: &mut FontSystem,
    font_id: fontdb::ID,
    glyph_id: u16,
    weight: fontdb::Weight,
) -> (bool, Option<Arc<[ColorLayer]>>) {
    let Some(index) = font_system.db().face(font_id).map(|face| face.index) else {
        return (false, None);
    };
    // fontdb hands out the whole file; the index picks the face inside a
    // collection, which is also how swash reaches the same bytes.
    let Some(font) = font_system.get_font(font_id, weight) else {
        return (false, None);
    };
    let Ok(face) = RawFace::parse(font.data(), index) else {
        return (false, None);
    };
    let (Some(colr), Some(cpal)) = (face.table(COLR), face.table(CPAL)) else {
        return (false, None);
    };
    // Version 1 adds gradients, transforms and compositing, which the winding
    // shader has no way to express: leave the whole font to the bitmap atlas
    // rather than render its glyphs half right.
    if read_u16(colr, 0) != Some(0) {
        return (false, None);
    }
    let Some(palettes) = cpal::Table::parse(cpal) else {
        return (false, None);
    };
    let Some(base) = base_glyph_record(colr, glyph_id) else {
        // A color font, just not for this glyph (its digits and spaces are
        // ordinary outlines).
        return (true, None);
    };
    (true, layer_list(colr, &palettes, base))
}

/// A COLRv0 base-glyph record: where this glyph's layers start, and how many.
struct BaseGlyph {
    first_layer: u16,
    num_layers: u16,
}

/// Binary search the baseGlyphRecords array, which the spec requires to be
/// sorted by glyph id.
fn base_glyph_record(colr: &[u8], glyph_id: u16) -> Option<BaseGlyph> {
    let count = read_u16(colr, 2)?;
    let records = read_u32(colr, 4)? as usize;
    let (mut lo, mut hi) = (0u16, count);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let at = records.checked_add(mid as usize * 6)?;
        let id = read_u16(colr, at)?;
        match glyph_id.cmp(&id) {
            std::cmp::Ordering::Less => hi = mid,
            std::cmp::Ordering::Greater => lo = mid + 1,
            std::cmp::Ordering::Equal => {
                return Some(BaseGlyph {
                    first_layer: read_u16(colr, at + 2)?,
                    num_layers: read_u16(colr, at + 4)?,
                });
            }
        }
    }
    None
}

/// The layerRecords slice a base record points at, with every palette index
/// resolved. An empty layer list is not a color glyph as far as drawing is
/// concerned, so it comes back as `None`.
fn layer_list(colr: &[u8], palettes: &cpal::Table, base: BaseGlyph) -> Option<Arc<[ColorLayer]>> {
    let layers_offset = read_u32(colr, 8)? as usize;
    let num_records = read_u16(colr, 12)?;
    let end = base.first_layer.checked_add(base.num_layers)?;
    if end > num_records || base.num_layers == 0 {
        return None;
    }
    let mut layers = Vec::with_capacity(base.num_layers as usize);
    for i in base.first_layer..end {
        let at = layers_offset.checked_add(i as usize * 4)?;
        let glyph_id = read_u16(colr, at)?;
        let palette_entry = read_u16(colr, at + 2)?;
        // 0xFFFF asks for the text color, and so does a palette index the CPAL
        // table turns out not to hold — a broken font, where drawing the layer
        // in the run's color at least keeps its shape on screen.
        let color = if palette_entry == FOREGROUND_PALETTE_INDEX {
            None
        } else {
            palettes.get(PALETTE, palette_entry).map(|c| {
                [
                    f32::from(c.red) / 255.0,
                    f32::from(c.green) / 255.0,
                    f32::from(c.blue) / 255.0,
                    f32::from(c.alpha) / 255.0,
                ]
            })
        };
        layers.push(ColorLayer { glyph_id, color });
    }
    Some(layers.into())
}

fn read_u16(data: &[u8], at: usize) -> Option<u16> {
    let bytes = data.get(at..at + 2)?;
    Some(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn read_u32(data: &[u8], at: usize) -> Option<u32> {
    let bytes = data.get(at..at + 4)?;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;

    /// Palette 0 of the vendored Twemoji subset, as CPAL holds it.
    fn rgb(hex: u32) -> [f32; 4] {
        [
            ((hex >> 16) & 0xff) as f32 / 255.0,
            ((hex >> 8) & 0xff) as f32 / 255.0,
            (hex & 0xff) as f32 / 255.0,
            1.0,
        ]
    }

    fn colr_font() -> (FontSystem, fontdb::ID) {
        let font_system = testing::color_font_system();
        let id = testing::font_id_of(&font_system, testing::COLOR_FAMILY);
        (font_system, id)
    }

    fn layers_of(
        cache: &mut ColrCache,
        font_system: &mut FontSystem,
        font_id: fontdb::ID,
        ch: char,
    ) -> Option<Arc<[ColorLayer]>> {
        let glyph_id = testing::glyph_id_of(font_system, font_id, ch)?;
        cache.layers(font_system, font_id, glyph_id, fontdb::Weight::NORMAL)
    }

    /// 🚀 is six layers, and the order is the order the font paints them in:
    /// exhaust first, nose cone last. Everything downstream — instance order,
    /// and so the painter's-algorithm overlap — hangs off this being the COLR
    /// order and not, say, a hash map's.
    #[test]
    fn layers_come_back_in_paint_order_with_their_palette_colors() {
        let (mut font_system, font_id) = colr_font();
        let mut cache = ColrCache::default();
        let layers = layers_of(&mut cache, &mut font_system, font_id, '🚀').expect("🚀 is COLR");
        assert_eq!(
            &*layers,
            &[
                ColorLayer {
                    glyph_id: 14,
                    color: Some(rgb(0xA0041E))
                },
                ColorLayer {
                    glyph_id: 15,
                    color: Some(rgb(0xFFAC33))
                },
                ColorLayer {
                    glyph_id: 16,
                    color: Some(rgb(0xFFCC4D))
                },
                ColorLayer {
                    glyph_id: 17,
                    color: Some(rgb(0x55ACEE))
                },
                ColorLayer {
                    glyph_id: 18,
                    color: Some(rgb(0x000000))
                },
                ColorLayer {
                    glyph_id: 19,
                    color: Some(rgb(0xA0041E))
                },
            ]
        );
    }

    /// CPAL stores colors BGRA behind a per-palette index array, so a color
    /// coming back with its channels in the right order is the whole of what
    /// palette resolution has to get right. ❤ is one layer, entry 0.
    #[test]
    fn palette_entries_resolve_to_straight_rgba() {
        let (mut font_system, font_id) = colr_font();
        let mut cache = ColrCache::default();
        let layers = layers_of(&mut cache, &mut font_system, font_id, '❤').expect("❤ is COLR");
        assert_eq!(
            &*layers,
            &[ColorLayer {
                glyph_id: 11,
                color: Some(rgb(0xDD2E44))
            }]
        );
    }

    /// The layers themselves are ordinary glyphs of the same face and have no
    /// base record of their own — asking about one must not recurse or invent
    /// a layer list.
    #[test]
    fn a_glyph_with_no_base_record_has_no_layers() {
        let (mut font_system, font_id) = colr_font();
        let mut cache = ColrCache::default();
        // Glyph 14 is 🚀's first layer.
        assert!(
            cache
                .layers(&mut font_system, font_id, 14, fontdb::Weight::NORMAL)
                .is_none()
        );
        assert_eq!(cache.font_is_colr(font_id), Some(true));
    }

    /// A plain outline font is probed once and then answered from one bool:
    /// caching a "not a color glyph" entry per glyph would grow a map the size
    /// of the document for every ordinary text run.
    #[test]
    fn an_outline_font_is_probed_once_and_leaves_no_per_glyph_entries() {
        let mut font_system = testing::font_system();
        let font_id = testing::font_id_of(&font_system, testing::STATIC_FAMILY);
        let mut cache = ColrCache::default();
        for ch in "abcdef".chars() {
            assert!(layers_of(&mut cache, &mut font_system, font_id, ch).is_none());
        }
        assert_eq!(cache.font_is_colr(font_id), Some(false));
        assert!(cache.layers.is_empty());
    }

    /// The negative case that matters most: NotoColorEmoji is CBDT, a bitmap
    /// color font with no COLR table at all, and it has to stay on the atlas
    /// path. (Skipped where this box has no such font.)
    #[test]
    fn a_cbdt_color_font_reports_no_layers() {
        let Some(mut font_system) = testing::bitmap_color_font_system() else {
            eprintln!("no {} — skipping", testing::CBDT_FONT_PATH);
            return;
        };
        let font_id = testing::font_id_of(&font_system, "Noto Color Emoji");
        let mut cache = ColrCache::default();
        assert!(
            testing::glyph_id_of(&mut font_system, font_id, '🚀').is_some(),
            "the font does cover the glyph, it just has no outlines for it"
        );
        assert!(layers_of(&mut cache, &mut font_system, font_id, '🚀').is_none());
        assert_eq!(cache.font_is_colr(font_id), Some(false));
    }
}
