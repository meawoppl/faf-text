//! wasm-bindgen glue: binds the faf-text renderer to an HTML canvas with
//! pointer-driven selection, keyboard editing, IME composition and search
//! highlighting.

use std::ops::Range;

use faf_text::cosmic_text::{Attrs, Cursor, Family, Metrics};
use faf_text::{Color, FontSystem, RectLayer, TextRenderer, TextView};
use wasm_bindgen::prelude::*;
use web_sys::HtmlCanvasElement;

const SELECTION: Color = Color::rgba(0.23, 0.39, 0.66, 1.0);
const HIGHLIGHT: Color = Color::rgba(0.88, 0.69, 0.41, 0.38);
const FOREGROUND: Color = Color::rgba(0.75, 0.79, 0.96, 1.0);
const CARET: Color = Color::rgba(1.0, 0.62, 0.39, 1.0);
const MARGIN: f32 = 24.0;
/// Caret thickness and composition underline thickness, in physical pixels.
const CARET_WIDTH: f32 = 2.0;

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

        let renderer = TextRenderer::new(&device, format);
        let mut font_system = faf_text::font_system_from_fonts(&[
            faf_text::FONT_DEJAVU_SANS,
            faf_text::FONT_DEJAVU_SANS_MONO,
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
        self.font_size = size;
        let px = size * self.dpr;
        self.view
            .set_metrics(&mut self.font_system, Metrics::new(px, px * 1.5));
    }

    pub fn set_search(&mut self, needle: &str) {
        self.search = needle.to_string();
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
    }

    /// Pointer coordinates are CSS px relative to the canvas.
    pub fn pointer_down(&mut self, x: f32, y: f32) {
        if let Some(cursor) = self.view.hit(x * self.dpr, y * self.dpr) {
            self.set_cursor(cursor, false);
            self.dragging = true;
        }
    }

    pub fn pointer_move(&mut self, x: f32, y: f32) {
        if !self.dragging {
            return;
        }
        if let Some(cursor) = self.view.hit(x * self.dpr, y * self.dpr) {
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
    pub fn caret_css_rect(&self) -> Box<[f32]> {
        let rect = self
            .view
            .cursor_rect(self.cursor)
            .map_or([0.0, 0.0, 0.0, 0.0], |r| [r[0], r[1], CARET_WIDTH, r[3]]);
        rect.map(|v| v / self.dpr).into()
    }

    /// Caret blink is host-side: JS toggles this on a timer and resets it on
    /// every edit or motion.
    pub fn set_caret_visible(&mut self, visible: bool) {
        self.caret_visible = visible;
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

    pub fn render(&mut self) -> Result<(), JsValue> {
        self.render_frame()
    }
}

impl FafTextDemo {
    /// Re-shape the buffer from the backing string. O(n) per edit, which is
    /// fine at demo sizes and keeps the model dead simple.
    fn reshape(&mut self) {
        self.view.set_text(
            &mut self.font_system,
            &self.text,
            &Attrs::new().family(Family::SansSerif),
        );
    }

    /// Move the caret; `extend` keeps the selection anchor put (Shift+motion).
    fn set_cursor(&mut self, cursor: Cursor, extend: bool) {
        self.cursor = cursor;
        if !extend {
            self.anchor = cursor;
        }
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

    fn render_frame(&mut self) -> Result<(), JsValue> {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            // Transient states: skip this frame and let rAF try again.
            _ => return Ok(()),
        };
        let target = frame.texture.create_view(&Default::default());

        self.renderer.begin();

        if self.selection_range().is_some() {
            for r in self.view.selection_rects(self.anchor, self.cursor) {
                self.renderer
                    .rect([r[0], r[1]], [r[2], r[3]], SELECTION, RectLayer::Under);
            }
        }
        // IME preedit: underline the composing run so it reads as provisional.
        if let Some(range) = self.composition.clone() {
            let start = cursor_of(&self.text, range.start);
            let end = cursor_of(&self.text, range.end);
            for r in self.view.selection_rects(start, end) {
                self.renderer.rect(
                    [r[0], r[1] + r[3] - CARET_WIDTH],
                    [r[2], CARET_WIDTH],
                    FOREGROUND,
                    RectLayer::Over,
                );
            }
        }
        if !self.search.is_empty() {
            for (a, b) in self.view.find_all(&self.search) {
                for r in self.view.selection_rects(a, b) {
                    self.renderer
                        .rect([r[0], r[1]], [r[2], r[3]], HIGHLIGHT, RectLayer::Over);
                }
            }
        }
        if let Some(r) = self
            .view
            .cursor_rect(self.cursor)
            .filter(|_| self.caret_visible)
        {
            self.renderer
                .rect([r[0], r[1]], [CARET_WIDTH, r[3]], CARET, RectLayer::Over);
        }
        self.renderer.text(
            &self.queue,
            &mut self.font_system,
            &self.view.buffer,
            self.view.pos,
            FOREGROUND,
        );
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
        Ok(())
    }
}
