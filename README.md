# faf-text

A GPU text renderer that is fast as f***. Native and WebAssembly, WebGPU and
WebGL2, one code path.

## How it works

**Glyphs are rendered from their outlines, on the GPU, every frame.** Each
glyph's Bézier outline is extracted once (via swash, at size 1.0 → pure em
units), cubics are flattened to quadratics, and the curves are packed into an
RGBA32F data texture. Every glyph on screen is one instanced quad; the fragment
shader decides inside/outside per pixel:

- Cast a horizontal ray through the sample point, solve `y(t) = 0` for every
  quadratic, and accumulate signed, clamped crossing distances — the non-zero
  winding rule with **analytic antialiasing** baked into the clamp. A second,
  90°-rotated cast averages in for isotropic AA. No MSAA, no SDF bake, no
  atlas for outlines.
- Crossings are classified by the sign pattern of the three control points
  (Lengyel-style bit tables), not by root-range checks — rays that graze curve
  endpoints exactly (common: font coordinates are neat 1/64 fractions) still
  count each crossing exactly once.
- Below ~24 px/em, three parallel rays per axis recover hairline stems that
  single sampling drops. Larger glyphs use the cheap single-ray path.
- Glyphs past 16 curves carry per-axis **band tables** ahead of their curve
  data: eight bands per axis, each listing only the curves that can cross a ray
  in it, so a fragment loops over a fraction of the glyph instead of all of it.
  Coverage is unchanged to the bit — a curve a band leaves out contributes
  exactly zero.

Because coverage is analytic, glyphs get **true subpixel positioning** (no
snapping, no subpixel atlas bins) and **free scaling** — zooming re-rasterizes
nothing; the curve data never changes.

**Variable-font weight is a GPU lerp.** A glyph from a face with a `wght` axis
is extracted twice, at both ends of the axis, and the two point-compatible
master outlines are stored side by side. The fragment shader mixes the control
points before the winding test, so weight animates per frame at zero CPU cost
and without touching the curve texture. Glyphs without a second master are
drawn by a pipeline the master-B fetch is compiled out of, so they pay nothing
for the feature.

**What's not curves rides a bitmap atlas.** Color emoji and any glyph without
an extractable outline are rasterized by swash into a shelf-packed
(`etagere`) RGBA atlas and drawn by a second instanced pipeline.

**Both glyph stores are unbounded by policy.** The curve texture doubles in
height on overflow (up to 8192 rows, or the device's limit) and re-uploads from
its CPU mirror; once capped, it drops the least recently used half of its
glyphs and repacks. The bitmap atlas deallocates any glyph not drawn in the
last two frames when it runs dry, and resets wholesale if fragmentation still
blocks the allocation. Eviction only ever happens at a frame boundary, so a
glyph queued for drawing can never have its data moved out from under it.

**Selection and highlights are rect layers.** Selection rects render *under*
the glyphs, highlight overlays render *over* them with alpha blending, both
instanced. Hit-testing (pixel → cursor) and cursor-range → rect geometry are
BiDi-aware, courtesy of cosmic-text's shaping and layout.

**Decorations are shapes, not rects.** Underline, strikethrough, squiggle and
rounded chip are one more instanced pipeline whose fragment shader switches on
a kind: a fill, a sine centerline the fragment measures its distance to, or a
rounded-rect SDF — both antialiased by `fwidth`, so a squiggle is exact at any
size and a chip's corners never stair-step. Underline and strikeout offsets and
thickness come from the face's own metrics (cached per font), with em-relative
fallbacks for faces that declare none; `TextView::decoration_rects` returns
baseline-anchored geometry for a cursor range, and text whose *attributes* ask
for an underline or strikeout decorates itself. Chips draw under the glyphs and
the line kinds over them.

**Panes go anywhere in 3D.** A block carries a 4×4 placement and the scene a
shared camera, so a pane of text can be tilted, turned or flown through a 3D
scene for one uniform write a frame — no glyph is rebuilt, nothing
re-rasterizes, and the curve data never changes. Antialiasing needs no help
there: coverage comes from screen-space derivatives of an interpolated varying,
so it is measured on the *projected* glyph, and a pane seen at a grazing angle
is as smooth as a flat one (and takes the hairline three-tap path on whichever
axis got compressed). Pointer input runs the same way in reverse — pixel to NDC
ray, ray onto the pane's own plane, and from there the ordinary 2D hit test.
Text is alpha-blended and does not z-write, so hosts sort blocks back to front
with a z key rather than a depth buffer. `cargo run --example panes3d` renders
three panes at oblique angles, one of them near edge-on, with a selection
painted through the ray path.

**Coverage has two construction-time knobs.** `TextRenderer::with_options`
takes a `CoverageBlend` and a `Subpixel` mode; `new` is the default pair, and
the default pair renders byte-for-byte what faf-text always did.
`CoverageBlend::Linear` points the pipelines at the sRGB view of the target
(`format.add_srgb_suffix()` in the surface's `view_formats`), converts instance
colors to linear light on the CPU, and applies a luminance-conditional contrast
correction to coverage, because blending in linear light on its own thins
dark-on-light stems. `Subpixel::Rgb` casts the horizontal ray three times, a
third of a pixel apart, for one coverage per LCD stripe, and hands the result
to the blender through dual-source blending (`Src1` factors, so the device
needs `wgpu::Features::DUAL_SOURCE_BLENDING` — WebGL2 never does, and asks for
grayscale instead). Both are *pipeline variants*, not shader branches: the
glyph fragment shader is a place where even a never-taken branch has measured
25%. Each is dropped rather than fatal when the device cannot do it, and
`effective_options()` says what survived. Subpixel coverage additionally turns
itself off per block for any placement that is not an axis-aligned scale and
translation — a pane tilted in 3D has no stripe axis to sample along, so its
glyphs draw grayscale while the flat ones next to it do not.

**Terminal content skips the shaper entirely.** A `TermGrid` is cells, colors,
style bits and a scrollback ring; translating a viewport into instances maps
each char straight through the font's charmap to a glyph id (cached per face),
places it on a whole-pixel cell grid, and merges adjacent same-background cells
into single rects. No shaping, no layout, no per-frame allocation — a 200×60
grid of colored log lines costs **0.12 ms of CPU per frame** to translate into
4.4k instances on this box, and the result goes into one retained block, so
appending a line re-uploads that block and nothing else. Correctness is not
traded away for it: CJK and other East Asian Width `W`/`F` characters take two
cells, and any cell the charmap misses — combining marks, ZWJ clusters, emoji —
falls back to a one-cell cosmic-text buffer, shaped once and cached by string,
which is also how color emoji reach the atlas. Box drawing and block elements
(U+2500–U+259F) are **generated**, not shaped: exact cell-bound rects, so lines
join their neighbours with no seam at any size, where a font's outlines stipple
the joins. `cargo run --example term` renders the showcase to `term.png`.

**The scene is retained, and damage-tracked.** Content lives in blocks; each
block owns a range of every instance arena (under-rects, chips, vector glyphs,
weight-blended glyphs, atlas glyphs, line decorations, over-rects) and one entry in a
dynamic-offset uniform buffer. Setting a block's content re-uploads that
block's ranges and nothing else, so typing in a search box does not touch the
document's glyphs. Moving a block — or reorienting it in 3D — writes one
matrix; no instance is rebuilt, which is what makes scrolling and pane
placement free.
Dropping blocks frees their ranges to a per-arena free list, and an arena
repacks itself once more than half of it is holes. When nothing changed at
all, `damaged()` says so and the host can skip recording and presenting
entirely — an idle window costs zero draw calls.

A block draws in at most seven calls, in this order — under-rects, **chips**,
vector glyphs, weight-blended vector glyphs, atlas glyphs, **line
decorations**, over-rects — and blocks composite in creation order. The
immediate-mode `begin`/`rect`/`text`/`decoration`/`finish` API is still there,
as a wrapper over one block that is rebuilt every frame.

## Compatibility

Everything is plain `wgpu` with WebGL2-safe choices — curve data lives in a
texture (`textureLoad`), not a storage buffer — so the same shaders run on
Vulkan/Metal/DX12/GL natively, and WebGPU or WebGL2 in the browser.

## Crates

- `crates/faf-text` — the renderer. `TextRenderer` (pipelines + glyph sources),
  `TextView` (positioned cosmic-text buffer with hit-testing/selection
  helpers), `TermGrid`/`GridFont` (the monospace cell fast path), `math`
  (camera matrices and the ray/pane hit test). No windowing dependencies.
- `crates/faf-text-web` — wasm-bindgen bindings: attach to a canvas, drive
  selection from pointer events, search highlighting, clipboard, and a
  display-only terminal mode that streams a synthetic log through a `TermGrid`
  sized to the canvas.
- `web/` — demo page.

## Running

Native smoke test (renders `offscreen.png`, no window needed):

```sh
cargo run --example offscreen -p faf-text
cargo run --example panes3d -p faf-text   # three text panes in 3D
cargo run --release --example term -p faf-text  # 200×60 terminal grid + box drawing
```

Web demo:

```sh
wasm-pack build crates/faf-text-web --target web --out-dir ../../web/pkg --release
python3 -m http.server -d web 8000
# open http://localhost:8000
```

## Current limitations

- A glyph that overflows a capped store falls back for the frame it overflowed
  in (curves → atlas, atlas → skipped glyph); it renders normally from the next
  frame on, once eviction has made room. A retained block built during such a
  frame keeps the fallback until its content is set again — `block_stale()`
  reports which blocks are in that state.
- A retained block's glyphs are kept warm in both stores and follow the curve
  texture through compaction, but the atlas's last-resort wholesale reset
  invalidates every UV: blocks with emoji in them lose those glyphs (and go
  stale) until they are re-set.
- Weight blending moves outlines, not advances: those come from shaping, at
  the attrs weight. Blending far from it reads tight or loose, so animate
  around the shaped weight rather than across the whole axis.
- Only the `wght` axis is interpolated, between its two extremes; other
  variation axes still bake into the extracted curves.
- `Subpixel::Rgb` runs no LCD filter: each stripe is a point sample a third of
  a pixel wide, so stem edges carry the full unfiltered orange/blue fringe. It
  is a real resolution win on a striped RGB panel at 11–13 px and visibly
  colored anywhere else (a rotated pane, a screenshot, a non-RGB panel). The
  usual fix is a five-tap filter across neighboring stripes, which costs more
  ray casts than the current three.
- `CoverageBlend::Linear` still reads lighter than the default for dark text on
  a light background even with the contrast correction: linear light *is*
  thinner there, and 1/1.43 only claws part of it back. It is the right mode for
  correctness (and for light-on-dark, where gamma blending over-fattens), not a
  drop-in replacement.
- Atlas glyphs (color emoji) stop snapping to the pixel grid once a block's
  placement is no longer an axis-aligned scale and translation — there is no
  grid to snap to — so they go slightly soft in 3D. Vector glyphs are exact at
  any angle.
- No depth buffer: blocks composite in draw order, and a host placing panes in
  3D sorts them back to front itself (`set_block_z`). Intersecting panes are
  not resolved per pixel.
- `TermGrid` translates the whole viewport every time it is asked, not just the
  cells that changed: at 0.12 ms for 12k cells the bookkeeping a per-cell damage
  map would need costs more than it saves. Idle frames skip the translation
  entirely (`take_dirty`).
- The grid draws box-drawing arcs (U+256D–U+2570) as sharp corners and
  diagonals as one-pixel staircases — rects are the only primitive it emits.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at
your option. Vendored fonts keep their own licenses (DejaVu: Bitstream Vera
derivative; Manrope: OFL 1.1, see `crates/faf-text/assets/`).
