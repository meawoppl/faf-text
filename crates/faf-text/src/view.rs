use cosmic_text::{Affinity, Attrs, Buffer, Cursor, FontSystem, LayoutRun, Metrics, Shaping};
use unicode_segmentation::UnicodeSegmentation;

/// A positioned rectangle in screen pixels: [x, y, width, height].
pub type Rect = [f32; 4];

/// Where a cursor lands on a laid-out visual row.
#[derive(Clone, Copy, Debug)]
struct RowHit {
    /// Index of the visual row in the buffer's layout-run order.
    row: usize,
    /// First byte index covered by the row (in its original line).
    start: usize,
    /// Last byte index covered by the row.
    end: usize,
    /// Caret x within the buffer (buffer-relative, not screen).
    x: f32,
    top: f32,
    height: f32,
}

/// Byte span the run's glyphs cover in the original line. `None` for runs
/// without glyphs (a blank line, for instance).
fn run_span(run: &LayoutRun) -> Option<(usize, usize)> {
    let mut glyphs = run.glyphs.iter();
    let first = glyphs.next()?;
    let (mut lo, mut hi) = (first.start, first.end);
    for glyph in glyphs {
        lo = lo.min(glyph.start);
        hi = hi.max(glyph.end);
    }
    Some((lo, hi))
}

/// Caret x for a byte index inside a visual row. Positions inside a glyph
/// cluster (a ligature covering several graphemes) are interpolated by
/// grapheme, matching how `Buffer::hit` splits clusters.
fn run_caret_x(run: &LayoutRun, index: usize) -> f32 {
    for glyph in run.glyphs {
        if index < glyph.start || index > glyph.end {
            continue;
        }
        let cluster = &run.text[glyph.start..glyph.end];
        let offset = index - glyph.start;
        let total = cluster.graphemes(true).count();
        let fraction = if total == 0 || !cluster.is_char_boundary(offset) {
            0.0
        } else {
            cluster[..offset].graphemes(true).count() as f32 / total as f32
        };
        return if glyph.level.is_rtl() {
            glyph.x + glyph.w * (1.0 - fraction)
        } else {
            glyph.x + glyph.w * fraction
        };
    }
    // Past every glyph on the row: the trailing edge.
    if run.rtl { 0.0 } else { run.line_w }
}

/// True when a piece of text carries no word content (pure whitespace).
fn is_blank(s: &str) -> bool {
    s.chars().all(char::is_whitespace)
}

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

    /// The visual row a cursor sits on, plus its caret x on that row.
    ///
    /// At a soft-wrap boundary the same byte index belongs to two rows; the
    /// cursor's affinity decides: [`Affinity::Before`] keeps the caret on the
    /// row before the break, [`Affinity::After`] moves it to the row after.
    fn row_for_cursor(&self, c: Cursor) -> Option<RowHit> {
        let mut hit: Option<RowHit> = None;
        let mut fallback: Option<RowHit> = None;
        for (row, run) in self.buffer.layout_runs().enumerate() {
            if run.line_i != c.line {
                // Rows of one line are contiguous, so we are past it now.
                if hit.is_some() || fallback.is_some() {
                    break;
                }
                continue;
            }
            let (start, end) = run_span(&run).unwrap_or((0, 0));
            let this = RowHit {
                row,
                start,
                end,
                x: run_caret_x(&run, c.index),
                top: run.line_top,
                height: run.line_height,
            };
            if c.index >= start && c.index <= end {
                match hit {
                    None => hit = Some(this),
                    // Second row containing the index: a wrap boundary.
                    Some(_) => {
                        if c.affinity == Affinity::After {
                            hit = Some(this);
                        }
                        break;
                    }
                }
            } else {
                fallback = Some(this);
            }
        }
        hit.or(fallback)
    }

    /// Caret geometry in screen pixels for a cursor: `[x, y, 1.0, height]`.
    ///
    /// The width is a nominal 1 px — callers overwrite it with the caret
    /// thickness they want (2 physical px is a good default). Returns `None`
    /// when the cursor's line has no laid-out rows.
    pub fn cursor_rect(&self, c: Cursor) -> Option<Rect> {
        let hit = self.row_for_cursor(c)?;
        Some([self.pos[0] + hit.x, self.pos[1] + hit.top, 1.0, hit.height])
    }

    /// One grapheme cluster left, crossing into the previous line at a line
    /// start. Returns `c` unchanged at the very beginning of the text.
    pub fn move_left(&self, c: Cursor) -> Cursor {
        let Some(line) = self.buffer.lines.get(c.line) else {
            return c;
        };
        let text = line.text();
        if c.index > 0 {
            let index = text
                .grapheme_indices(true)
                .map(|(i, _)| i)
                .take_while(|&i| i < c.index)
                .last()
                .unwrap_or(0);
            Cursor::new_with_affinity(c.line, index, Affinity::After)
        } else if c.line > 0 {
            let end = self.buffer.lines[c.line - 1].text().len();
            Cursor::new_with_affinity(c.line - 1, end, Affinity::After)
        } else {
            c
        }
    }

    /// One grapheme cluster right, crossing into the next line at a line end.
    /// Returns `c` unchanged at the very end of the text.
    pub fn move_right(&self, c: Cursor) -> Cursor {
        let Some(line) = self.buffer.lines.get(c.line) else {
            return c;
        };
        let text = line.text();
        if c.index < text.len() {
            if !text.is_char_boundary(c.index) {
                return c;
            }
            let index = text[c.index..]
                .graphemes(true)
                .next()
                .map_or(text.len(), |g| c.index + g.len());
            Cursor::new_with_affinity(c.line, index, Affinity::Before)
        } else if c.line + 1 < self.buffer.lines.len() {
            Cursor::new_with_affinity(c.line + 1, 0, Affinity::Before)
        } else {
            c
        }
    }

    /// To the start of the word left of the cursor (UAX #29 word bounds,
    /// skipping whitespace), crossing into the previous line if needed.
    pub fn move_word_left(&self, c: Cursor) -> Cursor {
        let Some(line) = self.buffer.lines.get(c.line) else {
            return c;
        };
        let text = line.text();
        let target = text
            .split_word_bound_indices()
            .filter(|&(i, w)| i < c.index && !is_blank(w))
            .map(|(i, _)| i)
            .next_back();
        match target {
            Some(index) => Cursor::new_with_affinity(c.line, index, Affinity::After),
            None if c.index > 0 => Cursor::new_with_affinity(c.line, 0, Affinity::After),
            None if c.line > 0 => {
                let end = self.buffer.lines[c.line - 1].text().len();
                Cursor::new_with_affinity(c.line - 1, end, Affinity::After)
            }
            None => c,
        }
    }

    /// To the end of the word right of the cursor, crossing into the next
    /// line if needed.
    pub fn move_word_right(&self, c: Cursor) -> Cursor {
        let Some(line) = self.buffer.lines.get(c.line) else {
            return c;
        };
        let text = line.text();
        let target = text
            .split_word_bound_indices()
            .filter(|&(_, w)| !is_blank(w))
            .map(|(i, w)| i + w.len())
            .find(|&end| end > c.index);
        match target {
            Some(index) => Cursor::new_with_affinity(c.line, index, Affinity::Before),
            None if c.index < text.len() => {
                Cursor::new_with_affinity(c.line, text.len(), Affinity::Before)
            }
            None if c.line + 1 < self.buffer.lines.len() => {
                Cursor::new_with_affinity(c.line + 1, 0, Affinity::Before)
            }
            None => c,
        }
    }

    /// One visual row up, landing at `sticky_x` (a screen-pixel x, usually
    /// remembered from where vertical motion started). Unchanged on the top
    /// row.
    pub fn move_up(&self, c: Cursor, sticky_x: f32) -> Cursor {
        self.move_rows(c, sticky_x, -1)
    }

    /// One visual row down, landing at `sticky_x`. Unchanged on the last row.
    pub fn move_down(&self, c: Cursor, sticky_x: f32) -> Cursor {
        self.move_rows(c, sticky_x, 1)
    }

    fn move_rows(&self, c: Cursor, sticky_x: f32, delta: i32) -> Cursor {
        let Some(hit) = self.row_for_cursor(c) else {
            return c;
        };
        let target = if delta < 0 {
            match hit.row.checked_sub(1) {
                Some(row) => row,
                None => return c,
            }
        } else {
            hit.row + 1
        };
        let Some((top, height)) = self
            .buffer
            .layout_runs()
            .nth(target)
            .map(|run| (run.line_top, run.line_height))
        else {
            return c;
        };
        self.hit(sticky_x, self.pos[1] + top + height * 0.5)
            .unwrap_or(c)
    }

    /// Start of the cursor's visual row (the row start, not the line start,
    /// on soft-wrapped text).
    pub fn line_start(&self, c: Cursor) -> Cursor {
        let index = self.row_for_cursor(c).map_or(0, |hit| hit.start);
        Cursor::new_with_affinity(c.line, index, Affinity::After)
    }

    /// End of the cursor's visual row.
    pub fn line_end(&self, c: Cursor) -> Cursor {
        let fallback = || self.buffer.lines.get(c.line).map_or(0, |l| l.text().len());
        let index = self.row_for_cursor(c).map_or_else(fallback, |hit| hit.end);
        Cursor::new_with_affinity(c.line, index, Affinity::Before)
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
