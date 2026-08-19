//! A frame-rate readout the renderer draws itself.
//!
//! The overlay is not a DOM element parked over the canvas: it is one more
//! retained block, a mono-font readout on a rounded chip, anchored to the
//! top-right corner of the surface. It reads
//! `webgl2 · 62 fps · 16.1 ms` — the backend the surface actually got, so a
//! live cell says whether it is on WebGPU or the WebGL2 fallback, then the
//! rate. That makes it work identically in the demo
//! page, in a docs.rs live cell and (were the pane in 3D) in any host that has
//! nowhere to put an HTML label — and it means the thing measuring the
//! renderer is drawn by the renderer.
//!
//! # What it measures, and the self-damage rule
//!
//! It counts **presented frames**, not `requestAnimationFrame` callbacks: the
//! demo skips both the render pass and the present when the scene graph
//! reports no damage, so a callback that drew nothing is not a frame.
//!
//! That immediately raises a circularity, because the overlay is itself scene
//! content: refreshing the readout dirties a block, which is damage, which
//! presents a frame, which would be another sample, which would keep the
//! number alive forever — an idle demo would read a steady 4 fps (the readout's
//! own refresh rate) instead of `idle`.
//!
//! The rule that breaks it, and the one [`FafTextDemo::render`] implements:
//!
//! > A timestamp is recorded only for frames where something **other than the
//! > stats block** was damaged.
//!
//! Mechanically that is an ordering: the host syncs its own blocks, reads
//! [`TextRenderer::damaged`] — at that point the flag can only be about
//! *other* content, because the overlay has not been touched yet — and passes
//! the answer to [`StatsOverlay::record`]. Only then does the overlay get its
//! chance to change its text and dirty its own block. So the overlay's 4 Hz
//! updates do present frames, and honestly do not count themselves.
//!
//! With that in place, an idle demo settles: the last real frame ages past
//! [`GAP_MS`], the readout flips to `webgl2 · idle` (one final text change, one final
//! present), and after that the label stops changing, nothing is dirty, and
//! the demo goes back to presenting exactly nothing.
//!
//! The refresh is also the damage-tracking showcase: changing the readout
//! re-uploads this block's dozen-odd instances and *nothing else* — the text
//! block's several thousand glyphs, or the terminal grid's several thousand
//! cells, are not touched by a number ticking over four times a second.
//!
//! [`FafTextDemo::render`]: crate::FafTextDemo::render

use std::collections::VecDeque;

use faf_text::cosmic_text::{Attrs, Family, Metrics};
use faf_text::{BlockContent, BlockId, Color, FontSystem, Rect, TextRenderer, TextView};
use web_sys::Performance;

/// Timestamps older than this are dropped, so the reading follows the last
/// second and a half rather than averaging over a whole session.
const WINDOW_MS: f64 = 1500.0;
/// A gap this long means the cell was paused (the docs loader stops ticking
/// cells scrolled out of view) or the scene simply went idle. The buffer is
/// cleared rather than averaged across the hole.
const GAP_MS: f64 = 600.0;
/// How often the *displayed* text may change. Faster than this and the number
/// is unreadable; it also bounds how often the overlay can damage its block.
const UPDATE_MS: f64 = 250.0;
/// Samples needed before a rate is shown at all. Below this the buffer has not
/// seen enough of the recent past to divide by.
const MIN_SAMPLES: usize = 5;
/// Readout size and inset from the canvas corner, CSS px (scaled by dpr).
const FONT_SIZE: f32 = 11.0;
const MARGIN: f32 = 8.0;
const PAD_X: f32 = 7.0;
const PAD_Y: f32 = 3.0;
/// Corner radius of the chip behind the readout, CSS px.
const RADIUS: f32 = 5.0;

/// Chip and text colors, in the demo's palette.
const CHIP: Color = Color::rgba(0.12, 0.13, 0.19, 0.86);
const TEXT: Color = Color::rgba(0.62, 0.81, 0.42, 1.0);

/// What the rate half of the readout says when there is nothing to measure.
const IDLE: &str = "idle";

/// Short name for the backend the surface actually got, so every live cell
/// self-reports WebGPU vs WebGL2. wgpu's debug names are the input.
fn backend_tag(backend: &str) -> String {
    match backend {
        "BrowserWebGpu" => "webgpu".to_string(),
        "Gl" => "webgl2".to_string(),
        other => other.to_lowercase(),
    }
}

/// The overlay: one block, one shaped line, and a ring of presentation
/// timestamps. See the module docs for the measurement rule.
pub(crate) struct StatsOverlay {
    block: BlockId,
    view: TextView,
    /// `performance.now()`, or `None` in an exotic host with no `Performance`
    /// — in which case every reading is `idle`, which is at least honest.
    clock: Option<Performance>,
    /// Presentation timestamps, ms, oldest first.
    samples: VecDeque<f64>,
    /// `webgpu` / `webgl2`, prefixed to every reading.
    backend: String,
    /// The text currently uploaded, so an unchanged number uploads nothing.
    label: String,
    /// When the label was last *considered* for a change.
    updated_at: f64,
    dpr: f32,
    /// Size of the chip in physical px, which is also what the top-right
    /// anchor subtracts from the surface width.
    chip: [f32; 2],
}

impl StatsOverlay {
    /// Create the overlay's block. Blocks composite in creation order, so this
    /// must be built *after* every block it should draw over.
    pub(crate) fn new(
        renderer: &mut TextRenderer,
        font_system: &mut FontSystem,
        dpr: f32,
        backend: &str,
    ) -> Self {
        let block = renderer.create_block();
        let px = Self::px(dpr);
        let view = TextView::new(font_system, Metrics::new(px, (px * 1.35).ceil()));
        StatsOverlay {
            block,
            view,
            clock: web_sys::window().and_then(|w| w.performance()),
            samples: VecDeque::new(),
            backend: backend_tag(backend),
            label: String::new(),
            updated_at: 0.0,
            dpr,
            chip: [0.0, 0.0],
        }
    }

    /// Show or hide the readout. Hidden blocks keep their instances, so
    /// toggling back on costs one flag.
    pub(crate) fn set_visible(&mut self, renderer: &mut TextRenderer, visible: bool) {
        renderer.set_block_visible(self.block, visible);
        if !visible {
            // Nothing is being presented for the overlay to measure while it
            // is off; start clean when it comes back rather than averaging
            // across the gap.
            self.samples.clear();
        }
    }

    /// Note that a frame is being presented for reasons of its own.
    ///
    /// `external_damage` is the caller's answer to "was anything but the stats
    /// block dirty this frame" — see the module docs. False means this frame
    /// exists only because the readout changed, and counting it would make the
    /// overlay measure itself.
    pub(crate) fn record(&mut self, external_damage: bool) {
        if !external_damage {
            return;
        }
        let now = self.now();
        if self.samples.back().is_some_and(|&last| now - last > GAP_MS) {
            self.samples.clear();
        }
        self.samples.push_back(now);
        // Trim to the window, but never below the minimum: a demo genuinely
        // running at 3 fps should read 3 fps, not `idle`. The gap rule already
        // bounds how stale a retained sample can be.
        while self.samples.len() > MIN_SAMPLES
            && self.samples.front().is_some_and(|&t| now - t > WINDOW_MS)
        {
            self.samples.pop_front();
        }
    }

    /// Recompute the readout (at most every [`UPDATE_MS`]) and keep it anchored
    /// to the top-right corner. Both halves are no-ops when nothing changed, so
    /// a steady reading damages nothing.
    pub(crate) fn refresh(
        &mut self,
        renderer: &mut TextRenderer,
        queue: &wgpu::Queue,
        font_system: &mut FontSystem,
        surface: [f32; 2],
        dpr: f32,
    ) {
        if dpr != self.dpr {
            self.dpr = dpr;
            let px = Self::px(dpr);
            self.view
                .set_metrics(font_system, Metrics::new(px, (px * 1.35).ceil()));
            self.rebuild(renderer, queue, font_system);
        }

        let now = self.now();
        if now - self.updated_at >= UPDATE_MS {
            self.updated_at = now;
            let label = self.reading(now);
            if label != self.label {
                self.label = label;
                self.rebuild(renderer, queue, font_system);
            }
        }

        // Anchor. `set_block_offset` early-outs on an unchanged matrix, so this
        // is free every frame the canvas has not resized.
        let x = (surface[0] - MARGIN * dpr - self.chip[0]).max(0.0);
        renderer.set_block_offset(self.block, [x, MARGIN * dpr]);
    }

    /// The readout for `now` — the backend tag plus a rate — and the gap rule
    /// that decides when there is nothing to read.
    fn reading(&mut self, now: f64) -> String {
        let rate = self.rate(now);
        format!("{} · {rate}", self.backend)
    }

    /// The rate half of the readout, or [`IDLE`].
    fn rate(&mut self, now: f64) -> String {
        if self.samples.back().is_some_and(|&last| now - last > GAP_MS) {
            self.samples.clear();
        }
        if self.samples.len() < MIN_SAMPLES {
            return IDLE.to_string();
        }
        let (Some(&first), Some(&last)) = (self.samples.front(), self.samples.back()) else {
            return IDLE.to_string();
        };
        let span = last - first;
        if span <= 0.0 {
            return IDLE.to_string();
        }
        // n intervals over n + 1 samples: the rate and the mean frame time are
        // the same measurement, reported both ways.
        let intervals = (self.samples.len() - 1) as f64;
        let fps = intervals * 1000.0 / span;
        let frame_ms = span / intervals;
        format!("{fps:.0} fps · {frame_ms:.1} ms")
    }

    /// Re-shape the readout and re-upload *this block only*.
    fn rebuild(
        &mut self,
        renderer: &mut TextRenderer,
        queue: &wgpu::Queue,
        font_system: &mut FontSystem,
    ) {
        let text = if self.label.is_empty() {
            format!("{} · {IDLE}", self.backend)
        } else {
            self.label.clone()
        };
        self.view.set_text(
            font_system,
            &text,
            &Attrs::new().family(Family::Name(crate::MONO_FAMILY)),
        );

        let text_w = self
            .view
            .buffer
            .layout_runs()
            .map(|run| run.line_w)
            .fold(0.0f32, f32::max);
        let line_h = self.view.buffer.metrics().line_height;
        let pad = [PAD_X * self.dpr, PAD_Y * self.dpr];
        self.chip = [
            (text_w + 2.0 * pad[0]).ceil(),
            (line_h + 2.0 * pad[1]).ceil(),
        ];
        let chip: Rect = [0.0, 0.0, self.chip[0], self.chip[1]];

        // The damage-tracking showcase: the number ticking over re-uploads this
        // block's handful of instances and touches no other block — not the
        // text block's thousands of glyphs, not the terminal grid's cells.
        renderer.set_block_content(
            queue,
            font_system,
            self.block,
            &BlockContent {
                buffer: Some(&self.view.buffer),
                pos: pad,
                default_color: TEXT,
                chips: &[(chip, RADIUS * self.dpr, CHIP)],
                ..Default::default()
            },
        );
    }

    fn now(&self) -> f64 {
        self.clock.as_ref().map_or(0.0, |clock| clock.now())
    }

    /// Readout em size in physical px.
    fn px(dpr: f32) -> f32 {
        (FONT_SIZE * dpr).max(6.0)
    }
}
