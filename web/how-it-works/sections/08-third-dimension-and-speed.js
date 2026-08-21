// Section 08: 3D placement, the terminal grid, and the benchmark scorecard.
//
// Sources mirrored here: crates/faf-text/src/shaders.wgsl (vector_fs,
// curve_winding, project), src/math.rs (screen_perspective, pointer_ndc,
// ndc_ray, block_hit), src/grid.rs (GridFont::new, TermGrid::build,
// box_drawing, emit_rail, char_width), examples/bench.rs, examples/term.rs.
// Every measured number cites where it was taken.

export default {
  slug: "third-dimension-and-speed",
  title: "The third dimension, the terminal, and the scorecard",

  html: `
    <p>Everything so far happened on a flat page. This section is about the
    three claims the project ends on: that the antialiasing survives
    perspective <em>with zero algorithm change</em>, that a terminal grid can
    skip the shaper entirely, and that the whole thing holds up against the
    paper it set out to match.</p>

    <h3>Antialiasing that survives perspective</h3>

    <p>The fragment shader never finds out the block was transformed. The
    vertex shader hands it <code>em</code> — the sample's position in the
    glyph's own em coordinates — as an ordinary varying, and
    <code>project()</code> in <code>shaders.wgsl</code> carries the
    homogeneous <code>w</code> through to clip space without dividing, so the
    GPU interpolates <code>em</code> perspective-correct across the quad. The
    first two lines of <code>vector_fs</code> are then
    <code>let fw = fwidth(in.em); let inv_diameter = 1.0 / fw;</code>.
    <code>fwidth</code> is a screen-space derivative: how much the
    interpolated value changes from one pixel to its neighbors, per screen
    axis. So <code>fw.x</code> is <em>em per pixel</em> along the screen's x
    axis, and <code>inv_diameter.x</code> is exactly <em>pixels per em</em>
    along that axis — measured at the fragment, after whatever matrix chain
    put it there.</p>

    <p>That constant is the whole antialiasing story. A ray crossing
    contributes <code>clamp(x&nbsp;·&nbsp;inv_diameter&nbsp;+&nbsp;0.5,
    0,&nbsp;1)</code> (<code>curve_winding</code>): <code>x</code> is the em
    distance from sample to crossing, and multiplying by pixels-per-em turns
    it into screen pixels, so the coverage ramp is one pixel wide <em>on
    screen</em> by construction — head-on, tilted, or at a grazing angle
    where one axis is compressed five-fold and the other barely at all.
    Anisotropy comes free because the two ray directions each use their own
    axis: horizontal rays divide by <code>fw.x</code>, vertical rays by
    <code>fw.y</code>. Even the small-size supersampling gate reads
    <code>max(fw.x, fw.y) &gt; 1/24</code>, so a foreshortened pane turns the
    3-tap path on exactly where its compressed axis drops below 24 px/em.
    This is why the roadmap called 3D "plumbing, not an algorithm change":
    the coverage math was projective-invariant from the first commit.</p>

    <div data-fig="tilt"></div>
    <div data-fig="fwidth"></div>

    <p>The plumbing itself is two design choices. First, a block's matrix
    maps block pixels to <em>homogeneous screen pixels</em>, not to clip
    space — the shader keeps its original <code>px / screen · 2 − w</code>
    arithmetic, so a 2D block's translation-only matrix adds exactly and the
    flat page renders byte-for-byte as it did before matrices existed.
    Second, <code>math::screen_perspective</code> places the eye at
    <code>(w/2, h/2, −h/2·cot(fov/2))</code> looking down +z, which makes the
    perspective camera agree with the 2D orthographic projection <em>exactly</em>
    at z&nbsp;=&nbsp;0: turning 3D on never moves flat content. Blocks
    composite painter-style — sorted by <code>(z, block&nbsp;id)</code> on
    the CPU, alpha-blended, <code>depth_stencil: None</code> in every
    pipeline — because antialiased text edges and depth writes are enemies:
    a depth test would clip the translucent fringe of whatever draws first.</p>

    <p>Pointing at a tilted pane runs the projection backwards:
    <code>pointer_ndc</code> flips the pointer into NDC,
    <code>ndc_ray</code> unprojects the near- and far-plane points through
    the inverted view-projection into a world ray, and
    <code>block_hit</code> intersects that ray with the block's own
    z&nbsp;=&nbsp;0 plane — rejecting edge-on, back-facing, and
    behind-the-eye hits — and hands back <em>block-local pixels</em>. From
    there the ordinary 2D <code>TextView::hit</code> takes over, so
    selection, caret motion and double-click word selection work identically
    on a pane at 72°. The arithmetic inside is f64: in f32 the round trip
    through an inverted perspective missed by 0.012&nbsp;px, more than the
    0.01&nbsp;px the tests demand.</p>

    <div data-fig="matrix-details"></div>

    <h3>The terminal: no shaper in the loop</h3>

    <p>A terminal knows where every glyph goes before it asks anyone: cells
    are fixed, the font is monospace, and the content churns every frame.
    So <code>grid.rs</code> never shapes. A char becomes a glyph id through
    the face's own charmap (<code>swash</code>'s
    <code>charmap().map(ch)</code>), cached per face in
    <code>GridFont</code>; a frame is a pure CPU pass
    (<code>TermGrid::build</code>) that turns the viewport into merged
    background rects, <code>GlyphSpec</code>s, and decoration runs. Only
    three things take a detour: East Asian Width <code>W</code>/<code>F</code>
    characters occupy two cells (123 merged ranges from
    EastAsianWidth-17.0.0, binary-searched in <code>char_width</code>, with
    everything below U+1100 skipping the search); charmap misses and emoji
    go through a one-cell shaped-and-cached fallback lane so combining marks
    and color fonts keep working; and box drawing never touches the font at
    all.</p>

    <p>Measured with <code>cargo run --release --example term</code> on this
    project's dev box: translating a 200×60 grid — 12,000 cells of colored
    log — into 4,374 instances (60 merged background rects, 4,293 glyphs,
    21 decorations) takes <strong>0.118&nbsp;ms</strong> mean CPU
    (p50&nbsp;0.116, p95&nbsp;0.134, n&nbsp;=&nbsp;500; run-to-run means
    wander 0.11–0.16&nbsp;ms with ordinary CPU-clock noise). The instances
    land in one <a href="#geometry-and-damage">retained block</a>; every
    other block in the scene uploads nothing.</p>

    <div data-fig="term"></div>

    <h3>Box drawing as constructed geometry</h3>

    <p>Cell metrics are rounded to whole pixels on purpose:
    <code>GridFont::new</code> takes the advance of <code>M</code> and
    ascent&nbsp;+&nbsp;descent&nbsp;+&nbsp;leading, and rounds both. Box
    drawing (<code>box_drawing</code> in <code>grid.rs</code>) then centers
    a stroke of thickness <code>t</code> at
    <code>round((cell&nbsp;−&nbsp;t)&nbsp;/&nbsp;2)</code> — and because
    every cell is the same whole-pixel size, every neighbor computes the
    same offset and a join has no seam. A light stroke is
    <code>round(h/8)</code> px; heavy is twice that; and each arm runs from
    the cell's center region all the way to its edge, so adjacent cells
    share the boundary exactly.</p>

    <p>Double lines fall out of one rule. A double arm is two light rails at
    ±light around where the single bar would sit, and <em>a rail's middle
    span is omitted exactly where a double perpendicular arm crosses it</em>
    — a single perpendicular arm never breaks a rail, it just runs into it.
    That single rule derives the whole double-line repertoire: ╬ gets four
    corner pieces (all four middles omitted), ╠ keeps a continuous left rail
    (nothing double crosses it) while its right rail breaks, and ╤ keeps
    both horizontal rails intact — its stem is single — with the stem
    starting at the <em>lower</em> rail because there is no up arm to reach
    for.</p>

    <div data-fig="box"></div>

    <h3>The scorecard</h3>

    <p><code>examples/bench.rs</code> replicates the benchmark from Lengyel
    2017 (JCGT vol.&nbsp;6 no.&nbsp;2, Table&nbsp;2 — the Slug paper): fill
    a two-megapixel target (1448², 2.10&nbsp;MP) with 50 lines of text at
    32&nbsp;px/em, ~4,200 glyphs, and time steady-state rendering. The
    trajectory on this repo's RTX&nbsp;3070 (Vulkan, release,
    submit-and-wait over 300 frames): <strong>1.07&nbsp;ms</strong> with the
    flat per-glyph curve loop, <strong>0.64&nbsp;ms</strong> after band
    tables (issue #3, 1.67×), <strong>0.54&nbsp;ms</strong> after sorted
    band lists with early-out and the median band split (issue #12, another
    1.20×). Two later changes deliberately did <em>not</em> move this
    number: RGBA16F curve storage (#13) halved the fetch bytes for a 2.6×
    memory win and a 0.7%-of-noise frame delta, and corner clipping (#14)
    cuts fragment invocations 5.3% at 64&nbsp;px/em but gates itself off
    below 48&nbsp;px/em, so the 32&nbsp;px workload is untouched.</p>

    <p>Normalized by raw throughput to the paper's GTX&nbsp;1060 (a ~4×
    scalar), 0.54&nbsp;ms lands at ≈2.2&nbsp;ms against Slug's published
    1.1–1.3&nbsp;ms — within about 2× of a shipping commercial
    implementation. What is genuinely the same: the robustness machinery.
    Ray crossings are classified by control-point sign patterns (the
    equivalence-class bit tables, masks <code>0x454</code>/<code>0x1510</code>),
    independently derived here after root-range checks mis-rendered glyphs
    whose exact 1/64-fraction coordinates graze ray endpoints. What is
    deliberately different: Slug bakes its curve data offline into
    per-font optimized buffers; faf-text extracts lazily at runtime into a
    growable, LRU-evicted, compactable texture, and encodes everything
    WebGL2-safe — RGBA16F data texture instead of storage buffers, no
    base-instance draws. What was skipped, knowingly: adaptive band counts,
    hierarchical bands and x/y band asymmetry (issue #3 scoped them out —
    Latin glyphs top out near 60 curves, so eight fixed bands per axis
    already leave a band+ray with barely two nonzero terms).</p>

    <div data-fig="bench"></div>
    <div data-fig="bench-details"></div>

    <h3>How this page reaches you</h3>

    <p>The deployment is the last piece of the architecture. The crate ships
    a tiny <code>docs-header.html</code> that docs.rs injects into the API
    docs; it upgrades placeholder <code>.faf-live</code> divs into live wasm
    canvases by importing a driver script from the project's GitHub Pages
    tree — so demo behavior (the frame-rate overlay, the WebGL2 fallback
    handshake) updates with a <code>gh-pages</code> push, no crate release
    required. That same tree, assembled by <code>scripts/build-site.sh</code>,
    serves the landing page, the demo bundle, and this very page: the
    section you are reading resolves its wasm at <code>../pkg/</code> when
    served from a checkout and <code>../demo/pkg/</code> when deployed, and
    every figure above ran on whichever it found.</p>
  `,

  async mount(root, ctx) {
    const { fig, palette } = ctx;

    // ---- Small matrix kit (mirrors glam column-major, math.rs semantics) --
    const mat = {
      mul(a, b) {
        const o = new Array(16).fill(0);
        for (let c = 0; c < 4; c++)
          for (let r = 0; r < 4; r++)
            for (let k = 0; k < 4; k++) o[c * 4 + r] += a[k * 4 + r] * b[c * 4 + k];
        return o;
      },
      vec(m, v) {
        const o = [0, 0, 0, 0];
        for (let r = 0; r < 4; r++)
          for (let k = 0; k < 4; k++) o[r] += m[k * 4 + r] * v[k];
        return o;
      },
      identity: () => [1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1],
    };

    // math::screen_perspective, line for line: directx-convention LH
    // perspective (depth 0..1) times a view that flips y and pushes the
    // origin out to the eye at (w/2, h/2, -distance).
    function screenPerspective([w, h], fovY) {
      const focal = 1 / Math.tan(fovY * 0.5);
      const distance = h * 0.5 * focal;
      const [near, far] = [distance * 0.01, distance * 100];
      const proj = new Array(16).fill(0);
      proj[0] = focal / (w / h);
      proj[5] = focal;
      proj[10] = far / (far - near);
      proj[11] = 1;
      proj[14] = (-near * far) / (far - near);
      const view = mat.identity();
      view[5] = -1;
      view[12] = -w * 0.5;
      view[13] = h * 0.5;
      view[14] = distance;
      return mat.mul(proj, view);
    }

    function paneModel([w, h], degrees) {
      const t = (degrees * Math.PI) / 180;
      const [c, s] = [Math.cos(t), Math.sin(t)];
      // T(center) · rotation_y(t) · T(-center), glam column layout.
      const rot = mat.identity();
      rot[0] = c;
      rot[2] = -s;
      rot[8] = s;
      rot[10] = c;
      const toC = mat.identity();
      toC[12] = w * 0.5;
      toC[13] = h * 0.5;
      const fromC = mat.identity();
      fromC[12] = -w * 0.5;
      fromC[13] = -h * 0.5;
      return mat.mul(toC, mat.mul(rot, fromC));
    }

    // ---- Figure A (live): the tilt demo ---------------------------------
    // DOM goes in now; the GPU attach happens at the end of mount.
    const tiltCanvas = document.createElement("canvas");
    let tiltDemo = null;
    const tiltSlider = fig.slider({
      label: "tilt",
      min: -60,
      max: 60,
      step: 1,
      value: 40,
      format: (v) => `${v}°`,
      oninput: (v) => tiltDemo?.set_tilt(v === 0 ? undefined : v),
    });
    root.querySelector('[data-fig="tilt"]').append(
      fig.figure(
        [tiltCanvas, fig.controls(tiltSlider.root)],
        "The renderer under a perspective tilt, live. No glyph re-rasterizes " +
          "and no instance rebuilds — the tilt is one matrix per block per " +
          "frame. The fragment shader takes fwidth(em) where each fragment " +
          "lands, so 1/fwidth is pixels-per-em along each screen axis of the " +
          "foreshortened pane, and the coverage ramp stays exactly one screen " +
          "pixel wide at every angle. Where the compressed axis drops below " +
          "24 px/em, the same measurement turns on 3-tap supersampling, only " +
          "there. The stats chip is drawn by the renderer itself.",
      ),
    );

    // ---- Figure B: one pixel's footprint in em space --------------------
    {
      const S = [300, 200]; // the mini-scene's screen, in px
      const EM_PX = 24; // the pane's glyphs: 24 px/em head-on
      const vp = screenPerspective(S, 0.7);
      const local = [90, 100]; // sample point, pane-local px
      const W = 720;
      const H = 330;
      const node = fig.svg(W, H);
      const scene = fig.svgEl("g", {});
      node.append(scene);

      const screenOf = (model, [x, y]) => {
        const clip = mat.vec(vp, mat.vec(model, [x, y, 0, 1]));
        const ndc = [clip[0] / clip[3], clip[1] / clip[3]];
        return [((ndc[0] + 1) / 2) * S[0], ((1 - ndc[1]) / 2) * S[1]];
      };

      const draw = (degrees) => {
        scene.replaceChildren();
        const model = paneModel(S, degrees);
        // The near half of a strongly tilted pane projects far outside its
        // panel, so the panel clips to its own box.
        const clip = fig.svgEl("clipPath", { id: "hiw08-fw-clip" },
          fig.svgEl("rect", { x: -24, y: -58, width: 384, height: H - 58 }));
        const L = fig.svgEl("g", { transform: "translate(24 58)", "clip-path": "url(#hiw08-fw-clip)" });
        const R = fig.svgEl("g", { transform: "translate(408 58)" });
        scene.append(clip, L, R);
        const label = (g, x, y, text, color = palette.textDim, size = 12, anchor = "start") =>
          g.append(
            fig.svgEl(
              "text",
              { x, y, fill: color, "font-size": size, "text-anchor": anchor, "vector-effect": null },
              text,
            ),
          );

        label(L, 0, -34, "screen space", palette.text, 13);
        label(L, 0, -18, `pane tilted ${degrees}° about its vertical center`, palette.muted, 11);
        label(R, 0, -34, "em space on the pane", palette.text, 13);
        label(R, 0, -18, "the same pixel, back-projected", palette.muted, 11);

        // Left: the projected pane and its foreshortened rulings.
        const outline = [[0, 0], [S[0], 0], S, [0, S[1]]].map((p) => screenOf(model, p));
        L.append(
          fig.svgEl("polygon", {
            points: outline.map((p) => p.join(",")).join(" "),
            fill: palette.blue,
            "fill-opacity": 0.08,
            stroke: palette.blue,
            "stroke-width": 1.5,
          }),
        );
        for (const fx of [0.25, 0.5, 0.75]) {
          const a = screenOf(model, [fx * S[0], 0]);
          const b = screenOf(model, [fx * S[0], S[1]]);
          L.append(
            fig.svgEl("line", {
              x1: a[0], y1: a[1], x2: b[0], y2: b[1],
              stroke: palette.blue, "stroke-opacity": 0.35, "stroke-width": 1,
            }),
          );
        }
        const sp = screenOf(model, local);
        L.append(
          fig.svgEl("circle", { cx: sp[0], cy: sp[1], r: 4, fill: palette.orange }),
          fig.svgEl("rect", {
            x: sp[0] - 0.5, y: sp[1] - 0.5, width: 1, height: 1,
            fill: "none", stroke: palette.orange, "stroke-width": 1,
          }),
        );
        label(L, sp[0] + 8, sp[1] + 4, "one pixel", palette.orange, 11);

        // Jacobian of local→screen at the sample, by central differences.
        const h = 0.01;
        const dx = screenOf(model, [local[0] + h, local[1]]).map(
          (v, i) => (v - screenOf(model, [local[0] - h, local[1]])[i]) / (2 * h),
        );
        const dy = screenOf(model, [local[0], local[1] + h]).map(
          (v, i) => (v - screenOf(model, [local[0], local[1] - h])[i]) / (2 * h),
        );
        // M = J⁻¹ maps screen deltas back to pane-local deltas.
        const det = dx[0] * dy[1] - dy[0] * dx[1];
        const M = [dy[1] / det, -dy[0] / det, -dx[1] / det, dx[0] / det]; // [m00 m01; m10 m11]
        // dpdx(em), dpdy(em): the pane-local (hence em) motion caused by one
        // screen pixel of x and of y. fwidth(em) = |dpdx| + |dpdy| per axis —
        // exactly the bounding-box extents of the pixel's footprint.
        const ex = [M[0] / EM_PX, M[2] / EM_PX];
        const ey = [M[1] / EM_PX, M[3] / EM_PX];
        const fw = [Math.abs(ex[0]) + Math.abs(ey[0]), Math.abs(ex[1]) + Math.abs(ey[1])];

        // Right: the footprint parallelogram, its fwidth bounding box, and
        // the head-on reference square (side 1/24 em).
        const k = Math.min(150 / Math.max(fw[0], fw[1]), 150 * EM_PX); // display px per em
        const c = [150, 100];
        const P = [
          [+0.5, +0.5], [-0.5, +0.5], [-0.5, -0.5], [+0.5, -0.5],
        ].map(([a, b]) => [
          c[0] + (a * ex[0] + b * ey[0]) * k,
          c[1] - (a * ex[1] + b * ey[1]) * k, // em y is up
        ]);
        const ref = (1 / EM_PX) * k;
        R.append(
          fig.svgEl("rect", {
            x: c[0] - (fw[0] * k) / 2, y: c[1] - (fw[1] * k) / 2,
            width: fw[0] * k, height: fw[1] * k,
            fill: "none", stroke: palette.red, "stroke-width": 1, "stroke-dasharray": "3 3",
          }),
          fig.svgEl("polygon", {
            points: P.map((p) => p.join(",")).join(" "),
            fill: palette.orange, "fill-opacity": 0.3,
            stroke: palette.orange, "stroke-width": 1.5,
          }),
          fig.svgEl("rect", {
            x: c[0] - ref / 2, y: c[1] - ref / 2, width: ref, height: ref,
            fill: "none", stroke: palette.teal, "stroke-width": 1, "stroke-dasharray": "5 3",
          }),
        );
        label(R, c[0], c[1] - (fw[1] * k) / 2 - 8, `fwidth(em).x = ${fw[0].toFixed(4)} em`, palette.red, 11, "middle");
        label(R, c[0] + (fw[0] * k) / 2 + 6, c[1] + 4, `fw.y = ${fw[1].toFixed(4)}`, palette.red, 11);
        label(R, c[0] - ref / 2, c[1] + (Math.max(fw[1] * k, ref)) / 2 + 16, `teal: head-on footprint (1/${EM_PX} em)`, palette.teal, 11);
        // Pinned to the svg's own bottom edge, clear of the panels at any tilt.
        label(scene, 24, H - 12, `coverage per crossing: clamp(x · inv_diameter + 0.5, 0, 1)`, palette.muted, 11);
        label(scene, 408, H - 12, `inv_diameter = 1/fwidth = (${(1 / fw[0]).toFixed(1)}, ${(1 / fw[1]).toFixed(1)}) px/em`, palette.text, 12);
      };

      const bSlider = fig.slider({
        label: "tilt",
        min: 0,
        max: 85,
        step: 1,
        value: 55,
        format: (v) => `${v}°`,
        oninput: draw,
      });
      draw(55);
      root.querySelector('[data-fig="fwidth"]').append(
        fig.figure(
          [node, fig.controls(bSlider.root)],
          "One screen pixel, back-projected onto a tilted pane. Left: the " +
            "pane under math::screen_perspective (mirrored here matrix for " +
            "matrix), with a sample pixel marked. Right: that pixel's " +
            "preimage in the glyph's em space — a parallelogram, because the " +
            "projection is anisotropic. The dashed red box is its per-axis " +
            "extent, which is precisely fwidth(em) = |dpdx| + |dpdy|: the " +
            "number vector_fs divides by. Its reciprocal is px-per-em along " +
            "each screen axis, so a coverage ramp of width 1/inv_diameter in " +
            "em is exactly one pixel on screen, at any tilt.",
        ),
      );
    }

    // ---- Figure C: box-drawing rail derivations -------------------------
    {
      // Mirror of grid.rs box_drawing's double-line branch, at one large
      // cell. Same arithmetic, same names: light = round(h/8), a bar of
      // thickness t sits at round((cell − t)/2), rails at ±light.
      const CELL = 120;
      const armsOf = { "╬": ["D", "D", "D", "D"], "╠": ["D", "D", "D", "N"], "╤": ["N", "D", "L", "D"] };
      const derive = (arms) => {
        const [x, y, w, h] = [0, 0, CELL, CELL];
        const light = Math.max(Math.round(h / 8), 1);
        const cx = (t) => x + Math.round((w - t) / 2);
        const cy = (t) => y + Math.round((h - t) / 2);
        const [cx0, cy0] = [cx(light), cy(light)];
        const [nearX, farX] = [cx0 - light, cx0 + light];
        const [nearY, farY] = [cy0 - light, cy0 + light];
        const midX = [nearX, farX + light];
        const midY = [nearY, farY + light];
        const [UP, RIGHT, DOWN, LEFT] = [0, 1, 2, 3];
        const rects = [];
        const omitted = [];
        const merge = (spans) => {
          const out = [];
          let cur = null;
          for (const [s, e] of spans) {
            if (e <= s) continue;
            if (cur && s <= cur[1]) cur[1] = Math.max(cur[1], e);
            else { if (cur) out.push(cur); cur = [s, e]; }
          }
          if (cur) out.push(cur);
          return out;
        };
        // What the rule removed: rail coverage with the middle kept, minus
        // coverage with it omitted.
        const removed = (fullSpans, keptSpans) => {
          const out = [];
          for (const [s, e] of merge(fullSpans)) {
            let at = s;
            for (const [ks, ke] of merge(keptSpans)) {
              if (ke <= at || ks >= e) continue;
              if (ks > at) out.push([at, ks]);
              at = Math.max(at, ke);
            }
            if (at < e) out.push([at, e]);
          }
          return out;
        };
        const emit = (spans, off, horiz, into = rects) => {
          for (const [s, e] of spans)
            into.push(horiz ? [s, off, e - s, light] : [off, s, light, e - s]);
        };
        const span = (present, s) => (present ? [...s] : [0, 0]);

        if (arms[UP] === "D" || arms[DOWN] === "D") {
          for (const [railX, crossed] of [[nearX, arms[LEFT] === "D"], [farX, arms[RIGHT] === "D"]]) {
            const parts = (mid) => [
              span(arms[UP] !== "N", [y, cy0]), span(mid, midY), span(arms[DOWN] !== "N", [farY, y + h]),
            ];
            emit(merge(parts(!crossed)), railX, false);
            if (crossed) emit(removed(parts(true), parts(false)), railX, false, omitted);
          }
        } else if (arms[UP] !== "N" || arms[DOWN] !== "N") {
          // Single stem beside a double crossbar: ╤'s stem starts at the
          // *lower* rail because there is no up arm.
          emit([[arms[UP] !== "N" ? y : farY, arms[DOWN] !== "N" ? y + h : cy0]], cx(light), false);
        }
        if (arms[RIGHT] === "D" || arms[LEFT] === "D") {
          for (const [railY, crossed] of [[nearY, arms[UP] === "D"], [farY, arms[DOWN] === "D"]]) {
            const parts = (mid) => [
              span(arms[LEFT] !== "N", [x, cx0]), span(mid, midX), span(arms[RIGHT] !== "N", [farX, x + w]),
            ];
            emit(merge(parts(!crossed)), railY, true);
            if (crossed) emit(removed(parts(true), parts(false)), railY, true, omitted);
          }
        }
        return { rects, omitted, light };
      };

      const row = fig.el("div", { style: "display:flex;flex-wrap:wrap;gap:18px;justify-content:center;" });
      const cases = [
        ["╬", "four double arms: every rail middle is crossed by a double arm and omitted — four corner pieces remain"],
        ["╠", "left arm absent: nothing double crosses the left rail, so its middle survives — one continuous rail"],
        ["╤", "the down arm is single: it breaks nothing, both rails run full width, and the stem starts at the lower rail"],
      ];
      for (const [ch, why] of cases) {
        const { rects, omitted } = derive(armsOf[ch]);
        const s = fig.svg(180, 200);
        const g = fig.svgEl("g", { transform: "translate(30 16)" });
        s.append(g);
        g.append(
          fig.svgEl("rect", {
            x: 0, y: 0, width: CELL, height: CELL,
            fill: "none", stroke: palette.muted, "stroke-width": 1, "stroke-dasharray": "3 4",
          }),
        );
        for (const [rx, ry, rw, rh] of rects)
          g.append(fig.svgEl("rect", { x: rx, y: ry, width: rw, height: rh, fill: palette.blue, "fill-opacity": 0.85 }));
        for (const [rx, ry, rw, rh] of omitted)
          g.append(
            fig.svgEl("rect", {
              x: rx, y: ry, width: rw, height: rh,
              fill: "none",
              stroke: palette.red, "stroke-width": 1, "stroke-dasharray": "3 3",
            }),
          );
        g.append(
          fig.svgEl(
            "text",
            { x: CELL / 2, y: CELL + 28, fill: palette.text, "font-size": 22, "text-anchor": "middle", "vector-effect": null },
            ch,
          ),
        );
        const cell = fig.el("div", { style: "max-width:200px;" }, s);
        cell.append(fig.el("p", { class: "hiw-caption", style: "margin-top:0;" }, why));
        row.append(cell);
      }
      root.querySelector('[data-fig="box"]').append(
        fig.figure(
          [row],
          "The double-line rule, constructed with box_drawing's own " +
            "arithmetic (grid.rs) at a 120 px cell: light = round(h/8) = 15, " +
            "rails at ±light around the centered bar. Blue: emitted rects. " +
            "Dashed red: the rail middle that the rule omits because a " +
            "double perpendicular arm crosses there. Every arm runs to the " +
            "cell edge, and cells are whole pixels, so neighbors join " +
            "seamlessly at any size.",
        ),
      );
    }

    // ---- Figure E: the benchmark trajectory ------------------------------
    {
      const W = 720, H = 310;
      const node = fig.svg(W, H);
      // Log scale: the story is multiplicative (1.67×, 1.20×, a ~4× GPU
      // gap), so equal steps should read as equal factors.
      const x0 = 130, x1 = 690, msLo = 0.42, msHi = 5.2;
      const X = (ms) => x0 + (Math.log(ms / msLo) / Math.log(msHi / msLo)) * (x1 - x0);
      const laneA = 95, laneB = 205;
      const axisY = 258;

      for (const ms of [0.5, 1, 2, 4]) {
        node.append(
          fig.svgEl("line", { x1: X(ms), y1: 48, x2: X(ms), y2: axisY, stroke: palette.border, "stroke-width": 1 }),
          fig.svgEl("text", { x: X(ms), y: axisY + 16, fill: palette.muted, "font-size": 11, "text-anchor": "middle", "vector-effect": null }, `${ms}`),
        );
      }
      node.append(
        fig.svgEl("text", { x: x1, y: axisY + 34, fill: palette.muted, "font-size": 11, "text-anchor": "end", "vector-effect": null }, "ms / frame (log scale)"),
        fig.svgEl("text", { x: 18, y: laneA - 4, fill: palette.textDim, "font-size": 12, "vector-effect": null }, "RTX 3070"),
        fig.svgEl("text", { x: 18, y: laneA + 12, fill: palette.muted, "font-size": 11, "vector-effect": null }, "measured"),
        fig.svgEl("text", { x: 18, y: laneB - 4, fill: palette.textDim, "font-size": 12, "vector-effect": null }, "GTX 1060"),
        fig.svgEl("text", { x: 18, y: laneB + 12, fill: palette.muted, "font-size": 11, "vector-effect": null }, "normalized ×4"),
      );

      // The paper's band, on the normalized lane only.
      node.append(
        fig.svgEl("rect", {
          x: X(1.1), y: laneB - 26, width: X(1.3) - X(1.1), height: 52,
          fill: palette.orange, "fill-opacity": 0.22,
          stroke: palette.orange, "stroke-width": 1, "stroke-dasharray": "4 3",
        }),
        fig.svgEl("text", { x: X(1.2), y: laneB - 34, fill: palette.orange, "font-size": 11, "text-anchor": "middle", "vector-effect": null }, "Slug, GTX 1060 (Table 2): 1.1–1.3 ms"),
      );

      const steps = [
        ["flat curve loop", 1.069, 4.3],
        ["+ band tables (#3)", 0.639, 2.6],
        ["+ sorted, median split (#12)", 0.544, 2.2],
      ];
      // Per-point label offsets, tuned so nothing collides: [dy, anchor].
      const lane = (y, vals, offs) => {
        for (let i = 0; i + 1 < vals.length; i++)
          node.append(
            fig.svgEl("line", {
              x1: X(vals[i][1]), y1: y, x2: X(vals[i + 1][1]), y2: y,
              stroke: palette.blue, "stroke-width": 2, "stroke-opacity": 0.55,
            }),
          );
        vals.forEach(([name, ms], i) => {
          const title = fig.svgEl("title", {}, `${name}: ${ms} ms/frame`);
          const dot = fig.svgEl("circle", { cx: X(ms), cy: y, r: 5.5, fill: palette.blue, stroke: palette.bgDark, "stroke-width": 2 });
          dot.append(title);
          const [dy, anchor] = offs[i];
          node.append(
            dot,
            fig.svgEl("text", {
              x: X(ms), y: y + dy, fill: palette.textDim, "font-size": 11,
              "text-anchor": anchor, "vector-effect": null,
            }, `${name}  ${ms.toFixed(2)}`),
          );
        });
      };
      lane(laneA, steps.map(([n, m]) => [n, m]), [[-16, "middle"], [24, "middle"], [-16, "middle"]]);
      lane(laneB, steps.map(([n, , g]) => [n, g]), [[-16, "middle"], [40, "middle"], [24, "middle"]]);

      root.querySelector('[data-fig="bench"]').append(
        fig.figure(
          [node],
          "The Lengyel-2017 Table-2 workload (examples/bench.rs: 2.10 MP, " +
            "50 lines at 32 px/em, ~4,200 glyphs), across the optimization " +
            "issues. Top lane: measured on this repo's RTX 3070 (Vulkan, " +
            "release, submit-and-wait over 300 frames; numbers from the " +
            "issue #3 and #12 records). Bottom lane: the same numbers scaled " +
            "by the ~4× raw-throughput gap to the paper's GTX 1060, against " +
            "the paper's published 1.1–1.3 ms band — within about 2× of the " +
            "shipping implementation, extracted lazily at runtime.",
        ),
        fig.el(
          "ul",
          { style: `color:${palette.muted};font-size:0.85rem;line-height:1.5;margin:0.4rem 0 0 1.2rem;` },
          fig.el("li", {}, "The GPU normalization is one scalar (raw FP32 throughput). It ignores bandwidth, cache, clocks and drivers — read it as “same order of magnitude,” not a ratio with two significant digits."),
          fig.el("li", {}, "Timing is wall-clock submit-and-wait per frame, not GPU timestamp queries, so it includes command re-record overhead (small at 2 MP)."),
          fig.el("li", {}, "The workload matches the paper's shape — area, line count, px/em — not its exact scene or typeface (DejaVu Sans here)."),
          fig.el("li", {}, "The adversarial case is dense 11 px text (examples/stress): ~1.9 ms for a 960×600 frame of 7,884 glyphs, with ±0.2 ms run-to-run noise."),
        ),
      );

      root.querySelector('[data-fig="bench-details"]').append(
        fig.details(
          "Gory details: the wrong ways that were measured, not guessed",
          `<p>Three results from the #12 record that only measurement would
           find. <strong>Backward rays fire toward the <em>near</em> edge of
           the glyph.</strong> A forward ray counts crossings ahead of the
           sample and stops at the first curve behind it — cheap on the high
           side of a band, dear on the low side — so samples below the median
           split must fire backwards. Inverting that comparison still renders
           correctly (the directions are mathematically equal) and costs 45%:
           bench went 0.63&nbsp;→&nbsp;0.85&nbsp;ms instead of →&nbsp;0.59.
           Correct-but-slow is the failure mode to watch for.</p>
           <p><strong>Direction is chosen in data, never control flow.</strong>
           A negative inv_diameter turns saturate(0.5&nbsp;+&nbsp;m·Cx) into
           saturate(0.5&nbsp;−&nbsp;m·Cx), and negating the band's sum undoes
           the crossing-sign swap — one loop, no branch. Two copies of the
           loop behind an <code>if</code> would run both per warp, since
           neighboring fragments straddle the split constantly.</p>
           <p><strong>The early-out's granularity is size-dependent, worth
           7–8% each way.</strong> Testing the break after every curve makes
           each fetch wait on the previous compare: a loss at 11&nbsp;px
           (stress 1.87&nbsp;→&nbsp;2.0&nbsp;ms), a win at 32&nbsp;px (bench
           0.59&nbsp;→&nbsp;0.54). Below the size gate the break is tested
           once per index texel, on the last entry of the four — the list is
           sorted, so that bound is the group's smallest and the test stays
           exactly as conservative.</p>
           <p>And the change that <em>didn't</em> move the needle: halving
           curve-fetch bytes with RGBA16F (#13) shifted bench 0.549&nbsp;→
           0.545&nbsp;ms — the whole scene's curve data is ~107&nbsp;KB and
           already sat in L2. It bought 2.6× memory headroom for WebGL2 and
           mobile, not frames on a 3070.</p>`,
        ),
      );

      root.querySelector('[data-fig="matrix-details"]').append(
        fig.details(
          "Gory details: why the matrix targets pixels, not clip space",
          `<p>The obvious design — make the block matrix map straight to clip
           space — was built and rejected on evidence. It turns the shader's
           <code>px / screen · 2 − w</code> into <code>px · (2/screen) − w</code>;
           those differ by an ulp for about half of all x values, a vertex
           then lands in a different 1/256 subpixel bin roughly 1% of the
           time, and the reference scene came back with 82 changed pixels
           (max delta 2/255) — visually identical, byte-identical no longer.
           Keeping the divide in <code>project()</code> and folding a host's
           view-projection into <em>pixel</em> space on the CPU makes a 2D
           block's matrix a pure translation, and multiplying by one and
           adding zero are exact in floating point, so the flat page renders
           bit-for-bit as before. Two consequences carry the theme: the
           default projection is <code>None</code> — symbolically the
           identity, never a numerically-identity matrix product — and pixel
           snapping for bitmap emoji is gated on an <code>is_axis_aligned</code>
           flag derived when the matrix is set, because under a tilt there is
           no pixel grid to snap to (slightly soft emoji in 3D is the
           documented trade; vector glyphs never snap — their coverage is
           analytic at any position).</p>
           <p>Hit-testing (math.rs) is the same story run backwards, and its
           tests encode the geometry's honest limits: a pane at 87° round-trips
           near its pivot to 0.01 px, but its far corners are genuinely seen
           from behind — under perspective the eye can be on the back side of
           a plane for part of a pane even when the pivot faces it — and
           <code>block_hit</code> is right to refuse them.</p>`,
        ),
      );
    }

    // ---- Figure D (live): the terminal grid ------------------------------
    const termCanvas = document.createElement("canvas");
    root.querySelector('[data-fig="term"]').append(
      fig.figure(
        [termCanvas],
        "The terminal grid, live: a synthetic colored log streaming through " +
          "TermGrid into one retained block. Every character resolves " +
          "through the charmap cache — no shaper in the loop — backgrounds " +
          "merge into one rect per run, and the header's box drawing is " +
          "procedural rects snapped to whole-pixel cell bounds, which is why " +
          "its joins have no seams. The stats chip is the renderer measuring " +
          "itself.",
      ),
    );

    // ---- Live canvases attach last (CONTRACT.md's mount-ordering rule) ----
    tiltDemo = await ctx.attachDemo(tiltCanvas, {
      height: 200,
      fontSize: 20,
      text:
        "A pane in perspective is still just text.\n" +
        "Coverage is measured where the fragment lands,\n" +
        "so the AA ramp is one screen pixel wide at any angle.",
      stats: true,
    });
    tiltDemo.set_tilt(40);
    ctx.animate(tiltCanvas, tiltDemo);

    const termDemo = await ctx.attachDemo(termCanvas, {
      height: 300,
      fontSize: 13,
      stats: true,
    });
    termDemo.set_terminal(true);
    ctx.animate(termCanvas, termDemo);
  },
};
