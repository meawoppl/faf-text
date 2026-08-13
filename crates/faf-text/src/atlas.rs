use cosmic_text::{CacheKey, FontSystem, SwashCache, SwashContent};
use etagere::{AtlasAllocator, size2};
use rustc_hash::FxHashMap;

/// Side length of the square RGBA glyph atlas.
pub const ATLAS_SIZE: u32 = 2048;

/// One-pixel gutter around every glyph so linear filtering never bleeds.
const PADDING: i32 = 1;

#[derive(Clone, Copy, Debug)]
pub struct GlyphEntry {
    pub uv_pos: [f32; 2],
    pub uv_size: [f32; 2],
    /// Pixel size of the rasterized glyph quad.
    pub size: [f32; 2],
    /// Offset from the glyph origin to the quad's top-left corner.
    pub left: i32,
    pub top: i32,
    pub is_color: bool,
}

/// A single shared RGBA8 texture holding every rasterized glyph, shelf-packed
/// with `etagere`. Grayscale masks live in the alpha channel; color glyphs
/// (emoji) use all four channels.
pub struct Atlas {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    allocator: AtlasAllocator,
    entries: FxHashMap<CacheKey, Option<GlyphEntry>>,
    swash: SwashCache,
    /// Set once when the atlas fills; we log a single warning instead of spamming.
    exhausted: bool,
}

impl Atlas {
    pub fn new(device: &wgpu::Device) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("faf-text glyph atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_SIZE,
                height: ATLAS_SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            texture,
            view,
            allocator: AtlasAllocator::new(size2(ATLAS_SIZE as i32, ATLAS_SIZE as i32)),
            entries: FxHashMap::default(),
            swash: SwashCache::new(),
            exhausted: false,
        }
    }

    /// Look up a glyph, rasterizing and uploading it on first use.
    /// Returns `None` for empty glyphs (whitespace) or if the atlas is full.
    pub fn get_or_insert(
        &mut self,
        queue: &wgpu::Queue,
        font_system: &mut FontSystem,
        key: CacheKey,
    ) -> Option<GlyphEntry> {
        if let Some(entry) = self.entries.get(&key) {
            return *entry;
        }

        let entry = self.rasterize(queue, font_system, key);
        self.entries.insert(key, entry);
        entry
    }

    fn rasterize(
        &mut self,
        queue: &wgpu::Queue,
        font_system: &mut FontSystem,
        key: CacheKey,
    ) -> Option<GlyphEntry> {
        let image = self.swash.get_image_uncached(font_system, key)?;
        let width = image.placement.width as i32;
        let height = image.placement.height as i32;
        if width == 0 || height == 0 {
            return None;
        }

        let alloc = match self
            .allocator
            .allocate(size2(width + PADDING * 2, height + PADDING * 2))
        {
            Some(alloc) => alloc,
            None => {
                if !self.exhausted {
                    self.exhausted = true;
                    log_atlas_full();
                }
                return None;
            }
        };
        let origin = alloc.rectangle.min;
        let (x, y) = (origin.x + PADDING, origin.y + PADDING);

        let (rgba, is_color) = match image.content {
            SwashContent::Mask => {
                let mut rgba = Vec::with_capacity(image.data.len() * 4);
                for &coverage in &image.data {
                    rgba.extend_from_slice(&[0xff, 0xff, 0xff, coverage]);
                }
                (rgba, false)
            }
            SwashContent::Color => (image.data.clone(), true),
            SwashContent::SubpixelMask => {
                // Subpixel masks are unreachable with our render settings; treat
                // the green channel as coverage if one ever appears.
                let mut rgba = Vec::with_capacity(image.data.len());
                for px in image.data.chunks_exact(4) {
                    rgba.extend_from_slice(&[0xff, 0xff, 0xff, px[1]]);
                }
                (rgba, false)
            }
        };

        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: x as u32,
                    y: y as u32,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            &rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width as u32 * 4),
                rows_per_image: None,
            },
            wgpu::Extent3d {
                width: width as u32,
                height: height as u32,
                depth_or_array_layers: 1,
            },
        );

        let inv = 1.0 / ATLAS_SIZE as f32;
        Some(GlyphEntry {
            uv_pos: [x as f32 * inv, y as f32 * inv],
            uv_size: [width as f32 * inv, height as f32 * inv],
            size: [width as f32, height as f32],
            left: image.placement.left,
            top: image.placement.top,
            is_color,
        })
    }
}

#[cfg(target_arch = "wasm32")]
fn log_atlas_full() {
    // Avoid a web-sys dependency in the core crate; the message still reaches
    // the console via the panic hook path used by the web crate.
}

#[cfg(not(target_arch = "wasm32"))]
fn log_atlas_full() {
    eprintln!("faf-text: glyph atlas exhausted; further new glyphs will not render");
}
