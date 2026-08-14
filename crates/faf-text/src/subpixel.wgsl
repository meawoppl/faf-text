// LCD subpixel coverage for the vector glyph pipelines.
//
// This file is *appended to* shaders.wgsl (with `enable dual_source_blending;`
// prepended) to build a second shader module, and only on a device that has
// wgpu's DUAL_SOURCE_BLENDING feature — the enable directive is itself gated on
// that capability, so the module cannot exist anywhere the blend factors would
// not. Everything below reuses the winding machinery from shaders.wgsl; only
// the sampling pattern and the output change.
//
// An LCD stripe is a third of a pixel wide, so the R, G and B emitters of one
// pixel sit at x - 1/3, x and x + 1/3. Coverage is therefore three numbers, and
// the blend has to happen per channel with the *text* color as the source —
// which is what dual-source blending is for: src0 carries the color, src1 the
// per-channel coverage, and the pipeline blends `src0 * src1 + dst * (1 -
// src1)`.
//
// Cost: the three stripes replace the x pass's small-size perpendicular taps
// (three casts either way), and the y pass keeps its 3 taps below ~24 px/em, so
// a small glyph runs exactly the six casts the grayscale path runs.

// Stripe offsets in pixels: R is the left third of the pixel, B the right.
// Blocks whose placement mirrors x are routed to the grayscale pipeline by the
// host side, so screen x and em x always point the same way here.
fn stripe_offset(stripe: u32) -> f32 {
    return (f32(stripe) - 1.0) / 3.0;
}

struct SubpixelOutput {
    // src0: the text color. The color blend factor is Src1, so only .rgb is
    // used for color; .a feeds the alpha blend, which stays a plain
    // src-over on the maximum stripe coverage.
    @location(0) @blend_src(0) color: vec4<f32>,
    // src1: per-stripe coverage, alpha-weighted.
    @location(0) @blend_src(1) coverage: vec4<f32>,
};

@fragment
fn vector_subpixel_fs(in: VectorOutput) -> SubpixelOutput {
    let fw = fwidth(in.em);
    let inv_diameter = 1.0 / fw;
    let small = max(fw.x, fw.y) > (1.0 / 24.0);
    let taps = select(1u, 3u, small);

    let block = Block(in.first, in.b_first, in.weight_t);

    var wind_x = array<f32, 3>(0.0, 0.0, 0.0);
    var wind_y = array<f32, 3>(0.0, 0.0, 0.0);
    if (in.count & BANDED_FLAG) != 0u {
        let big = max(fw.x, fw.y) * SPLIT_MIN_PX_PER_EM < 1.0;
        // All three stripes cast along the fragment's own y, so they share one
        // band header entry — the offset that separates them is along the ray.
        // It also puts them a third of a pixel apart across the band's split,
        // which the direction choice ignores: both lists hold the same curves,
        // so picking the "wrong" one for a stripe costs a fetch, not a pixel.
        let y_entry = band_entry(in.first, band_of(in.fraction.y));
        for (var stripe = 0u; stripe < 3u; stripe += 1u) {
            let os = vec2<f32>(stripe_offset(stripe) * fw.x, 0.0);
            wind_x[stripe] = band_rays(
                block, y_entry, in.em, os, inv_diameter.x, false, big);
        }
        for (var tap = 0u; tap < taps; tap += 1u) {
            let off = (f32(tap) + 0.5) / f32(taps) - 0.5;
            let ox = vec2<f32>(0.0, off * fw.x);
            let x_band = band_of(in.fraction.x + off * fw.x * in.frac_per_em.x);
            wind_y[tap] = band_rays(
                block, band_entry(in.first, BANDS + x_band), in.em, ox, inv_diameter.y, true, big);
        }
    } else {
        for (var i = 0u; i < in.count; i += 1u) {
            let curve = fetch_curve(block, i * 2u);
            let p0 = curve.p0 - in.em;
            let p1 = curve.p1 - in.em;
            let p2 = curve.p2 - in.em;
            // Horizontal ray, sample point moved *along* it by a third of a
            // pixel per stripe.
            for (var stripe = 0u; stripe < 3u; stripe += 1u) {
                let os = vec2<f32>(stripe_offset(stripe) * fw.x, 0.0);
                wind_x[stripe] += curve_winding(p0 - os, p1 - os, p2 - os, inv_diameter.x);
            }
            // Vertical ray (coords swapped), unchanged from the grayscale path.
            for (var tap = 0u; tap < taps; tap += 1u) {
                let off = (f32(tap) + 0.5) / f32(taps) - 0.5;
                let ox = vec2<f32>(0.0, off * fw.x);
                wind_y[tap] += curve_winding(p0.yx - ox, p1.yx - ox, p2.yx - ox, inv_diameter.y);
            }
        }
    }

    var cov_y = 0.0;
    for (var tap = 0u; tap < taps; tap += 1u) {
        cov_y += clamp(abs(wind_y[tap]), 0.0, 1.0);
    }
    cov_y = cov_y / f32(taps);

    // Per-stripe x coverage averaged with the shared y coverage — the same
    // isotropic average the grayscale path uses, one channel at a time.
    //
    // (The issue spec said the y cast "multiplies in". It cannot: both casts
    // measure the *same* vertical edge on a stem, so a product would square
    // that stem's coverage and render it a good deal lighter than the
    // grayscale path renders it. Averaging keeps the two paths the same weight
    // and puts the whole difference in the per-stripe term, which is what the
    // option is for.)
    var cov = vec3<f32>(
        clamp(abs(wind_x[0]), 0.0, 1.0),
        clamp(abs(wind_x[1]), 0.0, 1.0),
        clamp(abs(wind_x[2]), 0.0, 1.0),
    );
    cov = (cov + vec3<f32>(cov_y)) * 0.5;
    cov = vec3<f32>(
        shape_coverage(cov.r, in.color.rgb),
        shape_coverage(cov.g, in.color.rgb),
        shape_coverage(cov.b, in.color.rgb),
    );

    // Alpha follows the widest stripe, so a glyph over a transparent target
    // punches the same hole the grayscale path punches.
    let alpha = max(max(cov.r, cov.g), cov.b) * in.color.a;
    var out: SubpixelOutput;
    out.color = vec4<f32>(in.color.rgb, alpha);
    out.coverage = vec4<f32>(cov * in.color.a, alpha);
    return out;
}
