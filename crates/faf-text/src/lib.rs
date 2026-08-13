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
//!
//! # Layer order
//!
//! Content lives in blocks ([`TextRenderer::create_block`]), and a block draws
//! its seven layers in this order — one draw call each, and each skipped when
//! empty:
//!
//! 1. **under-rects** — selection backgrounds ([`RectLayer::Under`]).
//! 2. **chips** — rounded-rect backgrounds ([`DecorationKind::Chip`]): inline
//!    code, pills, tags. Behind the text they sit behind.
//! 3. **vector glyphs** — outlines evaluated in the fragment shader.
//! 4. **weight-blended vector glyphs** — the same, for glyphs carrying a
//!    second variable-font master.
//! 5. **atlas glyphs** — color emoji and anything without an outline.
//! 6. **line decorations** — underline, strikethrough, squiggle. Over the
//!    glyphs, so an underline crosses the descenders it passes through rather
//!    than being hidden by them.
//! 7. **over-rects** — highlight overlays and carets ([`RectLayer::Over`]).
//!
//! Layering is *within* a block; blocks composite in creation order. An
//! underlay for a block's text therefore belongs either in that block's
//! under-rects or in a block created before it.

mod arena;
mod atlas;
mod curves;
mod document;
mod renderer;
#[cfg(test)]
mod testing;
mod view;

pub use document::{CHUNK_LINES, DocCursor, DocStats, Document, RETAIN_CHUNKS, WINDOW_CHUNKS};
pub use renderer::{BlockContent, BlockId, DecorationKind, RectLayer, TextRenderer, UploadStats};
pub use view::{LineMetrics, Rect, TextView};

pub use cosmic_text;
pub use cosmic_text::{
    Affinity, Attrs, Buffer, Cursor, Family, FontSystem, Metrics, Shaping, UnderlineStyle, Weight,
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
