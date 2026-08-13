//! Replicates the benchmark from Lengyel 2017 (JCGT vol 6 no 2, Table 2):
//! fill a two-megapixel area with 50 lines of text at 32 pixels per em,
//! measure steady-state render time. Prepare runs once; the timed loop
//! re-records and submits the same draws, like a static scene.

use faf_text::{Attrs, Color, Family, Metrics, TextRenderer, TextView};
use std::time::Instant;

const SIZE: u32 = 1448; // 1448^2 ≈ 2.10 MP
const LINES: usize = 50;
const FRAMES: usize = 300;

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
        .unwrap();
    println!("adapter: {}", adapter.get_info().name);
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor::default())
        .await
        .unwrap();

    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("bench target"),
        size: wgpu::Extent3d {
            width: SIZE,
            height: SIZE,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = target.create_view(&Default::default());

    let mut font_system = faf_text::font_system_from_fonts(&[faf_text::FONT_DEJAVU_SANS]);
    let mut renderer = TextRenderer::new(&device, wgpu::TextureFormat::Rgba8Unorm);

    // 50 lines packed into the 2 MP square, 32 px/em like the paper.
    let line_height = SIZE as f32 / LINES as f32; // 28.96 px
    let mut text = String::new();
    let filler = "ABCDEFGHIJKLMNOPQRSTUVWXYZ abcdefghijklmnopqrstuvwxyz 0123456789 ";
    let chars_per_line = (SIZE as f32 / 17.0) as usize; // ~32px DejaVu advance ≈ 17px avg
    for i in 0..LINES {
        let mut line: String = filler
            .chars()
            .cycle()
            .skip(i * 7)
            .take(chars_per_line)
            .collect();
        line.push('\n');
        text.push_str(&line);
    }

    let mut tv = TextView::new(&mut font_system, Metrics::new(32.0, line_height));
    tv.set_size(&mut font_system, Some(SIZE as f32), None);
    tv.set_text(
        &mut font_system,
        &text,
        &Attrs::new().family(Family::SansSerif),
    );

    // Prepare once (uploads instances + curve data), then time pure rendering.
    renderer.begin();
    renderer.text(
        &queue,
        &mut font_system,
        &tv.buffer,
        [0.0, 0.0],
        Color::WHITE,
    );
    renderer.finish(&device, &queue, [SIZE as f32, SIZE as f32]);

    let glyphs: usize = tv.buffer.layout_runs().map(|r| r.glyphs.len()).sum();
    println!("glyphs on screen: {glyphs}");

    let render_once = |label: Option<&str>| {
        let mut encoder = device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
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
    };

    // Warmup (pipeline compile, first-submit overheads).
    for _ in 0..20 {
        render_once(Some("warmup"));
    }
    device.poll(wgpu::PollType::wait_indefinitely()).unwrap();

    let start = Instant::now();
    for _ in 0..FRAMES {
        render_once(None);
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    }
    let per_frame = start.elapsed().as_secs_f64() * 1000.0 / FRAMES as f64;
    println!("2.10 MP, 50 lines, 32 px/em: {per_frame:.3} ms/frame (submit+wait, {FRAMES} frames)");
}
