//! Three panes of text hung in a 3D scene, rendered to panes3d.png without a
//! window.
//!
//! Nothing about the glyphs changes to put them there: each pane is a retained
//! block with a model matrix, the scene shares one perspective camera, and the
//! coverage the fragment shader computes is measured on the projected glyph by
//! `fwidth`. The right-hand pane is turned 76° — a sliver about a quarter of
//! its real width — and its stems are still analytically antialiased.
//!
//! The middle pane also carries a selection that was hit-tested through the
//! projection: two synthetic pointer positions are turned into world rays,
//! `math::block_hit` drops them onto the pane's own plane, and from there the
//! ordinary 2D `TextView::hit` picks the cursors.

use faf_text::glam::{Mat4, Vec3};
use faf_text::math::{block_hit, ndc_ray, pointer_ndc, screen_perspective};
use faf_text::{Attrs, BlockContent, Color, Cursor, Family, Metrics, Rect, TextRenderer, TextView};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 800;
const SIZE: [f32; 2] = [WIDTH as f32, HEIGHT as f32];
/// Vertical field of view. Wide enough that the foreshortening reads, narrow
/// enough that a grazing pane does not run into the near plane.
const FOV_Y: f32 = 0.7;

const FG: Color = Color::rgba(0.75, 0.79, 0.96, 1.0);
const ACCENT: Color = Color::rgba(0.48, 0.64, 0.97, 1.0);
const SELECTION: Color = Color::rgba(0.23, 0.39, 0.66, 1.0);

/// A pane: text in its own block-local pixel space, plus where that space sits
/// in the world.
struct Pane {
    view: TextView,
    model: Mat4,
}

impl Pane {
    /// A pane of `width` px, turned `degrees` about its own vertical axis and
    /// parked with its top-left corner at `at` (world px, which under this
    /// camera is where it would have been in 2D).
    fn new(
        font_system: &mut faf_text::FontSystem,
        title: &str,
        body: &str,
        size: f32,
        width: f32,
        at: [f32; 2],
        degrees: f32,
    ) -> Self {
        let mut view = TextView::new(font_system, Metrics::new(size, size * 1.45));
        view.set_size(font_system, Some(width), None);
        view.set_text(
            font_system,
            &format!("{title}\n{body}"),
            &Attrs::new().family(Family::SansSerif),
        );
        // Turn about the middle of the pane so it opens toward the camera
        // instead of swinging out of frame.
        let pivot = Vec3::new(width * 0.5, 0.0, 0.0);
        let model = Mat4::from_translation(Vec3::new(at[0], at[1], 0.0) + pivot)
            * Mat4::from_rotation_y(degrees.to_radians())
            * Mat4::from_translation(-pivot);
        Self { view, model }
    }

    /// Distance from the eye to the pane's origin, for the painter's-order
    /// sort. The camera looks down +z from `-distance`, so a bigger world z is
    /// further away.
    fn depth(&self, vp: &Mat4) -> f32 {
        (*vp * self.model * faf_text::glam::vec4(0.0, 0.0, 0.0, 1.0)).w
    }
}

fn main() {
    pollster::block_on(run());
}

async fn run() {
    let instance = wgpu::Instance::default();
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        })
        .await
        .expect("no GPU adapter available");
    println!(
        "adapter: {} ({:?})",
        adapter.get_info().name,
        adapter.get_info().backend
    );
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor::default())
        .await
        .expect("failed to acquire device");

    let format = wgpu::TextureFormat::Rgba8Unorm;
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("panes3d target"),
        size: wgpu::Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());

    let mut font_system = faf_text::font_system_from_fonts(&[
        faf_text::FONT_DEJAVU_SANS,
        faf_text::FONT_DEJAVU_SANS_MONO,
    ]);
    let mut renderer = TextRenderer::new(&device, format);

    // One camera for the whole scene. It is aimed so that a pane left at z = 0
    // lands on exactly the pixels 2D would have given it, which makes the
    // world coordinates below plain layout numbers.
    let vp = screen_perspective(SIZE, FOV_Y);
    renderer.set_view_projection(vp.to_cols_array_2d());

    let panes = [
        Pane::new(
            &mut font_system,
            "Oblique",
            "Every glyph on this pane is still evaluated per pixel from its \
             Bézier outline. Turning the pane re-uploaded one matrix — no \
             instance was rebuilt, no curve re-extracted, nothing rasterized \
             at this angle or any other.",
            21.0,
            360.0,
            [140.0, 430.0],
            -34.0,
        ),
        Pane::new(
            &mut font_system,
            "Hit-tested",
            "The selection under this line was painted from a synthetic \
             pointer drag: screen pixel to NDC ray, block_hit onto this pane's \
             own plane, then the same TextView::hit the flat 2D path uses. \
             Selection geometry rides the pane because it is in the same block.",
            19.0,
            460.0,
            [620.0, 120.0],
            24.0,
        ),
        Pane::new(
            &mut font_system,
            "Grazing",
            "Seen almost edge-on. Coverage comes from fwidth of an \
             interpolated varying, so it is measured on the glyph as it \
             lands: compressed one way, full size the other. The stems stay \
             smooth, and the small-size three-tap path triggers off the \
             coarser axis.",
            22.0,
            700.0,
            [600.0, 420.0],
            76.0,
        ),
    ];

    // The selection: two pointer positions on the middle pane, put through the
    // whole host recipe. They are derived by projecting two block-local points
    // so the example is deterministic — a real host gets them from an event.
    let (drag_from, drag_to) = ([2.0, 84.0], [352.0, 122.0]);
    let selection = drag_select(&panes[1], &vp, drag_from, drag_to);
    let selected: Vec<Rect> = match selection {
        Some((a, b)) => {
            println!(
                "ray hit-test selected {:?}..{:?}: {:?}",
                (a.line, a.index),
                (b.line, b.index),
                selected_text(&panes[1].view, a, b)
            );
            panes[1].view.selection_rects(a, b)
        }
        None => {
            println!("ray missed the pane");
            Vec::new()
        }
    };
    let under_rects: Vec<(Rect, Color)> = selection
        .map(|_| selected.iter().map(|&r| (r, SELECTION)).collect())
        .unwrap_or_default();

    for (i, pane) in panes.iter().enumerate() {
        let block = renderer.create_block();
        renderer.set_block_content(
            &queue,
            &mut font_system,
            block,
            &BlockContent {
                buffer: Some(&pane.view.buffer),
                pos: [0.0, 0.0],
                default_color: FG,
                under_rects: if i == 1 { &under_rects } else { &[] },
                ..Default::default()
            },
        );
        renderer.set_block_transform(block, pane.model.to_cols_array_2d());
        // Painter's order: no depth buffer, alpha-blended text, so the pane
        // furthest from the eye has to be drawn first. Lower key draws first,
        // and a bigger `w` is further away.
        renderer.set_block_z(block, -pane.depth(&vp));
    }

    // A title in the plane of the screen, left at z = 0 — under this camera it
    // is pixel-exact 2D, which is the point of the projection helper.
    let mut title = TextView::new(&mut font_system, Metrics::new(44.0, 52.0));
    title.set_text(
        &mut font_system,
        "text in 3D spaces",
        &Attrs::new().family(Family::SansSerif),
    );
    let flat = renderer.create_block();
    renderer.set_block_content(
        &queue,
        &mut font_system,
        flat,
        &BlockContent {
            buffer: Some(&title.buffer),
            default_color: ACCENT,
            ..Default::default()
        },
    );
    renderer.set_block_offset(flat, [40.0, 40.0]);
    renderer.set_block_z(flat, 0.0);

    renderer.finish(&device, &queue, SIZE);

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("panes3d pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &target_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.055,
                        g: 0.059,
                        b: 0.082,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        renderer.render(&mut pass);
    }

    let padded_row = (WIDTH * 4).next_multiple_of(256);
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded_row * HEIGHT) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_row),
                rows_per_image: None,
            },
        },
        wgpu::Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        },
    );
    queue.submit([encoder.finish()]);

    let slice = readback.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map failed"));
    device.poll(wgpu::PollType::wait_indefinitely()).unwrap();

    let data = slice.get_mapped_range().unwrap();
    let mut pixels = Vec::with_capacity((WIDTH * HEIGHT * 4) as usize);
    for row in 0..HEIGHT {
        let start = (row * padded_row) as usize;
        pixels.extend_from_slice(&data[start..start + (WIDTH * 4) as usize]);
    }

    let file = std::fs::File::create("panes3d.png").unwrap();
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), WIDTH, HEIGHT);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()
        .unwrap()
        .write_image_data(&pixels)
        .unwrap();
    println!("wrote panes3d.png");
}

/// A drag across a pane, hit-tested the way a 3D host has to: the two ends are
/// **screen pixels**, and everything after that is the published recipe.
///
/// The pixels themselves are worked out by projecting two block-local points,
/// so the example selects the same words every run without hard-coding numbers
/// that depend on the camera.
fn drag_select(pane: &Pane, vp: &Mat4, from: [f32; 2], to: [f32; 2]) -> Option<(Cursor, Cursor)> {
    let cursor_at = |local: [f32; 2]| {
        let pixel = project_to_pixel(pane, vp, local)?;
        // ---- what a host does on a pointer event ----
        let (origin, dir) = ndc_ray(vp, pointer_ndc(SIZE, pixel));
        let hit = block_hit(&pane.model, vp, origin, dir)?;
        pane.view.hit(hit[0], hit[1])
        // ---------------------------------------------
    };
    Some((cursor_at(from)?, cursor_at(to)?))
}

/// Where a block-local point lands on the surface, in physical pixels.
fn project_to_pixel(pane: &Pane, vp: &Mat4, local: [f32; 2]) -> Option<[f32; 2]> {
    let clip = *vp * pane.model * faf_text::glam::vec4(local[0], local[1], 0.0, 1.0);
    if clip.w <= 0.0 {
        return None;
    }
    let ndc = clip.truncate() / clip.w;
    Some([(ndc.x * 0.5 + 0.5) * SIZE[0], (0.5 - ndc.y * 0.5) * SIZE[1]])
}

/// The text between two cursors, for the console.
fn selected_text(view: &TextView, a: Cursor, b: Cursor) -> String {
    let (start, end) = if a <= b { (a, b) } else { (b, a) };
    let mut out = String::new();
    for line in start.line..=end.line {
        let Some(text) = view.buffer.lines.get(line).map(|l| l.text()) else {
            break;
        };
        let from = if line == start.line { start.index } else { 0 };
        let to = if line == end.line {
            end.index.min(text.len())
        } else {
            text.len()
        };
        if from <= to && text.is_char_boundary(from) && text.is_char_boundary(to) {
            out.push_str(&text[from..to]);
        }
    }
    out
}
