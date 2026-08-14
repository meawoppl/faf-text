use cosmic_text::{
    Affinity, Attrs, Buffer, Cursor, FontSystem, LayoutRun, Metrics, Shaping, fontdb,
};
use rustc_hash::FxHashMap;
use unicode_segmentation::UnicodeSegmentation;

use crate::renderer::DecorationKind;

/// A positioned rectangle in screen pixels: [x, y, width, height].
pub type Rect = [f32; 4];

/// Where a face wants its decoration lines, in em units — multiply by a run's
/// font size for pixels.
///
/// Offsets are y-up from the baseline and mark the **top** of the stroke,
/// which is swash's convention (`Metrics::underline_offset`,
/// `strikeout_offset`, `stroke_size`): an underline offset is negative
/// (below the baseline), a strikeout offset positive.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LineMetrics {
    /// Top of an underline stroke, in em, y-up from the baseline (negative).
    pub underline_offset: f32,
    /// Top of a strikeout stroke, in em, y-up from the baseline (positive).
    pub strikeout_offset: f32,
    /// Stroke thickness in em, shared by both.
    pub thickness: f32,
}

impl LineMetrics {
    /// What a face that declares no decoration metrics gets: an underline a
    /// tenth of an em below the baseline, a strikeout a little above the
    /// x-height's middle, both 0.06 em thick.
    pub const FALLBACK: LineMetrics = LineMetrics {
        underline_offset: -0.1,
        strikeout_offset: 0.28,
        thickness: 0.06,
    };

    /// Read a face's metrics, in em. swash reports them in design units, so
    /// everything is divided by the head table's units per em; a field the
    /// face leaves at zero falls back rather than collapsing the decoration.
    fn of_font(font: &cosmic_text::Font) -> Self {
        let metrics = font.as_swash().metrics(&[]);
        let upem = f32::from(metrics.units_per_em);
        if upem <= 0.0 {
            return Self::FALLBACK;
        }
        let em = |value: f32, fallback: f32| {
            let value = value / upem;
            if value == 0.0 { fallback } else { value }
        };
        Self {
            underline_offset: em(metrics.underline_offset, Self::FALLBACK.underline_offset),
            strikeout_offset: em(metrics.strikeout_offset, Self::FALLBACK.strikeout_offset),
            thickness: em(metrics.stroke_size, Self::FALLBACK.thickness),
        }
    }
}

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

/// The glyph a decoration over `[x, x + width)` should take its metrics from:
/// the largest one the span covers, which is the one a reader's eye follows.
fn dominant_glyph<'a>(
    run: &LayoutRun<'a>,
    x: f32,
    width: f32,
) -> Option<&'a cosmic_text::LayoutGlyph> {
    run.glyphs
        .iter()
        .filter(|g| g.x < x + width && g.x + g.w > x)
        .max_by(|a, b| a.font_size.total_cmp(&b.font_size))
        .or_else(|| run.glyphs.first())
}

/// True when a piece of text carries no word content (pure whitespace).
fn is_blank(s: &str) -> bool {
    s.chars().all(char::is_whitespace)
}

/// A shaped text block positioned on screen, with hit-testing and
/// selection-geometry helpers layered on top of a cosmic-text [`Buffer`].
pub struct TextView {
    /// The shaped buffer. Hand it straight to [`crate::TextRenderer::text`] or
    /// [`crate::BlockContent::buffer`]; read [`Buffer::layout_runs`] for the
    /// laid-out glyphs.
    pub buffer: Buffer,
    /// Top-left corner in screen pixels.
    pub pos: [f32; 2],
    /// Decoration metrics of every face the shaped text uses, keyed by
    /// `fontdb::ID`. Filled in after each re-shape, because
    /// [`TextView::decoration_rects`] takes `&self` and reading a face's
    /// metrics needs the font system.
    line_metrics: FxHashMap<fontdb::ID, LineMetrics>,
}

impl TextView {
    /// An empty view laid out at `metrics`, parked at the origin. Set
    /// [`TextView::pos`] and give it text with [`TextView::set_text`].
    pub fn new(font_system: &mut FontSystem, metrics: Metrics) -> Self {
        Self {
            buffer: Buffer::new(font_system, metrics),
            pos: [0.0, 0.0],
            line_metrics: FxHashMap::default(),
        }
    }

    /// Replace the text and re-shape it with one set of attributes.
    ///
    /// ```
    /// # use faf_text::{Attrs, Family, Metrics, TextView};
    /// let mut fonts = faf_text::font_system_from_fonts(&[faf_text::FONT_DEJAVU_SANS]);
    /// let mut view = TextView::new(&mut fonts, Metrics::new(16.0, 22.0));
    /// view.set_text(&mut fonts, "shaped", &Attrs::new().family(Family::SansSerif));
    /// assert_eq!(view.find_all("ape").len(), 1);
    /// ```
    pub fn set_text(&mut self, font_system: &mut FontSystem, text: &str, attrs: &Attrs) {
        self.buffer.set_text(text, attrs, Shaping::Advanced, None);
        self.reshaped(font_system);
    }

    /// Shape a run of differently attributed spans. Attributes that carry a
    /// text decoration (`Attrs::underline`, `strikethrough`, `overline`) draw
    /// themselves — [`crate::TextRenderer`] turns the shaped decoration spans
    /// into instances with no further help.
    pub fn set_rich_text<'r, 's, I>(
        &mut self,
        font_system: &mut FontSystem,
        spans: I,
        default_attrs: &Attrs<'_>,
    ) where
        I: IntoIterator<Item = (&'s str, Attrs<'r>)>,
    {
        self.buffer
            .set_rich_text(spans, default_attrs, Shaping::Advanced, None);
        self.reshaped(font_system);
    }

    /// Constrain layout width/height (px) and re-shape.
    pub fn set_size(
        &mut self,
        font_system: &mut FontSystem,
        width: Option<f32>,
        height: Option<f32>,
    ) {
        self.buffer.set_size(width, height);
        self.reshaped(font_system);
    }

    /// Change font size / line height and re-shape. Cheap enough to drive from
    /// a per-frame animation at demo sizes, but it *is* a re-shape — animating
    /// weight instead ([`crate::TextRenderer::text_with_weight`]) is free.
    pub fn set_metrics(&mut self, font_system: &mut FontSystem, metrics: Metrics) {
        self.buffer.set_metrics(metrics);
        self.reshaped(font_system);
    }

    /// Lay the buffer out and top up the decoration-metrics cache with any
    /// face the new layout brought in. Faces already cached are not read
    /// again, so a re-shape of the same text is one hash lookup per glyph.
    fn reshaped(&mut self, font_system: &mut FontSystem) {
        self.buffer.shape_until_scroll(font_system, false);
        let mut missing: Vec<(fontdb::ID, fontdb::Weight)> = Vec::new();
        for run in self.buffer.layout_runs() {
            for glyph in run.glyphs {
                if !self.line_metrics.contains_key(&glyph.font_id)
                    && !missing.iter().any(|&(id, _)| id == glyph.font_id)
                {
                    missing.push((glyph.font_id, glyph.font_weight));
                }
            }
        }
        for (id, weight) in missing {
            let metrics = font_system
                .get_font(id, weight)
                .map_or(LineMetrics::FALLBACK, |font| LineMetrics::of_font(&font));
            self.line_metrics.insert(id, metrics);
        }
    }

    /// The decoration metrics cached for a face, or the fallbacks.
    pub fn line_metrics(&self, font_id: fontdb::ID) -> LineMetrics {
        self.line_metrics
            .get(&font_id)
            .copied()
            .unwrap_or(LineMetrics::FALLBACK)
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

    /// Screen-pixel rectangles to draw `kind` over the text between two
    /// cursors (any order) — the same BiDi-aware, one-per-run spans
    /// [`Self::selection_rects`] produces, but anchored to the baseline at the
    /// offset and thickness the face asks for.
    ///
    /// The line kinds come back as the bar (or, for a squiggle, the band its
    /// wave sweeps) and nothing more; a chip comes back as the whole line box,
    /// which is what an inline-code background wants. Feed the rects to
    /// [`crate::TextRenderer::decoration`].
    ///
    /// Metrics are per span, from the largest glyph the span covers on that
    /// run, so a decoration under mixed sizes follows the text that dominates
    /// it.
    pub fn decoration_rects(&self, a: Cursor, b: Cursor, kind: DecorationKind) -> Vec<Rect> {
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        let fallback_size = self.buffer.metrics().font_size;
        let mut rects = Vec::new();
        for run in self.buffer.layout_runs() {
            // Same guard as selection_rects: highlight() trusts the caller to
            // have checked that the run's line is in range.
            if run.line_i < start.line || run.line_i > end.line {
                continue;
            }
            for (x, width) in run.highlight(start, end) {
                let (font_id, size) = dominant_glyph(&run, x, width)
                    .map_or((None, fallback_size), |g| (Some(g.font_id), g.font_size));
                let metrics = font_id.map_or(LineMetrics::FALLBACK, |id| self.line_metrics(id));
                let baseline = self.pos[1] + run.line_y;
                // A sub-pixel bar would blink in and out along the line.
                let thickness = (metrics.thickness * size).max(1.0);
                let (y, height) = match kind {
                    DecorationKind::Underline => {
                        (baseline - metrics.underline_offset * size, thickness)
                    }
                    DecorationKind::Strikethrough => {
                        (baseline - metrics.strikeout_offset * size, thickness)
                    }
                    // The band is three strokes tall — one for the stroke,
                    // one for the swing either side of it — and is centered
                    // on where the underline would have sat.
                    DecorationKind::Squiggle => {
                        let center = baseline - metrics.underline_offset * size + thickness * 0.5;
                        (center - thickness * 1.5, thickness * 3.0)
                    }
                    DecorationKind::Chip { .. } => (self.pos[1] + run.line_top, run.line_height),
                };
                rects.push([self.pos[0] + x, y, width, height]);
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
