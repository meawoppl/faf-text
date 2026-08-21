// Section 03: robustness of the winding test — why root-range checks sparkle,
// how sign classification fixes it, and why tiny sizes get three rays.
//
// Every figure below runs a JavaScript mirror of `curve_winding` from
// crates/faf-text/src/shaders.wgsl (same constants, same branch structure)
// over curves fetched from `ctx.inspect` — the renderer's own extraction.

// ---------------------------------------------------------------------------
// The shader math, mirrored. Constants verbatim from shaders.wgsl.
// ---------------------------------------------------------------------------

const T0_MASK = 0x454; // patterns {2, 4, 6, 10}  — where the downward root counts
const T1_MASK = 0x1510; // patterns {4, 8, 10, 12} — where the upward root counts

const clamp01 = (v) => Math.min(Math.max(v, 0), 1);

// code = (p0.y>0)*2 | (p1.y>0)*4 | (p2.y>0)*8 — bit 0 unused, so the code is
// even and doubles as the shift amount into the masks.
function classify(y0, y1, y2) {
  return (y0 > 0 ? 2 : 0) | (y1 > 0 ? 4 : 0) | (y2 > 0 ? 8 : 0);
}

// Mirrors curve_winding() in shaders.wgsl: signed, antialiased contribution of
// one quadratic to the winding sum along the +x ray from the origin. Points
// are pre-translated so the sample sits at the origin. Returns diagnostics the
// figures draw: which roots were counted, at what t and x.
function curveWinding(p0, p1, p2, invDiameter) {
  const code = classify(p0[1], p1[1], p2[1]);
  const out = { code, alpha: 0, roots: [], linear: false };
  if (code === 0 || code === 14) return out;
  const a = [p0[0] - 2 * p1[0] + p2[0], p0[1] - 2 * p1[1] + p2[1]];
  const b = [p0[0] - p1[0], p0[1] - p1[1]];
  const c = p0;
  if (Math.abs(a[1]) > 1e-4) {
    // max() guards grazing cases where fp noise pushes the radicand barely
    // negative; the two crossings then coincide and cancel exactly.
    const s = Math.sqrt(Math.max(b[1] * b[1] - a[1] * c[1], 0.0));
    const t0 = (b[1] - s) / a[1];
    const t1 = (b[1] + s) / a[1];
    if ((T0_MASK >> code) & 1) {
      const x = (a[0] * t0 - 2 * b[0]) * t0 + c[0];
      out.alpha += clamp01(x * invDiameter + 0.5);
      out.roots.push({ t: t0, x, sign: +1 });
    }
    if ((T1_MASK >> code) & 1) {
      const x = (a[0] * t1 - 2 * b[0]) * t1 + c[0];
      out.alpha -= clamp01(x * invDiameter + 0.5);
      out.roots.push({ t: t1, x, sign: -1 });
    }
  } else {
    // (Near-)linear in y: one crossing, direction from the endpoint signs.
    out.linear = true;
    let sign = 0;
    if (p0[1] > 0 && p2[1] <= 0) sign = 1;
    else if (p2[1] > 0 && p0[1] <= 0) sign = -1;
    if (sign !== 0) {
      const t = c[1] / (2 * b[1]);
      const x = (a[0] * t - 2 * b[0]) * t + c[0];
      out.alpha = sign * clamp01(x * invDiameter + 0.5);
      out.roots.push({ t, x, sign });
    }
  }
  return out;
}

// Mirrors the flat loop of vector_fs(): coverage at one sample from `taps`
// parallel rays per axis (offsets (tap+0.5)/taps - 0.5 pixels, perpendicular
// to each ray), averaged over 2*taps measurements. Banding is bypassed on
// purpose — it is pixel-identical to this loop by construction.
function coverageAt(curves, sx, sy, pxPerEm, taps) {
  const fw = 1 / pxPerEm;
  const windX = [0, 0, 0];
  const windY = [0, 0, 0];
  for (const q of curves) {
    const p0 = [q.p0[0] - sx, q.p0[1] - sy];
    const p1 = [q.p1[0] - sx, q.p1[1] - sy];
    const p2 = [q.p2[0] - sx, q.p2[1] - sy];
    for (let tap = 0; tap < taps; tap++) {
      const off = (tap + 0.5) / taps - 0.5;
      const oy = off * fw;
      windX[tap] += curveWinding(
        [p0[0], p0[1] - oy], [p1[0], p1[1] - oy], [p2[0], p2[1] - oy], pxPerEm).alpha;
      windY[tap] += curveWinding(
        [p0[1], p0[0] - oy], [p1[1], p1[0] - oy], [p2[1], p2[0] - oy], pxPerEm).alpha;
    }
  }
  let cov = 0;
  for (let tap = 0; tap < taps; tap++) {
    cov += clamp01(Math.abs(windX[tap])) + clamp01(Math.abs(windY[tap]));
  }
  return cov / (2 * taps);
}

// The horizontal-ray half of the measurement only — the quantity that
// stipples on thin horizontal features.
function coverageXOnly(curves, sx, sy, pxPerEm, taps) {
  const fw = 1 / pxPerEm;
  let cov = 0;
  for (let tap = 0; tap < taps; tap++) {
    const off = ((tap + 0.5) / taps - 0.5) * fw;
    let wind = 0;
    for (const q of curves) {
      wind += curveWinding(
        [q.p0[0] - sx, q.p0[1] - sy - off],
        [q.p1[0] - sx, q.p1[1] - sy - off],
        [q.p2[0] - sx, q.p2[1] - sy - off], pxPerEm).alpha;
    }
    cov += clamp01(Math.abs(wind));
  }
  return cov / taps;
}

// f32-rounded quadratic roots (Math.fround after every operation), for the
// failure-mode figure: this is the arithmetic the naive root-range check
// lives or dies by. ys are already translated so the ray is y = 0.
function rootsF32(y0, y1, y2) {
  const fr = Math.fround;
  const a = fr(fr(y0 - fr(2 * y1)) + y2);
  const b = fr(y0 - y1);
  const c = fr(y0);
  const rad = fr(fr(b * b) - fr(a * c));
  const s = fr(Math.sqrt(Math.max(rad, 0)));
  return { a, b, c, rad, s, t0: fr(fr(b - s) / a), t1: fr(fr(b + s) / a) };
}

// ---------------------------------------------------------------------------
// Small drawing helpers (pixel-space; each figure builds its own mapping).
// ---------------------------------------------------------------------------

const MONO = "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace";

// Aspect-preserving em → px map into the rect (px,py,pw,ph). y flips.
function mkMap(bbox, px, py, pw, ph) {
  const [x0, y0, x1, y1] = bbox;
  const s = Math.min(pw / (x1 - x0), ph / (y1 - y0));
  const cx = (x0 + x1) / 2;
  const cy = (y0 + y1) / 2;
  const m = ([ex, ey]) => [px + pw / 2 + (ex - cx) * s, py + ph / 2 - (ey - cy) * s];
  m.scale = s;
  m.inv = ([X, Y]) => [(X - px - pw / 2) / s + cx, -(Y - py - ph / 2) / s + cy];
  return m;
}

function pathQ(fig, map, curves, attrs) {
  let d = "";
  for (const q of curves) {
    const A = map(q.p0);
    const B = map(q.p1);
    const C = map(q.p2);
    d += `M ${A[0]} ${A[1]} Q ${B[0]} ${B[1]} ${C[0]} ${C[1]} `;
  }
  return fig.svgEl("path", { d: d.trim(), fill: "none", ...attrs });
}

function txt(fig, x, y, str, attrs = {}) {
  return fig.svgEl("text", {
    x, y,
    fill: attrs.fill,
    "font-size": attrs.size ?? 12,
    "font-family": attrs.font ?? MONO,
    "font-weight": attrs.weight,
    "text-anchor": attrs.anchor ?? "start",
    "vector-effect": null,
  }, str);
}

function rayLine(fig, palette, x0, x1, y, color) {
  const g = fig.svgEl("g", {});
  g.append(
    fig.svgEl("line", { x1: x0, y1: y, x2: x1 - 8, y2: y, stroke: color, "stroke-width": 1.4 }),
    fig.svgEl("polygon", {
      points: `${x1 - 8},${y - 4} ${x1},${y} ${x1 - 8},${y + 4}`,
      fill: color, stroke: "none",
    }),
  );
  return g;
}

function rootMarker(fig, palette, X, Y, sign) {
  const color = sign > 0 ? palette.green : palette.red;
  const g = fig.svgEl("g", {});
  g.append(
    fig.svgEl("circle", { cx: X, cy: Y, r: 7, fill: palette.bg, stroke: color, "stroke-width": 1.6 }),
    txt(fig, X, Y + 4, sign > 0 ? "+" : "−", { fill: color, size: 12, anchor: "middle", weight: 700 }),
  );
  return g;
}

// One control point, filled if strictly above the ray, hollow otherwise.
function ctrlDot(fig, palette, X, Y, above, isControl) {
  const color = isControl ? palette.purple : palette.teal;
  return fig.svgEl("circle", {
    cx: X, cy: Y, r: isControl ? 4.5 : 4,
    fill: above ? color : palette.bg,
    stroke: color, "stroke-width": 1.6,
  });
}

// ---------------------------------------------------------------------------
// Data mining: specimens for the 8-class gallery, a shared-endpoint pair for
// the failure figure, a real hairline bar for the 3-tap figure.
// ---------------------------------------------------------------------------

// For each of the 8 sign classes, find a real (curve, ray-height) specimen.
function findSpecimens(curveSets) {
  const best = new Map();
  for (const curves of curveSets) {
    for (const q of curves) {
      const ys = [q.p0[1], q.p1[1], q.p2[1]];
      const xs = [q.p0[0], q.p1[0], q.p2[0]];
      const area =
        (Math.max(...ys) - Math.min(...ys)) * (Math.max(...xs) - Math.min(...xs));
      const su = [...new Set(ys)].sort((u, v) => u - v);
      const span = su[su.length - 1] - su[0] || 0.05;
      const cand = [];
      for (let i = 0; i + 1 < su.length; i++) cand.push((su[i] + su[i + 1]) / 2);
      cand.push(su[0] - 0.35 * span, su[su.length - 1] + 0.35 * span);
      // The two-crossing window for codes 4/10 is the sliver between the
      // higher endpoint and the curve's vertex (fonts put extrema on-curve,
      // so control-point overshoot is ~1/64 em when it exists at all); aim
      // a candidate ray into it explicitly.
      const aY = ys[0] - 2 * ys[1] + ys[2];
      const bY = ys[0] - ys[1];
      if (Math.abs(aY) > 1e-9) {
        const tv = bY / aY;
        if (tv > 0 && tv < 1) {
          const vy = (aY * tv - 2 * bY) * tv + ys[0]; // vertex height
          cand.push((vy + Math.max(ys[0], ys[2])) / 2, (vy + Math.min(ys[0], ys[2])) / 2);
        }
      }
      for (const e of cand) {
        const code = classify(q.p0[1] - e, q.p1[1] - e, q.p2[1] - e);
        const dist = Math.min(...ys.map((y) => Math.abs(y - e)));
        let score = area * dist;
        if (code === 4 || code === 10) {
          // Prefer specimens whose two counted roots are real (the curve
          // actually pokes across), so the gallery shows both markers.
          const a = q.p0[1] - 2 * q.p1[1] + q.p2[1];
          const b = q.p0[1] - q.p1[1];
          const c = q.p0[1] - e;
          score *= b * b - a * c > 1e-6 ? 4 : 0.05;
        }
        const cur = best.get(code);
        if (!cur || cur.score < score) best.set(code, { q, e, score });
      }
    }
  }
  return best;
}

// All pairs of consecutive curves sharing an on-curve point, with the contour
// descending monotonically through it: A = (above, above, ON), B = (ON,
// not-above, below). The ray through the shared y grazes the endpoint by
// construction — no epsilon anywhere.
function findSharedPairs(insp) {
  const pairs = [];
  let start = 0;
  for (const n of insp.contours) {
    for (let i = start; i + 1 < start + n; i++) {
      const A = insp.curves[i];
      const B = insp.curves[i + 1];
      const e = A.p2[1];
      if (B.p0[1] !== e) continue; // endpoint sharing makes these the same float
      const aY = Math.abs(A.p0[1] - 2 * A.p1[1] + A.p2[1]);
      const bY = Math.abs(B.p0[1] - 2 * B.p1[1] + B.p2[1]);
      if (aY < 1e-3 || bY < 1e-3) continue; // want genuinely curved specimens
      if (A.p0[1] > e && A.p1[1] > e && B.p1[1] <= e && B.p2[1] < e) {
        const drop = Math.min(A.p0[1] - e, e - B.p2[1]);
        pairs.push({ A, B, e, drop });
      }
    }
    start += n;
  }
  return pairs;
}

// Inside test with a sharp ray (huge invDiameter): |winding| rounds to 1 or 0.
function insideAt(curves, x, y) {
  let wind = 0;
  for (const q of curves) {
    wind += curveWinding(
      [q.p0[0] - x, q.p0[1] - y],
      [q.p1[0] - x, q.p1[1] - y],
      [q.p2[0] - x, q.p2[1] - y], 1e6).alpha;
  }
  return Math.abs(wind) > 0.5;
}

// Vertical scan at column x: [lo, hi] intervals where the glyph is solid.
function inkIntervals(curves, x, yMin, yMax, steps = 900) {
  const spans = [];
  let openAt = null;
  for (let i = 0; i <= steps; i++) {
    const y = yMin + ((yMax - yMin) * i) / steps;
    const inside = insideAt(curves, x, y);
    if (inside && openAt === null) openAt = y;
    if (!inside && openAt !== null) {
      spans.push([openAt, y]);
      openAt = null;
    }
  }
  if (openAt !== null) spans.push([openAt, yMax]);
  return spans;
}

const bitsOf = (code) => `${(code >> 1) & 1}${(code >> 2) & 1}${(code >> 3) & 1}`;

// ---------------------------------------------------------------------------

export default {
  slug: "exact-at-the-edges",
  title: "Exact at the edges",

  html: `
    <p>Every pixel of every vector glyph on this page runs the same short
    program: translate the glyph's quadratics so the pixel sits at the origin,
    fire a ray along +x, and sum signed crossings. A quadratic B&eacute;zier's
    height above the ray is itself a quadratic,
    <code>y(t) = a.y&middot;t&sup2; &minus; 2&middot;b.y&middot;t + c.y</code>
    with <code>a = p0 &minus; 2p1 + p2</code>, <code>b = p0 &minus; p1</code>,
    <code>c = p0</code> &mdash; the exact names <code>curve_winding</code> in
    <code>shaders.wgsl</code> uses. So a crossing is a root of a quadratic you
    solved in school, and the obvious implementation solves it, keeps the
    roots with <code>t &isin; [0,&thinsp;1)</code>, and adds &plusmn;1 per
    crossing by the curve's direction. Nearly every first draft of an outline
    rasterizer does this. It sparkles.</p>

    <h3>The ambush</h3>
    <p>The reason is that the &ldquo;measure-zero&rdquo; event &mdash; a ray
    passing <em>exactly</em> through a curve endpoint &mdash; is not measure
    zero here. Font coordinates are exact binary fractions: TrueType stores
    integers over a power-of-two units-per-em (1/2048ths for DejaVu), and the
    scaler's fixed-point heritage makes 1/64ths ubiquitous. Pixel sample
    positions are exact binary fractions too. Scale one tidy grid onto the
    other and rays graze endpoints <em>constantly</em> &mdash; and because
    every sample in a pixel row shares its ray height, a graze never hits one
    pixel; it hits the whole row.</p>
    <p>At a shared endpoint &mdash; where one curve's <code>p2</code> is the
    next curve's <code>p0</code>, the same float &mdash; the true crossing
    sits at <code>t = 1</code> of curve A and <code>t = 0</code> of curve B.
    But <code>t</code> is computed as <code>(b.y &plusmn; &radic;) / a.y</code>,
    three rounded operations deep, so A's root lands at 1&thinsp;&plusmn;&thinsp;&epsilon;
    and B's at &plusmn;&epsilon;, and whether <code>[0,&thinsp;1)</code>
    catches each one is rounding noise &mdash; decided independently per
    curve, per row. Count the crossing twice or zero times and the winding
    number is off by one for every sample to the left of the graze: a run of
    pixels flips inside-out, because winding is a parity-like quantity, not a
    local one. A one-ulp mistake does not smudge a pixel; it inverts a
    scanline.</p>
    <p>faf-text hit this the way everyone hits it: FreeMono's
    <code>p</code> rendered as mirrored-looking garbage &mdash; coherent
    stripes of inside-for-outside &mdash; while other glyphs looked
    fine. What cracked it was re-implementing the shader's arithmetic in a
    Python script over the glyph's real outline and printing the GPU-vs-CPU
    divergence as ASCII art: suddenly the corruption was inspectable, and it
    pointed straight at rows whose rays grazed shared endpoints. The figures
    in this section use the same trick &mdash; the winding math runs again in
    this page's JavaScript, mirroring <code>curve_winding</code> line for
    line, against curves pulled from the renderer's own extraction.</p>
    <div data-fig="fail"></div>

    <h3>Classify, don't solve-then-check</h3>
    <p>The fix, from Eric Lengyel's Slug paper (<em>GPU-Centered Font
    Rendering Directly from Glyph Outlines</em>, JCGT 2017), is to stop asking
    the root finder a question it cannot answer robustly. Whether a crossing
    exists in the arc, and which of the two closed-form roots it is, is
    decided <em>before</em> any root is computed &mdash; from the sign pattern
    of the three control-point heights, under one uniform convention:
    <strong>strictly above the ray</strong> (<code>y &gt; 0</code>) versus
    not. A point exactly <em>on</em> the ray is &ldquo;not above,&rdquo;
    always, for every curve that touches it &mdash; so the two curves meeting
    at a grazed endpoint can never disagree about it.</p>
    <p>Three points, two states each: eight classes. The shader packs them as
    <code>code = (p0.y&gt;0)&middot;2 | (p1.y&gt;0)&middot;4 |
    (p2.y&gt;0)&middot;8</code> &mdash; bit 0 unused, so the even-valued code
    doubles as a shift amount &mdash; and looks the answer up in two bitmask
    constants baked into <code>shaders.wgsl</code>:</p>
    <pre><code>const T0_MASK: u32 = 0x454u;  // patterns {2, 4, 6, 10}
const T1_MASK: u32 = 0x1510u; // patterns {4, 8, 10, 12}</code></pre>
    <p><code>(T0_MASK &gt;&gt; code) &amp; 1</code> says whether the root
    <code>t0 = (b.y &minus; s) / a.y</code> contributes, and
    <code>T1_MASK</code> the same for <code>t1</code>. No branches, no
    interval tests, no epsilons. And the two roots have fixed meanings:
    <code>t0</code> is <em>always</em> the downward crossing (+1) and
    <code>t1</code> always the upward one (&minus;1), whichever way the
    parabola opens &mdash; the derivative at <code>t0</code> is
    <code>&minus;2s</code>, at <code>t1</code> it is <code>+2s</code>, and
    <code>s = &radic;&hellip; &ge; 0</code>. Codes 0 and 14 (all not-above,
    all above) return early: the curve lives in the convex hull of its control
    points, so it cannot cross at all.</p>
    <div data-fig="gallery"></div>
    <div data-fig="details-proof"></div>
    <div data-fig="drag"></div>

    <h3>Grazing, tangents, and the clamp</h3>
    <p>Walk the two dangerous cases through the tables. A contour descending
    monotonically through a shared endpoint: curve A is (above, above, on)
    &mdash; code 6, count <code>t0</code>, one downward crossing &mdash; and
    curve B is (on, not-above, below) &mdash; code 0, count nothing. Exactly
    one crossing, unconditionally, no matter what the root arithmetic rounds
    to. A contour that dips down, <em>touches</em> the ray at a shared
    endpoint, and rises again: A is (above, &hellip;, on) and counts a
    downward crossing at the endpoint; B is (on, &hellip;, above) and counts
    an upward one at the same point &mdash; <code>+c</code> then
    <code>&minus;c</code> at the same x, net zero. Tangency cancels instead
    of corrupting.</p>
    <p>Two guards finish the job. When a class promises two roots (codes 4
    and 10) but the curve only <em>grazes</em> the ray, floating point can
    push the radicand a hair negative; the shader computes
    <code>s = sqrt(max(b.y&middot;b.y &minus; a.y&middot;c.y, 0.0))</code>, so
    <code>t0</code> and <code>t1</code> coincide, the same x is added and
    subtracted, and the pair cancels <em>exactly</em> &mdash; the same floats,
    not merely close ones. And when <code>|a.y| &le; 1e-4</code> the quadratic
    in y degenerates; a separate branch takes the single root
    <code>t = c.y / (2&middot;b.y)</code> with its direction read straight off
    the endpoint signs. The x-evaluation still uses the full quadratic: a
    curve can be linear in y and curved in x.</p>
    <p>One more piece rides along for free: antialiasing. A counted crossing
    does not contribute a whole &plusmn;1 but
    <code>&plusmn;clamp(x &middot; inv_diameter + 0.5, 0, 1)</code>, where
    <code>x</code> is how far ahead of the sample the crossing sits and
    <code>inv_diameter</code> is pixels-per-em from
    <code>fwidth</code>. Crossings more than half a pixel ahead count fully,
    more than half a pixel behind not at all, and in between the contribution
    ramps linearly &mdash; a box filter along the ray, which is what the
    continuity in the figure above is showing.</p>

    <h3>Below 24 pixels per em</h3>
    <p>A ray measures crossing positions along itself to a fraction of a
    pixel &mdash; that is the clamp &mdash; but perpendicular to itself it is
    blind: it either pierces a feature or misses it. Horizontal rays march
    down the glyph one per pixel row, so a horizontal bar thinner than a pixel
    &mdash; the crossbar of an <code>e</code> at 10&thinsp;px &mdash; slips
    between two adjacent rows' rays or gets speared by one, at the mercy of
    subpixel phase. The vertical rays still measure its thickness correctly,
    but the final coverage averages both axes, so the bar's apparent weight
    oscillates row by row: stipple.</p>
    <p>The renderer's answer is in <code>vector_fs</code>: when
    <code>max(fwidth(em).x, fwidth(em).y) &gt; 1/24</code> &mdash; fewer than
    24 pixels per em &mdash; each axis fires <em>three</em> parallel rays,
    offset &minus;&#8531;, 0, +&#8531; of a pixel perpendicular to the ray, and
    coverage becomes the mean of six measurements instead of two. Ray spacing
    drops from one pixel to a third of a pixel, so a half-pixel bar is caught
    by at least one tap in every row and its ink is apportioned smoothly.
    Because the gate reads <code>fwidth</code>, it is per-fragment and
    perspective-correct: a 3D-tilted pane crosses onto the 3-tap path exactly
    where foreshortening compresses it, with no knowledge that the block was
    rotated.</p>
    <div data-fig="taps"></div>
    <div data-fig="live"></div>
  `,

  async mount(root, ctx) {
    const { fig, palette } = ctx;

    // ---- Fetch all real data up front ----------------------------------
    const [pMono, gSans, oSans, eSans, threeMan, tMan] = await Promise.all([
      ctx.inspect("p", "DejaVu Sans Mono", 400),
      ctx.inspect("g", "DejaVu Sans", 400),
      ctx.inspect("o", "DejaVu Sans", 400),
      ctx.inspect("e", "DejaVu Sans", 400),
      ctx.inspect("3", "Manrope", 400),
      ctx.inspect("t", "Manrope", 400),
    ]);
    for (const [name, insp] of [
      ["p", pMono], ["g", gSans], ["o", oSans], ["e", eSans], ["3", threeMan], ["t", tMan],
    ]) {
      if (!insp) throw new Error(`inspect('${name}') returned null — wasm bundle stale?`);
    }

    // =====================================================================
    // Figure 1 (fail): root-range check vs sign classes at a real shared
    // endpoint, with the actual f32 arithmetic on display.
    // =====================================================================
    {
      const host = root.querySelector('[data-fig="fail"]');
      const fr = Math.fround;
      // Identify a curve's downward root by derivative sign: y'(t) = 2at − 2b.
      const down = (r) => (2 * r.a * r.t0 - 2 * r.b <= 0 ? r.t0 : r.t1);
      const inHalfOpen = (t) => t >= 0 && t < 1;
      const inOpenClosed = (t) => t > 0 && t <= 1;
      const inClosed = (t) => t >= 0 && t <= 1;

      // Evaluate every eligible pair with the f32 arithmetic a root-range
      // implementation would run, and prefer one the [0,1) check miscounts on
      // this machine (any of them *can* miscount; which do is rounding luck).
      const candidates = [];
      for (const [insp, label] of [
        [pMono, "DejaVu Sans Mono 'p'"],
        [gSans, "DejaVu Sans 'g'"],
        [oSans, "DejaVu Sans 'o'"],
      ]) {
        for (const p of findSharedPairs(insp)) candidates.push({ ...p, label });
      }
      if (!candidates.length) throw new Error("no shared-endpoint specimen found");
      const conventions = [
        { label: "t ∈ [0, 1)", pred: inHalfOpen },
        { label: "t ∈ (0, 1]", pred: inOpenClosed },
        { label: "t ∈ [0, 1]", pred: inClosed },
      ];
      for (const cand of candidates) {
        cand.rA = rootsF32(
          fr(cand.A.p0[1] - cand.e), fr(cand.A.p1[1] - cand.e), fr(cand.A.p2[1] - cand.e));
        cand.rB = rootsF32(
          fr(cand.B.p0[1] - cand.e), fr(cand.B.p1[1] - cand.e), fr(cand.B.p2[1] - cand.e));
        cand.tA = down(cand.rA);
        cand.tB = down(cand.rB);
        // Which interval conventions miscount this pair, in f32, on this
        // machine? (Which do is rounding luck — often the roots land exactly
        // on the boundary and only the closed interval trips.)
        cand.fails = conventions.filter(
          (cv) => (cv.pred(cand.tA) ? 1 : 0) + (cv.pred(cand.tB) ? 1 : 0) !== 1);
      }
      candidates.sort((u, v) =>
        (v.fails.length ? 1 : 0) - (u.fails.length ? 1 : 0) || v.drop - u.drop);
      const pair = candidates[0];
      const { A, B, e, rA, rB, tA, tB, label } = pair;
      const count = (pred) => (pred(tA) ? 1 : 0) + (pred(tB) ? 1 : 0);
      const chosen = pair.fails[0] ?? conventions[0];
      const nChosen = count(chosen.pred);

      const codeA = classify(A.p0[1] - e, A.p1[1] - e, A.p2[1] - e);
      const codeB = classify(B.p0[1] - e, B.p1[1] - e, B.p2[1] - e);

      // Geometry panels (same drawing twice, different counted markers).
      const xs = [A.p0[0], A.p1[0], A.p2[0], B.p0[0], B.p1[0], B.p2[0]];
      const ys = [A.p0[1], A.p1[1], A.p2[1], B.p0[1], B.p1[1], B.p2[1]];
      const bb = [
        Math.min(...xs) - 0.03, Math.min(...ys) - 0.03,
        Math.max(...xs) + 0.03, Math.max(...ys) + 0.03,
      ];
      const W = 350;
      const H = 260;
      const panel = (title, titleColor, marks) => {
        const node = fig.svg(W, H);
        const map = mkMap(bb, 10, 34, W - 20, H - 44);
        node.append(txt(fig, W / 2, 20, title, { fill: titleColor, size: 13, anchor: "middle", weight: 600 }));
        const [, rayY] = map([bb[0], e]);
        node.append(rayLine(fig, palette, 10, W - 10, rayY, palette.green));
        node.append(txt(fig, 14, rayY - 6, "ray  y = shared.y exactly", { fill: palette.muted, size: 10.5 }));
        node.append(pathQ(fig, map, [A], { stroke: palette.blue, "stroke-width": 2.2 }));
        node.append(pathQ(fig, map, [B], { stroke: palette.purple, "stroke-width": 2.2 }));
        for (const [q, col] of [[A, palette.blue], [B, palette.purple]]) {
          for (const p of [q.p0, q.p1, q.p2]) {
            const [X, Y] = map(p);
            node.append(fig.svgEl("circle", { cx: X, cy: Y, r: 3, fill: col, stroke: "none" }));
          }
        }
        const [shX, shY] = map(A.p2);
        node.append(fig.svgEl("circle", {
          cx: shX, cy: shY, r: 8, fill: "none", stroke: palette.red, "stroke-width": 1.6,
          "stroke-dasharray": "3 2",
        }));
        node.append(txt(fig, shX + 12, shY - 10, "shared endpoint", { fill: palette.red, size: 10.5 }));
        const [aX] = map(A.p0);
        node.append(txt(fig, aX + 8, map(A.p0)[1], "A", { fill: palette.blue, size: 12, weight: 700 }));
        node.append(txt(fig, map(B.p2)[0] + 8, map(B.p2)[1], "B", { fill: palette.purple, size: 12, weight: 700 }));
        marks(node, map);
        return node;
      };

      const fmt = (t) => t.toPrecision(9);
      const naivePanel = panel(
        `root-range check: keep ${chosen.label}`, palette.red,
        (node, map) => {
          const marksAt = [];
          if (chosen.pred(tA)) marksAt.push([tA, "A", -30]);
          if (chosen.pred(tB)) marksAt.push([tB, "B", 30]);
          let dy = 0;
          for (const [t, who, side] of marksAt) {
            const [X, Y] = map(A.p2);
            node.append(rootMarker(fig, palette, X + (dy ? 18 : -18), Y, 1));
            node.append(txt(fig, X + side, Y + 24 + dy, `${who}: t = ${fmt(who === "A" ? tA : tB)}`, {
              fill: palette.red, size: 10.5, anchor: "middle",
            }));
            dy += 14;
          }
          node.append(txt(fig, W / 2, H - 8,
            `${nChosen} crossing${nChosen === 1 ? "" : "s"} counted here — truth is 1`,
            { fill: nChosen === 1 ? palette.green : palette.red, size: 12, anchor: "middle", weight: 600 }));
        });
      const classPanel = panel(
        "sign classes: no interval test at all", palette.green,
        (node, map) => {
          const [X, Y] = map(A.p2);
          node.append(rootMarker(fig, palette, X, Y - 16, 1));
          node.append(txt(fig, X, Y + 26, `A is (1,1,0) = code ${codeA} → t0 counts`, {
            fill: palette.green, size: 10.5, anchor: "middle",
          }));
          node.append(txt(fig, X, Y + 40, `B is (0,${(codeB >> 2) & 1},0) = code ${codeB} → nothing`, {
            fill: palette.green, size: 10.5, anchor: "middle",
          }));
          node.append(txt(fig, W / 2, H - 8, "1 crossing counted — always", {
            fill: palette.green, size: 12, anchor: "middle", weight: 600 }));
        });

      const row = fig.el("div", {
        style: "display:flex; flex-wrap:wrap; gap:12px; justify-content:center;",
      }, naivePanel, classPanel);

      const verdictRow = (label, n, note) => `
        <tr>
          <td style="padding:2px 12px; color:${palette.textDim}; font-family:${MONO}; font-size:13px;">${label}</td>
          <td style="padding:2px 12px; color:${n === 1 ? palette.green : palette.red}; font-family:${MONO}; font-size:13px;">
            ${n} crossing${n === 1 ? "" : "s"} ${n === 1 ? "✓" : "✗"}</td>
          <td style="padding:2px 12px; color:${palette.muted}; font-size:13px;">${note}</td>
        </tr>`;
      const numbers = fig.details(
        "The actual f32 numbers behind this figure",
        `<p style="color:${palette.textDim}">Two consecutive curves of ${label},
         ray height set to the
         shared endpoint's y <em>exactly</em> — the floats are equal, no epsilon was
         used to construct the graze. Roots computed with f32 rounding after every
         operation (<code>Math.fround</code>), the precision the GPU works in; drivers
         that fuse multiply-adds round differently, which is precisely the problem —
         the naive count hangs on rounding luck.</p>
         <pre style="font-size:12.5px"><code>curve A (descends onto the endpoint):  t_down = ${fmt(tA)}
curve B (leaves it downward):          t_down = ${fmt(tB)}
radicand(A) = ${rA.rad.toExponential(6)}   s(A) = ${rA.s.toPrecision(9)}</code></pre>
         <table style="border-collapse:collapse; margin: 8px 0;">
           ${verdictRow("t ∈ [0, 1)", count(inHalfOpen), "half-open, the usual first guess")}
           ${verdictRow("t ∈ (0, 1]", count(inOpenClosed), "moving the boundary just moves the coin flip")}
           ${verdictRow("t ∈ [0, 1]", count(inClosed), "closed interval counts the graze twice")}
           ${verdictRow("sign classes", 1, "code " + codeA + " counts t0 once; code " + codeB + " counts nothing")}
         </table>
         <p style="color:${palette.textDim}">Whichever interval convention you pick, some
         rounding outcome breaks it: the roots land exactly on the interval boundary or
         an ulp to either side, and which happens depends on the divide, the sqrt, and
         whether the radicand was fused. A convention that survives this pair on this
         machine fails another pair, or this pair on another driver. When it fails, the
         winding number for every sample left of the graze is off by one — the row
         renders inside-out.</p>`,
      );

      host.append(
        fig.figure([row],
          "The failure mode, on real data: two consecutive quadratics from " + label +
          " share an on-curve endpoint, and the ray height equals that endpoint's y exactly " +
          "(both are exact binary fractions, so this happens in practice constantly). The true " +
          "contour crosses once. Left: a root-range check counts whatever the rounded roots " +
          "happen to fall inside its interval — with " + chosen.label + ", " + nChosen +
          " crossing" + (nChosen === 1 ? "" : "s") + " on this machine's arithmetic. Right: " +
          "sign classification — A's pattern says one downward crossing, B's says none, and " +
          "no root is ever tested against an interval."),
        numbers,
      );
    }

    // =====================================================================
    // Figure 2 (gallery): the 8 sign classes, each with a real specimen.
    // =====================================================================
    {
      const host = root.querySelector('[data-fig="gallery"]');
      // The two-root classes need a control point strictly past both
      // endpoints in y. Type designers put extrema on-curve, so these are
      // rare: over the printable-ASCII repertoire of the embedded fonts the
      // only y-axis specimens are in Manrope — '3' overshoots upward by
      // 1/64 em (code 4), 't' downward at the base of its hook (code 10).
      const best = findSpecimens([
        pMono.curves, gSans.curves, oSans.curves, threeMan.curves, tMan.curves,
      ]);
      const order = [0, 14, 6, 2, 12, 8, 4, 10];
      const missing = order.filter((c) => !best.has(c));
      if (missing.length) throw new Error(`gallery classes missing: ${missing}`);

      const CW = 197;
      const CH = 205;
      const node = fig.svg(4 * CW, 2 * CH);
      const describe = {
        0: ["nothing above", "no crossing"],
        14: ["all above", "no crossing"],
        6: ["count t0", "one ↓ crossing (+1)"],
        2: ["count t0", "one ↓ crossing (+1)"],
        12: ["count t1", "one ↑ crossing (−1)"],
        8: ["count t1", "one ↑ crossing (−1)"],
        4: ["count t0 and t1", "↑ then ↓ — or nothing"],
        10: ["count t0 and t1", "↓ then ↑ — or nothing"],
      };
      order.forEach((code, idx) => {
        const { q, e } = best.get(code);
        const cx = (idx % 4) * CW;
        const cy = Math.floor(idx / 4) * CH;
        const ysAll = [q.p0[1], q.p1[1], q.p2[1], e];
        const xsAll = [q.p0[0], q.p1[0], q.p2[0]];
        const spanY = Math.max(...ysAll) - Math.min(...ysAll) || 0.05;
        const spanX = Math.max(...xsAll) - Math.min(...xsAll) || 0.05;
        const bb = [
          Math.min(...xsAll) - 0.18 * spanX, Math.min(...ysAll) - 0.15 * spanY,
          Math.max(...xsAll) + 0.18 * spanX, Math.max(...ysAll) + 0.15 * spanY,
        ];
        const map = mkMap(bb, cx + 12, cy + 46, CW - 24, CH - 62);
        node.append(fig.svgEl("rect", {
          x: cx + 3, y: cy + 3, width: CW - 6, height: CH - 6, rx: 8,
          fill: "none", stroke: palette.border, "stroke-width": 1,
        }));
        node.append(txt(fig, cx + CW / 2, cy + 22, `(${bitsOf(code).split("").join(" ")})  code ${code}`, {
          fill: palette.text, size: 13, anchor: "middle", weight: 700,
        }));
        node.append(txt(fig, cx + CW / 2, cy + 38, describe[code][0], {
          fill: code === 0 || code === 14 ? palette.muted : palette.orange, size: 11.5, anchor: "middle",
        }));
        const [, rayY] = map([bb[0], e]);
        node.append(rayLine(fig, palette, cx + 12, cx + CW - 12, rayY, palette.green));
        node.append(
          fig.svgEl("line", {
            x1: map(q.p0)[0], y1: map(q.p0)[1], x2: map(q.p1)[0], y2: map(q.p1)[1],
            stroke: palette.muted, "stroke-width": 1, "stroke-dasharray": "3 3",
          }),
          fig.svgEl("line", {
            x1: map(q.p1)[0], y1: map(q.p1)[1], x2: map(q.p2)[0], y2: map(q.p2)[1],
            stroke: palette.muted, "stroke-width": 1, "stroke-dasharray": "3 3",
          }),
        );
        node.append(pathQ(fig, map, [q], { stroke: palette.blue, "stroke-width": 2.2 }));
        node.append(
          ctrlDot(fig, palette, ...map(q.p0), q.p0[1] > e, false),
          ctrlDot(fig, palette, ...map(q.p1), q.p1[1] > e, true),
          ctrlDot(fig, palette, ...map(q.p2), q.p2[1] > e, false),
        );
        // Counted roots, from the mirrored shader math (y-translate only, so
        // root.x stays in absolute em coordinates).
        const w = curveWinding(
          [q.p0[0], q.p0[1] - e], [q.p1[0], q.p1[1] - e], [q.p2[0], q.p2[1] - e], 48);
        for (const r of w.roots) {
          if (r.t < -0.2 || r.t > 1.2) continue;
          node.append(rootMarker(fig, palette, map([r.x, e])[0], rayY, r.sign));
        }
        node.append(txt(fig, cx + CW / 2, cy + CH - 8, describe[code][1], {
          fill: palette.muted, size: 10.5, anchor: "middle",
        }));
      });

      host.append(fig.figure([node],
        "All eight sign classes, each drawn with a real quadratic from the embedded fonts " +
        "(DejaVu 'p', 'g', 'o'; Manrope '3' and 't' for the two-root classes) and a ray height " +
        "that produces the class. A point is filled when strictly above the ray, hollow when " +
        "on-or-below. Green + is a counted downward crossing (t0), red − an upward one (t1); " +
        "markers come from the mirrored shader math, not hand placement. The header shows " +
        "(p0 p1 p2) bits and the code they pack into; T0_MASK = 0x454 has a 1-bit at codes " +
        "{2, 4, 6, 10}, T1_MASK = 0x1510 at {4, 8, 10, 12}. Codes 4 and 10 need a control " +
        "point strictly past both endpoints — type designers put extrema on-curve, so over " +
        "printable ASCII in three embedded families exactly two such curves exist, both " +
        "overshooting by 1/64 em. Their promised root pair merges and cancels when the ray " +
        "misses the sliver between endpoint and vertex."));
    }

    // Proof details block.
    root.querySelector('[data-fig="details-proof"]').append(fig.details(
      "Why the sign pattern cannot lie",
      `<p>Write the height in Bernstein form: <code>y(t) = y0&middot;(1&minus;t)&sup2; +
       y1&middot;2t(1&minus;t) + y2&middot;t&sup2;</code>. The Bernstein coefficients of
       y(t) <em>are</em> the control-point heights, and the basis functions are
       non-negative on [0,&thinsp;1] and sum to one. All three coefficients positive
       &rArr; y &gt; 0 everywhere (code 14); none positive &rArr; y &le; 0 everywhere
       (code 0) &mdash; the curve may touch the ray, but touching is &ldquo;not
       above,&rdquo; so nothing is counted, consistently.</p>
       <p>For the rest, Descartes' rule in the Bernstein basis: the number of roots in
       (0,&thinsp;1) is at most the number of sign changes in (y0, y1, y2) and has the
       same parity. A quadratic has at most two roots, so this pins the count down
       exactly: one sign change (patterns 100, 110, 011, 001) means exactly one
       crossing; two sign changes (010, 101) mean zero or two.</p>
       <p>Which closed-form root is which: with <code>y(t) = a&middot;t&sup2; &minus;
       2b&middot;t + c</code> and <code>s = &radic;(b&sup2; &minus; ac)</code>, the roots
       are <code>t0 = (b&minus;s)/a</code> and <code>t1 = (b+s)/a</code>, and
       <code>y'(t0) = 2a&middot;t0 &minus; 2b = &minus;2s &le; 0</code> while
       <code>y'(t1) = +2s &ge; 0</code> &mdash; independent of the sign of
       <code>a</code>. So t0 is unconditionally the downward (+1) crossing and t1 the
       upward (&minus;1) one, and the two masks just record, per class, which of the
       two exists inside the arc: T0 gets the &ldquo;starts above&rdquo; classes {2, 6}
       plus the two-root classes {4, 10}; T1 gets the &ldquo;ends above&rdquo; classes
       {8, 12} plus {4, 10} again. Packed as bits at even positions:
       <code>1&lt;&lt;2 | 1&lt;&lt;4 | 1&lt;&lt;6 | 1&lt;&lt;10 = 0x454</code> and
       <code>1&lt;&lt;4 | 1&lt;&lt;8 | 1&lt;&lt;10 | 1&lt;&lt;12 = 0x1510</code>.</p>
       <p>One subtlety earned a comment in the source: the root-existence guarantee is
       only as good as the radicand's sign, and floating point can land it barely
       negative exactly when the curve grazes. Clamping it to zero (<code>max(&hellip;,
       0.0)</code>) forces the promised pair of roots to coincide; both evaluate to the
       same x, one is added, one subtracted, and the contribution is exactly zero.
       There is no threshold to tune: the classification decided the <em>count</em>,
       the clamp only keeps the <em>positions</em> finite.</p>`,
    ));

    // =====================================================================
    // Figure 3 (drag): interactive — drag control points across the ray.
    // =====================================================================
    {
      const host = root.querySelector('[data-fig="drag"]');
      // Base curve: a real bowl curve from DejaVu Sans 'g', translated so a
      // nearby sample point sits at the origin (the shader's frame).
      const base = gSans.curves[1]; // (0.40625,0.421875)-(0.375,0.484375)-(0.296875,0.484375)
      const sample = [0.22, 0.45];
      const pts = [base.p0, base.p1, base.p2].map((p) => [p[0] - sample[0], p[1] - sample[1]]);

      let ppe = 16;
      let dragIdx = 1;
      const XMIN = -0.34, XMAX = 0.34, YMIN = -0.26, YMAX = 0.26;
      const W = 480;
      const H = Math.round((W * (YMAX - YMIN)) / (XMAX - XMIN));
      const map = mkMap([XMIN, YMIN, XMAX, YMAX], 0, 0, W, H);

      const node = fig.svg(W, H, { style: "touch-action:none; cursor:grab; max-width:100%; height:auto;" });
      const staticG = fig.svgEl("g", {});
      const dynG = fig.svgEl("g", {});
      node.append(staticG, dynG);

      // Static: ray + sample.
      const [, rayY] = map([0, 0]);
      const [origX] = map([0, 0]);
      staticG.append(rayLine(fig, palette, 0, W, rayY, palette.green));
      staticG.append(fig.svgEl("circle", { cx: origX, cy: rayY, r: 3.5, fill: palette.text, stroke: "none" }));
      staticG.append(txt(fig, origX + 6, rayY + 16, "sample", { fill: palette.muted, size: 11 }));
      staticG.append(txt(fig, W - 8, rayY - 8, "+x ray", { fill: palette.green, size: 11, anchor: "end" }));

      const readout = fig.el("div", {
        style: `font-family:${MONO}; font-size:13px; line-height:1.7; color:${palette.textDim};` +
          "white-space:pre; overflow-x:auto; margin-top:10px;",
      });

      const plotW = 480, plotH = 110;
      const plot = fig.svg(plotW, plotH);

      const fmtS = (v, d = 4) => (v >= 0 ? "+" : "") + v.toFixed(d);

      const render = () => {
        dynG.replaceChildren();
        const [P0, P1, P2] = pts;
        const w = curveWinding(P0, P1, P2, ppe);

        // AA window: |x| < half a pixel around the sample, along the ray.
        const halfPx = 0.5 / ppe;
        const [wx0] = map([-halfPx, 0]);
        const [wx1] = map([halfPx, 0]);
        dynG.append(fig.svgEl("rect", {
          x: wx0, y: 0, width: wx1 - wx0, height: H,
          fill: palette.muted, "fill-opacity": 0.14, stroke: "none",
        }));

        dynG.append(
          fig.svgEl("line", {
            x1: map(P0)[0], y1: map(P0)[1], x2: map(P1)[0], y2: map(P1)[1],
            stroke: palette.muted, "stroke-width": 1, "stroke-dasharray": "3 3",
          }),
          fig.svgEl("line", {
            x1: map(P1)[0], y1: map(P1)[1], x2: map(P2)[0], y2: map(P2)[1],
            stroke: palette.muted, "stroke-width": 1, "stroke-dasharray": "3 3",
          }),
        );
        dynG.append(pathQ(fig, map,
          [{ p0: P0, p1: P1, p2: P2 }], { stroke: palette.blue, "stroke-width": 2.4 }));
        [P0, P1, P2].forEach((p, i) => {
          const [X, Y] = map(p);
          dynG.append(ctrlDot(fig, palette, X, Y, p[1] > 0, i === 1));
          dynG.append(txt(fig, X + 9, Y - 8, ["p0", "p1", "p2"][i], {
            fill: i === 1 ? palette.purple : palette.teal, size: 11, weight: 600,
          }));
        });
        for (const r of w.roots) {
          if (r.t < -0.5 || r.t > 1.5) continue;
          dynG.append(rootMarker(fig, palette, map([r.x, 0])[0], rayY, r.sign));
        }

        // Readout.
        const bits = [P0, P1, P2].map((p) => (p[1] > 0 ? 1 : 0));
        const t0hit = (T0_MASK >> w.code) & 1;
        const t1hit = (T1_MASK >> w.code) & 1;
        let lines =
          `bits (p0,p1,p2) = (${bits.join(",")})   code = ${w.code}\n` +
          `(0x454  >> ${String(w.code).padEnd(2)}) & 1 = ${t0hit}   ` +
          `(0x1510 >> ${String(w.code).padEnd(2)}) & 1 = ${t1hit}`;
        if (w.linear) {
          lines += `\nlinear branch (|a.y| <= 1e-4): t = c.y/(2 b.y)`;
        }
        for (const r of w.roots) {
          const px = r.x * ppe;
          lines += `\n${r.sign > 0 ? "t0" : "t1"} = ${r.t.toFixed(4).padStart(8)}  ` +
            `x = ${fmtS(r.x)} em = ${fmtS(px, 2)} px  ` +
            `→ ${r.sign > 0 ? "+" : "−"}clamp(${fmtS(px, 2)} + 0.5) = ` +
            `${fmtS(r.sign * clamp01(px + 0.5), 3)}`;
        }
        if (!w.roots.length) lines += `\nno roots counted`;
        lines += `\nα = ${fmtS(w.alpha, 4)}     ` +
          `(coverage uses clamp(|Σα|, 0, 1) over all curves)`;
        readout.textContent = lines;

        // Sweep plot: alpha as the dragged point's y sweeps the window.
        plot.replaceChildren();
        const N = 160;
        const sweep = [];
        const codes = [];
        for (let i = 0; i <= N; i++) {
          const y = YMIN + ((YMAX - YMIN) * i) / N;
          const q = pts.map((p, j) => (j === dragIdx ? [p[0], y] : p));
          const ww = curveWinding(q[0], q[1], q[2], ppe);
          sweep.push(ww.alpha);
          codes.push(ww.code);
        }
        const sx = (i) => (i / N) * plotW;
        const sy = (v) => plotH / 2 - v * (plotH / 2 - 12);
        plot.append(fig.svgEl("line", {
          x1: 0, y1: sy(0), x2: plotW, y2: sy(0),
          stroke: palette.border, "stroke-width": 1,
        }));
        for (const v of [1, 0, -1]) {
          plot.append(txt(fig, plotW - 4, sy(v) + (v === 1 ? 10 : v === -1 ? -4 : -4),
            v > 0 ? "+1" : v < 0 ? "−1" : "0",
            { fill: palette.muted, size: 10, anchor: "end" }));
        }
        for (let i = 1; i <= N; i++) {
          if (codes[i] !== codes[i - 1]) {
            plot.append(fig.svgEl("line", {
              x1: sx(i - 0.5), y1: 6, x2: sx(i - 0.5), y2: plotH - 6,
              stroke: palette.red, "stroke-width": 1, "stroke-dasharray": "3 3",
              "stroke-opacity": 0.7,
            }));
          }
        }
        let d = "";
        sweep.forEach((v, i) => { d += `${i ? "L" : "M"} ${sx(i)} ${sy(v)} `; });
        plot.append(fig.svgEl("path", { d: d.trim(), fill: "none", stroke: palette.teal, "stroke-width": 1.8 }));
        const curY = pts[dragIdx][1];
        const curI = ((curY - YMIN) / (YMAX - YMIN)) * N;
        const cur = curveWinding(pts[0], pts[1], pts[2], ppe);
        plot.append(fig.svgEl("circle", {
          cx: sx(curI), cy: sy(cur.alpha), r: 4.5, fill: palette.orange, stroke: "none",
        }));
        plot.append(txt(fig, 4, 14, `α vs ${["p0", "p1", "p2"][dragIdx]}.y   (dashed red = class code changes)`, {
          fill: palette.muted, size: 11,
        }));
      };

      // Dragging.
      const toLocal = (ev) => {
        const r = node.getBoundingClientRect();
        return map.inv([
          ((ev.clientX - r.left) * W) / r.width,
          ((ev.clientY - r.top) * H) / r.height,
        ]);
      };
      let dragging = -1;
      node.addEventListener("pointerdown", (ev) => {
        const [ex, ey] = toLocal(ev);
        let bestI = -1;
        let bestD = 20 / map.scale; // 20 px hit radius, in em
        pts.forEach((p, i) => {
          const dd = Math.hypot(p[0] - ex, p[1] - ey);
          if (dd < bestD) { bestD = dd; bestI = i; }
        });
        if (bestI >= 0) {
          dragging = bestI;
          dragIdx = bestI;
          node.setPointerCapture(ev.pointerId);
          ev.preventDefault();
          render();
        }
      });
      node.addEventListener("pointermove", (ev) => {
        if (dragging < 0) return;
        const [ex, ey] = toLocal(ev);
        pts[dragging] = [
          Math.min(Math.max(ex, XMIN), XMAX),
          Math.min(Math.max(ey, YMIN), YMAX),
        ];
        render();
      });
      node.addEventListener("pointerup", () => { dragging = -1; });

      const ppeSlider = fig.slider({
        label: "pixels per em",
        min: 4, max: 64, step: 1, value: ppe,
        format: (v) => `${v} px`,
        oninput: (v) => { ppe = v; render(); },
      });

      render();
      host.append(fig.figure(
        [node, fig.controls(ppeSlider.root), readout, plot],
        "Drag any control point of this curve (a real quadratic from DejaVu Sans 'g', in the " +
        "shader's frame: sample at the dot, ray along +x, shaded band = the half-pixel AA " +
        "window). The class code jumps as a point crosses the ray — filled turns hollow — but " +
        "the contribution α never jumps: the teal trace below sweeps the dragged point's " +
        "y through the whole window and stays continuous straight through every dashed " +
        "code-change line. Watching α while a two-root class (code 4 or 10) collapses " +
        "shows the radicand clamp cancel the pair exactly."));
    }

    // =====================================================================
    // Figure 4 (taps): 1 ray vs 3 rays on a real sub-pixel crossbar.
    // =====================================================================
    {
      const host = root.querySelector('[data-fig="taps"]');
      const curves = eSans.curves;
      const bb = eSans.bbox;
      const holder = fig.el("div", {});
      let ppe = 10;

      const build = () => {
        holder.replaceChildren();
        const x0 = Math.floor(bb[0] * ppe) / ppe;
        const y0 = Math.floor(bb[1] * ppe) / ppe;
        const cols = Math.ceil((bb[2] - x0) * ppe);
        const rows = Math.ceil((bb[3] - y0) * ppe);
        const yTop = y0 + rows / ppe;
        const cell = Math.min(30, Math.floor(210 / Math.max(cols, rows)) + 12);

        // Coverage grids, both modes, plus the largest per-pixel difference.
        const cov = { 1: [], 3: [] };
        let maxDiff = 0;
        for (let r = 0; r < rows; r++) {
          const row1 = [], row3 = [];
          const sy = yTop - (r + 0.5) / ppe;
          for (let cIdx = 0; cIdx < cols; cIdx++) {
            const sx = x0 + (cIdx + 0.5) / ppe;
            const c1 = coverageAt(curves, sx, sy, ppe, 1);
            const c3 = coverageAt(curves, sx, sy, ppe, 3);
            row1.push(c1);
            row3.push(c3);
            maxDiff = Math.max(maxDiff, Math.abs(c1 - c3));
          }
          cov[1].push(row1);
          cov[3].push(row3);
        }

        // The crossbar: middle ink interval on the center column.
        const midX = x0 + (Math.floor(cols / 2) + 0.5) / ppe;
        const spans = inkIntervals(curves, midX, y0 - 0.02, yTop + 0.02);
        const bar = spans.length ? spans[Math.floor(spans.length / 2)] : null;
        const barPx = bar ? (bar[1] - bar[0]) * ppe : 0;

        const panelW = cols * cell + 20;
        const panelH = rows * cell + 40;
        const heat = (taps) => {
          const node = fig.svg(panelW, panelH);
          node.append(txt(fig, panelW / 2, 16,
            taps === 1 ? "1 ray per axis" : "3 rays per axis", {
              fill: taps === 1 ? palette.red : palette.green, size: 13, anchor: "middle", weight: 600,
            }));
          const gx = (ex) => 10 + (ex - x0) * ppe * cell;
          const gy = (ey) => 28 + (yTop - ey) * ppe * cell;
          for (let r = 0; r < rows; r++) {
            for (let cIdx = 0; cIdx < cols; cIdx++) {
              node.append(fig.svgEl("rect", {
                x: 10 + cIdx * cell, y: 28 + r * cell, width: cell - 1, height: cell - 1,
                fill: palette.text, "fill-opacity": (cov[taps][r][cIdx]).toFixed(3),
                stroke: "none",
              }));
            }
          }
          // Ray heights, then the true outline on top.
          for (let r = 0; r < rows; r++) {
            for (let tap = 0; tap < taps; tap++) {
              const off = (tap + 0.5) / taps - 0.5;
              const y = gy(yTop - (r + 0.5 + off) / ppe);
              node.append(fig.svgEl("line", {
                x1: 10, y1: y, x2: 10 + cols * cell, y2: y,
                stroke: taps === 1 ? palette.green : palette.teal,
                "stroke-width": 0.7, "stroke-opacity": 0.4,
              }));
            }
          }
          const mapPanel = ([ex, ey]) => [gx(ex), gy(ey)];
          node.append(pathQ(fig, mapPanel, curves, {
            stroke: palette.blue, "stroke-width": 1.1, "stroke-opacity": 0.9,
          }));
          return node;
        };

        // Column strip: the crossbar between rays, row by row.
        const strip = (() => {
          if (!bar) return fig.el("div", {});
          const barMid = (bar[0] + bar[1]) / 2;
          const rMid = Math.floor((yTop - barMid) * ppe);
          const r0 = Math.max(0, rMid - 2);
          const r1 = Math.min(rows - 1, rMid + 2);
          const n = r1 - r0 + 1;
          const rh = 34;
          const sw = 320;
          const node = fig.svg(sw, n * rh + 46);
          const halfW = 110;
          // em -> px within the strip: the top of row r0 is the strip's top.
          const yOfR = (ey) => 34 + ((yTop - r0 / ppe) - ey) * ppe * rh;
          const half = (xoff, taps, label, rayColor) => {
            node.append(txt(fig, xoff + halfW / 2, 14, label, {
              fill: taps === 1 ? palette.red : palette.green, size: 12, anchor: "middle", weight: 600,
            }));
            // pixel rows
            for (let r = r0; r <= r1; r++) {
              node.append(fig.svgEl("rect", {
                x: xoff, y: 34 + (r - r0) * rh, width: halfW, height: rh,
                fill: "none", stroke: palette.border, "stroke-width": 1,
              }));
            }
            // the real bar
            const bTop = yOfR(bar[1]);
            const bBot = yOfR(bar[0]);
            node.append(fig.svgEl("rect", {
              x: xoff, y: bTop, width: halfW, height: Math.max(bBot - bTop, 1.5),
              fill: palette.blue, "fill-opacity": 0.55, stroke: "none",
            }));
            // rays + the x-axis coverage measured through them
            for (let r = r0; r <= r1; r++) {
              const syC = yTop - (r + 0.5) / ppe;
              for (let tap = 0; tap < taps; tap++) {
                const off = (tap + 0.5) / taps - 0.5;
                const y = yOfR(syC - off / ppe);
                node.append(fig.svgEl("line", {
                  x1: xoff + 4, y1: y, x2: xoff + halfW - 4, y2: y,
                  stroke: rayColor, "stroke-width": 1, "stroke-opacity": 0.8,
                }));
              }
              const cx1 = coverageXOnly(curves, midX, syC, ppe, taps);
              node.append(txt(fig, xoff + halfW + 6, 34 + (r - r0) * rh + rh / 2 + 4,
                cx1.toFixed(2), { fill: palette.orange, size: 12 }));
            }
          };
          half(8, 1, "1 tap", palette.green);
          half(174, 3, "3 taps", palette.teal);
          node.append(txt(fig, sw / 2, n * rh + 42,
            `crossbar: ${barPx.toFixed(2)} px thick at ${ppe} px/em`, {
              fill: palette.muted, size: 11.5, anchor: "middle",
            }));
          return node;
        })();

        holder.append(fig.el("div", {
          style: "display:flex; flex-wrap:wrap; gap:16px; justify-content:center; align-items:flex-start;",
        }, heat(1), heat(3), strip));
        holder.dataset.maxdiff = maxDiff.toFixed(2);
        holder.dataset.barpx = barPx.toFixed(2);
      };

      const sizeSlider = fig.slider({
        label: "rendered size",
        min: 7, max: 22, step: 1, value: ppe,
        format: (v) => `${v} px/em`,
        oninput: (v) => { ppe = v; build(); },
      });
      build();

      host.append(fig.figure(
        [holder, fig.controls(sizeSlider.root)],
        "DejaVu Sans 'e' rendered by this page's mirror of the fragment shader's flat loop, " +
        "pixel by pixel. Left: one ray per axis — the sub-pixel crossbar is speared by some " +
        "rows' rays and missed by others, so its weight stipples. Middle: the 3-tap path the " +
        "renderer takes below 24 px/em — rays every third of a pixel catch the bar in every " +
        "row and apportion its ink smoothly. Right: the crossbar (blue band, real extracted " +
        "geometry) against each row's rays, with the horizontal-ray coverage measured through " +
        "them; at 1 tap the bar's rows read 0.00 or 1.00 by subpixel luck, at 3 taps they " +
        "grade. Drag the size down to make the bar thinner."));
    }

    // =====================================================================
    // Figure 5 (live): the real renderer, tiny vs large. Attached last so
    // headless one-shot screenshots still show the data figures above.
    // =====================================================================
    {
      const host = root.querySelector('[data-fig="live"]');
      const smallCanvas = document.createElement("canvas");
      const largeCanvas = document.createElement("canvas");
      const wrap = (label, canvas) => fig.el("div", { style: "flex:1 1 260px; min-width:240px;" },
        fig.el("div", {
          style: `color:${palette.muted}; font-size:13px; margin-bottom:6px; font-family:${MONO};`,
        }, label),
        canvas);
      host.append(fig.figure(
        [fig.el("div", { style: "display:flex; flex-wrap:wrap; gap:16px;" },
          wrap("10 px — 3-tap path", smallCanvas),
          wrap("40 px — single-ray path", largeCanvas))],
        "The real renderer, same text, same curve texture, two sizes. At 10 px every fragment " +
        "is under the 24 px/em gate (at 1× and 2× display scales) and fires six rays; " +
        "at 40 px, two. The switch is per fragment, from fwidth, so a glyph straddling the " +
        "gate — or a 3D-tilted pane compressing toward its horizon — changes paths exactly " +
        "where the pixel density demands it."));

      const text = "Sphinx of black quartz — thin bars in e, s, z.";
      const small = await ctx.attachDemo(smallCanvas, { height: 70, fontSize: 10, text });
      const large = await ctx.attachDemo(largeCanvas, { height: 230, fontSize: 40, text });
      ctx.animate(smallCanvas, small);
      ctx.animate(largeCanvas, large);
    }
  },
};
