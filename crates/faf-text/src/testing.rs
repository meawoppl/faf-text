//! Scaffolding for the GPU-backed unit tests: one shared headless device
//! (adapter setup is slow, and tests run in parallel) plus offscreen render
//! and readback.

use std::sync::OnceLock;

use cosmic_text::fontdb;

use crate::renderer::TextRenderer;
use crate::{
    FONT_DEJAVU_SANS, FONT_MANROPE_VARIABLE, FONT_TWEMOJI_COLR, FontSystem, font_system_from_fonts,
};

pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// A headless device, created once and shared by every test in the binary.
pub fn gpu() -> &'static (wgpu::Device, wgpu::Queue) {
    static GPU: OnceLock<(wgpu::Device, wgpu::Queue)> = OnceLock::new();
    GPU.get_or_init(|| {
        pollster::block_on(async {
            let instance = wgpu::Instance::default();
            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    ..Default::default()
                })
                .await
                .expect("no GPU adapter available");
            adapter
                .request_device(&wgpu::DeviceDescriptor::default())
                .await
                .expect("failed to acquire device")
        })
    })
}

/// A headless device with `DUAL_SOURCE_BLENDING`, or `None` where the adapter
/// has no such feature (which is where the renderer falls back to grayscale
/// coverage, so the tests that need this skip themselves).
pub fn dual_source_gpu() -> Option<&'static (wgpu::Device, wgpu::Queue)> {
    static GPU: OnceLock<Option<(wgpu::Device, wgpu::Queue)>> = OnceLock::new();
    GPU.get_or_init(|| {
        pollster::block_on(async {
            let instance = wgpu::Instance::default();
            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    ..Default::default()
                })
                .await
                .expect("no GPU adapter available");
            if !adapter
                .features()
                .contains(wgpu::Features::DUAL_SOURCE_BLENDING)
            {
                return None;
            }
            adapter
                .request_device(&wgpu::DeviceDescriptor {
                    required_features: wgpu::Features::DUAL_SOURCE_BLENDING,
                    ..Default::default()
                })
                .await
                .ok()
        })
    })
    .as_ref()
}

/// A headless device with `PIPELINE_STATISTICS_QUERY`, or `None` where the
/// adapter has no such feature. Only the fill-rate measurements need it, and
/// they skip themselves without it.
pub fn stats_gpu() -> Option<&'static (wgpu::Device, wgpu::Queue)> {
    static GPU: OnceLock<Option<(wgpu::Device, wgpu::Queue)>> = OnceLock::new();
    GPU.get_or_init(|| {
        pollster::block_on(async {
            let instance = wgpu::Instance::default();
            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    ..Default::default()
                })
                .await
                .expect("no GPU adapter available");
            if !adapter
                .features()
                .contains(wgpu::Features::PIPELINE_STATISTICS_QUERY)
            {
                return None;
            }
            adapter
                .request_device(&wgpu::DeviceDescriptor {
                    required_features: wgpu::Features::PIPELINE_STATISTICS_QUERY,
                    ..Default::default()
                })
                .await
                .ok()
        })
    })
    .as_ref()
}

/// Fragment-shader invocations one frame of `renderer` costs, counted by the
/// hardware — the number corner clipping (#14) is out to reduce. Includes the
/// helper invocations `fwidth` forces, since those are real work; what it
/// measures is what the GPU actually ran.
pub fn fragment_invocations(
    (device, queue): &(wgpu::Device, wgpu::Queue),
    renderer: &mut TextRenderer,
    width: u32,
    height: u32,
) -> u64 {
    renderer.finish(device, queue, [width as f32, height as f32]);

    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("fill-count target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let queries = device.create_query_set(&wgpu::QuerySetDescriptor {
        label: Some("fragment invocations"),
        ty: wgpu::QueryType::PipelineStatistics(
            wgpu::PipelineStatisticsTypes::FRAGMENT_SHADER_INVOCATIONS,
        ),
        count: 1,
    });
    // One u64 per statistic asked for.
    let resolved = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("query resolve"),
        size: 8,
        usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("query readback"),
        size: 8,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("fill-count pass"),
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
        pass.begin_pipeline_statistics_query(&queries, 0);
        renderer.render(&mut pass);
        pass.end_pipeline_statistics_query();
    }
    encoder.resolve_query_set(&queries, 0..1, &resolved, 0);
    encoder.copy_buffer_to_buffer(&resolved, 0, &readback, 0, 8);
    queue.submit([encoder.finish()]);

    let slice = readback.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map failed"));
    device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    let data = slice.get_mapped_range().unwrap();
    u64::from_le_bytes(data[..8].try_into().unwrap())
}

/// A font system holding just DejaVu Sans, so glyph ids are stable.
pub fn font_system() -> FontSystem {
    font_system_from_fonts(&[FONT_DEJAVU_SANS])
}

/// A font system with Manrope in it, whose `wght` axis (200–800) gives every
/// glyph two masters to blend between.
pub fn variable_font_system() -> FontSystem {
    font_system_from_fonts(&[FONT_MANROPE_VARIABLE])
}

/// A font system with the vendored COLRv0 emoji subset in it, whose glyphs are
/// stacks of palette-colored outlines rather than bitmaps.
pub fn color_font_system() -> FontSystem {
    font_system_from_fonts(&[FONT_DEJAVU_SANS, FONT_TWEMOJI_COLR])
}

/// Family name of the static face the tests fall back on.
pub const STATIC_FAMILY: &str = "DejaVu Sans";

/// Family name of the face [`variable_font_system`] is loaded for.
pub const VARIABLE_FAMILY: &str = "Manrope";

/// Family name of the face [`color_font_system`] adds.
pub const COLOR_FAMILY: &str = "Twemoji Mozilla";

/// Where this box keeps a CBDT (bitmap) color font — the negative case for the
/// COLR path, which has to leave it on the atlas. Tests using it skip
/// themselves when it is missing.
pub const CBDT_FONT_PATH: &str = "/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf";

/// A font system holding the CBDT color font above, or `None` where this box
/// does not have it.
pub fn bitmap_color_font_system() -> Option<FontSystem> {
    let data = std::fs::read(CBDT_FONT_PATH).ok()?;
    Some(FontSystem::new_with_fonts([fontdb::Source::Binary(
        std::sync::Arc::new(data),
    )]))
}

/// The glyph id `ch` maps to in a face, or `None` when the face has no glyph
/// for it (swash reports a miss as glyph 0).
pub fn glyph_id_of(font_system: &mut FontSystem, id: fontdb::ID, ch: char) -> Option<u16> {
    let font = font_system.get_font(id, fontdb::Weight::NORMAL)?;
    match font.as_swash().charmap().map(ch) {
        0 => None,
        glyph_id => Some(glyph_id),
    }
}

/// The id of the first face in `font_system`.
pub fn font_id(font_system: &FontSystem) -> fontdb::ID {
    font_system.db().faces().next().expect("no faces loaded").id
}

/// The id of the face for `family`. cosmic-text's `new_with_fonts` scans the
/// system fonts as well as the blobs it is handed, so an embedded font is
/// nowhere near the front of the database — it is appended, hence `last`.
pub fn font_id_of(font_system: &FontSystem, family: &str) -> fontdb::ID {
    font_system
        .db()
        .faces()
        .filter(|face| face.families.iter().any(|(name, _)| name == family))
        .last()
        .unwrap_or_else(|| panic!("no face loaded for family {family}"))
        .id
}

/// Finish the frame, draw everything queued on `renderer` into a `width ×
/// height` target, and read it back as tightly packed RGBA8.
pub fn render_pixels(renderer: &mut TextRenderer, width: u32, height: u32) -> Vec<u8> {
    render_pixels_on(gpu(), renderer, width, height, FORMAT)
}

/// [`render_pixels`] on a chosen device, rendering through a view of
/// `view_format` — which is how the gamma-correct pipelines are exercised: the
/// texture stays [`FORMAT`], so the readback is still raw sRGB-encoded bytes,
/// but the attachment the blender sees is the sRGB view of it.
pub fn render_pixels_on(
    (device, queue): &(wgpu::Device, wgpu::Queue),
    renderer: &mut TextRenderer,
    width: u32,
    height: u32,
    view_format: wgpu::TextureFormat,
) -> Vec<u8> {
    renderer.finish(device, queue, [width as f32, height as f32]);

    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("test target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[view_format],
    });
    let target_view = target.create_view(&wgpu::TextureViewDescriptor {
        format: Some(view_format),
        ..Default::default()
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("test pass"),
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

    let padded_row = (width * 4).next_multiple_of(256);
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("test readback"),
        size: (padded_row * height) as u64,
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
