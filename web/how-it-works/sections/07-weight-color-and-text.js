// Section 07: variable weight on the GPU, COLR color glyphs, and the text
// stack around them. Every figure draws real inspector data (ctx.inspect) or
// a live renderer canvas; sources of record are curves.rs (master extraction
// and packing), shaders.wgsl (fetch_pair / blend_pair / BLEND_MASTERS),
// renderer.rs (push_color_layers), colr.rs, view.rs, document.rs.

export default {
  slug: "weight-color-and-text",
  title: "Living type: weight, color, and the text stack",

  html: `
    <p>Everything so far treated a glyph as a frozen set of curves. This
    section is about the ways the same machinery bends without breaking:
    fonts whose weight is a number you can animate per frame, emoji that stay
    razor-sharp at any size because they were never bitmaps to begin with,
    and the shaping-and-editing stack that decides which glyphs to draw at
    all.</p>

    <h3>One glyph, two masters</h3>

    <p>A variable font is not a family of files — it is one outline plus
    instructions for moving its points. When <code>CurveStore::extract</code>
    (in <code>curves.rs</code>) meets a face with a <code>wght</code> axis, it
    extracts the glyph <em>twice</em>: once with the axis pinned to its
    minimum, once to its maximum. The two outlines are point-compatible by
    construction — variation in the font format is itself per-point, so both
    ends flatten into the same command sequence — and
    <code>masters_compatible</code> verifies it anyway (same commands, same
    order). A font that breaks the promise renders from master A alone,
    because blending mismatched command lists would pair up unrelated points
    and produce a shape belonging to neither master.</p>

    <p>Master B's records land in the curve texture immediately after A's,
    packed identically, so the entire addressing scheme is one stride: the
    twin of the record at <code>first + k</code> lives at
    <code>b_first + k</code>, where <code>b_first = first +
    record_texels</code> (<code>GlyphCurves::b_first</code>). No second
    index, no per-curve pointer. <a href="#finding-curves-fast">Band
    tables</a> keep indexing master A and reach B's twin by the same
    offset.</p>

    <p>The fragment shader then does something that looks too cheap to be
    legal: <code>fetch_pair</code> reads both masters' texels and
    <code>blend_pair</code> lerps <em>control points</em> —
    <code>mix(a0,&nbsp;b0,&nbsp;weight_t)</code> and likewise for the other
    two — before the winding test ever sees a curve. That is not an
    approximation of interpolating the outline; it <em>is</em> interpolating
    the outline, and the proof fits in two lines. A quadratic B&eacute;zier
    is <em>B</em><sub>P</sub>(u) = (1&minus;u)&sup2;p&#8320; +
    2u(1&minus;u)p&#8321; + u&sup2;p&#8322; — <em>linear</em> in its control
    points P. So for every u, (1&minus;t)&middot;<em>B</em><sub>A</sub>(u) +
    t&middot;<em>B</em><sub>B</sub>(u) =
    <em>B</em><sub>(1&minus;t)A+tB</sub>(u): the pointwise lerp of two
    outlines is exactly the outline of the lerped control points. Weight
    animation therefore costs the fragment shader three <code>mix()</code>
    calls per curve and the CPU nothing at all — no re-shaping, no
    re-extraction, no texture upload. The blend factor
    <code>weight_t</code> rides on each instance;
    <code>weight_blend</code> maps the shaped weight onto the axis, so on
    Manrope (axis 200&ndash;800) a weight-400 run sits at t&nbsp;=&nbsp;&#8531;
    and looks like the weight it was shaped at unless a caller overrides
    it.</p>

    <div data-fig="masters"></div>
    <div data-fig="linearity"></div>

    <h3>The branch that was never taken, and cost 25% anyway</h3>

    <p>The first implementation guarded the master-B fetch with a per-curve
    <code>if b_first != 0u</code>. On a page of entirely static DejaVu — the
    branch never once taken — <code>examples/bench</code> went from 0.64 to
    0.80&nbsp;ms per frame. A quarter of the frame, spent on code that never
    executed: the fetch and the mix still occupy registers, and the loop
    schedules around them whether or not any thread takes the branch. The
    fix is to make the compiler delete them: <code>shaders.wgsl</code>
    declares <code>override BLEND_MASTERS: bool = false</code>, and the
    renderer builds two pipelines from the <em>same module</em> via
    <code>PipelineCompilationOptions::constants</code>. With the override
    false, constant folding removes the second fetch and the mixes entirely
    — the static pipeline compiles to the pre-variable shader, instruction
    for instruction, and bench went back to 0.64&nbsp;ms. The price is one
    extra pipeline switch and draw call, paid only by blocks that actually
    contain blended glyphs (empty spans are skipped).</p>

    <div data-fig="pipelines"></div>

    <details class="hiw-details"><summary>Gory details: band tables across two masters</summary>
    <div class="hiw-details-body">
      <p>Band membership and the early-out sort keys must hold at
      <em>every</em> weight, not just the two ends. The reason they can is
      the same linearity again: a blended control point is a lerp of the two
      masters' and never leaves their per-coordinate hull, so one span per
      curve — min and max over <em>both</em> masters' control points
      (<code>spans()</code> in <code>curves.rs</code>) — bounds the curve at
      every <code>weight_t</code>. The shader has to honor the same
      convention: on the blending pipeline, <code>record_winding</code>
      computes its early-out bound with <code>ray_bound</code> over master A
      <em>and</em> master B separately, then takes the max. Bounding the
      blended curve instead would be tighter — and wrong: the index list was
      sorted by the two-master key, so a tighter per-sample bound could end
      the loop in front of a curve that still crosses the ray at some other
      weight ordering. <code>fetch_pair</code> keeps the masters apart
      precisely so this bound can see both.</p>
    </div></details>

    <h3>A rocket is six ordinary glyphs</h3>

    <p>A COLRv0 color glyph has no outline of its own. It is a base glyph id
    plus a list of layers — (glyph id, palette index) pairs, bottom to top —
    where every layer is an <em>ordinary</em> outline glyph of the same font,
    painted in one flat color from the font's CPAL palette. Which means the
    whole feature is a cache plus a loop
    (<code>push_color_layers</code> in <code>renderer.rs</code>): each layer
    extracts through <code>CurveStore::get_or_insert</code> untouched — band
    tables, corner clipping, LRU eviction, even masters all apply — and
    costs the color glyph one vector instance. Instances draw in queue
    order, so pushing layers in COLR order <em>is</em> the painter's
    algorithm the format asks for. Colors come from palette&nbsp;0 as
    straight RGBA, and the run's alpha multiplies the palette's, so fading
    text out fades its emoji with it; the reserved palette index
    <code>0xFFFF</code> means "use the text color" and takes the run's color
    whole.</p>

    <p>The one wrinkle is overflow. If the
    <a href="#memory-and-caching">curve store</a> fills up partway
    through a layer stack, drawing the layers that fit would paint a partial
    emoji — so <code>push_color_layers</code> truncates the instances it
    already pushed, returns false, and the caller sends the <em>whole</em>
    glyph to the bitmap atlas for that frame. Half a rocket is worse than a
    soft one.</p>

    <p>Soft is the operative word. The offscreen regression scene renders
    &#128640; at 200&nbsp;px both ways — a CBDT bitmap strike scaled up on
    the left, COLR layers through the winding shader on the right. Crop each
    render to its ink bounding box and call a pixel <em>soft</em> when it
    sits more than 6/255 from every one of the crop's eight most common
    colors — that is, it belongs to a transition gradient rather than a flat
    fill or the background. The upscaled strike measures 29.0% soft; the
    COLR vector render, 2.5%. At 22&nbsp;px the gap narrows to 44.2% vs
    20.2% — at small sizes antialiasing pixels dominate any renderer. A
    vector layer's edge is a single analytic-AA pixel at every size,
    perspective transforms included, because it goes through exactly the
    winding evaluation every letter on this page does.</p>

    <div data-fig="colr"></div>
    <div data-fig="crisp"></div>

    <details class="hiw-details"><summary>Gory details: what stays on the bitmap atlas</summary>
    <div class="hiw-details-body">
      <p><code>ColrCache</code> (<code>colr.rs</code>) caches two things, and
      the coarse one is load-bearing: a per-font "has usable COLRv0" flag, so
      an ordinary text run never accumulates a "not a color glyph" cache
      entry per letter. A font whose COLR table is v1 is marked unusable
      wholesale — gradients, transforms, and composites have no expression
      in a winding-rule shader — and its emoji take the bitmap atlas path,
      which is also where CBDT (NotoColorEmoji), sbix, and SVG-table fonts
      stay. CPAL stores its colors BGRA behind a per-palette index array;
      the parser resolves palette 0 by hand because the crate-level COLR API
      only exposes layers through a v1-shaped painter that never says which
      layer asked for 0xFFFF. The embedded Twemoji subset carries four emoji
      (&#10084; &#127752; &#128293; &#128640;) totalling 15 layer outlines;
      the three the offscreen strip draws — &#128640;'s six layers,
      &#128293;'s two, &#10084;'s one — pack into 364 texels of curve data,
      2,912 bytes, about 1&nbsp;KB per emoji. The strip as a whole grew the
      scene's packed curve store from 109,312 to 125,424 bytes; most of the
      difference is not the emoji but the strip's two caption lines pulling
      a fresh monospace face's letters into the store.</p>
    </div></details>

    <h3>The stack under the pixels</h3>

    <p><strong>Shaping.</strong> faf-text does not shape text; cosmic-text
    does — Unicode segmentation, font fallback, and OpenType shaping — and
    the renderer consumes its <code>LayoutRun</code>s as positioned glyph
    ids. Text attributes survive the whole trip: an underline or
    strikethrough set on a span arrives as decoration geometry in em units
    (offset and thickness from the font's own tables) and becomes GPU rect
    instances, no manual bookkeeping. The terminal grid mode
    (<code>grid.rs</code>) skips shaping entirely — charmap lookup plus a
    cached advance per cell — which is how a 200&times;60 colored grid turns
    into instances in 0.12&nbsp;ms.</p>

    <p><strong>Selection and BiDi.</strong>
    <code>TextView::selection_rects</code> (<code>view.rs</code>) returns one
    rectangle per highlighted span of each layout run, BiDi-aware
    <em>within</em> the line via cosmic-text's per-run highlight — a
    selection crossing a right-to-left run comes back as the visually
    correct, possibly discontiguous spans. One guard matters:
    upstream's <code>highlight()</code> has no line-range check and reports
    runs outside the cursor range as fully selected, so the view filters
    <code>line_i</code> first. The rects draw as a rect block created
    <em>before</em> the text block, so selection composites under the
    glyphs.</p>

    <p><strong>Caret and IME.</strong> <code>cursor_rect</code>
    (<code>view.rs</code>) resolves a cursor to caret geometry, including the
    annoying case: at a soft-wrap boundary the same byte index belongs to two
    rows, and the cursor's affinity picks which one. The web demo's
    <code>composition_start/update/end</code> trio splices IME preedit text
    into the backing string, so it shapes and renders inline (underlined) at
    the caret through the ordinary pipeline, and the final commit keeps or
    removes it. Nothing about composition is special-cased below the string
    layer.</p>

    <p><strong>Big documents.</strong> <code>Document</code>
    (<code>document.rs</code>) virtualizes shaping: text is split into
    128-line chunks and only the chunks intersecting the viewport (&plusmn;1)
    are ever shaped, with heights estimated for the rest and corrected as
    chunks come in. On a synthetic million-line log
    (<code>examples/bigfile</code>), indexing 1,000,001 lines into 7,813
    chunks takes ~100&nbsp;ms, no paint ever shaped more than 3 chunks, and
    0.083% of the document ever touched the shaper across three viewport
    jumps — while <code>find_all</code> still searches the whole backing
    text. API details live in the crate docs at
    <a href="https://docs.rs/faf-text">docs.rs/faf-text</a>.</p>
  `,

  async mount(root, ctx) {
    const { fig, palette } = ctx;

    const rgba = (c) =>
      `rgba(${Math.round(c[0] * 255)}, ${Math.round(c[1] * 255)}, ${Math.round(c[2] * 255)}, ${c[3]})`;
    const hex = (c) =>
      "#" +
      [c[0], c[1], c[2]]
        .map((v) =>
          Math.round(v * 255)
            .toString(16)
            .padStart(2, "0"),
        )
        .join("")
        .toUpperCase();
    const mix2 = (a, b, t) => [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
    const blendCurves = (a, b, t) =>
      a.map((c, i) => ({
        p0: mix2(c.p0, b[i].p0, t),
        p1: mix2(c.p1, b[i].p1, t),
        p2: mix2(c.p2, b[i].p2, t),
      }));

    // Both inspections up front; every figure below draws from these.
    const g = await ctx.inspect("g", "Manrope", 400);
    const rocket = await ctx.inspect("\u{1F680}", "Twemoji Mozilla", 400);

    // --- Figure (a): master interpolation, SVG + live GPU side by side ----
    let demo = null; // attached at the very end of mount
    let weightSlider = null; // figure (a)'s slider, re-read at attach time
    {
      const host = root.querySelector('[data-fig="masters"]');
      const A = g.curves;
      const B = g.master_b.curves;
      const { axis_min, axis_max, weight_t } = g.master_b;
      const em = fig.emSpace(g.bbox, { width: 300, pad: 22 });
      const node = fig.svg(em.width, em.height);
      node.append(em.g);

      // The two masters as static references.
      for (const [curves, color] of [
        [A, palette.muted],
        [B, palette.muted],
      ]) {
        em.g.append(
          fig.svgEl("path", {
            d: fig.glyphPathD(curves, g.contours),
            fill: "none",
            stroke: color,
            "stroke-width": 1,
            "stroke-dasharray": "3 3",
            "fill-rule": "nonzero",
          }),
        );
      }
      const blended = fig.svgEl("path", {
        d: fig.glyphPathD(blendCurves(A, B, weight_t), g.contours),
        fill: palette.blue,
        "fill-opacity": 0.45,
        "fill-rule": "nonzero",
        stroke: palette.blue,
        "stroke-width": 1.5,
      });
      em.g.append(blended);

      const canvas = document.createElement("canvas");
      canvas.style.flex = "1 1 260px";
      canvas.style.minWidth = "240px";
      const row = fig.el(
        "div",
        { style: "display:flex; flex-wrap:wrap; gap:12px; align-items:center;" },
        node,
        canvas,
      );

      const slider = fig.slider({
        label: "weight_t",
        min: 0,
        max: 1,
        step: 0.01,
        value: Math.round(weight_t * 100) / 100,
        format: (t) => `${t.toFixed(2)} → wght ${Math.round(axis_min + t * (axis_max - axis_min))}`,
        oninput: (t) => {
          blended.setAttribute("d", fig.glyphPathD(blendCurves(A, B, t), g.contours));
          demo?.set_weight_blend(t);
        },
      });

      host.append(
        fig.figure(
          [row, fig.controls(slider.root)],
          `Manrope 'g', both masters. Dashed gray: the wght ${axis_min} and ` +
            `${axis_max} outlines as extracted — ${A.length} quadratics each, ` +
            `stored back to back (${g.layout.masters} masters, ` +
            `${g.layout.record_texels} texels apart, ${g.layout.total_texels} ` +
            `texels total). Blue: the control-point lerp the fragment shader ` +
            `computes in blend_pair. The canvas is the real renderer doing ` +
            `the same blend on the GPU via set_weight_blend — drag the ` +
            `slider; nothing re-shapes and nothing re-uploads to the curve ` +
            `texture.`,
        ),
      );

      weightSlider = slider;
    }

    // --- Figure (b): linearity of Béziers, one curve blown up -------------
    {
      const host = root.querySelector('[data-fig="linearity"]');
      const A = g.curves;
      const B = g.master_b.curves;
      // The curve whose control points move farthest between masters — the
      // clearest specimen for the derivation.
      let pick = 0;
      let best = -1;
      for (let i = 0; i < A.length; i++) {
        const d = ["p0", "p1", "p2"].reduce(
          (s, k) => s + Math.hypot(A[i][k][0] - B[i][k][0], A[i][k][1] - B[i][k][1]),
          0,
        );
        if (d > best) {
          best = d;
          pick = i;
        }
      }
      const ca = A[pick];
      const cb = B[pick];
      const pts = ["p0", "p1", "p2"];
      const xs = pts.flatMap((k) => [ca[k][0], cb[k][0]]);
      const ys = pts.flatMap((k) => [ca[k][1], cb[k][1]]);
      const bbox = [Math.min(...xs), Math.min(...ys), Math.max(...xs), Math.max(...ys)];
      const em = fig.emSpace(bbox, { width: 460, pad: 34 });
      const node = fig.svg(em.width, em.height);
      node.append(em.g);

      const curveD = (c) => `M ${c.p0[0]} ${c.p0[1]} Q ${c.p1[0]} ${c.p1[1]} ${c.p2[0]} ${c.p2[1]}`;
      const polyD = (c) => `M ${c.p0[0]} ${c.p0[1]} L ${c.p1[0]} ${c.p1[1]} L ${c.p2[0]} ${c.p2[1]}`;

      // Masters: curve + control polygon.
      for (const [c, color] of [
        [ca, palette.blue],
        [cb, palette.teal],
      ]) {
        em.g.append(
          fig.svgEl("path", { d: polyD(c), fill: "none", stroke: color, "stroke-width": 0.8, "stroke-opacity": 0.5 }),
          fig.svgEl("path", { d: curveD(c), fill: "none", stroke: color, "stroke-width": 1.6 }),
        );
      }
      // Connecting segments p_i^A -> p_i^B: the tracks the blended points ride.
      for (const k of pts) {
        em.g.append(
          fig.svgEl("path", {
            d: `M ${ca[k][0]} ${ca[k][1]} L ${cb[k][0]} ${cb[k][1]}`,
            fill: "none",
            stroke: palette.muted,
            "stroke-width": 1,
            "stroke-dasharray": "3 3",
          }),
        );
      }
      const blendPath = fig.svgEl("path", {
        d: "",
        fill: "none",
        stroke: palette.green,
        "stroke-width": 2,
      });
      const blendPoly = fig.svgEl("path", {
        d: "",
        fill: "none",
        stroke: palette.green,
        "stroke-width": 0.8,
        "stroke-opacity": 0.5,
      });
      const dots = pts.map(() =>
        fig.svgEl("circle", { cx: 0, cy: 0, r: 3.4 / em.scale, fill: palette.purple }),
      );
      em.g.append(blendPoly, blendPath, ...dots);
      // Master endpoint dots.
      for (const [c, color] of [
        [ca, palette.blue],
        [cb, palette.teal],
      ]) {
        for (const k of pts) {
          em.g.append(fig.svgEl("circle", { cx: c[k][0], cy: c[k][1], r: 2.2 / em.scale, fill: color }));
        }
      }

      const setT = (t) => {
        const c = { p0: mix2(ca.p0, cb.p0, t), p1: mix2(ca.p1, cb.p1, t), p2: mix2(ca.p2, cb.p2, t) };
        blendPath.setAttribute("d", curveD(c));
        blendPoly.setAttribute("d", polyD(c));
        pts.forEach((k, i) => {
          dots[i].setAttribute("cx", c[k][0]);
          dots[i].setAttribute("cy", c[k][1]);
        });
      };
      setT(0.5);
      const slider = fig.slider({
        label: "t",
        min: 0,
        max: 1,
        step: 0.01,
        value: 0.5,
        format: (t) => t.toFixed(2),
        oninput: setT,
      });

      host.append(
        fig.figure(
          [node, fig.controls(slider.root)],
          `Why lerping control points is exact: curve ${pick} of Manrope 'g', ` +
            `the one that moves farthest between masters. Blue is master A, ` +
            `teal master B; each purple point slides along the dashed segment ` +
            `at fraction t, and the green curve those points define IS the ` +
            `pointwise lerp of the two curves at every u — a Bézier is ` +
            `linear in its control points, so mix-then-evaluate equals ` +
            `evaluate-then-mix. This is the whole correctness argument for ` +
            `blend_pair.`,
        ),
      );
    }

    // --- Figure (d): one WGSL module, two pipelines ------------------------
    {
      const host = root.querySelector('[data-fig="pipelines"]');
      const W = 620;
      const H = 300;
      const node = fig.svg(W, H);
      const box = (x, y, w, h, stroke) =>
        fig.svgEl("rect", {
          x,
          y,
          width: w,
          height: h,
          rx: 8,
          fill: "none",
          stroke,
          "stroke-width": 1.4,
        });
      const text = (x, y, s, opts = {}) =>
        fig.svgEl(
          "text",
          {
            x,
            y,
            fill: opts.fill ?? palette.text,
            "font-size": opts.size ?? 13,
            "font-family": opts.mono ? "ui-monospace, monospace" : "inherit",
            "text-anchor": opts.anchor ?? "start",
          },
          s,
        );
      const arrow = (x1, y1, x2, y2) =>
        fig.svgEl("path", {
          d: `M ${x1} ${y1} L ${x2} ${y2} M ${x2} ${y2} l -7 -4 m 7 4 l -7 4`,
          fill: "none",
          stroke: palette.muted,
          "stroke-width": 1.2,
        });

      // Source module.
      node.append(
        box(20, 20, 250, 92, palette.purple),
        text(36, 44, "shaders.wgsl — one module", { fill: palette.purple }),
        text(36, 68, "override BLEND_MASTERS:", { mono: true, size: 12, fill: palette.orange }),
        text(36, 86, "    bool = false;", { mono: true, size: 12, fill: palette.orange }),
        text(36, 104, "fetch_pair · blend_pair · winding", { mono: true, size: 11, fill: palette.muted }),
      );
      // Two pipelines.
      node.append(
        arrow(270, 50, 340, 50),
        arrow(270, 95, 340, 185),
        text(305, 40, "= false", { mono: true, size: 11, fill: palette.muted, anchor: "middle" }),
        text(285, 150, "= true", { mono: true, size: 11, fill: palette.muted }),

        box(345, 20, 255, 96, palette.blue),
        text(361, 44, "static pipeline", { fill: palette.blue }),
        text(361, 64, "master-B fetch + mix() folded away:", { size: 12 }),
        text(361, 82, "instruction-for-instruction the", { size: 12 }),
        text(361, 100, "pre-variable shader. bench 0.64 ms.", { size: 12 }),

        box(345, 155, 255, 96, palette.green),
        text(361, 179, "blend pipeline", { fill: palette.green }),
        text(361, 199, "twin of record first+k at b_first+k", { size: 11, mono: true }),
        text(361, 217, "three mix() per curve, early-out", { size: 12 }),
        text(361, 235, "bounds BOTH masters.", { size: 12 }),
      );
      node.append(
        text(20, 275, "The rejected design — one pipeline, a per-curve if b_first != 0u —", {
          size: 12,
          fill: palette.red,
        }),
        text(20, 292, "measured 0.64 → 0.80 ms/frame with the branch never taken.", {
          size: 12,
          fill: palette.red,
        }),
      );

      host.append(
        fig.figure(
          [node],
          "Pipeline specialization: the renderer builds both pipelines from " +
            "one WGSL module with PipelineCompilationOptions::constants. " +
            "Static text draws with a shader that contains no mention of a " +
            "second master; only blocks holding blended glyphs pay the extra " +
            "draw call (their span is skipped when empty).",
        ),
      );
    }

    // --- Figure (c): exploded COLR stack -----------------------------------
    {
      const host = root.querySelector('[data-fig="colr"]');
      const layers = rocket.colr;
      const n = layers.length;
      const union = layers.reduce(
        (u, l) => [
          Math.min(u[0], l.glyph.bbox[0]),
          Math.min(u[1], l.glyph.bbox[1]),
          Math.max(u[2], l.glyph.bbox[2]),
          Math.max(u[3], l.glyph.bbox[3]),
        ],
        [Infinity, Infinity, -Infinity, -Infinity],
      );
      const step = 0.3; // em of vertical separation per layer, fully exploded
      const expanded = [union[0], union[1], union[2], union[3] + step * (n - 1)];
      const em = fig.emSpace(expanded, { width: 300, pad: 24 });
      const node = fig.svg(580, em.height);
      node.append(em.g);

      const groups = layers.map((layer) => {
        const grp = fig.svgEl("g", {});
        grp.append(
          fig.svgEl("path", {
            d: fig.glyphPathD(layer.glyph.curves, layer.glyph.contours),
            fill: layer.color ? rgba(layer.color) : palette.text,
            "fill-rule": "nonzero",
            stroke: palette.muted,
            "stroke-width": 0.75,
            "stroke-opacity": 0.55,
          }),
        );
        em.g.append(grp);
        return grp;
      });

      // Labels live in pixel space, right of the glyph, tracking their layer.
      const labels = layers.map((layer, i) => {
        const sw = fig.svgEl("rect", {
          width: 10,
          height: 10,
          fill: layer.color ? rgba(layer.color) : palette.text,
          stroke: palette.border,
          "stroke-width": 1,
        });
        const tx = fig.svgEl(
          "text",
          { fill: palette.textDim, "font-size": 12, "font-family": "ui-monospace, monospace" },
          `${i}: glyph ${layer.glyph_id} · ${layer.glyph.curves.length} curves · ` +
            (layer.color ? hex(layer.color) : "text color"),
        );
        node.append(sw, tx);
        return { sw, tx };
      });

      const setExplode = (e) => {
        // Layer offsets, plus label anchors that track each layer's center.
        const anchors = layers.map((layer, i) => {
          const dy = i * step * e;
          groups[i].setAttribute("transform", `translate(0 ${dy})`);
          const cy = (layer.glyph.bbox[1] + layer.glyph.bbox[3]) / 2 + dy;
          return { i, py: em.toPx([union[2], cy])[1] };
        });
        // Two layers can share a vertical center; keep labels 18 px apart.
        anchors.sort((a, b) => a.py - b.py);
        for (let j = 1; j < anchors.length; j++) {
          anchors[j].py = Math.max(anchors[j].py, anchors[j - 1].py + 18);
        }
        for (const { i, py } of anchors) {
          labels[i].sw.setAttribute("x", 312);
          labels[i].sw.setAttribute("y", py - 9);
          labels[i].tx.setAttribute("x", 328);
          labels[i].tx.setAttribute("y", py);
        }
      };
      setExplode(1);
      const slider = fig.slider({
        label: "explode",
        min: 0,
        max: 1,
        step: 0.01,
        value: 1,
        format: (v) => v.toFixed(2),
        oninput: setExplode,
      });

      host.append(
        fig.figure(
          [node, fig.controls(slider.root)],
          `\u{1F680} in Twemoji Mozilla is ${n} ordinary glyphs drawn bottom ` +
            `to top — exhaust first, nose cone last — each an outline ` +
            `that went through the same flattener, store, and winding shader ` +
            `as every letter here, filled with its CPAL palette-0 color. ` +
            `Slide to 0 to reassemble: queue order is the painter's ` +
            `algorithm. The base glyph itself has ` +
            `${rocket.curves.length} curves — it exists only as its layers.`,
        ),
      );
    }

    // --- Figure (e): vector vs fixed-strike raster, zoomable ---------------
    {
      const host = root.querySelector('[data-fig="crisp"]');
      const layers = rocket.colr;
      const union = layers.reduce(
        (u, l) => [
          Math.min(u[0], l.glyph.bbox[0]),
          Math.min(u[1], l.glyph.bbox[1]),
          Math.max(u[2], l.glyph.bbox[2]),
          Math.max(u[3], l.glyph.bbox[3]),
        ],
        [Infinity, Infinity, -Infinity, -Infinity],
      );
      const w = union[2] - union[0];
      const h = union[3] - union[1];

      // Left: the same outlines rasterized ONCE at a 64 px strike, then
      // scaled by the browser's bilinear filter — the atlas path's mechanism.
      const STRIKE = 64;
      const raster = document.createElement("canvas");
      raster.width = STRIKE;
      raster.height = Math.round((STRIKE * h) / w);
      const c2d = raster.getContext("2d");
      const s = STRIKE / w;
      c2d.setTransform(s, 0, 0, -s, -union[0] * s, union[3] * s);
      for (const layer of layers) {
        c2d.fillStyle = layer.color ? rgba(layer.color) : palette.text;
        c2d.fill(new Path2D(fig.glyphPathD(layer.glyph.curves, layer.glyph.contours)));
      }

      // Right: the same layers as resolution-independent vector fills.
      const vec = fig.svgEl("svg", {
        viewBox: `${union[0]} ${-union[3]} ${w} ${h}`,
        style: "display:block;",
      });
      const flip = fig.svgEl("g", { transform: "scale(1 -1)" });
      vec.append(flip);
      for (const layer of layers) {
        flip.append(
          fig.svgEl("path", {
            d: fig.glyphPathD(layer.glyph.curves, layer.glyph.contours),
            fill: layer.color ? rgba(layer.color) : palette.text,
            "fill-rule": "nonzero",
          }),
        );
      }

      const label = (t) =>
        fig.el("div", { style: `color:${palette.muted}; font-size:12px; margin-top:4px;` }, t);
      const left = fig.el("div", {}, raster, label(`raster at a ${STRIKE} px strike, upscaled`));
      const right = fig.el("div", {}, vec, label("vector: winding rule at display size"));
      const row = fig.el(
        "div",
        { style: "display:flex; flex-wrap:wrap; gap:20px; align-items:flex-start;" },
        left,
        right,
      );

      const setSize = (S) => {
        const ph = Math.round((S * h) / w);
        raster.style.width = `${S}px`;
        raster.style.height = `${ph}px`;
        vec.setAttribute("width", S);
        vec.setAttribute("height", ph);
      };
      setSize(200);
      const slider = fig.slider({
        label: "display size",
        min: 64,
        max: 288,
        step: 1,
        value: 200,
        format: (v) => `${v} px`,
        oninput: setSize,
      });

      host.append(
        fig.figure(
          [row, fig.controls(slider.root)],
          "Same real outlines, two delivery mechanisms. Left reproduces the " +
            "bitmap-atlas failure mode: rasterized once at a fixed strike, " +
            "then scaled in the blit, edges smear into multi-pixel gradients. " +
            "Right stays a single AA pixel wide at any size because nothing " +
            "was ever sampled. In the shipped offscreen scene the measured " +
            "gap at 200 px is 29.0% soft pixels (CBDT strike, upscaled) vs " +
            "2.5% (COLR vector), over each render's ink bounding box.",
        ),
      );
    }

    // --- Live canvas attaches last (headless one-shots screenshot the SVGs
    // before the async GPU attach lands; see CONTRACT.md ordering note).
    {
      const canvas = root.querySelector('[data-fig="masters"] canvas');
      demo = await ctx.attachDemo(canvas, {
        height: 170,
        fontSize: 30,
        text: "Grumpy wizards make toxic brew.",
      });
      demo.set_weight_blend(Number(weightSlider.input.value));
      ctx.animate(canvas, demo);
    }
  },
};
