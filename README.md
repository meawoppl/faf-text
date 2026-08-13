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

A full frame is at most four draw calls: under-rects, vector glyphs, atlas
glyphs, over-rects.

## Compatibility

Everything is plain `wgpu` with WebGL2-safe choices — curve data lives in a
texture (`textureLoad`), not a storage buffer — so the same shaders run on
Vulkan/Metal/DX12/GL natively, and WebGPU or WebGL2 in the browser.

## Crates

- `crates/faf-text` — the renderer. `TextRenderer` (pipelines + glyph sources),
  `TextView` (positioned cosmic-text buffer with hit-testing/selection
  helpers). No windowing dependencies.
- `crates/faf-text-web` — wasm-bindgen bindings: attach to a canvas, drive
  selection from pointer events, search highlighting, clipboard.
- `web/` — demo page.

## Running

Native smoke test (renders `offscreen.png`, no window needed):

```sh
cargo run --example offscreen -p faf-text
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
  frame on, once eviction has made room.
- Variable-font weight is applied by swash at extraction; each weight gets its
  own curve set (correct, just not deduplicated).
- No gamma-aware blending option yet; text blends in the surface's space.
