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
  selection from pointer events, search highlighting, clipboard.
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
- No gamma-aware blending option yet; text blends in the surface's space.
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
