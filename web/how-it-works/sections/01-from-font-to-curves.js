// Section 01 — the outline pipeline: what lives in a font file, how faf-text
// extracts it (size 1.0, hinting off), and how every source primitive becomes
// one kind of quadratic record. Source of truth: crates/faf-text/src/curves.rs
// (Flattener, glyph_outline) and the winding shader in shaders.wgsl. The JS
// helpers below that redo renderer math mirror those functions line for line
// and say so where they do.

// --- small math helpers -----------------------------------------------------

const sub = (a, b) => [a[0] - b[0], a[1] - b[1]];
const add = (a, b) => [a[0] + b[0], a[1] + b[1]];
const mul = (a, s) => [a[0] * s, a[1] * s];
const len = (a) => Math.hypot(a[0], a[1]);

// Mirrors `lerp` in curves.rs.
const lerp2 = (a, b, t) => [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];

// Mirrors `split_cubic` in curves.rs: one de Casteljau step at t.
function splitCubic(c, t) {
  const ab = lerp2(c[0], c[1], t);
  const bc = lerp2(c[1], c[2], t);
  const cd = lerp2(c[2], c[3], t);
  const abc = lerp2(ab, bc, t);
  const bcd = lerp2(bc, cd, t);
  const mid = lerp2(abc, bcd, t);
  return [[c[0], ab, abc, mid], [mid, bcd, cd, c[3]]];
}

// Mirrors `Flattener::cubic_to` in curves.rs: split at 1/2 twice (quarters),
// then one quadratic per quarter with control (3(c1+c2) − p0 − p3) / 4.
function cubicToQuads(cubic) {
  const quads = [];
  const [a, b] = splitCubic(cubic, 0.5);
  for (const half of [a, b]) {
    for (const seg of splitCubic(half, 0.5)) {
      const control = [
        (3.0 * (seg[1][0] + seg[2][0]) - seg[0][0] - seg[3][0]) * 0.25,
        (3.0 * (seg[1][1] + seg[2][1]) - seg[0][1] - seg[3][1]) * 0.25,
      ];
      quads.push({ p0: seg[0], p1: control, p2: seg[3] });
    }
  }
  return quads;
}

function evalQuad(q, t) {
  const s = 1 - t;
  return [
    s * s * q.p0[0] + 2 * s * t * q.p1[0] + t * t * q.p2[0],
    s * s * q.p0[1] + 2 * s * t * q.p1[1] + t * t * q.p2[1],
  ];
}

function evalCubic(c, t) {
  const s = 1 - t;
  const b0 = s * s * s, b1 = 3 * s * s * t, b2 = 3 * s * t * t, b3 = t * t * t;
  return [
    b0 * c[0][0] + b1 * c[1][0] + b2 * c[2][0] + b3 * c[3][0],
    b0 * c[0][1] + b1 * c[1][1] + b2 * c[2][1] + b3 * c[3][1],
  ];
}

// Replays the outline commands the way `flatten_with` in curves.rs feeds them
// to the Flattener, so entry i labels stored curve i with the source command
// it came from: "quad" (quad_to, one record), "line" (line_to, degenerate
// record), "cubic" (curve_to, one of four records), or "close" (the closing
// edge close()/move_to emits when a contour ends away from its start).
function provenance(outline) {
  const out = [];
  let start = null, current = null, open = false;
  const close = () => {
    if (open && current && (current[0] !== start[0] || current[1] !== start[1])) {
      out.push({ kind: "close" });
    }
    open = false;
  };
  let prevQuad = null; // for implied-midpoint detection on consecutive quads
  for (const cmd of outline) {
    if (cmd.type === "move_to") {
      close();
      start = cmd.p; current = cmd.p; open = true;
      prevQuad = null;
    } else if (cmd.type === "line_to") {
      out.push({ kind: "line" });
      current = cmd.p; prevQuad = null;
    } else if (cmd.type === "quad_to") {
      const entry = { kind: "quad" };
      // TrueType stores runs of off-curve points; the on-curve point between
      // two of them is implied at their midpoint. swash reconstructs those,
      // and the reconstruction is recognizable: the shared endpoint of two
      // consecutive quad_to commands sits exactly between their controls.
      if (prevQuad &&
          Math.abs(prevQuad.cmd.p[0] - (prevQuad.cmd.c[0] + cmd.c[0]) / 2) < 1e-6 &&
          Math.abs(prevQuad.cmd.p[1] - (prevQuad.cmd.c[1] + cmd.c[1]) / 2) < 1e-6) {
        prevQuad.entry.impliedEnd = true;
      }
      out.push(entry);
      prevQuad = { cmd, entry };
      current = cmd.p;
    } else if (cmd.type === "curve_to") {
      for (let k = 0; k < 4; k++) out.push({ kind: "cubic", quarter: k });
      current = cmd.p; prevQuad = null;
    } else if (cmd.type === "close") {
      close();
      prevQuad = null;
    }
  }
  close();
  return out;
}

// Signed area of one contour (y-up: positive = counter-clockwise), from the
// exact integral under each quadratic segment.
function contourArea(curves) {
  let area = 0;
  for (const { p0, p1, p2 } of curves) {
    // ∮ x dy for one quadratic Bézier, closed by the caller's contour.
    area += (p0[0] * (2 * p1[1] + p2[1] - 3 * p0[1]) +
             2 * p1[0] * (p2[1] - p0[1]) +
             p2[0] * (3 * p2[1] - p1[1] * 2 - p0[1])) / 6;
  }
  return area;
}

// Winding number at `pt` by the shader's own crossing rules: a port of
// `curve_winding` in shaders.wgsl with the antialiasing ramp replaced by a
// hard step (same sign-pattern masks, same root selection).
const T0_MASK = 0x454, T1_MASK = 0x1510;
function curveCrossings(p0, p1, p2) {
  const code = (p0[1] > 0 ? 2 : 0) | (p1[1] > 0 ? 4 : 0) | (p2[1] > 0 ? 8 : 0);
  if (code === 0 || code === 14) return 0;
  const ay = p0[1] - 2 * p1[1] + p2[1];
  const by = p0[1] - p1[1];
  const ax = p0[0] - 2 * p1[0] + p2[0];
  const bx = p0[0] - p1[0];
  const xAt = (t) => (ax * t - 2 * bx) * t + p0[0];
  let alpha = 0;
  if (Math.abs(ay) > 1e-4) {
    const s = Math.sqrt(Math.max(by * by - ay * p0[1], 0));
    if ((T0_MASK >> code) & 1) alpha += xAt((by - s) / ay) > 0 ? 1 : 0;
    if ((T1_MASK >> code) & 1) alpha -= xAt((by + s) / ay) > 0 ? 1 : 0;
  } else {
    let sign = 0;
    if (p0[1] > 0 && p2[1] <= 0) sign = 1;
    else if (p2[1] > 0 && p0[1] <= 0) sign = -1;
    if (sign !== 0) alpha = sign * (xAt(p0[1] / (2 * by)) > 0 ? 1 : 0);
  }
  return alpha;
}

function windingAt(pt, curves) {
  let w = 0;
  for (const q of curves) {
    w += curveCrossings(sub(q.p0, pt), sub(q.p1, pt), sub(q.p2, pt));
  }
  return w;
}

const fmt = (v, digits = 6) => {
  const r = Number(v.toFixed(digits));
  return Object.is(r, -0) ? "0" : `${r}`;
};
const fmtPt = (p) => `(${fmt(p[0])}, ${fmt(p[1])})`;

const MONO = "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace";

export default {
  slug: "from-font-to-curves",
  title: "From font file to curves",

  html: `
    <p>A font file does not contain pictures of letters. A TrueType glyph is a
    list of closed contours, each a loop of points tagged <em>on-curve</em> or
    <em>off-curve</em>: every off-curve point is the control point of a
    quadratic Bézier between its on-curve neighbors, and two consecutive
    on-curve points make a straight edge. The format even elides points it can
    infer — between two consecutive <em>off</em>-curve points there is an
    implied on-curve point at their midpoint, so a circle-ish bowl can be a
    run of control points with the interpolated skeleton left unstated.
    CFF-flavored OpenType describes the same contours with cubic Béziers
    instead. Either way, what a renderer gets is piecewise polynomial curves
    and a fill rule, and everything faf-text does downstream — the curve
    texture, the band tables, the per-pixel winding test — wants exactly one
    primitive. This section is about getting from the font's vocabulary to
    that one primitive without lying about the shape.</p>

    <p>Extraction happens once per glyph, in
    <code>glyph_outline</code> (<code>curves.rs</code>): swash is asked for
    the outline at <strong>size 1.0</strong> with
    <code>DISABLE_HINTING</code> set. Both halves of that are load-bearing.
    Hinting is a grid-snap — it nudges outline points onto the pixel grid
    <em>of one specific size</em>, which is precisely the dependency this
    renderer must not have. And at size 1.0, the coordinates that come back
    are pure em units: the font's own design coordinates divided by its
    units-per-em, roughly spanning <code>[-0.2, 1.2]</code> for Latin text.
    The result is the core caching trick of the whole engine: an outline
    extracted once is correct at <em>every size it will ever be drawn</em>,
    because scaling became a per-instance multiply in the vertex shader
    rather than a property of the data. There is no 12&nbsp;px version of a
    glyph anywhere in the system — there is one set of curves, and there are
    sizes.</p>

    <div data-fig="emspace"></div>

    <p>The command stream — <code>move_to</code>, <code>line_to</code>,
    <code>quad_to</code>, <code>curve_to</code>, <code>close</code> — feeds
    the <code>Flattener</code> (<code>curves.rs</code>), which speaks exactly
    one primitive on the way out: the quadratic record
    <code>{p0,&nbsp;p1,&nbsp;p2}</code>. A <code>quad_to</code> passes
    through untouched. A <code>line_to</code> becomes a <em>degenerate</em>
    quadratic with the control point at the chord midpoint — and that is not
    an approximation. Substitute <code>m&nbsp;=&nbsp;(p0+p2)/2</code> into the
    Bézier and the quadratic terms cancel exactly:
    <code>(1−t)²p0 + 2t(1−t)m + t²p2 = (1−t)p0 + tp2</code>, the line itself,
    identical parameterization and all. The payoff for forcing everything
    into one shape is in the fragment shader: the inner loop fetches
    fixed-size records and evaluates them all the same way, with no
    per-record type dispatch. That is not a stylistic preference — a single
    data-dependent branch in that loop has been measured at 25% of frame time
    on this shader. Lines cost the same texels as any other record, and the
    shader never learns they were lines.</p>

    <div data-fig="explorer"></div>

    <p>Cubics take actual work. <code>Flattener::cubic_to</code> splits the
    cubic at <code>t&nbsp;=&nbsp;½</code>, splits each half at ½ again (de
    Casteljau both times), and replaces each quarter with one quadratic whose
    control point is <code>(3(c1+c2)&nbsp;−&nbsp;p0&nbsp;−&nbsp;p3)/4</code>.
    That formula is the average of the two candidates that each preserve one
    end tangent — <code>(3c1−p0)/2</code> and <code>(3c2−p3)/2</code> — it
    splits their disagreement down the middle rather than favoring either
    end. The error is not hand-wavy: it has a
    closed form (derivation in the fold below), maxing out at
    <code>√3/36&nbsp;·&nbsp;‖d‖</code> per segment, where
    <code>d&nbsp;=&nbsp;p3&nbsp;−&nbsp;3c2&nbsp;+&nbsp;3c1&nbsp;−&nbsp;p0</code>
    is the cubic's third finite difference. Each halving shrinks
    <code>d</code> by 8×, so two levels of splitting divide the bound by 64:
    the four-quadratic approximation sits within
    <code>√3·‖d‖/2304&nbsp;≈&nbsp;0.00075·‖d‖</code> em of the true cubic.
    For a violently curled cubic with <code>‖d‖ = 1</code> em that is
    ~7.5×10⁻⁴ em — the same order as the 4.9×10⁻⁴ em f16 grid the curve
    texture rounds to anyway, and at 96&nbsp;px/em it is seven hundredths of
    a pixel. The code comment says "comfortably below visible error at text
    sizes" and the numbers back it up. The fonts embedded in this page are
    all TrueType, so this path never fires here — the figure below runs the
    exact same arithmetic live instead.</p>

    <div data-fig="cubic"></div>

    <p>Contours close by rule, not by trust. <code>Flattener::close</code>
    emits the closing edge as a <code>line_to</code> back to the contour's
    start — but only when the pen is not already there, so no zero-length
    record is ever stored. A <code>move_to</code> implicitly closes whatever
    contour was open (fonts do leave contours unterminated), and the
    flattener closes the final contour after the last command. Alongside the
    records, it keeps a count of curves per contour — which is what later
    lets banded blocks share endpoints, because within one contour, curve
    <code>i</code>'s <code>p2</code> and curve <code>i+1</code>'s
    <code>p0</code> are the <em>same float</em>, carried across by the
    flattener rather than recomputed.</p>

    <p>The fill rule is nonzero winding, computed per pixel in the fragment
    shader as a sum of signed ray crossings. Which raises a convention
    problem: TrueType winds outer contours clockwise (in y-up font space) and
    holes counter-clockwise; CFF, with its PostScript heritage, does exactly
    the opposite. faf-text never checks. The shader takes
    <code>abs()</code> of the winding sum before clamping it into coverage —
    a hole still cancels its surrounding contour to zero <em>before</em> the
    abs, so counters stay empty, and a font with either convention (or a
    sloppy glyph that mixes them) fills identically. Orientation is a fact
    the renderer measures per pixel, not a promise it extracts from the
    file's flavor.</p>

    <div data-fig="winding"></div>

    <p>That is the whole diet: after this stage a glyph is a flat list of
    f16-quantized quadratics, a curve count per contour, and a bounding box —
    no arcs, no cubics, no line/curve distinction, nothing the fragment
    shader would ever need to branch on. The next sections are about
    <a href="#the-winding-rule">how the shader turns those numbers into
    coverage</a> and <a href="#memory-and-caching">where they live on the
    GPU</a>.</p>
  `,

  async mount(root, ctx) {
    const { fig, palette } = ctx;

    // ---- Figure: em-space diagram (one curve set, three sizes) ------------
    {
      const [g, H, x] = await Promise.all([
        ctx.inspect("g", "DejaVu Sans", 400),
        ctx.inspect("H", "DejaVu Sans", 400),
        ctx.inspect("x", "DejaVu Sans", 400),
      ]);
      const cap = H.bbox[3], xh = x.bbox[3], desc = g.bbox[1];
      const sizes = [20, 48, 110];
      const W = 560, pad = 20;
      const baseY = pad + cap * sizes[2] + 8;
      const height = Math.ceil(baseY - desc * sizes[2] + 26);
      const node = fig.svg(W, height);
      const guideEnd = 350;
      const guides = [
        { y: 0, label: "baseline  y = 0", color: palette.textDim, dash: null },
        { y: cap, label: `cap height  y = ${fmt(cap)}`, color: palette.muted, dash: "5 4" },
        { y: xh, label: `x-height  y = ${fmt(xh)}`, color: palette.muted, dash: "5 4" },
        { y: desc, label: `descender  y = ${fmt(desc)}`, color: palette.muted, dash: "5 4" },
      ];
      for (const gl of guides) {
        const py = baseY - gl.y * sizes[2];
        node.append(
          fig.svgEl("line", {
            x1: 12, y1: py, x2: guideEnd, y2: py,
            stroke: gl.color, "stroke-width": 1,
            "stroke-dasharray": gl.dash,
          }),
          fig.svgEl("text", {
            x: guideEnd + 10, y: py + 4, fill: gl.color,
            "font-size": 12, "font-family": MONO,
          }, gl.label),
        );
      }
      const d = fig.glyphPathD(g.curves, g.contours);
      let penX = 26;
      for (const s of sizes) {
        node.append(
          fig.svgEl("path", {
            d,
            fill: palette.blue,
            "fill-rule": "nonzero",
            transform: `translate(${penX - g.bbox[0] * s} ${baseY}) scale(${s} ${-s})`,
          }),
          fig.svgEl("text", {
            x: penX + ((g.bbox[2] - g.bbox[0]) * s) / 2,
            y: baseY - desc * sizes[2] + 18,
            fill: palette.muted, "font-size": 12, "font-family": MONO,
            "text-anchor": "middle",
          }, `${s} px/em`),
        );
        penX += (g.bbox[2] - g.bbox[0]) * s + 52;
      }
      root.querySelector('[data-fig="emspace"]').append(
        fig.figure(
          [node],
          `One curve set, three sizes. All three 'g's are the same ` +
            `${g.curves.length} quadratics in em units (y-up, baseline at 0), ` +
            `occupying ${g.layout.total_texels} texels of curve texture no matter ` +
            `how large they are drawn — size is a multiply at render time, not a ` +
            `property of the data. Guides are real metrics read back from the ` +
            `outlines: cap height is the top of DejaVu's 'H' bbox, x-height the ` +
            `top of 'x', descender the bottom of 'g'.`,
        ),
      );
    }

    // ---- Figure: interactive outline explorer -----------------------------
    {
      const host = root.querySelector('[data-fig="explorer"]');
      const families = await ctx.listFamilies();
      const inputStyle =
        `background:${palette.bgDark};color:${palette.text};border:1px solid ` +
        `${palette.border};border-radius:6px;padding:4px 8px;font-family:${MONO};font-size:14px;`;
      const charInput = fig.el("input", {
        type: "text", value: "g", style: inputStyle + "width:3.5em;text-align:center;",
        "aria-label": "character to inspect",
      });
      const familySel = fig.el(
        "select", { style: inputStyle },
        ...families.map((f) => fig.el("option", { value: f }, f)),
      );
      const drawing = fig.el("div", {});
      const info = fig.el("div", {
        style:
          `font-family:${MONO};font-size:12.5px;line-height:1.5;white-space:pre-line;` +
          `color:${palette.textDim};min-height:4.6em;margin-top:2px;`,
      });

      let token = 0;
      const draw = async () => {
        const my = ++token;
        const ch = Array.from(charInput.value)[0];
        if (!ch) return;
        const family = familySel.value;
        const insp = await ctx.inspect(ch, family, 400);
        if (my !== token) return;
        drawing.replaceChildren();
        info.textContent = "";
        if (!insp) {
          info.textContent = `no glyph for ${JSON.stringify(ch)} in ${family}`;
          return;
        }

        // COLR base glyphs have no outline of their own — show the layers.
        if (insp.curves.length === 0 && insp.colr) {
          let bbox = [Infinity, Infinity, -Infinity, -Infinity];
          for (const layer of insp.colr) {
            bbox = [
              Math.min(bbox[0], layer.glyph.bbox[0]), Math.min(bbox[1], layer.glyph.bbox[1]),
              Math.max(bbox[2], layer.glyph.bbox[2]), Math.max(bbox[3], layer.glyph.bbox[3]),
            ];
          }
          const em = fig.emSpace(bbox, { width: 380, pad: 22 });
          const node = fig.svg(em.width, em.height);
          node.append(em.g);
          let total = 0;
          for (const layer of insp.colr) {
            total += layer.glyph.curves.length;
            const c = layer.color;
            const css = c
              ? `rgba(${Math.round(c[0] * 255)},${Math.round(c[1] * 255)},${Math.round(c[2] * 255)},${c[3]})`
              : palette.text;
            em.g.append(
              fig.svgEl("path", {
                d: fig.glyphPathD(layer.glyph.curves, layer.glyph.contours),
                fill: css, "fill-rule": "nonzero",
              }),
            );
          }
          drawing.append(node);
          info.textContent =
            `glyph ${insp.glyph_id} — COLRv0 color glyph: ${insp.colr.length} layers, ` +
            `${total} quadratics total, drawn bottom to top.\n` +
            `Each layer is an ordinary outline glyph; a later section covers the stack.`;
          return;
        }
        if (insp.curves.length === 0) {
          info.textContent =
            `glyph ${insp.glyph_id} has no outline (whitespace, or a bitmap-only glyph).`;
          return;
        }

        const prov = provenance(insp.outline);
        const kinds = { quad: 0, line: 0, cubic: 0, close: 0 };
        for (const p of prov) kinds[p.kind]++;
        const provOk = prov.length === insp.curves.length;

        const em = fig.emSpace(insp.bbox, { width: 380, pad: 22 });
        const node = fig.svg(em.width, em.height);
        node.append(em.g);
        em.g.append(
          fig.svgEl("path", {
            d: fig.glyphPathD(insp.curves, insp.contours),
            fill: palette.blue, "fill-opacity": 0.28, "fill-rule": "nonzero",
            stroke: palette.blue, "stroke-width": 1.4,
          }),
        );
        const highlight = fig.svgEl("path", {
          d: "", fill: "none", stroke: palette.orange, "stroke-width": 3,
          "stroke-linecap": "round", "pointer-events": "none",
        });

        // contour index per curve, from the prefix sums of `contours`
        const contourOf = [];
        insp.contours.forEach((n, ci) => { for (let k = 0; k < n; k++) contourOf.push(ci); });

        const summary =
          `glyph ${insp.glyph_id} · ${insp.curves.length} quadratics in ` +
          `${insp.contours.length} contour${insp.contours.length === 1 ? "" : "s"} ` +
          `[${insp.contours.join(", ")}] · block ${insp.layout.total_texels} texels` +
          (provOk
            ? `\nsources: ${kinds.quad} quad_to · ${kinds.line} line_to · ` +
              `${kinds.cubic / 4 > 0 ? `${kinds.cubic / 4} curve_to (×4 each)` : "0 curve_to"} · ` +
              `${kinds.close} closing edge${kinds.close === 1 ? "" : "s"}` +
              `\nhover a curve for its stored control points`
            : "\nhover a curve for its stored control points");
        info.textContent = summary;

        const kindLabel = (p) => {
          if (!p) return "";
          if (p.kind === "line") return "from line_to — control at the chord midpoint";
          if (p.kind === "close") return "closing edge — emitted by close(), control at midpoint";
          if (p.kind === "cubic") return `quarter ${p.quarter + 1}/4 of a curve_to cubic`;
          return p.impliedEnd
            ? "from quad_to — its endpoint was an implied on-curve midpoint"
            : "from quad_to";
        };

        insp.curves.forEach((q, i) => {
          const p = provOk ? prov[i] : null;
          const isLine = p ? p.kind === "line" || p.kind === "close"
            : Math.abs(q.p1[0] - (q.p0[0] + q.p2[0]) / 2) < 1e-3 &&
              Math.abs(q.p1[1] - (q.p0[1] + q.p2[1]) / 2) < 1e-3;
          const dPath = `M ${q.p0[0]} ${q.p0[1]} Q ${q.p1[0]} ${q.p1[1]} ${q.p2[0]} ${q.p2[1]}`;
          const hit = fig.svgEl("path", {
            d: dPath, fill: "none", stroke: "#000", "stroke-opacity": 0,
            "stroke-width": 11, "pointer-events": "stroke", cursor: "crosshair",
          });
          hit.addEventListener("mouseenter", () => {
            highlight.setAttribute("d", dPath);
            const raw = insp.curves_raw[i];
            let dmax = 0;
            for (const k of ["p0", "p1", "p2"]) {
              dmax = Math.max(dmax, Math.abs(q[k][0] - raw[k][0]), Math.abs(q[k][1] - raw[k][1]));
            }
            info.textContent =
              `curve ${i} · contour ${contourOf[i]} · ${kindLabel(p) || (isLine ? "degenerate (line)" : "quadratic")}` +
              `\np0 ${fmtPt(q.p0)}  p1 ${fmtPt(q.p1)}  p2 ${fmtPt(q.p2)}` +
              `\nf16 rounding: ${dmax === 0 ? "every coordinate survived exactly" : `moved a coordinate by ${dmax.toExponential(2)} em`}` +
              `\nstored as ${insp.banded ? "1 texel + shared neighbor (banded block)" : "2 texels [p0 p1][p2 · ·] (flat block)"}`;
          });
          hit.addEventListener("mouseleave", () => {
            highlight.setAttribute("d", "");
            info.textContent = summary;
          });
          em.g.append(hit);
          // control point: purple when a genuine curve, muted when it is just
          // a line's midpoint control
          em.g.append(
            fig.svgEl("circle", {
              cx: q.p1[0], cy: q.p1[1], r: (isLine ? 2 : 3) / em.scale,
              fill: isLine ? palette.muted : palette.purple, "pointer-events": "none",
            }),
          );
          // on-curve endpoint; hollow when it was an implied TrueType midpoint
          const implied = p && p.kind === "quad" && p.impliedEnd;
          em.g.append(
            fig.svgEl("circle", {
              cx: q.p2[0], cy: q.p2[1], r: 2.4 / em.scale,
              fill: implied ? "none" : palette.teal,
              stroke: implied ? palette.teal : null,
              "stroke-width": implied ? 1.4 : null,
              "pointer-events": "none",
            }),
          );
        });
        em.g.append(highlight);
        drawing.append(node);
      };

      charInput.addEventListener("input", draw);
      familySel.addEventListener("change", draw);
      await draw();

      host.append(
        fig.figure(
          [
            fig.controls(
              fig.el("label", { style: `color:${palette.muted};font-size:14px;` }, "character"),
              charInput, familySel,
            ),
            drawing,
            info,
          ],
          "The outline explorer: type any character and see the renderer's " +
            "actual stored quadratics, straight out of the extraction code. " +
            "Teal dots are on-curve endpoints (hollow ones were implied " +
            "TrueType midpoints, reconstructed between two off-curve points); " +
            "purple dots are real control points; small muted dots are the " +
            "chord-midpoint controls of lines that became degenerate " +
            "quadratics. Hover any segment for its coordinates and its cost " +
            "in texels.",
        ),
        fig.details(
          "Where these numbers go next",
          `<p>Each quadratic is stored in an RGBA16F texture — in a flat block
           (≤16 curves) as two texels, <code>[p0.x p0.y p1.x p1.y]</code>
           <code>[p2.x p2.y · ·]</code>. Banded blocks exploit the closing
           rules above: within a contour, curve <code>i</code>'s
           <code>p2</code> is curve <code>i+1</code>'s <code>p0</code> —
           the same float, carried by the flattener — so the next curve's
           texel already holds this curve's endpoint and a contour of
           <em>n</em> curves fits in <em>n</em>+1 texels. Control points are
           rounded to f16 <em>inside</em> <code>Flattener::push</code>, the
           one place a coordinate enters the pipeline, so the bounding box,
           band membership, sort keys and shader all describe one outline.
           TrueType coordinates are 1/2048-grid fractions that mostly survive
           f16 exactly below 1 em; the hover readout shows the delta when one
           does not. Later sections cover <a href="#memory-and-caching">the
           texture layout</a> and <a href="#finding-curves-fast">the band
           tables</a> in full.</p>`,
        ),
      );
    }

    // ---- Figure: cubic → four quadratics, interactive ---------------------
    {
      const host = root.querySelector('[data-fig="cubic"]');
      const W = 460;
      const vx0 = -0.02, vx1 = 1.02, vy0 = -0.28, vy1 = 0.44;
      const scale = W / (vx1 - vx0);
      const H = Math.ceil((vy1 - vy0) * scale);
      const toS = (p) => [(p[0] - vx0) * scale, H - (p[1] - vy0) * scale];
      const toEm = (s) => [s[0] / scale + vx0, (H - s[1]) / scale + vy0];

      const pts = { p0: [0.05, 0.02], c1: [0.28, 0.4], c2: [0.72, -0.2], p3: [0.95, 0.26] };

      const node = fig.svg(W, H, { style: "max-width:100%;height:auto;touch-action:none;" });
      const handleLines = fig.svgEl("path", {
        d: "", fill: "none", stroke: palette.muted, "stroke-width": 1, "stroke-dasharray": "3 3",
      });
      const quadPolys = fig.svgEl("path", {
        d: "", fill: "none", stroke: palette.purple, "stroke-width": 1, "stroke-dasharray": "3 3",
        "stroke-opacity": 0.8,
      });
      const quadsPath = fig.svgEl("path", { d: "", fill: "none", stroke: palette.blue, "stroke-width": 3 });
      const cubicPath = fig.svgEl("path", {
        d: "", fill: "none", stroke: palette.orange, "stroke-width": 1.5, "stroke-dasharray": "6 4",
      });
      const dotsGroup = fig.svgEl("g", {});
      const handlesGroup = fig.svgEl("g", {});
      node.append(handleLines, quadPolys, quadsPath, cubicPath, dotsGroup, handlesGroup);

      const spark = fig.svg(W, 54);
      const sparkLine = fig.svgEl("polyline", {
        points: "", fill: "none", stroke: palette.red, "stroke-width": 1.5,
      });
      spark.append(
        fig.svgEl("line", { x1: 0, y1: 50, x2: W, y2: 50, stroke: palette.muted, "stroke-width": 1 }),
        sparkLine,
      );
      const readout = fig.el("div", {
        style: `font-family:${MONO};font-size:12.5px;color:${palette.textDim};white-space:pre-line;`,
      });

      const handleDefs = [
        { key: "p0", color: palette.teal }, { key: "c1", color: palette.orange },
        { key: "c2", color: palette.orange }, { key: "p3", color: palette.teal },
      ];
      const handles = handleDefs.map(({ key, color }) => {
        const c = fig.svgEl("circle", { r: 6, fill: color, cursor: "grab", "fill-opacity": 0.9 });
        handlesGroup.append(c);
        return { key, node: c };
      });

      const update = () => {
        const cubic = [pts.p0, pts.c1, pts.c2, pts.p3];
        const quads = cubicToQuads(cubic);
        const S = (p) => toS(p).join(" ");
        cubicPath.setAttribute("d", `M ${S(pts.p0)} C ${S(pts.c1)} ${S(pts.c2)} ${S(pts.p3)}`);
        quadsPath.setAttribute(
          "d", quads.map((q) => `M ${S(q.p0)} Q ${S(q.p1)} ${S(q.p2)}`).join(" "),
        );
        quadPolys.setAttribute(
          "d", quads.map((q) => `M ${S(q.p0)} L ${S(q.p1)} L ${S(q.p2)}`).join(" "),
        );
        handleLines.setAttribute(
          "d", `M ${S(pts.p0)} L ${S(pts.c1)} M ${S(pts.p3)} L ${S(pts.c2)}`,
        );
        dotsGroup.replaceChildren();
        quads.forEach((q, i) => {
          const [cx, cy] = toS(q.p1);
          dotsGroup.append(fig.svgEl("circle", { cx, cy, r: 3.5, fill: palette.purple }));
          if (i > 0) {
            const [sx, sy] = toS(q.p0);
            dotsGroup.append(fig.svgEl("circle", { cx: sx, cy: sy, r: 3, fill: palette.teal }));
          }
        });
        for (const h of handles) {
          const [cx, cy] = toS(pts[h.key]);
          h.node.setAttribute("cx", cx);
          h.node.setAttribute("cy", cy);
        }

        // measured parameterwise error vs the closed-form bound
        const d3 = [
          pts.p3[0] - 3 * pts.c2[0] + 3 * pts.c1[0] - pts.p0[0],
          pts.p3[1] - 3 * pts.c2[1] + 3 * pts.c1[1] - pts.p0[1],
        ];
        const bound = (Math.sqrt(3) / 36) * len(d3) / 64;
        const N = 512;
        let measured = 0;
        const errs = new Array(N + 1);
        for (let i = 0; i <= N; i++) {
          const t = i / N;
          const k = Math.min(3, Math.floor(t * 4));
          const u = t * 4 - k;
          const e = len(sub(evalCubic(cubic, t), evalQuad(quads[k], u)));
          errs[i] = e;
          measured = Math.max(measured, e);
        }
        const top = Math.max(measured, 1e-12);
        sparkLine.setAttribute(
          "points",
          errs.map((e, i) => `${(i / N) * W},${50 - (e / top) * 44}`).join(" "),
        );
        readout.textContent =
          `‖d‖ = ${fmt(len(d3), 4)} em → bound √3·‖d‖/2304 = ${bound.toExponential(3)} em` +
          `\nmeasured max ‖cubic(t) − quad(t)‖ = ${measured.toExponential(3)} em` +
          ` = ${fmt(measured * 96, 4)} px at 96 px/em`;
      };

      // dragging
      let active = null;
      const pointerPos = (ev) => {
        const r = node.getBoundingClientRect();
        return [((ev.clientX - r.left) / r.width) * W, ((ev.clientY - r.top) / r.height) * H];
      };
      node.addEventListener("pointerdown", (ev) => {
        const p = pointerPos(ev);
        let best = null, bestD = 18;
        for (const h of handles) {
          const d = len(sub(toS(pts[h.key]), p));
          if (d < bestD) { bestD = d; best = h.key; }
        }
        if (best) {
          active = best;
          try { node.setPointerCapture(ev.pointerId); } catch { /* synthetic event */ }
          ev.preventDefault();
        }
      });
      node.addEventListener("pointermove", (ev) => {
        if (!active) return;
        const [ex, ey] = toEm(pointerPos(ev));
        pts[active] = [
          Math.min(vx1, Math.max(vx0, ex)),
          Math.min(vy1, Math.max(vy0, ey)),
        ];
        update();
      });
      const release = () => { active = null; };
      node.addEventListener("pointerup", release);
      node.addEventListener("pointercancel", release);

      update();

      host.append(
        fig.figure(
          [node, spark, readout],
          "Cubic → four quadratics, live — drag any handle. The dashed " +
            "orange curve is the true cubic; blue is the four quadratics " +
            "Flattener::cubic_to replaces it with (this figure runs the same " +
            "split-at-halves, control = (3(c1+c2)−p0−p3)/4 arithmetic). Purple " +
            "dots and dashed legs are the sub-quadratics' control points, teal " +
            "the split points. You mostly cannot see the orange curve: the " +
            "deviation, plotted below in red at its own scale, stays " +
            "thousandths of an em. The strip shows the closed-form error " +
            "shape — two lobes per quarter, ∝ t(1−t)(1−2t).",
        ),
        fig.details(
          "The error bound, derived",
          `<p>Let the cubic be <code>B(t)</code> with points
           <code>p0,c1,c2,p3</code> and let <code>Q(t)</code> be the quadratic
           with the control above. Degree-elevate <code>Q</code> to a cubic —
           its inner points become <code>(p0+2q1)/3</code> and
           <code>(2q1+p3)/3</code> — and subtract. The endpoint terms vanish,
           and the inner differences both reduce to the same vector:
           <code>±d/6</code>, where
           <code>d = p3 − 3c2 + 3c1 − p0</code> (the third finite difference,
           i.e. <code>B‴/6</code>). So the error is exactly</p>
           <p><code>B(t) − Q(t) = (d/6)·(B₁³(t) − B₂³(t)) = (d/2)·t(1−t)(1−2t)</code></p>
           <p>a fixed direction times a scalar bump. The scalar's extrema sit
           at <code>t = ½ ± √3/6</code> with value <code>±√3/18</code>, so
           <code>max‖B−Q‖ = √3/36·‖d‖</code> — and because the sampled maximum
           of the figure is this same expression, the measured number above
           matches the bound to sampling precision; it is an equality, not an
           estimate. Splitting at <code>t = ½</code> leaves the third
           derivative alone but halves the parameter interval, so each half's
           <code>d</code> is 1/8 of the whole's; two levels give
           <code>d/64</code> per quarter and the total bound
           <code>√3·‖d‖/2304</code>. The four quadratics also interpolate the
           cubic exactly at <code>t = 0, ¼, ½, ¾, 1</code> — the split points
           (teal) sit <em>on</em> the cubic, because de Casteljau subdivision
           reproduces the curve's own points.</p>
           <p>One more honest caveat: the figure measures the
           <em>parameterwise</em> distance <code>‖B(t) − Q(t)‖</code>, which
           is an upper bound on the geometric distance between the curves —
           the quantity that could only be smaller.</p>`,
        ),
      );
    }

    // ---- Figure: contour orientation & winding ----------------------------
    {
      const host = root.querySelector('[data-fig="winding"]');
      const o = await ctx.inspect("o", "DejaVu Sans", 400);
      const em = fig.emSpace(o.bbox, { width: 300, pad: 30 });
      const node = fig.svg(em.width + 130, em.height);
      node.append(em.g);
      em.g.append(
        fig.svgEl("path", {
          d: fig.glyphPathD(o.curves, o.contours),
          fill: palette.blue, "fill-opacity": 0.25, "fill-rule": "nonzero",
          stroke: palette.blue, "stroke-width": 1.2,
        }),
      );

      // split curves per contour, find the outer one by |signed area|
      const contourCurves = [];
      let ci = 0;
      for (const n of o.contours) {
        contourCurves.push(o.curves.slice(ci, ci + n));
        ci += n;
      }
      const areas = contourCurves.map(contourArea);
      const outer = areas.reduce((a, i, k) => (Math.abs(areas[k]) > Math.abs(areas[a]) ? k : a), 0);

      // probe points, evaluated with the shader's own crossing rules
      const holeIdx = outer === 0 ? 1 : 0;
      const hole = contourCurves[holeIdx];
      const holeC = mul(hole.reduce((s, q) => add(s, q.p0), [0, 0]), 1 / hole.length);
      const holeMinX = Math.min(...hole.map((q) => Math.min(q.p0[0], q.p1[0], q.p2[0])));
      const ringPt = [(o.bbox[0] + holeMinX) / 2, holeC[1]];
      const outsidePt = [o.bbox[0] + 0.02, o.bbox[3] + 0.05];
      const probes = [
        { pt: outsidePt, name: "outside" },
        { pt: ringPt, name: "ink" },
        { pt: holeC, name: "hole" },
      ];

      const arrowsGroup = fig.svgEl("g", {});
      const labelsGroup = fig.svgEl("g", {});
      node.append(arrowsGroup, labelsGroup);
      const areaLine = fig.el("div", {
        style: `font-family:${MONO};font-size:12.5px;color:${palette.textDim};white-space:pre-line;`,
      });

      let flipped = false;
      const redraw = () => {
        const curves = flipped
          ? o.curves.map((q) => ({ p0: q.p2, p1: q.p1, p2: q.p0 }))
          : o.curves;
        arrowsGroup.replaceChildren();
        labelsGroup.replaceChildren();
        contourCurves.forEach((cc, k) => {
          const color = k === outer ? palette.green : palette.red;
          const step = Math.max(1, Math.floor(cc.length / 7));
          for (let i = 0; i < cc.length; i += step) {
            const q = cc[i];
            const pt = evalQuad(q, 0.5);
            let tan = sub(q.p2, q.p0);
            if (len(tan) < 1e-9) tan = sub(q.p1, q.p0);
            let u = mul(tan, 1 / len(tan));
            if (flipped) u = mul(u, -1);
            const n = [-u[1], u[0]];
            const s = 1 / em.scale;
            const tip = add(pt, mul(u, 7 * s));
            const b1 = add(add(pt, mul(u, -3 * s)), mul(n, 4 * s));
            const b2 = add(add(pt, mul(u, -3 * s)), mul(n, -4 * s));
            const [tx, ty] = em.toPx(tip);
            const [x1, y1] = em.toPx(b1);
            const [x2, y2] = em.toPx(b2);
            arrowsGroup.append(
              fig.svgEl("polygon", { points: `${tx},${ty} ${x1},${y1} ${x2},${y2}`, fill: color }),
            );
          }
        });
        for (const probe of probes) {
          const w = Math.round(windingAt(probe.pt, curves));
          const [px, py] = em.toPx(probe.pt);
          labelsGroup.append(
            fig.svgEl("circle", { cx: px, cy: py, r: 2.5, fill: palette.text }),
            fig.svgEl("text", {
              x: px + 7, y: py + 4, fill: palette.text,
              "font-size": 13, "font-family": MONO,
            }, `w = ${w}`),
          );
        }
        const dir = (a) => (a > 0 ? "counter-clockwise" : "clockwise");
        const aOuter = flipped ? -areas[outer] : areas[outer];
        const aHole = flipped ? -areas[holeIdx] : areas[holeIdx];
        areaLine.textContent =
          `outer contour: signed area ${fmt(aOuter, 4)} em² (${dir(aOuter)})` +
          `\nhole contour:  signed area ${fmt(aHole, 4)} em² (${dir(aHole)})`;
      };
      redraw();

      const flipBtn = fig.el(
        "button",
        {
          style:
            `background:${palette.bgDark};color:${palette.text};border:1px solid ${palette.border};` +
            `border-radius:6px;padding:5px 12px;font-size:13.5px;cursor:pointer;`,
          onclick: () => { flipped = !flipped; redraw(); },
        },
        "reverse both contours",
      );

      host.append(
        fig.figure(
          [node, areaLine, fig.controls(flipBtn)],
          "DejaVu Sans 'o': two contours, opposite orientations — arrows " +
            "follow real curve tangents at each segment's midpoint (green " +
            "outer, red hole), and the signed areas are integrated from the " +
            "stored quadratics. The winding numbers are computed by this " +
            "page with the same crossing rules the fragment shader runs: " +
            "0 outside, ±1 in the ink, and 0 again in the counter, where the " +
            "hole cancels the outer contour before any abs(). Press the " +
            "button to reverse every contour — TrueType order becomes CFF " +
            "order, every winding number flips sign, and |w|, the only thing " +
            "the shader keeps, does not move.",
        ),
        fig.details(
          "Closing rules and the degenerate-quadratic fine print",
          `<p>The flattener's contour bookkeeping, exactly as
           <code>curves.rs</code> implements it:</p>
           <ul>
             <li><code>close()</code> emits the closing edge as a
             <code>line_to</code> back to the contour's start — skipped when
             the pen already sits there, so a zero-length record is never
             stored.</li>
             <li><code>move_to</code> calls <code>close()</code> first: an
             unterminated contour is closed by the start of the next one, and
             <code>flatten_with</code> closes the last contour after the final
             command. Every curve belongs to exactly one contour (there is a
             debug assert counting them).</li>
             <li>A line's record has <code>a = p0 − 2·p1 + p2 = 0</code> by
             construction. In the fragment shader's
             <code>curve_winding</code>, records with <code>|a.y|</code> at or
             below a small threshold take a one-crossing linear branch
             (<code>t = c.y / 2b.y</code>) instead of the quadratic formula —
             a numerical guard, not a type tag: it is the same loop and the
             same record either way, chosen per curve by the data. After f16
             rounding a line's <code>a</code> is zero to within an f16 ulp of
             the midpoint, and both branches agree on where the crossing
             is.</li>
           </ul>
           <p>Orientation: TrueType's convention is outer contours clockwise
           in y-up font space, holes counter-clockwise; CFF inherits
           PostScript's, which is the reverse. The shader's coverage is
           <code>clamp(abs(winding), 0, 1)</code> — the comment in
           <code>shaders.wgsl</code> reads "abs() makes the result
           winding-orientation agnostic (TrueType and CFF wind opposite
           directions); holes still cancel to zero before the abs." The
           figure's button is that comment, made operational.</p>`,
        ),
      );
    }
  },
};
