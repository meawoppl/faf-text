# Changelog

## Unreleased

- Curve storage moved from RGBA32F to RGBA16F with contour-aware endpoint
  sharing: a banded glyph costs one texel per curve plus one per contour, and
  the offscreen scene's curve data went from 281 KB to 107 KB. Coordinates are
  rounded to f16 as they are flattened, so band tables and the shader agree on
  the outline to the bit.

## 0.1.0 — 2026-08-13

First release.

- Vector glyph rendering on the GPU: quadratic Béziers in a data texture,
  per-pixel non-zero winding with analytic antialiasing, sign-pattern crossing
  classification robust to rays grazing curve endpoints, per-glyph band tables,
  adaptive 3-tap supersampling below 24 px/em.
- Growable, LRU-evicting curve and bitmap-atlas stores, safe against retained
  instances across frames.
- Variable-font weight interpolated between `wght` masters in the fragment
  shader; static fonts keep a specialized zero-cost pipeline.
- Retained blocks with damage tracking: per-block instance arenas, dynamic-
  offset uniforms, zero uploads on idle frames.
- 2D/3D placement: per-block 4×4 matrices in homogeneous pixel space (2D is
  byte-identical to the fixed path), ray-based hit-testing, perspective-correct
  antialiasing.
- Text stack: cosmic-text shaping, BiDi-aware selection and hit-testing, caret
  and motion helpers, IME composition, decorations (underline, strikethrough,
  squiggle, chips), virtualized million-line documents, and a shaping-free
  terminal grid with procedural box drawing.
- Quality options: linear-light blending with contrast compensation; LCD RGB
  subpixel coverage via dual-source blending.
- Runs native (Vulkan/Metal/DX12/GL) and in browsers (WebGPU with WebGL2
  fallback) from one code path.
