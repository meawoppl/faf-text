//! Headless smoke test: renders the full feature set (vector glyphs at many
//! sizes, selection underlay, highlight overlay, decorations, emoji atlas
//! fallback, COLR/CPAL color glyphs on the vector path) to offscreen.png
//! without a window.
//!
//! The scene above y = 600 is a regression baseline and has not moved since
//! the renderer's first commit; the COLR strip below it was added by #15.

use faf_text::{
    Attrs, Color, Cursor, DecorationKind, Family, Metrics, RectLayer, TextRenderer, TextView,
    UnderlineStyle,
};

const WIDTH: u32 = 960;
const HEIGHT: u32 = 880;
/// Top of the COLR comparison strip. Everything above it is the original
/// scene, pixel for pixel.
const COLR_STRIP_Y: f32 = 600.0;

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
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor::default())
        .await
        .expect("failed to acquire device");

    let format = wgpu::TextureFormat::Rgba8Unorm;
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("offscreen target"),
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

    // Embedded DejaVu plus the system database so emoji (atlas path) works
    // where a color font exists.
    let mut font_system = faf_text::font_system_from_fonts(&[
        faf_text::FONT_DEJAVU_SANS,
        faf_text::FONT_DEJAVU_SANS_MONO,
        faf_text::FONT_MANROPE_VARIABLE,
        // COLRv0 emoji, asked for by family name below. It never displaces the
        // system emoji font in fallback — cosmic-text's fallback list names
        // "Noto Color Emoji" — so the body text's emoji still go to the atlas.
        faf_text::FONT_TWEMOJI_COLR,
    ]);
    font_system.db_mut().load_system_fonts();

    let mut renderer = TextRenderer::new(&device, format);

    let mut title = TextView::new(&mut font_system, Metrics::new(64.0, 76.0));
    title.pos = [40.0, 24.0];
    title.set_text(
        &mut font_system,
        "faf-text",
        &Attrs::new().family(Family::SansSerif),
    );

    let mut body = TextView::new(&mut font_system, Metrics::new(22.0, 32.0));
    body.pos = [40.0, 130.0];
    body.set_text(
        &mut font_system,
        "Glyph outlines live on the GPU as quadratic Béziers; every pixel \
         solves inside/outside with the non-zero winding rule and analytic \
         antialiasing. Zooming re-rasterizes nothing. Selection is an \
         instanced rect layer under the glyphs, and highlights are an \
         alpha-blended layer over them. Emoji ride the bitmap atlas: 🚀🔥\n\
         Ελληνικά, кириллица, ∮ E·da = Q, ∞ ≠ ∅ — one draw call each.",
        &Attrs::new().family(Family::SansSerif),
    );
    body.set_size(&mut font_system, Some(560.0), None);

    let mut mono = TextView::new(&mut font_system, Metrics::new(15.0, 22.0));
    mono.pos = [40.0, 430.0];
    mono.set_text(
        &mut font_system,
        "let coverage = clamp(abs(winding), 0.0, 1.0); // exact, no MSAA\n\
         for size in [8, 11, 15, 22, 64, 300] { render(size); } // one curve set",
        &Attrs::new().family(Family::Monospace),
    );

    // Variable font: one shaped buffer drawn five times, each blending the
    // glyph's two wght masters differently in the fragment shader. No
    // re-extraction, no second curve set — just a lerp per control point.
    let mut weights = TextView::new(&mut font_system, Metrics::new(26.0, 34.0));
    weights.pos = [40.0, 500.0];
    weights.set_text(
        &mut font_system,
        "weight",
        &Attrs::new().family(Family::Name("Manrope")),
    );

    // Decorations. The underline and the strikeout come from the *attributes*
    // — cosmic-text shapes them into per-run spans and the renderer turns
    // those into instances, so styled text decorates itself. The squiggle and
    // the chip have no attribute to ask for them, so they come from
    // `decoration_rects` over a cursor range instead. All four are one
    // pipeline and two draws.
    let sans = Attrs::new().family(Family::SansSerif);
    let code = Attrs::new().family(Family::Monospace);
    let mut decorated = TextView::new(&mut font_system, Metrics::new(17.0, 25.0));
    decorated.pos = [620.0, 30.0];
    decorated.set_rich_text(
        &mut font_system,
        [
            ("Decorations are a third primitive: ", sans.clone()),
            (
                "an underlined span",
                sans.clone().underline(UnderlineStyle::Single),
            ),
            (", ", sans.clone()),
            ("one struck out", sans.clone().strikethrough()),
            (", a squiggle under a mispelled word, and ", sans.clone()),
            ("chip()", code.clone()),
            (" on a rounded chip.", sans.clone()),
        ],
        &sans,
    );
    decorated.set_size(&mut font_system, Some(320.0), None);

    // A giant glyph to show off resolution independence — same curve data the
    // 22px body text uses.
    let mut huge = TextView::new(&mut font_system, Metrics::new(300.0, 300.0));
    huge.pos = [640.0, 180.0];
    huge.set_text(
        &mut font_system,
        "Qg",
        &Attrs::new().family(Family::SansSerif),
    );

    // COLR/CPAL. The same rocket twice: on the left out of the bitmap atlas
    // (Noto Color Emoji is CBDT — bitmap strikes, rescaled per size), on the
    // right as COLRv0 layered outlines (Twemoji), which go through the winding
    // shader like any other glyph. Both at 22 px and at 200 px, and the 200 px
    // pair is the point: the vector one is the *same curve data* as the 22 px
    // one and stays exact, where the bitmap one is a 136 px strike blown up.
    let bitmap_family = Family::Name("Noto Color Emoji");
    let colr_family = Family::Name("Twemoji Mozilla");
    let mut caption = |x: f32, text: &str| {
        let mut view = TextView::new(&mut font_system, Metrics::new(15.0, 22.0));
        view.pos = [x, COLR_STRIP_Y + 8.0];
        view.set_text(
            &mut font_system,
            text,
            &Attrs::new().family(Family::Monospace),
        );
        view
    };
    let atlas_caption = caption(40.0, "bitmap atlas · CBDT strikes");
    let vector_caption = caption(500.0, "vector · COLR/CPAL layers");

    let mut emoji = |x: f32, y: f32, px: f32, text: &str, family: Family| {
        let mut view = TextView::new(&mut font_system, Metrics::new(px, px * 1.05));
        view.pos = [x, y];
        view.set_text(&mut font_system, text, &Attrs::new().family(family));
        view
    };
    let atlas_small = emoji(40.0, COLR_STRIP_Y + 32.0, 22.0, "🚀🔥❤", bitmap_family);
    let vector_small = emoji(500.0, COLR_STRIP_Y + 32.0, 22.0, "🚀🔥❤", colr_family);
    let atlas_big = emoji(40.0, COLR_STRIP_Y + 66.0, 200.0, "🚀", bitmap_family);
    let vector_big = emoji(500.0, COLR_STRIP_Y + 66.0, 200.0, "🚀", colr_family);

    renderer.begin();

    // Selection: from mid-word in the body text, spanning a couple of lines.
    let sel_a = Cursor::new(0, 30);
    let sel_b = Cursor::new(0, 170);
    for rect in body.selection_rects(sel_a, sel_b) {
        renderer.rect(
            [rect[0], rect[1]],
            [rect[2], rect[3]],
            Color::rgba8(0x3b, 0x63, 0xa8, 0xff),
            RectLayer::Under,
        );
    }

    // Highlight overlay: every occurrence of "the".
    for (a, b) in body.find_all("the") {
        for rect in body.selection_rects(a, b) {
            renderer.rect(
                [rect[0], rect[1]],
                [rect[2], rect[3]],
                Color::rgba8(0xe0, 0xaf, 0x68, 0x60),
                RectLayer::Over,
            );
        }
    }

    // Caret: a 2 px Over rect straight from cursor_rect, parked inside the
    // code block right after "clamp(".
    if let Some(rect) = mono.cursor_rect(Cursor::new(0, 21)) {
        renderer.rect(
            [rect[0], rect[1]],
            [2.0, rect[3]],
            Color::rgba8(0xff, 0x9e, 0x64, 0xff),
            RectLayer::Over,
        );
    }

    // Diagnostics squiggle under the typo, and an inline-code chip behind the
    // mono span. The chip is the line box, tightened a little so it hugs the
    // text the way an editor's inline code does.
    for (a, b) in decorated.find_all("mispelled") {
        for rect in decorated.decoration_rects(a, b, DecorationKind::Squiggle) {
            renderer.decoration(
                rect,
                DecorationKind::Squiggle,
                Color::rgba8(0xf7, 0x76, 0x8e, 0xff),
            );
        }
    }
    for (a, b) in decorated.find_all("chip()") {
        for r in decorated.decoration_rects(a, b, DecorationKind::Chip { radius_px: 0.0 }) {
            renderer.chip(
                [r[0] - 4.0, r[1] + 3.0, r[2] + 8.0, r[3] - 6.0],
                6.0,
                Color::rgba8(0x2f, 0x33, 0x4d, 0xff),
            );
        }
    }

    let fg = Color::rgba8(0xc0, 0xca, 0xf5, 0xff);
    let accent = Color::rgba8(0x7a, 0xa2, 0xf7, 0xff);
    let green = Color::rgba8(0x9e, 0xce, 0x6a, 0xff);
    renderer.text(&queue, &mut font_system, &title.buffer, title.pos, accent);
    renderer.text(&queue, &mut font_system, &body.buffer, body.pos, fg);
    renderer.text(&queue, &mut font_system, &mono.buffer, mono.pos, green);
    renderer.text(
        &queue,
        &mut font_system,
        &decorated.buffer,
        decorated.pos,
        fg,
    );
    renderer.text(
        &queue,
        &mut font_system,
        &huge.buffer,
        huge.pos,
        Color::rgba8(0xbb, 0x9a, 0xf7, 0xff),
    );
    let muted = Color::rgba8(0x6a, 0x74, 0x9a, 0xff);
    for view in [&atlas_caption, &vector_caption] {
        renderer.text(&queue, &mut font_system, &view.buffer, view.pos, muted);
    }
    for view in [&atlas_small, &vector_small, &atlas_big, &vector_big] {
        renderer.text(&queue, &mut font_system, &view.buffer, view.pos, fg);
    }
    for step in 0..5 {
        renderer.text_with_weight(
            &queue,
            &mut font_system,
            &weights.buffer,
            [weights.pos[0] + step as f32 * 112.0, weights.pos[1]],
            Color::rgba8(0xff, 0x9e, 0x64, 0xff),
            Some(step as f32 / 4.0),
        );
    }

    renderer.finish(&device, &queue, [WIDTH as f32, HEIGHT as f32]);

    let (used, allocated) = renderer.curve_memory();
    println!("curve data: {used} bytes packed, {allocated} bytes allocated");

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("offscreen pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &target_view,
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

    // Read back with 256-byte row alignment.
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

    let file = std::fs::File::create("offscreen.png").unwrap();
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), WIDTH, HEIGHT);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()
        .unwrap()
        .write_image_data(&pixels)
        .unwrap();
    println!("wrote offscreen.png");
}
