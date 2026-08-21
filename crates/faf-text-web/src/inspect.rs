//! JSON bindings over [`faf_text::inspect`] for the how-it-works explainer:
//! figures on that page draw the renderer's *real* internals (flattened
//! quadratics, band tables, corner clips, COLR layers), and this module is how
//! they reach them from JS.
//!
//! The font system behind these calls holds exactly the four embedded fonts
//! the demo ships — DejaVu Sans, DejaVu Sans Mono, Manrope (variable) and the
//! Twemoji COLR subset — so the same call returns the same data everywhere.

use std::cell::RefCell;

use faf_text::inspect::{
    BandInspection, BandTables, ColorLayerInspection, GlyphInspection, MasterB, OutlineCommand,
    Quadratic, TexelLayout,
};
use faf_text::{FontSystem, fontdb};
use serde_json::{Value, json};
use wasm_bindgen::prelude::*;

thread_local! {
    /// One font system for all inspection calls (wasm is single-threaded;
    /// native callers get one per thread, which only tests ever see).
    static FONTS: RefCell<FontSystem> = RefCell::new(faf_text::font_system_from_fonts(&[
        faf_text::FONT_DEJAVU_SANS,
        faf_text::FONT_DEJAVU_SANS_MONO,
        faf_text::FONT_MANROPE_VARIABLE,
        faf_text::FONT_TWEMOJI_COLR,
    ]));
}

/// Inspect one character of one family at one weight, as a JSON string.
///
/// The schema is documented in `web/how-it-works/CONTRACT.md`; the data is
/// [`faf_text::inspect::character`]'s, produced by the production extraction
/// code paths. Returns the string `"null"` when the family is unknown or the
/// face has no glyph for the character.
#[wasm_bindgen]
pub fn inspect_glyph(ch: char, family: &str, weight: u16) -> String {
    FONTS.with(|fonts| {
        let fonts = &mut *fonts.borrow_mut();
        match faf_text::inspect::character(fonts, family, ch, fontdb::Weight(weight)) {
            Some(inspection) => {
                let mut value = glyph_json(&inspection);
                value["ch"] = json!(ch.to_string());
                value["family"] = json!(family);
                value["weight"] = json!(weight);
                value.to_string()
            }
            None => "null".to_string(),
        }
    })
}

/// The family names inspection can resolve, as a JSON array of strings.
#[wasm_bindgen]
pub fn list_families() -> String {
    FONTS.with(|fonts| Value::from(faf_text::inspect::families(&fonts.borrow())).to_string())
}

fn point(p: [f32; 2]) -> Value {
    json!([p[0], p[1]])
}

fn quad_json(q: &Quadratic) -> Value {
    json!({ "p0": point(q.p0), "p1": point(q.p1), "p2": point(q.p2) })
}

fn quads_json(quads: &[Quadratic]) -> Value {
    Value::from(quads.iter().map(quad_json).collect::<Vec<_>>())
}

fn outline_json(commands: &[OutlineCommand]) -> Value {
    Value::from(
        commands
            .iter()
            .map(|command| match *command {
                OutlineCommand::MoveTo(p) => json!({ "type": "move_to", "p": point(p) }),
                OutlineCommand::LineTo(p) => json!({ "type": "line_to", "p": point(p) }),
                OutlineCommand::QuadTo(c, p) => {
                    json!({ "type": "quad_to", "c": point(c), "p": point(p) })
                }
                OutlineCommand::CurveTo(c1, c2, p) => {
                    json!({ "type": "curve_to", "c1": point(c1), "c2": point(c2), "p": point(p) })
                }
                OutlineCommand::Close => json!({ "type": "close" }),
            })
            .collect::<Vec<_>>(),
    )
}

fn band_json(band: &BandInspection) -> Value {
    let entries = |list: &[faf_text::inspect::BandEntry]| {
        Value::from(
            list.iter()
                .map(|e| json!({ "curve": e.curve, "key": e.key }))
                .collect::<Vec<_>>(),
        )
    };
    json!({
        "interval": [band.interval[0], band.interval[1]],
        "split": band.split,
        "descending": entries(&band.descending),
        "ascending": entries(&band.ascending),
    })
}

fn bands_json(tables: &BandTables) -> Value {
    json!({
        "epsilon": tables.epsilon,
        "y": tables.y.iter().map(band_json).collect::<Vec<_>>(),
        "x": tables.x.iter().map(band_json).collect::<Vec<_>>(),
    })
}

fn master_b_json(b: &MasterB) -> Value {
    json!({
        "curves": quads_json(&b.curves),
        "curves_raw": quads_json(&b.curves_raw),
        "axis_min": b.axis_min,
        "axis_max": b.axis_max,
        "weight_t": b.weight_t,
    })
}

fn layer_json(layer: &ColorLayerInspection) -> Value {
    json!({
        "glyph_id": layer.glyph_id,
        "color": layer.color.map(|c| json!([c[0], c[1], c[2], c[3]])),
        "glyph": glyph_json(&layer.glyph),
    })
}

fn layout_json(layout: &TexelLayout) -> Value {
    json!({
        "header_texels": layout.header_texels,
        "index_texels": layout.index_texels,
        "records_offset": layout.records_offset,
        "record_texels": layout.record_texels,
        "total_texels": layout.total_texels,
        "masters": layout.masters,
    })
}

fn glyph_json(g: &GlyphInspection) -> Value {
    json!({
        "glyph_id": g.glyph_id,
        "units": "em",
        "outline": outline_json(&g.outline),
        "curves": quads_json(&g.curves),
        "curves_raw": quads_json(&g.curves_raw),
        "contours": g.contours,
        "bbox": [g.bbox[0], g.bbox[1], g.bbox[2], g.bbox[3]],
        "banded": g.banded,
        "bands": g.bands.as_ref().map(bands_json),
        "clips": [g.clips[0], g.clips[1], g.clips[2], g.clips[3]],
        "master_b": g.master_b.as_ref().map(master_b_json),
        "colr": g.colr.as_ref().map(|layers| {
            Value::from(layers.iter().map(layer_json).collect::<Vec<_>>())
        }),
        "layout": layout_json(&g.layout),
    })
}
