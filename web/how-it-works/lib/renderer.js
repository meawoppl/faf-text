// Wasm plumbing for the how-it-works page: locate the pkg, initialize it
// once, attach live FafTextDemo canvases, and expose the glyph inspector as
// parsed objects. Section modules receive all of this through `ctx` and must
// not import the pkg themselves — the path differs between local serving and
// the deployed site, and this file is the one place that knows the try-order.
//
// Path resolution: the wasm-pack bundle lives at `../pkg/` when web/ is served
// directly (python3 -m http.server -d web PORT → /how-it-works/ next to
// /pkg/), and at `../demo/pkg/` on the deployed gh-pages tree (where this page
// is /how-it-works/ and the demo is /demo/). `window.FAF_PKG_BASE` overrides
// both, for pointing a checkout at any bundle at all.

const PKG_CANDIDATES = ["../pkg/", "../demo/pkg/"];
const PKG_MODULE = "faf_text_web.js";

let loading = null;

/// Resolve and initialize the wasm module once. Returns { mod, pkgUrl } where
/// `mod` is the module namespace (FafTextDemo, inspect_glyph, list_families)
/// and `pkgUrl` is the base the bundle was found under (absolute URL, ends
/// with '/'). Rejects when no candidate serves the bundle.
export function loadWasm() {
  loading ??= (async () => {
    const override = window.FAF_PKG_BASE ? [window.FAF_PKG_BASE] : [];
    for (const base of [...override, ...PKG_CANDIDATES]) {
      const pkgUrl = new URL(base, import.meta.url.replace(/lib\/[^/]*$/, ""));
      try {
        const mod = await import(new URL(PKG_MODULE, pkgUrl));
        await mod.default();
        return { mod, pkgUrl: pkgUrl.href };
      } catch (e) {
        console.warn(`faf-text pkg not at ${pkgUrl}:`, e.message ?? e);
      }
    }
    throw new Error(
      "faf_text_web.js not found — build it with wasm-pack (see CONTRACT.md)",
    );
  })();
  return loading;
}

// wgpu cannot recover from a browser that exposes `navigator.gpu` but hands
// back a null adapter (headless Chrome under SwiftShader, some Linux builds),
// so probe from JS first and ask for the WebGL2 backend when the probe fails —
// the same handshake web/index.html and the docs loader do.
let probing = null;
const forceGl = () =>
  (probing ??= (async () => {
    try {
      return !(navigator.gpu && (await navigator.gpu.requestAdapter()));
    } catch (_) {
      return true;
    }
  })());

// Shared clock: one rAF loop drives every live canvas on the page, and a cell
// scrolled out of view stops being ticked (its stats overlay then honestly
// reads `idle`). Same scheme as the live rustdoc cells.
let animators = [];
const visible = new WeakSet();
const animIo = new IntersectionObserver(
  (entries) => {
    for (const en of entries) {
      if (en.isIntersecting) visible.add(en.target);
      else visible.delete(en.target);
    }
  },
  { rootMargin: "100px" },
);
const t0 = performance.now();
let running = false;
const frame = () => {
  const t = (performance.now() - t0) / 1000;
  for (const a of animators) {
    if (!visible.has(a.host)) continue;
    try {
      if (a.tick) a.tick(t, a.demo);
      a.demo.render();
    } catch (e) {
      console.warn("how-it-works demo stopped:", e);
      animators = animators.filter((x) => x !== a);
    }
  }
  requestAnimationFrame(frame);
};

/// Put a demo on the shared ticker. `tick(t, demo)` runs each frame while
/// `host` (usually the canvas) is near the viewport; pass null to just keep
/// rendering damage (slider-driven scenes want exactly that). Idle frames
/// cost nothing: `demo.render()` returns without presenting when nothing
/// changed.
export function animate(host, demo, tick = null) {
  animIo.observe(host);
  animators.push({ host, demo, tick });
  if (!running) {
    running = true;
    requestAnimationFrame(frame);
  }
}

// Attached demos must stay referenced or the GC detaches their wasm surfaces.
const live = [];

/// Attach a FafTextDemo to a canvas. The canvas should already be in the DOM
/// (its width follows the column it is laid out in). Options:
///   height    CSS px, default 160
///   text      initial content, default a one-liner about the winding rule
///   fontSize  CSS px, default 18
///   stats     renderer-drawn fps overlay, default false
///   caret     show the blinking caret, default false
/// Returns the demo; wire animation with `animate(canvas, demo, tick)`.
export async function attachDemo(canvas, opts = {}) {
  const {
    height = 160,
    text = "Every pixel of this solves the winding rule.",
    fontSize = 18,
    stats = false,
    caret = false,
  } = opts;
  const [{ mod }, gl] = await Promise.all([loadWasm(), forceGl()]);
  const dpr = window.devicePixelRatio || 1;
  canvas.classList.add("hiw-canvas");
  canvas.style.height = `${height}px`;
  const fit = () => {
    const w = Math.max(1, Math.round(canvas.clientWidth * dpr));
    const h = Math.max(1, Math.round(height * dpr));
    canvas.width = w;
    canvas.height = h;
    return [w, h];
  };
  fit();
  const demo = await mod.FafTextDemo.attach(canvas, dpr, gl);
  demo.set_caret_visible(caret);
  demo.set_font_size(fontSize);
  demo.set_text(text);
  if (stats) demo.set_stats_overlay(true);
  live.push(demo);
  new ResizeObserver(() => {
    const [w, h] = fit();
    demo.resize(w, h, dpr);
  }).observe(canvas);
  return demo;
}

/// The renderer's own numbers for one glyph, as a parsed object — flattened
/// quadratics (f32 and the f16 values actually stored), contours, bbox, band
/// tables with sort keys, corner clips, master B, COLR layers, texel layout.
/// Schema in CONTRACT.md. Returns null when the family has no such glyph.
export async function inspect(ch, family = "DejaVu Sans", weight = 400) {
  const { mod } = await loadWasm();
  return JSON.parse(mod.inspect_glyph(ch, family, weight));
}

/// Family names the inspector can resolve (the four embedded fonts).
export async function listFamilies() {
  const { mod } = await loadWasm();
  return JSON.parse(mod.list_families());
}
