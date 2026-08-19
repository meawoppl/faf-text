// faf-text live docs: upgrade `.faf-live` placeholders in the rendered rustdoc
// into real wasm canvases, each driven through the demo crate's own API and
// loaded lazily from the gh-pages `demo/` deployment. The APNG inside each
// placeholder stays as the fallback: offline docs, a blocked fetch, a GPU-less
// browser or any error at all simply leaves it visible and animating. Live
// cells share one ticker and stop being ticked while scrolled out of view.
//
// EVERGREEN CONTRACT. This file is deployed to the site root of gh-pages
// (`https://meawoppl.github.io/faf-text/docs-loader.js`) and is fetched at page
// load by the stub in `crates/faf-text/docs-header.html`, which docs.rs bakes
// into every published version's pages. That means *this* file is live for
// every version of the docs at once, while the stub and the CSS beside it are
// frozen at whatever the crate shipped. So: never change this loader in a way
// that needs a new stub, and treat the markup and CSS class names as the frozen
// interface between the two. Load-bearing names, all styled by the header and
// emitted by `lib.rs` docs: `.faf-live` (one cell; `data-demo` picks the demo,
// optional `data-height` overrides the canvas height, and any `img` inside it
// is the fallback still), `.faf-live-grid` (the containing grid) and
// `.faf-live-caption` (the caption under a cell). `.faf-live canvas` is styled
// too, so the canvas this file creates must stay a direct child of the cell.
// New demo kinds are safe (old pages simply have no placeholder asking for
// them); renaming or restructuring those classes is not. Dependency-free
// vanilla ES module by the same rule — nothing here may assume a bundler, an
// import map, or anything on the page other than the stub that imported it.

const PKG =
  window.FAF_DEMO_PKG ||
  "https://meawoppl.github.io/faf-text/demo/pkg/faf_text_web.js";

// Shared clock: each animator is {host, tick(t, demo)}, ticked only while its
// host element is near the viewport. One rAF loop drives every cell, so a
// page full of them costs one callback a frame.
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
      a.tick(t, a.demo);
      a.demo.render();
    } catch (e) {
      console.warn("faf-text live demo stopped:", e);
      animators = animators.filter((x) => x !== a);
    }
  }
  requestAnimationFrame(frame);
};
const animate = (host, demo, tick) => {
  animIo.observe(host);
  animators.push({ host, demo, tick });
  if (!running) {
    running = true;
    requestAnimationFrame(frame);
  }
};

const SAMPLE =
  "Glyph outlines live on the GPU as quadratic Béziers, and every pixel " +
  "solves inside/outside with the non-zero winding rule.";

// Canvas height per demo, in CSS px. The width is whatever column the grid
// hands the cell, and `data-height` on the placeholder overrides this.
const HEIGHTS = { zoom: 130, weight: 130, tilt: 180, terminal: 190 };

// Demo builders: build(demo, host) wires one cell to the shared ticker.
// `demo` is a FafTextDemo already attached to the cell's canvas.
const demos = {
  // Font size sweeping 12 → 56 px. The curve texture is identical in every
  // frame of this; only the quad and the per-pixel evaluation change.
  zoom(demo, host) {
    demo.set_text("Bézier");
    animate(host, demo, (t) => {
      demo.set_font_size(34 - 22 * Math.cos(t * 1.6));
    });
  },

  // The `wght` axis of the embedded variable font, blended in the fragment
  // shader: no re-shaping and no curve is re-extracted.
  weight(demo, host) {
    demo.set_text("Manrope");
    demo.set_font_size(44);
    animate(host, demo, (t) => {
      demo.set_weight_blend(0.5 - 0.5 * Math.cos(t * 1.4));
    });
  },

  // The whole pane stood up in 3D behind a perspective camera — one matrix
  // per block per frame, and the analytic coverage is measured on the
  // projected glyph, so grazing angles stay smooth.
  tilt(demo, host) {
    demo.set_text(SAMPLE);
    demo.set_font_size(15);
    animate(host, demo, (t) => {
      demo.set_tilt(32 * Math.sin(t * 0.55));
    });
  },

  // A synthetic colored log streaming through a TermGrid: char → glyph id
  // through the charmap, no shaping anywhere on this path.
  terminal(demo, host) {
    demo.set_terminal(true);
    animate(host, demo, () => {});
  },
};

// Demos must stay referenced or the GC detaches their wasm surfaces.
const live = [];
let loading = null;
const boot = () =>
  (loading ??= import(PKG).then(async (mod) => {
    await mod.default();
    return mod;
  }));

// wgpu cannot recover from a browser that exposes `navigator.gpu` but hands
// back a null adapter (headless Chrome, some Linux builds), so probe from JS
// first and ask for the WebGL2 backend when it fails — the same handshake
// `web/index.html` does.
let probing = null;
const forceGl = () =>
  (probing ??= (async () => {
    try {
      return !(navigator.gpu && (await navigator.gpu.requestAdapter()));
    } catch (_) {
      return true;
    }
  })());

const upgrade = async (el) => {
  if (el.dataset.fafMounted) return;
  el.dataset.fafMounted = "1";
  const build = demos[el.dataset.demo];
  if (!build) return;
  try {
    const [mod, gl] = await Promise.all([boot(), forceGl()]);
    const dpr = window.devicePixelRatio || 1;
    const canvas = document.createElement("canvas");
    canvas.style.touchAction = "none";
    // Height is fixed; width follows the column the grid gives the cell.
    const cssHeight = Number(el.dataset.height || HEIGHTS[el.dataset.demo] || 150);
    canvas.style.height = `${cssHeight}px`;
    el.prepend(canvas);
    const fit = () => {
      const w = Math.max(1, Math.round(canvas.clientWidth * dpr));
      const h = Math.max(1, Math.round(cssHeight * dpr));
      canvas.width = w;
      canvas.height = h;
      return [w, h];
    };
    fit();

    const demo = await mod.FafTextDemo.attach(canvas, dpr, gl);
    demo.set_caret_visible(false);
    // The fps readout is drawn by the renderer, in a block of its own, and
    // counts presented frames — so a cell the observer has stopped ticking
    // reads `idle` rather than lying about a rAF rate it is not getting.
    demo.set_stats_overlay(true);
    build(demo, el);
    live.push(demo);
    // Rustdoc pages reflow (sidebar toggles, window resizes); re-configure
    // the surface rather than letting the canvas stretch.
    new ResizeObserver(() => {
      const [w, h] = fit();
      demo.resize(w, h, dpr);
    }).observe(canvas);
    // The still image was the fallback; the live cell replaces it.
    for (const img of el.querySelectorAll("img")) img.style.display = "none";
  } catch (e) {
    // Leave the animated fallback in place. This is the expected path on any
    // browser without WebGPU *and* without WebGL2, and offline.
    console.warn("faf-text live demo unavailable:", e);
  }
};

const start = () => {
  const hosts = document.querySelectorAll(".faf-live[data-demo]");
  if (!hosts.length) return;
  const io = new IntersectionObserver(
    (entries) => {
      for (const en of entries) {
        if (en.isIntersecting) {
          io.unobserve(en.target);
          upgrade(en.target);
        }
      }
    },
    { rootMargin: "200px" },
  );
  hosts.forEach((h) => io.observe(h));
};
if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", start);
} else {
  start();
}
