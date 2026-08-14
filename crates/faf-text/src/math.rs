//! Placement math: the matrices a block is drawn with, and the ray test that
//! turns a pointer back into block-local pixels.
//!
//! # Spaces
//!
//! - **Block-local px** — where a block's instances live: x right, y down,
//!   origin at the block's top-left, one unit = one physical pixel. Everything
//!   [`crate::TextView`] produces (glyph quads, selection rects, caret
//!   geometry) is in this space, minus the block's own origin.
//! - **World** — where a block's *model* matrix puts it. With the default
//!   view-projection, world **is** screen pixels (x right, y down, z into the
//!   screen), so a 2D block's model matrix is a plain translation and a 3D one
//!   rotates around a point measured in pixels.
//! - **Clip** — what the vertex shader emits, y up, depth 0..1 (the wgpu
//!   convention).
//!
//! The default view-projection is [`screen_ortho`], which the renderer applies
//! itself; [`screen_perspective`] is the drop-in 3D replacement, built so that
//! blocks left at z = 0 land on exactly the pixels they would have in 2D.
//!
//! # Pointer → block-local px
//!
//! Hosts hit-test through a ray:
//!
//! ```no_run
//! # use faf_text::glam::Mat4;
//! use faf_text::math::{block_hit, ndc_ray, pointer_ndc};
//! # let (model, vp) = (Mat4::IDENTITY, Mat4::IDENTITY);
//! # let (screen, pointer_px) = ([960.0, 600.0], [120.0, 80.0]);
//! let ndc = pointer_ndc(screen, pointer_px);         // physical px → NDC
//! let (origin, dir) = ndc_ray(&vp, ndc);             // NDC → world ray
//! let local = block_hit(&model, &vp, origin, dir);
//! // `local` is block-local px: add the block's origin and feed it to
//! // `TextView::hit`, which takes over exactly as it does in 2D.
//! ```

use glam::{DVec4, Mat4, Vec2, Vec3};

/// Screen-pixel space to clip space: the projection every block gets by
/// default, and the one the renderer's own last step applies.
///
/// x runs right, y runs **down** from the top-left corner, and z is left
/// alone, so a point at z = 0 lands on the near plane where alpha-blended text
/// wants it.
pub fn screen_ortho(size: [f32; 2]) -> Mat4 {
    Mat4::from_cols_array_2d(&[
        [2.0 / size[0], 0.0, 0.0, 0.0],
        [0.0, -2.0 / size[1], 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [-1.0, 1.0, 0.0, 1.0],
    ])
}

/// Inverse of [`screen_ortho`]: clip space back to screen pixels, homogeneous
/// (w is carried through untouched, so a perspective point survives the trip).
///
/// The renderer composes this with a host's view-projection because its own
/// last step is [`screen_ortho`] — see the note on [`crate::TextRenderer::set_view_projection`].
pub(crate) fn px_from_clip(size: [f32; 2]) -> Mat4 {
    Mat4::from_cols_array_2d(&[
        [size[0] * 0.5, 0.0, 0.0, 0.0],
        [0.0, -size[1] * 0.5, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [size[0] * 0.5, size[1] * 0.5, 0.0, 1.0],
    ])
}

/// A perspective camera for text in 3D, aimed so that the plane z = 0 is the
/// screen: a block left there renders on exactly the pixels [`screen_ortho`]
/// would have put it on, and one rotated out of it foreshortens around where
/// it was.
///
/// The eye sits at `(width/2, height/2, -distance)` looking toward +z, with
/// world +y running down the screen — the same axes as 2D, so a host can turn
/// 3D on without renumbering anything. `fov_y` is the vertical field of view in
/// radians (0.7 ≈ 40° is a good default); it only changes how strong the
/// foreshortening is, never where the z = 0 plane lands.
pub fn screen_perspective(size: [f32; 2], fov_y: f32) -> Mat4 {
    let focal = 1.0 / (fov_y * 0.5).tan();
    // Distance at which a `size[1]`-tall plane exactly fills the frustum.
    let distance = size[1] * 0.5 * focal;
    // glam 0.33 deprecated `Mat4::perspective_lh` for these named-convention
    // helpers; `directx` is the wgpu one — left-handed view space in, NDC with
    // y up and depth 0..1 out.
    let proj = glam::camera::lh::proj::directx::perspective(
        fov_y,
        size[0] / size[1],
        distance * 0.01,
        distance * 100.0,
    );
    // World (x right, y down, z into the screen) to a left-handed view space
    // (x right, y up, z forward): flip y and push the origin out to the eye.
    let view = Mat4::from_cols_array_2d(&[
        [1.0, 0.0, 0.0, 0.0],
        [0.0, -1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [-size[0] * 0.5, size[1] * 0.5, distance, 1.0],
    ]);
    proj * view
}

/// Physical pixel on the surface to normalized device coordinates: the first
/// step of the pointer → ray recipe. NDC y is **up**, so this flips it.
pub fn pointer_ndc(size: [f32; 2], pos: [f32; 2]) -> Vec2 {
    Vec2::new(pos[0] / size[0] * 2.0 - 1.0, 1.0 - pos[1] / size[1] * 2.0)
}

/// Unproject an NDC point into a world-space ray `(origin, direction)`, by
/// running the near and far plane points back through the view-projection.
///
/// The direction is normalized; under an orthographic `vp` every ray is
/// parallel, which is exactly right.
///
/// Inverting a projection and then walking hundreds of pixels along the result
/// is where a hit test loses its accuracy, so the arithmetic here (and in
/// [`block_hit`]) is done in `f64`. It costs nothing — this runs once per
/// pointer event — and it keeps the round trip inside a hundredth of a pixel.
pub fn ndc_ray(vp: &Mat4, ndc: Vec2) -> (Vec3, Vec3) {
    let inv = vp.as_dmat4().inverse();
    let unproject = |z: f64| {
        let p = inv * DVec4::new(ndc.x as f64, ndc.y as f64, z, 1.0);
        if p.w == 0.0 {
            p.truncate()
        } else {
            p.truncate() / p.w
        }
    };
    // wgpu clip depth runs 0 (near) to 1 (far).
    let near = unproject(0.0);
    let far = unproject(1.0);
    (near.as_vec3(), (far - near).normalize_or_zero().as_vec3())
}

/// Intersect a world-space ray with a block's plane, and return where it hit in
/// **block-local pixels** — the coordinates [`crate::TextView::hit`] takes, so
/// 3D hit-testing hands straight back to the 2D path.
///
/// A block is the plane z = 0 of its own space, facing the viewer (who is at
/// negative z under both [`screen_ortho`] and [`screen_perspective`]). The
/// answer is unbounded: the block's content decides what is actually under the
/// pointer, not this function.
///
/// `None` when the block is edge-on, when it is turned away (the ray would hit
/// its back), when the plane is behind the ray's origin, or when the hit point
/// projects behind the camera — which is why `vp` is wanted as well as the
/// model matrix.
pub fn block_hit(model: &Mat4, vp: &Mat4, ray_origin: Vec3, ray_dir: Vec3) -> Option<[f32; 2]> {
    let inv = model.as_dmat4().inverse();
    if !inv.is_finite() {
        return None; // degenerate placement: no plane to hit
    }
    let origin = inv * ray_origin.as_dvec3().extend(1.0);
    if origin.w == 0.0 {
        return None;
    }
    let origin = origin.truncate() / origin.w;
    let dir = (inv * ray_dir.as_dvec3().extend(0.0)).truncate();

    // Block space is the same handedness as the screen: the viewer is at
    // negative z, so a ray reaching the front face travels toward +z.
    if dir.z <= 0.0 {
        return None;
    }
    let t = -origin.z / dir.z;
    if t < 0.0 {
        return None;
    }
    let hit = origin + dir * t;

    // Under a perspective vp, everything behind the eye projects to a mirrored
    // ghost in front of it. w > 0 is the standard way to throw that away.
    let clip = vp.as_dmat4() * model.as_dmat4() * DVec4::new(hit.x, hit.y, 0.0, 1.0);
    (clip.w > 0.0).then_some([hit.x as f32, hit.y as f32])
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::Vec4;

    const SIZE: [f32; 2] = [960.0, 600.0];

    /// Project a block-local point the way the renderer does.
    fn project(model: &Mat4, vp: &Mat4, local: [f32; 2]) -> Vec3 {
        let clip = *vp * *model * Vec4::new(local[0], local[1], 0.0, 1.0);
        clip.truncate() / clip.w
    }

    #[test]
    fn the_screen_ortho_maps_pixels_onto_the_corners_of_clip_space() {
        let m = screen_ortho(SIZE);
        let corner = |px: [f32; 2]| (m * Vec4::new(px[0], px[1], 0.0, 1.0)).truncate();
        assert_eq!(corner([0.0, 0.0]), Vec3::new(-1.0, 1.0, 0.0), "top left");
        assert_eq!(corner(SIZE), Vec3::new(1.0, -1.0, 0.0), "bottom right");
        assert_eq!(corner([480.0, 300.0]), Vec3::ZERO, "the middle");
    }

    #[test]
    fn px_from_clip_undoes_the_screen_ortho() {
        let round_trip = px_from_clip(SIZE) * screen_ortho(SIZE);
        for (a, b) in round_trip
            .to_cols_array()
            .iter()
            .zip(Mat4::IDENTITY.to_cols_array())
        {
            assert!((a - b).abs() < 1e-5, "{round_trip} is not the identity");
        }
    }

    #[test]
    fn a_block_left_at_z_zero_projects_the_same_under_perspective_as_in_2d() {
        let ortho = screen_ortho(SIZE);
        let perspective = screen_perspective(SIZE, 0.7);
        for px in [[0.0, 0.0], SIZE, [120.0, 480.0], [913.0, 17.5]] {
            let flat = project(&Mat4::IDENTITY, &ortho, px);
            let deep = project(&Mat4::IDENTITY, &perspective, px);
            assert!(
                (flat.x - deep.x).abs() < 1e-5 && (flat.y - deep.y).abs() < 1e-5,
                "{px:?}: ortho {flat} vs perspective {deep}"
            );
        }
    }

    /// The round trip the hosts rely on: a known block-local point, projected
    /// to NDC, then recovered from the ray that NDC casts.
    #[test]
    fn a_ray_cast_at_a_projected_point_lands_back_on_it() {
        let vp = screen_perspective(SIZE, 0.7);
        // Pivot on the eye's own axis: a pane tilted this far anywhere else
        // would have its front face swung away from the camera entirely.
        let center = Vec3::new(SIZE[0] * 0.5, SIZE[1] * 0.5, 0.0);
        // A pane rotated about its own center, the way a host would.
        let pane = |degrees: f32| {
            Mat4::from_translation(center)
                * Mat4::from_rotation_y(degrees.to_radians())
                * Mat4::from_translation(-center)
        };
        let round_trip = |degrees: f32, local: [f32; 2]| {
            let model = pane(degrees);
            let ndc = project(&model, &vp, local);
            let (origin, dir) = ndc_ray(&vp, ndc.truncate());
            let hit = block_hit(&model, &vp, origin, dir)
                .unwrap_or_else(|| panic!("{degrees}° missed at {local:?}"));
            assert!(
                (hit[0] - local[0]).abs() < 0.01 && (hit[1] - local[1]).abs() < 0.01,
                "{degrees}°: {local:?} came back as {hit:?}"
            );
        };

        // Around the pivot, every angle round-trips — including one so close
        // to edge-on that the pane is 20 px wide on screen.
        for degrees in [0.0f32, 20.0, -45.0, 72.0, 87.0] {
            for local in [[480.0, 300.0], [540.0, 360.0], [421.5, 238.25]] {
                round_trip(degrees, local);
            }
        }
        // Out at the corners only up to a moderate tilt: at 87° the pane's
        // plane passes within a few dozen px of the eye, so its far half is
        // genuinely seen from behind and `block_hit` is right to refuse it.
        for degrees in [0.0f32, 20.0, -45.0] {
            for local in [[0.0, 0.0], [860.0, 580.0], [42.5, 91.25]] {
                round_trip(degrees, local);
            }
        }
    }

    #[test]
    fn the_ray_test_rejects_edge_on_and_back_facing_blocks() {
        let vp = screen_perspective(SIZE, 0.7);
        let center = Vec3::new(480.0, 300.0, 0.0);
        let pane = |degrees: f32| {
            Mat4::from_translation(center)
                * Mat4::from_rotation_y(degrees.to_radians())
                * Mat4::from_translation(-center)
        };
        let (origin, dir) = ndc_ray(&vp, Vec2::ZERO);
        assert!(block_hit(&pane(0.0), &vp, origin, dir).is_some(), "head on");
        assert!(
            block_hit(&pane(90.0), &vp, origin, dir).is_none(),
            "edge on"
        );
        assert!(block_hit(&pane(180.0), &vp, origin, dir).is_none(), "back");
        assert!(
            block_hit(&Mat4::ZERO, &vp, origin, dir).is_none(),
            "a collapsed model matrix has no plane to hit"
        );
        // A ray fired away from the plane never reaches it.
        assert!(block_hit(&pane(0.0), &vp, origin, -dir).is_none());
    }

    #[test]
    fn an_orthographic_ray_is_the_pointer_itself() {
        let vp = screen_ortho(SIZE);
        for px in [[10.0, 10.0], [480.0, 300.0], [947.0, 588.0]] {
            let (origin, dir) = ndc_ray(&vp, pointer_ndc(SIZE, px));
            let hit = block_hit(&Mat4::IDENTITY, &vp, origin, dir).expect("hit");
            assert!(
                (hit[0] - px[0]).abs() < 0.01 && (hit[1] - px[1]).abs() < 0.01,
                "{px:?} came back as {hit:?}"
            );
            assert!((dir - Vec3::Z).length() < 1e-5, "parallel, straight in");
        }
    }
}
