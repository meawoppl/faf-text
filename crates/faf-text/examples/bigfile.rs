//! Virtualized document smoke test: build a ~1M line log in memory, then
//! render three scroll positions to PNGs without ever shaping more than the
//! handful of chunks the viewport needs.
//!
//! Writes bigfile-top.png, bigfile-middle.png and bigfile-end.png, and prints
//! the shaping-window stats after each paint.

use std::time::Instant;

use faf_text::{
    Attrs, CHUNK_LINES, Color, Document, Family, Metrics, RETAIN_CHUNKS, RectLayer, TextRenderer,
    WINDOW_CHUNKS,
};

const WIDTH: u32 = 900;
const HEIGHT: u32 = 600;
const PAD: f32 = 16.0;
const LINES: usize = 1_000_000;

fn main() {
    pollster::block_on(run());
}

/// A synthetic log: ~1M lines, a few of them carrying a needle to search for.
fn generate(lines: usize) -> String {
    const LEVELS: [&str; 4] = ["INFO ", "WARN ", "ERROR", "DEBUG"];
    const WHAT: [&str; 6] = [
        "connection established with peer",
        "flushed 4096 records to the write-ahead log",
        "retrying after transient failure",
        "cache miss, falling back to origin",
        "checkpoint complete, 12ms",
        "NEEDLE found in the haystack",
    ];
    let mut out = String::with_capacity(lines * 72);
    for i in 0..lines {
        let level = LEVELS[i % LEVELS.len()];
        // The needle is rare, so most of the hits sit in unshaped regions.
        let what = if i % 99_991 == 7 {
            WHAT[5]
        } else {
            WHAT[i % (WHAT.len() - 1)]
        };
        out.push_str(&format!("{i:07} {level} [worker-{:02}] {what}\n", i % 32));
    }
    out
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
    println!("adapter: {}", adapter.get_info().name);
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor::default())
        .await
        .expect("failed to acquire device");

    let format = wgpu::TextureFormat::Rgba8Unorm;
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("bigfile target"),
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

    let mut font_system = faf_text::font_system_from_fonts(&[faf_text::FONT_DEJAVU_SANS_MONO]);
    let mut renderer = TextRenderer::new(&device, format);

    let t = Instant::now();
    let text = generate(LINES);
    println!(
        "generated {} MiB of log in {:?}",
        text.len() / (1024 * 1024),
        t.elapsed()
    );

    let mut doc = Document::new(Metrics::new(14.0, 18.0));
    doc.set_attrs(&Attrs::new().family(Family::Monospace));
    let t = Instant::now();
    doc.set_text(&text);
    println!(
        "indexed {} lines into {} chunks of {CHUNK_LINES} in {:?}",
        doc.line_count(),
        doc.chunk_count(),
        t.elapsed()
    );

    // Streaming append: the line index only scans what arrives.
    let t = Instant::now();
    doc.append("9999999 INFO  [worker-00] streamed in after the fact\n");
    println!("appended one line in {:?}", t.elapsed());

    let t = Instant::now();
    let hits = doc.find_all("NEEDLE");
    println!(
        "find_all over the whole {} MiB backing text: {} hits in {:?}",
        doc.text().len() / (1024 * 1024),
        hits.len(),
        t.elapsed()
    );

    let pane_w = WIDTH as f32 - 2.0 * PAD;
    let pane_h = HEIGHT as f32 - 2.0 * PAD;
    let shots = [
        ("bigfile-top.png", 0usize),
        ("bigfile-middle.png", LINES / 2),
        ("bigfile-end.png", LINES - 40),
    ];

    let mut worst = 0;
    for (path, line) in shots {
        let scroll = doc.line_top(line);
        let t = Instant::now();
        doc.set_viewport(&mut font_system, pane_w, scroll, pane_h);
        let paint = t.elapsed();
        let stats = doc.stats();
        worst = worst.max(stats.resident_chunks);

        println!(
            "\n{path}: line {line}, scroll {scroll:.0} of {:.0} px\n  \
             shaped now {} / total {} chunks ({} of {} lines, {:.4}%), \
             resident {}, evicted {}, viewport pass {:?}",
            doc.total_height(),
            stats.shaped_last,
            stats.shaped_total,
            stats.lines_shaped_total,
            doc.line_count(),
            100.0 * stats.lines_shaped_total as f64 / doc.line_count() as f64,
            stats.resident_chunks,
            stats.evicted_total,
            paint,
        );
        assert!(
            stats.shaped_last <= 5,
            "one paint shaped more than 5 chunks: {stats:?}"
        );
        assert!(
            stats.resident_chunks <= 2 * (WINDOW_CHUNKS + RETAIN_CHUNKS) + 1,
            "the shaping window grew past its budget: {stats:?}"
        );

        renderer.begin();

        // Highlight every needle that happens to be on screen; find_all
        // searched all 1M lines, selection_rects only draws visible ones.
        for (a, b) in &hits {
            for rect in doc.selection_rects(*a, *b) {
                renderer.rect(
                    [PAD + rect[0], PAD + rect[1] - scroll],
                    [rect[2], rect[3]],
                    Color::rgba8(0xe0, 0xaf, 0x68, 0x70),
                    RectLayer::Over,
                );
            }
        }

        // Selection underlay over the first three visible lines.
        if let Some(a) = doc.hit(0.0, scroll + 1.0) {
            let b = faf_text::DocCursor::new(a.line + 3, 0);
            for rect in doc.selection_rects(a, b) {
                renderer.rect(
                    [PAD + rect[0], PAD + rect[1] - scroll],
                    [rect[2], rect[3]],
                    Color::rgba8(0x3b, 0x63, 0xa8, 0xff),
                    RectLayer::Under,
                );
            }
        }

        let fg = Color::rgba8(0xc0, 0xca, 0xf5, 0xff);
        for view in doc.visible() {
            renderer.text(
                &queue,
                &mut font_system,
                &view.buffer,
                [PAD + view.pos[0], PAD + view.pos[1] - scroll],
                fg,
            );
        }
        renderer.finish(&device, &queue, [WIDTH as f32, HEIGHT as f32]);

        let pixels = draw(&device, &queue, &renderer, &target, &target_view);
        write_png(path, &pixels);
        println!("  wrote {path} ({} non-background pixels)", ink(&pixels));
    }

    println!("\npeak resident chunks: {worst}");
}

fn draw(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    renderer: &TextRenderer,
    target: &wgpu::Texture,
    target_view: &wgpu::TextureView,
) -> Vec<u8> {
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("bigfile pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.078,
                        g: 0.082,
                        b: 0.11,
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
        label: Some("bigfile readback"),
        size: (padded_row * HEIGHT) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: target,
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
    pixels
}

/// Pixels that differ from the clear color — a cheap "did anything render".
fn ink(pixels: &[u8]) -> usize {
    pixels
        .chunks_exact(4)
        .filter(|p| p[0] > 40 || p[1] > 40 || p[2] > 50)
        .count()
}

fn write_png(path: &str, pixels: &[u8]) {
    let file = std::fs::File::create(path).unwrap();
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), WIDTH, HEIGHT);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()
        .unwrap()
        .write_image_data(pixels)
        .unwrap();
}
