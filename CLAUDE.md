# faf-text

GPU text renderer: vector glyphs evaluated in the fragment shader (winding
rule over quadratic Béziers in a data texture), bitmap atlas fallback for
emoji, rect layers for selection/highlight. See README.md for architecture.

Project goal: a rich text interface usable in 2D panes and eventually 3D
spaces. The fwidth-based analytic AA is perspective-correct by construction,
so 3D is plumbing (per-pane matrices, ray hit-testing — issue #9), not an
algorithm change. Roadmap lives in issues #2–#11; Slug-paper (Lengyel 2017) performance follow-ups in #12–#15.

## Repo patterns

- Workspace: `crates/faf-text` (core, no windowing deps), `crates/faf-text-web`
  (wasm-bindgen), `web/` (demo page, `web/pkg` is wasm-pack output, gitignored).
- Visual verification without a window: `cargo run --example offscreen -p
  faf-text` renders every feature to `offscreen.png` (works headless — this
  box has an RTX 3070; wgpu picks Vulkan).
- Wasm build: `~/.cargo/bin/wasm-pack build crates/faf-text-web --target web
  --out-dir ../../web/pkg --release` (wasm-pack is in ~/.cargo/bin, which is
  NOT on PATH). Release builds spend minutes in wasm-opt; run in background.
- Serve demo: `python3 -m http.server -d web <port>`.
- `shaders.wgsl` constants `CURVE_TEX_WIDTH`, `BANDS` and `BANDED_FLAG` must
  match `curves.rs`; `curves::tests::shader_constants_match_the_record_layout`
  greps the WGSL source to enforce it. Only the width is shared — the curve
  texture's height changes at runtime.
- Fragment-shader benchmark: `cargo run --release --example stress -p faf-text`
  (a full 960×600 frame of 11px text, ~7.9k glyphs, timed over 200 serialized
  frames).
- GPU unit tests are fine and expected: `src/testing.rs` (cfg(test)) hands out
  one shared headless device plus `render_pixels` for readback comparisons.
  `cargo test -p faf-text` exercises curve-texture growth and eviction on the
  real GPU. The clear color is opaque black, so readbacks come back with alpha
  255 everywhere: "drew nothing" must be asserted on RGB (`tests::drew`), never
  on "every byte is zero".
- Browser check: `python3 -m http.server -d web PORT` plus `google-chrome
  --headless=new --use-angle=swiftshader --enable-unsafe-swiftshader
  --enable-logging=stderr --screenshot=… --virtual-time-budget=60000`. The
  budget has to be generous: with the damage fast-out an idle demo presents
  nothing, virtual time races ahead, and a smaller budget screenshots before
  the first rAF tick even runs. `console.log` shows up as `INFO:CONSOLE`.

## Hard-won knowledge

- wgpu 30 API deltas vs older docs: `bind_group_layouts: &[Option<&_>]`,
  `PipelineLayoutDescriptor::immediate_size`, `VertexState::buffers:
  &[Option<_>]`, `RenderPipelineDescriptor::multiview_mask`,
  `RenderPassColorAttachment::depth_slice`, `PollType::wait_indefinitely()`,
  `get_mapped_range()` returns Result, `queue.present(frame)` (not
  `frame.present()`), `SurfaceConfiguration::color_space`,
  `get_current_texture()` returns `CurrentSurfaceTexture` enum (not Result).
- cosmic-text 0.19: `set_text`/`set_size` no longer take `FontSystem`; call
  `shape_until_scroll(&mut fs, false)` after mutations. `LayoutRun::highlight`
  has NO line-range guard — runs on lines outside the cursor range come back
  fully selected; `TextView::selection_rects` filters `line_i` for this.
- Winding-rule shader: classify ray crossings by control-point sign pattern
  (masks 0x454/0x1510), NEVER by root-range checks — font coords are exact
  1/64 fractions and rays graze endpoints exactly (FreeMono 'p' rendered
  mirrored-looking garbage until fixed). Hairline stems need the 3-tap path
  below ~24px.
- Glyph outlines extracted at size 1.0 + DISABLE_HINTING = pure em units.
- Curve-texture addressing is by *texel*, not curve record: `GlyphCurves.first`
  is a glyph's base texel and everything stored in the block (band header
  entries, index-list entries) is an offset from it, so compaction relocates a
  glyph by memcpy + rewriting `first`. Blocks are always an even number of
  texels so a two-texel curve record never straddles a row.
- Band tables (>16 curves) split the glyph's **em bbox**, not the instance
  quad: the quad's 1.5px pad is a size-dependent number of em, and bands are
  baked once per glyph. The renderer passes `band_scale`/`band_bias` to map the
  quad corner into band space. A band gets every curve whose control-point
  range overlaps it ±5% of a band height — leave a curve out and its winding
  contribution is exactly 0.0, which is why banding is bit-for-bit
  pixel-identical to the flat loop (assert this with the offscreen example).
- Variable weight: a `wght` face is extracted at both axis ends and master B's
  records go in a parallel region after A's, so one constant (`b_offset =
  count * 2` texels) reaches any twin and band lists keep indexing A. Band
  membership is decided over *both* masters' control-point ranges, or a blend
  could put a curve in a band that omitted it.
- A per-curve `if b_base != 0u` in the fragment shader cost **25%** even when
  never taken (0.64 → 0.80 ms in `examples/bench`). The fix is the WGSL
  `override BLEND_MASTERS` plus two pipelines built from the same module with
  `PipelineCompilationOptions::constants`; the static path then compiles to
  exactly the old shader. Overrides work fine on wgpu 30 + naga.
- `FontSystem::new_with_fonts` also loads **system fonts** on native (cosmic's
  `load_fonts` calls `db.load_system_fonts()` first), so an embedded blob is
  the *last* face in the database, not the first — look faces up by family
  (`testing::font_id_of`), never by position.
- Glyph stores never evict mid-frame: queued instances hold raw `first` indices
  and atlas UVs, so overflow sets a flag, falls back for that frame, and the
  eviction runs in `begin_frame`. Growth/compaction bump `CurveStore.generation`
  and `TextRenderer::finish` rebuilds the bind group when it changes.
- `wgpu::Device` is `Clone` (an Arc handle), so `CurveStore` keeps its own copy
  and can recreate its texture without threading a `&Device` through `text()`.
- Retained scene graph (`renderer.rs` + `arena.rs`): a block owns one `Span`
  (start/cap/len, in instances) per arena and one slot in a dynamic-offset
  uniform buffer, stride `max(256, min_uniform_buffer_offset_alignment)`.
  Screen size stayed a separate global rather than being duplicated per block,
  so a resize writes 16 bytes instead of dirtying every block; the block
  uniform holds only placement, which is what #9 widens to a mat4.
- Arena mirrors are `Vec<u32>`, not `Vec<u8>`: a byte vec is only 1-aligned and
  `bytemuck::cast_slice` to an instance type may panic on it.
- **WebGL2 has no base-instance draw**, so a block's range is addressed by
  offsetting the vertex buffer (`buffer.slice(byte_offset..)`, `draw(0..6,
  0..len)`), never by `draw(.., first..last)`.
- Retained instances bake raw curve texel bases and atlas UVs, which the stores
  are otherwise free to move at a frame edge. Three things keep that honest:
  `begin_frame` touches every live block's glyph keys *before* the stores
  evict (so live content is never in the LRU's cold half),
  `CurveStore::relocations` (old base → new) lets compaction be patched in
  place, and the atlas's wholesale reset bumps `Atlas::generation`, which
  blanks retained atlas quads and marks those blocks stale.
- Per-block layering is *within* a block; blocks composite in creation order.
  A selection underlay therefore belongs in a block created *before* the text
  block, not in the text block's `under_rects` — which is why the web demo has
  four blocks (selection, text, search, caret) rather than three.
- The debugging trick that cracked the shader bug: replicate the WGSL math in
  a Python simulator over real outline data (`/tmp/sim_shader.py` pattern) —
  GPU-vs-CPU divergence becomes printable ASCII art.
