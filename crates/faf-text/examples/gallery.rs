//! Documentation gallery generator: renders every image the crate docs, the
//! README and the gh-pages landing page point at into `site/gallery/`.
//!
//! Everything here runs headless through the same offscreen pattern
//! `examples/offscreen.rs` uses — a plain `Rgba8Unorm` texture, a render pass,
//! and a 256-byte-aligned readback — so a box with any GPU can regenerate the
//! whole set with `cargo run --example gallery -p faf-text`.
//!
//! Output:
//!
//! - `hero.png` — a 1200×630 social card: the full feature set in one frame.
//! - `zoom.apng` / `zoom.png` — a font-size sweep, 10 → 80 px. Nothing
//!   re-rasterizes between those frames; the curve data is identical.
//! - `weight.apng` / `weight.png` — a `wght` sweep across Manrope's two
//!   masters, blended in the fragment shader.
//! - `tilt.apng` / `tilt.png` — a text pane turning ±30° under a perspective
//!   camera, which costs one matrix per frame.
//! - `terminal.apng` / `terminal.png` — a colored log streaming through a
//!   [`TermGrid`](faf_text::TermGrid).
//!
//! The animations are APNG (the `png` crate writes them natively), which every
//! browser, GitHub, docs.rs and crates.io animate from a plain `<img>`. Each
//! one also gets a single-frame PNG sibling as a universal fallback.

use std::f32::consts::PI;
use std::path::{Path, PathBuf};

use faf_text::glam::{Mat4, Vec3};
use faf_text::math::screen_perspective;
use faf_text::{
    Attrs, BlockContent, Cell, CellStyle, Color, Cursor, DecorationKind, Family, GridFont,
    GridScene, Metrics, RectLayer, TermGrid, TextRenderer, TextView, UnderlineStyle,
};

// Tokyo-night-ish, the same palette the web demo and `examples/term` use.
const BG: Color = Color::rgba(0.082, 0.086, 0.118, 1.0);
const FG: Color = Color::rgba(0.75, 0.79, 0.96, 1.0);
const DIM: Color = Color::rgba(0.35, 0.38, 0.5, 1.0);
const BLUE: Color = Color::rgba(0.48, 0.64, 0.97, 1.0);
const CYAN: Color = Color::rgba(0.45, 0.8, 0.85, 1.0);
const GREEN: Color = Color::rgba(0.62, 0.81, 0.42, 1.0);
const YELLOW: Color = Color::rgba(0.88, 0.69, 0.4, 1.0);
const RED: Color = Color::rgba(0.97, 0.46, 0.55, 1.0);
const PURPLE: Color = Color::rgba(0.73, 0.6, 0.97, 1.0);
const ORANGE: Color = Color::rgba(1.0, 0.62, 0.39, 1.0);
const SELECTION: Color = Color::rgba(0.23, 0.39, 0.66, 1.0);
const CHIP: Color = Color::rgba(0.18, 0.2, 0.3, 1.0);

/// Animation timing. 24 fps reads as smooth and keeps the frame counts (and so
/// the file sizes) modest.
const FPS: u16 = 24;

/// The variable family whose `wght` masters the GPU blends.
const MANROPE: Family<'static> = Family::Name("Manrope");

fn main() {
    pollster::block_on(run());
}

async fn run() {
    let out = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "site/gallery".to_string()),
    );
    std::fs::create_dir_all(&out).expect("create output directory");

    let gpu = Gpu::new().await;
    println!("adapter: {}", gpu.adapter);

    hero(&gpu, &out);
    zoom(&gpu, &out);
    weight(&gpu, &out);
    tilt(&gpu, &out);
    terminal(&gpu, &out);

    println!("\ngallery written to {}", out.display());
    let mut entries: Vec<_> = std::fs::read_dir(&out)
        .expect("read output directory")
        .filter_map(Result::ok)
        .collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
        println!("  {:<14} {:>8.1} KiB", name_of(&entry), len as f64 / 1024.0);
    }
}

fn name_of(entry: &std::fs::DirEntry) -> String {
    entry.file_name().to_string_lossy().into_owned()
}

// ---- Headless plumbing ----

/// One device, shared by every scene.
struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    adapter: String,
}

impl Gpu {
    async fn new() -> Self {
        let instance = wgpu::Instance::default();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                ..Default::default()
            })
            .await
            .expect("no GPU adapter available");
        let info = adapter.get_info();
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default())
            .await
            .expect("failed to acquire device");
        Self {
            device,
            queue,
            adapter: format!("{} ({:?})", info.name, info.backend),
        }
    }

    /// A font system over the embedded blobs plus whatever the box has, so the
    /// emoji in the hero find a color face.
    fn fonts(&self) -> faf_text::FontSystem {
        let mut font_system = faf_text::font_system_from_fonts(&[
            faf_text::FONT_DEJAVU_SANS,
            faf_text::FONT_DEJAVU_SANS_MONO,
            faf_text::FONT_MANROPE_VARIABLE,
        ]);
        font_system.db_mut().load_system_fonts();
        font_system
    }
}

/// A render target plus its readback buffer, reused across an animation's
/// frames.
struct Target {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    readback: wgpu::Buffer,
    width: u32,
    height: u32,
    padded_row: u32,
}

impl Target {
    const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

    fn new(gpu: &Gpu, width: u32, height: u32) -> Self {
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("gallery target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: Self::FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let padded_row = (width * 4).next_multiple_of(256);
        let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("gallery readback"),
            size: (padded_row * height) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        Self {
            texture,
            view,
            readback,
            width,
            height,
            padded_row,
        }
    }

    fn size(&self) -> [f32; 2] {
        [self.width as f32, self.height as f32]
    }

    /// Draw whatever the renderer has and read the result back as tightly
    /// packed RGB — the gallery images are opaque, and dropping the alpha
    /// channel is a free 25% off every frame.
    fn draw(&self, gpu: &Gpu, renderer: &TextRenderer) -> Vec<u8> {
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("gallery pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: BG.0[0] as f64,
                            g: BG.0[1] as f64,
                            b: BG.0[2] as f64,
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
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &self.readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(self.padded_row),
                    rows_per_image: None,
                },
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
        gpu.queue.submit([encoder.finish()]);

        let slice = self.readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |r| r.expect("map failed"));
        gpu.device
            .poll(wgpu::PollType::wait_indefinitely())
            .unwrap();
        let rgb = {
            let data = slice.get_mapped_range().unwrap();
            let mut rgb = Vec::with_capacity((self.width * self.height * 3) as usize);
            for row in 0..self.height {
                let start = (row * self.padded_row) as usize;
                let line = &data[start..start + (self.width * 4) as usize];
                for px in line.chunks_exact(4) {
                    rgb.extend_from_slice(&px[..3]);
                }
            }
            rgb
        };
        self.readback.unmap();
        rgb
    }
}

// ---- PNG / APNG output ----

fn encoder<'a>(
    path: &Path,
    width: u32,
    height: u32,
) -> png::Encoder<'a, std::io::BufWriter<std::fs::File>> {
    let file = std::fs::File::create(path).unwrap_or_else(|e| panic!("create {path:?}: {e}"));
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::High);
    enc
}

fn write_png(path: &Path, width: u32, height: u32, rgb: &[u8]) {
    encoder(path, width, height)
        .write_header()
        .unwrap()
        .write_image_data(rgb)
        .unwrap();
}

/// Write an APNG. The first frame goes into the `IDAT` (so a decoder that
/// knows nothing about `acTL` still shows a sensible still), the rest into
/// `fdAT` chunks, all full-size with a uniform delay.
fn write_apng(path: &Path, width: u32, height: u32, frames: &[Vec<u8>]) {
    let mut enc = encoder(path, width, height);
    enc.set_animated(frames.len() as u32, 0).unwrap();
    enc.set_frame_delay(1, FPS).unwrap();
    let mut writer = enc.write_header().unwrap();
    for frame in frames {
        writer.write_image_data(frame).unwrap();
    }
    writer.finish().unwrap();
}

/// Write `name.apng` plus the `name.png` still that stands in for it wherever
/// an APNG will not animate.
fn write_animation(
    out: &Path,
    name: &str,
    width: u32,
    height: u32,
    frames: &[Vec<u8>],
    still: usize,
) {
    write_apng(&out.join(format!("{name}.apng")), width, height, frames);
    write_png(
        &out.join(format!("{name}.png")),
        width,
        height,
        &frames[still],
    );
    println!("  {name}: {} frames, {width}×{height}", frames.len());
}

// ---- Shared scene helpers ----

/// Width of a view's longest laid-out line, for centering.
fn text_width(view: &TextView) -> f32 {
    view.buffer
        .layout_runs()
        .map(|run| run.line_w)
        .fold(0.0, f32::max)
}

/// A view with one line of text in `family` at `size`.
fn line(
    font_system: &mut faf_text::FontSystem,
    text: &str,
    family: Family<'_>,
    size: f32,
) -> TextView {
    let mut view = TextView::new(font_system, Metrics::new(size, size * 1.35));
    view.set_text(font_system, text, &Attrs::new().family(family));
    view
}

/// Extract every glyph a scene will ever draw, in one frame that is thrown
/// away.
///
/// The curve store flushes its texture only when the mirror grows into a new
/// row (`CurveStore::flush` early-outs on `total_rows <= uploaded_rows`), so a
/// glyph first extracted *after* a flush — a digit that only appears halfway
/// through a sweep, say — can end up in an already-uploaded partial row and
/// never reach the GPU, which draws it as nothing. Priming the whole character
/// set before the first flush sidesteps it: every glyph these scenes use is
/// resident by the time frame 0 is captured.
fn warm_up(
    gpu: &Gpu,
    renderer: &mut TextRenderer,
    font_system: &mut faf_text::FontSystem,
    screen: [f32; 2],
    strings: &[(&str, Family<'_>, f32)],
) {
    renderer.begin();
    for (text, family, size) in strings {
        let view = line(font_system, text, *family, *size);
        renderer.text(&gpu.queue, font_system, &view.buffer, [0.0, 0.0], FG);
    }
    renderer.finish(&gpu.device, &gpu.queue, screen);
}

/// A one-line readout parked against the right edge, whatever it says.
fn readout(font_system: &mut faf_text::FontSystem, text: &str, right: f32, top: f32) -> TextView {
    let mut view = line(font_system, text, Family::Monospace, 14.0);
    view.pos = [right - text_width(&view), top];
    view
}

// ---- hero.png ----

/// The social card: 1200×630, everything the renderer does in one frame.
fn hero(gpu: &Gpu, out: &Path) {
    const WIDTH: u32 = 1200;
    const HEIGHT: u32 = 630;

    let mut font_system = gpu.fonts();
    let mut renderer = TextRenderer::new(&gpu.device, Target::FORMAT);
    let target = Target::new(gpu, WIDTH, HEIGHT);

    let sans = Attrs::new().family(Family::SansSerif);
    let code = Attrs::new().family(Family::Monospace);

    // The wordmark, in the variable face so the title itself is a GPU weight
    // blend rather than a second font file.
    let mut title = line(&mut font_system, "faf-text", MANROPE, 112.0);
    title.pos = [70.0, 44.0];

    let mut tagline = line(
        &mut font_system,
        "a GPU text renderer that is fast as f***",
        Family::SansSerif,
        27.0,
    );
    tagline.pos = [76.0, 186.0];

    // The feature paragraph, decorating itself out of its own attributes.
    let mut body = TextView::new(&mut font_system, Metrics::new(21.0, 31.0));
    body.pos = [76.0, 248.0];
    body.set_rich_text(
        &mut font_system,
        [
            ("Glyph outlines live on the GPU as quadratic Béziers and every pixel solves ", sans.clone()),
            ("inside/outside", sans.clone().underline(UnderlineStyle::Single)),
            (" with the non-zero winding rule. Antialiasing is analytic, positioning is exactly subpixel, and zooming ", sans.clone()),
            ("re-rasterizes nothing", sans.clone().strikethrough()),
            (" — becuase there is nothing to rasterize. Emoji ride a bitmap atlas 🚀, selection is a rect layer, and ", sans.clone()),
            ("TermGrid", code.clone()),
            (" skips the shaper entirely.", sans.clone()),
        ],
        &sans,
    );
    body.set_size(&mut font_system, Some(600.0), None);

    // Label + row for the weight gradient.
    let mut weight_label = line(
        &mut font_system,
        "one shaped buffer · five GPU weight blends",
        Family::SansSerif,
        15.0,
    );
    weight_label.pos = [76.0, 482.0];
    let mut weight_word = line(&mut font_system, "weight", MANROPE, 40.0);
    weight_word.pos = [76.0, 506.0];
    // The heaviest master is the widest one, so step by that and the row never
    // collides with itself.
    let weight_step = text_width(&weight_word) * 1.14;

    let mut footer = line(
        &mut font_system,
        "native · WebGPU · WebGL2 · wasm — one code path",
        Family::SansSerif,
        16.0,
    );
    footer.pos = [76.0, 576.0];

    // Right column: resolution independence, and the shader line that decides
    // every pixel of it.
    let mut huge = line(&mut font_system, "Qg", Family::SansSerif, 300.0);
    huge.pos = [792.0, 96.0];

    let mut mono = TextView::new(&mut font_system, Metrics::new(15.0, 23.0));
    mono.pos = [772.0, 452.0];
    mono.set_text(
        &mut font_system,
        "// one curve set, every size\nlet cov = clamp(winding, 0., 1.);",
        &code,
    );

    renderer.begin();

    // Selection under a phrase in the body, and a highlight over another —
    // the two rect layers, below and above the glyphs.
    for (a, b) in body.find_all("non-zero winding rule") {
        for r in body.selection_rects(a, b) {
            renderer.rect([r[0], r[1]], [r[2], r[3]], SELECTION, RectLayer::Under);
        }
    }
    for (a, b) in body.find_all("analytic") {
        for r in body.selection_rects(a, b) {
            renderer.rect(
                [r[0], r[1]],
                [r[2], r[3]],
                Color::rgba(0.88, 0.69, 0.41, 0.38),
                RectLayer::Over,
            );
        }
    }
    // A diagnostics squiggle under the typo, and a chip behind the inline
    // code — the decoration pipeline's two other shapes.
    for (a, b) in body.find_all("becuase") {
        for r in body.decoration_rects(a, b, DecorationKind::Squiggle) {
            renderer.decoration(r, DecorationKind::Squiggle, RED);
        }
    }
    for (a, b) in body.find_all("TermGrid") {
        for r in body.decoration_rects(a, b, DecorationKind::Chip { radius_px: 0.0 }) {
            renderer.chip([r[0] - 4.0, r[1] + 4.0, r[2] + 8.0, r[3] - 7.0], 6.0, CHIP);
        }
    }
    // A caret parked in the code, the way a host would draw one.
    if let Some(r) = mono.cursor_rect(Cursor::new(1, 12)) {
        renderer.rect([r[0], r[1]], [2.0, r[3]], ORANGE, RectLayer::Over);
    }

    renderer.text_with_weight(
        &gpu.queue,
        &mut font_system,
        &title.buffer,
        title.pos,
        BLUE,
        Some(0.86),
    );
    renderer.text(
        &gpu.queue,
        &mut font_system,
        &tagline.buffer,
        tagline.pos,
        PURPLE,
    );
    renderer.text(&gpu.queue, &mut font_system, &body.buffer, body.pos, FG);
    renderer.text(
        &gpu.queue,
        &mut font_system,
        &weight_label.buffer,
        weight_label.pos,
        DIM,
    );
    for step in 0..5 {
        renderer.text_with_weight(
            &gpu.queue,
            &mut font_system,
            &weight_word.buffer,
            [
                weight_word.pos[0] + step as f32 * weight_step,
                weight_word.pos[1],
            ],
            ORANGE,
            Some(step as f32 / 4.0),
        );
    }
    renderer.text(
        &gpu.queue,
        &mut font_system,
        &footer.buffer,
        footer.pos,
        DIM,
    );
    renderer.text(&gpu.queue, &mut font_system, &huge.buffer, huge.pos, PURPLE);
    renderer.text(&gpu.queue, &mut font_system, &mono.buffer, mono.pos, GREEN);

    renderer.finish(&gpu.device, &gpu.queue, target.size());
    let rgb = target.draw(gpu, &renderer);
    write_png(&out.join("hero.png"), WIDTH, HEIGHT, &rgb);
    println!("  hero: {WIDTH}×{HEIGHT}");
}

// ---- zoom.apng ----

/// Font size sweeping 10 → 80 px and back. The point of the animation is that
/// the curve texture is byte-identical in every frame: only the quad and the
/// per-pixel evaluation change.
fn zoom(gpu: &Gpu, out: &Path) {
    const WIDTH: u32 = 600;
    const HEIGHT: u32 = 220;
    const FRAMES: usize = 40;

    let mut font_system = gpu.fonts();
    let mut renderer = TextRenderer::new(&gpu.device, Target::FORMAT);
    let target = Target::new(gpu, WIDTH, HEIGHT);

    let mut label = line(
        &mut font_system,
        "no re-rasterization",
        Family::SansSerif,
        14.0,
    );
    label.pos = [16.0, 14.0];

    warm_up(
        gpu,
        &mut renderer,
        &mut font_system,
        target.size(),
        &[
            ("0123456789 px", Family::Monospace, 14.0),
            ("no re-rasterization", Family::SansSerif, 14.0),
            ("Bézier", Family::SansSerif, 40.0),
        ],
    );

    let mut frames = Vec::with_capacity(FRAMES);
    for i in 0..FRAMES {
        // A full cosine period: 10 → 80 → 10 with no seam when it loops.
        let phase = 2.0 * PI * i as f32 / FRAMES as f32;
        let size = 45.0 - 35.0 * phase.cos();

        let mut word = line(&mut font_system, "Bézier", Family::SansSerif, size);
        word.pos = [
            (WIDTH as f32 - text_width(&word)) * 0.5,
            (HEIGHT as f32 - size * 1.35) * 0.5 + 12.0,
        ];
        let px = readout(
            &mut font_system,
            &format!("{size:.0} px"),
            WIDTH as f32 - 16.0,
            14.0,
        );

        renderer.begin();
        renderer.text(&gpu.queue, &mut font_system, &label.buffer, label.pos, DIM);
        renderer.text(&gpu.queue, &mut font_system, &px.buffer, px.pos, GREEN);
        renderer.text(&gpu.queue, &mut font_system, &word.buffer, word.pos, BLUE);
        renderer.finish(&gpu.device, &gpu.queue, target.size());
        frames.push(target.draw(gpu, &renderer));
    }

    // The still is the top of the sweep, where the letterforms read best.
    write_animation(out, "zoom", WIDTH, HEIGHT, &frames, FRAMES / 2);
}

// ---- weight.apng ----

/// Manrope's `wght` axis, swept in the fragment shader. Nothing re-shapes: the
/// same buffer is drawn every frame with a different blend constant.
fn weight(gpu: &Gpu, out: &Path) {
    const WIDTH: u32 = 600;
    const HEIGHT: u32 = 220;
    const FRAMES: usize = 40;

    let mut font_system = gpu.fonts();
    let mut renderer = TextRenderer::new(&gpu.device, Target::FORMAT);
    let target = Target::new(gpu, WIDTH, HEIGHT);

    let mut label = line(
        &mut font_system,
        "variable weight, blended on the GPU",
        Family::SansSerif,
        14.0,
    );
    label.pos = [16.0, 14.0];

    let mut word = line(&mut font_system, "Manrope", MANROPE, 72.0);
    word.pos = [
        (WIDTH as f32 - text_width(&word)) * 0.5,
        (HEIGHT as f32 - 72.0 * 1.35) * 0.5 + 10.0,
    ];

    warm_up(
        gpu,
        &mut renderer,
        &mut font_system,
        target.size(),
        &[
            ("wght 0123456789", Family::Monospace, 14.0),
            (
                "variable weight, blended on the GPU",
                Family::SansSerif,
                14.0,
            ),
            ("Manrope", MANROPE, 72.0),
        ],
    );

    let mut frames = Vec::with_capacity(FRAMES);
    for i in 0..FRAMES {
        let phase = 2.0 * PI * i as f32 / FRAMES as f32;
        let t = 0.5 - 0.5 * phase.cos();

        let wght = readout(
            &mut font_system,
            &format!("wght {:.0}", 200.0 + t * 600.0),
            WIDTH as f32 - 16.0,
            14.0,
        );

        // The axis as a bar, so the sweep reads as a value and not a wobble.
        let bar = [16.0, HEIGHT as f32 - 34.0, WIDTH as f32 - 32.0, 6.0];
        renderer.begin();
        renderer.rect([bar[0], bar[1]], [bar[2], bar[3]], CHIP, RectLayer::Under);
        renderer.rect(
            [bar[0], bar[1]],
            [bar[2] * t.max(0.004), bar[3]],
            ORANGE,
            RectLayer::Under,
        );
        renderer.text(&gpu.queue, &mut font_system, &label.buffer, label.pos, DIM);
        renderer.text(&gpu.queue, &mut font_system, &wght.buffer, wght.pos, GREEN);
        renderer.text_with_weight(
            &gpu.queue,
            &mut font_system,
            &word.buffer,
            word.pos,
            YELLOW,
            Some(t),
        );
        renderer.finish(&gpu.device, &gpu.queue, target.size());
        frames.push(target.draw(gpu, &renderer));
    }

    // The still is the top of the sweep: the boldest master, bar full.
    write_animation(out, "weight", WIDTH, HEIGHT, &frames, FRAMES / 2);
}

// ---- tilt.apng ----

/// A pane of text turning ±30° about its own vertical axis under a perspective
/// camera. One matrix per frame: no glyph is rebuilt and no curve re-extracted,
/// and the analytic coverage is measured on the *projected* glyph, so a
/// foreshortened stem is as smooth as a flat one.
fn tilt(gpu: &Gpu, out: &Path) {
    const WIDTH: u32 = 600;
    const HEIGHT: u32 = 300;
    const FRAMES: usize = 48;
    /// Wide enough that the foreshortening reads, narrow enough that the near
    /// edge stays off the near plane.
    const FOV_Y: f32 = 0.7;
    const PANE_W: f32 = 440.0;
    const LINE_H: f32 = 30.0;

    let mut font_system = gpu.fonts();
    let mut renderer = TextRenderer::new(&gpu.device, Target::FORMAT);
    let target = Target::new(gpu, WIDTH, HEIGHT);

    let vp = screen_perspective(target.size(), FOV_Y);
    renderer.set_view_projection(vp.to_cols_array_2d());

    let mut pane = TextView::new(&mut font_system, Metrics::new(21.0, LINE_H));
    pane.set_size(&mut font_system, Some(PANE_W), None);
    pane.set_text(
        &mut font_system,
        "Panes go anywhere in 3D. A block carries a 4×4 placement and the scene \
         a shared camera, so turning this one costs a single uniform write — no \
         glyph is rebuilt and no curve re-extracted.",
        &Attrs::new().family(Family::SansSerif),
    );
    // A selection, so the rect layer rides the pane through the projection the
    // way a ray-hit-tested drag would.
    let selection: Vec<(faf_text::Rect, Color)> = pane
        .find_all("a single uniform write")
        .into_iter()
        .flat_map(|(a, b)| pane.selection_rects(a, b))
        .map(|r| (r, SELECTION))
        .collect();
    let lines = pane.buffer.layout_runs().count() as f32;

    let block = renderer.create_block();
    renderer.set_block_content(
        &gpu.queue,
        &mut font_system,
        block,
        &BlockContent {
            buffer: Some(&pane.buffer),
            default_color: FG,
            under_rects: &selection,
            ..Default::default()
        },
    );

    // Turn about the pane's own middle so it opens toward the camera instead
    // of swinging out of frame.
    let origin = Vec3::new(
        (WIDTH as f32 - PANE_W) * 0.5,
        (HEIGHT as f32 - lines * LINE_H) * 0.5,
        0.0,
    );
    let pivot = Vec3::new(PANE_W * 0.5, 0.0, 0.0);

    let mut frames = Vec::with_capacity(FRAMES);
    for i in 0..FRAMES {
        let phase = 2.0 * PI * i as f32 / FRAMES as f32;
        let degrees = 30.0 * phase.sin();
        let model = Mat4::from_translation(origin + pivot)
            * Mat4::from_rotation_y(degrees.to_radians())
            * Mat4::from_translation(-pivot);
        renderer.begin_frame();
        renderer.set_block_transform(block, model.to_cols_array_2d());
        renderer.finish(&gpu.device, &gpu.queue, target.size());
        frames.push(target.draw(gpu, &renderer));
    }

    // A quarter into the loop is the widest turn — the still that shows what
    // the animation is about.
    write_animation(out, "tilt", WIDTH, HEIGHT, &frames, FRAMES / 4);
}

// ---- terminal.apng ----

/// A synthetic colored log streaming through a [`TermGrid`]: char → glyph id
/// through the charmap, whole-pixel cells, merged background runs, procedural
/// box drawing. No shaping anywhere on this path.
fn terminal(gpu: &Gpu, out: &Path) {
    const WIDTH: u32 = 600;
    const HEIGHT: u32 = 250;
    const FRAMES: usize = 40;
    const FONT_SIZE: f32 = 12.0;
    const MARGIN: f32 = 8.0;

    let mut font_system = gpu.fonts();
    let mut renderer = TextRenderer::new(&gpu.device, Target::FORMAT);
    let target = Target::new(gpu, WIDTH, HEIGHT);

    let mut font = GridFont::new(&mut font_system, Family::Monospace, FONT_SIZE);
    let [cell_w, cell_h] = font.cell_size();
    let cols = ((WIDTH as f32 - 2.0 * MARGIN) / cell_w).floor() as u16;
    let rows = ((HEIGHT as f32 - 2.0 * MARGIN) / cell_h).floor() as u16;
    let mut grid = TermGrid::new(cols, rows, 256);
    let mut scene = GridScene::new();
    let block = renderer.create_block();
    renderer.set_block_offset(block, [MARGIN, MARGIN]);

    // Prime the store with the stream's whole character set, for the reason
    // `warm_up` spells out — a level or a marker that only turns up on frame
    // 30 has to be resident before the first flush.
    let charset: Vec<char> = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789 \
         :;,.()[]{}<>-_+=%/|↯─┌┐├┤░▒▓█▁▂▃▄▅▆▇日本語"
        .chars()
        .collect();
    for (r, chunk) in charset.chunks(cols as usize - 1).enumerate() {
        grid.print(
            0,
            r as u16,
            &chunk.iter().collect::<String>(),
            FG,
            Some(BLUE),
        );
    }
    renderer.begin_frame();
    grid.render(
        &mut renderer,
        &gpu.queue,
        &mut font_system,
        &mut font,
        &mut scene,
        block,
        [0.0, 0.0],
    );
    renderer.finish(&gpu.device, &gpu.queue, target.size());
    grid.clear();

    // Fill the grid before the first captured frame so the animation opens on
    // a full screen rather than on empty rows scrolling in.
    let mut rng = Rng(0x1234_5678_9abc_def0);
    for f in 0..rows as usize {
        push_log_line(&mut grid, f, &mut rng);
    }

    let mut frames = Vec::with_capacity(FRAMES);
    for f in 0..FRAMES {
        push_log_line(&mut grid, rows as usize + f, &mut rng);
        header(&mut grid);
        renderer.begin_frame();
        grid.render(
            &mut renderer,
            &gpu.queue,
            &mut font_system,
            &mut font,
            &mut scene,
            block,
            [0.0, 0.0],
        );
        renderer.finish(&gpu.device, &gpu.queue, target.size());
        frames.push(target.draw(gpu, &renderer));
    }
    println!(
        "  terminal: {cols}×{rows} cells, {} instances/frame",
        scene.instances()
    );

    write_animation(out, "terminal", WIDTH, HEIGHT, &frames, FRAMES / 2);
}

/// A xorshift, so the log is the same log on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.next() as usize % items.len()]
    }
}

const LEVELS: [(&str, Color); 5] = [
    ("TRACE", DIM),
    ("DEBUG", BLUE),
    ("INFO ", GREEN),
    ("WARN ", YELLOW),
    ("ERROR", RED),
];

const TARGETS: [&str; 6] = [
    "curves::store",
    "renderer::block",
    "atlas::shelf",
    "grid::stream",
    "document::chunk",
    "wgpu::queue",
];

const MESSAGES: [&str; 6] = [
    "uploaded {n} instances in {n} writes",
    "glyph {n}: {n} quads, banded, bbox stable",
    "block {n} damaged; {n} cells retranslated",
    "shelf {n} full, evicting {n} cold glyphs",
    "chunk {n} shaped ({n} lines) and retained",
    "present skipped: idle for {n} frames",
];

/// One synthetic log line, printed as colored segments and scrolled in from
/// the bottom.
fn push_log_line(grid: &mut TermGrid, frame: usize, rng: &mut Rng) {
    grid.scroll_up(1);
    let row = grid.rows() - 1;
    let (level, level_color) = *rng.pick(&LEVELS);
    let target = *rng.pick(&TARGETS);
    let message = *rng.pick(&MESSAGES);

    let mut col = grid.print(
        0,
        row,
        &format!("{:>4}.{:03} ", frame / 60, (frame * 17) % 1000),
        DIM,
        None,
    );
    // The level is the one run with a background: a highlight is a merged
    // rect, however many cells it covers.
    col = grid.print(
        col,
        row,
        level,
        BG,
        Some(match level {
            "ERROR" => RED,
            "WARN " => YELLOW,
            _ => level_color,
        }),
    );
    col = grid.print(col, row, &format!(" {target:<16}"), CYAN, None);
    for part in message.split('{') {
        if let Some(rest) = part.strip_prefix("n}") {
            col = grid.print(col, row, &(rng.next() % 1000).to_string(), PURPLE, None);
            col = grid.print(col, row, rest, FG, None);
        } else {
            col = grid.print(col, row, part, FG, None);
        }
    }
    if level == "ERROR" {
        grid.print_styled(col, row, " ↯", RED, None, CellStyle::UNDERLINE);
    }
}

/// A framed header the stream scrolls under. It is redrawn every frame because
/// every line of log scrolls the whole grid up by one.
fn header(grid: &mut TermGrid) {
    let w = grid.cols();
    for col in 0..w {
        grid.set_cell(col, 0, Cell::new(' ', FG));
        grid.set_cell(col, 2, Cell::new('─', BLUE));
    }
    grid.print(1, 0, "┌ faf-text ", CYAN, None);
    grid.print(12, 0, "TermGrid", FG, None);
    grid.print(21, 0, "· no shaping, box drawing is procedural", DIM, None);
    grid.print(1, 2, "├", BLUE, None);
    grid.print(w - 1, 2, "┤", BLUE, None);
    grid.print(1, 1, "  ░▒▓█ ▁▂▃▄▅▆▇█ ╔═╦═╗ ┌─┬─┐ 日本語", GREEN, None);
}
