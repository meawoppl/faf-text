// Geometry & the retained scene: corner clipping (curves.rs `corner_clips` /
// `curve_support`, shaders.wgsl `octagon_corner`) and the retained block /
// arena / damage machinery (renderer.rs, arena.rs). Every figure draws real
// inspector output or real measured numbers; the support-plane figure mirrors
// `curve_support`'s closed form in JS over the same stored curves.

/// Mirror of curves.rs `CLIP_MIN_AREA`: smallest triangle worth clipping, as a
/// fraction of the bbox area.
const CLIP_MIN_AREA = 0.04;

/// Mirror of curves.rs `curve_support`: the furthest one quadratic reaches
/// along (unnormalized) normal `n`, exactly. dot(B(t), n) is a scalar
/// quadratic; its max over [0,1] is an endpoint or, when the parabola opens
/// downward, its vertex. Returns { v, p } — the support value and a point on
/// the curve that attains it.
function curveSupport({ p0, p1, p2 }, n) {
  const d2 = ([x, y]) => n[0] * x + n[1] * y;
  const a0 = d2(p0);
  const a1 = d2(p1);
  const a2 = d2(p2);
  let best = a0 >= a2 ? { v: a0, p: p0 } : { v: a2, p: p2 };
  const d = a0 - 2 * a1 + a2;
  if (d < 0) {
    const t = (a0 - a1) / d;
    if (t > 0 && t < 1) {
      const s = 1 - t;
      const v = s * s * a0 + 2 * s * t * a1 + t * t * a2;
      if (v > best.v) {
        best = {
          v,
          p: [
            s * s * p0[0] + 2 * s * t * p1[0] + t * t * p2[0],
            s * s * p0[1] + 2 * s * t * p1[1] + t * t * p2[1],
          ],
        };
      }
    }
  }
  return best;
}

/// Max of dot(p, n) over a curve's three control points — the hull bound the
/// renderer deliberately does NOT use.
function hullSupport({ p0, p1, p2 }, n) {
  const d2 = ([x, y]) => n[0] * x + n[1] * y;
  let best = { v: d2(p0), p: p0 };
  for (const p of [p1, p2]) {
    const v = d2(p);
    if (v > best.v) best = { v, p };
  }
  return best;
}

const fmt = (x, digits = 3) => Number(x.toFixed(digits)).toString();

export default {
  slug: "geometry-and-damage",
  title: "Geometry & the retained scene",

  html: `
    <p>Everything so far happened per fragment. This section is about the two
    layers wrapped around the fragment shader: the <em>geometry</em> each
    glyph submits (less of it than you would think), and the <em>retained
    scene</em> that decides, each frame, which bytes move to the GPU (usually
    none).</p>

    <h3>Cutting the corners off</h3>

    <p>A glyph draws as a quad over its bounding box, and most glyphs leave
    the corners of that box empty — the diagonal stroke of an
    <code>A</code> touches neither top corner, a <code>v</code> touches
    neither bottom one. A fragment out in that emptiness still pays the whole
    winding apparatus — band lookup, sorted walk, ray casts — to arrive at a
    coverage of exactly zero. Lengyel's Slug paper (§3) points out the fix is
    geometric: compute, once per glyph, how deep each corner can be cut back
    along the 45° diagonal before the cut would meet ink, and never rasterize
    those triangles at all.</p>

    <p>The cut depth is a <em>support plane</em> problem, solved in
    <code>corner_clips</code> in <code>curves.rs</code>. For a corner with
    outward normal <code>n</code>, the support value
    <code>h&nbsp;=&nbsp;max&nbsp;dot(p,&nbsp;n)</code> over the whole outline
    is the furthest the glyph reaches toward that corner; the plane
    <code>dot(p,&nbsp;n)&nbsp;=&nbsp;h</code> has every point of every curve
    on one side of it. The leg cut off each box edge is then just
    <code>dot(corner,&nbsp;n)&nbsp;&minus;&nbsp;h</code>. That makes the clip
    safe by construction, not by tuning: a point outside a closed outline's
    convex hull has winding number zero, and a sample beyond a separating
    plane is at least the plane distance from every point of the curve, so
    the antialiasing window finds nothing there either.</p>

    <p>The trap is <em>what</em> you take the max over. The obvious bound —
    the control points, i.e. the convex hull — is a disaster for round
    glyphs. A quadratic approximating a quarter circle from
    <code>(r,&nbsp;0)</code> to <code>(0,&nbsp;r)</code> has its middle
    control point at <code>(r,&nbsp;r)</code>: <em>exactly</em> the corner of
    its bounding box, so the hull bound clips nothing at all — while the
    curve itself never gets closer than
    <code>B(½)&nbsp;=&nbsp;(¾r,&nbsp;¾r)</code>, half a radius down each edge
    from the corner. Real fonts split their arcs into shorter segments, which
    pulls the control polygon closer to the curve, but the hull bound does
    not have to be a full half radius short to fail: on DejaVu's
    <code>o</code> it lands just <em>below</em> the minimum worthwhile cut
    while the curve bound lands just above it, so hull planes clip that bowl
    nowhere and curve planes clip it twice. So <code>curve_support</code>
    bounds the curve:
    <code>dot(B(t),&nbsp;n)</code> is a scalar quadratic in <code>t</code>,
    and the maximum of a quadratic over <code>[0,&nbsp;1]</code> is an
    endpoint or, when the parabola opens downward, its interior vertex —
    three lines of code, closed form, and just as conservative, because the
    fragment shader tests the curve, not the hull.</p>

    <div data-fig="support"></div>

    <p>Variable fonts come along for free: a Bézier is linear in its control
    points, so the glyph blended at weight <code>t</code> is the pointwise
    blend of the two masters' curves, and
    <code>dot(mix(A(u),&nbsp;B(u),&nbsp;t),&nbsp;n)&nbsp;&le;&nbsp;max(h_A,&nbsp;h_B)</code>
    for every <code>u</code> and every <code>t</code>. One max over both
    masters' records bounds every weight the shader can ever render. And not
    every corner is worth cutting: a triangle under
    <code>CLIP_MIN_AREA&nbsp;=&nbsp;4%</code> of the bbox is left square,
    because the extra primitives cost more than the handful of fragments they
    would save.</p>

    <div data-fig="octagon"></div>

    <p>The four legs ride each glyph instance as one <code>vec4</code> in em
    — the curve texture is untouched — and the vertex shader
    (<code>octagon_corner</code> in <code>shaders.wgsl</code>) turns the quad
    into an octagon: an 18-vertex fan, the inner quad first and then one
    triangle per corner. An unclipped corner's two polygon points coincide,
    so its triangle has zero area and rasterizes nothing. The clip is also
    gated on size at queue time — below
    <code>CLIP_MIN_PX_PER_EM&nbsp;=&nbsp;48</code> px/em a corner triangle is
    a handful of pixels — and a block containing no clipped glyph still
    draws <code>0..6</code>, so small text pays nothing at all, not even the
    degenerate triangles.</p>

    <div data-fig="savings"></div>

    <p>One honest wrinkle: turning clipping on broke byte-identity with the
    old scene, and it took a proof to accept it. The offscreen regression
    scene changed by 72 pixels, each by at most 1/255, and total ink went
    75,624,048&nbsp;&rarr;&nbsp;75,624,055 — it <em>gained</em> seven
    255ths. That is not lost coverage; it is interpolation. A rasterizer
    builds attribute gradients from the vertices it is given, and the
    octagon's inner quad has different vertices than the plain quad, so every
    varying lands an ulp or so away and <code>fwidth</code> moves by parts in
    ten thousand. (Re-splitting the <em>same</em> rectangle along the other
    diagonal, with no clipping at all, moves 88 pixels by up to 98/255 —
    which is why the fan is ordered so an unclipped glyph emits
    <code>quad_corner</code>'s exact six vertices in its exact order.) The
    conservativeness itself is checked against the bytes:
    <code>corner_clipping_drops_no_pixel_that_carried_ink</code> renders a
    300&nbsp;px <code>A</code> both ways and asserts every pixel outside the
    octagon was black in the unclipped render, and every pixel inside agrees
    to within one 255th.</p>

    <div data-fig="clip-details"></div>

    <h3>The retained scene</h3>

    <p>Above the glyphs sits a retained scene graph
    (<code>renderer.rs</code>): content lives in <strong>blocks</strong>, and
    a block owns two things — a slot in one dynamic-offset uniform buffer,
    and a <code>Span</code> (start, capacity, length, in instances) in each
    of seven instance <strong>arenas</strong>, drawn in a fixed order:
    under-rects, chips, vector glyphs, weight-blended glyphs, atlas glyphs,
    line decorations, over-rects. An arena (<code>arena.rs</code>) is one
    growable GPU vertex buffer plus a CPU mirror: allocation is a bump
    pointer and a free list, and once half the arena sits in the free list it
    is repacked — every live span copied back contiguously in one write.</p>

    <div data-fig="arena"></div>

    <p>Damage tracking falls out of the split. Setting a block's content
    marks it <code>content_dirty</code>, and <code>finish</code> re-uploads
    only that block's non-empty spans. Moving a block
    (<code>set_block_offset</code>) marks it <code>uniform_dirty</code> and
    writes exactly one 80-byte <code>BlockUniform</code> — no instance is
    touched, which is why a pane can be re-oriented in 3D every frame for the
    price of one uniform write. A window resize writes the 16-byte globals
    buffer and — under the default 2D projection — nothing else. And an idle
    frame uploads <em>zero</em> bytes:
    <code>UploadStats</code> reports zeros, <code>damaged()</code> reports
    false, and the host skips recording and presenting entirely. That last
    bit is observable — the stats chip below reads <code>idle</code> because
    it counts frames actually presented, not the browser's rAF rate.</p>

    <p>Two of the quieter decisions here are load-bearing. First, the
    per-block matrix maps block-local pixels to <em>homogeneous screen
    pixels</em>, not to clip space. A clip-space matrix would fold the screen
    divide into the matrix, turning the shader's
    <code>px&nbsp;/&nbsp;W&nbsp;*&nbsp;2&nbsp;&minus;&nbsp;1</code> into
    <code>px&nbsp;*&nbsp;(2/W)&nbsp;&minus;&nbsp;1</code> — which differs by
    one ulp for about half of all x values. Trying it moved 82 pixels of the
    regression scene (by at most 2/255): invisible, but no longer
    byte-identical. Keeping the divide in the shader's <code>project</code>
    makes a 2D block's matrix a pure translation, so the vertex math is
    bit-for-bit what it was before the matrix existed; a host's 3D
    view-projection is folded into pixel space on the CPU instead. Second,
    WebGL2 has no base-instance draws, so a block's range is addressed by
    offsetting the vertex buffer itself —
    <code>buffer.slice(byte_offset..)</code> then
    <code>draw(0..vertices,&nbsp;0..len)</code> — never by a first-instance
    parameter the backend cannot express.</p>

    <div data-fig="live"></div>
    <div data-fig="scene-details"></div>
  `,

  async mount(root, ctx) {
    const { fig, palette } = ctx;

    // All inspections up front; live canvas attaches last (CONTRACT.md's
    // mount-ordering rule).
    const names = ["A", "v", "7", "o", "O"];
    const glyphs = new Map(
      (await Promise.all(names.map((ch) => ctx.inspect(ch, "DejaVu Sans", 400)))).map((g, i) => [
        names[i],
        g,
      ]),
    );

    // --- Figure: octagon on a real glyph -------------------------------
    const octagonHost = root.querySelector('[data-fig="octagon"]');
    const holder = fig.el("div");
    const capHolder = fig.el("div");

    const drawOctagon = (ch) => {
      const g = glyphs.get(ch);
      const [minX, minY, maxX, maxY] = g.bbox;
      const w = maxX - minX;
      const h = maxY - minY;
      const em = fig.emSpace(g.bbox, { width: 340, pad: 34 });
      const node = fig.svg(em.width, em.height);
      node.append(em.g);

      // The bbox the quad would cover.
      em.g.append(
        fig.svgEl("rect", {
          x: minX,
          y: minY,
          width: w,
          height: h,
          fill: "none",
          stroke: palette.muted,
          "stroke-width": 1,
        }),
      );
      // Clipped corner triangles: geometry that never rasterizes.
      const corners = [
        [minX, maxY, [1, 0], [0, -1]],
        [maxX, maxY, [-1, 0], [0, -1]],
        [maxX, minY, [-1, 0], [0, 1]],
        [minX, minY, [1, 0], [0, 1]],
      ];
      g.clips.forEach((leg, i) => {
        if (leg <= 0) return;
        const [cx, cy, dx, dy] = corners[i];
        const a = [cx + dx[0] * leg, cy + dx[1] * leg];
        const b = [cx + dy[0] * leg, cy + dy[1] * leg];
        em.g.append(
          fig.svgEl("polygon", {
            points: `${cx},${cy} ${a.join(",")} ${b.join(",")}`,
            fill: palette.red,
            "fill-opacity": 0.18,
            stroke: "none",
          }),
        );
        // The support plane, extended past the triangle's hypotenuse.
        const ext = 0.16 * Math.min(w, h);
        const dir = [b[0] - a[0], b[1] - a[1]];
        const len = Math.hypot(dir[0], dir[1]) || 1;
        const u = [dir[0] / len, dir[1] / len];
        em.g.append(
          fig.svgEl("line", {
            x1: a[0] - u[0] * ext,
            y1: a[1] - u[1] * ext,
            x2: b[0] + u[0] * ext,
            y2: b[1] + u[1] * ext,
            stroke: palette.orange,
            "stroke-width": 1,
            "stroke-dasharray": "5 3",
          }),
        );
      });
      // The octagon the vertex shader emits.
      em.g.append(
        fig.svgEl("polygon", {
          points: fig
            .clipOctagon(g.bbox, g.clips)
            .map((p) => p.join(","))
            .join(" "),
          fill: "none",
          stroke: palette.orange,
          "stroke-width": 1.4,
        }),
      );
      // The glyph, from the stored curves.
      em.g.append(
        fig.svgEl("path", {
          d: fig.glyphPathD(g.curves, g.contours),
          fill: palette.blue,
          "fill-opacity": 0.35,
          "fill-rule": "nonzero",
          stroke: palette.blue,
          "stroke-width": 1.2,
        }),
      );
      // Leg labels in pixel space.
      g.clips.forEach((leg, i) => {
        if (leg <= 0) return;
        const [cx, cy, dx, dy] = corners[i];
        const mid = em.toPx([cx + (dx[0] + dy[0]) * leg * 0.36, cy + (dx[1] + dy[1]) * leg * 0.36]);
        node.append(
          fig.svgEl("text", {
            x: mid[0],
            y: mid[1] + 4,
            "text-anchor": "middle",
            fill: palette.red,
            "font-size": 11,
            style: "font-family: inherit;",
          }, fmt(leg)),
        );
      });

      const saved = g.clips.reduce((s, l) => s + (l * l) / 2, 0) / (w * h);
      const legText = g.clips.some((l) => l > 0)
        ? `legs [${g.clips.map((l) => fmt(l)).join(", ")}] em — ` +
          `${fmt(saved * 100, 1)}% of the bbox never rasterizes.`
        : `legs [0, 0, 0, 0] — every cut worth making is under the 4% area ` +
          `threshold, so the quad stays a quad.`;
      holder.replaceChildren(node);
      capHolder.replaceChildren(
        fig.caption(
          `'${ch}' in DejaVu Sans, from ctx.inspect: bbox (gray), the stored ` +
            `outline (blue), support planes (dashed orange) and the octagon ` +
            `the vertex shader emits (solid orange). Shaded red triangles are ` +
            `never rasterized. ${legText}`,
        ),
      );
    };

    const buttons = names.map((ch) =>
      fig.el(
        "button",
        {
          style:
            `background: ${palette.bgDark}; color: ${palette.text}; ` +
            `border: 1px solid ${palette.border}; ` +
            "border-radius: 6px; padding: 4px 14px; cursor: pointer; font: inherit;",
          onclick: () => {
            drawOctagon(ch);
            buttons.forEach((b) => (b.style.borderColor = palette.border));
            buttons[names.indexOf(ch)].style.borderColor = palette.blue;
          },
        },
        ch,
      ),
    );
    drawOctagon("A");
    buttons[0].style.borderColor = palette.blue;
    octagonHost.append(fig.figure([holder, fig.controls(...buttons), capHolder]));

    // --- Figure: hull bound vs curve bound on a round glyph -------------
    const o = glyphs.get("o");
    {
      const n = [1, 1]; // top-right corner normal, unnormalized as in CLIP_NORMALS
      let hull = { v: -Infinity, p: null };
      let curve = { v: -Infinity, p: null };
      for (const q of o.curves) {
        const hs = hullSupport(q, n);
        if (hs.v > hull.v) hull = hs;
        const cs = curveSupport(q, n);
        if (cs.v > curve.v) curve = cs;
      }
      const [minX, minY, maxX, maxY] = o.bbox;
      const w = maxX - minX;
      const h = maxY - minY;
      const cornerV = maxX + maxY; // dot(corner, n)
      const hullLeg = cornerV - hull.v;
      const curveLeg = cornerV - curve.v;
      const minLeg = Math.sqrt(2 * CLIP_MIN_AREA * w * h);

      const gaugeH = 88;
      const em = fig.emSpace(o.bbox, { width: 420, pad: 44 });
      const node = fig.svg(em.width, em.height + gaugeH);
      node.append(em.g);
      em.g.append(
        fig.svgEl("rect", {
          x: minX, y: minY, width: w, height: h,
          fill: "none", stroke: palette.muted, "stroke-width": 1,
        }),
        fig.svgEl("path", {
          d: fig.glyphPathD(o.curves, o.contours),
          fill: palette.blue,
          "fill-opacity": 0.3,
          "fill-rule": "nonzero",
          stroke: palette.blue,
          "stroke-width": 1.2,
        }),
      );
      // A 45° support plane through a point p: direction (1, -1) in em.
      const plane = (p, color) => {
        const ext = 0.3 * Math.min(w, h);
        return fig.svgEl("line", {
          x1: p[0] - ext, y1: p[1] + ext,
          x2: p[0] + ext, y2: p[1] - ext,
          stroke: color, "stroke-width": 1.2, "stroke-dasharray": "5 3",
        });
      };
      em.g.append(plane(hull.p, palette.red), plane(curve.p, palette.green));
      em.g.append(
        fig.svgEl("circle", { cx: hull.p[0], cy: hull.p[1], r: 3.4 / em.scale, fill: palette.purple }),
        fig.svgEl("circle", { cx: curve.p[0], cy: curve.p[1], r: 3.4 / em.scale, fill: palette.teal }),
      );

      // The leg gauge: hull leg, curve leg and the 4%-area gate on one axis.
      const gy = em.height + 34;
      const gx0 = 60;
      const gx1 = em.width - 30;
      const maxLeg = Math.max(curveLeg, minLeg, hullLeg) * 1.25;
      const gx = (leg) => gx0 + (leg / maxLeg) * (gx1 - gx0);
      const gtext = (x, y, s, fill, anchor = "middle") =>
        fig.svgEl("text", {
          x, y, fill, "font-size": 11, "text-anchor": anchor,
          style: "font-family: inherit;",
        }, s);
      node.append(
        fig.svgEl("line", { x1: gx0, y1: gy, x2: gx1, y2: gy, stroke: palette.border, "stroke-width": 1 }),
        gtext(gx0 - 8, gy + 4, "leg:", palette.muted, "end"),
        // The gate: legs left of it are not worth the extra triangles.
        fig.svgEl("line", {
          x1: gx(minLeg), y1: gy - 22, x2: gx(minLeg), y2: gy + 10,
          stroke: palette.muted, "stroke-width": 1, "stroke-dasharray": "3 3",
        }),
        gtext(gx(minLeg), gy + 24, `4% gate ${fmt(minLeg)}`, palette.muted),
        fig.svgEl("circle", { cx: gx(hullLeg), cy: gy, r: 4, fill: palette.red }),
        gtext(gx(hullLeg), gy - 12, `hull ${fmt(hullLeg)}`, palette.red, "end"),
        fig.svgEl("circle", { cx: gx(curveLeg), cy: gy, r: 4, fill: palette.green }),
        gtext(gx(curveLeg) + 6, gy - 12, `curve ${fmt(curveLeg)}`, palette.green, "start"),
        gtext((gx0 + gx1) / 2, gy + 44,
          `inspect('o').clips = [${o.clips.map((l) => fmt(l)).join(", ")}] em`, palette.textDim),
      );

      root.querySelector('[data-fig="support"]').append(
        fig.figure(
          [node],
          `Two bounds on DejaVu Sans 'o', toward its top-right corner ` +
            `(normal n = (1, 1)), computed with curve_support's closed form ` +
            `over the stored curves. The control-hull bound (red plane, ` +
            `through the purple control point) allows a ${fmt(hullLeg)} em ` +
            `leg; bounding the curve itself (green plane, through the teal ` +
            `point where dot(B(t), n) peaks) allows ${fmt(curveLeg)} em. The ` +
            `gauge shows why the difference is decisive: the minimum ` +
            `worthwhile leg for this bbox is ${fmt(minLeg)} em (the ` +
            `4%-of-bbox-area gate), and only the curve bound clears it — ` +
            `the hull bound would leave 'o' entirely unclipped, and the ` +
            `production clip legs confirm the curve bound cuts both right ` +
            `corners.`,
        ),
      );
    }

    // --- Figure: measured fragment savings ------------------------------
    {
      const rows = [
        { label: "large-glyph sample", v: 11.1, note: "hardware pipeline-statistics counter" },
        { label: "64 px/em page", v: 5.3, note: "hardware pipeline-statistics counter" },
        { label: "whole offscreen scene", v: 1.2, note: "only a 64 px title and a 300 px “Qg” pass the 48 px/em gate" },
      ];
      const W = 520;
      const labelW = 190;
      const barMax = W - labelW - 70;
      const rowH = 30;
      const barH = 14;
      const top = 8;
      const H = top + rows.length * rowH + 26;
      const maxV = 12;
      const node = fig.svg(W, H);
      // Recessive gridlines + scale.
      for (const t of [0, 5, 10]) {
        const x = labelW + (t / maxV) * barMax;
        node.append(
          fig.svgEl("line", {
            x1: x, y1: top, x2: x, y2: top + rows.length * rowH,
            stroke: palette.border, "stroke-width": 1,
          }),
          fig.svgEl("text", {
            x, y: top + rows.length * rowH + 16, "text-anchor": "middle",
            fill: palette.muted, "font-size": 10, style: "font-family: inherit;",
          }, `${t}%`),
        );
      }
      rows.forEach((r, i) => {
        const y = top + i * rowH + (rowH - barH) / 2;
        const bw = (r.v / maxV) * barMax;
        const rr = Math.min(4, bw / 2);
        const x0 = labelW;
        const bar = fig.svgEl("path", {
          d:
            `M ${x0} ${y} L ${x0 + bw - rr} ${y} Q ${x0 + bw} ${y} ${x0 + bw} ${y + rr} ` +
            `L ${x0 + bw} ${y + barH - rr} Q ${x0 + bw} ${y + barH} ${x0 + bw - rr} ${y + barH} ` +
            `L ${x0} ${y + barH} Z`,
          fill: palette.blue,
          stroke: "none",
        });
        bar.append(fig.svgEl("title", {}, `${r.label}: ${r.v}% fewer fragment invocations (${r.note})`));
        node.append(
          bar,
          fig.svgEl("text", {
            x: labelW - 10, y: y + barH - 3, "text-anchor": "end",
            fill: palette.textDim, "font-size": 11, style: "font-family: inherit;",
          }, r.label),
          fig.svgEl("text", {
            x: x0 + bw + 8, y: y + barH - 3,
            fill: palette.text, "font-size": 11, style: "font-family: inherit;",
          }, `${r.v}%`),
        );
      });
      root.querySelector('[data-fig="savings"]').append(
        fig.figure(
          [node],
          "Fragment invocations saved by corner clipping, counted by the " +
            "GPU's pipeline-statistics query (the harness lives in the test " +
            "corner_clipping_cuts_fragment_invocations): 11.1% on large-glyph " +
            "content, 5.3% on a 64 px/em page, 1.2% on the whole offscreen " +
            "regression scene, where only " +
            "a 64 px title and a 300 px “Qg” clear the 48 px/em gate. The " +
            "11 px stress page and 32 px bench page are unmoved — neither " +
            "clips at all.",
        ),
      );
    }

    // --- Gory details: the clip math ------------------------------------
    root.querySelector('[data-fig="clip-details"]').append(
      fig.details(
        "Gory details: the closed form, both masters, and the AA pad",
        `<p>Write f(t) = dot(B(t), n) with a0, a1, a2 the dots of the three
         control points. Expanding the Bernstein basis, f(t) = a0 +
         2(a1&nbsp;&minus;&nbsp;a0)t + (a0&nbsp;&minus;&nbsp;2a1&nbsp;+&nbsp;a2)t².
         With d = a0&nbsp;&minus;&nbsp;2a1&nbsp;+&nbsp;a2: if d&nbsp;&ge;&nbsp;0
         the parabola opens upward along n and the max is an endpoint,
         max(a0,&nbsp;a2). Otherwise the vertex sits at
         t&nbsp;=&nbsp;(a0&nbsp;&minus;&nbsp;a1)/d, and if that lands inside
         (0,&nbsp;1) it competes with the endpoints. That is
         <code>curve_support</code> verbatim — including the degenerate case:
         the flattener stores a line as a quadratic with its control at the
         midpoint, which makes d exactly 0 and leaves the endpoints
         answering.</p>
         <p>For the quarter circle p0&nbsp;=&nbsp;(r,&nbsp;0),
         p1&nbsp;=&nbsp;(r,&nbsp;r), p2&nbsp;=&nbsp;(0,&nbsp;r),
         n&nbsp;=&nbsp;(1,&nbsp;1): a0&nbsp;=&nbsp;a2&nbsp;=&nbsp;r,
         a1&nbsp;=&nbsp;2r, d&nbsp;=&nbsp;&minus;2r, vertex at t&nbsp;=&nbsp;½,
         f(½)&nbsp;=&nbsp;3r/2. The hull says 2r. The difference,
         r/2, is the leg the hull bound throws away on every round corner.</p>
         <p>The two-master claim is one line of algebra: blending happens on
         control points, dot is linear, so the blended curve's f is the same
         pointwise blend of the two masters' f, and a convex combination
         never exceeds the larger of its ends. <code>corner_clips</code>
         therefore takes its max over master A's records and master B's in
         the same loop.</p>
         <p>Two conservatisms stack on top. The legs are computed against the
         em bbox, but applied at the corners of the instance quad, which is
         the bbox <em>padded</em> by 1.5 px for the antialiasing falloff — so
         the plane the geometry actually cuts along sits a further
         pad&nbsp;&middot;&nbsp;&radic;2 px outside the outline's hull, more
         than the half pixel the coverage window can reach. And the fan in
         <code>octagon_corner</code> is ordered inner-quad-first — vertices
         (0,0)&nbsp;(1,0)&nbsp;(0,1)&nbsp;(0,1)&nbsp;(1,0)&nbsp;(1,1), the
         exact floats <code>quad_corner</code> produced before clipping
         existed — so a glyph below the gate, or a corner not worth cutting,
         is not merely equivalent to the old draw. It <em>is</em> the old
         draw, byte for byte.</p>`,
      ),
    );

    // --- Figure: arenas, spans, one dirty block --------------------------
    {
      const layers = [
        "under-rects", "chips", "vector glyphs", "blended glyphs",
        "atlas glyphs", "line decorations", "over-rects",
      ];
      // Which arenas each of the web demo's own four blocks fills (from
      // sync_blocks in faf-text-web): selection → under-rects; text → the
      // three glyph arenas plus attribute decorations; search → chips (in
      // chip mode); caret → over-rects. Span *widths* here are schematic —
      // real lengths depend on the text.
      const blocks = [
        { name: "selection", color: palette.teal, spans: { 0: 0.22 } },
        {
          name: "text", color: palette.blue, dirty: true,
          spans: { 2: 0.62, 4: 0.14, 5: 0.2 },
        },
        { name: "search", color: palette.green, spans: { 1: 0.18 } },
        { name: "caret", color: palette.purple, spans: { 6: 0.06 } },
      ];
      const W = 640;
      const rowH = 22;
      const gap = 14;
      const stripW = 380;
      const x0 = 16;
      const top = 26;
      const H = top + layers.length * (rowH + gap) + 40;
      const node = fig.svg(W, H);
      const text = (x, y, s, fill, size = 10, anchor = "start") =>
        fig.svgEl("text", {
          x, y, fill, "font-size": size, "text-anchor": anchor,
          style: "font-family: inherit;",
        }, s);

      node.append(text(x0, 14, "instance arenas (one vertex buffer each)", palette.textDim, 11));
      layers.forEach((name, i) => {
        const y = top + i * (rowH + gap);
        node.append(
          fig.svgEl("rect", {
            x: x0, y, width: stripW, height: rowH, rx: 3,
            fill: palette.bgDark, stroke: palette.border, "stroke-width": 1,
          }),
          text(x0 + stripW + 8, y + rowH - 7, name, palette.muted),
        );
        let cursor = 0;
        for (const b of blocks) {
          const frac = b.spans[i];
          if (!frac) continue;
          const sx = x0 + 2 + cursor * (stripW - 4);
          const sw = frac * (stripW - 4);
          cursor += frac + 0.02; // the 2px surface gap, in fraction terms
          node.append(
            fig.svgEl("rect", {
              x: sx, y: y + 2, width: sw, height: rowH - 4, rx: 2,
              fill: b.color, "fill-opacity": b.dirty ? 0.75 : 0.45,
              stroke: b.dirty ? palette.orange : "none",
              "stroke-width": b.dirty ? 1.4 : 0,
              "stroke-dasharray": b.dirty ? "4 3" : null,
            }),
          );
        }
      });
      // Legend for the three blocks.
      let lx = x0;
      const ly = top + layers.length * (rowH + gap) + 10;
      for (const b of blocks) {
        node.append(
          fig.svgEl("rect", { x: lx, y: ly, width: 10, height: 10, rx: 2, fill: b.color, "fill-opacity": 0.6 }),
          text(lx + 15, ly + 9, b.name + (b.dirty ? " (content_dirty)" : ""), palette.textDim),
        );
        lx += b.name.length * 6.4 + (b.dirty ? 105 : 40);
      }

      // Uniform buffer column.
      const ux = 520;
      node.append(text(ux, 14, "block uniforms", palette.textDim, 11));
      blocks.forEach((b, i) => {
        const y = top + i * 54;
        node.append(
          fig.svgEl("rect", {
            x: ux, y, width: 100, height: 44, rx: 3,
            fill: palette.bgDark, stroke: palette.border, "stroke-width": 1,
          }),
          fig.svgEl("rect", {
            x: ux + 2, y: y + 2, width: 96 * (80 / 256), height: 40, rx: 2,
            fill: b.color, "fill-opacity": 0.45,
          }),
          text(ux + 36, y + 20, "80 B used", palette.muted, 9),
          text(ux + 36, y + 33, "of 256 B", palette.muted, 9),
        );
      });
      node.append(
        text(ux, top + blocks.length * 54 + 12, "stride = max(256,", palette.muted, 9),
        text(ux, top + blocks.length * 54 + 24, "device alignment)", palette.muted, 9),
      );

      root.querySelector('[data-fig="arena"]').append(
        fig.figure(
          [node],
          "The seven instance arenas and the demo page's own four blocks, " +
            "in draw order: selection (under-rects), text, search (chips, in " +
            "chip mode), caret (over-rects) — the arenas each block fills " +
            "match faf-text-web's sync_blocks; span widths are schematic. " +
            "Blocks own a Span per arena and one 256-byte-stride slot in the " +
            "dynamic-offset uniform buffer, of which the 80-byte BlockUniform " +
            "is used. Editing the text dirties only the dashed spans: finish " +
            "issues one write_buffer per non-empty span of that block, and " +
            "the other blocks upload nothing.",
        ),
      );
    }

    // --- Gory details: the scene machinery -------------------------------
    root.querySelector('[data-fig="scene-details"]').append(
      fig.details(
        "Gory details: allocator, repack, and the matrix that is not applied",
        `<p>The arena allocator (<code>arena.rs</code>) is a bump pointer
         plus a free list of released spans, and it never coalesces:
         allocations carry 25% slack rounded up to a multiple of eight
         (<code>plan_cap</code>), so a caret blink or a typed character
         reuses its span in place, and once the free list holds more than
         half the arena (<code>REPACK_FRAGMENTATION&nbsp;=&nbsp;0.5</code>)
         the whole arena is repacked — every live span copied back
         contiguously, O(live instances), one upload. The CPU mirror is a
         <code>Vec&lt;u32&gt;</code> rather than <code>Vec&lt;u8&gt;</code>
         because a byte vector is only 1-aligned and
         <code>bytemuck::cast_slice</code> may legitimately panic on it.</p>
         <p>Block uniforms sit in one buffer bound at a dynamic offset
         because that is WebGL2-clean where storage buffers are not; the
         stride is the device's minimum uniform alignment, floored at 256
         bytes so blocks cost the same everywhere. The draw loop binds the
         same bind group with a different offset per block — no bind-group
         churn, no per-block allocation.</p>
         <p>The default view-projection is <code>None</code>, and that is a
         <em>symbolic</em> identity, not a numeric one: <code>compose</code>
         returns the model matrix untouched rather than multiplying by a
         matrix that is only numerically the identity. Multiplying by the
         numeric identity is not a no-op in floats — it reassociates the
         arithmetic — and the whole point of the homogeneous-pixel-space
         convention is that a 2D block's vertex math never changes shape.
         When a host does set a projection, <code>finish</code> re-derives
         every block's uniform on resize (the fold-in depends on the surface
         size); with the default, a resize touches nothing but the 16-byte
         globals write, which is also the only thing a resize writes at
         all.</p>`,
      ),
    );

    // --- Figure: live damage proof (attaches last) -----------------------
    const liveHost = root.querySelector('[data-fig="live"]');
    const canvas = document.createElement("canvas");
    let demo = null;
    let timer = null;
    const initialText =
      "This canvas is idle. Idle means: zero bytes uploaded, zero draw calls, nothing presented.";
    const phrase = " Typing damages one block; only its spans re-upload.";
    const btnStyle =
      `background: ${palette.bgDark}; color: ${palette.text}; ` +
      `border: 1px solid ${palette.border}; ` +
      "border-radius: 6px; padding: 4px 14px; cursor: pointer; font: inherit;";
    const typeBtn = fig.el(
      "button",
      {
        style: btnStyle,
        onclick: () => {
          if (!demo || timer) return;
          demo.key_input("End", true, false); // caret to the end of the text
          let i = 0;
          timer = setInterval(() => {
            if (i >= phrase.length) {
              clearInterval(timer);
              timer = null;
              return;
            }
            demo.insert_text(phrase[i++]);
          }, 45);
        },
      },
      "type",
    );
    const resetBtn = fig.el(
      "button",
      {
        style: btnStyle,
        onclick: () => {
          if (!demo) return;
          if (timer) {
            clearInterval(timer);
            timer = null;
          }
          demo.set_text(initialText);
        },
      },
      "reset",
    );
    liveHost.append(
      fig.figure(
        [canvas, fig.controls(typeBtn, resetBtn)],
        "Damage tracking, observable. The stats chip counts frames actually " +
          "presented, so it reads “idle” while nothing changes — the page's " +
          "rAF loop calls render() every frame, and render() does nothing. " +
          "Press “type”: each inserted character dirties the text block and " +
          "the caret block's over-rect, finish re-uploads those spans, one " +
          "frame presents, and the chip wakes to report it. When the typing " +
          "stops, it falls back to idle.",
      ),
    );

    demo = await ctx.attachDemo(canvas, {
      height: 150,
      fontSize: 19,
      text: initialText,
      stats: true,
      caret: true,
    });
    // No tick: the shared rAF loop calls render(), which presents nothing
    // while the scene is undamaged — that is the point of the figure.
    ctx.animate(canvas, demo);
  },
};
