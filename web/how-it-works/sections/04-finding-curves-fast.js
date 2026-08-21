// Section 04: the acceleration structure — band tables, sorted early-out,
// median split. Every figure is drawn from ctx.inspect, i.e. from the same
// band builder (curves.rs) the renderer uploads to the GPU; the walk the
// early-out figure simulates is the loop in shaders.wgsl `band_winding`.

export default {
  slug: "finding-curves-fast",
  title: "Finding the right curves, fast",

  html: `
    <p>The <a href="#the-winding-rule">winding-rule shader</a> has one cost
    that nothing else can hide: the loop. Left alone, a glyph's flat curve
    list is walked in full — every fragment fetches every curve, fires its
    rays, and sums the contributions. That is
    <em>O(curves)</em> per fragment, and both factors get big at once. A Latin
    glyph runs 10–60 quadratics; a dense CJK ideograph runs 100–200+. Below
    24&nbsp;px/em the shader takes three ray taps per axis, so a 200-curve
    ideograph costs 200 fetches and 200&nbsp;×&nbsp;3&nbsp;taps&nbsp;×&nbsp;2&nbsp;axes
    = 1,200 winding evaluations — <em>per pixel</em>, for every pixel the
    glyph's quad touches. This section is about three data structures that cut
    that down, under one non-negotiable constraint: the output may not move by
    a single bit.</p>

    <h3>Band tables</h3>

    <p>A horizontal ray through a sample can only be crossed by curves that
    overlap the ray's y coordinate. So at extraction time, any glyph with more
    than <code>BAND_MIN_CURVES = 16</code> curves gets <em>band tables</em>:
    its em bounding box is split into <code>BANDS = 8</code> uniform strips
    per axis, and each band stores the list of curves that could cross a ray
    inside it. Horizontal rays consult the y-bands, vertical rays the x-bands.
    Membership is decided from control-point ranges: a quadratic Bézier never
    leaves the convex hull of its three control points, so a curve joins every
    band its control-point range overlaps — widened by
    <code>BAND_EPSILON</code>, 5% of a band's height, slack that only has to
    cover floating-point disagreement between the CPU's band boundaries and
    the shader's interpolated band coordinate (<code>band_lists</code> in
    <code>curves.rs</code>).</p>

    <p>Two details are load-bearing. The bands split the glyph's <em>em
    bbox</em>, not the padded quad the instance draws: the quad's 1.5&nbsp;px
    pad is a size-dependent number of em, and the tables are baked once per
    glyph, so the instance carries a <code>band_scale</code>/<code>band_bias</code>
    pair that maps quad-local coordinates back into band space. And the two
    layouts are told apart by a flag bit: the instance's curve-count field
    donates its high bit (<code>BANDED_FLAG = 0x80000000</code>), and the
    shader branches on it once per fragment — a branch that is uniform across
    the primitive, so it costs nothing in divergence.</p>

    <div data-fig="explorer"></div>

    <p>Why is leaving a curve out of a band safe? Because an omitted curve
    contributes <em>exactly</em> 0.0 — not approximately. If a curve does not
    overlap a band, all three of its control points sit on one side of every
    ray in that band; the shader classifies crossings by the sign pattern of
    the control points' y coordinates, and the all-above / all-below patterns
    return <code>0.0</code> before any arithmetic happens
    (<code>curve_winding</code>, codes 0 and 14). The banded sum is therefore
    the flat sum minus terms that were identically zero, and the GPU test
    <code>band_tables_do_not_move_a_single_pixel</code> holds the renderer to
    that: same scene, banding forced on and off, readbacks compared byte for
    byte.</p>

    <p>One subtlety survives contact with small sizes: the three ray taps
    offset each ray perpendicular by up to a third of a pixel, which at
    11&nbsp;px can be half a band. So the band index is picked <em>per tap</em>, from
    the tapped coordinate rather than the fragment's — one extra header fetch,
    bought so that correctness never depends on the taps staying inside one
    band.</p>

    <h3>Stopping early: the sorted lists</h3>

    <p>Band tables bound <em>which</em> curves a ray sees. The next trick —
    straight from Lengyel's Slug paper — bounds <em>how many of them it has to
    look at</em>. A crossing at signed distance <em>x</em> ahead of the sample
    contributes <code>saturate(x·m + 0.5)</code> to the winding sum, where
    <em>m</em> is pixels per em (the shader's <code>inv_diameter</code>, the
    reciprocal of <code>fwidth</code>). Read that clamp as a one-pixel
    antialiasing window centered on the sample: a crossing more than half a
    pixel <em>ahead</em> saturates to 1, more than half a pixel <em>behind</em>
    saturates to 0. Now apply the convex hull again: every crossing of a curve
    satisfies <em>x&nbsp;≤&nbsp;max</em>, the largest control-point coordinate
    along the ray axis. If <code>(max − sample)·m &lt; −0.5</code>, both of
    the curve's possible crossings saturate to zero and its net contribution
    is exactly 0.0.</p>

    <p>So each band's list is sorted by that <em>max</em>, descending
    (<code>bands()</code> in <code>curves.rs</code>). The ray walks curves
    far-to-near; the moment one fetched curve's bound falls below −0.5, the
    loop breaks — every later curve has a smaller key and is even further
    behind (<code>band_winding</code> in <code>shaders.wgsl</code>). Nothing
    skipped could have contributed anything but zero, which is why this is
    still bit-identical, not merely close.</p>

    <div data-fig="earlyout"></div>

    <h3>The median split, and a lesson about being correct but slow</h3>

    <p>A forward (+x) ray counts the crossings <em>ahead</em> of the sample
    and stops at the first curve behind it — so its cost is proportional to
    how many curves sit between the sample and the glyph's far edge. A sample
    near the right edge of an <code>@</code> is cheap; a sample near the left
    edge walks nearly the whole band. The fix is Slug's band split: each band
    stores the <em>median</em> of its members' midpoints along the ray axis,
    plus a second copy of the list sorted ascending by minimum coordinate.
    Samples past the split fire their ray <em>backwards</em> off the
    ascending list. Every sample now walks toward its own nearer edge, and the
    average walk roughly halves.</p>

    <div data-fig="split"></div>

    <p>Here is the pedagogical moment. The two ray directions are
    mathematically interchangeable — a winding number is a winding number
    whichever way you count crossings — so an implementation that sends
    samples toward the <em>far</em> edge renders every pixel correctly. The
    first cut of this code did exactly that, with the split comparison
    inverted. Every image diff came back clean. The benchmark did not:
    0.63&nbsp;ms went to 0.85 instead of 0.59, because every sample was now
    walking the long way on purpose — worse than no split at all. Correct-but-slow
    is the failure mode to fear in this kind of code, precisely because
    no picture will ever show it to you.</p>

    <div data-fig="msbars"></div>

    <p>Two implementation choices keep the split from eating its own win. The
    direction is chosen with <code>select()</code>, in data, never with an
    <code>if</code> around two copies of the loop: neighbouring fragments land
    on opposite sides of a split constantly, and a warp that has to execute
    both copies pays for both. (The backward ray needs no new math at all — a
    negative <code>inv_diameter</code> and one negated sum reuse the forward
    winding code unchanged; the identity is in the gory details below.) And
    the whole mechanism rides a size gate, <code>SPLIT_MIN_PX_PER_EM = 30</code>
    — deliberately a hair under the paper's 32, so a glyph rendered at exactly
    32&nbsp;px/em lands on the fast side of its own benchmark. Below the gate,
    band lists are a texel or two long, the split's divergence costs more than
    it saves, and the 11&nbsp;px stress scene stays unmoved.</p>

    <div data-fig="bench"></div>
  `,

  async mount(root, ctx) {
    const { fig, palette: P } = ctx;

    // ---- shared helpers -------------------------------------------------

    const curveD = (c) =>
      `M ${c.p0[0]} ${c.p0[1]} Q ${c.p1[0]} ${c.p1[1]} ${c.p2[0]} ${c.p2[1]}`;

    const label = (x, y, text, attrs = {}) =>
      fig.svgEl(
        "text",
        {
          x,
          y,
          fill: P.textDim,
          "font-size": 11,
          "font-family": "ui-monospace, monospace",
          ...attrs,
        },
        text,
      );

    // A little arrow in pixel space: shaft plus a solid head.
    const arrow = (x, y, dx, len, color) => {
      const g = fig.svgEl("g", {});
      g.append(
        fig.svgEl("line", {
          x1: x,
          y1: y,
          x2: x + dx * len,
          y2: y,
          stroke: color,
          "stroke-width": 2,
        }),
        fig.svgEl("polygon", {
          points: `${x + dx * (len + 7)},${y} ${x + dx * len},${y - 4} ${x + dx * len},${y + 4}`,
          fill: color,
          stroke: "none",
        }),
      );
      return g;
    };

    const button = (text, onclick) =>
      fig.el(
        "button",
        {
          style:
            "background: transparent; color: inherit; border: 1px solid " +
            `${P.border}; border-radius: 4px; padding: 2px 10px; cursor: pointer; font: inherit;`,
          onclick,
        },
        text,
      );
    const markActive = (buttons, active) => {
      for (const [b, is] of buttons.map((b, i) => [b, i === active])) {
        b.style.borderColor = is ? P.blue : P.border;
        b.style.color = is ? P.blue : "inherit";
      }
    };

    // ---- Figure A: band explorer ---------------------------------------

    const candidates = ["@", "g", "B", "&", "Q", "∮"];
    const specimens = [];
    for (const ch of candidates) {
      const insp = await ctx.inspect(ch, "DejaVu Sans", 400);
      if (insp && insp.banded) specimens.push(insp);
    }

    const exHost = root.querySelector('[data-fig="explorer"]');
    const exState = { spec: 0, axis: "y", band: 0 };
    // Start on the band with the most members, so the figure opens showing
    // something worth hovering.
    const fullestBand = (insp, axis) => {
      const bands = insp.bands[axis];
      let best = 0;
      for (let i = 1; i < bands.length; i++) {
        if (bands[i].descending.length > bands[best].descending.length) best = i;
      }
      return best;
    };
    exState.band = fullestBand(specimens[0], "y");

    const exSvgHost = fig.el("div", {});
    const exInfo = fig.el("div", {
      style:
        "font: 12px/1.6 ui-monospace, monospace; color: " +
        `${P.textDim}; margin-top: 8px; max-height: 132px; overflow-y: auto;`,
    });

    const fmtKey = (k) => k.toFixed(4).replace(/0+$/, "").replace(/\.$/, "");
    const listLine = (name, entries) =>
      `<span style="color:${P.muted}">${name}:</span> ` +
      entries.map((e) => `#${e.curve} <span style="color:${P.muted}">${fmtKey(e.key)}</span>`).join(" · ");

    function renderExplorer() {
      const insp = specimens[exState.spec];
      const bands = insp.bands[exState.axis];
      const band = bands[exState.band];
      const [minX, minY, maxX, maxY] = insp.bbox;
      const em = fig.emSpace(insp.bbox, { width: 470, pad: 30 });
      const node = fig.svg(em.width, em.height);
      node.append(em.g);

      // The glyph, from the stored (f16) curves.
      em.g.append(
        fig.svgEl("path", {
          d: fig.glyphPathD(insp.curves, insp.contours),
          fill: P.blue,
          "fill-opacity": 0.16,
          "fill-rule": "nonzero",
          stroke: P.blue,
          "stroke-width": 1,
          "stroke-opacity": 0.7,
        }),
      );

      // Band strips with hover targets. y-bands are horizontal strips
      // (crossed by horizontal rays); x-bands are vertical.
      const horiz = exState.axis === "y";
      bands.forEach((b, i) => {
        const [lo, hi] = b.interval;
        const rect = horiz
          ? { x: minX, y: lo, width: maxX - minX, height: hi - lo }
          : { x: lo, y: minY, width: hi - lo, height: maxY - minY };
        const strip = fig.svgEl("rect", {
          ...rect,
          fill: P.orange,
          "fill-opacity": i === exState.band ? 0.14 : 0,
          stroke: P.orange,
          "stroke-opacity": 0.45,
          "stroke-width": 0.75,
          "stroke-dasharray": "3 3",
          style: "pointer-events: all; cursor: pointer;",
        });
        // svgEl sets attrs with setAttribute, so listeners go on explicitly.
        strip.addEventListener("mouseenter", () => {
          exState.band = i;
          renderExplorer();
        });
        em.g.append(strip);
      });

      // The hovered band's members, highlighted on the outline.
      for (const e of band.descending) {
        em.g.append(
          fig.svgEl("path", {
            d: curveD(insp.curves[e.curve]),
            fill: "none",
            stroke: P.green,
            "stroke-width": 2.2,
          }),
        );
      }

      // The band's median split, drawn across the strip only: rays to its
      // low side fire backwards.
      const [lo, hi] = band.interval;
      em.g.append(
        fig.svgEl("line", {
          x1: horiz ? band.split : lo,
          y1: horiz ? lo : band.split,
          x2: horiz ? band.split : hi,
          y2: horiz ? hi : band.split,
          stroke: P.purple,
          "stroke-width": 2,
        }),
      );

      // Band indices in the margin (pixel space — labels never scale).
      bands.forEach((b, i) => {
        const mid = (b.interval[0] + b.interval[1]) / 2;
        const p = horiz ? em.toPx([minX, mid]) : em.toPx([mid, maxY]);
        node.append(
          label(horiz ? p[0] - 22 : p[0] - 3, horiz ? p[1] + 4 : p[1] - 8, `${i}`, {
            fill: i === exState.band ? P.orange : P.muted,
          }),
        );
      });

      exSvgHost.replaceChildren(node);
      exInfo.innerHTML =
        `<span style="color:${P.orange}">${exState.axis}-band ${exState.band}</span> ` +
        `[${fmtKey(lo)}, ${fmtKey(hi)}] em · ${band.descending.length} of ` +
        `${insp.curves.length} curves · split at ${fmtKey(band.split)} ` +
        `<span style="color:${P.muted}">(membership margin ±${(insp.bands.epsilon * 100).toFixed(0)}% of a band)</span><br>` +
        listLine(horiz ? "descending, key = max x" : "descending, key = max y", band.descending) +
        "<br>" +
        listLine(horiz ? "ascending, key = min x" : "ascending, key = min y", band.ascending);
    }

    const specButtons = specimens.map((s, i) =>
      button(s.ch, () => {
        exState.spec = i;
        exState.band = fullestBand(specimens[i], exState.axis);
        markActive(specButtons, i);
        renderExplorer();
      }),
    );
    const axisButtons = ["y", "x"].map((axis, i) =>
      button(axis === "y" ? "y-bands (horizontal rays)" : "x-bands (vertical rays)", () => {
        exState.axis = axis;
        exState.band = fullestBand(specimens[exState.spec], axis);
        markActive(axisButtons, i);
        renderExplorer();
      }),
    );
    markActive(specButtons, 0);
    markActive(axisButtons, 0);

    exHost.append(
      fig.figure(
        [exSvgHost, fig.controls(...specButtons, ...axisButtons), exInfo],
        "The band tables the renderer actually built, via faf_text::inspect. " +
          "Hover a strip: its member curves light up green, and below are its two " +
          "index lists exactly as stored — the same curves sorted descending by " +
          "their maximum coordinate along the ray axis (for forward rays) and " +
          "ascending by their minimum (for backward rays), with each entry's " +
          "sort key. The purple tick is the band's median split. Membership is " +
          "conservative: a curve joins every band its control-point range " +
          "overlaps, widened by 5% of a band's height.",
      ),
      fig.details(
        "Gory details: how the tables sit in the texture",
        `<p>A banded block opens with 16 header texels — 8 y-band entries then
         8 x-band entries, one RGBA16F texel each, holding <code>(descending
         list offset, curve count, split coordinate, ascending list
         offset)</code>. Offsets are texels <em>from the block's base</em>, so
         compaction can relocate a whole glyph with a memcpy plus one rewrite
         of its base index. List entries are curve-record texel offsets stored
         as f16 floats, which hold integers exactly only up to 2048 — a glyph
         whose offsets would pass that stays on the flat layout (it takes a
         ~550-curve glyph to trip; DejaVu Sans tops out at offset 254). The
         header, both lists per band, and the split coordinate are all
         computed from coordinates <em>already rounded to f16</em> — the
         flattener quantizes before anything downstream reads a number, so the
         CPU sorts by exactly the values the shader will fetch.</p>
         <p>The membership epsilon is not about geometry — control points
         already bound the curve — it covers the possibility that the shader's
         interpolated band coordinate lands a float's width across a boundary
         from where the CPU put it. And because a variable-weight glyph blends
         two masters in the shader, membership and sort keys span <em>both</em>
         masters' control points: a blended point is a lerp of the two, so it
         never leaves their per-coordinate hull, and one table serves every
         weight.</p>`,
      ),
    );
    renderExplorer();

    // ---- Figure B: early-out sweep --------------------------------------

    const eoInsp = specimens[0]; // '@' if present — the longest band lists
    const eoHost = root.querySelector('[data-fig="earlyout"]');
    const PX_PER_EM = 64; // stated in the caption; well past the size gate
    const eoState = {
      band: fullestBand(eoInsp, "y"),
      frac: 0.85,
      split: true,
    };

    // The walk the shader does in band_winding at this size (per-curve
    // "eager" break — px/em is past SPLIT_MIN_PX_PER_EM): evaluate a curve,
    // then break once its bound falls behind the AA window.
    function walkBand(band, sx) {
      const back = eoState.split && sx < band.split;
      const list = back ? band.ascending : band.descending;
      const evaluated = [];
      const skipped = [];
      let broke = false;
      for (const e of list) {
        if (broke) {
          skipped.push(e.curve);
          continue;
        }
        evaluated.push(e.curve);
        const bound = (back ? sx - e.key : e.key - sx) * PX_PER_EM;
        if (bound < -0.5) broke = true;
      }
      return { back, evaluated, skipped, broke };
    }

    const eoSvgHost = fig.el("div", {});
    const eoCounter = fig.el("div", {
      style: `font: 13px/1.6 ui-monospace, monospace; color: ${P.textDim}; margin-top: 6px;`,
    });

    function renderEarlyOut() {
      const insp = eoInsp;
      const [minX, minY, maxX, maxY] = insp.bbox;
      const band = insp.bands.y[eoState.band];
      const [lo, hi] = band.interval;
      const sy = (lo + hi) / 2;
      const sx = minX + eoState.frac * (maxX - minX);
      const walk = walkBand(band, sx);

      const em = fig.emSpace(insp.bbox, { width: 470, pad: 30 });
      const node = fig.svg(em.width, em.height);
      node.append(em.g);

      // Faint whole glyph, the band strip, then the members styled by what
      // this sample's walk did with them.
      em.g.append(
        fig.svgEl("path", {
          d: fig.glyphPathD(insp.curves, insp.contours),
          fill: P.blue,
          "fill-opacity": 0.07,
          "fill-rule": "nonzero",
          stroke: P.muted,
          "stroke-width": 1,
          "stroke-opacity": 0.5,
        }),
        fig.svgEl("rect", {
          x: minX,
          y: lo,
          width: maxX - minX,
          height: hi - lo,
          fill: P.orange,
          "fill-opacity": 0.1,
          stroke: P.orange,
          "stroke-opacity": 0.5,
          "stroke-width": 0.75,
          "stroke-dasharray": "3 3",
        }),
      );
      for (const i of walk.skipped) {
        em.g.append(
          fig.svgEl("path", {
            d: curveD(insp.curves[i]),
            fill: "none",
            stroke: P.muted,
            "stroke-width": 1.6,
            "stroke-dasharray": "3 3",
          }),
        );
      }
      for (const i of walk.evaluated) {
        em.g.append(
          fig.svgEl("path", {
            d: curveD(insp.curves[i]),
            fill: "none",
            stroke: P.blue,
            "stroke-width": 2.4,
          }),
        );
      }
      // Split line across the strip.
      em.g.append(
        fig.svgEl("line", {
          x1: band.split,
          y1: lo,
          x2: band.split,
          y2: hi,
          stroke: P.purple,
          "stroke-width": 2,
        }),
      );
      // Sample point and ray direction, in pixel space.
      const [px, py] = em.toPx([sx, sy]);
      node.append(
        arrow(px, py, walk.back ? -1 : 1, 30, P.red),
        fig.svgEl("circle", { cx: px, cy: py, r: 4.5, fill: P.red, stroke: P.bgDark, "stroke-width": 2 }),
      );
      const [splitPx] = em.toPx([band.split, sy]);
      node.append(label(splitPx - 12, em.toPx([0, hi])[1] - 6, "split", { fill: P.purple }));

      eoSvgHost.replaceChildren(node);
      const n = band.descending.length;
      eoCounter.innerHTML =
        `ray: <span style="color:${P.red}">${walk.back ? "backward (ascending list)" : "forward (descending list)"}</span>` +
        ` · evaluated <span style="color:${P.blue}">${walk.evaluated.length}</span>` +
        ` of ${n} in this band <span style="color:${P.muted}">(${insp.curves.length} in the glyph` +
        `; the flat loop would evaluate all ${insp.curves.length})</span>`;
    }

    const eoSlider = fig.slider({
      label: "sample position",
      min: 0,
      max: 1,
      step: 0.005,
      value: eoState.frac,
      format: (v) => `${Math.round(v * 100)}% across`,
      oninput: (v) => {
        eoState.frac = v;
        renderEarlyOut();
      },
    });
    const eoBand = fig.slider({
      label: "y-band",
      min: 0,
      max: 7,
      step: 1,
      value: eoState.band,
      format: (v) => `${v}`,
      oninput: (v) => {
        eoState.band = v;
        renderEarlyOut();
      },
    });
    const splitToggle = fig.el("input", { type: "checkbox" });
    splitToggle.checked = true;
    splitToggle.addEventListener("change", () => {
      eoState.split = splitToggle.checked;
      renderEarlyOut();
    });
    let sweeping = false;
    const sweepBtn = button("sweep", () => {
      if (sweeping) return;
      sweeping = true;
      const t0 = performance.now();
      const step = () => {
        const t = Math.min((performance.now() - t0) / 3000, 1);
        eoSlider.set(t);
        if (t < 1) requestAnimationFrame(step);
        else sweeping = false;
      };
      requestAnimationFrame(step);
    });

    eoHost.append(
      fig.figure(
        [
          eoSvgHost,
          fig.controls(
            eoSlider.root,
            eoBand.root,
            fig.el("label", { style: "display:flex; gap:6px; align-items:center;" }, splitToggle, "median split"),
            sweepBtn,
          ),
          eoCounter,
        ],
        "One sample's walk through one band, simulated with the renderer's own " +
          "sort keys at 64 px/em. Blue curves were fetched and evaluated; dashed " +
          "gray ones sit behind the break and were never touched — each would " +
          "have contributed exactly 0.0. The red dot fires along the arrow; with " +
          "the median split on, samples left of the purple line fire backwards " +
          "off the ascending list, and the evaluated count stays small on both " +
          "sides. Turn the split off and drag the sample left: the forward ray " +
          "must now walk nearly the whole list to prove the far curves are behind it.",
      ),
      fig.details(
        "Gory details: how often to test the break",
        `<p>The break's <em>granularity</em> is itself size-gated, and that is
         worth 7%. Testing after every curve stops soonest, but makes each
         texture fetch wait on the previous curve's compare — a dependency
         chain where the GPU wants four independent fetches in flight. Above
         the gate (long lists, breaks that fire early) the per-curve test wins:
         0.59 → 0.54 ms/frame in the 32 px/em benchmark. Below it (lists a
         texel or two long, breaks that almost never fire) the same test cost
         7% in the 11 px stress scene — so small sizes test the break once per
         four-index texel, on the <em>last</em> curve of the group. The list is
         sorted, so that curve's bound is the smallest of the four and the
         group-level test is exactly as conservative; the up-to-three curves a
         group overruns contribute exactly zero anyway.</p>
         <p>With two masters, the key must bound <em>both</em>: the shader's
         early-out recomputes the same two-master maximum the CPU sorted by
         (<code>record_winding</code> keeps the masters apart for exactly
         this). Bounding the blended outline instead would be tighter — and
         wrong: it could end the loop in front of a curve that still crosses
         the ray at some other weight.</p>`,
      ),
    );
    renderEarlyOut();

    // ---- Figure C: median split, rays toward the near edge --------------

    const spInsp = specimens.find((s) => s.ch === "B") ?? eoInsp;
    const spHost = root.querySelector('[data-fig="split"]');
    {
      const insp = spInsp;
      const bandIdx = fullestBand(insp, "y");
      const band = insp.bands.y[bandIdx];
      const [minX, , maxX] = insp.bbox;
      const [lo, hi] = band.interval;
      const em = fig.emSpace(insp.bbox, { width: 470, pad: 30 });
      const node = fig.svg(em.width, em.height + 16);
      node.append(em.g);
      em.g.append(
        fig.svgEl("path", {
          d: fig.glyphPathD(insp.curves, insp.contours),
          fill: P.blue,
          "fill-opacity": 0.1,
          "fill-rule": "nonzero",
          stroke: P.blue,
          "stroke-width": 1,
          "stroke-opacity": 0.55,
        }),
        fig.svgEl("rect", {
          x: minX,
          y: lo,
          width: maxX - minX,
          height: hi - lo,
          fill: P.orange,
          "fill-opacity": 0.1,
          stroke: P.orange,
          "stroke-opacity": 0.5,
          "stroke-width": 0.75,
          "stroke-dasharray": "3 3",
        }),
        fig.svgEl("line", {
          x1: band.split,
          y1: lo,
          x2: band.split,
          y2: hi,
          stroke: P.purple,
          "stroke-width": 2,
        }),
      );
      const sy = (lo + hi) / 2;
      for (const f of [0.14, 0.62, 0.9]) {
        const backSide = f < 0.5;
        const sx = backSide
          ? minX + f * (band.split - minX) * 2
          : band.split + (f - 0.5) * (maxX - band.split) * 2;
        const [px, py] = em.toPx([sx, sy]);
        node.append(
          arrow(px, py, backSide ? -1 : 1, 26, P.red),
          fig.svgEl("circle", { cx: px, cy: py, r: 4, fill: P.red, stroke: P.bgDark, "stroke-width": 2 }),
        );
      }
      const [splitPx, stripLoPy] = em.toPx([band.split, lo]);
      const stripHiPy = em.toPx([band.split, hi])[1];
      node.append(
        label(splitPx - 38, stripHiPy - 8, "median split", { fill: P.purple }),
        label(em.toPx([minX, sy])[0], stripLoPy + 16, "← backward, ascending list", {
          fill: P.muted,
        }),
        label(em.toPx([maxX, sy])[0], stripLoPy + 16, "forward, descending list →", {
          fill: P.muted,
          "text-anchor": "end",
        }),
      );
      spHost.append(
        fig.figure(
          [node],
          `Every ray fires toward its own nearer edge. In this band of DejaVu ` +
            `'${insp.ch}' (split at ${fmtKey(band.split)} em, the median of its ` +
            `members' midpoints), samples left of the purple line cast backwards ` +
            `and stop at the first curve more than half a pixel to their right; ` +
            `samples right of it cast forwards and stop at the first curve behind ` +
            `them on the left. Both directions compute the same winding number — ` +
            `the difference is purely how many curves they touch before the break.`,
        ),
        fig.details(
          "Gory details: the backward ray is the forward code",
          `<p>Nothing in <code>curve_winding</code> knows about direction. A
           crossing's contribution is <code>saturate(0.5 + m·x)</code>;
           feed it a negative <em>m</em> and it computes
           <code>saturate(0.5 − m·x)</code>, which by the identity
           <code>saturate(0.5 − t) = 1 − saturate(0.5 + t)</code> is
           one minus the forward answer, per crossing. Summed with crossing
           signs over closed contours, the constant 1s cancel: signed
           crossings along a whole line always sum to zero. So the shader
           passes <code>-inv_diameter</code>, negates the band's sum to undo
           the sign swap (<code>band_rays</code>), and both directions agree
           <em>exactly</em> — the additions happen in a different order, but a
           band-and-ray almost never has more than two nonzero terms, and two
           floats add commutatively to the bit. That is why the offscreen
           regression scene stayed byte-identical through all of this.</p>`,
        ),
      );
    }

    // ---- Figure D: the 0.86 vs 0.54 bars --------------------------------

    const barHost = root.querySelector('[data-fig="msbars"]');
    {
      // examples/bench, RTX 3070 / Vulkan; means of the runs recorded in
      // issue #12 (individual runs in the hover titles).
      const rows = [
        { name: "sorted early-out, no split", v: 0.635, runs: "0.632 / 0.637", color: P.blue },
        { name: "split, rays toward the far edge", v: 0.862, runs: "0.845 / 0.878", color: P.red },
        { name: "split, rays toward the near edge (shipped)", v: 0.544, runs: "0.547 / 0.539 / 0.547", color: P.green },
      ];
      const W = 640;
      const left = 16;
      const top = 26;
      const rowH = 40;
      const barH = 22;
      const span = W - left - 70;
      const vmax = 1.0;
      const H = top + rows.length * rowH + 40;
      const node = fig.svg(W, H);
      // hairline gridlines + ticks
      for (const t of [0, 0.25, 0.5, 0.75, 1.0]) {
        const x = left + (t / vmax) * span;
        node.append(
          fig.svgEl("line", { x1: x, y1: top - 6, x2: x, y2: top + rows.length * rowH - 10, stroke: P.border, "stroke-width": 1 }),
          label(x - 8, H - 20, t.toFixed(2), { fill: P.muted }),
        );
      }
      node.append(label(left + span / 2 - 30, H - 4, "ms / frame", { fill: P.muted }));
      rows.forEach((r, i) => {
        const y = top + i * rowH;
        const w = (r.v / vmax) * span;
        const rr = 4;
        const bar = fig.svgEl("path", {
          d:
            `M ${left} ${y} L ${left + w - rr} ${y} Q ${left + w} ${y} ${left + w} ${y + rr} ` +
            `L ${left + w} ${y + barH - rr} Q ${left + w} ${y + barH} ${left + w - rr} ${y + barH} ` +
            `L ${left} ${y + barH} Z`,
          fill: r.color,
          stroke: "none",
        });
        bar.append(fig.svgEl("title", {}, `${r.name}: ${r.runs} ms/frame`));
        node.append(
          bar,
          label(left + w + 8, y + barH / 2 + 4, `${r.v.toFixed(2)} ms`, { fill: P.text }),
          label(left + 2, y - 5, r.name, { fill: P.textDim }),
        );
      });
      barHost.append(
        fig.figure(
          [node],
          "The cost of firing the ray the wrong way (examples/bench: a 2.10-" +
            "megapixel page, 50 lines at 32 px/em, RTX 3070, means over the " +
            "recorded runs). The far-edge build renders byte-identically to the " +
            "shipped one — every winding number is the same — but every sample " +
            "walks toward the crowded side of its band, and the split becomes " +
            "slower than no split at all: 0.86 ms against 0.54.",
        ),
      );
    }

    // ---- Figure E: bench trajectory --------------------------------------

    const benchHost = root.querySelector('[data-fig="bench"]');
    {
      // examples/bench across the two acceleration issues; the target band is
      // Slug's reported 1.1–1.3 ms (GTX 1060) divided by the ~4× factor the
      // issues used to normalize this GPU against the paper's.
      const steps = [
        { name: "flat loop", v: 1.069, detail: "every fragment walks every curve" },
        { name: "+ band tables", v: 0.639, detail: "8 bands per axis over the em bbox" },
        { name: "+ sort & split", v: 0.544, detail: "sorted early-out, median split, near-edge rays" },
      ];
      const tgtLo = 1.1 / 4.05;
      const tgtHi = 1.3 / 4.05;
      const W = 640;
      const H = 280;
      const left = 52;
      const bottom = H - 40;
      const top = 18;
      const span = bottom - top;
      const vmax = 1.2;
      const yOf = (v) => bottom - (v / vmax) * span;
      const node = fig.svg(W, H);
      for (const t of [0, 0.4, 0.8, 1.2]) {
        node.append(
          fig.svgEl("line", { x1: left, y1: yOf(t), x2: W - 12, y2: yOf(t), stroke: P.border, "stroke-width": 1 }),
          label(left - 36, yOf(t) + 4, t.toFixed(1), { fill: P.muted }),
        );
      }
      node.append(label(left - 44, 12, "ms / frame", { fill: P.muted }));
      // Target band: a wash, with a legend-style label in the clear top-right.
      node.append(
        fig.svgEl("rect", {
          x: left,
          y: yOf(tgtHi),
          width: W - 12 - left,
          height: yOf(tgtLo) - yOf(tgtHi),
          fill: P.green,
          "fill-opacity": 0.12,
          stroke: "none",
        }),
        label(W - 12, 38, "Slug's 1.1–1.3 ms on a GTX 1060, ÷≈4 for this GPU", {
          fill: P.green,
          "text-anchor": "end",
        }),
      );
      const slot = (W - 12 - left) / steps.length;
      steps.forEach((s, i) => {
        const cx = left + slot * (i + 0.5);
        const bw = 24;
        const y = yOf(s.v);
        const rr = 4;
        const bar = fig.svgEl("path", {
          d:
            `M ${cx - bw / 2} ${bottom} L ${cx - bw / 2} ${y + rr} Q ${cx - bw / 2} ${y} ${cx - bw / 2 + rr} ${y} ` +
            `L ${cx + bw / 2 - rr} ${y} Q ${cx + bw / 2} ${y} ${cx + bw / 2} ${y + rr} L ${cx + bw / 2} ${bottom} Z`,
          fill: P.blue,
          stroke: "none",
        });
        bar.append(fig.svgEl("title", {}, `${s.name}: ${s.v} ms/frame — ${s.detail}`));
        node.append(
          bar,
          label(cx - 24, y - 8, `${s.v.toFixed(2)} ms`, { fill: P.text }),
          label(cx - 40, H - 20, s.name, { fill: P.textDim }),
        );
      });
      benchHost.append(
        fig.figure(
          [node],
          "The same benchmark across the two acceleration rounds: 1.07 ms with " +
            "the flat loop, 0.64 with band tables, 0.54 with the sorted lists and " +
            "median split — output byte-identical at every step. The green band " +
            "is the Slug paper's reported range for its Table-2 workload on a " +
            "GTX 1060, divided by the ~4× factor used to compare that GPU " +
            "to this one — a rough normalization, so read it as a target " +
            "neighborhood, not a finish line: this renderer lands within about " +
            "2× of the reference implementation.",
        ),
      );
    }
  },
};
