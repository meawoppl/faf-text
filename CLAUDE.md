# faf-text

GPU text renderer: vector glyphs evaluated in the fragment shader (winding
rule over quadratic Béziers in a data texture), bitmap atlas fallback for
emoji, rect layers for selection/highlight. See README.md for architecture.

Project goal: a rich text interface usable in 2D panes and eventually 3D
spaces. The fwidth-based analytic AA is perspective-correct by construction,
so 3D is plumbing (per-pane matrices, ray hit-testing — issue #9), not an
algorithm change. Roadmap lives in issues #2–#11.

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
- `shaders.wgsl` constants `CURVE_TEX_WIDTH` must match `curves.rs`.

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
- The debugging trick that cracked the shader bug: replicate the WGSL math in
  a Python simulator over real outline data (`/tmp/sim_shader.py` pattern) —
  GPU-vs-CPU divergence becomes printable ASCII art.
