use cosmic_text::{Attrs, Buffer, Cursor, FontSystem, Metrics, Shaping};

/// A positioned rectangle in screen pixels: [x, y, width, height].
pub type Rect = [f32; 4];

/// A shaped text block positioned on screen, with hit-testing and
/// selection-geometry helpers layered on top of a cosmic-text [`Buffer`].
pub struct TextView {
    pub buffer: Buffer,
    /// Top-left corner in screen pixels.
    pub pos: [f32; 2],
}

impl TextView {
    pub fn new(font_system: &mut FontSystem, metrics: Metrics) -> Self {
        Self {
            buffer: Buffer::new(font_system, metrics),
            pos: [0.0, 0.0],
        }
    }

    pub fn set_text(&mut self, font_system: &mut FontSystem, text: &str, attrs: &Attrs) {
        self.buffer.set_text(text, attrs, Shaping::Advanced, None);
        self.buffer.shape_until_scroll(font_system, false);
    }

    /// Constrain layout width/height (px) and re-shape.
    pub fn set_size(
        &mut self,
        font_system: &mut FontSystem,
        width: Option<f32>,
        height: Option<f32>,
    ) {
        self.buffer.set_size(width, height);
        self.buffer.shape_until_scroll(font_system, false);
    }

    pub fn set_metrics(&mut self, font_system: &mut FontSystem, metrics: Metrics) {
        self.buffer.set_metrics(metrics);
        self.buffer.shape_until_scroll(font_system, false);
    }

    /// Map a screen-pixel position to the closest text cursor.
    pub fn hit(&self, x: f32, y: f32) -> Option<Cursor> {
        self.buffer.hit(x - self.pos[0], y - self.pos[1])
    }

    /// Screen-pixel rectangles covering the text between two cursors
    /// (any order). One rect per line, BiDi-aware within a line.
    pub fn selection_rects(&self, a: Cursor, b: Cursor) -> Vec<Rect> {
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        let mut rects = Vec::new();
        for run in self.buffer.layout_runs() {
            // cosmic-text's highlight() assumes the run's line intersects the
            // cursor range; out-of-range lines would come back fully selected.
            if run.line_i < start.line || run.line_i > end.line {
                continue;
            }
            for (x, width) in run.highlight(start, end) {
                rects.push([
                    self.pos[0] + x,
                    self.pos[1] + run.line_top,
                    width,
                    run.line_height,
                ]);
            }
        }
        rects
    }

    /// Cursor pairs for every occurrence of `needle` (case-sensitive).
    /// Feed the results through [`Self::selection_rects`] for highlights.
    pub fn find_all(&self, needle: &str) -> Vec<(Cursor, Cursor)> {
        let mut out = Vec::new();
        if needle.is_empty() {
            return out;
        }
        for (line_i, line) in self.buffer.lines.iter().enumerate() {
            for (byte_i, matched) in line.text().match_indices(needle) {
                out.push((
                    Cursor::new(line_i, byte_i),
                    Cursor::new(line_i, byte_i + matched.len()),
                ));
            }
        }
        out
    }
}
