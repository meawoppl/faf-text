use cosmic_text::{CacheKey, FontSystem, SwashCache, SwashContent};
use etagere::{AllocId, Allocation, AtlasAllocator, Size, size2};
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

/// A cached rasterization plus its shelf allocation and the frame it was last
/// drawn on, which drives eviction.
struct Entry {
    glyph: Option<GlyphEntry>,
    alloc: Option<AllocId>,
    last_used: u64,
}

/// Outcome of a rasterization attempt.
enum Rasterized {
    /// Settled: a packed glyph, or a definitive "nothing to draw". Cacheable.
    Done(Option<(GlyphEntry, AllocId)>),
    /// The atlas is out of room even after evicting stale glyphs. Nothing is
    /// cached, so the glyph is retried once space frees up.
    Full,
}

/// A single shared RGBA8 texture holding every rasterized glyph, shelf-packed
/// with `etagere`. Grayscale masks live in the alpha channel; color glyphs
/// (emoji) use all four channels.
///
/// When the shelf allocator runs dry, every glyph not drawn this frame or last
/// is deallocated and the allocation retried. Glyphs still in use are never
/// evicted mid-frame — their UVs are already baked into queued instances — so
/// if the retry still fails (free space too fragmented to reuse) the whole
/// allocator is reset at the next frame edge and glyphs re-rasterize lazily.
pub struct Atlas {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    /// Bumped by the wholesale reset, the one event that invalidates a UV a
    /// retained instance is holding. Ordinary eviction cannot: the renderer
    /// touches the keys its blocks reference at every frame edge, so a live
    /// glyph is never stale enough to be dropped.
    pub generation: u64,
    size: u32,
    allocator: AtlasAllocator,
    entries: FxHashMap<CacheKey, Entry>,
    swash: SwashCache,
    frame: u64,
    /// Allocation failed even after eviction; wipe the allocator next frame.
    needs_reset: bool,
}

impl Atlas {
    pub fn new(device: &wgpu::Device) -> Self {
        Self::with_size(device, ATLAS_SIZE)
    }

    /// Same as [`Atlas::new`] with an explicit side length (tests use a tiny
    /// atlas to exercise eviction).
    pub(crate) fn with_size(device: &wgpu::Device, size: u32) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("faf-text glyph atlas"),
            size: wgpu::Extent3d {
                width: size,
                height: size,
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
            generation: 0,
            size,
            allocator: AtlasAllocator::new(size2(size as i32, size as i32)),
            entries: FxHashMap::default(),
            swash: SwashCache::new(),
            frame: 0,
            needs_reset: false,
        }
    }

    /// Start a frame: stamp lookups with `frame` and run any reset that last
    /// frame deferred.
    pub fn begin_frame(&mut self, frame: u64) {
        self.frame = frame;
        if self.needs_reset {
            self.needs_reset = false;
            self.allocator.clear();
            self.entries.clear();
            self.generation += 1;
        }
    }

    /// Stamp a glyph as used on `frame` without looking anything up.
    ///
    /// Eviction drops whatever was not drawn this frame or last, and a
    /// retained block bakes its UVs once instead of asking again every frame —
    /// so the renderer touches the keys its live blocks reference with the
    /// frame about to start, or the shelf under them would be handed to another
    /// glyph.
    pub fn touch(&mut self, key: &CacheKey, frame: u64) {
        if let Some(entry) = self.entries.get_mut(key) {
            entry.last_used = frame;
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
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.last_used = self.frame;
            return entry.glyph;
        }

        match self.rasterize(queue, font_system, key) {
            Rasterized::Done(packed) => {
                let glyph = packed.map(|(glyph, _)| glyph);
                self.entries.insert(
                    key,
                    Entry {
                        glyph,
                        alloc: packed.map(|(_, alloc)| alloc),
                        last_used: self.frame,
                    },
                );
                glyph
            }
            Rasterized::Full => None,
        }
    }

    /// Allocate, evicting glyphs untouched this frame and last if the shelf
    /// allocator is dry.
    fn allocate(&mut self, size: Size) -> Option<Allocation> {
        if let Some(alloc) = self.allocator.allocate(size) {
            return Some(alloc);
        }
        let Self {
            entries, allocator, ..
        } = self;
        let keep_from = self.frame.saturating_sub(1);
        entries.retain(|_, entry| {
            if entry.last_used >= keep_from {
                return true;
            }
            if let Some(id) = entry.alloc {
                allocator.deallocate(id);
            }
            false
        });
        if let Some(alloc) = self.allocator.allocate(size) {
            return Some(alloc);
        }
        // Everything left is live this frame, or the free space is too
        // fragmented to reuse. Start over at the next frame edge rather than
        // pulling texture space out from under queued instances.
        self.needs_reset = true;
        None
    }

    fn rasterize(
        &mut self,
        queue: &wgpu::Queue,
        font_system: &mut FontSystem,
        key: CacheKey,
    ) -> Rasterized {
        let Some(image) = self.swash.get_image_uncached(font_system, key) else {
            return Rasterized::Done(None);
        };
        let width = image.placement.width as i32;
        let height = image.placement.height as i32;
        if width == 0 || height == 0 {
            return Rasterized::Done(None);
        }

        let Some(alloc) = self.allocate(size2(width + PADDING * 2, height + PADDING * 2)) else {
            return Rasterized::Full;
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

        let inv = 1.0 / self.size as f32;
        Rasterized::Done(Some((
            GlyphEntry {
                uv_pos: [x as f32 * inv, y as f32 * inv],
                uv_size: [width as f32 * inv, height as f32 * inv],
                size: [width as f32, height as f32],
                left: image.placement.left,
                top: image.placement.top,
                is_color,
            },
            alloc.id,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;
    use cosmic_text::{CacheKeyFlags, fontdb};

    /// Tiny atlas: a handful of 24 px glyphs fill it.
    const SIZE: u32 = 64;

    fn key(font_id: fontdb::ID, glyph_id: u16) -> CacheKey {
        CacheKey::new(
            font_id,
            glyph_id,
            24.0,
            (0.0, 0.0),
            fontdb::Weight::NORMAL,
            CacheKeyFlags::empty(),
        )
        .0
    }

    /// Insert glyphs until the atlas gives up, returning the ids that packed
    /// and the id that did not.
    fn fill(
        atlas: &mut Atlas,
        font_system: &mut FontSystem,
        font_id: fontdb::ID,
    ) -> (Vec<u16>, u16) {
        let (_, queue) = testing::gpu();
        let mut packed = Vec::new();
        for glyph_id in 36..500 {
            if atlas
                .get_or_insert(queue, font_system, key(font_id, glyph_id))
                .is_some()
            {
                packed.push(glyph_id);
            }
            if atlas.needs_reset {
                return (packed, glyph_id);
            }
        }
        panic!("the atlas never filled up");
    }

    #[test]
    fn a_full_atlas_of_live_glyphs_resets_at_the_next_frame_edge() {
        let (device, _) = testing::gpu();
        let mut font_system = testing::font_system();
        let font_id = testing::font_id(&font_system);
        let mut atlas = Atlas::with_size(device, SIZE);

        atlas.begin_frame(1);
        let (packed, _) = fill(&mut atlas, &mut font_system, font_id);
        assert!(!packed.is_empty());
        assert!(!atlas.entries.is_empty(), "nothing is evicted mid-frame");

        atlas.begin_frame(2);
        assert!(!atlas.needs_reset);
        assert!(atlas.entries.is_empty(), "the entry map is wiped");
        assert!(atlas.allocator.is_empty(), "so is the shelf allocator");
    }

    #[test]
    fn stale_glyphs_are_evicted_to_make_room() {
        let (device, queue) = testing::gpu();
        let mut font_system = testing::font_system();
        let font_id = testing::font_id(&font_system);
        let mut atlas = Atlas::with_size(device, SIZE);

        // Learn how much fits, then start over from a clean allocator.
        atlas.begin_frame(1);
        let (packed, wall) = fill(&mut atlas, &mut font_system, font_id);
        atlas.begin_frame(2);

        // Same glyphs, same order, same packing — the atlas is full again,
        // this time without a pending reset.
        for glyph_id in &packed {
            assert!(
                atlas
                    .get_or_insert(queue, &mut font_system, key(font_id, *glyph_id))
                    .is_some()
            );
        }
        assert!(!atlas.needs_reset);
        assert_eq!(atlas.entries.len(), packed.len());

        // Two frames later nothing in there is live, so the glyph that did not
        // fit before now evicts its way in.
        atlas.begin_frame(4);
        assert!(
            atlas
                .get_or_insert(queue, &mut font_system, key(font_id, wall))
                .is_some(),
            "eviction should have freed room"
        );
        assert!(!atlas.needs_reset, "no nuclear reset was needed");
        assert_eq!(atlas.entries.len(), 1, "the stale glyphs are gone");
    }

    #[test]
    fn glyphs_drawn_this_frame_survive_an_eviction_pass() {
        let (device, queue) = testing::gpu();
        let mut font_system = testing::font_system();
        let font_id = testing::font_id(&font_system);
        let mut atlas = Atlas::with_size(device, SIZE);

        atlas.begin_frame(1);
        let (packed, wall) = fill(&mut atlas, &mut font_system, font_id);
        atlas.begin_frame(2);
        for glyph_id in &packed {
            atlas.get_or_insert(queue, &mut font_system, key(font_id, *glyph_id));
        }

        // Frame 4 touches one glyph before overflowing: its allocation, and so
        // the UVs already queued for it, must survive the eviction pass.
        atlas.begin_frame(4);
        let live = key(font_id, packed[0]);
        let entry = atlas
            .get_or_insert(queue, &mut font_system, live)
            .expect("still cached");
        atlas.get_or_insert(queue, &mut font_system, key(font_id, wall));
        assert_eq!(
            atlas
                .get_or_insert(queue, &mut font_system, live)
                .map(|e| e.uv_pos),
            Some(entry.uv_pos),
            "a glyph used this frame keeps its slot"
        );
    }
}
