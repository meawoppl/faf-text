# How-it-works section author contract

Everything you need to write a section for `/how-it-works/`, with zero other
context. Read this whole file once before writing code.

## The one-paragraph version

You ship exactly one file, `sections/NN-slug.js` (`NN` and the slug come from
your brief). It default-exports `{ slug, title, html, mount }`. Your prose
goes in `html` as real DOM text; your figures are built in `mount(root, ctx)`
from the `ctx` toolkit — a live renderer canvas and/or SVG drawn from
`ctx.inspect(...)`, which returns the renderer's *actual* per-glyph internals.
You import nothing except `./lib/*` (already handed to you via `ctx`) and you
touch no file but your own.

## Hard rules

- **One file.** Create `sections/NN-slug.js` and nothing else. Do not edit
  `index.html` (its `SECTIONS` manifest is reconciled by the orchestrator
  after branches merge — parallel edits there conflict), `style.css`, or
  anything under `lib/`.
- **No imports outside `lib/` + your own file.** Everything you need arrives
  through `ctx`; if you find yourself importing the wasm pkg directly or
  fetching a font, stop — the path you hardcode will break on the deployed
  site.
- **Real data only.** Every number a figure draws comes from `ctx.inspect`
  or a live canvas. No hand-traced outlines, no illustrative fakes. Every
  prose claim must be checkable against the source.
- **Prose is DOM text.** Sentences live in `html` (accessibility,
  find-in-page). Canvases and SVG are for figures only. Give figures captions.
- **Tokyo Night, transparent figure backgrounds.** Use `ctx.palette` tokens
  only; the page supplies backgrounds.
- No attribution to any AI tooling anywhere — code, comments, or prose.

## Section module shape

```js
export default {
  slug: "bands",              // section element id, TOC anchor
  title: "Band tables",       // <h2> and TOC text
  html: `
    <p>Prose…</p>
    <div data-fig="main"></div>
  `,
  async mount(root, ctx) {    // called after html lands in the DOM
    const host = root.querySelector('[data-fig="main"]');
    // build figures, append to host
  },
};
```

`root` is the `<section>` your `html` was injected into (after the `<h2>`).
`mount` may be async; a thrown error is caught and shown as a banner without
killing the other sections. If the wasm bundle failed to load, `ctx.pkgUrl`
is `null` and `ctx.inspect` / `ctx.attachDemo` will reject — it is fine to
let that happen (the catch shows the banner); prose still renders.

During development load *just your section* without touching the manifest:

```
http://localhost:PORT/how-it-works/?section=NN-slug.js
```

## The `ctx` object

| Member | What it is |
| --- | --- |
| `ctx.pkgUrl` | Resolved wasm pkg base URL (string, ends `/`), or `null` if it failed to load. |
| `ctx.loadWasm()` | `Promise<{ mod, pkgUrl }>` — the raw module namespace (`FafTextDemo`, `inspect_glyph`, `list_families`), initialized once. You rarely need this. |
| `ctx.attachDemo(canvas, opts)` | Attach a live `FafTextDemo` to a canvas. Probes WebGPU and falls back to WebGL2 (headless/SwiftShader lands on WebGL2 — expected). Options: `height` (CSS px, default 160), `text`, `fontSize` (default 18), `stats` (renderer-drawn fps overlay, default false), `caret` (default false). Handles dpr sizing and resize. Returns the demo. |
| `ctx.animate(host, demo, tick?)` | Put a demo on the page's shared rAF ticker; `tick(t, demo)` runs while `host` is near the viewport. Pass no tick for slider-driven scenes — `demo.render()` presents nothing when nothing changed, so idle costs zero. |
| `ctx.inspect(ch, family?, weight?)` | `Promise<object|null>` — the inspector, schema below. Defaults: `"DejaVu Sans"`, `400`. |
| `ctx.listFamilies()` | `Promise<string[]>` — `["DejaVu Sans", "DejaVu Sans Mono", "Manrope", "Twemoji Mozilla"]`. |
| `ctx.fig` | The figure toolkit (below). |
| `ctx.palette` | Color tokens (below). |

Useful `FafTextDemo` methods for live figures: `set_text`, `set_font_size`
(CSS px), `set_weight_blend(t|undefined)` (Manrope master lerp; `undefined`
returns to the static face), `set_tilt(degrees|undefined)`, `set_terminal(bool)`,
`set_search(needle)` + `set_search_mode('highlight'|'underline'|'squiggle'|'chip')`,
`set_stats_overlay(bool)`, `backend()`.

### `ctx.fig` — the figure toolkit (`lib/figure.js`)

- `el(tag, attrs, ...children)` — HTML builder; `on*` attrs become listeners.
- `svgEl(tag, attrs, ...children)` — namespaced SVG builder. Stroked shapes
  get `vector-effect: non-scaling-stroke` automatically, so `stroke-width`
  stays in screen px even inside the em flip.
- `svg(width, height, attrs?)` — responsive `<svg>` with matching viewBox.
- `emSpace(bbox, { width = 420, pad = 24 })` — returns `{ g, width, height,
  scale, toPx }`. `g` is a group whose transform maps em space (y-up) into a
  `width × height` px box, so you draw with the inspector's raw coordinates.
  Append `g` to `svg(em.width, em.height)`. **Lengths that must be screen px
  (circle radii, dash sizes) go in as `px / em.scale`** — only strokes are
  auto-corrected. Use `toPx([x, y])` to place text labels in pixel space
  (never scale text with the glyph).
- `glyphPathD(curves, contours)` — SVG path data (`M … Q … Z` per contour)
  straight from an inspection's `curves` + `contours`. Works for `master_b.curves`
  and COLR layers too. Fill with `fill-rule: nonzero` — that *is* the shader's
  winding rule.
- `clipOctagon(bbox, clips)` — the corner-clipped quad as em-space points.
- `slider({ label, min, max, step, value, format, oninput })` → `{ root,
  input, set }`.
- `figure(children, caption?)` → `.hiw-figure` wrapper; `controls(...)` →
  `.hiw-controls` row; `caption(text)`; `details(summary, ...nodes|html)` →
  collapsed "gory details" block.

Any shipped section shows the whole toolkit in use;
`sections/01-from-font-to-curves.js` is a compact place to start.

### `ctx.palette`

```
bg      #1a1b26   bgDark  #15161e   border #2a2c3a
text    #c0caf5   textDim #a9b1d6   muted  #565f89
blue    #7aa2f7   green   #9ece6a   red    #f7768e
orange  #e0af68   purple  #bb9af7   teal   #7dcfff
```

Suggested figure conventions (keep the page coherent): glyph fills/strokes
blue, on-curve points teal, control points purple, clip/band overlays orange
(dashed), deltas/errors red, "good"/pass green, annotations muted.

### CSS classes you may use in `html`

`.hiw-figure`, `.hiw-caption`, `.hiw-canvas`, `.hiw-controls`, `.hiw-slider`,
`.hiw-details` (+ `.hiw-details-body`), `.hiw-banner`, plus plain `p`, `h3`,
`ul/ol`, `code`, `pre`. Do not invent new global classes; scope any extra
styling inline on your own elements.

## The inspector

`ctx.inspect(ch, family, weight)` runs the renderer's own extraction on the
embedded fonts and returns what production computes — same flattener, same
band builder, same clip solver (`faf_text::inspect` in the core crate; the
figures cannot drift from the renderer). Coordinates are **em units, y-up,
baseline origin** (extraction happens at size 1.0 with hinting disabled). It
returns `null` for an unknown family or an unmapped character.

### Field reference

| Field | Meaning |
| --- | --- |
| `ch`, `family`, `weight` | Echo of the call. |
| `glyph_id` | Charmap-resolved glyph id in the face. |
| `units` | Always `"em"`. |
| `outline` | Swash's outline commands before flattening: `{type: "move_to"\|"line_to", p}`, `{type: "quad_to", c, p}`, `{type: "curve_to", c1, c2, p}`, `{type: "close"}`. Empty when the glyph has no outline (e.g. a COLR base glyph that exists only as layers). |
| `curves` | Master A's quadratics **as stored** — control points rounded to f16 in the flattener. `{p0, p1, p2}`, each `[x, y]`. Lines arrive as quadratics with the control at the midpoint; cubics as four quadratics each. |
| `curves_raw` | The same curves before f16 rounding, index-parallel with `curves`, so quantization deltas are `curves[i] − curves_raw[i]`. Often exactly zero: TrueType coordinates are n/2048-style fractions f16 holds exactly below 1 em. Nonzero deltas live where cubics were split or lines midpointed into odd fractions. |
| `contours` | Curves per contour, in order: `curves[0..contours[0]]` is contour 0. Within a contour `curves[i].p2 === curves[i+1].p0` (endpoint sharing — the same float, which is what halves the banded record region). |
| `bbox` | `[min_x, min_y, max_x, max_y]` em, over both masters. |
| `banded` | True when the glyph carries band tables (> 16 curves and offsets within f16's exact-integer range). |
| `bands` | `null` unless banded. `{epsilon, y: [8 bands], x: [8 bands]}`. Each band: `interval` (`[lo, hi]` em along the banding axis; membership tested against it widened by `epsilon` of its height), `split` (median of members' ray-axis midpoints, f16-rounded as stored — samples past it fire the ray backwards), `descending` / `ascending` (the *same* members in opposite orders; each entry `{curve, key}` where `curve` indexes `curves` and `key` is the sort key the shader's early-out compares: the curve's max (descending) or min (ascending) control-point coordinate along the ray axis, over both masters). `y` bands are crossed by horizontal rays, `x` bands by vertical ones. |
| `clips` | Corner-clip legs, em: the isoceles right triangle cut off each bbox corner by the support-plane clip. Corner order is the unit quad's `(0,0) (1,0) (1,1) (0,1)` = em `(min_x,max_y) (max_x,max_y) (max_x,min_y) (min_x,min_y)`. 0 where a corner is not worth clipping — `'g'` and `'O'` clip nothing, `'A'` is `[0.265625, 0.28125, 0, 0]` (both top corners), `'v'` is `[0, 0, 0.203125, 0.203125]`. |
| `master_b` | `null` for static faces. For Manrope (`wght` 200–800): `{curves, curves_raw, axis_min: 200, axis_max: 800, weight_t}` — curves index-parallel to master A's; `weight_t` is where the requested weight falls on the axis (400 → 0.3333…), the blend the fragment shader defaults to. |
| `colr` | `null` unless the glyph is COLRv0. Array of layers, **bottom to top in paint order**: `{glyph_id, color, glyph}` where `color` is straight RGBA 0..1 from CPAL palette 0 (or `null` = "use the text color") and `glyph` is a *full nested inspection* of the layer's ordinary outline glyph (no recursion — layers never have `colr`). |
| `layout` | Texels inside the glyph's block, offsets relative to the block base: `header_texels` (16 for banded — 2 axes × 8 bands × 1 texel, else 0), `index_texels` (the sorted lists), `records_offset` (= header + index), `record_texels` (one master's records: `2 × count` flat, `count + contours` rounded to even when banded — endpoint sharing), `total_texels` (whole block = `records_offset + masters × record_texels`), `masters` (1 or 2). One texel = RGBA16F = 8 bytes. |

### Real sample: `g` in DejaVu Sans (`ctx.inspect('g', 'DejaVu Sans', 400)`)

Captured from the actual module; long arrays elided with `…` (everything
else verbatim):

```jsonc
{
  "ch": "g", "family": "DejaVu Sans", "weight": 400, "glyph_id": 74, "units": "em",
  "outline": [
    {"p": [0.453125, 0.28125], "type": "move_to"},
    {"c": [0.453125, 0.375], "p": [0.40625, 0.421875], "type": "quad_to"},
    {"c": [0.375, 0.484375], "p": [0.296875, 0.484375], "type": "quad_to"},
    // … 29 more, 32 total …
  ],
  "curves": [
    { "p0": [0.453125, 0.28125], "p1": [0.453125, 0.375], "p2": [0.40625, 0.421875] },
    { "p0": [0.40625, 0.421875], "p1": [0.375, 0.484375], "p2": [0.296875, 0.484375] },
    // … 27 more, 29 total …
  ],
  "curves_raw": [ /* same shape, 29 entries — identical to curves for this glyph:
                     DejaVu's 1/2048 coordinates survive f16 exactly */ ],
  "contours": [8, 21],
  "bbox": [0.0625, -0.203125, 0.546875, 0.5625],
  "banded": true,
  "bands": {
    "epsilon": 0.05,
    "y": [
      {
        "interval": [-0.203125, -0.107421875],
        "split": 0.25,
        "descending": [{"curve": 8, "key": 0.546875}, {"curve": 9, "key": 0.484375},
                       {"curve": 15, "key": 0.40625} /* … 8 entries … */],
        "ascending":  [{"curve": 11, "key": 0.125}, {"curve": 12, "key": 0.125},
                       {"curve": 13, "key": 0.125} /* … 8 entries … */]
      }
      // … 7 more bands (always 8 per axis) …
    ],
    "x": [ /* 8 bands, same shape */ ]
  },
  "clips": [0, 0, 0, 0],
  "master_b": null,
  "colr": null,
  "layout": {"header_texels": 16, "index_texels": 72, "masters": 1,
             "record_texels": 32, "records_offset": 88, "total_texels": 120}
}
```

### Real sample: 🚀 in Twemoji Mozilla (`ctx.inspect('🚀', 'Twemoji Mozilla', 400)`)

The base glyph has no outline of its own — it *is* its six layers, bottom to
top (exhaust first, nose cone last):

```jsonc
{
  "ch": "🚀", "family": "Twemoji Mozilla", "weight": 400, "glyph_id": 4, "units": "em",
  "outline": [], "curves": [], "curves_raw": [], "contours": [],
  "bbox": [0, 0, 0, 0], "banded": false, "bands": null, "clips": [0, 0, 0, 0], "master_b": null,
  "colr": [
    {
      "glyph_id": 14, "color": [0.6275, 0.0157, 0.1176, 1.0],   // #A0041E flame
      "glyph": {  // a full inspection of the layer, same schema as the top level
        "glyph_id": 14, "units": "em",
        "curves": [ { "p0": [0.03125, 0.40625], "p1": [0.140625, 0.5],
                      "p2": [0.25, 0.59375] } /* … 16 total … */ ],
        "contours": [16],
        "bbox": [0.03125, -0.09375, 0.71875, 0.59375],
        "banded": false, "bands": null, "clips": [0, 0, 0, 0.5],
        "outline": [ /* … */ ], "curves_raw": [ /* … */ ], "master_b": null, "colr": null,
        "layout": {"header_texels": 0, "index_texels": 0, "masters": 1,
                   "record_texels": 32, "records_offset": 0, "total_texels": 32}
      }
    },
    { "glyph_id": 15, "color": [1.0, 0.6745, 0.2, 1.0],       "glyph": { /* 12 curves */ } },
    { "glyph_id": 16, "color": [1.0, 0.8, 0.302, 1.0],        "glyph": { /*  8 curves */ } },
    { "glyph_id": 17, "color": [0.3333, 0.6745, 0.9333, 1.0], "glyph": { /* 16 curves */ } },
    { "glyph_id": 18, "color": [0.0, 0.0, 0.0, 1.0],          "glyph": { /* 12 curves */ } },
    { "glyph_id": 19, "color": [0.6275, 0.0157, 0.1176, 1.0], "glyph": { /*  9 curves */ } }
  ],
  "layout": {"header_texels": 0, "index_texels": 0, "masters": 1,
             "record_texels": 0, "records_offset": 0, "total_texels": 0}
}
```

Handy specimens: `g`/`B` (banded, multi-contour), `A`/`v`/`7` (nonzero corner
clips), `O` (round — support planes clip nothing), `i` (two contours, flat),
`g` in `Manrope` (banded **and** dual-master: 43 curves, `masters: 2`,
`total_texels: 222`), `❤ 🌈 🔥 🚀` in `Twemoji Mozilla` (COLR; the subset has
only these four).

## Local workflow

Ports are assigned per author so eight parallel sessions never collide:
**HTTP `89NN`, CDP `93NN`** where `NN` is your section number (section 03 →
serve on 8903, debug Chrome on 9303).

```sh
# 0. Once per checkout, if web/pkg is missing or stale (dev build, seconds;
#    wasm-pack lives in ~/.cargo/bin which is NOT on PATH):
~/.cargo/bin/wasm-pack build crates/faf-text-web --target web --out-dir ../../web/pkg --dev

# 1. Serve the web tree (from the repo root):
python3 -m http.server -d web 89NN

# 2. Open just your section:
#    http://localhost:89NN/how-it-works/?section=NN-slug.js
```

The wasm pkg resolves as `../pkg/` when serving `web/` locally and
`../demo/pkg/` on the deployed site; `lib/renderer.js` tries them in that
order, and `window.FAF_PKG_BASE` (set it before the module script runs)
overrides both. You never hardcode either path.

### Headless screenshot check

One-shot, for inspector-driven SVG figures:

```sh
google-chrome --headless=new --use-angle=swiftshader --enable-unsafe-swiftshader \
  --enable-logging=stderr --screenshot=/tmp/section-NN.png --window-size=1000,2000 \
  --virtual-time-budget=60000 "http://localhost:89NN/how-it-works/?section=NN-slug.js"
```

Two caveats, both measured, not theoretical. First, the budget must be
generous — the renderer presents nothing when idle, so virtual time races
ahead. Second, **the one-shot can capture live canvases mid-startup even
then**: virtual time exhausts the rAF backlog before the real async GPU
attach resolves, and the shot shows an empty canvas. Order your `mount`
accordingly — data figures appended first, `attachDemo` awaited
last — and treat the one-shot as the check for the SVG figures.
`console.log` shows up as `INFO:CONSOLE`. SwiftShader exposes a null WebGPU
adapter, so live canvases land on **WebGL2 — that is the expected backend
headless**, not a failure (the `No available adapters.` console line is the
probe, not an error).

Verifying a *live canvas* (or dragging a slider first) needs CDP, where real
time runs normally: start Chrome with `--remote-debugging-port=93NN` (plus
the SwiftShader flags above, no `--screenshot`), then drive it from node over
`ws` (`npm i ws`) with `Runtime.evaluate` + `Page.captureScreenshot` — poll
for your section's DOM (e.g. `#your-slug canvas` width > 1), sleep ~2 s for
frames to present, shoot.

## Deployment shape (why the rules are what they are)

`scripts/build-site.sh` copies `web/how-it-works/` verbatim into
`site/how-it-works/`; the wasm bundle it runs against is the release demo
bundle at `site/demo/pkg/`. So: no build step ever touches your section, no
bundler exists, and only vanilla ES modules with relative imports into
`lib/` survive the trip.
