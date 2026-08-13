//! wasm-bindgen glue: binds the faf-text renderer to an HTML canvas with
//! pointer-driven selection, keyboard editing, IME composition and search
//! highlighting.

use std::ops::Range;

use faf_text::cosmic_text::{Attrs, Cursor, Family, Metrics};
use faf_text::glam::{Mat4, Vec3, Vec4};
use faf_text::math::{block_hit, ndc_ray, pointer_ndc, screen_perspective};
use faf_text::{
    BlockContent, BlockId, Color, DecorationKind, FontSystem, Rect, TextRenderer, TextView,
};
use wasm_bindgen::prelude::*;
use web_sys::HtmlCanvasElement;

const SELECTION: Color = Color::rgba(0.23, 0.39, 0.66, 1.0);
const HIGHLIGHT: Color = Color::rgba(0.88, 0.69, 0.41, 0.38);
/// Search marks that are a line rather than a wash of color want full alpha.
const MARK: Color = Color::rgba(0.97, 0.46, 0.56, 1.0);
const FOREGROUND: Color = Color::rgba(0.75, 0.79, 0.96, 1.0);
const CARET: Color = Color::rgba(1.0, 0.62, 0.39, 1.0);
const MARGIN: f32 = 24.0;
/// The embedded variable font, whose `wght` masters the GPU can blend.
const VARIABLE_FAMILY: &str = "Manrope";
/// Caret thickness, in physical pixels.
const CARET_WIDTH: f32 = 2.0;
/// Corner radius of a search chip, in CSS pixels.
const CHIP_RADIUS: f32 = 6.0;
/// Vertical field of view of the 3D camera, radians.
const FOV_Y: f32 = 0.7;
/// How far the tilted pane shrinks, so its near edge stays on the canvas.
const TILT_SCALE: f32 = 0.78;

/// How a search match is marked. Everything but `Highlight` goes through the
/// decoration pipeline.
#[derive(Clone, Copy, Default, PartialEq)]
enum SearchMode {
    /// An alpha-blended rect over the glyphs — the original behaviour.
    #[default]
    Highlight,
    Underline,
    Squiggle,
    /// A rounded chip behind... well, over: the search block composites above
    /// the text block, so its chips read as rounded highlights.
    Chip,
}

impl SearchMode {
    fn parse(name: &str) -> Self {
        match name {
            "underline" => Self::Underline,
            "squiggle" => Self::Squiggle,
            "chip" => Self::Chip,
            _ => Self::Highlight,
        }
    }

    /// The decoration this mode draws, if it draws one.
    fn kind(self, radius_px: f32) -> Option<DecorationKind> {
        match self {
            Self::Highlight => None,
            Self::Underline => Some(DecorationKind::Underline),
            Self::Squiggle => Some(DecorationKind::Squiggle),
            Self::Chip => Some(DecorationKind::Chip { radius_px }),
        }
    }
}

/// Line separators cosmic-text splits paragraphs on, normalized to `\n` so
/// that byte offsets in the backing string line up with buffer lines.
fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace(['\r', '\u{b}', '\u{c}', '\u{1c}', '\u{1d}', '\u{1e}'], "\n")
        .replace(['\u{85}', '\u{2028}', '\u{2029}'], "\n")
}

/// Byte offset of a cursor in the backing string.
fn offset_of(text: &str, c: Cursor) -> usize {
    let mut start = 0;
    for (i, line) in text.split('\n').enumerate() {
        if i == c.line {
            return start + c.index.min(line.len());
        }
        start += line.len() + 1;
    }
    text.len()
}

/// Inverse of [`offset_of`].
fn cursor_of(text: &str, offset: usize) -> Cursor {
    let offset = offset.min(text.len());
    let mut start = 0;
    for (i, line) in text.split('\n').enumerate() {
        let end = start + line.len();
        if offset <= end {
            return Cursor::new(i, offset - start);
        }
        start = end + 1;
    }
    Cursor::new(0, 0)
}

/// Which retained blocks a mutation invalidated. Everything the demo draws
/// lives in one of four blocks, and a change to one of them re-uploads that
/// block's instances and nothing else — typing in the search box never touches
/// the text block's several thousand glyphs.
#[derive(Clone, Copy, Default)]
struct Dirty {
    text: bool,
    selection: bool,
    search: bool,
    caret: bool,
}

impl Dirty {
    /// A re-shape moves every piece of geometry, so it dirties everything.
    fn all() -> Self {
        Self {
            text: true,
            selection: true,
            search: true,
            caret: true,
        }
    }

    /// The caret moved: selection geometry and the caret follow it, the text
    /// does not.
    fn cursor(&mut self) {
        self.selection = true;
        self.caret = true;
    }
}

#[wasm_bindgen]
pub struct FafTextDemo {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    backend: String,
    renderer: TextRenderer,
    font_system: FontSystem,
    view: TextView,
    dpr: f32,
    font_size: f32,
    /// Authoritative text; the shaped buffer is rebuilt from it on every edit.
    text: String,
    /// Insertion point, and the fixed end of the selection.
    cursor: Cursor,
    anchor: Cursor,
    /// Column vertical motion tries to keep, in physical pixels.
    sticky_x: Option<f32>,
    /// Byte range of in-flight IME composition text inside `text`.
    composition: Option<Range<usize>>,
    caret_visible: bool,
    dragging: bool,
    search: String,
    search_mode: SearchMode,
    /// GPU weight blend for the whole view, and with it the family the text is
    /// shaped in: `None` is the static demo font, `Some(t)` the variable one.
    weight_blend: Option<f32>,
    /// Y rotation of the whole pane in degrees, or `None` for flat 2D. Every
    /// block shares it, so caret and selection stay glued to the glyphs.
    tilt: Option<f32>,
    /// The scene, in z-order. Selection sits in its own block *below* the text
    /// because blocks composite in creation order — a block's own under-rects
    /// only go under its own glyphs. Search highlights and the caret go above.
    selection_block: BlockId,
    text_block: BlockId,
    search_block: BlockId,
    caret_block: BlockId,
    dirty: Dirty,
}

#[wasm_bindgen]
impl FafTextDemo {
    /// Create the renderer on a canvas. The canvas backing size should
    /// already be set (CSS size × devicePixelRatio). Pass `force_gl` when a
    /// JS-side `navigator.gpu.requestAdapter()` probe failed — wgpu's WebGPU
    /// backend throws an uncatchable TypeError on null adapters.
    pub async fn attach(
        canvas: HtmlCanvasElement,
        dpr: f32,
        force_gl: bool,
    ) -> Result<FafTextDemo, JsValue> {
        console_error_panic_hook::set_once();

        let width = canvas.width().max(1);
        let height = canvas.height().max(1);

        let backend_order: &[wgpu::Backends] = if force_gl {
            &[wgpu::Backends::GL]
        } else {
            &[wgpu::Backends::BROWSER_WEBGPU, wgpu::Backends::GL]
        };
        let mut picked = None;
        for &backends in backend_order {
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
                backends,
                ..wgpu::InstanceDescriptor::new_without_display_handle()
            });
            let Ok(surface) = instance.create_surface(wgpu::SurfaceTarget::Canvas(canvas.clone()))
            else {
                continue;
            };
            if let Ok(adapter) = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    compatible_surface: Some(&surface),
                    ..Default::default()
                })
                .await
            {
                picked = Some((surface, adapter));
                break;
            }
        }
        let Some((surface, adapter)) = picked else {
            return Err(JsValue::from_str("no WebGPU or WebGL2 adapter available"));
        };
        let backend = format!("{:?}", adapter.get_info().backend);
        let limits = wgpu::Limits::downlevel_webgl2_defaults().using_resolution(adapter.limits());
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                required_limits: limits,
                ..Default::default()
            })
            .await
            .map_err(|e| JsValue::from_str(&format!("no device: {e}")))?;

        let caps = surface.get_capabilities(&adapter);
        // Prefer a non-sRGB format: text is traditionally blended in gamma
        // space, which is what CSS text rendering does too.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Srgb,
            width,
            height,
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&device, &config);

        let mut renderer = TextRenderer::new(&device, format);
        let selection_block = renderer.create_block();
        let text_block = renderer.create_block();
        let search_block = renderer.create_block();
        let caret_block = renderer.create_block();
        let mut font_system = faf_text::font_system_from_fonts(&[
            faf_text::FONT_DEJAVU_SANS,
            faf_text::FONT_DEJAVU_SANS_MONO,
            faf_text::FONT_MANROPE_VARIABLE,
        ]);

        let font_size = 18.0;
        let mut view = TextView::new(
            &mut font_system,
            Metrics::new(font_size * dpr, font_size * dpr * 1.5),
        );
        view.pos = [MARGIN * dpr, MARGIN * dpr];
        view.set_size(
            &mut font_system,
            Some(width as f32 - 2.0 * MARGIN * dpr),
            None,
        );

        Ok(FafTextDemo {
            surface,
            device,
            queue,
            config,
            backend,
            renderer,
            font_system,
            view,
            dpr,
            font_size,
            text: String::new(),
            cursor: Cursor::new(0, 0),
            anchor: Cursor::new(0, 0),
            sticky_x: None,
            composition: None,
            caret_visible: true,
            dragging: false,
            search: String::new(),
            search_mode: SearchMode::default(),
            weight_blend: None,
            tilt: None,
            selection_block,
            text_block,
            search_block,
            caret_block,
            dirty: Dirty::all(),
        })
    }

    pub fn backend(&self) -> String {
        self.backend.clone()
    }

    pub fn set_text(&mut self, text: &str) {
        self.text = normalize(text);
        self.reshape();
        self.composition = None;
        self.sticky_x = None;
        self.set_cursor(Cursor::new(0, 0), false);
    }

    /// The full text, so JS can round-trip it (save, copy-all, tests).
    pub fn text(&self) -> String {
        self.text.clone()
    }

    pub fn set_font_size(&mut self, size: f32) {
        if self.font_size == size {
            return;
        }
        self.font_size = size;
        let px = size * self.dpr;
        self.view
            .set_metrics(&mut self.font_system, Metrics::new(px, px * 1.5));
        self.dirty = Dirty::all();
    }

    /// Blend the whole view between the variable font's lightest and boldest
    /// masters, in the fragment shader: 0 is the axis minimum, 1 the maximum.
    /// `undefined` goes back to the static demo font and its shaped weight.
    /// Cheap enough to drive from a per-frame sine — nothing re-shapes and no
    /// curve is re-extracted.
    pub fn set_weight_blend(&mut self, t: Option<f32>) {
        // Only the font swap needs a re-shape; the blend itself is free.
        let reshape = t.is_some() != self.weight_blend.is_some();
        let t = t.map(|t| t.clamp(0.0, 1.0));
        if t == self.weight_blend {
            return;
        }
        self.weight_blend = t;
        if reshape {
            self.reshape();
        } else {
            // The blend is baked into each glyph instance, so animating it is
            // one block's worth of re-upload and no shaping at all.
            self.dirty.text = true;
        }
    }

    pub fn set_search(&mut self, needle: &str) {
        if self.search == needle {
            return;
        }
        self.search = needle.to_string();
        self.dirty.search = true;
    }

    /// How search matches are marked: `highlight` (an alpha-blended rect over
    /// the glyphs), or `underline`, `squiggle` or `chip` — the same cursor
    /// ranges routed through the decoration pipeline instead.
    pub fn set_search_mode(&mut self, mode: &str) {
        let mode = SearchMode::parse(mode);
        if self.search_mode == mode {
            return;
        }
        self.search_mode = mode;
        self.dirty.search = true;
    }

    pub fn resize(&mut self, width: u32, height: u32, dpr: f32) {
        self.dpr = dpr;
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        self.surface.configure(&self.device, &self.config);
        self.view.pos = [MARGIN * dpr, MARGIN * dpr];
        let px = self.font_size * dpr;
        self.view
            .set_metrics(&mut self.font_system, Metrics::new(px, px * 1.5));
        self.view.set_size(
            &mut self.font_system,
            Some(self.config.width as f32 - 2.0 * MARGIN * dpr),
            None,
        );
        self.dirty = Dirty::all();
    }

    /// Stand the text pane up in 3D: `degrees` of rotation about its own
    /// vertical axis, seen through a perspective camera. `undefined` puts it
    /// back flat, where the renderer's placement math is bit-for-bit the plain
    /// 2D path again.
    ///
    /// Nothing re-shapes and no instance is rebuilt — this is one matrix per
    /// block per frame — so a slow sway is free to animate. Pointer selection
    /// keeps working: [`FafTextDemo::pointer_down`] casts a ray instead of
    /// reading a pixel.
    pub fn set_tilt(&mut self, degrees: Option<f32>) {
        if self.tilt == degrees {
            return;
        }
        self.tilt = degrees;
    }

    /// Pointer coordinates are CSS px relative to the canvas.
    pub fn pointer_down(&mut self, x: f32, y: f32) {
        if let Some(cursor) = self.hit(x, y) {
            self.set_cursor(cursor, false);
            self.dragging = true;
        }
    }

    pub fn pointer_move(&mut self, x: f32, y: f32) {
        if !self.dragging {
            return;
        }
        if let Some(cursor) = self.hit(x, y) {
            self.set_cursor(cursor, true);
        }
    }

    pub fn pointer_up(&mut self) {
        self.dragging = false;
    }

    /// The currently selected text (for clipboard integration in JS).
    pub fn selected_text(&self) -> String {
        match self.selection_range() {
            Some(range) => self.text[range].to_string(),
            None => String::new(),
        }
    }

    /// Caret geometry in CSS pixels relative to the canvas: `[x, y, w, h]`.
    /// JS parks the hidden IME input here so the composition popup lands on
    /// the caret.
    ///
    /// While the pane is tilted the caret is projected through the same
    /// matrices the glyphs go through, so the composition popup follows it
    /// into 3D.
    pub fn caret_css_rect(&self) -> Box<[f32]> {
        let rect = self
            .view
            .cursor_rect(self.cursor)
            .map_or([0.0, 0.0, 0.0, 0.0], |r| [r[0], r[1], CARET_WIDTH, r[3]]);
        let rect = match self.pane_model() {
            Some(model) => {
                let local = self.local(rect);
                let top = self.project(&model, [local[0], local[1]]);
                let bottom = self.project(&model, [local[0], local[1] + local[3]]);
                match (top, bottom) {
                    (Some(top), Some(bottom)) => {
                        [top[0], top[1], CARET_WIDTH, (bottom[1] - top[1]).abs()]
                    }
                    _ => rect,
                }
            }
            None => rect,
        };
        rect.map(|v| v / self.dpr).into()
    }

    /// Caret blink is host-side: JS toggles this on a timer and resets it on
    /// every edit or motion.
    pub fn set_caret_visible(&mut self, visible: bool) {
        if self.caret_visible == visible {
            return;
        }
        self.caret_visible = visible;
        self.dirty.caret = true;
    }

    /// Handle a `keydown`. `key` is the raw `KeyboardEvent.key` value; any
    /// single-character key inserts itself. Returns true when the event was
    /// consumed and JS should call `preventDefault()`.
    pub fn key_input(&mut self, key: &str, ctrl: bool, shift: bool) -> bool {
        match key {
            "ArrowLeft" | "ArrowRight" => {
                let forward = key == "ArrowRight";
                // A plain arrow with a selection collapses it to that edge.
                match self.selection_range() {
                    Some(range) if !shift => {
                        let offset = if forward { range.end } else { range.start };
                        self.set_cursor(cursor_of(&self.text, offset), false);
                    }
                    _ => {
                        let next = match (forward, ctrl) {
                            (true, false) => self.view.move_right(self.cursor),
                            (true, true) => self.view.move_word_right(self.cursor),
                            (false, false) => self.view.move_left(self.cursor),
                            (false, true) => self.view.move_word_left(self.cursor),
                        };
                        self.set_cursor(next, shift);
                    }
                }
                self.sticky_x = None;
                true
            }
            "ArrowUp" | "ArrowDown" => {
                let sticky = match self.sticky_x {
                    Some(x) => x,
                    None => self.view.cursor_rect(self.cursor).map_or(0.0, |r| r[0]),
                };
                let next = if key == "ArrowUp" {
                    self.view.move_up(self.cursor, sticky)
                } else {
                    self.view.move_down(self.cursor, sticky)
                };
                self.set_cursor(next, shift);
                self.sticky_x = Some(sticky);
                true
            }
            "Home" | "End" => {
                let next = if ctrl {
                    let offset = if key == "Home" { 0 } else { self.text.len() };
                    cursor_of(&self.text, offset)
                } else if key == "Home" {
                    self.view.line_start(self.cursor)
                } else {
                    self.view.line_end(self.cursor)
                };
                self.set_cursor(next, shift);
                self.sticky_x = None;
                true
            }
            "Backspace" | "Delete" => {
                let range = self.selection_range().unwrap_or_else(|| {
                    let here = offset_of(&self.text, self.cursor);
                    let other = if key == "Backspace" {
                        offset_of(&self.text, self.view.move_left(self.cursor))
                    } else {
                        offset_of(&self.text, self.view.move_right(self.cursor))
                    };
                    here.min(other)..here.max(other)
                });
                self.replace_range(range, "");
                true
            }
            "Enter" => {
                self.insert_text("\n");
                true
            }
            "Tab" => {
                self.insert_text("    ");
                true
            }
            "a" | "A" if ctrl => {
                self.anchor = Cursor::new(0, 0);
                self.cursor = cursor_of(&self.text, self.text.len());
                self.sticky_x = None;
                self.dirty.cursor();
                true
            }
            // Copy/cut/paste stay with the browser's clipboard events.
            _ if ctrl => false,
            _ if key.chars().count() == 1 => {
                self.insert_text(key);
                true
            }
            _ => false,
        }
    }

    /// Insert text at the caret, replacing the selection.
    pub fn insert_text(&mut self, text: &str) {
        let range = self.selection_range().unwrap_or_else(|| {
            offset_of(&self.text, self.cursor)..offset_of(&self.text, self.cursor)
        });
        self.replace_range(range, &normalize(text));
    }

    /// IME composition started: the pending text replaces the selection.
    pub fn composition_start(&mut self) {
        if let Some(range) = self.selection_range() {
            self.replace_range(range, "");
        }
        let at = offset_of(&self.text, self.cursor);
        self.composition = Some(at..at);
    }

    /// Preedit text changed. It lives in the backing string so it shapes and
    /// renders inline (underlined) at the caret.
    pub fn composition_update(&mut self, text: &str) {
        let range = match self.composition.clone() {
            Some(range) => range,
            None => {
                self.composition_start();
                self.composition.clone().unwrap_or_default()
            }
        };
        let text = normalize(text);
        self.text.replace_range(range.clone(), &text);
        self.composition = Some(range.start..range.start + text.len());
        self.reshape();
        self.set_cursor(cursor_of(&self.text, range.start + text.len()), false);
    }

    /// Composition finished: `commit` keeps the preedit text, otherwise it is
    /// removed (the browser fires `compositionend` with the final text first,
    /// so a committed run needs no further edit).
    pub fn composition_end(&mut self, commit: bool) {
        let Some(range) = self.composition.take() else {
            return;
        };
        if !commit {
            self.replace_range(range, "");
        }
    }

    /// Draw a frame, unless nothing changed since the last one. Returns whether
    /// anything was actually rendered and presented — an idle demo renders
    /// zero frames per second and the canvas still shows the right thing.
    pub fn render(&mut self) -> Result<bool, JsValue> {
        self.render_frame()
    }
}

impl FafTextDemo {
    /// Re-shape the buffer from the backing string. O(n) per edit, which is
    /// fine at demo sizes and keeps the model dead simple.
    fn reshape(&mut self) {
        // Only the variable face has masters to blend, so the weight wave
        // switches the text over to it.
        let family = match self.weight_blend {
            Some(_) => Family::Name(VARIABLE_FAMILY),
            None => Family::SansSerif,
        };
        self.view.set_text(
            &mut self.font_system,
            &self.text,
            &Attrs::new().family(family),
        );
        self.dirty = Dirty::all();
    }

    /// Move the caret; `extend` keeps the selection anchor put (Shift+motion).
    fn set_cursor(&mut self, cursor: Cursor, extend: bool) {
        self.cursor = cursor;
        if !extend {
            self.anchor = cursor;
        }
        self.dirty.cursor();
    }

    fn selection_range(&self) -> Option<Range<usize>> {
        let a = offset_of(&self.text, self.anchor);
        let b = offset_of(&self.text, self.cursor);
        (a != b).then(|| a.min(b)..a.max(b))
    }

    /// Splice the backing string, re-shape, and park the caret after the
    /// inserted text.
    fn replace_range(&mut self, range: Range<usize>, with: &str) {
        if range.start > self.text.len() || range.end > self.text.len() {
            return;
        }
        let end = range.start + with.len();
        self.text.replace_range(range, with);
        self.composition = None;
        self.sticky_x = None;
        self.reshape();
        self.set_cursor(cursor_of(&self.text, end), false);
    }

    /// Rect in block-local pixels: every block is parked at the text origin, so
    /// the margin (and any future scroll) is a uniform, not geometry.
    fn local(&self, r: Rect) -> Rect {
        [r[0] - self.view.pos[0], r[1] - self.view.pos[1], r[2], r[3]]
    }

    fn surface(&self) -> [f32; 2] {
        [self.config.width as f32, self.config.height as f32]
    }

    /// The camera the tilted scene is seen through. Aimed so that a pane left
    /// at z = 0 lands on exactly its 2D pixels, which is why turning the tilt
    /// on does not make the layout jump.
    fn camera(&self) -> Mat4 {
        screen_perspective(self.surface(), FOV_Y)
    }

    /// Where the pane sits in the world, or `None` when it is flat. Every
    /// block shares this one matrix: the caret, the selection underlay and the
    /// search marks are all in the pane's own pixel space, so sharing it is
    /// what keeps them glued to the glyphs at any angle.
    fn pane_model(&self) -> Option<Mat4> {
        let degrees = self.tilt?;
        let margin = MARGIN * self.dpr;
        let surface = self.surface();
        // Turn about the middle of the visible pane, and shrink a little so
        // the near edge does not swing off the canvas.
        let pivot = Vec3::new(
            (surface[0] - 2.0 * margin) * 0.5,
            (surface[1] - 2.0 * margin) * 0.5,
            0.0,
        );
        let origin = Vec3::new(self.view.pos[0], self.view.pos[1], 0.0);
        Some(
            Mat4::from_translation(origin + pivot)
                * Mat4::from_rotation_y(degrees.to_radians())
                * Mat4::from_scale(Vec3::new(TILT_SCALE, TILT_SCALE, 1.0))
                * Mat4::from_translation(-pivot),
        )
    }

    /// Pointer (CSS px, canvas-relative) to a text cursor. Flat, that is the
    /// 2D hit test on a physical pixel; tilted, it is the published recipe —
    /// pixel → NDC → world ray → the pane's own plane — and the *same* 2D hit
    /// test on the block-local pixel that comes back.
    fn hit(&self, x: f32, y: f32) -> Option<Cursor> {
        let px = [x * self.dpr, y * self.dpr];
        let Some(model) = self.pane_model() else {
            return self.view.hit(px[0], px[1]);
        };
        let vp = self.camera();
        let (origin, dir) = ndc_ray(&vp, pointer_ndc(self.surface(), px));
        let local = block_hit(&model, &vp, origin, dir)?;
        self.view
            .hit(local[0] + self.view.pos[0], local[1] + self.view.pos[1])
    }

    /// The other direction: a block-local point to a physical pixel on the
    /// surface. `None` if it is behind the camera.
    fn project(&self, model: &Mat4, local: [f32; 2]) -> Option<[f32; 2]> {
        let clip = self.camera() * *model * Vec4::new(local[0], local[1], 0.0, 1.0);
        if clip.w <= 0.0 {
            return None;
        }
        let surface = self.surface();
        Some([
            (clip.x / clip.w * 0.5 + 0.5) * surface[0],
            (0.5 - clip.y / clip.w * 0.5) * surface[1],
        ])
    }

    /// Push whatever changed since the last frame into the blocks that own it.
    fn sync_blocks(&mut self) {
        let origin = self.view.pos;
        let blocks = [
            self.selection_block,
            self.text_block,
            self.search_block,
            self.caret_block,
        ];
        // Placement is a uniform per block either way — a swaying pane costs
        // four matrix writes a frame and not one instance.
        match self.pane_model() {
            Some(model) => {
                let model = model.to_cols_array_2d();
                self.renderer
                    .set_view_projection(self.camera().to_cols_array_2d());
                for block in blocks {
                    self.renderer.set_block_transform(block, model);
                }
            }
            None => {
                self.renderer.clear_view_projection();
                for block in blocks {
                    self.renderer.set_block_offset(block, origin);
                }
            }
        }

        if std::mem::take(&mut self.dirty.text) {
            self.renderer.set_block_content(
                &self.queue,
                &mut self.font_system,
                self.text_block,
                &BlockContent {
                    buffer: Some(&self.view.buffer),
                    pos: [0.0, 0.0],
                    default_color: FOREGROUND,
                    weight: self.weight_blend,
                    ..Default::default()
                },
            );
        }

        if std::mem::take(&mut self.dirty.selection) {
            let rects: Vec<(Rect, Color)> = match self.selection_range() {
                Some(_) => self
                    .view
                    .selection_rects(self.anchor, self.cursor)
                    .into_iter()
                    .map(|r| (self.local(r), SELECTION))
                    .collect(),
                None => Vec::new(),
            };
            self.renderer.set_block_content(
                &self.queue,
                &mut self.font_system,
                self.selection_block,
                &BlockContent {
                    under_rects: &rects,
                    ..Default::default()
                },
            );
        }

        if std::mem::take(&mut self.dirty.search) {
            // One cursor range per match, drawn as whichever primitive the
            // mode picked: a rect over the glyphs, a line decoration, or a
            // chip. Every mode is the same geometry — only the layer and the
            // shape differ.
            let radius = CHIP_RADIUS * self.dpr;
            let kind = self.search_mode.kind(radius);
            let mut rects: Vec<(Rect, Color)> = Vec::new();
            let mut chips: Vec<(Rect, f32, Color)> = Vec::new();
            let mut decorations: Vec<(Rect, DecorationKind, Color)> = Vec::new();
            if !self.search.is_empty() {
                for (a, b) in self.view.find_all(&self.search) {
                    match kind {
                        None => rects.extend(
                            self.view
                                .selection_rects(a, b)
                                .into_iter()
                                .map(|r| (self.local(r), HIGHLIGHT)),
                        ),
                        Some(DecorationKind::Chip { .. }) => chips.extend(
                            self.view
                                .decoration_rects(a, b, DecorationKind::Chip { radius_px: radius })
                                .into_iter()
                                .map(|r| (self.local(r), radius, HIGHLIGHT)),
                        ),
                        Some(kind) => decorations.extend(
                            self.view
                                .decoration_rects(a, b, kind)
                                .into_iter()
                                .map(|r| (self.local(r), kind, MARK)),
                        ),
                    }
                }
            }
            self.renderer.set_block_content(
                &self.queue,
                &mut self.font_system,
                self.search_block,
                &BlockContent {
                    over_rects: &rects,
                    chips: &chips,
                    decorations: &decorations,
                    ..Default::default()
                },
            );
        }

        if std::mem::take(&mut self.dirty.caret) {
            let mut rects: Vec<(Rect, Color)> = Vec::new();
            let mut decorations: Vec<(Rect, DecorationKind, Color)> = Vec::new();
            // IME preedit: underline the composing run so it reads as
            // provisional. A real underline decoration, so it sits where the
            // face wants it rather than at the bottom of the line box.
            if let Some(range) = self.composition.clone() {
                let start = cursor_of(&self.text, range.start);
                let end = cursor_of(&self.text, range.end);
                for r in self
                    .view
                    .decoration_rects(start, end, DecorationKind::Underline)
                {
                    decorations.push((self.local(r), DecorationKind::Underline, FOREGROUND));
                }
            }
            if let Some(r) = self
                .view
                .cursor_rect(self.cursor)
                .filter(|_| self.caret_visible)
            {
                let r = self.local(r);
                rects.push(([r[0], r[1], CARET_WIDTH, r[3]], CARET));
            }
            self.renderer.set_block_content(
                &self.queue,
                &mut self.font_system,
                self.caret_block,
                &BlockContent {
                    over_rects: &rects,
                    decorations: &decorations,
                    ..Default::default()
                },
            );
        }
    }

    fn render_frame(&mut self) -> Result<bool, JsValue> {
        self.renderer.begin_frame();
        self.sync_blocks();
        // Nothing changed: what is on the canvas is still right, so there is
        // no pass to record and no frame to present.
        if !self.renderer.damaged() {
            return Ok(false);
        }

        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            // Transient states: skip this frame and let rAF try again.
            _ => return Ok(false),
        };
        let target = frame.texture.create_view(&Default::default());

        self.renderer.finish(
            &self.device,
            &self.queue,
            [self.config.width as f32, self.config.height as f32],
        );

        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("faf-text-web pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.084,
                            g: 0.088,
                            b: 0.118,
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
            self.renderer.render(&mut pass);
        }
        self.queue.submit([encoder.finish()]);
        self.queue.present(frame);
        Ok(true)
    }
}
