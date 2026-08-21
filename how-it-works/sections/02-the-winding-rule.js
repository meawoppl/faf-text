// Section 02 — the winding rule: how vector_fs decides inside/outside per
// pixel and antialiases analytically. Every figure below runs a line-for-line
// JavaScript port of `curve_winding` and the single-tap loop of `vector_fs`
// (crates/faf-text/src/shaders.wgsl) over geometry pulled from
// `ctx.inspect(...)` — the renderer's own extraction.

// Mirrors of shaders.wgsl's crossing-count masks, verbatim:
//   const T0_MASK: u32 = 0x454u;  // patterns {2, 4, 6, 10}
//   const T1_MASK: u32 = 0x1510u; // patterns {4, 8, 10, 12}
const T0_MASK = 0x454;
const T1_MASK = 0x1510;

const clamp01 = (v) => Math.min(1, Math.max(0, v));

// JS port of `curve_winding` (shaders.wgsl): signed, antialiased contribution
// of one quadratic to the winding sum along the +x ray from the origin.
// Points arrive pre-translated so the sample is the origin (and pre-swapped
// for the vertical ray, exactly like `p0.yx` in vector_fs). `out`, when
// given, collects every counted crossing for the figures.
function curveWinding(x0, y0, x1, y1, x2, y2, inv, out) {
  const code = (y0 > 0 ? 2 : 0) | (y1 > 0 ? 4 : 0) | (y2 > 0 ? 8 : 0);
  if (code === 0 || code === 14) return 0;
  const ay = y0 - 2 * y1 + y2;
  const by = y0 - y1;
  const cy = y0;
  const ax = x0 - 2 * x1 + x2;
  const bx = x0 - x1;
  const cx = x0;
  let alpha = 0;
  if (Math.abs(ay) > 1e-4) {
    const s = Math.sqrt(Math.max(by * by - ay * cy, 0));
    const t0 = (by - s) / ay;
    const t1 = (by + s) / ay;
    if ((T0_MASK >> code) & 1) {
      const x = (ax * t0 - 2 * bx) * t0 + cx;
      const c = clamp01(x * inv + 0.5);
      alpha += c;
      if (out) out.push({ t: t0, x, sign: 1, contrib: c });
    }
    if ((T1_MASK >> code) & 1) {
      const x = (ax * t1 - 2 * bx) * t1 + cx;
      const c = clamp01(x * inv + 0.5);
      alpha -= c;
      if (out) out.push({ t: t1, x, sign: -1, contrib: -c });
    }
  } else {
    // (Near-)linear in y: one crossing, direction from the endpoint signs.
    let sign = 0;
    if (y0 > 0 && y2 <= 0) sign = 1;
    else if (y2 > 0 && y0 <= 0) sign = -1;
    if (sign !== 0) {
      const t = cy / (2 * by);
      const x = (ax * t - 2 * bx) * t + cx;
      const c = clamp01(x * inv + 0.5);
      alpha = sign * c;
      if (out) out.push({ t, x, sign, contrib: sign * c });
    }
  }
  return alpha;
}

// The flat (unbanded) loop of `vector_fs`, one tap per axis: horizontal ray
// as-is, vertical ray with the coordinates swapped (`p0.yx` in the WGSL).
function windingX(curves, ex, ey, inv, out) {
  let w = 0;
  for (const { p0, p1, p2 } of curves) {
    w += curveWinding(p0[0] - ex, p0[1] - ey, p1[0] - ex, p1[1] - ey, p2[0] - ex, p2[1] - ey, inv, out);
  }
  return w;
}

function windingY(curves, ex, ey, inv, out) {
  let w = 0;
  for (const { p0, p1, p2 } of curves) {
    w += curveWinding(p0[1] - ey, p0[0] - ex, p1[1] - ey, p1[0] - ex, p2[1] - ey, p2[0] - ex, inv, out);
  }
  return w;
}

// vector_fs's tail on the single-tap path:
//   coverage = (clamp(abs(wind_x), 0, 1) + clamp(abs(wind_y), 0, 1)) / 2
function coverageAt(curves, ex, ey, inv) {
  return (clamp01(Math.abs(windingX(curves, ex, ey, inv))) + clamp01(Math.abs(windingY(curves, ex, ey, inv)))) / 2;
}

const MONO = "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace";
const fmt = (v, d = 3) => (v < 0 ? "−" : "+") + Math.abs(v).toFixed(d);

// ---------------------------------------------------------------------------
// Figure a: the hero — draggable sample point over a real glyph, both rays,
// every crossing marked and priced.
function buildHero(host, fig, palette, insp) {
  const [minX, minY, maxX, maxY] = insp.bbox;
  const draw = [minX - 0.07, minY - 0.07, maxX + 0.17, maxY + 0.11];
  const em = fig.emSpace(draw, { width: 430, pad: 12 });
  const node = fig.svg(em.width, em.height);
  node.style.cursor = "crosshair";
  node.style.touchAction = "none";
  node.append(em.g);

  em.g.append(
    fig.svgEl("path", {
      d: fig.glyphPathD(insp.curves, insp.contours),
      fill: palette.blue,
      "fill-opacity": 0.28,
      "fill-rule": "nonzero",
      stroke: palette.blue,
      "stroke-width": 1.2,
    }),
  );

  const rayG = fig.svgEl("g", {});
  em.g.append(rayG);
  const labelG = fig.svgEl("g", {});
  node.append(labelG);

  const readout = fig.el("div", {
    style: `font-family:${MONO};font-size:13px;line-height:1.7;color:${palette.textDim};` +
      "margin-top:10px;white-space:pre-wrap;",
  });

  const origin = em.toPx([0, 0]);
  const pxToEm = (px, py) => [(px - origin[0]) / em.scale, (origin[1] - py) / em.scale];

  // Default sample: inside the counter, a fraction of a pixel from the
  // bowl's inner edge, where the AA window has something to price
  // fractionally.
  const state = { ex: insp.curves[0].p0[0] - 0.006, ey: (insp.curves[0].p0[1] + insp.curves[0].p1[1]) / 2, ppem: 48 };

  const marker = (gx, gy, sign, contrib, dx, dy) => {
    const [px, py] = em.toPx([gx, gy]);
    const color = sign > 0 ? palette.green : palette.red;
    const dim = Math.abs(contrib) < 0.005;
    labelG.append(
      fig.svgEl("circle", { cx: px, cy: py, r: 4, fill: color, "fill-opacity": dim ? 0.35 : 0.95, stroke: "none" }),
      fig.svgEl(
        "text",
        {
          x: px + dx, y: py + dy, fill: color, "fill-opacity": dim ? 0.5 : 1,
          "font-size": 10.5, "font-family": MONO,
        },
        fmt(contrib, 2),
      ),
    );
  };

  const update = () => {
    rayG.replaceChildren();
    labelG.replaceChildren();
    const { ex, ey, ppem } = state;
    const w = 1 / ppem; // AA window: one pixel, in em

    // Rays: faint behind the sample, solid ahead (only crossings ahead can
    // contribute — that is exactly what the clamp encodes).
    const ray = (x1, y1, x2, y2, faint) =>
      rayG.append(
        fig.svgEl("line", {
          x1, y1, x2, y2,
          stroke: palette.textDim,
          "stroke-width": faint ? 0.8 : 1.3,
          "stroke-opacity": faint ? 0.3 : 0.85,
          "stroke-dasharray": faint ? "3 4" : null,
        }),
      );
    ray(draw[0], ey, ex, ey, true);
    ray(ex, ey, draw[2], ey, false);
    ray(ex, draw[1], ex, ey, true);
    ray(ex, ey, ex, draw[3], false);

    // The one-pixel AA window on each ray, centered on the sample.
    for (const [xa, ya, xb, yb] of [
      [ex - w / 2, ey, ex + w / 2, ey],
      [ex, ey - w / 2, ex, ey + w / 2],
    ]) {
      rayG.append(
        fig.svgEl("line", {
          x1: xa, y1: ya, x2: xb, y2: yb,
          stroke: palette.orange, "stroke-width": 7, "stroke-opacity": 0.4,
        }),
      );
    }

    // Arrowheads + axis labels, in pixel space.
    const arrow = (tip, rot, text, dx, dy) => {
      const [px, py] = em.toPx(tip);
      labelG.append(
        fig.svgEl("polygon", {
          points: "0,-4 8,0 0,4",
          transform: `translate(${px} ${py}) rotate(${rot})`,
          fill: palette.textDim, "fill-opacity": 0.85,
        }),
        fig.svgEl("text", { x: px + dx, y: py + dy, fill: palette.muted, "font-size": 11, "font-family": MONO }, text),
      );
    };
    arrow([draw[2], ey], 0, "+x ray", -44, -8);
    arrow([ex, draw[3]], -90, "+y ray", 8, 12);

    const hx = [];
    const hy = [];
    const wx = windingX(insp.curves, ex, ey, ppem, hx);
    const wy = windingY(insp.curves, ex, ey, ppem, hy);
    hx.sort((a, b) => a.x - b.x);
    hy.sort((a, b) => a.x - b.x);
    // Crossing markers: on the horizontal ray a crossing sits x ahead of the
    // sample; on the vertical (swapped) ray "x" is a distance in real y.
    let flip = 0;
    for (const c of hx) marker(ex + c.x, ey, c.sign, c.contrib, 7, (flip++ % 2) ? 14 : -8);
    for (const c of hy) marker(ex, ey + c.x, c.sign, c.contrib, 8, 4);

    // The sample point.
    const [spx, spy] = em.toPx([ex, ey]);
    labelG.append(
      fig.svgEl("circle", { cx: spx, cy: spy, r: 6.5, fill: "none", stroke: palette.teal, "stroke-width": 1.5 }),
      fig.svgEl("circle", { cx: spx, cy: spy, r: 2.4, fill: palette.teal }),
    );

    const covX = clamp01(Math.abs(wx));
    const covY = clamp01(Math.abs(wy));
    const cov = (covX + covY) / 2;
    const chips = (list) => list.map((c) => fmt(c.contrib, 2)).join("  ") || "(none)";
    readout.innerHTML =
      `sample (${ex.toFixed(3)}, ${ey.toFixed(3)}) em · window = 1 px = 1/${ppem} em\n` +
      `+x ray  Σ = <b style="color:${palette.text}">${fmt(wx)}</b>   crossings: ${chips(hx)}\n` +
      `+y ray  Σ = <b style="color:${palette.text}">${fmt(wy)}</b>   crossings: ${chips(hy)}\n` +
      `coverage = (clamp∣Σx∣ + clamp∣Σy∣) / 2 = (${covX.toFixed(3)} + ${covY.toFixed(3)}) / 2 = ` +
      `<b style="color:${palette.orange}">${cov.toFixed(3)}</b>` +
      (ppem < 24
        ? `\n<span style="color:${palette.muted}">(below 24 px/em the real shader averages three parallel rays per axis; one is shown)</span>`
        : "");
  };

  let dragging = false;
  const move = (e) => {
    const rect = node.getBoundingClientRect();
    const vx = ((e.clientX - rect.left) * em.width) / rect.width;
    const vy = ((e.clientY - rect.top) * em.height) / rect.height;
    const [ex, ey] = pxToEm(vx, vy);
    state.ex = Math.min(draw[2] - 0.01, Math.max(draw[0] + 0.01, ex));
    state.ey = Math.min(draw[3] - 0.01, Math.max(draw[1] + 0.01, ey));
    update();
  };
  node.addEventListener("pointerdown", (e) => {
    dragging = true;
    try { node.setPointerCapture(e.pointerId); } catch { /* synthetic event */ }
    move(e);
  });
  node.addEventListener("pointermove", (e) => dragging && move(e));
  node.addEventListener("pointerup", () => (dragging = false));
  node.addEventListener("pointercancel", () => (dragging = false));

  const size = fig.slider({
    label: "pixel size",
    min: 12, max: 96, step: 1, value: state.ppem,
    format: (v) => `${v} px/em`,
    oninput: (v) => {
      state.ppem = v;
      update();
    },
  });

  update();
  host.append(
    fig.figure(
      [node, readout, fig.controls(size.root)],
      "Drag the sample point anywhere over DejaVu Sans 'g'. The two rays are " +
        "the shader's winding probes; every marked dot is a root of one stored " +
        "quadratic, green for a downward crossing (+1), red for upward (−1), " +
        "labeled with its clamp() contribution at the current pixel size. " +
        "Crossings behind the sample fade to ±0.00 — the clamp, not a branch, is " +
        "what makes the ray one-sided. Orange: the one-pixel AA window. The " +
        "arithmetic is a line-for-line port of curve_winding and the 1-tap " +
        "loop of vector_fs (shaders.wgsl). Inside the counter, +1 and −1 " +
        "crossings cancel: the hole falls out of the sum, no even–odd " +
        "special-casing.",
    ),
  );
}

// ---------------------------------------------------------------------------
// Figure b: the parabola y(t), for three real (curve, ray) pairs covering the
// three sign situations. `sources` is searched in order — text faces put
// on-curve points at y-extrema, so the two-crossing case needs a specimen
// whose control point overshoots both endpoints (Manrope's 't' has one).
function buildParabolas(host, fig, palette, sources) {
  const find = (codes) => {
    for (const src of sources) {
      let best = null;
      src.insp.curves.forEach((c, i) => {
        const ys = [c.p0[1], c.p1[1], c.p2[1]];
        const ay = ys[0] - 2 * ys[1] + ys[2];
        if (Math.abs(ay) < 0.02) return; // want visible curvature
        const lo = Math.min(...ys);
        const hi = Math.max(...ys);
        // Best ray height per curve: farthest from every critical level (the
        // control y's and the parabola vertex), so nothing in the panel
        // grazes and the roots separate legibly.
        const b0 = ys[0] - ys[1];
        const levels = [...ys, ys[0] - (b0 * b0) / ay];
        let cand = null;
        for (let k = 1; k < 96; k++) {
          const ey = lo + ((hi - lo) * k) / 96;
          const code = (ys[0] > ey ? 2 : 0) | (ys[1] > ey ? 4 : 0) | (ys[2] > ey ? 8 : 0);
          if (!codes.includes(code)) continue;
          // When the pattern counts both roots they must actually be real
          // and separated (a ray below the vertex grazes: rad < 0, the two
          // clamps cancel exactly — correct, but not this panel's story).
          const rad = b0 * b0 - ay * (ys[0] - ey);
          if ((T0_MASK >> code) & 1 && (T1_MASK >> code) & 1 && rad <= 1e-6) continue;
          const clearance = Math.min(...levels.map((L) => Math.abs(L - ey)));
          if (!cand || clearance > cand.clearance) cand = { ey, code, clearance };
        }
        if (cand && (!best || Math.abs(ay) > best.score)) {
          best = { i, ey: cand.ey, code: cand.code, score: Math.abs(ay), src };
        }
      });
      if (best) return best;
    }
    return null;
  };

  const cases = [
    { codes: [2, 6], label: "one crossing, downward" },
    { codes: [4, 10], label: "two crossings" },
    { codes: [8, 12], label: "one crossing, upward" },
  ];

  const panels = [];
  for (const cs of cases) {
    const ex = find(cs.codes);
    if (!ex) continue;
    const c = ex.src.insp.curves[ex.i];
    const y0 = c.p0[1] - ex.ey;
    const y1 = c.p1[1] - ex.ey;
    const y2 = c.p2[1] - ex.ey;
    const a = y0 - 2 * y1 + y2;
    const b = y0 - y1;
    const W = 246;
    const H = 210;
    const mL = 14;
    const mR = 14;
    const mT = 46;
    const mB = 30;
    const tMin = -0.14;
    const tMax = 1.14;
    const yAt = (t) => (a * t - 2 * b) * t + y0;
    let yLo = 0;
    let yHi = 0;
    for (let t = tMin; t <= tMax; t += 0.02) {
      yLo = Math.min(yLo, yAt(t));
      yHi = Math.max(yHi, yAt(t));
    }
    const padY = 0.15 * (yHi - yLo || 1);
    yLo -= padY;
    yHi += padY;
    const X = (t) => mL + ((t - tMin) / (tMax - tMin)) * (W - mL - mR);
    const Y = (y) => mT + ((yHi - y) / (yHi - yLo)) * (H - mT - mB);

    const node = fig.svg(W, H);
    // The ray: y = 0.
    node.append(
      fig.svgEl("line", { x1: mL, y1: Y(0), x2: W - mR, y2: Y(0), stroke: palette.muted, "stroke-width": 1 }),
      fig.svgEl("text", { x: W - mR - 30, y: Y(0) - 5, fill: palette.muted, "font-size": 10, "font-family": MONO }, "y = 0"),
    );
    // Segment bounds t = 0, 1.
    for (const t of [0, 1]) {
      node.append(
        fig.svgEl("line", {
          x1: X(t), y1: mT - 6, x2: X(t), y2: H - mB + 4,
          stroke: palette.border, "stroke-width": 1, "stroke-dasharray": "3 3",
        }),
        fig.svgEl("text", { x: X(t) - 8, y: H - mB + 16, fill: palette.muted, "font-size": 10, "font-family": MONO }, `t=${t}`),
      );
    }
    // The parabola itself.
    let d = "";
    for (let k = 0; k <= 64; k++) {
      const t = tMin + ((tMax - tMin) * k) / 64;
      d += `${k ? "L" : "M"} ${X(t).toFixed(2)} ${Y(yAt(t)).toFixed(2)} `;
    }
    node.append(fig.svgEl("path", { d, fill: "none", stroke: palette.blue, "stroke-width": 2 }));
    // Endpoints (t = 0 and t = 1 are p0.y and p2.y).
    for (const [t, name] of [[0, "p₀"], [1, "p₂"]]) {
      node.append(
        fig.svgEl("circle", { cx: X(t), cy: Y(yAt(t)), r: 3, fill: palette.teal }),
        fig.svgEl("text", { x: X(t) + 5, y: Y(yAt(t)) - 6, fill: palette.teal, "font-size": 10.5, "font-family": MONO }, name),
      );
    }
    // Roots, counted or not.
    const s = Math.sqrt(Math.max(b * b - a * y0, 0));
    const roots = [
      { t: (b - s) / a, counted: (T0_MASK >> ex.code) & 1, sign: 1, name: "t₀", slope: -2 * s },
      { t: (b + s) / a, counted: (T1_MASK >> ex.code) & 1, sign: -1, name: "t₁", slope: 2 * s },
    ];
    for (const r of roots) {
      if (r.t < tMin || r.t > tMax) continue;
      const color = r.sign > 0 ? palette.green : palette.red;
      const px = X(r.t);
      const py = Y(0);
      if (r.counted) {
        // Tangent tick: slope dy/dt at the root is exactly −2s / +2s.
        const dxp = X(1) - X(0); // px per unit t
        const dyp = -r.slope * ((H - mT - mB) / (yHi - yLo));
        const nrm = 16 / Math.hypot(dxp, dyp);
        node.append(
          fig.svgEl("line", {
            x1: px - dxp * nrm, y1: py - dyp * nrm, x2: px + dxp * nrm, y2: py + dyp * nrm,
            stroke: color, "stroke-width": 1.2, "stroke-opacity": 0.7,
          }),
          fig.svgEl("circle", { cx: px, cy: py, r: 4, fill: color }),
          // Right-align labels near the right edge so they never clip or
          // collide with the "y = 0" tag.
          fig.svgEl(
            "text",
            {
              x: px > W * 0.6 ? px - 6 : px + 6,
              y: py + (r.sign > 0 ? 16 : -9),
              "text-anchor": px > W * 0.6 ? "end" : "start",
              fill: color, "font-size": 10.5, "font-family": MONO,
            },
            `${r.name} (${r.sign > 0 ? "+1" : "−1"}, y′=${r.sign > 0 ? "−2s" : "+2s"})`,
          ),
        );
      } else {
        node.append(
          fig.svgEl("circle", { cx: px, cy: py, r: 4, fill: "none", stroke: palette.muted, "stroke-width": 1.2 }),
          fig.svgEl(
            "text",
            { x: px - 12, y: py - 9, fill: palette.muted, "font-size": 10, "font-family": MONO },
            `${r.name} masked`,
          ),
        );
      }
    }
    const signs = [2, 4, 8].map((bit) => (ex.code & bit ? "+" : "−")).join(",");
    node.append(
      fig.svgEl(
        "text",
        { x: mL, y: 16, fill: palette.text, "font-size": 12, "font-family": MONO },
        `(p₀,p₁,p₂) = (${signs})  → code ${ex.code}`,
      ),
      fig.svgEl("text", { x: mL, y: 32, fill: palette.muted, "font-size": 11 }, `${ex.src.label} curve #${ex.i}: ${cs.label}`),
    );
    panels.push(node);
  }

  host.append(
    fig.figure(
      [fig.el("div", { style: "display:flex;flex-wrap:wrap;gap:10px;justify-content:center;" }, ...panels)],
      "The scalar quadratic y(t) = a·t² − 2b·t + c for three real curve/ray " +
        "pairs, found by scanning the inspector's data. The sign pattern of " +
        "(p₀, p₁, p₂) against the ray picks which roots are crossings — " +
        "filled dots count, hollow ones are masked out. t₀ always pierces the " +
        "ray going down (slope exactly −2s), t₁ always going up (+2s), so the " +
        "crossing's sign needs no branch. The two-crossing case is genuinely " +
        "rare in text faces — fonts put on-curve points at the extrema, so a " +
        "control point almost never overshoots both endpoints; the specimen " +
        "here is the shallow dip in the foot of Manrope's 't', grazing a ray " +
        "by ±0.004 em. Exactly the geometry where a t ∈ [0,1) range check " +
        "would fall apart, and the sign masks do not.",
    ),
  );
}

// ---------------------------------------------------------------------------
// Figure c: the AA window on a real stem — the clamp as a box filter.
function buildWindow(host, fig, palette, insp) {
  // The most vertical, tallest curve in the glyph = a stem edge.
  let best = null;
  insp.curves.forEach((c, i) => {
    const xs = [c.p0[0], c.p1[0], c.p2[0]];
    const ys = [c.p0[1], c.p1[1], c.p2[1]];
    const xspan = Math.max(...xs) - Math.min(...xs);
    const yspan = Math.max(...ys) - Math.min(...ys);
    if (yspan < 0.05) return;
    const score = yspan / (xspan + 1e-4);
    if (!best || score > best.score) {
      best = { i, score, x: (Math.min(...xs) + Math.max(...xs)) / 2, ey: (Math.min(...ys) + Math.max(...ys)) / 2 };
    }
  });

  const ppem = 32;
  const w = 1 / ppem;
  const ey = best.ey;
  const s0 = best.x; // provisional; refined to the left edge below
  const span = 3.4 * w;
  const N = 340;
  const xs = [];
  const ramp = [];
  const binary = [];
  for (let k = 0; k <= N; k++) {
    const x = s0 - span + (2 * span * k) / N;
    xs.push(x);
    ramp.push(clamp01(Math.abs(windingX(insp.curves, x, ey, ppem))));
    binary.push(clamp01(Math.abs(windingX(insp.curves, x, ey, 1e7))));
  }
  // Left edge of the stem: first 0->1 binary transition in range.
  let edge = best.x;
  for (let k = 1; k <= N; k++) {
    if (binary[k - 1] < 0.5 && binary[k] >= 0.5) {
      edge = (xs[k - 1] + xs[k]) / 2;
      break;
    }
  }
  const sw = edge - 0.25 * w; // window center: sample ~a quarter pixel outside
  const covW = clamp01(Math.abs(windingX(insp.curves, sw, ey, ppem)));

  const W = 600;
  const H = 268;
  const mL = 46;
  const mR = 16;
  const stripT = 20;
  const stripB = 52;
  const plotT = 78;
  const plotB = H - 34;
  const X = (x) => mL + ((x - (s0 - span)) / (2 * span)) * (W - mL - mR);
  const Y = (c) => plotB - c * (plotB - plotT);
  const node = fig.svg(W, H);

  // Top strip: binary ink along the scanline.
  node.append(
    fig.svgEl("text", { x: mL, y: stripT - 6, fill: palette.muted, "font-size": 11 }, "ink on the scanline (binary winding)"),
  );
  let run = null;
  for (let k = 0; k <= N; k++) {
    const inside = binary[k] >= 0.5;
    if (inside && run === null) run = xs[k];
    if ((!inside || k === N) && run !== null) {
      node.append(
        fig.svgEl("rect", {
          x: X(run), y: stripT, width: X(xs[k]) - X(run), height: stripB - stripT,
          fill: palette.blue, "fill-opacity": 0.5, stroke: "none",
        }),
      );
      run = null;
    }
  }

  // Pixel grid ticks (1 px = 1/32 em), labeled relative to the edge.
  for (let p = -3; p <= 3; p++) {
    const x = edge + p * w;
    if (x < s0 - span || x > s0 + span) continue;
    node.append(
      fig.svgEl("line", { x1: X(x), y1: plotT, x2: X(x), y2: plotB, stroke: palette.border, "stroke-width": 1 }),
      fig.svgEl("text", { x: X(x) - 8, y: plotB + 16, fill: palette.muted, "font-size": 10, "font-family": MONO }, `${p}px`),
    );
  }
  // Coverage gridlines.
  for (const c of [0, 0.5, 1]) {
    node.append(
      fig.svgEl("line", { x1: mL, y1: Y(c), x2: W - mR, y2: Y(c), stroke: palette.border, "stroke-width": 1, "stroke-dasharray": "2 4" }),
      fig.svgEl("text", { x: 8, y: Y(c) + 4, fill: palette.muted, "font-size": 10, "font-family": MONO }, c.toFixed(1)),
    );
  }

  // The 1-px window at the marked sample, spanning strip and plot.
  node.append(
    fig.svgEl("rect", {
      x: X(sw - w / 2), y: stripT, width: X(sw + w / 2) - X(sw - w / 2), height: plotB - stripT,
      fill: palette.orange, "fill-opacity": 0.13, stroke: palette.orange, "stroke-width": 1, "stroke-dasharray": "4 3",
    }),
    fig.svgEl("text", { x: X(sw - w / 2), y: stripT + 44, fill: palette.orange, "font-size": 10.5, "font-family": MONO }, "1-px window"),
  );

  // Curves: binary step (what winding alone gives) and the clamped ramp.
  const path = (arr) => {
    let d = "";
    for (let k = 0; k <= N; k++) d += `${k ? "L" : "M"} ${X(xs[k]).toFixed(2)} ${Y(arr[k]).toFixed(2)} `;
    return d;
  };
  node.append(
    fig.svgEl("path", { d: path(binary), fill: "none", stroke: palette.red, "stroke-width": 1.4, "stroke-dasharray": "5 3" }),
    fig.svgEl("path", { d: path(ramp), fill: "none", stroke: palette.green, "stroke-width": 2 }),
    fig.svgEl("circle", { cx: X(sw), cy: Y(covW), r: 4.5, fill: palette.teal }),
    fig.svgEl(
      "text",
      { x: X(sw) + 8, y: Y(covW) - 8, fill: palette.teal, "font-size": 11, "font-family": MONO },
      `coverage ${covW.toFixed(2)}`,
    ),
    // Legend sits in the plot's lower left, where coverage is flat zero.
    fig.svgEl("text", { x: mL + 8, y: plotB - 26, fill: palette.red, "font-size": 11, "font-family": MONO }, "binary ╌╌"),
    fig.svgEl("text", { x: mL + 8, y: plotB - 12, fill: palette.green, "font-size": 11, "font-family": MONO }, "clamp(x·inv + ½) ──"),
  );

  host.append(
    fig.figure(
      [node],
      `A horizontal scanline through the ${insp.ch === "n" ? "left stem of DejaVu Sans 'n'" : `stem of '${insp.ch}'`} ` +
        "at 32 px/em, computed with the shader mirror. Dashed red: raw winding " +
        "— a step at each edge, one hard pixel. Green: the same sum with each " +
        "crossing priced at clamp(x·inv_diameter + ½): every step becomes a " +
        "one-pixel-wide linear ramp. The marked sample sits a quarter pixel " +
        "outside the edge; its orange one-pixel window overlaps the ink strip " +
        "by exactly the coverage it reports — the clamp IS the box filter, " +
        "evaluated in closed form.",
    ),
  );
}

// ---------------------------------------------------------------------------
// Heat-map panels shared by figures d and e.
function heatCanvas(curves, bbox, ppem, mode, cssScale, rgb) {
  const pad = 2;
  const [minX, minY, maxX, maxY] = bbox;
  const w = Math.ceil((maxX - minX) * ppem) + 2 * pad;
  const h = Math.ceil((maxY - minY) * ppem) + 2 * pad;
  const canvas = document.createElement("canvas");
  canvas.width = w;
  canvas.height = h;
  canvas.style.width = `${w * cssScale}px`;
  canvas.style.imageRendering = "pixelated";
  const g2d = canvas.getContext("2d");
  const img = g2d.createImageData(w, h);
  let ink = 0;
  for (let j = 0; j < h; j++) {
    const ey = maxY + pad / ppem - (j + 0.5) / ppem;
    for (let i = 0; i < w; i++) {
      const ex = minX - pad / ppem + (i + 0.5) / ppem;
      const cx = clamp01(Math.abs(windingX(curves, ex, ey, ppem)));
      const cy = clamp01(Math.abs(windingY(curves, ex, ey, ppem)));
      const cov = mode === "x" ? cx : mode === "y" ? cy : (cx + cy) / 2;
      ink += cov;
      const o = (j * w + i) * 4;
      img.data[o] = rgb[0];
      img.data[o + 1] = rgb[1];
      img.data[o + 2] = rgb[2];
      img.data[o + 3] = Math.round(cov * 255);
    }
  }
  g2d.putImageData(img, 0, 0);
  return { canvas, ink, w, h };
}

const panelLabel = (fig, palette, text) =>
  fig.el("div", { style: `font-family:${MONO};font-size:12px;color:${palette.muted};margin-top:6px;text-align:center;` }, text);

// ---------------------------------------------------------------------------
export default {
  slug: "the-winding-rule",
  title: "Inside or outside: the winding rule, per pixel",

  html: `
    <p>Strip a text renderer down to its one non-negotiable question and this
    is what is left: <em>given a point and a closed outline, is the point
    inside?</em> Every glyph outline faf-text draws — at every weight, every
    size, under every 3D tilt — funnels through one fragment shader entry
    point, <code>vector_fs</code>, that answers it from scratch for every
    pixel of every glyph, every frame. No signed-distance bake, no atlas of
    pre-rasterized glyph bitmaps, no MSAA. This section derives that shader
    from first principles.</p>

    <p>The answer is the <strong>non-zero winding rule</strong>. Fire a ray
    from the point off to infinity, in any direction, and watch the outline
    cross it: each crossing counts +1 in one direction, −1 in the other. Sum
    them. Zero means outside; anything else means inside. The rule handles
    holes with no special cases at all — the counter of a 'g' is a contour
    wound the other way, its crossings cancel the outer contour's, and the sum
    lands back on zero. Because TrueType and CFF outlines wind in opposite
    conventions, the shader takes <code>abs()</code> of the sum at the end and
    never needs to know which kind of font it is drawing.</p>

    <div data-fig="hero"></div>

    <h3>Solving the crossing in closed form</h3>

    <p>A crossing is a point where a curve's y equals the ray's y.
    Intersecting a Bézier with an arbitrary line is doable but joyless;
    intersecting it with an <em>axis-aligned</em> ray is a scalar quadratic.
    The shader first translates every control point so the sample sits at the
    origin (<code>curve.p0 - in.em</code> in <code>vector_fs</code>) — the ray
    is now simply the positive x axis, and crossings are the roots of
    y(t)&nbsp;=&nbsp;0. Expanding the Bernstein form and collecting powers of
    t:</p>

    <pre><code>y(t) = (1−t)²·p₀ + 2t(1−t)·p₁ + t²·p₂
     = (p₀ − 2p₁ + p₂)·t² − 2(p₀ − p₁)·t + p₀
     =        a·t²        −      2b·t      + c

a·t² − 2b·t + c = 0   ⇒   t = (b ± s)/a,   s = √(b² − a·c)</code></pre>

    <p>The factor of −2 on b is not cosmetic: it makes the quadratic formula
    lose its 2s and 4s (this is the classic "half-b" form), and it is exactly
    what <code>curve_winding</code> computes — <code>a = p0 - 2.0 * p1 + p2;
    b = p0 - p1; c = p0</code>, then <code>t0 = (b.y - s) / a.y</code> and
    <code>t1 = (b.y + s) / a.y</code>.</p>

    <p>Now the part that earns its keep. Differentiate: y′(t) = 2a·t − 2b.
    Substitute the roots and the algebra collapses:</p>

    <pre><code>y′(t₀) = 2a·(b − s)/a − 2b = −2s
y′(t₁) = 2a·(b + s)/a − 2b = +2s</code></pre>

    <p>Since s&nbsp;=&nbsp;√(…)&nbsp;≥&nbsp;0, <strong>t₀ always crosses the
    ray going down and t₁ always going up</strong> — independent of the
    curve's orientation, of the sign of a, even of which root comes first in t
    (when a&nbsp;&lt;&nbsp;0 they trade places on the t axis, but never trade
    directions). The crossing's winding sign, the thing the whole rule is
    built on, comes out of the root formula for free. No cross products, no
    orientation tests, no branches.</p>

    <div data-fig="parabola"></div>

    <h3>Which roots are real crossings</h3>

    <p>The quadratic formula is an oracle that always answers, even when the
    curve <em>segment</em> (t ∈ [0,1]) never touches the ray: roots land
    outside the segment, or the radicand goes negative. The obvious filter —
    keep roots with t in [0,&nbsp;1) — is a trap, and it is worth being loud
    about why. Font coordinates are exact binary fractions (1/64ths from the
    hinting-free extraction, 1/2048ths in DejaVu's em grid), so a ray through
    a glyph's y-extremum or a contour's shared endpoint grazes it
    <em>exactly</em>, and the root sits precisely on the boundary of the
    range check. Two adjacent curves share that point — count it in both and
    the winding is off by one everywhere to its left; count it in neither,
    same disaster. An early version of this shader rendered FreeMono's 'p' as
    mirrored garbage from exactly this. The
    <a href="#exact-at-the-edges">next section</a> dissects the failure on
    real curves, digit by digit.</p>

    <p><code>curve_winding</code> instead classifies by the <em>sign
    pattern</em> of the three control points' y coordinates —
    <code>code = (p0.y&gt;0)·2 | (p1.y&gt;0)·4 | (p2.y&gt;0)·8</code>, with
    "exactly on the ray" counting as not-above. Both owners of a shared
    endpoint then classify that point identically, and the sum comes out
    right by construction: a ray through a point the outline passes through
    counts one crossing, on one of the two curves; a ray grazing a local
    extremum counts two that cancel. Eight possible
    patterns, and each one <em>determines</em> how many crossings the segment
    has and in which directions (a quadratic's convexity leaves no other
    option). The answers are baked into two bitmask constants,
    <code>T0_MASK = 0x454</code> and <code>T1_MASK = 0x1510</code>: bit
    <code>code</code> of each says whether t₀ / t₁ is a genuine crossing.
    Classification, root selection and signing — two mask lookups and zero
    comparisons on t.</p>

    <div data-fig="masks"></div>

    <h3>Analytic antialiasing: the clamp is a box filter</h3>

    <p>Everything so far yields a binary verdict, which rasterizes as
    staircases. The usual escapes are supersampling (pay 4–16× and still see
    bands) or prefiltering the shape before sampling it. faf-text prefilters
    — <em>exactly</em>, in closed form, using a number the winding math
    already computed and threw away: the crossing's position. For each
    counted root, the shader evaluates the curve's x at that t (the
    crossing's signed distance ahead of the sample along the ray) and, instead
    of counting the crossing as 0-or-1, counts</p>

    <pre><code>clamp(x · inv_diameter + ½, 0, 1)      // curve_winding, both root branches</code></pre>

    <p>where <code>inv_diameter = 1.0 / fwidth(in.em)</code> is this
    fragment's density in pixels per em, one per axis, taken at the very top
    of <code>vector_fs</code> before any control flow (WGSL requires
    derivatives in uniform control flow, and it matters here). To see what the
    clamp <em>is</em>, slide the sample along the ray and watch one crossing's
    binary contribution: it is a step function, 1 while the crossing is ahead,
    0 once it is behind. Convolve that step with a box the width of one pixel
    (w em) centered on the sample:</p>

    <pre><code>(1/w) ∫ step(x − u) du  over u ∈ [−w/2, +w/2]   =   clamp(x/w + ½, 0, 1)</code></pre>

    <p>— the clamp, term for term. And since the winding number along the
    whole line is exactly the sum of its crossings' signed steps, summing the
    clamps box-filters the <em>entire winding function</em> along the ray:
    perfect one-dimensional prefiltering, no extra samples, no distance field,
    from arithmetic the crossing test already owed us. It is also
    perspective-correct by construction — <code>fwidth</code> measures em per
    pixel as it actually lands on screen, so a glyph on a 3D-tilted pane gets
    a wider filter exactly where it is foreshortened, with no part of the
    coverage math knowing a rotation happened.</p>

    <div data-fig="window"></div>

    <p>One subtlety the figure above makes visible: the clamp also quietly
    encodes "ahead". A crossing half a pixel behind the sample prices out to
    exactly 0, one half a pixel ahead to exactly 1 — so the shader never
    tests which side of the sample a root landed on. The one-sidedness of the
    ray and its antialiasing are the same clamp.</p>

    <div data-fig="heat"></div>

    <h3>Two axes, averaged</h3>

    <p>A horizontal ray filters along x and only along x. For an edge
    <em>parallel</em> to it — the flat top of a 'z', a serif — the ray's
    verdict is decided purely by the sign tests on y, and those flip all at
    once as the sample steps past the edge: a hard, full-amplitude staircase,
    with the perpendicular edges antialiased beautifully all the while. So
    <code>vector_fs</code> fires a second ray along +y and averages. The
    vertical ray costs no second code path: swap each control point's
    coordinates (<code>p0.yx</code> in the WGSL) and
    <code>curve_winding</code> runs unchanged, casting "up" while believing
    it is casting "right". The per-axis sums then meet in
    <code>vector_fs</code>'s closing loop, which at one tap per axis reduces
    to:</p>

    <pre><code>coverage = (clamp(abs(wind_x), 0, 1) + clamp(abs(wind_y), 0, 1)) / 2</code></pre>

    <p>Honesty requires the fine print: the average does not make aliasing
    vanish, it halves it. On an axis-parallel edge the parallel ray still
    contributes its step; the perpendicular ray lays a clean ramp under it,
    and the sum moves 0 → ¼ … ¾ → 1 across the pixel — a half-amplitude
    residual riding a correct ramp, visible in the middle panel below if you
    look at the bars of the 'z'. Every edge orientation is within 45° of one
    ray's normal, so every edge gets at least half of a proper box filter.
    Below 24 px/em a second problem appears — a hairline stem can slip
    <em>between</em> both rays entirely — and <code>vector_fs</code> switches
    to three parallel rays per axis, a third of a pixel apart, which both
    catches the hairlines and chops the residual steps three ways. That gate
    (<code>max(fw.x, fw.y) &gt; 1.0/24.0</code>) is why body text and display
    text run different tap counts from the same shader.</p>

    <div data-fig="iso"></div>

    <div data-fig="gory"></div>
  `,

  async mount(root, ctx) {
    const { fig, palette } = ctx;

    const [g, n, z, tM] = await Promise.all([
      ctx.inspect("g", "DejaVu Sans", 400),
      ctx.inspect("n", "DejaVu Sans", 400),
      ctx.inspect("z", "DejaVu Sans", 400),
      ctx.inspect("t", "Manrope", 400),
    ]);

    buildHero(root.querySelector('[data-fig="hero"]'), fig, palette, g);
    buildParabolas(root.querySelector('[data-fig="parabola"]'), fig, palette, [
      { insp: g, label: "DejaVu 'g'" },
      { insp: tM, label: "Manrope 't'" },
    ]);

    // The mask table, from the same constants the mirror uses.
    const rows = [0, 2, 4, 6, 8, 10, 12, 14]
      .map((code) => {
        const signs = [2, 4, 8].map((b) => (code & b ? "+" : "−")).join(", ");
        const t0 = (T0_MASK >> code) & 1;
        const t1 = (T1_MASK >> code) & 1;
        const what =
          t0 && t1 ? "two crossings" : t0 ? "one, downward" : t1 ? "one, upward" : "no crossing";
        const cell = (on, label, color) =>
          `<td style="padding:3px 14px;color:${on ? color : palette.muted};">${on ? label : "·"}</td>`;
        return (
          `<tr><td style="padding:3px 14px;color:${palette.text};">${code}</td>` +
          `<td style="padding:3px 14px;">(${signs})</td>` +
          `<td style="padding:3px 14px;">${what}</td>` +
          cell(t0, "t₀ → +1", palette.green) +
          cell(t1, "t₁ → −1", palette.red) +
          "</tr>"
        );
      })
      .join("");
    root.querySelector('[data-fig="masks"]').append(
      fig.details(
        "The gory details: all eight sign patterns",
        `<p>The full contents of <code>T0_MASK = 0x454</code> and
         <code>T1_MASK = 0x1510</code>, decoded. A pattern's crossing count is
         forced by convexity: a quadratic's y(t) is a parabola, so between two
         control points it can change sign at most twice, and the hull signs
         pin down exactly where. "+" means strictly above the ray (a point
         exactly on the ray counts as below — that tie-break is what makes
         shared endpoints count exactly once).</p>
         <table style="border-collapse:collapse;font-family:${MONO};font-size:13px;color:${palette.textDim};">
         <tr style="color:${palette.muted};"><td style="padding:3px 14px;">code</td>
         <td style="padding:3px 14px;">(p₀, p₁, p₂)</td>
         <td style="padding:3px 14px;">geometry</td>
         <td style="padding:3px 14px;">t₀</td><td style="padding:3px 14px;">t₁</td></tr>
         ${rows}</table>
         <p style="margin-top:12px;">Two more branches guard the numerics.
         When |a.y| ≤ 10⁻⁴ the curve is (near-)linear in y and the quadratic
         formula would divide by ~zero, so <code>curve_winding</code> solves
         −2b·t + c = 0 directly (t = c.y / 2b.y) and takes the crossing
         direction from the endpoint signs. And the radicand is clamped with
         <code>max(…, 0.0)</code>: the sign pattern already guarantees the
         counted roots exist, so a barely-negative radicand can only be
         floating-point noise on a grazing ray, where the two crossings
         coincide and cancel exactly.</p>`,
      ),
    );

    buildWindow(root.querySelector('[data-fig="window"]'), fig, palette, n);

    // Figure d: JS heat-map vs the live renderer.
    const heatHost = root.querySelector('[data-fig="heat"]');
    const heat = heatCanvas(g.curves, g.bbox, 64, "avg", 5, [192, 202, 245]);
    const demoCanvas = document.createElement("canvas");
    const demoWrap = fig.el(
      "div",
      { style: "width:230px;" },
      demoCanvas,
      panelLabel(fig, palette, "the renderer, live (WebGPU/WebGL2)"),
    );
    heatHost.append(
      fig.figure(
        [
          fig.el(
            "div",
            { style: "display:flex;gap:28px;align-items:center;justify-content:center;flex-wrap:wrap;" },
            fig.el(
              "div",
              { style: "text-align:center;" },
              heat.canvas,
              panelLabel(fig, palette, `JS mirror · ${heat.w}×${heat.h} px · 64 px/em · 5× zoom`),
            ),
            demoWrap,
          ),
        ],
        `Proof the mirror is the shader: left, coverage for every pixel of a ` +
          `${heat.w}×${heat.h} grid over the 'g' at 64 px/em, computed in ` +
          "JavaScript by the ported curve_winding/vector_fs math and magnified " +
          "5× so the one-pixel AA ramps are visible; right, the actual " +
          "renderer drawing the same glyph from the same curve records at the " +
          "same nominal size (at native scale, and through your GPU). Total " +
          `JS-side coverage: ${heat.ink.toFixed(1)} px² of ink. The mirror walks ` +
          "the flat curve list; the GPU walks band tables — the sums are " +
          "identical because every curve a band omits contributes exactly zero " +
          "(the renderer asserts this bit-for-bit in its " +
          "band_tables_do_not_move_a_single_pixel test).",
      ),
    );

    // Figure e: isotropy — x-only vs y-only vs averaged.
    const isoHost = root.querySelector('[data-fig="iso"]');
    const modes = [
      ["x", "clamp∣Σx∣ only (+x ray)"],
      ["y", "clamp∣Σy∣ only (+y ray)"],
      ["avg", "(x + y) / 2 — the shader"],
    ];
    const isoPanels = modes.map(([mode, label]) => {
      const p = heatCanvas(z.curves, z.bbox, 26, mode, 11, [192, 202, 245]);
      return fig.el("div", { style: "text-align:center;" }, p.canvas, panelLabel(fig, palette, label));
    });
    isoHost.append(
      fig.figure(
        [fig.el("div", { style: "display:flex;gap:22px;justify-content:center;flex-wrap:wrap;align-items:flex-start;" }, ...isoPanels)],
        "Why two axes: DejaVu Sans 'z' at 26 px/em (11× zoom), same mirror, " +
          "three coverage recipes. Left: the horizontal ray alone — its own " +
          "axis is filtered, so the diagonal is smooth, but the horizontal " +
          "bars' edges are hard steps. Middle: the vertical ray alone — the " +
          "bars go soft, the stroke ends harden. Right: the average " +
          "vector_fs actually computes; every orientation now gets at least " +
          "half of a true box filter, and the residual half-steps on " +
          "axis-parallel edges are what the 3-tap path (below 24 px/em) " +
          "further subdivides.",
      ),
    );

    root.querySelector('[data-fig="gory"]').append(
      fig.details(
        "The gory details: where “exact” ends",
        `<p>The per-ray sum of clamps is the exact box-filtered winding
         function — but three later steps reintroduce approximation, and each
         is a deliberate trade. First, <code>clamp(abs(…), 0, 1)</code>
         per axis: a feature narrower than the window (two opposite crossings
         inside one pixel) partially cancels <em>before</em> the clamp, which
         is the correct area answer; but overlapping ink (winding 2) saturates
         to 1, which is what you want visually and no longer a pure area
         integral. The <code>abs()</code> is orientation insurance — TrueType
         winds clockwise where CFF winds counter, and holes have already
         cancelled arithmetically before it applies.</p>
         <p>Second, the two-axis average is a compromise kernel: a plus-shaped
         filter rather than a square, exact on edges normal to a ray, half
         amplitude on edges parallel to one. Third, below 24 px/em the 3-tap
         path replaces each axis's single ray with three, a third of a pixel
         apart, converging toward true area coverage exactly where glyph
         features shrink toward the pixel grid; the tap count is decided per
         fragment from <code>fwidth</code>, so a 3D-foreshortened pane
         upgrades itself only where it needs to.</p>
         <p>Last, precision: the coverage this section computes matches the
         GPU to the eye, and the control points are not even a source of
         disagreement — the inspector hands the mirror the same f16-quantized
         curves the texture stores (that quantization is worth ≤ 4.9×10⁻⁴ em
         of wobble; against the renderer's baseline scene it moved 2882 of
         576,000 pixels, 83% of them by ≤ 2/255 — the curve-texture section
         has the full accounting). But the GPU evaluates in f32 where this
         page ran f64, and it feeds coverage through
         <code>shape_coverage</code> — an identity on the default pipeline,
         while the linear-blending variant raises coverage to the ±1.43 power
         to keep stem weight perceptually stable in linear light. All of which
         is why “line-for-line port” and “bit-identical” are different
         claims.</p>`,
      ),
    );

    // Live canvas attaches last so one-shot screenshots always have the data
    // figures (see CONTRACT.md).
    const demo = await ctx.attachDemo(demoCanvas, { height: 170, text: "g", fontSize: 64 });
    ctx.animate(demoCanvas, demo);
  },
};
