//! wasm-bindgen glue: binds the faf-text renderer to an HTML canvas with
//! pointer-driven selection and search highlighting.

use faf_text::cosmic_text::{Attrs, Cursor, Family, Metrics};
use faf_text::{Color, FontSystem, RectLayer, TextRenderer, TextView};
use wasm_bindgen::prelude::*;
use web_sys::HtmlCanvasElement;

const SELECTION: Color = Color::rgba(0.23, 0.39, 0.66, 1.0);
const HIGHLIGHT: Color = Color::rgba(0.88, 0.69, 0.41, 0.38);
const FOREGROUND: Color = Color::rgba(0.75, 0.79, 0.96, 1.0);
const MARGIN: f32 = 24.0;

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
    selection: Option<(Cursor, Cursor)>,
    dragging: bool,
    search: String,
}

#[wasm_bindgen]
impl FafTextDemo {
    /// Create the renderer on a canvas. The canvas backing size should
    /// already be set (CSS size × devicePixelRatio).
    pub async fn attach(canvas: HtmlCanvasElement, dpr: f32) -> Result<FafTextDemo, JsValue> {
        console_error_panic_hook::set_once();

        let width = canvas.width().max(1);
        let height = canvas.height().max(1);

        // Try WebGPU first, then a WebGL2-only instance — some environments
        // (older browsers, headless) expose navigator.gpu but never deliver
        // an adapter.
        let mut picked = None;
        for backends in [wgpu::Backends::BROWSER_WEBGPU, wgpu::Backends::GL] {
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
            selection: None,
            dragging: false,
            search: String::new(),
        })
    }

    pub fn backend(&self) -> String {
        self.backend.clone()
    }

    pub fn set_text(&mut self, text: &str) {
        self.view.set_text(
            &mut self.font_system,
            text,
            &Attrs::new().family(Family::SansSerif),
        );
        self.selection = None;
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
            self.selection = Some((cursor, cursor));
            self.dragging = true;
        }
    }

    pub fn pointer_move(&mut self, x: f32, y: f32) {
        if !self.dragging {
            return;
        }
        if let (Some((anchor, _)), Some(cursor)) =
            (self.selection, self.view.hit(x * self.dpr, y * self.dpr))
        {
            self.selection = Some((anchor, cursor));
        }
    }

    pub fn pointer_up(&mut self) {
        self.dragging = false;
    }

    /// The currently selected text (for clipboard integration in JS).
    pub fn selected_text(&self) -> String {
        let Some((a, b)) = self.selection else {
            return String::new();
        };
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        let mut out = String::new();
        for (i, line) in self.view.buffer.lines.iter().enumerate() {
            if i < start.line || i > end.line {
                continue;
            }
            let text = line.text();
            let from = if i == start.line { start.index } else { 0 };
            let to = if i == end.line { end.index } else { text.len() };
            if i > start.line {
                out.push('\n');
            }
            out.push_str(&text[from.min(text.len())..to.min(text.len())]);
        }
        out
    }

    pub fn render(&mut self) -> Result<(), JsValue> {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            // Transient states: skip this frame and let rAF try again.
            _ => return Ok(()),
        };
        let target = frame.texture.create_view(&Default::default());

        self.renderer.begin();

        if let Some((a, b)) = self.selection {
            for r in self.view.selection_rects(a, b) {
                self.renderer
                    .rect([r[0], r[1]], [r[2], r[3]], SELECTION, RectLayer::Under);
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
