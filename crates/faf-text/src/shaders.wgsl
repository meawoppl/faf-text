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
// with dynamic offsets are WebGL2-clean, storage buffers are not. The buffer's
// stride is a full 256 bytes, so the mat4 fits in the layout #4 shipped.
//
// `transform` maps block-local px to **screen-pixel space, homogeneous**: xy in
// physical pixels (y down, origin top-left), z the depth to emit, w the
// perspective divisor. It is not a px → clip matrix, and that is deliberate:
// the divide by `screen_size` below stays in the shader, so a 2D block whose
// transform is a plain translation computes the exact same floats the
// pre-matrix renderer did — bit for bit, not just visually. A host's
// view-projection is folded in on the CPU (`px_from_clip * vp * model`).
struct BlockXform {
    transform: mat4x4<f32>,
    flags: u32,
    // Three scalars, not a vec3: a vec3 would align to 16 and grow the struct.
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
};
@group(1) @binding(0) var<uniform> block_xf: BlockXform;

// The block's placement is an axis-aligned scale + translation, so pixel
// snapping means something. Must match BLOCK_SNAP in renderer.rs.
const BLOCK_SNAP: u32 = 1u;

// Must match CURVE_TEX_WIDTH in curves.rs.
const CURVE_TEX_WIDTH: u32 = 256u;
// Bands per axis. Must match BANDS in curves.rs.
const BANDS: u32 = 8u;
// Texels per band header entry: (descending list offset, curve count, split
// coordinate, ascending list offset). Must match BAND_ENTRY_TEXELS in curves.rs.
const BAND_ENTRY_TEXELS: u32 = 1u;
// Pixels per em below which a glyph counts as small: no median band split, and
// the band loop's early-out tested once per index texel instead of once per
// curve. Both are wins on long lists and losses on short ones — neighbouring
// samples pick opposite ray directions, and the tighter break turns four
// independent fetches into a dependency chain — which is why Lengyel gates the
// split on size too. The threshold sits a little under the paper's 32 px/em so
// a glyph rendered at exactly 32 (its own Table-2 workload) lands on the fast
// side of the fence rather than on it.
const SPLIT_MIN_PX_PER_EM: f32 = 30.0;
// Set in a vector instance's `count` field when the glyph's block opens with
// band tables instead of curve records. Must match BANDED_FLAG in curves.rs.
const BANDED_FLAG: u32 = 0x80000000u;
// True on the pipeline that draws variable-font glyphs, which carry a second
// master to interpolate toward. The single-master pipeline leaves it false and
// the whole master-B fetch folds away: the test would sit in the innermost
// loop, and even never taken it costs a quarter of the fragment time.
override BLEND_MASTERS: bool = false;

// True on the pipelines built for CoverageBlend::Linear, which render into the
// sRGB view of the target so the blender works in light-linear space. Same
// reasoning as BLEND_MASTERS: an override rather than a uniform, so the
// default (gamma) pipelines compile to exactly the shader they did before this
// existed.
override LINEAR_BLEND: bool = false;

// Contrast exponent for gamma-correct coverage. Blending in linear light is
// physically right and perceptually wrong for text: it thins dark-on-light
// stems and fattens light-on-dark ones. Skia-adjacent stacks correct that with
// a contrast/gamma boost applied to coverage before the blend, keyed on the
// text color; 1.43 is the usual constant (Skia's default text contrast lands
// in the same neighborhood, as does FreeType's `gamma` for LCD filtering).
// This is the simple luminance-conditional variant: dark text gets
// coverage^(1/1.43) (fatter), light text coverage^1.43 (thinner).
const COVERAGE_CONTRAST: f32 = 1.43;

// Coverage, conditioned for the space the blender is about to work in. Folds
// away to `coverage` on the gamma pipelines.
//
// `color` is already linear when LINEAR_BLEND is on (the CPU converted it), so
// the luminance test is a linear-light one — its 0.5 threshold sits around
// 0xbc in sRGB, which puts everything but near-white text on the "dark" side.
fn shape_coverage(coverage: f32, color: vec3<f32>) -> f32 {
    if LINEAR_BLEND {
        let luma = dot(color, vec3<f32>(0.2126, 0.7152, 0.0722));
        let exponent = select(1.0 / COVERAGE_CONTRAST, COVERAGE_CONTRAST, luma >= 0.5);
        return pow(coverage, exponent);
    }
    return coverage;
}

// Unit-quad corner from the vertex index (two CCW triangles, 6 vertices).
fn quad_corner(vi: u32) -> vec2<f32> {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
    );
    return corners[vi];
}

// Block-local pixels to homogeneous screen pixels. Every vertex of every
// pipeline goes through here. A translation-only transform reduces to
// `px + offset` exactly (multiplying by one and adding zero are exact), so a
// 2D block renders bit-for-bit like the pre-matrix renderer did.
fn place(px: vec2<f32>) -> vec4<f32> {
    return block_xf.transform * vec4<f32>(px, 0.0, 1.0);
}

// Homogeneous screen pixels to clip space. w is carried through untouched —
// no manual perspective divide — so the varyings interpolate
// perspective-correct and the fragment shader's `fwidth` sees the real
// on-screen footprint of a foreshortened glyph.
fn project(p: vec4<f32>) -> vec4<f32> {
    let ndc = p.xy / globals.screen_size * 2.0 - p.w;
    return vec4<f32>(ndc.x, -ndc.y, p.z, p.w);
}

fn to_clip(px: vec2<f32>) -> vec4<f32> {
    return project(place(px));
}

// Same, but snapped to whole pixels when the block is axis-aligned — the
// atlas path only, where a bitmap sampled off the pixel grid goes soft. Under
// any other transform there is no grid to snap to and the quad passes
// through: slightly soft emoji in 3D, which is the documented trade. The
// vector path never snaps; its coverage is analytic at any position.
fn to_clip_snapped(px: vec2<f32>) -> vec4<f32> {
    var p = place(px);
    if (block_xf.flags & BLOCK_SNAP) != 0u {
        p = vec4<f32>(round(p.xy), p.z, p.w);
    }
    return project(p);
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

// ---- Decorations (chips, underline, strikethrough, squiggle) ----
//
// One pipeline, two draws per block: chips go under the glyphs and line
// decorations over them. The kind switch is per decoration instance — a
// handful per block — not per glyph, so unlike the glyph shader (where a
// never-taken branch in the curve loop cost 25%) it is free in practice.
//
// `params` carries what the shape needs: a squiggle's amplitude, wavelength
// and stroke thickness in px, a chip's corner radius in px. All coverage is
// analytic: a signed distance in local px, divided by the px footprint of one
// local unit (`fwidth` of the interpolated local position, taken before the
// switch so derivatives stay in uniform control flow).

const DECO_KIND_SOLID: u32 = 0u;
const DECO_KIND_SQUIGGLE: u32 = 1u;
const DECO_KIND_CHIP: u32 = 2u;

const TAU: f32 = 6.2831853;

struct DecoInstance {
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) color: vec4<f32>,
    @location(3) params: vec4<f32>,
    @location(4) kind: u32,
};

struct DecoOutput {
    @builtin(position) clip: vec4<f32>,
    // Position within the rect, in the rect's own pixels.
    @location(0) local: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) @interpolate(flat) params: vec4<f32>,
    @location(3) @interpolate(flat) size: vec2<f32>,
    @location(4) @interpolate(flat) kind: u32,
};

@vertex
fn deco_vs(@builtin(vertex_index) vi: u32, inst: DecoInstance) -> DecoOutput {
    let corner = quad_corner(vi);
    var out: DecoOutput;
    out.clip = to_clip(inst.pos + corner * inst.size);
    out.local = corner * inst.size;
    out.color = inst.color;
    out.params = inst.params;
    out.size = inst.size;
    out.kind = inst.kind;
    return out;
}

// Signed distance to a rounded rect, negative inside (Quilez's formulation).
fn rounded_box_sdf(p: vec2<f32>, half: vec2<f32>, radius: f32) -> f32 {
    let q = abs(p) - half + vec2<f32>(radius);
    return length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - radius;
}

@fragment
fn deco_fs(in: DecoOutput) -> @location(0) vec4<f32> {
    // Local units per pixel: 1.0 for an unscaled block. Sampled up front —
    // derivatives may not be taken inside the switch below.
    let aa = max(max(fwidth(in.local.x), fwidth(in.local.y)), 1e-6);

    // Coverage as a signed distance, positive inside the shape.
    var inside = aa; // a solid kind fills its rect
    switch in.kind {
        case DECO_KIND_SQUIGGLE: {
            // Sine centerline through the middle of the band. The vertical
            // distance is divided by sqrt(1 + slope²) — the first-order
            // distance to the curve itself, which keeps the steep parts of the
            // wave the same width as the flat parts.
            let amplitude = in.params.x;
            let wavelength = max(in.params.y, 1e-3);
            let phase = in.local.x * TAU / wavelength;
            let center = in.size.y * 0.5 + amplitude * sin(phase);
            let slope = amplitude * cos(phase) * TAU / wavelength;
            let dist = abs(in.local.y - center) * inverseSqrt(1.0 + slope * slope);
            inside = in.params.z * 0.5 - dist;
        }
        case DECO_KIND_CHIP: {
            let half = in.size * 0.5;
            let radius = clamp(in.params.x, 0.0, min(half.x, half.y));
            inside = -rounded_box_sdf(in.local - half, half, radius);
        }
        default: {}
    }
    let coverage = clamp(inside / aa + 0.5, 0.0, 1.0);
    return vec4<f32>(in.color.rgb, in.color.a * coverage);
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
    out.clip = to_clip_snapped(inst.pos + corner * inst.size);
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
        return vec4<f32>(in.color.rgb, in.color.a * shape_coverage(texel.a, in.color.rgb));
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
//   [header: BANDS y-band then BANDS x-band entries, one texel each]
//   [index lists: two per band, texel-aligned, in header order]
//   [curve records: 2 texels each]
//
// A header entry is (descending list offset, curve count, split coordinate,
// ascending list offset), offsets in texels from `first`; a list entry is a
// curve record's texel offset from `first`. Bands split the glyph's em bbox
// uniformly — `fraction` is the sample's position in that box.
//
// The two lists hold the same curves ordered for the two ray directions —
// descending by their maximum coordinate along the ray axis, ascending by their
// minimum — so the loop can stop as soon as a fetched curve sits far enough
// behind the sample for Eq. 3's saturate to be exactly 0. Past the band's
// `split` coordinate the ray is fired backwards off the ascending list, so a
// sample near the right edge of a glyph never walks the curves on its left.
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

// One quadratic's masters, unblended. The banded path keeps them apart because
// its early-out has to bound BOTH of them (see `record_winding`); B mirrors A
// on the single-master pipeline, where the second fetch folds away entirely.
struct CurvePair {
    a0: vec2<f32>,
    a1: vec2<f32>,
    a2: vec2<f32>,
    b0: vec2<f32>,
    b1: vec2<f32>,
    b2: vec2<f32>,
};

fn fetch_pair(block: Block, record: u32) -> CurvePair {
    let t0 = textureLoad(curve_tex, texel_coord(block.base + record), 0);
    let t1 = textureLoad(curve_tex, texel_coord(block.base + record + 1u), 0);
    var pair = CurvePair(t0.xy, t0.zw, t1.xy, t0.xy, t0.zw, t1.xy);
    if BLEND_MASTERS {
        let b0 = textureLoad(curve_tex, texel_coord(block.b_base + record), 0);
        let b1 = textureLoad(curve_tex, texel_coord(block.b_base + record + 1u), 0);
        pair.b0 = b0.xy;
        pair.b1 = b0.zw;
        pair.b2 = b1.xy;
    }
    return pair;
}

fn blend_pair(pair: CurvePair, weight_t: f32) -> Curve {
    if BLEND_MASTERS {
        return Curve(
            mix(pair.a0, pair.b0, weight_t),
            mix(pair.a1, pair.b1, weight_t),
            mix(pair.a2, pair.b2, weight_t),
        );
    }
    return Curve(pair.a0, pair.a1, pair.a2);
}

fn fetch_curve(block: Block, record: u32) -> Curve {
    return blend_pair(fetch_pair(block, record), block.weight_t);
}

// The coordinate a ray runs along: x for the horizontal cast, y for the
// vertical one — which the shader fires by swapping the two coordinates, so it
// reads x there too.
fn ray_coord(p: vec2<f32>, swap: bool) -> f32 {
    return select(p.x, p.y, swap);
}

// How far along the ray one master's control points reach, scaled by the
// signed `inv`: the maximum for a forward ray, the minimum for a backward one
// (scaling by a negative number turns the max into the min). `shift` moves the
// sample along the ray, matching the `offset` the winding math subtracts.
fn ray_bound(
    p0: vec2<f32>,
    p1: vec2<f32>,
    p2: vec2<f32>,
    em: vec2<f32>,
    shift: f32,
    inv: f32,
    swap: bool,
) -> f32 {
    let c0 = (ray_coord(p0 - em, swap) - shift) * inv;
    let c1 = (ray_coord(p1 - em, swap) - shift) * inv;
    let c2 = (ray_coord(p2 - em, swap) - shift) * inv;
    return max(max(c0, c1), c2);
}

// One curve's contribution to a ray, and the bound that ends the band's loop.
// `record` is its texel offset from the block base; `swap` casts the vertical
// ray by exchanging the coordinates. `inv` is signed: negative fires the ray
// backwards along the axis, which turns Eq. 3's saturate(0.5 + m*Cx) into
// saturate(0.5 - m*Cx) without touching the winding math at all — the caller
// negates the sum to undo the crossing-sign swap that goes with it.
//
// Returns (winding, bound). `bound` is how far along the ray the curve's
// control points reach, in half-pixels from the sample: once it is below -0.5
// the saturate is exactly 0 for this curve — and, because the list is sorted on
// the same quantity, for every curve behind it.
fn record_winding(
    block: Block,
    record: f32,
    em: vec2<f32>,
    offset: vec2<f32>,
    inv: f32,
    swap: bool,
) -> vec2<f32> {
    let pair = fetch_pair(block, u32(record));
    let curve = blend_pair(pair, block.weight_t);
    var p0 = curve.p0 - em;
    var p1 = curve.p1 - em;
    var p2 = curve.p2 - em;
    if swap {
        p0 = p0.yx;
        p1 = p1.yx;
        p2 = p2.yx;
    }
    let q0 = p0 - offset;
    let q1 = p1 - offset;
    let q2 = p2 - offset;
    // Single master: the blended points ARE master A's, so this is the exact
    // quantity `curves.rs` sorted the list on.
    var bound = max(max(q0.x * inv, q1.x * inv), q2.x * inv);
    if BLEND_MASTERS {
        // Bounding the *blended* outline would be wrong here. The list was
        // sorted by the maximum over both masters — a blended control point
        // never leaves the masters' per-coordinate hull, so that key holds at
        // every weight_t — and a tighter bound could end the loop in front of
        // a curve that still crosses this ray.
        bound = max(
            ray_bound(pair.a0, pair.a1, pair.a2, em, offset.x, inv, swap),
            ray_bound(pair.b0, pair.b1, pair.b2, em, offset.x, inv, swap),
        );
    }
    return vec2<f32>(curve_winding(q0, q1, q2, inv), bound);
}

// Header entry `slot` (y-bands 0..BANDS, then x-bands): one texel each,
// (descending list offset, curve count, split coordinate, ascending list
// offset).
fn band_entry(base: u32, slot: u32) -> vec4<f32> {
    return textureLoad(curve_tex, texel_coord(base + slot * BAND_ENTRY_TEXELS), 0);
}

fn band_of(coord: f32) -> u32 {
    return u32(clamp(i32(coord * f32(BANDS)), 0, i32(BANDS) - 1));
}

// Winding of one band's list, walked until a curve falls behind the sample's
// antialiasing window. Indices come four to a texel; the curves the band leaves
// out — and the ones the early-out skips — contribute exactly zero, so the sum
// is the flat loop's sum, in a different order.
//
// `eager` picks how often the break is tested. Off, it is tested once per
// index texel, on the *last* curve of the four: the list is sorted, so that
// curve's bound is the smallest of the group and deciding on it is just as
// conservative, and the three curves a group can overrun contribute exactly
// zero anyway. On, every curve is tested, which stops sooner but makes each
// fetch wait on the previous one's compare instead of letting four issue
// together. Big glyphs walk long lists and want the tighter test (0.54 vs 0.59
// ms/frame in `examples/bench`); at 11 px the list is a texel or two long, the
// break almost never fires, and paying the dependency chain for it cost 7% in
// `examples/stress` — so it rides the same size gate the median split does.
fn band_winding(
    block: Block,
    list: u32,
    count: u32,
    em: vec2<f32>,
    offset: vec2<f32>,
    inv: f32,
    swap: bool,
    eager: bool,
) -> f32 {
    let base = block.base + list;
    var wind = 0.0;
    for (var i = 0u; i < count; i += 4u) {
        let indices = textureLoad(curve_tex, texel_coord(base + i / 4u), 0);
        let c0 = record_winding(block, indices.x, em, offset, inv, swap);
        wind += c0.x;
        var bound = c0.y;
        if eager && c0.y < -0.5 {
            break;
        }
        if i + 1u < count {
            let c1 = record_winding(block, indices.y, em, offset, inv, swap);
            wind += c1.x;
            bound = c1.y;
            if eager && c1.y < -0.5 {
                break;
            }
        }
        if i + 2u < count {
            let c2 = record_winding(block, indices.z, em, offset, inv, swap);
            wind += c2.x;
            bound = c2.y;
            if eager && c2.y < -0.5 {
                break;
            }
        }
        if i + 3u < count {
            let c3 = record_winding(block, indices.w, em, offset, inv, swap);
            wind += c3.x;
            bound = c3.y;
        }
        if bound < -0.5 {
            break;
        }
    }
    return wind;
}

// One band, fired toward the nearer end of the glyph. A forward ray counts the
// crossings ahead of the sample and stops at the first curve behind it, so it
// is cheap on the high side of a band and dear on the low side; a backward ray
// is the mirror image. Splitting at the median therefore roughly halves the
// average walk: samples below the split fire backwards, samples above keep
// going forwards.
//
// The two lists hold the same curves, so both directions compute the same
// number: saturate(0.5 - m*Cx) = 1 - saturate(0.5 + m*Cx), and the crossing
// signs of a closed contour sum to zero along the whole line, which is exactly
// what swapping them (the -1 below) accounts for.
//
// The choice is made in *data*, not control flow — one loop, walked with a
// different list offset and a flipped `inv`. Branching to two copies of the
// loop instead costs far more than the split saves: neighbouring fragments
// land on opposite sides of it all the time, and a warp then executes both.
fn band_rays(
    block: Block,
    entry: vec4<f32>,
    em: vec2<f32>,
    offset: vec2<f32>,
    inv_diameter: f32,
    swap: bool,
    big: bool,
) -> f32 {
    let back = big && ray_coord(em, swap) < entry.z;
    let list = select(u32(entry.x), u32(entry.w), back);
    let inv = select(inv_diameter, -inv_diameter, back);
    let sign = select(1.0, -1.0, back);
    return sign * band_winding(block, list, u32(entry.y), em, offset, inv, swap, big);
}

@fragment
fn vector_fs(in: VectorOutput) -> @location(0) vec4<f32> {
    // Em units per pixel, per axis; derivatives taken before any control flow.
    let fw = fwidth(in.em);
    let inv_diameter = 1.0 / fw;

    // Below ~24 px/em, hairline strokes can slip between the two axis rays;
    // three parallel rays per axis recover true area coverage. Larger glyphs
    // keep the cheap single-ray path.
    //
    // `fw` is per axis and comes from the real screen derivatives, so a glyph
    // foreshortened by a 3D transform is measured as it lands: the compressed
    // axis reports more em per pixel, `max` picks it, and a pane seen at a
    // grazing angle takes the three-tap path exactly where it needs it. No
    // part of the coverage math knows the block was rotated.
    let small = max(fw.x, fw.y) > (1.0 / 24.0);
    let taps = select(1u, 3u, small);

    let block = Block(in.first, in.b_first, in.weight_t);

    var wind_x = array<f32, 3>(0.0, 0.0, 0.0);
    var wind_y = array<f32, 3>(0.0, 0.0, 0.0);
    // Uniform per primitive, so the branch stays coherent across the quad.
    if (in.count & BANDED_FLAG) != 0u {
        // Big enough for the median split and the per-curve early-out; a
        // small glyph's band lists are too short for either to pay off.
        let big = max(fw.x, fw.y) * SPLIT_MIN_PX_PER_EM < 1.0;
        for (var tap = 0u; tap < taps; tap += 1u) {
            let off = (f32(tap) + 0.5) / f32(taps) - 0.5;
            // A tap can shift the ray across a band boundary at small sizes,
            // so the band is picked from the tapped coordinate, not the
            // fragment's — one extra header fetch buys exactness.
            let oy = vec2<f32>(0.0, off * fw.y);
            let y_band = band_of(in.fraction.y + off * fw.y * in.frac_per_em.y);
            wind_x[tap] = band_rays(
                block, band_entry(in.first, y_band), in.em, oy, inv_diameter.x, false, big);
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
    return vec4<f32>(in.color.rgb, in.color.a * shape_coverage(coverage, in.color.rgb));
}
