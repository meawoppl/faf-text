//! Side-by-side of the three coverage/blend configurations from issue #11, at
//! the sizes where they matter (11–14 px): gamma-blended grayscale (the
//! default), linear-blended grayscale, and gamma-blended LCD subpixel.
//!
//! Writes `quality.png` (three panels, each with dark-on-light and
//! light-on-dark halves) and `quality-zoom.png` (an 8× nearest-neighbour crop
//! of the same stems, grayscale over subpixel, so the R/B fringing is visible
//! as pixels rather than as a hunch).
//!
//! The subpixel panel needs `wgpu::Features::DUAL_SOURCE_BLENDING`; without it
//! the example says so and renders that panel grayscale, which is exactly what
//! `TextRenderer::effective_options` reports.

use faf_text::{
    Attrs, Color, CoverageBlend, Family, Metrics, RectLayer, RendererOptions, Subpixel,
    TextRenderer, TextView,
};

const PANEL_W: u32 = 420;
const PANELS: u32 = 3;
const WIDTH: u32 = PANEL_W * PANELS;
const HEIGHT: u32 = 396;
const MARGIN: f32 = 16.0;
/// Where the light-on-dark half of a panel starts.
const DARK_TOP: f32 = 214.0;

const PARAGRAPH: &str = "Illuminating hairline stems at eleven pixels: the \
    fragment shader still solves inside/outside analytically, so nothing is \
    quantized to a bitmap and the only question is what the coverage is \
    handed to.";

/// Panel background (light half), and the two inks. Deliberately neutral
/// (R = G = B): any channel difference in the output is then the subpixel path
/// talking, which is what `report` counts.
const LIGHT_BG: Color = Color::rgba(0.94, 0.94, 0.94, 1.0);
const DARK_INK: Color = Color::rgba(0.09, 0.09, 0.09, 1.0);
const LIGHT_INK: Color = Color::rgba(0.90, 0.90, 0.90, 1.0);

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
    let info = adapter.get_info();
    let dual_source = adapter
        .features()
        .contains(wgpu::Features::DUAL_SOURCE_BLENDING);
    println!("adapter: {} ({:?})", info.name, info.backend);
    println!("DUAL_SOURCE_BLENDING: {dual_source}");

    // Ask for the feature only where it exists — requesting a missing one
    // fails device creation outright, while the renderer falls back quietly.
    let wanted = RendererOptions {
        blend: CoverageBlend::Gamma,
        subpixel: Subpixel::Rgb,
    };
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            required_features: wanted.required_features() & adapter.features(),
            ..Default::default()
        })
        .await
        .expect("failed to acquire device");

    // One texture, two views: the linear panel renders through the sRGB view of
    // the very same pixels, which is the arrangement a host has to make for
    // CoverageBlend::Linear.
    let format = wgpu::TextureFormat::Rgba8Unorm;
    let srgb = format.add_srgb_suffix();
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("quality target"),
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
        view_formats: &[srgb],
    });
    let gamma_view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let srgb_view = target.create_view(&wgpu::TextureViewDescriptor {
        format: Some(srgb),
        ..Default::default()
    });

    let mut font_system = faf_text::font_system_from_fonts(&[faf_text::FONT_DEJAVU_SANS]);

    let panels = [
        (
            "Gamma blend, grayscale (default)",
            RendererOptions::default(),
        ),
        (
            "Linear blend, grayscale",
            RendererOptions {
                blend: CoverageBlend::Linear,
                ..Default::default()
            },
        ),
        ("Gamma blend, LCD subpixel", wanted),
    ];

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    for (panel, (label, options)) in panels.iter().enumerate() {
        let mut renderer = TextRenderer::with_options(&device, format, *options);
        let got = renderer.effective_options();
        println!("panel {}: {label} -> {got:?}", panel + 1);

        let x = panel as f32 * PANEL_W as f32;
        renderer.begin();
        // The light half of the panel, and a hairline gutter between panels.
        renderer.rect(
            [x + 1.0, 30.0],
            [PANEL_W as f32 - 2.0, DARK_TOP - 30.0],
            LIGHT_BG,
            RectLayer::Under,
        );

        let mut heading = TextView::new(&mut font_system, Metrics::new(13.0, 18.0));
        heading.set_text(
            &mut font_system,
            label,
            &Attrs::new().family(Family::SansSerif),
        );
        renderer.text(
            &queue,
            &mut font_system,
            &heading.buffer,
            [x + MARGIN, 8.0],
            Color::rgba(0.6, 0.6, 0.6, 1.0),
        );

        // Same paragraph at 11 and 13 px, dark on light and light on dark:
        // linear blending and LCD coverage both behave differently by polarity.
        let mut y = 40.0;
        for (size, ink) in [
            (11.0, DARK_INK),
            (13.0, DARK_INK),
            (11.0, LIGHT_INK),
            (13.0, LIGHT_INK),
        ] {
            if ink == LIGHT_INK && y < DARK_TOP {
                y = DARK_TOP + 12.0;
            }
            let mut para = TextView::new(&mut font_system, Metrics::new(size, size * 1.45));
            para.set_size(&mut font_system, Some(PANEL_W as f32 - 2.0 * MARGIN), None);
            para.set_text(
                &mut font_system,
                PARAGRAPH,
                &Attrs::new().family(Family::SansSerif),
            );
            renderer.text(&queue, &mut font_system, &para.buffer, [x + MARGIN, y], ink);
            y += para
                .buffer
                .layout_runs()
                .map(|run| run.line_height)
                .sum::<f32>()
                + 14.0;
        }

        renderer.finish(&device, &queue, [WIDTH as f32, HEIGHT as f32]);

        let linear = got.blend == CoverageBlend::Linear;
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("quality pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: if linear { &srgb_view } else { &gamma_view },
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    // The first panel clears the whole page; the others load it.
                    load: if panel == 0 {
                        wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.06,
                            g: 0.06,
                            b: 0.06,
                            a: 1.0,
                        })
                    } else {
                        wgpu::LoadOp::Load
                    },
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
        label: Some("quality readback"),
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
    drop(data);
    write_png("quality.png", WIDTH, HEIGHT, &pixels);

    report(&pixels);
    write_png_from(
        "quality-zoom.png",
        &zoom(&pixels, MARGIN as u32 - 2, 40, 46, 26, 8),
    );
}

/// Numbers to go with the picture: how much ink each panel put down, and how
/// far apart the R and B stripes actually land.
fn report(pixels: &[u8]) {
    for panel in 0..PANELS {
        let x0 = panel * PANEL_W;
        let mut ink: u64 = 0;
        let mut fringed = 0u32;
        let mut worst = 0i32;
        for y in 30..HEIGHT {
            for x in x0 + 1..x0 + PANEL_W - 1 {
                let p = &pixels[((y * WIDTH + x) * 4) as usize..][..3];
                ink += p.iter().map(|&c| c as u64).sum::<u64>();
                let d = p[0] as i32 - p[2] as i32;
                if d != 0 {
                    fringed += 1;
                }
                worst = worst.max(d.abs());
            }
        }
        println!(
            "panel {}: ink {ink}, pixels with R != B: {fringed}, worst |R-B|: {worst}",
            panel + 1
        );
    }
}

/// Nearest-neighbour zoom of the same crop out of every panel, stacked with a
/// one-pixel rule between them.
fn zoom(pixels: &[u8], cx: u32, cy: u32, cw: u32, ch: u32, scale: u32) -> (u32, u32, Vec<u8>) {
    let (w, h) = (cw * scale, ch * scale * PANELS + (PANELS - 1));
    let mut out = vec![0u8; (w * h * 4) as usize];
    let mut row = 0;
    for panel in 0..PANELS {
        for y in 0..ch * scale {
            for x in 0..w {
                let sx = cx + panel * PANEL_W + x / scale;
                let sy = cy + y / scale;
                let src = ((sy * WIDTH + sx) * 4) as usize;
                let dst = ((row * w + x) * 4) as usize;
                out[dst..dst + 4].copy_from_slice(&pixels[src..src + 4]);
            }
            row += 1;
        }
        if panel + 1 < PANELS {
            for x in 0..w {
                let dst = ((row * w + x) * 4) as usize;
                out[dst..dst + 4].copy_from_slice(&[255, 80, 80, 255]);
            }
            row += 1;
        }
    }
    (w, h, out)
}

fn write_png_from(path: &str, (w, h, pixels): &(u32, u32, Vec<u8>)) {
    write_png(path, *w, *h, pixels);
}

fn write_png(path: &str, width: u32, height: u32, pixels: &[u8]) {
    let file = std::fs::File::create(path).unwrap();
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()
        .unwrap()
        .write_image_data(pixels)
        .unwrap();
    println!("wrote {path}");
}
