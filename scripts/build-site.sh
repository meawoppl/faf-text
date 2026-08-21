#!/usr/bin/env bash
#
# Assemble the gh-pages tree into site/.
#
#   site/index.html      landing page (written below)
#   site/docs-loader.js  the driver docs.rs pages import for their live cells
#   site/demo/           the interactive demo: web/index.html + web/pkg
#   site/gallery/        hero.png plus the four APNG animations and their stills
#
# The demo's wasm bundle is *copied*, never built here — a release wasm-pack
# build spends minutes in wasm-opt, which has no business inside a site script.
# Build it first with:
#
#   ~/.cargo/bin/wasm-pack build crates/faf-text-web --target web \
#       --out-dir ../../web/pkg --release
#
# Then publish site/ to the gh-pages branch. The URLs the crate docs and the
# README point at assume that layout:
#
#   https://meawoppl.github.io/faf-text/demo/
#   https://meawoppl.github.io/faf-text/docs-loader.js
#   https://raw.githubusercontent.com/meawoppl/faf-text/gh-pages/gallery/hero.png
#
# Usage: scripts/build-site.sh [--skip-gallery]

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
site="$root/site"
skip_gallery=0
[[ "${1:-}" == "--skip-gallery" ]] && skip_gallery=1

echo "==> site root: $site"
rm -rf "$site/demo"
mkdir -p "$site/demo" "$site/gallery"

# 1. Gallery. The generator renders everything headless on the GPU and writes
#    straight into site/gallery.
if [[ $skip_gallery -eq 0 ]]; then
  echo "==> rendering gallery"
  (cd "$root" && cargo run --release --quiet --example gallery -p faf-text)
else
  echo "==> skipping gallery (--skip-gallery)"
fi
for asset in hero.png zoom.apng zoom.png weight.apng weight.png tilt.apng tilt.png \
  terminal.apng terminal.png; do
  [[ -f "$site/gallery/$asset" ]] ||
    echo "    warning: site/gallery/$asset is missing"
done

# 2. Demo. web/pkg is wasm-pack output and gitignored, so it may simply not be
#    there yet.
echo "==> copying demo"
cp "$root/web/index.html" "$site/demo/index.html"
if [[ -d "$root/web/pkg" ]]; then
  cp -r "$root/web/pkg" "$site/demo/pkg"
  rm -f "$site/demo/pkg/.gitignore"
else
  echo "    warning: web/pkg not found — the demo page will not load."
  echo "    build it with:"
  echo "      ~/.cargo/bin/wasm-pack build crates/faf-text-web --target web \\"
  echo "          --out-dir ../../web/pkg --release"
fi

# 3. Live-docs loader. Every published version's docs.rs pages import this from
#    the site root, so it ships at the top level and its URL is permanent.
echo "==> copying docs-loader.js"
cp "$root/web/docs-loader.js" "$site/docs-loader.js"

# 3b. The how-it-works explainer. Static files only; its wasm imports resolve
#     to the demo bundle copied above (lib/renderer.js tries ../pkg/ first for
#     local serving, then ../demo/pkg/ — the deployed layout).
echo "==> copying how-it-works"
rm -rf "$site/how-it-works"
cp -r "$root/web/how-it-works" "$site/how-it-works"

# 4. Landing page.
echo "==> writing index.html"
cat >"$site/index.html" <<'HTML'
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>faf-text — a GPU text renderer</title>
<meta name="description" content="A GPU text renderer: glyph outlines evaluated per pixel in the fragment shader with the non-zero winding rule. Native and WebAssembly, WebGPU and WebGL2, one code path.">
<meta property="og:title" content="faf-text">
<meta property="og:description" content="Glyph outlines evaluated per pixel in the fragment shader. Free scaling, true subpixel positioning, variable weight as a GPU lerp.">
<meta property="og:image" content="https://meawoppl.github.io/faf-text/gallery/hero.png">
<meta name="twitter:card" content="summary_large_image">
<style>
  :root { color-scheme: dark; }
  * { box-sizing: border-box; }
  body {
    margin: 0; background: #15161e; color: #c0caf5;
    font: 16px/1.6 system-ui, -apple-system, "Segoe UI", sans-serif;
    -webkit-font-smoothing: antialiased;
  }
  main { max-width: 940px; margin: 0 auto; padding: 48px 24px 96px; }
  h1 { font-size: 15px; letter-spacing: 0.14em; text-transform: uppercase;
       color: #565f89; margin: 0 0 24px; font-weight: 600; }
  h2 { font-size: 20px; color: #7aa2f7; margin: 48px 0 12px; font-weight: 600; }
  p { color: #a9b1d6; margin: 0 0 16px; }
  a { color: #7dcfff; }
  img { display: block; width: 100%; height: auto; border-radius: 8px;
        border: 1px solid #2a2c3a; background: #1a1b26; }
  .hero { margin: 0 0 32px; }
  .links { display: flex; flex-wrap: wrap; gap: 12px; margin: 0 0 8px; padding: 0; list-style: none; }
  .links a {
    display: inline-block; padding: 10px 18px; border-radius: 8px;
    background: #1f2130; border: 1px solid #2a2c3a; color: #c0caf5;
    text-decoration: none; font-weight: 600;
  }
  .links a:hover { border-color: #7aa2f7; color: #7aa2f7; }
  .links a.primary { background: #7aa2f7; border-color: #7aa2f7; color: #15161e; }
  .links a.primary:hover { background: #9ab4f9; border-color: #9ab4f9; color: #15161e; }
  .card { display: block; margin: 24px 0 0; padding: 18px 22px; border-radius: 10px;
          background: #1a1b26; border: 1px solid #2a2c3a; text-decoration: none; }
  .card:hover { border-color: #7aa2f7; }
  .card strong { color: #7aa2f7; font-size: 17px; }
  .card p { color: #a9b1d6; margin: 8px 0 0; font-size: 15px; }
  .grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(300px, 1fr)); gap: 20px; }
  figure { margin: 0; }
  figcaption { color: #565f89; font-size: 14px; margin-top: 8px; }
  code { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 0.9em; color: #9ece6a; }
  ul.facts { color: #a9b1d6; padding-left: 20px; }
  ul.facts li { margin-bottom: 8px; }
  footer { color: #565f89; font-size: 14px; margin-top: 64px;
           border-top: 1px solid #2a2c3a; padding-top: 20px; }
</style>
</head>
<body>
<main>
  <h1>faf-text</h1>

  <img class="hero" src="gallery/hero.png" alt="faf-text: a GPU text renderer">

  <p>Every glyph is a quadratic Bézier outline evaluated <em>per pixel</em> in
  the fragment shader with the non-zero winding rule, with analytic
  antialiasing baked into the crossing clamp. No glyph atlas for outlines, no
  SDF bake, no MSAA — so scaling is free, positioning is exactly subpixel, and
  a pane tilted in 3D is as smooth as a flat one.</p>

  <ul class="links">
    <li><a class="primary" href="demo/">Live demo</a></li>
    <li><a href="how-it-works/">How it works</a></li>
    <li><a href="https://docs.rs/faf-text">Documentation</a></li>
    <li><a href="https://github.com/meawoppl/faf-text">Source</a></li>
  </ul>
  <p><small style="color:#565f89">The demo needs WebGPU or WebGL2; it is
  editable — click the text and type.</small></p>

  <a class="card" href="how-it-works/">
    <strong>How it works — the full explainer</strong>
    <p>Eight sections, from the bytes of a font file to the fragment shader's
    winding loop, band tables, f16 texels, corner clipping, variable weight,
    COLR emoji, 3D panes and the benchmark scorecard. Every figure is drawn
    from the renderer's real data, and the live cells are the renderer itself
    running in your browser.</p>
  </a>

  <h2>What it does</h2>
  <div class="grid">
    <figure>
      <img src="gallery/zoom.apng" alt="font size sweeping from 10 to 80 px">
      <figcaption>Zoom re-rasterizes nothing: the curve data is identical in
      every one of these frames.</figcaption>
    </figure>
    <figure>
      <img src="gallery/weight.apng" alt="a variable font's weight axis animating">
      <figcaption>Variable weight is a GPU lerp between two masters, mixed
      before the winding test.</figcaption>
    </figure>
    <figure>
      <img src="gallery/tilt.apng" alt="a pane of text turning in 3D">
      <figcaption>A pane in 3D costs one matrix a frame. Coverage is measured
      on the projected glyph.</figcaption>
    </figure>
    <figure>
      <img src="gallery/terminal.apng" alt="a colored log streaming through a terminal grid">
      <figcaption>Terminal cells skip the shaper entirely, and box drawing is
      generated, not shaped.</figcaption>
    </figure>
  </div>

  <h2>Why it is fast</h2>
  <ul class="facts">
    <li>Crossings are classified by the sign pattern of the control points, not
    by root-range checks, so a ray grazing a curve endpoint still counts
    exactly once.</li>
    <li>Glyphs past 16 curves carry per-axis band tables, so a fragment loops
    over a fraction of the outline — bit-for-bit the same coverage.</li>
    <li>The scene is retained and damage-tracked: setting one block's content
    re-uploads that block alone, and an idle frame draws nothing at all.</li>
    <li>Everything is plain <code>wgpu</code> with WebGL2-safe choices, so one
    code path covers Vulkan, Metal, DX12, GL, WebGPU and WebGL2.</li>
  </ul>

  <footer>
    Dual-licensed under MIT or Apache-2.0. Vendored fonts keep their own
    licenses (DejaVu: Bitstream Vera derivative; Manrope: OFL 1.1).
  </footer>
</main>
</body>
</html>
HTML

echo "==> done"
find "$site" -maxdepth 2 -mindepth 1 | sort | sed 's|^|    |'
