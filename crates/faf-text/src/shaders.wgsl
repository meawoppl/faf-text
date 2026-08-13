struct Globals {
    screen_size: vec2<f32>,
    _pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var atlas_tex: texture_2d<f32>;
@group(0) @binding(2) var atlas_samp: sampler;
@group(0) @binding(3) var curve_tex: texture_2d<f32>;

// Must match CURVE_TEX_WIDTH in curves.rs.
const CURVE_TEX_WIDTH: u32 = 256u;

// Unit-quad corner from the vertex index (two CCW triangles, 6 vertices).
fn quad_corner(vi: u32) -> vec2<f32> {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
    );
    return corners[vi];
}

fn to_clip(px: vec2<f32>) -> vec4<f32> {
    let ndc = px / globals.screen_size * 2.0 - 1.0;
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
// Each glyph instance references a run of quadratics in `curve_tex`, stored in
// em units (y-up, baseline origin). The fragment shader casts an axis ray
// through the sample point, solves y(t) = 0 for every curve, and accumulates
// signed, clamped crossing distances — the non-zero winding rule with analytic
// antialiasing baked into the clamp. No MSAA, no atlas, no re-raster on zoom.

struct VectorInstance {
    @location(0) pos: vec2<f32>,      // quad top-left, px
    @location(1) size: vec2<f32>,     // quad size, px
    @location(2) em_pos: vec2<f32>,   // em coords at quad top-left (y-up space)
    @location(3) em_size: vec2<f32>,  // em delta across quad (y negative)
    @location(4) color: vec4<f32>,
    @location(5) first: u32,
    @location(6) count: u32,
};

struct VectorOutput {
    @builtin(position) clip: vec4<f32>,
    @location(0) em: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) @interpolate(flat) first: u32,
    @location(3) @interpolate(flat) count: u32,
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
    return out;
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

    var wind_x = array<f32, 3>(0.0, 0.0, 0.0);
    var wind_y = array<f32, 3>(0.0, 0.0, 0.0);
    for (var i = 0u; i < in.count; i += 1u) {
        let texel = (in.first + i) * 2u;
        let coord0 = vec2<i32>(i32(texel % CURVE_TEX_WIDTH), i32(texel / CURVE_TEX_WIDTH));
        let coord1 = vec2<i32>(coord0.x + 1, coord0.y);
        let t0 = textureLoad(curve_tex, coord0, 0);
        let t1 = textureLoad(curve_tex, coord1, 0);
        let p0 = t0.xy - in.em;
        let p1 = t0.zw - in.em;
        let p2 = t1.xy - in.em;
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
    // abs() makes the result winding-orientation agnostic (TrueType and CFF
    // wind opposite directions); holes still cancel to zero before the abs.
    var coverage = 0.0;
    for (var tap = 0u; tap < taps; tap += 1u) {
        coverage += clamp(abs(wind_x[tap]), 0.0, 1.0) + clamp(abs(wind_y[tap]), 0.0, 1.0);
    }
    coverage = coverage / (2.0 * f32(taps));
    return vec4<f32>(in.color.rgb, in.color.a * coverage);
}
