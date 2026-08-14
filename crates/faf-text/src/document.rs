//! Virtualized documents: hold a whole log in memory, shape only what shows.
//!
//! A [`Document`] owns the backing text plus a line index (byte offsets of
//! line starts, extended incrementally by [`Document::append`]). Lines are
//! grouped into [`CHUNK_LINES`]-line chunks; only the chunks intersecting the
//! viewport (± one chunk) are shaped into cosmic-text buffers, and chunks that
//! drift further than [`RETAIN_CHUNKS`] away are dropped.
//!
//! Total height starts as an estimate — `line_height` × an estimated wrapped
//! row count per line — and is **corrected** as chunks shape and report their
//! true height. The scroll extent therefore shifts slightly while scrolling
//! through unvisited regions; that jitter is the standard trade for not
//! shaping 100 MB of text up front.

use cosmic_text::{Attrs, AttrsOwned, Buffer, Cursor, FontSystem, Metrics};
use rustc_hash::FxHashMap;

use crate::view::{Rect, TextView};

/// Lines per shaped chunk.
pub const CHUNK_LINES: usize = 128;

/// Chunks shaped on each side of the viewport range.
pub const WINDOW_CHUNKS: usize = 1;

/// Chunks kept resident on each side of the viewport range. Anything further
/// out is evicted.
pub const RETAIN_CHUNKS: usize = 3;

/// Hard cap on resident chunks, so a pathological viewport (taller than the
/// document, say) cannot pin unbounded memory.
const MAX_RESIDENT: usize = 16;

/// Rough average glyph advance as a fraction of the font size, used to guess
/// how many columns fit in the layout width before a chunk is shaped.
const EST_ADVANCE_RATIO: f32 = 0.55;

/// A position in the document: a document-global line index plus a byte offset
/// **within that line** (the same convention as [`Cursor::index`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct DocCursor {
    pub line: usize,
    pub byte: usize,
}

impl DocCursor {
    pub const fn new(line: usize, byte: usize) -> Self {
        Self { line, byte }
    }
}

/// Counters describing what the shaping window has been doing. Handy for
/// asserting in tests and printing in examples.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DocStats {
    /// Chunks currently holding a shaped buffer.
    pub resident_chunks: usize,
    /// Chunks shaped during the most recent [`Document::set_viewport`].
    pub shaped_last: usize,
    /// Chunks shaped since the document was built.
    pub shaped_total: usize,
    /// Chunks dropped since the document was built.
    pub evicted_total: usize,
    /// Document lines that have ever been handed to the shaper.
    pub lines_shaped_total: usize,
}

/// One shaped chunk: a [`TextView`] whose `pos` is in document space.
struct Chunk {
    view: TextView,
    /// Measured height in px, as laid out at the current width.
    height: f32,
}

/// A large body of text that shapes lazily, one viewport at a time.
pub struct Document {
    text: String,
    /// Byte offset of every line start. Always begins with 0, so
    /// `line_starts.len() == line_count()`.
    line_starts: Vec<u32>,
    /// Character count of every line, kept alongside the index so height
    /// estimates can be recomputed on resize without touching the text.
    line_cols: Vec<u32>,

    metrics: Metrics,
    attrs: AttrsOwned,
    width: f32,
    scroll_y: f32,
    viewport_h: f32,

    /// Current height of each chunk: estimated, or measured once shaped.
    heights: Vec<f32>,
    /// Whether `heights[i]` came from an actual layout.
    measured: Vec<bool>,
    /// Prefix sums of `heights`; `tops.len() == heights.len() + 1`.
    tops: Vec<f32>,

    chunks: FxHashMap<usize, Chunk>,
    /// Chunk indices covering the viewport ± [`WINDOW_CHUNKS`], ascending.
    window: Vec<usize>,
    stats: DocStats,
}

impl Document {
    pub fn new(metrics: Metrics) -> Self {
        let mut doc = Self {
            text: String::new(),
            line_starts: Vec::new(),
            line_cols: Vec::new(),
            metrics,
            attrs: AttrsOwned::new(&Attrs::new()),
            width: f32::INFINITY,
            scroll_y: 0.0,
            viewport_h: 0.0,
            heights: Vec::new(),
            measured: Vec::new(),
            tops: Vec::new(),
            chunks: FxHashMap::default(),
            window: Vec::new(),
            stats: DocStats::default(),
        };
        doc.reindex(0);
        doc.reestimate_from(0);
        doc
    }

    /// Default attributes for every shaped chunk. Invalidates shaped chunks.
    pub fn set_attrs(&mut self, attrs: &Attrs) {
        self.attrs = AttrsOwned::new(attrs);
        self.invalidate_shaping();
    }

    pub fn metrics(&self) -> Metrics {
        self.metrics
    }

    /// Change font size / line height. Invalidates shaped chunks.
    pub fn set_metrics(&mut self, metrics: Metrics) {
        if metrics != self.metrics {
            self.metrics = metrics;
            self.invalidate_shaping();
        }
    }

    /// Replace the whole document.
    pub fn set_text(&mut self, text: &str) {
        self.text.clear();
        self.text.push_str(text);
        self.reindex(0);
        self.invalidate_shaping();
    }

    /// Append to the end of the document, extending the line index over the
    /// appended slice only — nothing before `from` is rescanned.
    pub fn append(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let from = self.text.len();
        self.text.push_str(text);
        // The line the append lands in grows, so its chunk (and every chunk
        // after it) has to be re-shaped and re-estimated.
        let dirty = chunk_of_line(self.line_count() - 1);
        self.reindex(from);
        self.drop_chunks_from(dirty);
        self.reestimate_from(dirty);
    }

    /// The whole backing text.
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    /// One line's text, without its trailing newline.
    pub fn line(&self, line: usize) -> &str {
        let (start, end) = self.line_bounds(line);
        &self.text[start..end]
    }

    /// Document height in px: measured where chunks have shaped, estimated
    /// everywhere else.
    pub fn total_height(&self) -> f32 {
        *self.tops.last().unwrap_or(&0.0)
    }

    pub fn scroll_y(&self) -> f32 {
        self.scroll_y
    }

    pub fn viewport_height(&self) -> f32 {
        self.viewport_h
    }

    pub fn stats(&self) -> DocStats {
        self.stats
    }

    /// Number of chunks the document is split into.
    pub fn chunk_count(&self) -> usize {
        self.heights.len()
    }

    /// The chunk a document line belongs to.
    pub fn chunk_of_line(&self, line: usize) -> usize {
        chunk_of_line(line)
    }

    /// Half-open document line range `[start, end)` covered by a chunk.
    pub fn chunk_lines(&self, chunk: usize) -> (usize, usize) {
        let start = chunk * CHUNK_LINES;
        (
            start.min(self.line_count()),
            (start + CHUNK_LINES).min(self.line_count()),
        )
    }

    /// Document-space y of a chunk's top edge.
    pub fn chunk_top(&self, chunk: usize) -> f32 {
        self.tops.get(chunk).copied().unwrap_or(0.0)
    }

    /// Whether a chunk currently holds a shaped buffer.
    pub fn is_shaped(&self, chunk: usize) -> bool {
        self.chunks.contains_key(&chunk)
    }

    /// Resident chunk indices, ascending. Mostly useful for tests.
    pub fn resident_chunks(&self) -> Vec<usize> {
        let mut out: Vec<usize> = self.chunks.keys().copied().collect();
        out.sort_unstable();
        out
    }

    /// Estimated document-space y of a line's top edge. Exact for lines in
    /// chunks that have not shaped yet; approximate inside a shaped chunk,
    /// where the true row heights are known only to the layout.
    pub fn line_top(&self, line: usize) -> f32 {
        let line = line.min(self.line_count().saturating_sub(1));
        let chunk = chunk_of_line(line);
        let (start, _) = self.chunk_lines(chunk);
        let cols = self.est_cols();
        let rows: f32 = self.line_cols[start..line]
            .iter()
            .map(|&c| est_rows(cols, c))
            .sum();
        self.chunk_top(chunk) + rows * self.metrics.line_height
    }

    /// Point the shaping window at `scroll_y .. scroll_y + height` of a
    /// `width`-wide pane, shaping whatever is missing and evicting whatever
    /// has drifted out of range.
    pub fn set_viewport(
        &mut self,
        font_system: &mut FontSystem,
        width: f32,
        scroll_y: f32,
        height: f32,
    ) {
        if width != self.width {
            self.width = width;
            self.invalidate_shaping();
        }
        self.scroll_y = scroll_y;
        self.viewport_h = height;
        self.stats.shaped_last = 0;

        // Shaping corrects chunk heights, which moves everything below them,
        // which can pull a new chunk into view. One extra pass settles it; any
        // residual drift shows up on the next frame.
        let mut range = (0, 0);
        for _ in 0..2 {
            range = self.viewport_chunks();
            let lo = range.0.saturating_sub(WINDOW_CHUNKS);
            let hi = (range.1 + WINDOW_CHUNKS).min(self.chunk_count().saturating_sub(1));
            let mut corrected = false;
            for chunk in lo..=hi {
                if !self.chunks.contains_key(&chunk) {
                    corrected |= self.shape_chunk(font_system, chunk);
                }
            }
            self.window = (lo..=hi).filter(|c| self.chunks.contains_key(c)).collect();
            if !corrected {
                break;
            }
        }

        self.evict(range);
        self.stats.resident_chunks = self.chunks.len();
    }

    /// Shaped views covering the viewport ± [`WINDOW_CHUNKS`], each positioned
    /// in document space (subtract the scroll offset to draw them).
    pub fn visible(&self) -> impl Iterator<Item = &TextView> {
        self.window
            .iter()
            .filter_map(|chunk| self.chunks.get(chunk).map(|c| &c.view))
    }

    /// Map a document-space point to the closest cursor. Only the shaped
    /// window has geometry, so a point outside it clamps to the nearest shaped
    /// chunk; with nothing shaped at all the answer is `None`.
    pub fn hit(&self, x: f32, y_doc: f32) -> Option<DocCursor> {
        let chunk = *self.window.iter().min_by_key(|&&chunk| {
            let c = &self.chunks[&chunk];
            let top = c.view.pos[1];
            let d = if y_doc < top {
                top - y_doc
            } else if y_doc > top + c.height {
                y_doc - (top + c.height)
            } else {
                0.0
            };
            d.to_bits()
        })?;
        let cursor = self.chunks[&chunk].view.hit(x, y_doc)?;
        Some(self.to_doc(chunk, cursor))
    }

    /// Every occurrence of `needle` in the **whole** backing text, shaped or
    /// not. Case-sensitive; matches may span lines.
    pub fn find_all(&self, needle: &str) -> Vec<(DocCursor, DocCursor)> {
        if needle.is_empty() {
            return Vec::new();
        }
        self.text
            .match_indices(needle)
            .map(|(at, matched)| {
                (
                    self.cursor_at_offset(at),
                    self.cursor_at_offset(at + matched.len()),
                )
            })
            .collect()
    }

    /// Document-space rects covering the text between two cursors, for the
    /// **visible** part of the range only — unshaped regions have no geometry.
    pub fn selection_rects(&self, a: DocCursor, b: DocCursor) -> Vec<Rect> {
        let (start, end) = order(a, b);
        let mut rects = Vec::new();
        for &chunk in &self.window {
            let (first, last) = self.chunk_lines(chunk);
            if last == first || start.line >= last || end.line < first {
                continue;
            }
            let local_a = if start.line < first {
                Cursor::new(0, 0)
            } else {
                Cursor::new(start.line - first, start.byte)
            };
            let local_b = if end.line >= last {
                Cursor::new(last - 1 - first, self.line(last - 1).len())
            } else {
                Cursor::new(end.line - first, end.byte)
            };
            rects.extend(self.chunks[&chunk].view.selection_rects(local_a, local_b));
        }
        rects
    }

    /// The text between two cursors (any order), straight out of the backing
    /// string — no shaping required.
    pub fn text_between(&self, a: DocCursor, b: DocCursor) -> String {
        let (start, end) = order(a, b);
        let (from, to) = (self.offset_of(start), self.offset_of(end));
        self.text[from..to].to_string()
    }

    /// Byte offset of a cursor in the backing text.
    pub fn offset_of(&self, c: DocCursor) -> usize {
        let (start, end) = self.line_bounds(c.line.min(self.line_count() - 1));
        (start + c.byte).min(end)
    }

    /// The cursor at a byte offset in the backing text.
    pub fn cursor_at_offset(&self, offset: usize) -> DocCursor {
        let offset = offset.min(self.text.len());
        let line = self.line_starts.partition_point(|&s| s as usize <= offset) - 1;
        DocCursor::new(line, offset - self.line_starts[line] as usize)
    }

    // --- internals ---------------------------------------------------------

    /// Byte range of a line, excluding its trailing newline.
    fn line_bounds(&self, line: usize) -> (usize, usize) {
        let start = self.line_starts[line] as usize;
        let end = match self.line_starts.get(line + 1) {
            // Every following line start sits one byte past a '\n'.
            Some(&next) => next as usize - 1,
            None => self.text.len(),
        };
        (start, end)
    }

    /// Rebuild the line index over `self.text[from..]`, keeping every entry
    /// that starts at or before `from`. Nothing before `from` is rescanned.
    fn reindex(&mut self, from: usize) {
        let keep = self
            .line_starts
            .partition_point(|&s| (s as usize) <= from)
            .max(1);
        self.line_starts.truncate(keep);
        self.line_cols.truncate(keep);
        if self.line_starts.is_empty() {
            self.line_starts.push(0);
            self.line_cols.push(0);
        }
        // The line `from` landed in may have grown, so recount from there.
        let first_dirty = self.line_starts.len() - 1;
        for (i, b) in self.text.as_bytes().iter().enumerate().skip(from) {
            if *b == b'\n' {
                self.line_starts.push(i as u32 + 1);
                self.line_cols.push(0);
            }
        }
        for line in first_dirty..self.line_starts.len() {
            let (start, end) = self.line_bounds(line);
            self.line_cols[line] = char_len(&self.text[start..end]) as u32;
        }
    }

    /// Drop every shaped chunk and re-estimate all heights.
    fn invalidate_shaping(&mut self) {
        self.drop_chunks_from(0);
        self.reestimate_from(0);
    }

    fn drop_chunks_from(&mut self, first: usize) {
        let before = self.chunks.len();
        self.chunks.retain(|&chunk, _| chunk < first);
        self.stats.evicted_total += before - self.chunks.len();
        self.window.retain(|c| self.chunks.contains_key(c));
    }

    /// Re-derive estimated heights for chunk `first` onward (which must have
    /// no resident buffers), then rebuild the prefix sums.
    fn reestimate_from(&mut self, first: usize) {
        let chunks = self.line_count().div_ceil(CHUNK_LINES).max(1);
        self.heights.resize(chunks, 0.0);
        self.measured.resize(chunks, false);
        let cols = self.est_cols();
        for chunk in first..chunks {
            let (start, end) = self.chunk_lines(chunk);
            let rows: f32 = self.line_cols[start..end]
                .iter()
                .map(|&c| est_rows(cols, c))
                .sum();
            self.heights[chunk] = rows * self.metrics.line_height;
            self.measured[chunk] = false;
        }
        self.rebuild_tops();
    }

    fn rebuild_tops(&mut self) {
        self.tops.clear();
        self.tops.reserve(self.heights.len() + 1);
        let mut y = 0.0;
        self.tops.push(y);
        for &h in &self.heights {
            y += h;
            self.tops.push(y);
        }
        // Chunk tops moved, so every resident view has to be repositioned.
        for (&chunk, c) in self.chunks.iter_mut() {
            c.view.pos = [0.0, self.tops[chunk]];
        }
    }

    /// Columns that fit in the layout width, or infinity when not wrapping.
    fn est_cols(&self) -> f32 {
        match self.wrap_width() {
            Some(w) => (w / (EST_ADVANCE_RATIO * self.metrics.font_size)).max(1.0),
            None => f32::INFINITY,
        }
    }

    fn wrap_width(&self) -> Option<f32> {
        (self.width.is_finite() && self.width > 0.0).then_some(self.width)
    }

    /// Inclusive chunk range intersecting the viewport.
    fn viewport_chunks(&self) -> (usize, usize) {
        let last = self.chunk_count().saturating_sub(1);
        let bottom = self.scroll_y + self.viewport_h.max(0.0);
        // tops[i + 1] is chunk i's bottom edge.
        let first = self.tops[1..]
            .partition_point(|&t| t <= self.scroll_y)
            .min(last);
        let end = self.tops[..self.tops.len() - 1]
            .partition_point(|&t| t < bottom)
            .saturating_sub(1);
        (first, end.max(first).min(last))
    }

    /// Shape one chunk and record its true height. Returns whether the
    /// correction changed the document's layout.
    fn shape_chunk(&mut self, font_system: &mut FontSystem, chunk: usize) -> bool {
        let (start, end) = self.chunk_lines(chunk);
        if end == start {
            return false;
        }
        let from = self.line_starts[start] as usize;
        let to = self.line_bounds(end - 1).1;
        let text = self.text[from..to].to_string();

        let mut view = TextView::new(font_system, self.metrics);
        view.set_text(font_system, &text, &self.attrs.as_attrs());
        if let Some(w) = self.wrap_width() {
            view.set_size(font_system, Some(w), None);
        }
        let height = measure(&view.buffer);
        view.pos = [0.0, self.tops[chunk]];

        self.stats.shaped_last += 1;
        self.stats.shaped_total += 1;
        self.stats.lines_shaped_total += end - start;
        self.chunks.insert(chunk, Chunk { view, height });

        let corrected = self.heights[chunk] != height;
        self.heights[chunk] = height;
        self.measured[chunk] = true;
        if corrected {
            self.rebuild_tops();
        }
        corrected
    }

    /// Drop chunks outside the retention band, then trim the furthest chunks
    /// if the cache is still over its cap.
    fn evict(&mut self, viewport: (usize, usize)) {
        let keep_lo = viewport.0.saturating_sub(RETAIN_CHUNKS);
        let keep_hi = viewport.1 + RETAIN_CHUNKS;
        let before = self.chunks.len();
        self.chunks
            .retain(|&chunk, _| chunk >= keep_lo && chunk <= keep_hi);

        // Never trim below the window itself; those buffers are about to be
        // drawn.
        let cap = MAX_RESIDENT.max(self.window.len());
        if self.chunks.len() > cap {
            let center = (viewport.0 + viewport.1) / 2;
            let mut resident: Vec<usize> = self.chunks.keys().copied().collect();
            resident.sort_unstable_by_key(|&c| c.abs_diff(center));
            for &chunk in &resident[cap..] {
                self.chunks.remove(&chunk);
            }
        }
        self.stats.evicted_total += before - self.chunks.len();
        self.window.retain(|c| self.chunks.contains_key(c));
    }

    fn to_doc(&self, chunk: usize, cursor: Cursor) -> DocCursor {
        DocCursor::new(chunk * CHUNK_LINES + cursor.line, cursor.index)
    }
}

fn chunk_of_line(line: usize) -> usize {
    line / CHUNK_LINES
}

fn order(a: DocCursor, b: DocCursor) -> (DocCursor, DocCursor) {
    if a <= b { (a, b) } else { (b, a) }
}

/// Character count, taking the SIMD-friendly path on ASCII (the common case
/// for the multi-megabyte logs this module exists for).
fn char_len(s: &str) -> usize {
    if s.is_ascii() {
        s.len()
    } else {
        s.chars().count()
    }
}

/// Estimated wrapped row count for a line of `chars` characters.
fn est_rows(cols: f32, chars: u32) -> f32 {
    if !cols.is_finite() {
        return 1.0;
    }
    (chars as f32 / cols).ceil().max(1.0)
}

/// True laid-out height of a shaped buffer.
fn measure(buffer: &Buffer) -> f32 {
    buffer
        .layout_runs()
        .map(|run| run.line_top + run.line_height)
        .fold(0.0, f32::max)
}
