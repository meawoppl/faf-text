struct Globals {
    screen_size: vec2<f32>,
    _pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var atlas_tex: texture_2d<f32>;
@group(0) @binding(2) var atlas_samp: sampler;
@group(0) @binding(3) var curve_tex: texture_2d<f32>;

// Where a retained block sits. One uniform buffer holds every block's copy and
// the draw loop binds this at the block's dynamic offset — uniform buffers
// with dynamic offsets are WebGL2-clean, storage buffers are not.
//
// Headroom: the buffer's stride is a full 256 bytes, so issue #9 can widen
// this to a mat4x4 (a pane placed in 3D) without touching the layout, the bind
// group or the draw loop — `place` becomes a matrix multiply and every vertex
// entry point below already routes through it.
struct BlockXform {
    offset: vec2<f32>,
    _pad: vec2<f32>,
};
@group(1) @binding(0) var<uniform> block_xf: BlockXform;

// Must match CURVE_TEX_WIDTH in curves.rs.
const CURVE_TEX_WIDTH: u32 = 256u;
// Bands per axis. Must match BANDS in curves.rs.
const BANDS: u32 = 8u;
// Set in a vector instance's `count` field when the glyph's block opens with
// band tables instead of curve records. Must match BANDED_FLAG in curves.rs.
const BANDED_FLAG: u32 = 0x80000000u;
// True on the pipeline that draws variable-font glyphs, which carry a second
// master to interpolate toward. The single-master pipeline leaves it false and
// the whole master-B fetch folds away: the test would sit in the innermost
// loop, and even never taken it costs a quarter of the fragment time.
override BLEND_MASTERS: bool = false;

// Unit-quad corner from the vertex index (two CCW triangles, 6 vertices).
fn quad_corner(vi: u32) -> vec2<f32> {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
    );
    return corners[vi];
}

// Block-local pixels to screen pixels. Adding zero is exact, so a block parked
// at the origin renders bit-for-bit like the unplaced geometry did.
fn place(px: vec2<f32>) -> vec2<f32> {
    return px + block_xf.offset;
}

fn to_clip(px: vec2<f32>) -> vec4<f32> {
    let ndc = place(px) / globals.screen_size * 2.0 - 1.0;
    return vec4<f32>(ndc.x, -ndc.y, 0.0, 1.0);
}

// ---- Solid rects (selection underlay / highlight overlay) ----

struct RectInstance {
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) color: vec4<f32>,
};

struct RectOutput {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn rect_vs(@builtin(vertex_index) vi: u32, inst: RectInstance) -> RectOutput {
    var out: RectOutput;
    out.clip = to_clip(inst.pos + quad_corner(vi) * inst.size);
    out.color = inst.color;
    return out;
}

@fragment
fn rect_fs(in: RectOutput) -> @location(0) vec4<f32> {
    return in.color;
}

// ---- Glyphs (instanced quads sampling the atlas) ----

const GLYPH_KIND_MASK: u32 = 0u;
const GLYPH_KIND_COLOR: u32 = 1u;

struct GlyphInstance {
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) uv_pos: vec2<f32>,
    @location(3) uv_size: vec2<f32>,
    @location(4) color: vec4<f32>,
    @location(5) kind: u32,
};

struct GlyphOutput {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) @interpolate(flat) kind: u32,
};

@vertex
fn glyph_vs(@builtin(vertex_index) vi: u32, inst: GlyphInstance) -> GlyphOutput {
    let corner = quad_corner(vi);
    var out: GlyphOutput;
    out.clip = to_clip(inst.pos + corner * inst.size);
    out.uv = inst.uv_pos + corner * inst.uv_size;
    out.color = inst.color;
    out.kind = inst.kind;
    return out;
}

@fragment
fn glyph_fs(in: GlyphOutput) -> @location(0) vec4<f32> {
    let texel = textureSampleLevel(atlas_tex, atlas_samp, in.uv, 0.0);
    if in.kind == GLYPH_KIND_MASK {
        // Grayscale coverage stored in alpha; tint with the instance color.
        return vec4<f32>(in.color.rgb, in.color.a * texel.a);
    }
    // Color glyph (emoji): texel carries its own color.
    return vec4<f32>(texel.rgb, texel.a * in.color.a);
}

// ---- Vector glyphs: per-pixel inside/outside against quadratic Béziers ----
//
// Each glyph instance references a block of quadratics in `curve_tex`, stored
// in em units (y-up, baseline origin). The fragment shader casts an axis ray
// through the sample point, solves y(t) = 0 for every curve, and accumulates
// signed, clamped crossing distances — the non-zero winding rule with analytic
// antialiasing baked into the clamp. No MSAA, no atlas, no re-raster on zoom.
//
// `first` is the block's base texel and `count` the number of quadratics. When
// `count` carries BANDED_FLAG the block opens with band tables (see the
// record-layout comment in curves.rs) and each ray reads only the curves that
// can cross it, instead of the whole glyph:
//
//   [header: BANDS y-band then BANDS x-band entries, 2 per texel]
//   [index lists: texel-aligned, in header order]
//   [curve records: 2 texels each]
//
// A header entry is (list offset in texels from `first`, curve count); a list
// entry is a curve record's texel offset from `first`. Bands split the glyph's
// em bbox uniformly — `fraction` is the sample's position in that box.
//
// A glyph from a variable font stores a second master (the same records at the
// wght axis maximum) starting at `b_first`, in the same order, so the twin of
// the record at `first + k` is at `b_first + k`. Those glyphs are drawn by the
// pipeline with BLEND_MASTERS on; everything else keeps a shader with no
// mention of a second master in it at all.

struct VectorInstance {
    @location(0) pos: vec2<f32>,      // quad top-left, px
    @location(1) size: vec2<f32>,     // quad size, px
    @location(2) em_pos: vec2<f32>,   // em coords at quad top-left (y-up space)
    @location(3) em_size: vec2<f32>,  // em delta across quad (y negative)
    @location(4) color: vec4<f32>,
    @location(5) first: u32,
    @location(6) count: u32,          // high bit: BANDED_FLAG
    @location(7) band_scale: vec2<f32>, // quad corner -> band space (0..1 over
    @location(8) band_bias: vec2<f32>,  // the bbox; the quad's pad falls outside)
    @location(9) weight_t: f32,       // 0 = wght axis min, 1 = axis max
    @location(10) b_first: u32,       // master-B base texel, 0 = single master
};

struct VectorOutput {
    @builtin(position) clip: vec4<f32>,
    @location(0) em: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) @interpolate(flat) first: u32,
    @location(3) @interpolate(flat) count: u32,
    @location(4) fraction: vec2<f32>,
    // Band-space units per em, for moving a tap's ray offset into band space.
    @location(5) @interpolate(flat) frac_per_em: vec2<f32>,
    @location(6) @interpolate(flat) b_first: u32,
    @location(7) @interpolate(flat) weight_t: f32,
};

// Where a glyph's curve records live, and how to blend its two masters.
struct Block {
    base: u32,
    b_base: u32,
    weight_t: f32,
};

@vertex
fn vector_vs(@builtin(vertex_index) vi: u32, inst: VectorInstance) -> VectorOutput {
    let corner = quad_corner(vi);
    var out: VectorOutput;
    out.clip = to_clip(inst.pos + corner * inst.size);
    out.em = inst.em_pos + corner * inst.em_size;
    out.color = inst.color;
    out.first = inst.first;
    out.count = inst.count;
    out.fraction = inst.band_bias + corner * inst.band_scale;
    out.frac_per_em = inst.band_scale / inst.em_size;
    out.b_first = inst.b_first;
    out.weight_t = inst.weight_t;
    return out;
}

fn texel_coord(texel: u32) -> vec2<i32> {
    return vec2<i32>(i32(texel % CURVE_TEX_WIDTH), i32(texel / CURVE_TEX_WIDTH));
}

// Crossing-count tables indexed by the sign pattern of (p0.y, p1.y, p2.y):
// code = (p0.y>0)*2 | (p1.y>0)*4 | (p2.y>0)*8. Classifying by endpoint signs
// (a point exactly ON the ray counts as "not above") makes shared endpoints
// and tangent points count consistently — root-range checks (t in [0,1))
// double-count or miss crossings when the ray grazes an endpoint exactly.
// t0 is always the downward crossing (+), t1 the upward one (-).
const T0_MASK: u32 = 0x454u;  // patterns {2, 4, 6, 10}
const T1_MASK: u32 = 0x1510u; // patterns {4, 8, 10, 12}

// Signed, antialiased contribution of one quadratic to the winding sum along
// the +x ray from the origin (points pre-translated so the sample point is
// the origin). Coverage AA comes from clamping the crossing distance.
fn curve_winding(p0: vec2<f32>, p1: vec2<f32>, p2: vec2<f32>, inv_diameter: f32) -> f32 {
    let code = select(0u, 2u, p0.y > 0.0) | select(0u, 4u, p1.y > 0.0) | select(0u, 8u, p2.y > 0.0);
    if code == 0u || code == 14u {
        return 0.0;
    }
    let a = p0 - 2.0 * p1 + p2;
    let b = p0 - p1;
    let c = p0;

    var alpha = 0.0;
    if abs(a.y) > 1e-4 {
        // The sign pattern guarantees the counted roots exist; max() guards
        // grazing cases where fp noise pushes the radicand barely negative
        // (the two crossings then coincide and cancel exactly).
        let s = sqrt(max(b.y * b.y - a.y * c.y, 0.0));
        let t0 = (b.y - s) / a.y;
        let t1 = (b.y + s) / a.y;
        if ((T0_MASK >> code) & 1u) != 0u {
            let x = (a.x * t0 - 2.0 * b.x) * t0 + c.x;
            alpha += clamp(x * inv_diameter + 0.5, 0.0, 1.0);
        }
        if ((T1_MASK >> code) & 1u) != 0u {
            let x = (a.x * t1 - 2.0 * b.x) * t1 + c.x;
            alpha -= clamp(x * inv_diameter + 0.5, 0.0, 1.0);
        }
    } else {
        // (Near-)linear in y: one crossing, direction from the endpoint signs.
        var sign = 0.0;
        if p0.y > 0.0 && p2.y <= 0.0 {
            sign = 1.0;
        } else if p2.y > 0.0 && p0.y <= 0.0 {
            sign = -1.0;
        }
        if sign != 0.0 {
            let t = c.y / (2.0 * b.y);
            let x = (a.x * t - 2.0 * b.x) * t + c.x;
            alpha = sign * clamp(x * inv_diameter + 0.5, 0.0, 1.0);
        }
    }
    return alpha;
}

// One quadratic in em units. Points come from master A, pulled toward master B
// by `weight_t` on the blending pipeline: a control point is affine in the
// masters, so blending the points IS the blended outline.
struct Curve {
    p0: vec2<f32>,
    p1: vec2<f32>,
    p2: vec2<f32>,
};

fn fetch_curve(block: Block, record: u32) -> Curve {
    let t0 = textureLoad(curve_tex, texel_coord(block.base + record), 0);
    let t1 = textureLoad(curve_tex, texel_coord(block.base + record + 1u), 0);
    var curve = Curve(t0.xy, t0.zw, t1.xy);
    if BLEND_MASTERS {
        let b0 = textureLoad(curve_tex, texel_coord(block.b_base + record), 0);
        let b1 = textureLoad(curve_tex, texel_coord(block.b_base + record + 1u), 0);
        curve.p0 = mix(curve.p0, b0.xy, block.weight_t);
        curve.p1 = mix(curve.p1, b0.zw, block.weight_t);
        curve.p2 = mix(curve.p2, b1.xy, block.weight_t);
    }
    return curve;
}

// One curve's contribution to a ray. `record` is its texel offset from the
// block base; `swap` casts the vertical ray by exchanging the coordinates.
fn record_winding(
    block: Block,
    record: f32,
    em: vec2<f32>,
    offset: vec2<f32>,
    inv_diameter: f32,
    swap: bool,
) -> f32 {
    let curve = fetch_curve(block, u32(record));
    var p0 = curve.p0 - em;
    var p1 = curve.p1 - em;
    var p2 = curve.p2 - em;
    if swap {
        p0 = p0.yx;
        p1 = p1.yx;
        p2 = p2.yx;
    }
    return curve_winding(p0 - offset, p1 - offset, p2 - offset, inv_diameter);
}

// Header entry `slot` (y-bands 0..BANDS, then x-bands): two entries per texel.
fn band_entry(base: u32, slot: u32) -> vec2<f32> {
    let texel = textureLoad(curve_tex, texel_coord(base + slot / 2u), 0);
    return select(texel.zw, texel.xy, (slot & 1u) == 0u);
}

fn band_of(coord: f32) -> u32 {
    return u32(clamp(i32(coord * f32(BANDS)), 0, i32(BANDS) - 1));
}

// Winding of every curve in one band's list. Indices come four to a texel;
// the list order matches the flat loop's, and the curves it leaves out
// contribute exactly zero, so both paths sum the same terms in the same order.
fn band_winding(
    block: Block,
    entry: vec2<f32>,
    em: vec2<f32>,
    offset: vec2<f32>,
    inv_diameter: f32,
    swap: bool,
) -> f32 {
    let list = block.base + u32(entry.x);
    let count = u32(entry.y);
    var wind = 0.0;
    for (var i = 0u; i < count; i += 4u) {
        let indices = textureLoad(curve_tex, texel_coord(list + i / 4u), 0);
        wind += record_winding(block, indices.x, em, offset, inv_diameter, swap);
        if i + 1u < count {
            wind += record_winding(block, indices.y, em, offset, inv_diameter, swap);
        }
        if i + 2u < count {
            wind += record_winding(block, indices.z, em, offset, inv_diameter, swap);
        }
        if i + 3u < count {
            wind += record_winding(block, indices.w, em, offset, inv_diameter, swap);
        }
    }
    return wind;
}

@fragment
fn vector_fs(in: VectorOutput) -> @location(0) vec4<f32> {
    // Em units per pixel, per axis; derivatives taken before any control flow.
    let fw = fwidth(in.em);
    let inv_diameter = 1.0 / fw;

    // Below ~24 px/em, hairline strokes can slip between the two axis rays;
    // three parallel rays per axis recover true area coverage. Larger glyphs
    // keep the cheap single-ray path.
    let small = max(fw.x, fw.y) > (1.0 / 24.0);
    let taps = select(1u, 3u, small);

    let block = Block(in.first, in.b_first, in.weight_t);

    var wind_x = array<f32, 3>(0.0, 0.0, 0.0);
    var wind_y = array<f32, 3>(0.0, 0.0, 0.0);
    // Uniform per primitive, so the branch stays coherent across the quad.
    if (in.count & BANDED_FLAG) != 0u {
        for (var tap = 0u; tap < taps; tap += 1u) {
            let off = (f32(tap) + 0.5) / f32(taps) - 0.5;
            // A tap can shift the ray across a band boundary at small sizes,
            // so the band is picked from the tapped coordinate, not the
            // fragment's — one extra header fetch buys exactness.
            let oy = vec2<f32>(0.0, off * fw.y);
            let y_band = band_of(in.fraction.y + off * fw.y * in.frac_per_em.y);
            wind_x[tap] = band_winding(
                block, band_entry(in.first, y_band), in.em, oy, inv_diameter.x, false);
            let ox = vec2<f32>(0.0, off * fw.x);
            let x_band = band_of(in.fraction.x + off * fw.x * in.frac_per_em.x);
            wind_y[tap] = band_winding(
                block, band_entry(in.first, BANDS + x_band), in.em, ox, inv_diameter.y, true);
        }
    } else {
        for (var i = 0u; i < in.count; i += 1u) {
            let curve = fetch_curve(block, i * 2u);
            let p0 = curve.p0 - in.em;
            let p1 = curve.p1 - in.em;
            let p2 = curve.p2 - in.em;
            for (var tap = 0u; tap < taps; tap += 1u) {
                let off = (f32(tap) + 0.5) / f32(taps) - 0.5;
                // Horizontal ray, offset perpendicular (in y)…
                let oy = vec2<f32>(0.0, off * fw.y);
                wind_x[tap] += curve_winding(p0 - oy, p1 - oy, p2 - oy, inv_diameter.x);
                // …and vertical ray (coords swapped), offset in x.
                let ox = vec2<f32>(0.0, off * fw.x);
                wind_y[tap] += curve_winding(p0.yx - ox, p1.yx - ox, p2.yx - ox, inv_diameter.y);
            }
        }
    }
    // abs() makes the result winding-orientation agnostic (TrueType and CFF
    // wind opposite directions); holes still cancel to zero before the abs.
    var coverage = 0.0;
    for (var tap = 0u; tap < taps; tap += 1u) {
        coverage += clamp(abs(wind_x[tap]), 0.0, 1.0) + clamp(abs(wind_y[tap]), 0.0, 1.0);
    }
    coverage = coverage / (2.0 * f32(taps));
    return vec4<f32>(in.color.rgb, in.color.a * coverage);
}
