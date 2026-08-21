// Section 05 — Memory: the curve texture's texel layout, f16 quantization,
// endpoint sharing, and the stores (growth, LRU compaction, relocation
// patching, the atlas lane, and the flush high-water-mark bug).
//
// Sources of truth: crates/faf-text/src/curves.rs (CurveStore, Flattener,
// banded_block, shared_records, flush), crates/faf-text/src/atlas.rs (Atlas),
// crates/faf-text/src/renderer.rs (begin_frame, relocate_glyph_instances),
// CHANGELOG 0.2.0. Every figure draws numbers from ctx.inspect.

export default {
  slug: "memory-and-caching",
  title: "Memory: texels, halves, and caches that never re-rasterize",

  html: `
    <p>Everything the renderer knows about every outline it has ever drawn
    lives in one GPU texture: 256 texels wide, RGBA16F, starting at 256×2048
    and doubling in height up to 256×8192 — 16&nbsp;MiB, room for roughly two
    million quadratics (<code>CURVE_TEX_WIDTH</code>,
    <code>CURVE_TEX_HEIGHT</code>, <code>CURVE_TEX_MAX_HEIGHT</code> in
    <code>curves.rs</code>). It is a texture and not a storage buffer for one
    blunt reason: WebGL2 has no storage buffers, and this renderer runs the
    same shaders on Vulkan, Metal, DX12, GL, WebGPU and WebGL2. The fragment
    shader reads curves with <code>textureLoad</code> — unfiltered,
    exact-texel fetches — which every one of those backends can do.</p>

    <p>Why <em>half</em>-floats? RGBA16F is half the bytes and half the fetch
    bandwidth of RGBA32F, and — counterintuitively — the <em>better</em>
    supported format of the two on WebGL2: wgpu's GLES backend gives
    <code>Rgba16Float</code> texture binding and filtering with no extension
    at all, where <code>Rgba32Float</code> is sampled-but-unfilterable. The
    honest caveat: halving the fetch bytes did not move frame time on the
    desktop GPU this was measured on (an RTX&nbsp;3070; the benchmark went
    0.549&nbsp;→&nbsp;0.545&nbsp;ms/frame, inside noise) because a whole
    scene's curve data — 107&nbsp;KB — already sat in L2. The f16 move buys a
    2.6× memory reduction and bandwidth headroom for WebGL2 and mobile, not
    desktop frames.</p>

    <h3>Anatomy of a block</h3>

    <p>A glyph owns one contiguous run of texels — its <em>block</em> — and
    one number that says where it starts: <code>GlyphCurves::first</code>,
    the block's base texel. Small glyphs (≤16 curves) store a flat array of
    two-texel records, <code>[p0.xy&nbsp;p1.xy][p2.xy&nbsp;0&nbsp;0]</code>,
    addressed by pure arithmetic (<code>first + 2i</code>). Bigger glyphs get
    the banded layout: a 16-texel header (8 y-bands then 8 x-bands, one texel
    each), then sorted index lists, then the curve records. The load-bearing
    detail is that <strong>every offset stored inside a block is relative to
    the block's base</strong> — header entries point at index lists, index
    entries point at records, all as base-relative texel counts. So when
    compaction needs to move a glyph, it moves the texels and rewrites
    <code>first</code>. One memcpy, one field. Nothing inside the block
    changes.</p>

    <div data-fig="texelmap"></div>

    <h3>Shared endpoints: one texel per curve</h3>

    <p>Outlines are closed contours, and within a contour, curve
    <em>i</em>'s end point <em>is</em> curve <em>i+1</em>'s start point — not
    "equal to", <em>is</em>: the flattener carries the point across, so p2 of
    one record and p0 of the next are the same float. Banded blocks exploit
    that. Each curve stores a single texel,
    <code>[p0.xy&nbsp;p1.xy]</code>, and its p2 is simply the first two lanes
    of the <em>next</em> texel. The shader's fetch doesn't change — it always
    read two adjacent texels — it just stops re-reading data it already had a
    copy of. A contour of n curves costs n+1 texels: n curve texels plus one
    terminator holding the closing p2 (<code>shared_records</code> in
    <code>curves.rs</code>). That's ~1.1 texels per curve instead of 2, and
    together with f16 it took the offscreen test scene's curve data from
    281&nbsp;KB to 107&nbsp;KB.</p>

    <p>Flat blocks deliberately <em>skip</em> sharing. The flat path walks
    records by arithmetic alone — curve <em>i</em> lives at texel
    <code>2i</code> — and sharing would break that: stepping over contour
    terminators needs either an index list (which is the banded layout) or a
    per-texel branch in the innermost loop, and a branch there has measured
    25% of the whole fragment pass in this shader, <em>even when never
    taken</em>. A flat block is at most 16 curves, so unshared records leave
    about 130 bytes per glyph on the table. Cheap insurance.</p>

    <div data-fig="chain"></div>

    <h3>f16, quantized at the source</h3>

    <p>Control points are em coordinates, roughly in [−0.2, 1.2], and an f16
    has an 11-bit significand: in [0.5,&nbsp;1) the representable values are
    2<sup>−11</sup> apart — <strong>4.9×10<sup>−4</sup>&nbsp;em</strong>, the
    worst-case bound the code reasons with (0.031&nbsp;px at 64&nbsp;px/em,
    0.15&nbsp;px at 300); round-to-nearest moves a coordinate at most half a
    pitch, and the pitch coarsens by 2× above 1&nbsp;em, where only a tall
    ascender ever reaches. The subtle decision is
    <em>where</em> to round: in the flattener, the moment a curve is emitted
    (<code>Flattener::push</code>), not on upload. Everything downstream —
    the bbox, band membership, the early-out sort keys, the corner-clip
    support planes — is then computed from the very numbers the shader will
    read. Round on upload instead and the CPU sorts curves by numbers the GPU
    does not have; the early-out could terminate a loop in front of a curve
    that still crosses the ray. Rounding is a function of the value, so a
    point carried from one record's p2 to the next record's p0 rounds
    identically in both, and endpoint sharing stays exact.</p>

    <p>Integers ride in the same lanes: band headers and index lists store
    texel offsets and counts as f16, which holds integers exactly only up to
    2048 (<code>F16_EXACT_INT</code>). A block whose offsets would pass that
    can't be banded — <code>banded_block</code> returns <code>None</code> and
    the glyph keeps the flat layout, which does its addressing in u32 and
    stores no integers at all. Real fonts don't get close: the largest offset
    DejaVu Sans or Manrope encodes is 254; it takes a ~550-curve glyph to
    trip the limit.</p>

    <p>How much does the rounding actually cost? For the four fonts embedded
    in this page: <em>nothing</em>, and the figure below audits that claim
    live. The scaler hands outlines over in 26.6 fixed point — every
    coordinate a multiple of 1/64 em — and the flattener's only synthesized
    values, line midpoints, halve that to 1/128. Every multiple of 1/128 em
    below 16 em <em>is</em> an f16, so for all-quadratic TrueType faces the
    quantization is the identity. The deltas live elsewhere: when a cubic
    face is flattened, the quarter-splits and the midpoint control rule
    manufacture coordinates on grids as fine as 1/16384 em, and those do
    round — which is what the offscreen regression scene measured (details
    in the collapsible below the figure).</p>

    <div data-fig="deltas"></div>

    <h3>The stores</h3>

    <p>Extraction happens once per (font, glyph id, weight, style-flags) key
    — <code>GlyphKey</code> — and never again. Because curves are stored in
    em units, one extraction serves every size, rotation, and 3D projection
    the glyph will ever be drawn at; there is no per-size rasterization to
    cache and therefore none to invalidate. The cache's job is purely spatial:
    where does each block live, and what happens when the texture fills
    up.</p>

    <p>Three things can happen to the texture, in escalating order. It
    <strong>grows</strong>: height doubles (2048 → 4096 → 8192), a new
    texture is created, and the CPU-side mirror is re-uploaded whole. Growth
    moves no glyph — every <code>first</code> is still correct — so it bumps
    <code>generation</code> (the renderer must rebuild its bind group around
    the new texture) but deliberately <em>not</em>
    <code>layout_generation</code>. It <strong>overflows</strong>: at the
    height cap, a failed allocation sets a flag, the glyph falls back to the
    bitmap atlas <em>for that frame</em>, and nothing else happens — because
    the frame in flight has already queued instances holding raw texel
    addresses, and pulling texels out from under them would draw garbage. And
    at the next frame edge it <strong>compacts</strong>: the least-recently-
    used half of the entries is dropped, survivors are repacked densely, each
    survivor's <code>first</code> is rewritten, and every move is recorded in
    a relocation map (old base → new base).</p>

    <p>That map exists because <a href="#geometry-and-damage">retained
    blocks</a> bake texel addresses into
    instances that live for many frames. At each frame edge the renderer runs
    a fixed sequence: first it <em>touches</em> every key its live blocks
    reference — stamping them used for the frame about to start, so retained
    content is never in the LRU's cold half — then the store compacts, then
    <code>relocate_glyph_instances</code> patches every retained vector
    instance through the map: <code>first</code> becomes the new base, and a
    dual-master glyph's <code>b_first</code> becomes
    <code>moved&nbsp;+&nbsp;(b_first&nbsp;−&nbsp;first)</code> — the A→B
    stride is inside the block, so it survives the move untouched. A baked
    address that is <em>not</em> in the map belonged to an evicted glyph: its
    instance's curve count is set to 0, which draws nothing and clears the
    banded flag with it, and the block is marked stale so the host re-sets
    its content.</p>

    <div data-fig="lifecycle"></div>

    <p>The bitmap atlas is the parallel lane for what the vector path can't
    express — CBDT/sbix/SVG emoji, COLRv1 — plus the overflow fallback above.
    It is a 2048² RGBA8 texture, shelf-packed with the
    <code>etagere</code> allocator, one pixel of gutter around every glyph so
    linear filtering never bleeds. Its eviction is gentler and lazier than
    the curve store's: when an allocation fails, everything not drawn this
    frame or last is deallocated and the allocation retried; only if that
    still fails (free space too fragmented) does it schedule a wholesale
    reset at the next frame edge, bumping <code>Atlas::generation</code> —
    the one event that invalidates a retained UV, answered by collapsing
    retained atlas quads to zero size and marking their blocks stale.</p>

    <h3>A bug worth a figure: the high-water mark</h3>

    <p>New curve data reaches the GPU through <code>CurveStore::flush</code>,
    which uploads only what changed since the last flush. The original code
    tracked progress in <em>rows</em>: compare total rows to uploaded rows,
    return early if nothing new. But rows are the transport's granularity,
    not the data's — glyphs are appended in texels, and a small glyph
    extracted after a flush can land entirely <em>inside</em> the current
    partial row. Total rows: unchanged. Early return: taken. The store held
    the curves, the texture never saw them, and the glyph rendered as nothing
    — permanently, because the cache said it was already resident. The fix
    tracks <code>uploaded_floats</code> and re-uploads from the row holding
    the first new float, refreshing the partial row new curves share with old
    ones. The general lesson: an upload high-water mark must use the finest
    granularity the data <em>grows</em> by, never the granularity the
    transport prefers.</p>

    <div data-fig="flushbug"></div>
  `,

  async mount(root, ctx) {
    const { fig, palette } = ctx;
    const mono =
      "font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 12px;";

    const REGION_COLORS = {
      header: palette.orange,
      index: palette.purple,
      records: palette.blue,
      recordsB: palette.green,
      term: palette.teal,
      pad: palette.muted,
    };
    const REGION_NAMES = {
      header: "band header",
      index: "index lists",
      records: "curve records (A)",
      recordsB: "curve records (B)",
      term: "contour terminator",
      pad: "padding",
    };

    // Per-texel meaning of a glyph's block, derived from the inspection's
    // layout + bands + contours — the same arithmetic banded_block uses.
    function texelMeta(insp) {
      const L = insp.layout;
      const meta = new Array(L.total_texels);
      let off = 0;
      if (L.header_texels > 0) {
        const bandList = [...insp.bands.y, ...insp.bands.x];
        for (let i = 0; i < L.header_texels; i++) {
          const axis = i < 8 ? "y" : "x";
          meta[off++] = {
            region: "header",
            label:
              `texel ${i} — header, ${axis}-band ${i % 8}: ` +
              `[descending-list offset, curve count, split coordinate, ascending-list offset]`,
          };
        }
        bandList.forEach((band, bi) => {
          const axis = bi < 8 ? "y" : "x";
          const n = band.descending.length;
          const texels = Math.ceil(n / 4);
          for (const name of ["descending", "ascending"]) {
            for (let k = 0; k < texels; k++) {
              const hi = Math.min(n, k * 4 + 4);
              meta[off] = {
                region: "index",
                label:
                  `texel ${off} — ${axis}-band ${bi % 8} ${name} list, ` +
                  `entries ${k * 4}–${hi - 1}` +
                  (hi - k * 4 < 4 ? " (rest zero-padded)" : "") +
                  " — each entry a base-relative record offset",
              };
              off++;
            }
          }
        });
        while (off < L.records_offset) {
          meta[off] = {
            region: "pad",
            label: `texel ${off} — padding: index region rounded to an even texel count`,
          };
          off++;
        }
      }
      for (let m = 0; m < L.masters; m++) {
        const base = L.records_offset + m * L.record_texels;
        const master = m === 0 ? "master A" : "master B";
        let t = base;
        let curve = 0;
        if (L.header_texels > 0) {
          // Banded: shared endpoints, one texel per curve + terminators.
          for (let c = 0; c < insp.contours.length; c++) {
            for (let j = 0; j < insp.contours[c]; j++) {
              meta[t++] = {
                region: m === 0 ? "records" : "recordsB",
                label:
                  `texel ${t - 1} — ${master}, curve ${curve}: [p0.x p0.y p1.x p1.y]; ` +
                  `its p2 is the first two lanes of the next texel`,
              };
              curve++;
            }
            meta[t++] = {
              region: "term",
              label:
                `texel ${t - 1} — ${master}, contour ${c} terminator: the closing p2 ` +
                `(the contour's first point again)`,
            };
          }
          while (t < base + L.record_texels) {
            meta[t] = {
              region: "pad",
              label: `texel ${t} — padding: record region rounded up to even`,
            };
            t++;
          }
        } else {
          // Flat: two unshared texels per curve, addressed as first + 2i.
          for (let i = 0; i < insp.curves.length; i++) {
            meta[t++] = {
              region: m === 0 ? "records" : "recordsB",
              label: `texel ${t - 1} — ${master}, curve ${i}, texel 0: [p0.x p0.y p1.x p1.y]`,
            };
            meta[t++] = {
              region: m === 0 ? "records" : "recordsB",
              label:
                `texel ${t - 1} — ${master}, curve ${i}, texel 1: [p2.x p2.y 0 0] — ` +
                `unshared, so the flat loop finds curve i at texel 2·i`,
            };
          }
        }
      }
      return meta;
    }

    // ---- Figure (a): the texel map ------------------------------------
    {
      const host = root.querySelector('[data-fig="texelmap"]');
      const specimens = [
        { label: "DejaVu 'g' — banded", ch: "g", family: "DejaVu Sans" },
        { label: "Manrope 'g' — banded, two masters", ch: "g", family: "Manrope" },
        { label: "DejaVu 'i' — flat", ch: "i", family: "DejaVu Sans" },
      ];
      for (const s of specimens) s.insp = await ctx.inspect(s.ch, s.family, 400);

      const readout = fig.el("div", { style: mono + `color: ${palette.textDim}; min-height: 2.6em;` },
        "hover a texel");
      const gridHost = fig.el("div", {});
      const buttons = specimens.map((s, i) =>
        fig.el("button", {
          style:
            `margin-right: 8px; padding: 3px 10px; background: transparent; ` +
            `color: ${palette.textDim}; border: 1px solid ${palette.border}; ` +
            `border-radius: 4px; cursor: pointer; font-size: 12px;`,
          onclick: () => select(i),
        }, s.label),
      );

      const WRAP = 24; // texels per drawn row (drawing convenience, not the texture's 256)
      function drawMap(insp) {
        const meta = texelMeta(insp);
        const cell = 15;
        const rows = Math.ceil(meta.length / WRAP);
        const svg = fig.svg(WRAP * (cell + 1) + 8, rows * (cell + 1) + 8);
        meta.forEach((m, i) => {
          const x = 4 + (i % WRAP) * (cell + 1);
          const y = 4 + Math.floor(i / WRAP) * (cell + 1);
          const rect = fig.svgEl("rect", {
            x, y, width: cell, height: cell,
            fill: REGION_COLORS[m.region],
            "fill-opacity": m.region === "pad" ? 0.35 : 0.75,
            stroke: palette.bgDark,
            "stroke-width": 1,
            style: "cursor: crosshair;",
          }, fig.svgEl("title", {}, m.label));
          rect.addEventListener("mouseenter", () => {
            readout.textContent = m.label;
            rect.setAttribute("fill-opacity", "1");
          });
          rect.addEventListener("mouseleave", () =>
            rect.setAttribute("fill-opacity", m.region === "pad" ? "0.35" : "0.75"));
          svg.append(rect);
        });
        return svg;
      }

      const legend = fig.el("div", { style: mono + "margin-top: 6px;" },
        ...Object.entries(REGION_NAMES).map(([k, name]) =>
          fig.el("span", { style: `margin-right: 14px; color: ${palette.textDim};` },
            fig.el("span", {
              style:
                `display: inline-block; width: 10px; height: 10px; margin-right: 4px; ` +
                `background: ${REGION_COLORS[k]}; border-radius: 2px;`,
            }), name)));

      const capLine = fig.el("div", { style: mono + `color: ${palette.muted}; margin-top: 4px;` });

      function select(i) {
        const s = specimens[i];
        buttons.forEach((b, j) =>
          (b.style.borderColor = j === i ? palette.blue : palette.border));
        gridHost.replaceChildren(drawMap(s.insp));
        const L = s.insp.layout;
        capLine.textContent =
          `${s.family} '${s.ch}': ${s.insp.curves.length} curves, ` +
          `${s.insp.contours.length} contour${s.insp.contours.length === 1 ? "" : "s"}, ` +
          `${L.masters} master${L.masters === 1 ? "" : "s"} — ` +
          `${L.header_texels} header + ${L.index_texels} index + ` +
          `${L.masters} × ${L.record_texels} record texels = ${L.total_texels} texels ` +
          `= ${L.total_texels * 8} bytes`;
        readout.textContent = "hover a texel";
      }

      host.append(
        fig.figure(
          [fig.el("div", { style: "margin-bottom: 8px;" }, ...buttons), gridHost, legend, capLine, readout],
          "A real glyph's block, texel by texel, wrapped at 24 texels per line for " +
            "display (the texture itself is 256 wide). Orange: the 16-texel band " +
            "header. Purple: sorted index lists, four base-relative record offsets " +
            "per texel. Blue/green: master A/B curve texels — one per curve, thanks " +
            "to endpoint sharing. Teal: contour terminators. Every offset inside " +
            "the block is relative to GlyphCurves::first, so compaction relocates " +
            "the whole thing with a memcpy and one field rewrite.",
        ),
        fig.details(
          "Gory details: what a header texel holds",
          `<p>One texel per band, in fixed order: 8 y-bands then 8 x-bands
           (<code>HEADER_TEXELS = BANDS × 2</code>). The four f16 lanes are
           <em>(descending-list offset, curve count, split coordinate,
           ascending-list offset)</em>. The two list offsets are texel counts
           from the block base; the split is an em coordinate, f16-rounded by
           <code>median_split</code> before it is compared to anything, so the
           CPU and the shader agree on which side of it a sample falls. Index
           entries are also base-relative record offsets — the shader resolves
           a curve without ever knowing where the record region starts, which
           is exactly what lets a block move without internal rewrites. All of
           these integers are exact in f16 only up to 2048
           (<code>F16_EXACT_INT</code>); <code>banded_block</code> refuses to
           build a block that would encode a larger one and the glyph stays
           flat.</p>`,
        ),
      );
      select(0);
    }

    // ---- Figure (b): the shared-endpoint chain -------------------------
    {
      const host = root.querySelector('[data-fig="chain"]');
      const g = await ctx.inspect("g", "DejaVu Sans", 400);
      const n = g.contours[0];
      const contour = g.curves.slice(0, n);

      // Outline of contour 0 alone, em-space.
      const xs = contour.flatMap((c) => [c.p0[0], c.p1[0], c.p2[0]]);
      const ys = contour.flatMap((c) => [c.p0[1], c.p1[1], c.p2[1]]);
      const bbox = [Math.min(...xs), Math.min(...ys), Math.max(...xs), Math.max(...ys)];
      const em = fig.emSpace(bbox, { width: 300, pad: 22 });
      const outlineSvg = fig.svg(em.width, em.height);
      outlineSvg.append(em.g);
      em.g.append(fig.svgEl("path", {
        d: fig.glyphPathD(contour, [n]),
        fill: palette.blue, "fill-opacity": 0.12,
        stroke: "none",
      }));
      const curveSegs = contour.map((c) =>
        fig.svgEl("path", {
          d: `M ${c.p0[0]} ${c.p0[1]} Q ${c.p1[0]} ${c.p1[1]} ${c.p2[0]} ${c.p2[1]}`,
          fill: "none", stroke: palette.blue, "stroke-width": 1.5,
        }));
      curveSegs.forEach((s) => em.g.append(s));
      const dots = contour.map((c, i) => {
        const d = fig.svgEl("circle", {
          cx: c.p0[0], cy: c.p0[1], r: 3.5 / em.scale, fill: palette.teal,
        }, fig.svgEl("title", {}, `p0 of curve ${i} = p2 of curve ${(i + n - 1) % n}`));
        em.g.append(d);
        return d;
      });

      // The texel strip: n curve texels + 1 terminator.
      const bw = 44, bh = 34, gap = 4, x0 = 6, y0 = 24;
      const strip = fig.svg(x0 * 2 + (n + 1) * (bw + gap), 96);
      const boxes = [];
      for (let i = 0; i <= n; i++) {
        const x = x0 + i * (bw + gap);
        const isTerm = i === n;
        const box = fig.svgEl("rect", {
          x, y: y0, width: bw, height: bh, rx: 3,
          fill: isTerm ? palette.teal : palette.blue,
          "fill-opacity": 0.22,
          stroke: isTerm ? palette.teal : palette.blue,
          "stroke-width": 1,
        });
        strip.append(box);
        boxes.push(box);
        strip.append(
          fig.svgEl("text", {
            x: x + bw / 2, y: y0 + 14, "text-anchor": "middle",
            fill: palette.text, "font-size": 10, style: mono,
          }, isTerm ? "p2[" + (n - 1) + "]" : `p0[${i}]`),
          fig.svgEl("text", {
            x: x + bw / 2, y: y0 + 27, "text-anchor": "middle",
            fill: isTerm ? palette.muted : palette.textDim, "font-size": 10, style: mono,
          }, isTerm ? "(term)" : `p1[${i}]`),
          fig.svgEl("text", {
            x: x + bw / 2, y: y0 - 6, "text-anchor": "middle",
            fill: palette.muted, "font-size": 9, style: mono,
          }, `${i}`),
        );
      }
      const bracket = fig.svgEl("path", { d: "", fill: "none", stroke: palette.orange, "stroke-width": 1.5 });
      const bracketLabel = fig.svgEl("text", {
        x: 0, y: y0 + bh + 26, fill: palette.orange, "font-size": 10, style: mono,
      });
      strip.append(bracket, bracketLabel);

      function highlight(i) {
        curveSegs.forEach((s, j) => {
          s.setAttribute("stroke", j === i ? palette.orange : palette.blue);
          s.setAttribute("stroke-width", j === i ? "3" : "1.5");
        });
        dots.forEach((d, j) =>
          d.setAttribute("fill", j === i || j === (i + 1) % n ? palette.orange : palette.teal));
        boxes.forEach((b, j) => {
          const active = j === i || j === i + 1;
          b.setAttribute("stroke-width", active ? "2.5" : "1");
          b.setAttribute("stroke", active ? palette.orange
            : j === n ? palette.teal : palette.blue);
        });
        const xa = x0 + i * (bw + gap), xb = x0 + (i + 1) * (bw + gap) + bw;
        const yb = y0 + bh + 6;
        bracket.setAttribute("d", `M ${xa} ${yb} v 5 H ${xb} v -5`);
        bracketLabel.setAttribute("x", "6");
        bracketLabel.textContent =
          `curve ${i} = texel ${i} + the first two lanes of texel ${i + 1}`;
      }
      const sl = fig.slider({
        label: "curve", min: 0, max: n - 1, step: 1, value: 2,
        format: (v) => `${v} of ${n}`,
        oninput: (v) => highlight(v),
      });
      highlight(2);

      host.append(fig.figure(
        [
          fig.el("div", { style: "display: flex; flex-wrap: wrap; gap: 16px; align-items: center;" },
            outlineSvg, strip),
          fig.controls(sl.root),
        ],
        `The first contour of DejaVu Sans 'g' (${n} curves) and its texel run in a ` +
          `banded block. Each blue texel is one curve's [p0 p1]; the curve's p2 is ` +
          `the next texel's first two lanes — the same float the flattener carried ` +
          `across, so sharing is exact. The teal terminator holds the closing p2. ` +
          `${n} curves in ${n + 1} texels instead of ${2 * n}; a flat block keeps ` +
          `the 2-texel form so it can address curve i at texel 2i with no branch.`,
      ));
    }

    // ---- Figure (c): the f16 exactness audit + rounding envelope --------
    {
      const host = root.querySelector('[data-fig="deltas"]');
      // f16 rounding, exact: Chrome has Math.f16round; fall back through a
      // Float16Array view if it ever doesn't.
      const f16 = Math.f16round
        ? Math.f16round
        : (x) => { const a = new Float16Array(1); a[0] = x; return a[0]; };

      const specimens = [
        ["DejaVu Sans", "g"], ["DejaVu Sans", "W"],
        ["Manrope", "g"], ["Manrope", "S"],
      ];
      const options = [];
      for (const [family, ch] of specimens) {
        const insp = await ctx.inspect(ch, family, 400);
        if (!insp || insp.curves.length === 0) continue;
        let moved = 0, total = 0, grid = 1;
        const coords = [];
        insp.curves.forEach((c, i) => {
          const raw = insp.curves_raw[i];
          for (const k of ["p0", "p1", "p2"]) for (const a of [0, 1]) {
            total++;
            coords.push(raw[k][a]);
            if (c[k][a] !== raw[k][a]) moved++;
          }
        });
        while (grid <= 1 << 20 && !coords.every((x) => Number.isInteger(x * grid))) grid *= 2;
        options.push({ family, ch, insp, moved, total, grid, coords });
      }

      const svgHost = fig.el("div", {});
      const stats = fig.el("div", { style: mono + `color: ${palette.textDim}; margin-top: 4px;` });
      let current = options[0];
      let probeX = 0.7503; // an off-grid coordinate, to show a real rounding

      const selectEl = fig.el("select", {
        style:
          `background: ${palette.bgDark}; color: ${palette.text}; ` +
          `border: 1px solid ${palette.border}; border-radius: 4px; padding: 2px 6px;`,
        onchange: (e) => { current = options[Number(e.target.value)]; draw(); },
      }, ...options.map((o, i) =>
        fig.el("option", { value: i },
          `${o.family} '${o.ch}' — ${o.moved} of ${o.total} coordinates moved`)));

      // The rounding-error envelope |f16round(x) − x| over the em range,
      // computed for real; per-pixel max so the sawtooth's teeth (a few per
      // pixel at this scale) read as their true envelope.
      const PW = 420, PH = 120, X_MAX = 1.3, Y_MAX = Math.pow(2, -11) * 1.15;
      function draw() {
        const o = current;
        const svg = fig.svg(PW + 10, PH + 60);
        const y0 = 8; // top of plot area
        const yOf = (err) => y0 + PH - (err / Y_MAX) * PH;
        const xOf = (x) => (x / X_MAX) * PW;
        // Envelope, per-pixel max of |f16round(x) - x|.
        let d = `M 0 ${yOf(0)}`;
        for (let px = 0; px <= PW; px++) {
          let maxErr = 0;
          for (let s = 0; s < 16; s++) {
            const x = ((px + s / 16) / PW) * X_MAX;
            maxErr = Math.max(maxErr, Math.abs(f16(x) - x));
          }
          d += ` L ${px} ${yOf(maxErr)}`;
        }
        svg.append(fig.svgEl("path", {
          d: d + ` L ${PW} ${yOf(0)} Z`,
          fill: palette.red, "fill-opacity": 0.25, stroke: palette.red, "stroke-width": 1,
        }));
        // Half-pitch guide lines with labels.
        for (const [err, label] of [
          [Math.pow(2, -11), "2⁻¹¹ em — max rounding in [1, 2): the pitch doubles at 1 em"],
          [Math.pow(2, -12), "2⁻¹² em — max rounding in [0.5, 1)"],
          [Math.pow(2, -13), "2⁻¹³"],
        ]) {
          svg.append(
            fig.svgEl("line", {
              x1: 0, y1: yOf(err), x2: PW, y2: yOf(err),
              stroke: palette.muted, "stroke-width": 1, "stroke-dasharray": "3 4",
            }),
            fig.svgEl("text", {
              x: 4, y: yOf(err) - 3, fill: palette.muted, "font-size": 9, style: mono,
            }, label),
          );
        }
        // Baseline + em ticks.
        svg.append(fig.svgEl("line", {
          x1: 0, y1: yOf(0), x2: PW, y2: yOf(0), stroke: palette.border, "stroke-width": 1,
        }));
        for (const x of [0, 0.25, 0.5, 1.0]) {
          svg.append(fig.svgEl("text", {
            x: xOf(x), y: yOf(0) + 12, fill: palette.muted, "font-size": 9, style: mono,
          }, `${x} em`));
        }
        // The glyph's real coordinates, ticked where they land: all on the
        // 1/grid lattice, all at zero error.
        const seen = new Set();
        for (const c of o.coords) {
          const key = Math.round(Math.abs(c) * 16384);
          if (c < 0 || c > X_MAX || seen.has(key)) continue;
          seen.add(key);
          svg.append(fig.svgEl("line", {
            x1: xOf(c), y1: yOf(0), x2: xOf(c), y2: yOf(0) - 6,
            stroke: palette.green, "stroke-width": 1,
          }));
        }
        // The probe: a real rounding at an arbitrary coordinate.
        const err = Math.abs(f16(probeX) - probeX);
        svg.append(
          fig.svgEl("line", {
            x1: xOf(probeX), y1: y0, x2: xOf(probeX), y2: yOf(0),
            stroke: palette.orange, "stroke-width": 1, "stroke-dasharray": "2 3",
          }),
          fig.svgEl("circle", {
            cx: xOf(probeX), cy: yOf(Math.min(err, Y_MAX)), r: 3, fill: palette.orange,
          }),
          fig.svgEl("text", {
            x: 4, y: PH + 40, fill: palette.orange, "font-size": 10, style: mono,
          }, `probe x = ${probeX.toFixed(6)} em → f16 ${f16(probeX).toFixed(6)}`),
          fig.svgEl("text", {
            x: 4, y: PH + 54, fill: palette.orange, "font-size": 10, style: mono,
          }, `error ${err.toExponential(2)} em = ${(err * 300).toFixed(3)} px at 300 px/em`),
        );
        svgHost.replaceChildren(svg);
        stats.textContent =
          `${o.family} '${o.ch}': ${o.moved} of ${o.total} stored coordinates moved by ` +
          `quantization — every coordinate is a multiple of 1/${o.grid} em ` +
          `(green ticks), and n/${o.grid} below 16 em is exactly an f16.`;
      }

      const probeSlider = fig.slider({
        label: "probe coordinate", min: 0, max: X_MAX, step: 0.0001, value: probeX,
        format: (v) => `${Number(v).toFixed(4)} em`,
        oninput: (v) => { probeX = v; draw(); },
      });
      draw();

      host.append(
        fig.figure(
          [svgHost, stats, fig.controls(selectEl, probeSlider.root)],
          "The true cost of f16, measured two ways. The red envelope is the " +
            "actual rounding error |f16(x) − x| across the em range (computed " +
            "with Math.f16round, per-pixel maximum): zero at every representable " +
            "value, up to half the local pitch between them, doubling at 0.25, " +
            "0.5 and 1 em. Green ticks: where this glyph's real coordinates land " +
            "— all on the 1/128-em lattice, so the audit finds zero moved " +
            "coordinates. Drag the probe to any off-lattice coordinate to see a " +
            "genuine rounding, in em and in pixels at 300 px/em.",
        ),
        fig.details(
          "Gory details: where real deltas come from, and how the pixel cost was measured",
          `<p>The embedded faces are all-quadratic TrueType, so their
           coordinates arrive pre-snapped to the scaler's 26.6 grid and f16
           changes nothing. Cubic outlines are different: the flattener
           approximates each cubic with four quadratics (split at quarters,
           midpoint control rule — <code>Flattener::cubic_to</code>), and that
           arithmetic lands on fractions as fine as 1/16384 em, which f16
           does round. The offscreen regression scene measured the resulting
           pixels: 2882 of 576,000 changed, 83% of them by ≤2/255 of coverage,
           with a tail to 45/255 on near-tangent edges — where a ray-crossing
           position is ill-conditioned in <em>any</em> precision. That this is
           quantization and nothing else was proven by a control: the same
           shared-endpoint layout uploaded through an RGBA32F texture is
           byte-identical to the pre-f16 scene, and with quantization applied
           it reproduces the RGBA16F render exactly. If a scene ever needs
           more precision than f16, the move is per-glyph fixed point —
           Rgba16Unorm normalized over the glyph's bbox, ~1e-5 em — not a
           wider float.</p>`,
        ),
      );
    }

    // ---- Figure (d): the store lifecycle -------------------------------
    {
      const host = root.querySelector('[data-fig="lifecycle"]');
      const ROW = 256; // the texture's real width in texels
      const sizes = [];
      for (const ch of "gBQaOesknRdwm&@%WMK") {
        const insp = await ctx.inspect(ch, "DejaVu Sans", 400);
        if (insp && insp.layout.total_texels > 0) {
          sizes.push({ ch, texels: insp.layout.total_texels });
        }
      }
      // Simulate a store scaled down for the page: starts 2 rows tall, caps
      // at 4 (the real one starts at 2048 rows and caps at 8192; the widths
      // and every block size are real).
      const CAP = 4 * ROW;
      let capacity = 2 * ROW;
      const placed = [];
      let cum = 0, grewAt = -1, overflow = null;
      for (const s of sizes) {
        if (cum + s.texels > CAP) { overflow = s; break; }
        if (cum + s.texels > capacity && grewAt < 0) { capacity = CAP; grewAt = placed.length; }
        placed.push({ ...s, first: cum });
        cum += s.texels;
      }
      // LRU stamps: everything placed before growth was drawn on frame 1,
      // the rest on frame 2; the retained block's glyphs ('g' and 'Q') are
      // touched with frame 3 at the frame edge.
      const retained = new Set(["g", "Q"]);
      placed.forEach((b, i) => {
        b.lastUsed = retained.has(b.ch) ? 3 : i < grewAt ? 1 : 2;
      });
      const order = [...placed].sort((a, b) => a.lastUsed - b.lastUsed);
      const doomed = new Set(order.slice(0, Math.floor(order.length / 2)).map((b) => b.ch));
      const survivors = placed.filter((b) => !doomed.has(b.ch));
      let c2 = 0;
      for (const s of survivors) { s.newFirst = c2; c2 += s.texels; }

      const scale = 420 / ROW, rowH = 20, rowGap = 5;
      function chunkRects(first, texels) {
        const out = [];
        let t = first, left = texels;
        while (left > 0) {
          const row = Math.floor(t / ROW), col = t % ROW;
          const take = Math.min(left, ROW - col);
          out.push({ row, col, take });
          t += take; left -= take;
        }
        return out;
      }
      function drawStrip(svg, yTop, rows, blocks, opts = {}) {
        for (let r = 0; r < rows; r++) {
          svg.append(fig.svgEl("rect", {
            x: 0, y: yTop + r * (rowH + rowGap), width: 420, height: rowH,
            fill: palette.bgDark, stroke: palette.border, "stroke-width": 1,
          }));
        }
        for (const b of blocks) {
          const first = opts.compacted ? b.newFirst : b.first;
          const color = opts.doomed && doomed.has(b.ch)
            ? palette.red
            : retained.has(b.ch) ? palette.green : palette.blue;
          chunkRects(first, b.texels).forEach((c, i) => {
            svg.append(fig.svgEl("rect", {
              x: c.col * scale, y: yTop + c.row * (rowH + rowGap) + 1,
              width: Math.max(c.take * scale - 1, 1), height: rowH - 2,
              fill: color, "fill-opacity": opts.doomed && doomed.has(b.ch) ? 0.5 : 0.7,
              stroke: "none",
            }));
            if (i === 0 && c.take * scale > 12) {
              svg.append(fig.svgEl("text", {
                x: c.col * scale + 4, y: yTop + c.row * (rowH + rowGap) + 14,
                fill: palette.bgDark, "font-size": 11, "font-weight": "bold", style: mono,
              }, b.ch));
            }
          });
        }
      }

      const stripHost = fig.el("div", {});
      const desc = fig.el("div", {
        style: mono + `color: ${palette.textDim}; min-height: 5.5em; margin-top: 6px;`,
      });

      const steps = [
        {
          name: "extract + append",
          rows: 2, blocks: placed.slice(0, Math.max(grewAt, 1)),
          html:
            `<b>extract → append.</b> First use of a (font, glyph, weight) key runs ` +
            `<code>CurveStore::get_or_insert</code>: the outline is flattened, quantized ` +
            `and packed into a block, appended to the CPU mirror at the next even texel. ` +
            `<code>flush</code> uploads the new tail before the frame draws. ` +
            `Green blocks belong to a retained text block ('g', 'Q').`,
        },
        {
          name: "grow",
          rows: 4, blocks: placed,
          html:
            `<b>grow.</b> Block '${placed[grewAt]?.ch}' did not fit, so ` +
            `<code>grow_to_fit</code> doubled the height and re-uploaded the mirror ` +
            `whole. No glyph moved — every <code>first</code> is unchanged — so ` +
            `<code>generation</code> bumps (bind group rebuild) but ` +
            `<code>layout_generation</code> does not.`,
        },
        {
          name: "overflow",
          rows: 4, blocks: placed, overflow: true,
          html:
            `<b>overflow.</b> At the cap, '${overflow?.ch}' (${overflow?.texels} texels) ` +
            `fails to allocate. Mid-frame eviction is forbidden — queued instances hold ` +
            `raw texel addresses — so the store sets <code>needs_compact</code>, nothing ` +
            `is cached, and '${overflow?.ch}' renders from the bitmap atlas this frame.`,
        },
        {
          name: "mark LRU",
          rows: 4, blocks: placed, doomed: true, overflow: true,
          html:
            `<b>frame edge: touch, then mark.</b> The renderer stamps every key its ` +
            `retained blocks reference (<code>touch</code>) <i>before</i> the store acts, ` +
            `so 'g' and 'Q' are warm. <code>compact</code> then drops the least-recently-` +
            `used half of the entries: ${[...doomed].map((c) => `'${c}'`).join(", ")}.`,
        },
        {
          name: "compact",
          rows: 4, blocks: survivors, compacted: true, arrows: true,
          html:
            `<b>compact.</b> Survivors are memcpy'd left, densely; every internal offset ` +
            `is base-relative, so only <code>first</code> is rewritten. The moves are ` +
            `recorded: <code>relocations = {` +
            survivors.slice(0, 4).map((s) => `${s.first}→${s.newFirst}`).join(", ") +
            (survivors.length > 4 ? ", …" : "") +
            `}</code>, and <code>layout_generation</code> bumps.`,
        },
        {
          name: "relocate",
          rows: 4, blocks: survivors, compacted: true,
          html:
            `<b>relocate.</b> <code>relocate_glyph_instances</code> patches retained ` +
            `instances through the map: 'g' instance <code>first: ` +
            `${survivors.find((s) => s.ch === "g")?.first} → ` +
            `${survivors.find((s) => s.ch === "g")?.newFirst}</code> ` +
            `(a dual-master glyph's <code>b_first</code> moves by the same delta — the ` +
            `A→B stride is inside the block). An instance whose base is <i>not</i> in ` +
            `the map was evicted: its <code>count</code> is set to 0, which draws ` +
            `nothing and clears BANDED_FLAG with it, and its block is marked stale.`,
        },
      ];

      function render(step) {
        const st = steps[step];
        const h = st.rows * (rowH + rowGap) + (st.overflow ? 56 : 10) + 18;
        const svg = fig.svg(430, h);
        svg.append(fig.svgEl("text", {
          x: 0, y: 12, fill: palette.muted, "font-size": 10, style: mono,
        }, `curve texture: ${st.rows} rows × 256 texels (page-compressed)`));
        drawStrip(svg, 18, st.rows, st.blocks, st);
        if (st.arrows) {
          for (const s of survivors.slice(0, 5)) {
            if (s.first === s.newFirst) continue;
            const [r0, c0] = [Math.floor(s.first / ROW), s.first % ROW];
            const [r1, c1] = [Math.floor(s.newFirst / ROW), s.newFirst % ROW];
            svg.append(fig.svgEl("path", {
              d: `M ${c0 * scale} ${18 + r0 * (rowH + rowGap) + rowH / 2} ` +
                 `L ${c1 * scale + 3} ${18 + r1 * (rowH + rowGap) + rowH / 2}`,
              stroke: palette.orange, "stroke-width": 1, "stroke-dasharray": "3 2",
              fill: "none", opacity: 0.9,
            }));
          }
        }
        if (st.overflow && overflow) {
          const y = 18 + st.rows * (rowH + rowGap) + 8;
          svg.append(
            fig.svgEl("rect", {
              x: 0, y, width: Math.max(overflow.texels * scale, 30), height: rowH - 2,
              fill: "none", stroke: palette.red, "stroke-width": 1.5, "stroke-dasharray": "4 3",
            }),
            fig.svgEl("text", {
              x: 0, y: y + rowH + 12,
              fill: palette.red, "font-size": 10, style: mono,
            }, `'${overflow.ch}' (${overflow.texels} texels) → bitmap atlas this frame`),
          );
        }
        stripHost.replaceChildren(svg);
        desc.innerHTML = st.html;
      }

      const stepSlider = fig.slider({
        label: "lifecycle step", min: 0, max: steps.length - 1, step: 1, value: 0,
        format: (v) => `${v + 1}/${steps.length}: ${steps[v].name}`,
        oninput: (v) => render(v),
      });
      render(0);

      host.append(fig.figure(
        [stripHost, desc, fig.controls(stepSlider.root)],
        "The store's whole lifecycle on real block sizes (every block is a DejaVu " +
          "glyph at its true texel count; the strip is shortened from 2048 rows to " +
          "a few for the page). Extract appends, flush uploads the tail, growth " +
          "doubles and moves nothing, overflow defers to the frame edge, compaction " +
          "drops the cold half and memcpy's survivors left, and retained instances " +
          "are patched through the relocation map rather than rebuilt.",
      ));
    }

    // ---- Figure (e): the partial-row flush bug --------------------------
    {
      const host = root.querySelector('[data-fig="flushbug"]');
      const a = await ctx.inspect("i", "DejaVu Sans", 400);
      const b = await ctx.inspect("l", "DejaVu Sans", 400);
      const ta = a.layout.total_texels, tb = b.layout.total_texels;
      const ROW = 256, scale = 420 / ROW, barH = 26;

      function panel(title, fixed) {
        const svg = fig.svg(620, 122);
        svg.append(fig.svgEl("text", {
          x: 0, y: 12, fill: palette.text, "font-size": 11, "font-weight": "bold", style: mono,
        }, title));
        const y = 36;
        // Row 0 of the texture.
        svg.append(fig.svgEl("rect", {
          x: 0, y, width: 420, height: barH,
          fill: palette.bgDark, stroke: palette.border, "stroke-width": 1,
        }));
        // Block labels above the bar (the blocks are too narrow to hold text).
        svg.append(
          fig.svgEl("text", {
            x: 0, y: y - 8, fill: palette.blue, "font-size": 10, style: mono,
          }, `'i' (${ta} texels, flushed)`),
          fig.svgEl("text", {
            x: Math.max(ta * scale + 24, 230), y: y - 8,
            fill: fixed ? palette.green : palette.red, "font-size": 10, style: mono,
          }, `'l' (${tb} texels, appended after the flush)`),
        );
        // Glyph 'i': flushed first.
        svg.append(fig.svgEl("rect", {
          x: 0, y: y + 1, width: ta * scale, height: barH - 2,
          fill: palette.blue, "fill-opacity": 0.7,
        }));
        // Glyph 'l': extracted after the flush, inside the same partial row.
        svg.append(fig.svgEl("rect", {
          x: ta * scale, y: y + 1, width: tb * scale, height: barH - 2,
          fill: fixed ? palette.green : palette.red,
          "fill-opacity": fixed ? 0.7 : 0.5,
        }));
        // Leader from the 'l' label to its (narrow) block: an elbow routed
        // between the label row and the bar so it crosses no text.
        svg.append(fig.svgEl("path", {
          d: `M ${Math.max(ta * scale + 24, 230) - 6} ${y - 11} ` +
             `V ${y - 2} H ${(ta + tb / 2) * scale} V ${y + 1}`,
          stroke: fixed ? palette.green : palette.red,
          "stroke-width": 1, fill: "none", opacity: 0.7,
        }));
        if (!fixed) {
          // The row-granular mark covers the whole row after flush #1.
          svg.append(
            fig.svgEl("rect", {
              x: 0, y: y - 3, width: 420, height: barH + 6,
              fill: "none", stroke: palette.orange, "stroke-width": 1.5,
              "stroke-dasharray": "5 3",
            }),
            fig.svgEl("text", {
              x: 0, y: y + barH + 22, fill: palette.orange, "font-size": 10, style: mono,
            }, `mark after flush #1: "1 row uploaded" (uploaded_rows = ceil(${ta}/256) = 1)`),
            fig.svgEl("text", {
              x: 0, y: y + barH + 36, fill: palette.red, "font-size": 10, style: mono,
            }, `flush #2: total_rows (1) <= uploaded_rows (1) -> early return.`),
            fig.svgEl("text", {
              x: 0, y: y + barH + 50, fill: palette.red, "font-size": 10, style: mono,
            }, `'l' is in the mirror, never in the texture: it draws as nothing, forever.`),
          );
        } else {
          const mx = ta * scale;
          svg.append(
            fig.svgEl("line", {
              x1: mx, y1: y - 6, x2: mx, y2: y + barH + 6,
              stroke: palette.green, "stroke-width": 2,
            }),
            fig.svgEl("text", {
              x: 0, y: y + barH + 22, fill: palette.green, "font-size": 10, style: mono,
            }, `mark after flush #1: uploaded_floats = ${ta} texels x 4 halves`),
            fig.svgEl("text", {
              x: 0, y: y + barH + 36, fill: palette.textDim, "font-size": 10, style: mono,
            }, `flush #2: data.len() > uploaded_floats -> re-upload from the row`),
            fig.svgEl("text", {
              x: 0, y: y + barH + 50, fill: palette.textDim, "font-size": 10, style: mono,
            }, `holding the first new half: row 0 is rewritten, 'l' included.`),
          );
        }
        return svg;
      }

      host.append(fig.figure(
        [panel("The bug: a row-granular high-water mark", false),
         panel("The fix: track uploaded halves, not rows", true)],
        `One 256-texel row of the curve texture, drawn with the real block sizes ` +
          `of DejaVu 'i' (${ta} texels) and 'l' (${tb} texels). Uploads happen in ` +
          `whole rows, so the original mark counted rows — and a glyph appended ` +
          `inside an already-flushed partial row looked like nothing new. The fix ` +
          `(CurveStore::flush) counts uploaded halves and restarts the upload from ` +
          `the row containing the first new one. Regression test: ` +
          `flush_uploads_glyphs_that_fit_inside_a_partial_row.`,
      ));
    }
  },
};
