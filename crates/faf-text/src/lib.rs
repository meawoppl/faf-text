//! A GPU text renderer that is fast as f***. Native and WebAssembly, WebGPU
//! and WebGL2, one code path.
//!
//! **Glyphs are rendered from their outlines, on the GPU, every frame.** Each
//! glyph's Bézier outline is extracted once (via swash, at size 1.0, so the
//! coordinates are pure em units), cubics are flattened to quadratics, and the
//! curves are packed into an RGBA16F data texture. Every glyph on screen is one
//! instanced quad, and the fragment shader decides inside/outside per pixel:
//! cast a horizontal ray through the sample point, solve `y(t) = 0` for every
//! quadratic, and accumulate signed, clamped crossing distances — the non-zero
//! winding rule, with **analytic antialiasing** baked into the clamp. There is
//! no glyph atlas for outlines, no SDF bake and no MSAA.
//!
//! What falls out of that:
//!
//! - **Free scaling and true subpixel positioning.** Zooming re-rasterizes
//!   nothing; the curve data never changes and a glyph is never snapped to the
//!   pixel grid.
//! - **Variable weight for free.** A face with a `wght` axis is extracted at
//!   both ends and the fragment shader mixes the two point-compatible masters
//!   before the winding test, so weight animates per frame at zero CPU cost
//!   ([`TextRenderer::text_with_weight`]).
//! - **Correct antialiasing in 3D.** Coverage comes from screen-space
//!   derivatives of an interpolated varying, so it is measured on the
//!   *projected* glyph: a pane seen at a grazing angle is as smooth as a flat
//!   one ([`TextRenderer::set_block_transform`]).
//! - **WebGL2 compatibility.** Curve data lives in a texture (`textureLoad`),
//!   never a storage buffer, so the same shaders run on Vulkan, Metal, DX12,
//!   GL, WebGPU and WebGL2.
//! - **Color emoji as vectors.** A COLR/CPAL v0 glyph is a stack of ordinary
//!   outlines in flat palette colors, so each layer goes through the same curve
//!   store and draws as one more instanced quad, bottom to top — crisp at any
//!   size, and no atlas space.
//!
//! CBDT/sbix/SVG color fonts, COLRv1, and any glyph without an extractable
//! outline fall back to a shelf-packed bitmap atlas. Selection backgrounds,
//! highlight overlays and carets are instanced rect layers below and above the
//! glyphs; underline,
//! strikethrough, squiggle and rounded chip are a third instanced pipeline
//! whose fragment shader switches on a [`DecorationKind`].
//!
//! # Live demos
//!
//! On docs.rs each cell below is a real canvas driven by this renderer compiled
//! to WebAssembly. Everywhere else — GitHub, crates.io, offline docs — the
//! animated PNG behind it plays instead, which is the same scene rendered
//! headless by `cargo run --example gallery`. The full interactive demo is at
//! <https://meawoppl.github.io/faf-text/demo/>.
//!
//! <div class="faf-live-grid">
//! <div class="faf-live" data-demo="zoom">
//!
//! ![font size sweeping 10 to 80 px, with nothing re-rasterizing](https://raw.githubusercontent.com/meawoppl/faf-text/gh-pages/gallery/zoom.apng)
//!
//! <span class="faf-live-caption">zoom — one curve set, every size</span>
//! </div>
//! <div class="faf-live" data-demo="weight">
//!
//! ![a variable font's weight axis blended in the fragment shader](https://raw.githubusercontent.com/meawoppl/faf-text/gh-pages/gallery/weight.apng)
//!
//! <span class="faf-live-caption">weight — two masters, lerped per pixel</span>
//! </div>
//! <div class="faf-live" data-demo="tilt">
//!
//! ![a pane of text turning in 3D under a perspective camera](https://raw.githubusercontent.com/meawoppl/faf-text/gh-pages/gallery/tilt.apng)
//!
//! <span class="faf-live-caption">tilt — one matrix a frame</span>
//! </div>
//! <div class="faf-live" data-demo="terminal">
//!
//! ![a colored log streaming through a terminal grid](https://raw.githubusercontent.com/meawoppl/faf-text/gh-pages/gallery/terminal.apng)
//!
//! <span class="faf-live-caption">terminal — cells, no shaping</span>
//! </div>
//! </div>
//!
//! # Quickstart
//!
//! [`TextRenderer`] needs a `wgpu::Device` and the format of whatever it will
//! draw into; [`TextView`] wraps a shaped cosmic-text buffer with the
//! hit-testing and selection geometry a host wants. The immediate-mode API is
//! the shortest path to pixels:
//!
//! ```no_run
//! use faf_text::{Attrs, Color, Cursor, Family, Metrics, RectLayer, TextRenderer, TextView};
//!
//! # fn demo(device: &wgpu::Device, queue: &wgpu::Queue, format: wgpu::TextureFormat) {
//! // Fonts: the embedded blobs, with no system font scan (the only option on
//! // wasm, and a faster start everywhere else).
//! let mut font_system = faf_text::font_system_from_fonts(&[faf_text::FONT_DEJAVU_SANS]);
//! let mut renderer = TextRenderer::new(device, format);
//!
//! let mut view = TextView::new(&mut font_system, Metrics::new(24.0, 32.0));
//! view.pos = [40.0, 40.0];
//! view.set_text(
//!     &mut font_system,
//!     "Every pixel of this solves the winding rule.",
//!     &Attrs::new().family(Family::SansSerif),
//! );
//!
//! // One frame: a selection under the text, then the text itself.
//! renderer.begin();
//! for rect in view.selection_rects(Cursor::new(0, 11), Cursor::new(0, 20)) {
//!     renderer.rect(
//!         [rect[0], rect[1]],
//!         [rect[2], rect[3]],
//!         Color::rgba8(0x3b, 0x63, 0xa8, 0xff),
//!         RectLayer::Under,
//!     );
//! }
//! renderer.text(queue, &mut font_system, &view.buffer, view.pos, Color::WHITE);
//! renderer.finish(device, queue, [960.0, 600.0]);
//!
//! // ...then, inside a render pass targeting `format`:
//! // renderer.render(&mut pass);
//! # }
//! ```
//!
//! A host that redraws the same content every frame should use **retained
//! blocks** ([`TextRenderer::create_block`] and
//! [`TextRenderer::set_block_content`]) instead: setting a block's content
//! re-uploads that block's instance ranges and nothing else, moving it writes
//! one matrix, and a frame in which nothing changed reports
//! [`TextRenderer::damaged`] false so the host can skip recording and
//! presenting entirely.
//!
//! # Map of the crate
//!
//! | Item | What it is |
//! | --- | --- |
//! | [`TextRenderer`] | Pipelines, glyph stores, retained blocks, and the immediate-mode wrapper over one of them. |
//! | [`TextView`] | A positioned, shaped buffer: hit-testing, cursor motion, selection and decoration geometry. |
//! | [`Document`] | A large document as chunked [`TextView`]s, shaped and evicted around a viewport. |
//! | [`TermGrid`] | The monospace cell fast path: no shaping, procedural box drawing, merged background runs. |
//! | [`math`] | Camera matrices and the pointer-ray/pane hit test that puts a click back into block-local pixels. |
//! | [`Color`], [`RectLayer`], [`DecorationKind`] | The small value types the drawing calls take. |
//! | [`RendererOptions`] | Construction-time quality knobs: [`CoverageBlend`] and [`Subpixel`]. |
//!
//! cosmic-text is re-exported whole as [`cosmic_text`], and glam as [`glam`],
//! so a host never has to pin a second copy of either.
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
//! 3. **vector glyphs** — outlines evaluated in the fragment shader, including
//!    one instance per layer of every COLRv0 color glyph.
//! 4. **weight-blended vector glyphs** — the same, for glyphs carrying a
//!    second variable-font master.
//! 5. **atlas glyphs** — bitmap color emoji and anything without an outline.
//! 6. **line decorations** — underline, strikethrough, squiggle. Over the
//!    glyphs, so an underline crosses the descenders it passes through rather
//!    than being hidden by them.
//! 7. **over-rects** — highlight overlays and carets ([`RectLayer::Over`]).
//!
//! Layering is *within* a block; blocks composite in creation order (or in
//! whatever order [`TextRenderer::set_block_z`] asks for). An underlay for a
//! block's text therefore belongs either in that block's under-rects or in a
//! block created before it.
//!
//! # Terminal grid
//!
//! [`TermGrid`] is a shaping-free fast path for monospace cell content: char →
//! glyph id through the font's charmap, fixed cell metrics, merged background
//! runs, and procedural rects for box drawing. It renders into an ordinary
//! block through [`GlyphSpec`], so a terminal pane and a prose pane share one
//! curve store and one draw order.
//!
//! # Panes in 3D
//!
//! A block carries a full 4×4 placement ([`TextRenderer::set_block_transform`])
//! and the scene a shared camera ([`TextRenderer::set_view_projection`]), so a
//! pane of text can sit anywhere in a 3D scene for the price of one uniform
//! write per frame. Antialiasing needs no help there: coverage comes from
//! screen-space derivatives, so it is measured on the projected glyph and stays
//! correct at grazing angles. [`math`] has the camera helpers and the ray test
//! that turns a pointer back into the block-local pixels [`TextView::hit`]
//! wants.
#![warn(missing_docs)]

mod arena;
mod atlas;
mod colr;
mod curves;
mod document;
mod grid;
pub mod math;
mod renderer;
#[cfg(test)]
mod testing;
mod view;

pub use document::{CHUNK_LINES, DocCursor, DocStats, Document, RETAIN_CHUNKS, WINDOW_CHUNKS};
pub use grid::{Cell, CellStyle, GridFont, GridScene, TermGrid, char_width};
pub use renderer::{
    BlockContent, BlockId, CoverageBlend, DecorationKind, GlyphSpec, RectLayer, RendererOptions,
    Subpixel, TextRenderer, UploadStats,
};
pub use view::{LineMetrics, Rect, TextView};

pub use cosmic_text;
pub use cosmic_text::{
    Affinity, Attrs, Buffer, Cursor, Family, FontSystem, Metrics, Shaping, UnderlineStyle, Weight,
    fontdb,
};
/// Re-exported so hosts build the matrices [`math`] and [`TextRenderer`] take
/// without pinning a second copy of glam.
pub use glam;

/// Straight (non-premultiplied) RGBA color, 0..=1 per channel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color(pub [f32; 4]);

impl Color {
    /// Opaque white.
    pub const WHITE: Color = Color([1.0, 1.0, 1.0, 1.0]);
    /// Opaque black.
    pub const BLACK: Color = Color([0.0, 0.0, 0.0, 1.0]);

    /// From four 0..=1 floats.
    ///
    /// ```
    /// # use faf_text::Color;
    /// assert_eq!(Color::rgba(1.0, 1.0, 1.0, 1.0), Color::WHITE);
    /// ```
    pub const fn rgba(r: f32, g: f32, b: f32, a: f32) -> Self {
        Color([r, g, b, a])
    }

    /// From four 8-bit channels — the form a hex color is usually written in.
    ///
    /// ```
    /// # use faf_text::Color;
    /// let tokyo_night_fg = Color::rgba8(0xc0, 0xca, 0xf5, 0xff);
    /// ```
    pub fn rgba8(r: u8, g: u8, b: u8, a: u8) -> Self {
        Color([
            r as f32 / 255.0,
            g as f32 / 255.0,
            b as f32 / 255.0,
            a as f32 / 255.0,
        ])
    }

    /// From cosmic-text's packed color, which is how per-span colors arrive
    /// out of shaping.
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
/// Twemoji Mozilla, subset to four emoji (❤ 🌈 🔥 🚀) — a **COLRv0** color
/// font, whose glyphs are stacks of ordinary outlines painted in CPAL palette
/// colors. Those take the vector path like any other outline, so they stay
/// crisp at any size instead of riding the bitmap atlas. Apache-2.0 (build)
/// and CC-BY 4.0 (artwork) — see `assets/Twemoji-LICENSE.txt`.
pub const FONT_TWEMOJI_COLR: &[u8] = include_bytes!("../assets/TwemojiMozilla-COLR-subset.ttf");

/// Build a [`FontSystem`] from font blobs alone — no system font scan, which
/// keeps startup fast and is the only option on wasm.
pub fn font_system_from_fonts(fonts: &[&[u8]]) -> FontSystem {
    FontSystem::new_with_fonts(
        fonts
            .iter()
            .map(|data| cosmic_text::fontdb::Source::Binary(std::sync::Arc::new(data.to_vec()))),
    )
}
