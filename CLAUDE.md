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
  box has an RTX 3070; wgpu picks Vulkan). `examples/panes3d` does the same for
  3D panes (`panes3d.png`). The offscreen scene is a **regression baseline**:
  its md5 is **`ef89cd412c84ab51059c70f285ecd4a1`** — the union of #14's corner
  clipping (re-triangulated the two glyphs over the 48 px/em gate) and #15's
  COLR strip (canvas grown 600 → 880 px; the strip was *added below* on
  purpose, so rows 0–599 are byte-identical to the pre-#15 scene). Earlier
  baselines, for archaeology: `293e8f38` (#15 alone), `4f37b594` (#14 alone),
  `729ddb4c` (post-#13 f16), `beec6786` (post-#7 decorations). Any change to
  placement math has to keep the current md5; across a canvas resize, answer
  "did an existing pixel move" by building the pre-change tree in a throwaway
  `git worktree` with its own `CARGO_TARGET_DIR` and diffing the crops.
  `panes3d.png` is `80928801bd41af0a08cacf96772270c1` and f16 did *not* move
  it: at 3D-pane sizes the quantization stays under half a level of 8-bit
  coverage.
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
  frames). `stress` and `bench` both take `--linear` / `--subpixel` to run the
  same scene through the issue-11 options. `stress` is noisy run to run
  (1.80–2.10 ms/frame on the same build); `bench` is the stable one.
- Quality options: `cargo run --release --example quality -p faf-text` renders
  the three configurations side by side into `quality.png` plus an 8× crop in
  `quality-zoom.png`, and prints ink totals and R≠B pixel counts per panel.
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
- Documentation assets: `cargo run --release --example gallery -p faf-text`
  renders `site/gallery/` (a 1200×630 `hero.png` plus `zoom`/`weight`/`tilt`/
  `terminal` as APNG + a single-frame PNG sibling). `site/` is gitignored;
  `scripts/build-site.sh` assembles the gh-pages tree (landing page +
  `docs-loader.js` + `demo/` from `web/` + `gallery/`) and never runs the
  release wasm build itself.
- Live docs are split so behaviour is **evergreen**:
  `crates/faf-text/docs-header.html` is injected by
  `[package.metadata.docs.rs] rustdoc-args = ["--html-in-header", …]` and is
  frozen at publish time, so it carries only the cell CSS plus a stub that
  `import()`s `window.FAF_DOCS_LOADER ||
  https://meawoppl.github.io/faf-text/docs-loader.js` with `.catch(() => {})`.
  The driver itself is `web/docs-loader.js`, deployed to the *site root* and
  live for every published version at once: it upgrades the `.faf-live`
  placeholder divs into wasm canvases from
  `https://meawoppl.github.io/faf-text/demo/pkg/` (`window.FAF_DEMO_PKG`
  overrides). The frozen interface between them is the class names
  (`.faf-live` + `data-demo`/`data-height`/its `img` fallback,
  `.faf-live-grid`, `.faf-live-caption`) — new demo kinds are safe, renaming
  those is not. Both URLs are overridable, so a local test is: build docs with
  `RUSTDOCFLAGS="--html-in-header $(pwd)/crates/faf-text/docs-header.html"`,
  serve a tree holding `doc/`, `docs-loader.js` and `pkg/`, and inject a
  classic `<script>` setting the two globals (classic scripts run before the
  deferred module stub). The four cells do come up headless under SwiftShader,
  on the WebGL2 fallback; a 404 loader leaves the APNGs visible with no
  unhandled rejection.
- `--screenshot` + `--virtual-time-budget` is *unreliable* for the live-docs
  page: the cells' IntersectionObserver and the wasm boot race virtual time and
  half the runs capture zero canvases. Drive it over CDP in real time instead
  (`--remote-debugging-port=9222` + a node script over `/tmp/node_modules/ws`,
  navigate, wait ~20 s, `Runtime.evaluate` a DOM probe, then
  `Page.captureScreenshot` with `captureBeyondViewport`). Note `Runtime.enable`
  replays the *previous* page's console messages, so trust the probe's return
  value over the log order.
- No `include` list in `crates/faf-text/Cargo.toml` on purpose — the default
  (everything not gitignored) already ships `assets/`, both `.wgsl` files and
  `docs-header.html`; an `include` would silently drop them. `README.md` there
  is a symlink to the workspace one, which `cargo package` resolves.

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
- cosmic-text 0.19 **does** carry text decorations end to end (the issue-7 spec
  guessed it did not): `Attrs::underline(UnderlineStyle)/strikethrough()/
  overline()` shape into `LayoutRun::decorations`, a `&[DecorationSpan]` of
  glyph ranges plus offset/thickness **in em** (from skrifa's
  `post`/`OS/2`). `push_run_decorations` turns those into instances, so
  attributed text underlines itself; squiggles and chips have no attribute and
  stay manual. Its y convention is `line_y - offset * font_size` — offsets are
  y-up from the baseline at the *top* of the stroke.
- swash's own copy of the same numbers (`font.as_swash().metrics(&[])`) is
  `underline_offset` (post.underlinePosition), `strikeout_offset`
  (OS/2.yStrikeoutPosition), `stroke_size` (post.underlineSize — post wins over
  OS/2), all in design units, with `.scale(ppem)`/`.linear_scale(s)` to convert.
  `view.rs` caches them per `fontdb::ID` divided by `units_per_em`, because
  `decoration_rects` takes `&self` and `FontSystem::get_font` needs `&mut`.
  DejaVu Sans: -40 / 530 / 90 over 2048 upem.
- `font_system_from_fonts` scans system fonts on native, so `Family::SansSerif`
  in a test resolves to whatever *this box* calls sans-serif (Noto Sans here),
  not the embedded blob — tests that assert against a font's own metrics build
  a `FontSystem::new_with_locale_and_db` over a one-face `fontdb::Database`.
- WGSL derivatives must sit in uniform control flow, so `deco_fs` takes
  `fwidth(local)` **before** the kind switch and every shape reduces to a
  signed distance in local px; coverage is `clamp(d / aa + 0.5, 0, 1)`. The
  switch itself is free — it runs per decoration instance, not per glyph, and
  `examples/bench` is unmoved (0.643 ms/frame before and after).
- Winding-rule shader: classify ray crossings by control-point sign pattern
  (masks 0x454/0x1510), NEVER by root-range checks — font coords are exact
  1/64 fractions and rays graze endpoints exactly (FreeMono 'p' rendered
  mirrored-looking garbage until fixed). Hairline stems need the 3-tap path
  below ~24px.
- Glyph outlines extracted at size 1.0 + DISABLE_HINTING = pure em units.
- **Fixed bug worth remembering** — `CurveStore::flush` used a row-granular
  high-water mark (`total_rows <= uploaded_rows` early return), so a glyph
  extracted after a flush whose block fit inside the current partial row
  never reached the texture and drew as nothing, permanently. The fix tracks
  uploaded *floats* (`uploaded_floats`) and re-uploads from the row holding
  the first new float; regression test
  `flush_uploads_glyphs_that_fit_inside_a_partial_row`. Moral: upload
  high-water marks must use the finest granularity data grows by, not the
  granularity the transport (rows) prefers. `examples/gallery`'s `warm_up`
  predates the fix and is now just a determinism nicety.
- Curve-texture addressing is by *texel*, not curve record: `GlyphCurves.first`
  is a glyph's base texel and everything stored in the block (band header
  entries, index-list entries) is an offset from it, so compaction relocates a
  glyph by memcpy + rewriting `first`. Blocks are always an even number of
  texels so a two-texel curve record never straddles a row.
- The curve texture is **RGBA16F** (#13). Three things fall out of that and all
  three are load-bearing:
  - **Quantize in the flattener, not on upload.** `Flattener::push` rounds
    every control point to f16, so the bbox, band membership, the early-out
    sort keys and the shader all describe one outline. Rounding later would let
    the CPU sort by numbers the GPU does not have.
  - **f16 holds integers exactly only to 2048**, and band headers/index lists
    are integers in texels. `banded_block` returns `None` past that and the
    glyph keeps the flat layout (which addresses records with u32 arithmetic
    and stores no integers at all). DejaVu Sans and Manrope top out at 254, so
    it takes a ~550-curve glyph to trip.
  - **Endpoint sharing only in banded blocks.** p2 of a curve is p0 of the
    next within a contour (the flattener carries the point, so they are the
    same float), so a curve is one texel plus one terminator per contour and
    `fetch_pair`'s two-texel read is unchanged. The flat loop walks records by
    arithmetic (`i * 2`), which sharing would break — stepping over contour
    terminators needs an index list or a per-texel branch, and a branch in that
    loop has measured 25% here. Flat blocks are ≤16 curves, so it costs ~130
    bytes a glyph and keeps both paths bit-identical
    (`band_tables_do_not_move_a_single_pixel` still passes exactly).
- f16 quantization is worth **≤4.9e-4 em** (2^-11 near 1.0), but the *pixel*
  effect is not that number times px/em: the offscreen scene moved 2882 px of
  576000, 83% of them by ≤2/255, with a tail to **45/255** on near-tangent
  edges where a crossing position is ill-conditioned. Proven to be quantization
  and nothing else by rendering the same shared-endpoint layout through an
  RGBA32F texture: unquantized it is **byte-identical** to the pre-#13 scene,
  quantized it reproduces the RGBA16F render exactly. If a scene ever needs
  more precision than this, the move is per-glyph fixed point (Rgba16Unorm over
  the bbox, ~1e-5 em), not a wider float.
- Halving the curve-fetch bytes did **not** move frame time on the 3070 (bench
  0.549 → 0.545 ms/frame, stress 1.90 → 1.86 inside its ±0.2 noise). The whole
  offscreen curve set is ~107 KB and already sat in L2, and #12 had already cut
  the fetch *count*; #13 buys memory (2.6×) and bandwidth headroom for WebGL2
  and mobile, not frames here.
- RGBA16F is *better* supported than RGBA32F on WebGL2, not worse: wgpu's GLES
  backend gives it `TEXTURE_BINDING | FILTERABLE | STORAGE` with no extension,
  where `Rgba32Float` is sampled-but-unfilterable. A `Float { filterable:
  false }` bind-group layout accepts it (wgpu only rejects the other
  direction), so the curve binding did not change.
- Band tables (>16 curves) split the glyph's **em bbox**, not the instance
  quad: the quad's 1.5px pad is a size-dependent number of em, and bands are
  baked once per glyph. The renderer passes `band_scale`/`band_bias` to map the
  quad corner into band space. A band gets every curve whose control-point
  range overlaps it ±5% of a band height — leave a curve out and its winding
  contribution is exactly 0.0, which is why banding is bit-for-bit
  pixel-identical to the flat loop (assert this with the offscreen example).
- Sorted band lists + early-out + median split (#12) keep that bit-identity:
  every skipped curve contributes exactly 0.0, and reordering the adds turns
  out not to matter because a band+ray almost never has more than two *nonzero*
  terms (addition of two floats is commutative to the bit). `offscreen.png` and
  `panes3d.png` md5s are unchanged, and so is `band_tables_do_not_move_a_single_pixel`,
  which now also renders at 40px so the backward-ray path is covered.
- **The backward ray fires toward the *near* edge of the glyph, not the far
  one.** A forward (+axis) ray counts crossings *ahead* of the sample and stops
  at the first curve behind it, so it is cheap on the high side of a band and
  dear on the low side; samples below the median split must therefore fire
  backwards. Getting that comparison inverted still renders correctly (the two
  directions are mathematically equal) and cost 45% — `bench` went 0.63 → 0.85
  instead of → 0.59. Correct-but-slow is the failure mode to watch for here.
- Backward rays need no branch in `curve_winding`: pass a **negative**
  `inv_diameter` (which turns saturate(0.5 + m·Cx) into saturate(0.5 − m·Cx))
  and negate the band's sum (which undoes the crossing-sign swap). The identity
  saturate(0.5 − x) = 1 − saturate(0.5 + x) plus "signed crossings over a whole
  line sum to zero" makes the two directions agree exactly.
- Corner clipping (#14) is **geometry, and only geometry**: the vertex shader
  turns the quad into an octagon from a `vec4` of per-instance clip legs, the
  curve texture is untouched, and the four clips live on `GlyphCurves` (CPU
  side). Things that turned out to matter:
  - **Bound the curve, not the control hull.** A quarter circle's middle
    control point sits *exactly* on the corner of its bounding box, so
    control-point support planes clip no round glyph at all (DejaVu's 'O', 'e',
    'c', the bowl of a 'Q' — nothing). `dot(B(t), n)` is a scalar quadratic;
    its max over [0,1] is an endpoint or the vertex, three lines of code, and
    just as conservative — the fragment shader tests the curve, not the hull.
  - Blend masters for free: a Bézier is linear in its control points, so the
    blended curve is the pointwise blend and `max(h_a, h_b)` bounds every
    `weight_t`.
  - **A differently-triangulated quad is not byte-identical.** The rasterizer
    builds attribute gradients from the actual vertices, so re-splitting the
    same rectangle along the other diagonal moves interpolated varyings by an
    ulp and `fwidth` by ~1e-4 relative — 88 pixels of the offscreen scene, up
    to 98/255, when the *whole* scene switches. Hence the fan is ordered so the
    unclipped case emits `quad_corner`'s exact six vertices in its exact order,
    and blocks with nothing clipped still `draw(0..6)`. With clipping on, the
    two glyphs past the gate move 72 pixels by at most 1/255 (total ink
    75624048 → 75624055): that is the interpolation wobble, not lost ink, and
    it is why the offscreen md5 changed.
  - Savings are real but modest and *scene-dependent*: ~5% of a glyph's bbox on
    average over ASCII in DejaVu Sans (the 4%-of-bbox threshold rejects the
    marginal corners), 5.3% of fragment invocations on a 64 px/em bench page,
    11% on a large-glyph sample, 1.2% on the whole offscreen scene (where only
    a 64 px title and a 300 px "Qg" clear the 48 px/em gate).
- Pick the ray direction with `select`, never with an `if` around two copies of
  the loop: neighbouring fragments land on opposite sides of a split constantly,
  and a warp executing both copies eats the whole win.
- The early-out's *granularity* is size-dependent and that is worth 7%: testing
  the break after every curve makes each fetch wait on the previous compare,
  which is a loss at 11px (`stress` 1.87 → 2.0) and a win at 32px (`bench` 0.59
  → 0.54). Below the size gate the break is tested once per index texel, on the
  last of the four — the list is sorted, so that curve's bound is the smallest
  of the group and the test is just as conservative.
- A sorted list's key must bound **both** masters (max over A's and B's control
  points), and the shader's early-out has to compare that same two-master
  number — not the blended outline's max, which is smaller and would break the
  loop in front of a curve that still crosses the ray. `fetch_pair` keeps the
  masters apart for exactly that; `fetch_curve` is `blend_pair(fetch_pair(…))`
  so the flat path is unchanged.
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
  so a resize writes 16 bytes instead of dirtying every block.
- **The block matrix is px → homogeneous *screen pixels*, not px → clip**, and
  that choice is load-bearing. A clip-space matrix makes the shader compute
  `px * (2/W) - 1` where it used to compute `px / W * 2 - 1`; those differ by
  an ulp for ~54% of x values, the vertex lands in a different 1/256 subpixel
  bin about 1% of the time, and the offscreen scene came back with 82 pixels
  changed (max Δ2) — visually identical, md5 not. Keeping the divide in
  `project()` and folding a host's view-projection into pixel space on the CPU
  (`px_from_clip(size) * vp * model`) makes a 2D block's matrix a pure
  translation, so the shader adds exactly like it always did and the scene is
  byte-identical. The default projection is `None`, *symbolically* the
  identity, so blocks upload their model matrix untouched (never
  `px_from_clip * screen_ortho * model`, which is only numerically the
  identity) and a 2D resize still dirties nothing.
- Consequences of the pixel-space matrix: pixel snapping for the atlas path is
  `round(p.xy)` in the vertex shader, gated on a flag derived when the matrix
  is set (`is_axis_aligned`); a host projection makes every block's uniform
  depend on the surface size, so `finish` re-derives them on resize; and the
  block's z passes straight through to clip z, so a block rotated out of the
  z = 0 plane *without* a view-projection is clipped away by the 0..1 depth
  range (`math::screen_perspective` is the fix).
- `math::screen_perspective(size, fov)` puts the eye at `(w/2, h/2, -h/2·cot(fov/2))`
  looking down +z with world y down, which makes the projection agree with the
  2D ortho exactly at z = 0: turning 3D on never moves flat content.
- Hit-testing is `pointer_ndc` → `ndc_ray` → `block_hit`, and the arithmetic is
  f64 inside: in f32 the round trip through an inverted perspective was 0.012
  px off, which is more than the 0.01 px the tests want.
- A strongly tilted pane is only hit-testable where it faces the camera —
  under perspective the eye can be on the *back* side of the plane for part of
  a pane even when the pane's pivot faces it. Test round trips near the pivot,
  and pivot on the camera axis for grazing angles.
- glam 0.33 deprecates `Mat4::perspective_lh`; the wgpu-convention replacement
  is `glam::camera::lh::proj::directx::perspective` (LH view space in, y-up
  NDC with 0..1 depth out).
- Headless-Chrome screenshots of an *animating* demo can capture before the
  first frame: with a per-frame sway on, `--virtual-time-budget=60000` never
  gets through the rAF backlog under SwiftShader and the shot lands during
  startup. Freeze the animation (sway 0) for a screenshot check, or use a page
  with less text.
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
- A block owns seven arena spans, drawn in that order: under-rects, chips,
  vector glyphs, blended glyphs, atlas glyphs, line decorations, over-rects.
  Chips and line decorations are the same instance type and the same pipeline,
  split into two spans purely so the glyphs can be drawn between them.
- Per-block layering is *within* a block; blocks composite in draw order,
  which is `set_block_z` then creation order — block ids ascend, so sorting by
  `(z, id)` *is* the stable insertion-order tiebreak.
  A selection underlay therefore belongs in a block created *before* the text
  block, not in the text block's `under_rects` — which is why the web demo has
  four blocks (selection, text, search, caret) rather than three. All four
  share one model matrix under the demo's 3D tilt, which is what keeps the
  caret and the selection glued to the glyphs.
- Quality options (#11) are pipeline variants, never uniforms, for the same
  reason `BLEND_MASTERS` is: `LINEAR_BLEND` gates the contrast correction in
  `shape_coverage` and folds away when off, and passing `("LINEAR_BLEND", 0.0)`
  explicitly is byte-identical to passing no constants at all (offscreen md5
  unchanged, `bench` 0.643 → 0.643 ms).
- LCD subpixel needs `enable dual_source_blending;`, which naga validates
  against the *device's* capabilities — a module containing it cannot be
  created without `wgpu::Features::DUAL_SOURCE_BLENDING`. So `subpixel.wgsl` is
  a second module, built at runtime as `"enable …" + shaders.wgsl +
  subpixel.wgsl` and only when the feature is present; the base module never
  mentions it and WebGL2 is unaffected. The RTX 3070 (Vulkan) has the feature;
  `testing::dual_source_gpu()` hands out a device with it or `None`.
- Subpixel coverage costs three x casts plus the y pass: at 11px that is the
  same six casts the grayscale 3-tap path runs (stress unmoved within noise),
  at 32px it is 4 vs 2 and the frame doubles (bench 0.643 → 1.270 ms). Linear
  blending is ~free (0.647 ms).
- The issue's "y cast multiplies in" for subpixel is wrong and the code
  averages instead: both casts measure the *same* vertical edge on a stem, so a
  product squares its coverage. Averaging keeps subpixel and grayscale at the
  same weight — panel ink in `examples/quality` matches to 5 significant
  figures (58753047 vs 58753326).
- Per-block subpixel gating reuses the `is_axis_aligned` predicate behind
  `BLOCK_SNAP` (plus "not mirrored in x", which would swap R and B), evaluated
  in `upload_uniforms` where the composed matrix already exists, and cached on
  the block so `render(&self)` can pick a pipeline per block.

- COLR/CPAL (`colr.rs`, #15): a COLRv0 color glyph is a *stack of ordinary
  glyphs*, so the whole feature is a cache plus a loop in `push_color_layers` —
  the layers extract through `CurveStore::get_or_insert` untouched and the
  renderer emits one vector instance each, in COLR order (instances draw in
  queue order, which *is* the painter's algorithm the format asks for).
  Palette 0, and the run color's alpha multiplies the palette alpha; palette
  index 0xFFFF means "the text color" and takes the run's color whole.
- ttf-parser 0.25 is already in the tree via fontdb, but cosmic-text does not
  re-export it, so `cargo add ttf-parser` (same version, one copy in the lock).
  It parses the sfnt/collection table directory and CPAL (BGRA behind a
  per-palette index array); the COLRv0 base-glyph/layer arrays are read by hand
  because ttf-parser only exposes them through a v1-shaped `Painter` trait,
  which never says *which* layer asked for 0xFFFF. Font bytes come from
  `FontSystem::get_font(id, weight).data()` plus `db().face(id).index`.
- `ColrCache` caches two things and the coarse one is load-bearing: a per-font
  "has usable COLRv0" bool, so an ordinary text run does not leave a "not a
  color glyph" entry per letter. COLRv1 → the whole font is marked unusable and
  goes to the bitmap atlas (gradients/transforms/composites have no expression
  in the winding shader), which is also where CBDT (NotoColorEmoji), sbix and
  SVG stay.
- A color glyph is all-or-nothing per frame: if the curve store overflows
  partway through a layer stack, `push_color_layers` truncates the instances it
  pushed and returns false so the glyph falls back to the atlas whole. Half a
  rocket is worse than a soft one.
- Terminal grid (`grid.rs`): char → glyph id is
  `font.as_swash().charmap().map(ch)` (0 = miss), and the advance is
  `font.as_swash().glyph_metrics(&[]).scale(px).advance_width(id)`. Both are
  cached per face in `GridFont`. `Family::Monospace` does **not** resolve in a
  fonts-only `FontSystem` — cosmic-text sets the db's monospace default to
  "Noto Sans Mono", which a blob-only database has never heard of — so
  `resolve_face` falls back through (family, normal style), `Monospace`, and
  finally the *last* face in the db.
- Cell metrics are **rounded to whole pixels** (advance of `M`, and ascent +
  descent + leading), and that is load-bearing: procedural box drawing centers
  its strokes at `round((cell - thickness) / 2)` in every cell, so two
  neighbours agree on the offset to the pixel and a join has no seam. Fractional
  cells put the same stroke at different subpixel offsets per column and the
  seams stipple.
- Box drawing's double-line rule: a double arm is two light rails at ±light
  around where the single bar would be, and a rail's middle is omitted exactly
  where a *double* perpendicular arm crosses it. That one rule gives ╬ four
  corners, ╠ a continuous left rail and a broken right one, and ═ two full
  rails; a *single* perpendicular arm never breaks a rail, it just runs into it
  (which is why ╤'s stem starts at the lower rail).
- The East Asian Width table is generated from
  `unicode.org/Public/UCD/latest/ucd/EastAsianWidth.txt` (17.0.0 as of writing):
  keep W and F, add the blocks the file documents as defaulting to W for
  unassigned code points (CJK Ext A, CJK Unified, CJK Compat, planes 2 and 3),
  sort and merge. `ｱ` U+FF71 is *halfwidth* (class H, narrow) — the fullwidth
  twin is U+30A2.
- `BlockContent::glyphs` / `TextRenderer::glyphs` take `GlyphSpec`s (font id,
  glyph id, size, pen, color, weight) and skip shaping. `push_text` and
  `push_glyphs` share `vector_instance` and `push_atlas_glyph`; the shared
  helpers are exactly the old arithmetic, and the offscreen md5 is unchanged,
  which is how that refactor was checked.
- Grid → instance translation for 200×60 colored log cells is **0.12 ms**
  (mean, release, this box), producing ~4.4k instances; `set_block_content` +
  `finish` is another ~0.26 ms. `cargo run --release --example term` reports it.
- The web demo's terminal toggle (`set_terminal`) hides the four editable
  blocks with `set_block_visible` instead of emptying them, and refuses every
  edit entry point while it is on, so toggling back restores the text *and* the
  caret/selection; toggling off still re-uploads from the view (`Dirty::all`)
  rather than trusting instances that sat hidden through an eviction. The grid
  lives in a fifth block created last (so it composites on top) and keeps a
  plain offset even under the 3D tilt — at z = 0 the perspective camera agrees
  with the 2D projection, so flat content does not move. A streaming log
  scrolls the header off the top every line, so the header is simply redrawn
  after each `scroll_up` — three rows, cheap, and always in frame.
- The fps overlay (`faf-text-web/src/stats.rs`, `set_stats_overlay`) is drawn by
  the renderer — a sixth block, created lazily on first enable so it is the
  newest block and composites over the grid. It reads
  `webgl2 · 62 fps · 16.1 ms`: the backend tag comes from the wgpu debug name
  (`BrowserWebGpu` → `webgpu`, `Gl` → `webgl2`), so every docs.rs cell says
  which backend it got. It measures **presented**
  frames, which makes it circular: refreshing the readout dirties a block,
  which is damage, which presents, which would be a sample. The rule that
  breaks the loop is an *ordering* in `render_frame`: sync everything else,
  read `damaged()` (at that point it can only be about other content), pass
  that bool to `StatsOverlay::record`, and only then let the overlay touch its
  own block. So the readout's own 4 Hz updates present frames but never count
  themselves, and an idle demo falls to `idle` via the 600 ms gap rule and then
  stops presenting entirely (the label stops changing → nothing dirty).
- Driving the demo page for a browser check *interactively* (toggle a control,
  then screenshot) needs CDP, not `--screenshot`: `google-chrome --headless=new
  --use-angle=swiftshader --enable-unsafe-swiftshader --remote-debugging-port=
  9222`, then a node script over `ws` (`npm i ws`) doing `Runtime.evaluate` +
  `Page.captureScreenshot`. Real time runs normally there, so rAF animation
  screenshots land where expected and no virtual-time budget is involved.
- The debugging trick that cracked the shader bug: replicate the WGSL math in
  a Python simulator over real outline data (`/tmp/sim_shader.py` pattern) —
  GPU-vs-CPU divergence becomes printable ASCII art.
