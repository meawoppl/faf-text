//! Fragment-shader stress test: a full 960×600 frame of 11 px text, rendered
//! offscreen N times with the GPU serialized per frame, reporting wall clock.
//!
//! At 11 px every glyph takes the three-tap path — six ray casts per fragment —
//! so this is the scene the per-glyph band tables exist for, and the size the
//! issue-11 quality options are aimed at: `--linear` and `--subpixel` run the
//! same scene through them (LCD coverage costs three x casts plus the three-tap
//! y pass, so six either way).

use std::time::Instant;

use faf_text::{
    Attrs, Color, CoverageBlend, Family, Metrics, RendererOptions, Subpixel, TextRenderer, TextView,
};

const WIDTH: u32 = 960;
const HEIGHT: u32 = 600;
const FRAMES: u32 = 200;
const WARMUP: u32 = 20;

const PARAGRAPH: &str = "Glyph outlines live on the GPU as quadratic Béziers and every pixel \
    solves inside/outside with the non-zero winding rule, so zooming re-rasterizes nothing and \
    subpixel positioning is exact: 0123456789 @&%$#§¶ ΓΔΘΞΣΦΨΩ кириллица ∮ E·da = Q ∞ ≠ ∅. \
    Quick wafting zephyrs vex bold Jim; big fjords vex quick waltz nymph. ";

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
    println!("adapter: {} ({:?})", info.name, info.backend);
    let args: Vec<String> = std::env::args().collect();
    let flag = |name: &str| args.iter().any(|a| a == name);
    let options = RendererOptions {
        blend: if flag("--linear") {
            CoverageBlend::Linear
        } else {
            CoverageBlend::Gamma
        },
        subpixel: if flag("--subpixel") {
            Subpixel::Rgb
        } else {
            Subpixel::Off
        },
    };
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            required_features: options.required_features() & adapter.features(),
            ..Default::default()
        })
        .await
        .expect("failed to acquire device");

    let format = wgpu::TextureFormat::Rgba8Unorm;
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("stress target"),
        size: wgpu::Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[options.target_format(format)],
    });
    // Linear mode renders through the sRGB view of the same texture.
    let target_view = target.create_view(&wgpu::TextureViewDescriptor {
        format: Some(options.target_format(format)),
        ..Default::default()
    });

    let mut font_system = faf_text::font_system_from_fonts(&[faf_text::FONT_DEJAVU_SANS]);
    let mut renderer = TextRenderer::with_options(&device, format, options);
    println!("options: {:?}", renderer.effective_options());

    // One wrapped block of 11 px text, repeated until it fills the frame.
    let mut body = TextView::new(&mut font_system, Metrics::new(11.0, 13.0));
    body.pos = [4.0, 2.0];
    // Enough to reach the bottom of the frame at 13 px line height.
    let text = PARAGRAPH.repeat(25);
    body.set_text(
        &mut font_system,
        &text,
        &Attrs::new().family(Family::SansSerif),
    );
    body.set_size(&mut font_system, Some(WIDTH as f32 - 8.0), None);

    let glyphs: usize = body
        .buffer
        .layout_runs()
        .take_while(|run| run.line_top < HEIGHT as f32)
        .map(|run| run.glyphs.len())
        .sum();
    println!("{glyphs} glyphs on screen at 11 px");

    let mut frame = |frames: u32| {
        for _ in 0..frames {
            renderer.begin();
            renderer.text(
                &queue,
                &mut font_system,
                &body.buffer,
                body.pos,
                Color::WHITE,
            );
            renderer.finish(&device, &queue, [WIDTH as f32, HEIGHT as f32]);

            let mut encoder =
                device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("stress pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &target_view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
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
            queue.submit([encoder.finish()]);
            // Serialize: without this the CPU runs ahead and times nothing.
            device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        }
    };

    frame(WARMUP);
    let start = Instant::now();
    frame(FRAMES);
    let elapsed = start.elapsed();

    let per_frame = elapsed.as_secs_f64() * 1000.0 / FRAMES as f64;
    println!(
        "{FRAMES} frames in {:.2?} — {per_frame:.3} ms/frame, {:.0} fps",
        elapsed,
        1000.0 / per_frame
    );
}
