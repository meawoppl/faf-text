//! Terminal grid benchmark and showcase.
//!
//! Streams a synthetic colored log through a 200×60 [`TermGrid`] for 500
//! frames — append, scroll, translate, upload, draw — and reports the CPU cost
//! of the grid → instance translation, which is the only per-frame work a
//! terminal front end does here. Then it draws a box-drawing showcase frame to
//! `term.png`: procedural rects snapped to cell bounds, so the joins are
//! seamless at any size.

use std::time::Instant;

use faf_text::{
    BlockContent, Cell, CellStyle, Color, Family, GridFont, GridScene, TermGrid, TextRenderer,
};

const COLS: u16 = 200;
const ROWS: u16 = 60;
const SCROLLBACK: usize = 2000;
const FRAMES: usize = 500;
const FONT_SIZE: f32 = 13.0;
const MARGIN: f32 = 8.0;

// Tokyo-night-ish palette.
const BG: Color = Color([0.06, 0.06, 0.09, 1.0]);
const FG: Color = Color([0.75, 0.79, 0.96, 1.0]);
const DIM: Color = Color([0.35, 0.38, 0.5, 1.0]);
const BLUE: Color = Color([0.48, 0.64, 0.97, 1.0]);
const CYAN: Color = Color([0.45, 0.8, 0.85, 1.0]);
const GREEN: Color = Color([0.62, 0.81, 0.42, 1.0]);
const YELLOW: Color = Color([0.88, 0.69, 0.4, 1.0]);
const RED: Color = Color([0.97, 0.46, 0.55, 1.0]);
const MAGENTA: Color = Color([0.73, 0.6, 0.97, 1.0]);

fn main() {
    pollster::block_on(run());
}

/// A tiny xorshift, so the log is the same log on every run.
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
    "uploaded {n} instances in {n} writes, arena at {n}%",
    "glyph {n} extracted: {n} quads, banded, bbox stable",
    "block {n} damaged by scroll; {n} cells retranslated",
    "shelf {n} full, evicting {n} glyphs not drawn since frame {n}",
    "chunk {n} shaped ({n} lines, {n} runs) and retained",
    "present skipped: nothing damaged for {n} frames",
];

/// One synthetic log line, printed as colored segments.
fn push_log_line(grid: &mut TermGrid, frame: usize, rng: &mut Rng) {
    grid.scroll_up(1);
    let row = grid.rows() - 1;
    let (level, level_color) = *rng.pick(&LEVELS);
    let target = *rng.pick(&TARGETS);
    let message = *rng.pick(&MESSAGES);

    let mut col = grid.print(
        0,
        row,
        &format!("{:>8}.{:03}  ", frame / 60, (frame * 17) % 1000),
        DIM,
        None,
    );
    // The level is the one cell run with a background: a highlight is a
    // merged rect, however many cells it covers.
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
    col = grid.print(col, row, &format!(" {target:<16} "), CYAN, None);

    // Numbers in the message body get their own color, the way a structured
    // logger highlights values.
    for part in message.split('{') {
        if let Some(rest) = part.strip_prefix("n}") {
            col = grid.print(col, row, &(rng.next() % 1000).to_string(), MAGENTA, None);
            col = grid.print(col, row, rest, FG, None);
        } else {
            col = grid.print(col, row, part, FG, None);
        }
    }
    if level == "ERROR" {
        grid.print_styled(col, row, "  see backtrace", RED, None, CellStyle::UNDERLINE);
    }
}

/// The showcase frame: a double-line frame around single-line panels, the full
/// shade and eighth-block ramps, wide CJK cells and an emoji or two.
fn draw_showcase(grid: &mut TermGrid) {
    grid.clear();
    let (w, h) = (grid.cols(), grid.rows());

    frame_box(grid, 0, 0, w, h, true, BLUE);
    grid.print(3, 0, "╡ faf-text terminal grid ╞", CYAN, None);

    // A single-line panel, joined into the double frame's left and right
    // edges with the mixed single/double tees.
    let panel_top = 3;
    let panel_bottom = 12;
    for row in panel_top..=panel_bottom {
        grid.set_cell(0, row, Cell::new('║', BLUE));
        grid.set_cell(w - 1, row, Cell::new('║', BLUE));
    }
    for col in 1..w - 1 {
        grid.set_cell(col, panel_top, Cell::new('─', DIM));
        grid.set_cell(col, panel_bottom, Cell::new('─', DIM));
    }
    grid.set_cell(0, panel_top, Cell::new('╟', BLUE));
    grid.set_cell(w - 1, panel_top, Cell::new('╢', BLUE));
    grid.set_cell(0, panel_bottom, Cell::new('╟', BLUE));
    grid.set_cell(w - 1, panel_bottom, Cell::new('╢', BLUE));
    // A crossing column, tee'd into both panel edges.
    let split = 60;
    for row in panel_top + 1..panel_bottom {
        grid.set_cell(split, row, Cell::new('│', DIM));
    }
    grid.set_cell(split, panel_top, Cell::new('┬', DIM));
    grid.set_cell(split, panel_bottom, Cell::new('┴', DIM));

    grid.print(3, panel_top + 1, "box drawing is procedural", FG, None);
    grid.print(
        3,
        panel_top + 2,
        "  ┌───┬───┐   ┏━━━┳━━━┓   ╔═══╦═══╗",
        DIM,
        None,
    );
    grid.print(
        3,
        panel_top + 3,
        "  │   │   │   ┃   ┃   ┃   ║   ║   ║",
        DIM,
        None,
    );
    grid.print(
        3,
        panel_top + 4,
        "  ├───┼───┤   ┣━━━╋━━━┫   ╠═══╬═══╣",
        DIM,
        None,
    );
    grid.print(
        3,
        panel_top + 5,
        "  │   │   │   ┃   ┃   ┃   ║   ║   ║",
        DIM,
        None,
    );
    grid.print(
        3,
        panel_top + 6,
        "  └───┴───┘   ┗━━━┻━━━┛   ╚═══╩═══╝",
        DIM,
        None,
    );
    grid.print(
        3,
        panel_top + 7,
        "  mixed ╒═╤═╕ ╓─╥─╖ ┍━┯━┑ ╞╪╡ ╟╫╢",
        CYAN,
        None,
    );
    grid.print(
        3,
        panel_top + 8,
        "  arcs ╭─╮╰─╯  diagonals ╱╲╳  stubs ╴╵╶╷╸╹╺╻",
        CYAN,
        None,
    );

    grid.print(
        split + 3,
        panel_top + 1,
        "block elements and shades",
        FG,
        None,
    );
    grid.print(split + 3, panel_top + 2, "░░░▒▒▒▓▓▓███", MAGENTA, None);
    grid.print(split + 3, panel_top + 3, "▁▂▃▄▅▆▇█ eighths", GREEN, None);
    grid.print(split + 3, panel_top + 4, "▏▎▍▌▋▊▉█ columns", CYAN, None);
    grid.print(
        split + 3,
        panel_top + 5,
        "▖▗▘▙▚▛▜▝▞▟ quadrants",
        YELLOW,
        None,
    );
    grid.print(split + 3, panel_top + 6, "┄┄┈┈╌╌ ┆┊╎ dashes", DIM, None);
    grid.print(
        split + 3,
        panel_top + 7,
        "wide cells: 日本語テキスト ｜ 한글",
        FG,
        None,
    );
    grid.print(split + 3, panel_top + 8, "emoji: 🚀 🔥 ✅", FG, None);

    // A sparkline out of the eighth blocks, which is what they are for.
    let bars = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let mut spark = String::new();
    for i in 0..40 {
        let level = ((i as f32 * 0.7).sin() * 3.5 + 4.0) as usize;
        spark.push(bars[level.min(7)]);
    }
    grid.print(split + 3, panel_top + 10, &spark, CYAN, None);
    grid.print(3, panel_top + 10, "styles:", FG, None);
    grid.print_styled(
        11,
        panel_top + 10,
        "underlined",
        FG,
        None,
        CellStyle::UNDERLINE,
    );
    grid.print_styled(
        23,
        panel_top + 10,
        "struck out",
        DIM,
        None,
        CellStyle::STRIKE,
    );
    grid.print_styled(35, panel_top + 10, "bold", FG, None, CellStyle::BOLD);
    grid.print(41, panel_top + 10, " reversed ", BG, Some(GREEN));

    // Log lines below the panel, drawn with the same code the stream uses.
    let mut rng = Rng(0x5eed_1234_9876_4321);
    let first_log = panel_bottom + 2;
    for row in first_log..h - 2 {
        // push_log_line works at the bottom row, so lines are printed here
        // directly to leave the frame intact.
        let (level, color) = *rng.pick(&LEVELS);
        let mut col = grid.print(3, row, "  12.480  ", DIM, None);
        col = grid.print(col, row, level, BG, Some(color));
        col = grid.print(
            col,
            row,
            &format!("  {:<16}  ", rng.pick(&TARGETS)),
            CYAN,
            None,
        );
        let message = *rng.pick(&MESSAGES);
        for part in message.split('{') {
            if let Some(rest) = part.strip_prefix("n}") {
                col = grid.print(col, row, &(rng.next() % 1000).to_string(), MAGENTA, None);
                col = grid.print(col, row, rest, FG, None);
            } else {
                col = grid.print(col, row, part, FG, None);
            }
        }
    }
}

/// A box of `w × h` cells, drawn out of the light or double set.
fn frame_box(grid: &mut TermGrid, x: u16, y: u16, w: u16, h: u16, double: bool, color: Color) {
    let (tl, tr, bl, br, horiz, vert) = if double {
        ('╔', '╗', '╚', '╝', '═', '║')
    } else {
        ('┌', '┐', '└', '┘', '─', '│')
    };
    for col in x + 1..x + w - 1 {
        grid.set_cell(col, y, Cell::new(horiz, color));
        grid.set_cell(col, y + h - 1, Cell::new(horiz, color));
    }
    for row in y + 1..y + h - 1 {
        grid.set_cell(x, row, Cell::new(vert, color));
        grid.set_cell(x + w - 1, row, Cell::new(vert, color));
    }
    grid.set_cell(x, y, Cell::new(tl, color));
    grid.set_cell(x + w - 1, y, Cell::new(tr, color));
    grid.set_cell(x, y + h - 1, Cell::new(bl, color));
    grid.set_cell(x + w - 1, y + h - 1, Cell::new(br, color));
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

    let mut font_system = faf_text::font_system_from_fonts(&[
        faf_text::FONT_DEJAVU_SANS_MONO,
        faf_text::FONT_DEJAVU_SANS,
    ]);
    // For the CJK and emoji cells in the showcase: the fallback lane shapes
    // them, and shaping can only find what the database has.
    font_system.db_mut().load_system_fonts();

    let mut font = GridFont::new(
        &mut font_system,
        Family::Name("DejaVu Sans Mono"),
        FONT_SIZE,
    );
    let [cell_w, cell_h] = font.cell_size();
    let [grid_w, grid_h] = font.grid_size(COLS, ROWS);
    let width = (grid_w + 2.0 * MARGIN) as u32;
    let height = (grid_h + 2.0 * MARGIN) as u32;
    println!("grid {COLS}×{ROWS}, cell {cell_w}×{cell_h} px, surface {width}×{height}");

    let format = wgpu::TextureFormat::Rgba8Unorm;
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("term target"),
        size: wgpu::Extent3d {
            width,
            height,
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

    let mut renderer = TextRenderer::new(&device, format);
    let block = renderer.create_block();
    renderer.set_block_offset(block, [MARGIN, MARGIN]);

    let mut grid = TermGrid::new(COLS, ROWS, SCROLLBACK);
    let mut scene = GridScene::new();
    let mut rng = Rng(0x1234_5678_9abc_def0);

    // Fill the screen once so the first timed frame is a full one.
    for frame in 0..ROWS as usize {
        push_log_line(&mut grid, frame, &mut rng);
    }

    let mut translate = Vec::with_capacity(FRAMES);
    let mut upload = Vec::with_capacity(FRAMES);
    let mut instances = 0;
    for frame in 0..FRAMES {
        push_log_line(&mut grid, frame, &mut rng);
        renderer.begin_frame();

        // The measured number: cells → instances, on the CPU, from scratch.
        let start = Instant::now();
        grid.build(&mut font, &mut font_system, [0.0, 0.0], &mut scene);
        translate.push(start.elapsed().as_secs_f64() * 1e3);
        instances = scene.instances();

        let start = Instant::now();
        renderer.set_block_content(
            &queue,
            &mut font_system,
            block,
            &BlockContent {
                under_rects: &scene.rects,
                glyphs: &scene.glyphs,
                decorations: &scene.decorations,
                ..Default::default()
            },
        );
        renderer.finish(&device, &queue, [width as f32, height as f32]);
        upload.push(start.elapsed().as_secs_f64() * 1e3);

        draw(&device, &queue, &renderer, &target_view);
    }

    report("translate", &mut translate);
    report("upload   ", &mut upload);
    println!(
        "{instances} instances/frame ({} rects, {} glyphs, {} decorations)",
        scene.rects.len(),
        scene.glyphs.len(),
        scene.decorations.len()
    );
    let stats = renderer.upload_stats();
    println!(
        "last frame: {} instance writes, {} instances, {} uniform writes",
        stats.content_writes, stats.instances, stats.uniform_writes
    );

    // The acceptance frame: box drawing, blocks, shades, wide cells.
    draw_showcase(&mut grid);
    renderer.begin_frame();
    grid.render(
        &mut renderer,
        &queue,
        &mut font_system,
        &mut font,
        &mut scene,
        block,
        [0.0, 0.0],
    );
    renderer.finish(&device, &queue, [width as f32, height as f32]);
    draw(&device, &queue, &renderer, &target_view);

    let pixels = read_back(&device, &queue, &target, width, height);
    let file = std::fs::File::create("term.png").unwrap();
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .unwrap()
        .write_image_data(&pixels)
        .unwrap();
    println!("wrote term.png");
}

fn report(label: &str, samples: &mut [f64]) {
    samples.sort_by(f64::total_cmp);
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    println!(
        "{label}: mean {:.3} ms  p50 {:.3}  p95 {:.3}  max {:.3}  (n = {})",
        mean,
        samples[samples.len() / 2],
        samples[samples.len() * 95 / 100],
        samples[samples.len() - 1],
        samples.len()
    );
}

fn draw(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    renderer: &TextRenderer,
    view: &wgpu::TextureView,
) {
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("term pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
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
    queue.submit([encoder.finish()]);
}

fn read_back(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    target: &wgpu::Texture,
    width: u32,
    height: u32,
) -> Vec<u8> {
    let padded_row = (width * 4).next_multiple_of(256);
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("term readback"),
        size: (padded_row * height) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
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
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    queue.submit([encoder.finish()]);

    let slice = readback.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map failed"));
    device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    let data = slice.get_mapped_range().unwrap();
    let row = (width * 4) as usize;
    let mut pixels = Vec::with_capacity(row * height as usize);
    for y in 0..height as usize {
        let start = y * padded_row as usize;
        pixels.extend_from_slice(&data[start..start + row]);
    }
    pixels
}
