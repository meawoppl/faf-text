//! faf-text: a GPU text renderer built for speed.
//!
//! Glyph outlines are flattened once to quadratic Béziers (em units) and
//! packed into a data texture; the fragment shader decides inside/outside per
//! pixel with the non-zero winding rule and analytic antialiasing. Scaling and
//! subpixel positioning are exact and free — nothing re-rasterizes. Color
//! emoji fall back to a shelf-packed bitmap atlas. Selection backgrounds and
//! highlight overlays are instanced rect layers below and above the glyphs.
//!
//! Everything is plain wgpu with WebGL2-safe choices (data textures instead of
//! storage buffers), so the same code runs native, WebGPU, and WebGL2.

mod atlas;
mod curves;
mod document;
mod renderer;
#[cfg(test)]
mod testing;
mod view;

pub use document::{CHUNK_LINES, DocCursor, DocStats, Document, RETAIN_CHUNKS, WINDOW_CHUNKS};
pub use renderer::{RectLayer, TextRenderer};
pub use view::{Rect, TextView};

pub use cosmic_text;
pub use cosmic_text::{
    Affinity, Attrs, Buffer, Cursor, Family, FontSystem, Metrics, Shaping, Weight,
};

/// Straight (non-premultiplied) RGBA color, 0..=1 per channel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color(pub [f32; 4]);

impl Color {
    pub const WHITE: Color = Color([1.0, 1.0, 1.0, 1.0]);
    pub const BLACK: Color = Color([0.0, 0.0, 0.0, 1.0]);

    pub const fn rgba(r: f32, g: f32, b: f32, a: f32) -> Self {
        Color([r, g, b, a])
    }

    pub fn rgba8(r: u8, g: u8, b: u8, a: u8) -> Self {
        Color([
            r as f32 / 255.0,
            g as f32 / 255.0,
            b as f32 / 255.0,
            a as f32 / 255.0,
        ])
    }

    pub fn from_cosmic(c: cosmic_text::Color) -> Self {
        Self::rgba8(c.r(), c.g(), c.b(), c.a())
    }
}

/// DejaVu Sans, embedded for demos and wasm targets with no system fonts.
pub const FONT_DEJAVU_SANS: &[u8] = include_bytes!("../assets/DejaVuSans.ttf");
/// DejaVu Sans Mono, ditto.
pub const FONT_DEJAVU_SANS_MONO: &[u8] = include_bytes!("../assets/DejaVuSansMono.ttf");
/// Manrope, a variable font with a `wght` axis spanning 200–800. Glyphs from a
/// variable face carry both axis-end masters on the GPU, so
/// [`TextRenderer::text_with_weight`] can blend weight per frame for free.
/// SIL Open Font License 1.1 — see `assets/Manrope-OFL.txt`.
pub const FONT_MANROPE_VARIABLE: &[u8] = include_bytes!("../assets/Manrope-Variable.ttf");

/// Build a [`FontSystem`] from font blobs alone — no system font scan, which
/// keeps startup fast and is the only option on wasm.
pub fn font_system_from_fonts(fonts: &[&[u8]]) -> FontSystem {
    FontSystem::new_with_fonts(
        fonts
            .iter()
            .map(|data| cosmic_text::fontdb::Source::Binary(std::sync::Arc::new(data.to_vec()))),
    )
}
