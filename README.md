# faf-text

A GPU text renderer that is fast as f***. Native and WebAssembly, WebGPU and
WebGL2, one code path.

![faf-text](https://raw.githubusercontent.com/meawoppl/faf-text/gh-pages/gallery/hero.png)

**[Try the live demo →](https://meawoppl.github.io/faf-text/demo/)** · [docs.rs](https://docs.rs/faf-text)

Glyph outlines are evaluated *per pixel* in the fragment shader with the
non-zero winding rule. There is no atlas for outlines, no SDF bake and no MSAA,
so scaling is free, positioning is exactly subpixel, and text stays correctly
antialiased at any angle in 3D.

| | |
| --- | --- |
| ![font size sweeping 10 to 80 px](https://raw.githubusercontent.com/meawoppl/faf-text/gh-pages/gallery/zoom.apng) | ![a variable font's weight axis blended on the GPU](https://raw.githubusercontent.com/meawoppl/faf-text/gh-pages/gallery/weight.apng) |
| **Zoom re-rasterizes nothing.** The curve data is byte-identical in every frame. | **Variable weight is a GPU lerp** between two masters, mixed before the winding test. |
| ![a pane of text turning in 3D](https://raw.githubusercontent.com/meawoppl/faf-text/gh-pages/gallery/tilt.apng) | ![a colored log streaming through a terminal grid](https://raw.githubusercontent.com/meawoppl/faf-text/gh-pages/gallery/terminal.apng) |
| **A pane in 3D costs one matrix a frame.** Coverage is measured on the projected glyph. | **Terminal cells skip the shaper entirely,** and box drawing is generated, not shaped. |

Those are animated PNGs, rendered headless by `cargo run --example gallery -p
faf-text` and served from the `gh-pages` branch; on [docs.rs](https://docs.rs/faf-text)
the same four cells are live wasm canvases.

## How it works

Each glyph's Bézier outline is extracted once, in pure em units, flattened to
quadratics, and packed into an RGBA16F data texture; a fragment shader then
answers inside-or-outside per pixel with the non-zero winding rule, crossings
classified by control-point sign patterns and antialiasing computed in closed
form from the crossing positions. Band tables and sorted early-outs cut the
per-fragment loop to a fraction of the outline — bit-identically. Variable
weight is a control-point lerp on the GPU, COLRv0 emoji are layer stacks
through the same shader, whatever is not curves rides a bitmap atlas, and the
retained, damage-tracked scene means an idle frame draws nothing at all.

**[Read the full explainer, with live figures →](https://meawoppl.github.io/faf-text/how-it-works/)**
Eight sections, every figure drawn from the renderer's own data.

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

Documentation assets and the `gh-pages` tree:

```sh
cargo run --release --example gallery -p faf-text  # site/gallery/*.png, *.apng
scripts/build-site.sh                              # site/ = landing + demo + gallery
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
- Color glyphs take the vector path only for COLR **v0**. COLRv1 is detected
  and sent to the bitmap atlas whole-font, so a v1 font's v0 compatibility
  records (where it has any) are not used either. Layers resolve against
  palette 0; there is no palette selection API.
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
derivative; Manrope: OFL 1.1; Twemoji Mozilla subset: Apache-2.0 build,
CC-BY 4.0 artwork — see `crates/faf-text/assets/`).
