//! Terminal grid: a shaping-free fast path for monospace cell content.
//!
//! A log pane or a terminal emulator knows exactly where every glyph goes
//! before it asks anyone: cells are a fixed size, the font is monospace, and
//! the interesting content changes every frame. Running that through a shaper
//! is pure overhead, so this module doesn't. [`TermGrid`] is the model (cells,
//! scrollback, viewport) and [`GridFont`] the translation table: char → glyph
//! id through the font's own charmap ([`swash`]), cached per face, with fixed
//! cell metrics taken from the advance of `M`.
//!
//! Everything past that point is the ordinary renderer. Glyph ids are queued
//! as [`GlyphSpec`]s, so a grid's glyphs land in the same curve store as a
//! prose pane's and cost the same nothing to draw. Three things do *not* go
//! through the charmap:
//!
//! - **Wide characters.** East Asian Width `W`/`F` (see [`char_width`]) take
//!   two cells; the second is a continuation and draws nothing.
//! - **The fallback lane.** A charmap miss (CJK in a Latin mono font), a
//!   grapheme cluster with combining marks in it, or an emoji goes through a
//!   one-cell cosmic-text [`Buffer`], shaped once and cached by string. Font
//!   fallback, marks and color emoji all keep working; correctness beats
//!   purity.
//! - **Box drawing and block elements** (U+2500–U+259F). Those are generated
//!   as rects snapped to exact cell bounds, never from font outlines, because
//!   a font's idea of where a line sits leaves hairline gaps between cells at
//!   most sizes. Shades are the cell rect at reduced alpha.
//!
//! The translation is a pure CPU pass ([`TermGrid::build`]) into a reusable
//! [`GridScene`]; [`TermGrid::render`] hands that to one retained block, so
//! appending a line to a 200×60 grid re-uploads that block and nothing else.

use std::collections::VecDeque;

use cosmic_text::{Attrs, Buffer, Family, FamilyOwned, FontSystem, Metrics, Shaping, fontdb};
use rustc_hash::FxHashMap;
use unicode_segmentation::UnicodeSegmentation;

use crate::Color;
use crate::renderer::{BlockContent, BlockId, DecorationKind, GlyphSpec, TextRenderer};
use crate::view::{LineMetrics, Rect};

// ---- Cells ----

/// A cell's style bits. Hand-rolled rather than a `bitflags` dependency: there
/// are four of them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct CellStyle(pub u8);

impl CellStyle {
    pub const NONE: CellStyle = CellStyle(0);
    /// Draws from the family's bold face when one resolved.
    pub const BOLD: CellStyle = CellStyle(1 << 0);
    /// Draws from the family's italic face when one resolved.
    pub const ITALIC: CellStyle = CellStyle(1 << 1);
    pub const UNDERLINE: CellStyle = CellStyle(1 << 2);
    pub const STRIKE: CellStyle = CellStyle(1 << 3);

    /// True when every bit of `bits` is set here.
    pub const fn contains(self, bits: CellStyle) -> bool {
        self.0 & bits.0 == bits.0
    }

    pub const fn with(self, bits: CellStyle) -> CellStyle {
        CellStyle(self.0 | bits.0)
    }

    pub const fn without(self, bits: CellStyle) -> CellStyle {
        CellStyle(self.0 & !bits.0)
    }

    /// Which of a [`GridFont`]'s four faces this style draws from: regular,
    /// bold, italic, bold italic.
    const fn face(self) -> usize {
        (self.0 & CellStyle::BOLD.0) as usize | ((self.0 & CellStyle::ITALIC.0) as usize)
    }
}

impl std::ops::BitOr for CellStyle {
    type Output = CellStyle;
    fn bitor(self, rhs: CellStyle) -> CellStyle {
        self.with(rhs)
    }
}

/// One cell: a character, its colors, and its style bits.
///
/// A cell holds a single `char`. The rest of a grapheme cluster — combining
/// marks, ZWJ sequences, variation selectors — lives beside the row and is
/// picked up by the fallback lane; [`TermGrid::cluster`] is how to read it
/// back.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    /// `None` leaves the surface (or whatever is behind the block) showing.
    pub bg: Option<Color>,
    pub style: CellStyle,
}

impl Cell {
    /// The `ch` of the second cell of a wide character: it draws nothing, and
    /// carries the colors of the cell it continues so background runs merge.
    pub const CONTINUATION: char = '\0';

    pub fn new(ch: char, fg: Color) -> Self {
        Self {
            ch,
            fg,
            bg: None,
            style: CellStyle::NONE,
        }
    }

    /// The right half of a wide character.
    pub fn is_continuation(&self) -> bool {
        self.ch == Cell::CONTINUATION
    }
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: Color::WHITE,
            bg: None,
            style: CellStyle::NONE,
        }
    }
}

/// A row of cells plus the multi-char clusters a few of them may carry.
///
/// `clusters` is empty for the overwhelming majority of rows (plain text has
/// one char per cell), so it is a sorted `Vec` and not a map: the common case
/// is one `is_empty` check.
#[derive(Clone, Debug, Default)]
struct Row {
    cells: Vec<Cell>,
    clusters: Vec<(u16, Box<str>)>,
}

impl Row {
    fn blank(cols: u16) -> Self {
        Self {
            cells: vec![Cell::default(); cols as usize],
            clusters: Vec::new(),
        }
    }

    fn cluster(&self, col: u16) -> Option<&str> {
        if self.clusters.is_empty() {
            return None;
        }
        self.clusters
            .binary_search_by_key(&col, |(c, _)| *c)
            .ok()
            .map(|i| &*self.clusters[i].1)
    }

    fn set_cluster(&mut self, col: u16, text: &str) {
        match self.clusters.binary_search_by_key(&col, |(c, _)| *c) {
            Ok(i) => self.clusters[i].1 = text.into(),
            Err(i) => self.clusters.insert(i, (col, text.into())),
        }
    }

    fn clear_cluster(&mut self, col: u16) {
        if let Ok(i) = self.clusters.binary_search_by_key(&col, |(c, _)| *c) {
            self.clusters.remove(i);
        }
    }

    fn reset(&mut self, cols: u16) {
        self.cells.clear();
        self.cells.resize(cols as usize, Cell::default());
        self.clusters.clear();
    }

    fn resize(&mut self, cols: u16) {
        self.cells.resize(cols as usize, Cell::default());
        self.clusters.retain(|(col, _)| *col < cols);
    }
}

// ---- The grid ----

/// A terminal-style cell grid with scrollback.
///
/// Coordinates are `(col, row)` throughout — x then y, like every other
/// position in this crate — and rows are viewport rows: row 0 is the top of
/// what is on screen, which is not the top of the scrollback when
/// [`TermGrid::set_view_offset`] has been used.
pub struct TermGrid {
    cols: u16,
    rows: u16,
    screen: Vec<Row>,
    /// Rows that have scrolled off the top, oldest first, capped at
    /// `scrollback_limit`.
    scrollback: VecDeque<Row>,
    scrollback_limit: usize,
    view_offset: usize,
    dirty: bool,
}

impl TermGrid {
    pub fn new(cols: u16, rows: u16, scrollback_limit: usize) -> Self {
        Self {
            cols,
            rows,
            screen: (0..rows).map(|_| Row::blank(cols)).collect(),
            scrollback: VecDeque::new(),
            scrollback_limit,
            view_offset: 0,
            dirty: true,
        }
    }

    pub fn cols(&self) -> u16 {
        self.cols
    }

    pub fn rows(&self) -> u16 {
        self.rows
    }

    /// Rows currently held in scrollback.
    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    /// How many lines back the viewport is showing.
    pub fn view_offset(&self) -> usize {
        self.view_offset
    }

    /// Scroll the viewport back through the scrollback, clamped to what the
    /// ring still holds. Returns the offset actually taken.
    pub fn set_view_offset(&mut self, lines_back: usize) -> usize {
        let offset = lines_back.min(self.scrollback.len());
        if offset != self.view_offset {
            self.view_offset = offset;
            self.dirty = true;
        }
        offset
    }

    /// True if anything changed since the last [`TermGrid::take_dirty`]. The
    /// point of a retained block is to skip the translation entirely when the
    /// grid is idle.
    pub fn dirty(&self) -> bool {
        self.dirty
    }

    /// Read and clear the dirty flag: `if grid.take_dirty() { grid.render(..) }`.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::replace(&mut self.dirty, false)
    }

    /// Resize the grid, keeping content anchored to the *bottom* left the way
    /// a terminal does: rows pushed off the top go to scrollback, and new rows
    /// or columns come up blank.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        if (cols, rows) == (self.cols, self.rows) {
            return;
        }
        if cols != self.cols {
            for row in self.screen.iter_mut() {
                row.resize(cols);
            }
            for row in self.scrollback.iter_mut() {
                row.resize(cols);
            }
            self.cols = cols;
        }
        while self.screen.len() > rows as usize {
            let row = self.screen.remove(0);
            self.push_scrollback(row);
        }
        while self.screen.len() < rows as usize {
            self.screen.push(Row::blank(cols));
        }
        self.rows = rows;
        self.view_offset = self.view_offset.min(self.scrollback.len());
        self.dirty = true;
    }

    /// Blank every cell on screen. Scrollback is untouched.
    pub fn clear(&mut self) {
        let cols = self.cols;
        for row in self.screen.iter_mut() {
            row.reset(cols);
        }
        self.dirty = true;
    }

    /// A cell of the *screen* (scrollback is not addressable this way).
    pub fn cell(&self, col: u16, row: u16) -> Option<Cell> {
        self.screen
            .get(row as usize)
            .and_then(|r| r.cells.get(col as usize))
            .copied()
    }

    /// The full grapheme cluster in a screen cell, when it has more in it than
    /// the one `char` [`Cell`] carries.
    pub fn cluster(&self, col: u16, row: u16) -> Option<&str> {
        self.screen.get(row as usize)?.cluster(col)
    }

    pub fn set_cell(&mut self, col: u16, row: u16, cell: Cell) {
        if col >= self.cols {
            return;
        }
        let Some(row) = self.screen.get_mut(row as usize) else {
            return;
        };
        row.cells[col as usize] = cell;
        row.clear_cluster(col);
        self.dirty = true;
    }

    /// Write `text` starting at `(col, row)`, one cell per grapheme cluster
    /// (two for a wide one). Returns the column after the last cell written.
    ///
    /// Nothing wraps: a terminal's line wrapping is the emulator's decision,
    /// so text that runs past the last column is **clipped**. A wide character
    /// that would straddle the right edge is dropped and its would-be cell is
    /// left blank, which is what a terminal puts there. Control characters are
    /// skipped.
    pub fn print(&mut self, col: u16, row: u16, text: &str, fg: Color, bg: Option<Color>) -> u16 {
        self.print_styled(col, row, text, fg, bg, CellStyle::NONE)
    }

    /// [`TermGrid::print`] with style bits.
    pub fn print_styled(
        &mut self,
        col: u16,
        row: u16,
        text: &str,
        fg: Color,
        bg: Option<Color>,
        style: CellStyle,
    ) -> u16 {
        let cols = self.cols;
        if row as usize >= self.screen.len() {
            return col;
        }
        let line = &mut self.screen[row as usize];
        let mut at = col;
        for cluster in text.graphemes(true) {
            let Some(ch) = cluster.chars().next() else {
                continue;
            };
            if ch.is_control() {
                continue;
            }
            if at >= cols {
                break;
            }
            let width = cluster_width(cluster);
            let cell = Cell { ch, fg, bg, style };
            if at as u32 + width as u32 > cols as u32 {
                // A wide char with one cell left: the cell stays blank, and
                // (like a terminal) nothing after it is printed either.
                line.cells[at as usize] = Cell { ch: ' ', ..cell };
                line.clear_cluster(at);
                at = cols;
                break;
            }
            line.cells[at as usize] = cell;
            if cluster.chars().nth(1).is_some() {
                line.set_cluster(at, cluster);
            } else {
                line.clear_cluster(at);
            }
            if width == 2 {
                line.cells[at as usize + 1] = Cell {
                    ch: Cell::CONTINUATION,
                    ..cell
                };
                line.clear_cluster(at + 1);
            }
            at += width;
        }
        self.dirty = true;
        at
    }

    /// Scroll `n` rows off the top into scrollback, blanking the rows that
    /// appear at the bottom. A viewport that is already scrolled back stays
    /// where it is relative to the text, until the ring drops that text.
    pub fn scroll_up(&mut self, n: u16) {
        let n = n.min(self.rows);
        for _ in 0..n {
            let mut row = self.screen.remove(0);
            row.resize(self.cols);
            self.push_scrollback(row);
            self.screen.push(Row::blank(self.cols));
        }
        if self.view_offset > 0 {
            self.view_offset = (self.view_offset + n as usize).min(self.scrollback.len());
        }
        self.dirty = true;
    }

    /// Append a line at the bottom, scrolling first — the streaming-log move.
    pub fn push_line(&mut self, text: &str, fg: Color, bg: Option<Color>) {
        self.scroll_up(1);
        if self.rows > 0 {
            self.print(0, self.rows - 1, text, fg, bg);
        }
    }

    fn push_scrollback(&mut self, row: Row) {
        if self.scrollback_limit == 0 {
            return;
        }
        if self.scrollback.len() == self.scrollback_limit {
            self.scrollback.pop_front();
            // The line the viewport was pinned to just fell out of the ring.
            self.view_offset = self.view_offset.saturating_sub(1);
        }
        self.scrollback.push_back(row);
    }

    /// The row shown at viewport row `row`: scrollback while the viewport is
    /// scrolled back past it, the screen otherwise.
    fn visible_row(&self, row: u16) -> Option<&Row> {
        let row = row as usize;
        if row < self.view_offset {
            let back = self.view_offset - row;
            self.scrollback
                .len()
                .checked_sub(back)
                .and_then(|index| self.scrollback.get(index))
        } else {
            self.screen.get(row - self.view_offset)
        }
    }

    /// The cell shown at a viewport position, reading through the scrollback
    /// offset. `(0, 0)` is the top left of what is drawn.
    pub fn view_cell(&self, col: u16, row: u16) -> Option<Cell> {
        self.visible_row(row)?.cells.get(col as usize).copied()
    }
}

// ---- East Asian Width ----

/// Cells a character occupies: 2 for East Asian Width `W` (wide) and `F`
/// (fullwidth), 1 for everything else.
///
/// Combining marks are not asked about — they never reach a cell of their own,
/// because [`TermGrid::print`] splits its input into grapheme clusters and a
/// cluster gets one cell.
pub fn char_width(ch: char) -> u16 {
    let cp = ch as u32;
    // Everything below the first wide range, which is all of Latin, is the
    // hot path and skips the search entirely.
    if cp < WIDE_RANGES[0].0 {
        return 1;
    }
    match WIDE_RANGES.binary_search_by(|&(lo, hi)| {
        if cp < lo {
            std::cmp::Ordering::Greater
        } else if cp > hi {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Equal
        }
    }) {
        Ok(_) => 2,
        Err(_) => 1,
    }
}

/// Width of a whole grapheme cluster: its base character's, except that an
/// emoji-presentation selector (U+FE0F) makes the cluster wide — the same rule
/// terminals apply, and the one that keeps `⚠️` from clipping its neighbour.
fn cluster_width(cluster: &str) -> u16 {
    let mut chars = cluster.chars();
    let Some(base) = chars.next() else {
        return 0;
    };
    if cluster.contains('\u{FE0F}') {
        return 2;
    }
    char_width(base)
}

/// East Asian Width `W` and `F` ranges, merged and sorted, from
/// `EastAsianWidth-17.0.0.txt` (UAX #11, 2025-07-24). The blocks that file
/// documents as defaulting to `W` for unassigned code points — CJK Ext A, CJK
/// Unified, CJK Compatibility Ideographs, and planes 2 and 3 — are folded in,
/// so an unassigned ideograph still takes two cells rather than jumping to one
/// the day it is assigned.
static WIDE_RANGES: [(u32, u32); 123] = [
    (0x1100, 0x115F),
    (0x231A, 0x231B),
    (0x2329, 0x232A),
    (0x23E9, 0x23EC),
    (0x23F0, 0x23F0),
    (0x23F3, 0x23F3),
    (0x25FD, 0x25FE),
    (0x2614, 0x2615),
    (0x2630, 0x2637),
    (0x2648, 0x2653),
    (0x267F, 0x267F),
    (0x268A, 0x268F),
    (0x2693, 0x2693),
    (0x26A1, 0x26A1),
    (0x26AA, 0x26AB),
    (0x26BD, 0x26BE),
    (0x26C4, 0x26C5),
    (0x26CE, 0x26CE),
    (0x26D4, 0x26D4),
    (0x26EA, 0x26EA),
    (0x26F2, 0x26F3),
    (0x26F5, 0x26F5),
    (0x26FA, 0x26FA),
    (0x26FD, 0x26FD),
    (0x2705, 0x2705),
    (0x270A, 0x270B),
    (0x2728, 0x2728),
    (0x274C, 0x274C),
    (0x274E, 0x274E),
    (0x2753, 0x2755),
    (0x2757, 0x2757),
    (0x2795, 0x2797),
    (0x27B0, 0x27B0),
    (0x27BF, 0x27BF),
    (0x2B1B, 0x2B1C),
    (0x2B50, 0x2B50),
    (0x2B55, 0x2B55),
    (0x2E80, 0x2E99),
    (0x2E9B, 0x2EF3),
    (0x2F00, 0x2FD5),
    (0x2FF0, 0x303E),
    (0x3041, 0x3096),
    (0x3099, 0x30FF),
    (0x3105, 0x312F),
    (0x3131, 0x318E),
    (0x3190, 0x31E5),
    (0x31EF, 0x321E),
    (0x3220, 0x3247),
    (0x3250, 0xA48C),
    (0xA490, 0xA4C6),
    (0xA960, 0xA97C),
    (0xAC00, 0xD7A3),
    (0xF900, 0xFAFF),
    (0xFE10, 0xFE19),
    (0xFE30, 0xFE52),
    (0xFE54, 0xFE66),
    (0xFE68, 0xFE6B),
    (0xFF01, 0xFF60),
    (0xFFE0, 0xFFE6),
    (0x16FE0, 0x16FE4),
    (0x16FF0, 0x16FF6),
    (0x17000, 0x18CD5),
    (0x18CFF, 0x18D1E),
    (0x18D80, 0x18DF2),
    (0x1AFF0, 0x1AFF3),
    (0x1AFF5, 0x1AFFB),
    (0x1AFFD, 0x1AFFE),
    (0x1B000, 0x1B122),
    (0x1B132, 0x1B132),
    (0x1B150, 0x1B152),
    (0x1B155, 0x1B155),
    (0x1B164, 0x1B167),
    (0x1B170, 0x1B2FB),
    (0x1D300, 0x1D356),
    (0x1D360, 0x1D376),
    (0x1F004, 0x1F004),
    (0x1F0CF, 0x1F0CF),
    (0x1F18E, 0x1F18E),
    (0x1F191, 0x1F19A),
    (0x1F200, 0x1F202),
    (0x1F210, 0x1F23B),
    (0x1F240, 0x1F248),
    (0x1F250, 0x1F251),
    (0x1F260, 0x1F265),
    (0x1F300, 0x1F320),
    (0x1F32D, 0x1F335),
    (0x1F337, 0x1F37C),
    (0x1F37E, 0x1F393),
    (0x1F3A0, 0x1F3CA),
    (0x1F3CF, 0x1F3D3),
    (0x1F3E0, 0x1F3F0),
    (0x1F3F4, 0x1F3F4),
    (0x1F3F8, 0x1F43E),
    (0x1F440, 0x1F440),
    (0x1F442, 0x1F4FC),
    (0x1F4FF, 0x1F53D),
    (0x1F54B, 0x1F54E),
    (0x1F550, 0x1F567),
    (0x1F57A, 0x1F57A),
    (0x1F595, 0x1F596),
    (0x1F5A4, 0x1F5A4),
    (0x1F5FB, 0x1F64F),
    (0x1F680, 0x1F6C5),
    (0x1F6CC, 0x1F6CC),
    (0x1F6D0, 0x1F6D2),
    (0x1F6D5, 0x1F6D8),
    (0x1F6DC, 0x1F6DF),
    (0x1F6EB, 0x1F6EC),
    (0x1F6F4, 0x1F6FC),
    (0x1F7E0, 0x1F7EB),
    (0x1F7F0, 0x1F7F0),
    (0x1F90C, 0x1F93A),
    (0x1F93C, 0x1F945),
    (0x1F947, 0x1F9FF),
    (0x1FA70, 0x1FA7C),
    (0x1FA80, 0x1FA8A),
    (0x1FA8E, 0x1FAC6),
    (0x1FAC8, 0x1FAC8),
    (0x1FACD, 0x1FADC),
    (0x1FADF, 0x1FAEA),
    (0x1FAEF, 0x1FAF8),
    (0x20000, 0x2FFFD),
    (0x30000, 0x3FFFD),
];

/// Characters that want the fallback lane whatever the charmap says: emoji,
/// which a text font may well have a monochrome glyph for but which belong on
/// the shaped path, where font fallback finds the color font and the bitmap
/// atlas draws its layers.
fn is_emoji(ch: char) -> bool {
    matches!(ch as u32, 0x1F000..=0x1FAFF | 0x2600..=0x26FF | 0x2B00..=0x2BFF)
}

// ---- Font ----

/// One glyph as the grid needs it: an id to draw and the advance to center it
/// in its cell span.
#[derive(Clone, Copy, Debug, PartialEq)]
struct GridGlyph {
    id: u16,
    advance: f32,
}

/// A cell's worth of shaped output, from the fallback lane. Positions are
/// relative to the pen the run started at.
#[derive(Clone, Debug, Default)]
struct FallbackRun {
    glyphs: Vec<FallbackGlyph>,
    width: f32,
}

#[derive(Clone, Copy, Debug)]
struct FallbackGlyph {
    font_id: fontdb::ID,
    glyph_id: u16,
    weight: fontdb::Weight,
    font_size: f32,
    dx: f32,
    dy: f32,
}

/// One face of the grid's family, with its two caches: char → glyph id from
/// the charmap, and cluster string → shaped run for the fallback lane.
struct Face {
    id: fontdb::ID,
    weight: fontdb::Weight,
    style: fontdb::Style,
    glyphs: FxHashMap<char, Option<GridGlyph>>,
    fallback: FxHashMap<Box<str>, FallbackRun>,
}

/// The four style variants a grid can draw, in [`CellStyle::face`] order.
const VARIANTS: [(fontdb::Weight, fontdb::Style); 4] = [
    (fontdb::Weight::NORMAL, fontdb::Style::Normal),
    (fontdb::Weight::BOLD, fontdb::Style::Normal),
    (fontdb::Weight::NORMAL, fontdb::Style::Italic),
    (fontdb::Weight::BOLD, fontdb::Style::Italic),
];

/// The translation table between a [`TermGrid`]'s chars and the renderer's
/// glyphs: four faces of one monospace family, the cell metrics they imply,
/// and the caches that make the second frame free.
///
/// Build one per (family, size) and keep it: every cache in it is a pure win
/// that survives scrolling, since a terminal's alphabet is tiny.
pub struct GridFont {
    family: FamilyOwned,
    font_size: f32,
    cell: [f32; 2],
    ascent: f32,
    /// Underline and strikeout, in px y-up from the baseline (the top of the
    /// stroke), plus the stroke thickness — swash's convention, resolved once.
    underline_offset: f32,
    strikeout_offset: f32,
    stroke: f32,
    faces: [Face; 4],
    /// Reused by the fallback lane so a cache miss does not allocate a buffer.
    scratch: Option<Buffer>,
}

impl GridFont {
    /// Resolve a monospace family at `font_size` px and take the cell metrics
    /// from it: the cell is the advance of `M` wide and ascent + descent +
    /// leading tall, both **rounded to whole pixels** so that every cell
    /// boundary lands on a pixel boundary and procedural box drawing can join
    /// gap-free.
    ///
    /// Bold and italic are separate faces when the database has them and the
    /// regular face otherwise; a variable face is asked for the bold weight,
    /// which the GPU blends.
    ///
    /// # Panics
    ///
    /// If `font_system`'s database has no faces at all, or the resolved face
    /// has no data behind it.
    pub fn new(font_system: &mut FontSystem, family: Family<'_>, font_size: f32) -> Self {
        let faces = VARIANTS.map(|(weight, style)| Face {
            id: resolve_face(font_system, family, weight, style),
            weight,
            style,
            glyphs: FxHashMap::default(),
            fallback: FxHashMap::default(),
        });

        let font = font_system
            .get_font(faces[0].id, faces[0].weight)
            .expect("grid font face has no data");
        let swash = font.as_swash();
        let metrics = swash.metrics(&[]).scale(font_size);
        let m_glyph = swash.charmap().map('M');
        let advance = swash
            .glyph_metrics(&[])
            .scale(font_size)
            .advance_width(m_glyph);
        let cell_w = if advance > 0.0 {
            advance.round()
        } else {
            (font_size * 0.6).round()
        };
        let cell_h = (metrics.ascent + metrics.descent + metrics.leading).round();
        let or_else = |value: f32, fallback: f32| {
            if value == 0.0 {
                fallback * font_size
            } else {
                value
            }
        };
        let fallback = LineMetrics::FALLBACK;

        Self {
            family: FamilyOwned::new(family),
            font_size,
            cell: [cell_w.max(1.0), cell_h.max(1.0)],
            ascent: metrics.ascent.round(),
            underline_offset: or_else(metrics.underline_offset, fallback.underline_offset),
            strikeout_offset: or_else(metrics.strikeout_offset, fallback.strikeout_offset),
            stroke: or_else(metrics.stroke_size, fallback.thickness).max(1.0),
            faces,
            scratch: None,
        }
    }

    /// Cell size in px, `[width, height]`. Whole pixels, by construction.
    pub fn cell_size(&self) -> [f32; 2] {
        self.cell
    }

    /// Px size of a `cols × rows` grid drawn with this font.
    pub fn grid_size(&self, cols: u16, rows: u16) -> [f32; 2] {
        [self.cell[0] * cols as f32, self.cell[1] * rows as f32]
    }

    pub fn font_size(&self) -> f32 {
        self.font_size
    }

    /// Baseline offset from the top of a cell.
    pub fn ascent(&self) -> f32 {
        self.ascent
    }

    /// Char → glyph id straight off the charmap, no shaping. `None` is a miss
    /// (the face has no glyph), which sends the cell to the fallback lane.
    fn glyph(&mut self, font_system: &mut FontSystem, face: usize, ch: char) -> Option<GridGlyph> {
        if let Some(hit) = self.faces[face].glyphs.get(&ch) {
            return *hit;
        }
        let face_ref = &self.faces[face];
        let found = font_system
            .get_font(face_ref.id, face_ref.weight)
            .and_then(|font| {
                let swash = font.as_swash();
                let id = swash.charmap().map(ch);
                (id != 0).then(|| GridGlyph {
                    id,
                    advance: swash
                        .glyph_metrics(&[])
                        .scale(self.font_size)
                        .advance_width(id),
                })
            });
        self.faces[face].glyphs.insert(ch, found);
        found
    }

    /// The fallback lane: shape one cell's text through cosmic-text and cache
    /// the result by string. This is where CJK, combining marks and emoji get
    /// their font fallback, their marks positioned and their color layers.
    fn fallback(&mut self, font_system: &mut FontSystem, face: usize, text: &str) -> &FallbackRun {
        if !self.faces[face].fallback.contains_key(text) {
            let run = self.shape_cell(font_system, face, text);
            self.faces[face].fallback.insert(text.into(), run);
        }
        &self.faces[face].fallback[text]
    }

    fn shape_cell(&mut self, font_system: &mut FontSystem, face: usize, text: &str) -> FallbackRun {
        let metrics = Metrics::new(self.font_size, self.cell[1]);
        let mut buffer = self
            .scratch
            .take()
            .unwrap_or_else(|| Buffer::new(font_system, metrics));
        let attrs = Attrs::new()
            .family(self.family.as_family())
            .weight(self.faces[face].weight)
            .style(self.faces[face].style);
        buffer.set_metrics(metrics);
        buffer.set_size(None, None);
        buffer.set_text(text, &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(font_system, false);

        let mut run = FallbackRun::default();
        for line in buffer.layout_runs() {
            for glyph in line.glyphs.iter() {
                run.width = run.width.max(glyph.x + glyph.w);
                run.glyphs.push(FallbackGlyph {
                    font_id: glyph.font_id,
                    glyph_id: glyph.glyph_id,
                    weight: glyph.font_weight,
                    font_size: glyph.font_size,
                    dx: glyph.x + glyph.x_offset * glyph.font_size,
                    dy: glyph.y - glyph.y_offset * glyph.font_size,
                });
            }
        }
        self.scratch = Some(buffer);
        run
    }
}

/// Pick a face for a (family, weight, style), falling back to whatever the
/// database has: a fonts-only [`FontSystem`] has no `Monospace` default, and a
/// family with no italic still has to draw italic cells.
fn resolve_face(
    font_system: &FontSystem,
    family: Family<'_>,
    weight: fontdb::Weight,
    style: fontdb::Style,
) -> fontdb::ID {
    let db = font_system.db();
    let query = |families: &[Family<'_>], style| {
        db.query(&fontdb::Query {
            families,
            weight,
            stretch: fontdb::Stretch::Normal,
            style,
        })
    };
    query(&[family], style)
        .or_else(|| query(&[family], fontdb::Style::Normal))
        .or_else(|| query(&[Family::Monospace], fontdb::Style::Normal))
        // `new_with_fonts` appends blobs after the system scan, so the last
        // face is the one a fonts-only system was built for.
        .or_else(|| db.faces().last().map(|face| face.id))
        .expect("font database has no faces")
}

// ---- Translation ----

/// The instances one viewport translates into: merged background runs and the
/// procedural box-drawing rects (both under-rects, in that order), glyphs, and
/// underline/strikeout decorations.
///
/// Keep one and hand it back to [`TermGrid::build`] every frame — it exists so
/// that a 60 fps grid allocates nothing.
#[derive(Default)]
pub struct GridScene {
    pub rects: Vec<(Rect, Color)>,
    pub glyphs: Vec<GlyphSpec>,
    pub decorations: Vec<(Rect, DecorationKind, Color)>,
}

impl GridScene {
    pub fn new() -> Self {
        Self::default()
    }

    /// Total instance count, the number the renderer will upload.
    pub fn instances(&self) -> usize {
        self.rects.len() + self.glyphs.len() + self.decorations.len()
    }

    fn clear(&mut self) {
        self.rects.clear();
        self.glyphs.clear();
        self.decorations.clear();
    }
}

impl TermGrid {
    /// Translate the viewport into instances — the whole CPU cost of a frame,
    /// and the number the `term` example times.
    ///
    /// `origin` is the top-left of cell (0, 0), block-local. Backgrounds merge
    /// across cells that share a color, so a run of 200 unstyled cells is one
    /// rect and not 200.
    pub fn build(
        &self,
        font: &mut GridFont,
        font_system: &mut FontSystem,
        origin: [f32; 2],
        scene: &mut GridScene,
    ) {
        scene.clear();
        let [cell_w, cell_h] = font.cell;
        let cols = self.cols as usize;
        for r in 0..self.rows {
            let Some(row) = self.visible_row(r) else {
                continue;
            };
            let top = origin[1] + r as f32 * cell_h;
            let baseline = top + font.ascent;

            // Background runs: one rect per maximal same-color span.
            let mut run: Option<(usize, Color)> = None;
            for c in 0..=cols {
                let bg = row.cells.get(c).and_then(|cell| cell.bg);
                match (run, bg) {
                    (Some((_, color)), Some(next)) if color == next => continue,
                    (Some((start, color)), _) => {
                        let x = origin[0] + start as f32 * cell_w;
                        scene
                            .rects
                            .push(([x, top, (c - start) as f32 * cell_w, cell_h], color));
                        run = bg.map(|color| (c, color));
                    }
                    (None, Some(color)) => run = Some((c, color)),
                    (None, None) => {}
                }
            }

            for c in 0..cols {
                let cell = row.cells[c];
                if cell.is_continuation() {
                    continue;
                }
                let x = origin[0] + c as f32 * cell_w;
                let cluster = row.cluster(c as u16);
                // Cells the character occupies, as `print` counted them, and
                // never past the last column.
                let width = cluster.map_or_else(|| char_width(cell.ch), cluster_width);
                let span = width.min((cols - c) as u16) as f32 * cell_w;

                // Box drawing and block elements never touch the font: their
                // geometry is the cell, so it joins its neighbours exactly.
                if cluster.is_none()
                    && push_procedural(cell.ch, [x, top, span, cell_h], cell.fg, &mut scene.rects)
                {
                    continue;
                }
                if cell.ch == ' ' && cluster.is_none() {
                    continue;
                }

                let face = cell.style.face();
                let mut encoded = [0u8; 4];
                let text = cluster.unwrap_or_else(|| cell.ch.encode_utf8(&mut encoded));
                let simple = cluster.is_none() && !is_emoji(cell.ch);
                let direct = simple
                    .then(|| font.glyph(font_system, face, cell.ch))
                    .flatten();

                match direct {
                    Some(glyph) => {
                        let face = &font.faces[face];
                        scene.glyphs.push(GlyphSpec {
                            font_id: face.id,
                            glyph_id: glyph.id,
                            font_size: font.font_size,
                            pos: [x + (span - glyph.advance) * 0.5, baseline],
                            color: cell.fg,
                            weight: face.weight,
                        });
                    }
                    None => {
                        let run = font.fallback(font_system, face, text);
                        let dx = (span - run.width) * 0.5;
                        for glyph in &run.glyphs {
                            scene.glyphs.push(GlyphSpec {
                                font_id: glyph.font_id,
                                glyph_id: glyph.glyph_id,
                                font_size: glyph.font_size,
                                pos: [x + dx + glyph.dx, baseline + glyph.dy],
                                color: cell.fg,
                                weight: glyph.weight,
                            });
                        }
                    }
                }
            }

            // Underline and strikeout, merged the same way backgrounds are.
            for (bit, kind, offset) in [
                (
                    CellStyle::UNDERLINE,
                    DecorationKind::Underline,
                    font.underline_offset,
                ),
                (
                    CellStyle::STRIKE,
                    DecorationKind::Strikethrough,
                    font.strikeout_offset,
                ),
            ] {
                let mut run: Option<(usize, Color)> = None;
                for c in 0..=cols {
                    let lit = row
                        .cells
                        .get(c)
                        .filter(|cell| cell.style.contains(bit))
                        .map(|cell| cell.fg);
                    match (run, lit) {
                        (Some((_, color)), Some(next)) if color == next => continue,
                        (Some((start, color)), _) => {
                            let x = origin[0] + start as f32 * cell_w;
                            let rect = [
                                x,
                                (baseline - offset).round(),
                                (c - start) as f32 * cell_w,
                                font.stroke.round().max(1.0),
                            ];
                            scene.decorations.push((rect, kind, color));
                            run = lit.map(|color| (c, color));
                        }
                        (None, Some(color)) => run = Some((c, color)),
                        (None, None) => {}
                    }
                }
            }
        }
    }

    /// [`TermGrid::build`] into a retained block. Streaming appends therefore
    /// re-upload the grid's instances and nothing else in the scene.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &self,
        renderer: &mut TextRenderer,
        queue: &wgpu::Queue,
        font_system: &mut FontSystem,
        font: &mut GridFont,
        scene: &mut GridScene,
        block: BlockId,
        origin: [f32; 2],
    ) {
        self.build(font, font_system, origin, scene);
        renderer.set_block_content(
            queue,
            font_system,
            block,
            &BlockContent {
                under_rects: &scene.rects,
                glyphs: &scene.glyphs,
                decorations: &scene.decorations,
                ..Default::default()
            },
        );
    }
}

// ---- Box drawing and block elements ----

/// An arm of a box-drawing character.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stroke {
    None,
    Light,
    Heavy,
    Double,
}

use Stroke::Double as D;
use Stroke::Heavy as H;
use Stroke::Light as L;
use Stroke::None as N;

const UP: usize = 0;
const RIGHT: usize = 1;
const DOWN: usize = 2;
const LEFT: usize = 3;

/// Geometry for U+2500–U+259F, appended to the under-rects. Returns false for
/// anything else, which is then a glyph like any other.
///
/// These characters are drawn rather than shaped because their whole job is to
/// meet their neighbours exactly: cells are whole pixels (see
/// [`GridFont::new`]), strokes are centered on the same whole-pixel offsets in
/// every cell, and each arm runs from the center to the cell edge, so two
/// adjacent cells share an edge and a join has no seam. A font's own outlines
/// are a fraction of a pixel off at most sizes and stipple the seams.
fn push_procedural(ch: char, cell: Rect, color: Color, out: &mut Vec<(Rect, Color)>) -> bool {
    match ch as u32 {
        cp @ 0x2500..=0x257F => {
            box_drawing(cp, cell, color, out);
            true
        }
        cp @ 0x2580..=0x259F => {
            block_element(cp, cell, color, out);
            true
        }
        _ => false,
    }
}

fn box_drawing(cp: u32, cell: Rect, color: Color, out: &mut Vec<(Rect, Color)>) {
    let [x, y, w, h] = cell;
    // One eighth of the cell height, the proportion terminal fonts draw a
    // light stroke at, and never thinner than a pixel.
    let light = (h / 8.0).round().max(1.0);
    let heavy = (light * 2.0).min(h).max(1.0);
    let thick = |stroke: Stroke| match stroke {
        N => 0.0,
        H => heavy,
        _ => light,
    };
    // Left/top edge of a centered bar of thickness `t`, snapped to a pixel.
    let cx = |t: f32| x + ((w - t) * 0.5).round();
    let cy = |t: f32| y + ((h - t) * 0.5).round();

    if (0x2571..=0x2573).contains(&cp) {
        if cp != 0x2571 {
            diagonal(cell, light, true, color, out);
        }
        if cp != 0x2572 {
            diagonal(cell, light, false, color, out);
        }
        return;
    }

    let arms = BOX_ARMS[(cp - 0x2500) as usize];
    if let Some(count) = dash_count(cp) {
        let t = thick(arms[RIGHT].max_stroke(arms[UP]));
        if arms[RIGHT] != N {
            dashes(count, x, w, (cy(t), t), true, color, out);
        } else {
            dashes(count, y, h, (cx(t), t), false, color, out);
        }
        return;
    }

    let horiz_double = arms[RIGHT] == D || arms[LEFT] == D;
    let vert_double = arms[UP] == D || arms[DOWN] == D;
    if !horiz_double && !vert_double {
        // Every arm runs from its own centered offset to the cell edge, so
        // arms of different weights still overlap in the middle and corners
        // close.
        for (dir, stroke) in arms.iter().copied().enumerate() {
            if stroke == N {
                continue;
            }
            let t = thick(stroke);
            match dir {
                UP => emit_rail(
                    [(y, cy(t) + t), (0.0, 0.0), (0.0, 0.0)],
                    (cx(t), t),
                    false,
                    color,
                    out,
                ),
                DOWN => emit_rail(
                    [(cy(t), y + h), (0.0, 0.0), (0.0, 0.0)],
                    (cx(t), t),
                    false,
                    color,
                    out,
                ),
                LEFT => emit_rail(
                    [(x, cx(t) + t), (0.0, 0.0), (0.0, 0.0)],
                    (cy(t), t),
                    true,
                    color,
                    out,
                ),
                _ => emit_rail(
                    [(cx(t), x + w), (0.0, 0.0), (0.0, 0.0)],
                    (cy(t), t),
                    true,
                    color,
                    out,
                ),
            }
        }
        return;
    }

    // Doubles are two light rails either side of where the single bar would
    // be. A rail's middle is left out exactly where a *double* arm crosses it
    // — that is what makes ╬ four corners and ╠ a continuous left rail — while
    // a single arm just runs into the rail it meets.
    let cx0 = cx(light);
    let cy0 = cy(light);
    let (near_x, far_x) = (cx0 - light, cx0 + light);
    let (near_y, far_y) = (cy0 - light, cy0 + light);
    let mid_x = (near_x, far_x + light);
    let mid_y = (near_y, far_y + light);
    let span = |present: bool, span: (f32, f32)| if present { span } else { (0.0, 0.0) };

    if vert_double {
        for (rail_x, crossed) in [(near_x, arms[LEFT] == D), (far_x, arms[RIGHT] == D)] {
            emit_rail(
                [
                    span(arms[UP] != N, (y, cy0)),
                    span(!crossed, mid_y),
                    span(arms[DOWN] != N, (far_y, y + h)),
                ],
                (rail_x, light),
                false,
                color,
                out,
            );
        }
    } else if arms[UP] != N || arms[DOWN] != N {
        let t = thick(arms[UP].max_stroke(arms[DOWN]));
        let start = if arms[UP] != N { y } else { far_y };
        let end = if arms[DOWN] != N { y + h } else { cy0 };
        emit_rail(
            [(start, end), (0.0, 0.0), (0.0, 0.0)],
            (cx(t), t),
            false,
            color,
            out,
        );
    }

    if horiz_double {
        for (rail_y, crossed) in [(near_y, arms[UP] == D), (far_y, arms[DOWN] == D)] {
            emit_rail(
                [
                    span(arms[LEFT] != N, (x, cx0)),
                    span(!crossed, mid_x),
                    span(arms[RIGHT] != N, (far_x, x + w)),
                ],
                (rail_y, light),
                true,
                color,
                out,
            );
        }
    } else if arms[LEFT] != N || arms[RIGHT] != N {
        let t = thick(arms[LEFT].max_stroke(arms[RIGHT]));
        let start = if arms[LEFT] != N { x } else { far_x };
        let end = if arms[RIGHT] != N { x + w } else { cx0 };
        emit_rail(
            [(start, end), (0.0, 0.0), (0.0, 0.0)],
            (cy(t), t),
            true,
            color,
            out,
        );
    }
}

impl Stroke {
    /// The heavier of two arms, for the characters whose two collinear arms
    /// differ (╼ and friends draw one bar at the thicker weight).
    fn max_stroke(self, other: Stroke) -> Stroke {
        if other == H || self == N { other } else { self }
    }
}

/// Emit one rail: up to three ascending spans along its length, merged where
/// they touch, at a fixed (offset, thickness) across it. Zero-length spans are
/// the absent ones.
fn emit_rail(
    spans: [(f32, f32); 3],
    across: (f32, f32),
    horizontal: bool,
    color: Color,
    out: &mut Vec<(Rect, Color)>,
) {
    let mut current: Option<(f32, f32)> = None;
    for (start, end) in spans {
        if end <= start {
            continue;
        }
        current = match current {
            Some((s, e)) if start <= e => Some((s, e.max(end))),
            Some((s, e)) => {
                push_span(s, e, across, horizontal, color, out);
                Some((start, end))
            }
            None => Some((start, end)),
        };
    }
    if let Some((s, e)) = current {
        push_span(s, e, across, horizontal, color, out);
    }
}

fn push_span(
    start: f32,
    end: f32,
    across: (f32, f32),
    horizontal: bool,
    color: Color,
    out: &mut Vec<(Rect, Color)>,
) {
    let rect = if horizontal {
        [start, across.0, end - start, across.1]
    } else {
        [across.0, start, across.1, end - start]
    };
    out.push((rect, color));
}

/// A dashed bar: `count` dashes evenly spaced along `len`, each a third of its
/// step short of the next.
fn dashes(
    count: usize,
    start: f32,
    len: f32,
    across: (f32, f32),
    horizontal: bool,
    color: Color,
    out: &mut Vec<(Rect, Color)>,
) {
    let step = len / count as f32;
    let gap = (step / 3.0).round().max(1.0);
    for i in 0..count {
        let a = (start + i as f32 * step).round();
        let b = (start + (i + 1) as f32 * step - gap).round();
        if b > a {
            push_span(a, b, across, horizontal, color, out);
        }
    }
}

/// Dashes in U+2504–U+250B (triple, quadruple) and U+254C–U+254F (double).
fn dash_count(cp: u32) -> Option<usize> {
    match cp {
        0x2504..=0x2507 => Some(3),
        0x2508..=0x250B => Some(4),
        0x254C..=0x254F => Some(2),
        _ => None,
    }
}

/// A diagonal, as a staircase of one-pixel rows — rects are the only
/// primitive here, and at cell sizes the stair is what a hinted font draws
/// anyway. `down` runs top-left to bottom-right (╲).
fn diagonal(cell: Rect, thickness: f32, down: bool, color: Color, out: &mut Vec<(Rect, Color)>) {
    let [x, y, w, h] = cell;
    let steps = h.max(1.0) as usize;
    for i in 0..steps {
        let f = (i as f32 + 0.5) / h;
        let f = if down { f } else { 1.0 - f };
        out.push((
            [
                (x + f * (w - thickness)).round(),
                y + i as f32,
                thickness,
                1.0,
            ],
            color,
        ));
    }
}

/// U+2580–U+259F: halves, eighths, quadrants and shades. Shades are the full
/// cell at a fraction of the foreground's alpha, which is what they are for —
/// a dimmed background — and stays exact at any cell size.
fn block_element(cp: u32, cell: Rect, color: Color, out: &mut Vec<(Rect, Color)>) {
    let [x, y, w, h] = cell;
    let eighth = |extent: f32, n: u32| (extent * n as f32 / 8.0).round();
    match cp {
        // Upper half, upper eighth.
        0x2580 => out.push(([x, y, w, (h * 0.5).round()], color)),
        0x2594 => out.push(([x, y, w, eighth(h, 1)], color)),
        // Lower eighths, up to the full block.
        0x2581..=0x2588 => {
            let height = eighth(h, cp - 0x2580);
            out.push(([x, y + h - height, w, height], color));
        }
        // Left eighths: U+2589 is seven eighths, U+258F one.
        0x2589..=0x258F => out.push(([x, y, eighth(w, 0x2590 - cp), h], color)),
        0x2590 => {
            let width = (w * 0.5).round();
            out.push(([x + w - width, y, width, h], color));
        }
        0x2595 => {
            let width = eighth(w, 1);
            out.push(([x + w - width, y, width, h], color));
        }
        // Light, medium, dark shade.
        0x2591..=0x2593 => {
            let alpha = 0.25 * (cp - 0x2590) as f32;
            let [r, g, b, a] = color.0;
            out.push(([x, y, w, h], Color([r, g, b, a * alpha])));
        }
        // Quadrants, as a mask of upper-left, upper-right, lower-left,
        // lower-right.
        0x2596..=0x259F => {
            let mask = QUADRANTS[(cp - 0x2596) as usize];
            let half_w = (w * 0.5).round();
            let half_h = (h * 0.5).round();
            let quads = [
                [x, y, half_w, half_h],
                [x + half_w, y, w - half_w, half_h],
                [x, y + half_h, half_w, h - half_h],
                [x + half_w, y + half_h, w - half_w, h - half_h],
            ];
            for (i, rect) in quads.into_iter().enumerate() {
                if mask & (1 << i) != 0 {
                    out.push((rect, color));
                }
            }
        }
        _ => {}
    }
}

/// U+2596–U+259F as bitmasks of [upper-left, upper-right, lower-left,
/// lower-right].
static QUADRANTS: [u8; 10] = [
    0b0100, // ▖ lower left
    0b1000, // ▗ lower right
    0b0001, // ▘ upper left
    0b1101, // ▙ upper left and lower left and lower right
    0b1001, // ▚ upper left and lower right
    0b0111, // ▛ upper left and upper right and lower left
    0b1011, // ▜ upper left and upper right and lower right
    0b0010, // ▝ upper right
    0b0110, // ▞ upper right and lower left
    0b1110, // ▟ upper right and lower left and lower right
];

/// Every box-drawing character's arms, as [up, right, down, left], indexed by
/// `cp - 0x2500`. Transcribed from the Unicode names; the dashed characters
/// carry the arms of the solid line they dash (the dash count is separate) and
/// the diagonals U+2571–U+2573 have no arms at all.
///
/// The four arcs U+256D–U+2570 are drawn as their sharp-cornered equivalents:
/// a quarter circle is not a rect, and at cell sizes the difference is one
/// pixel of the corner.
#[rustfmt::skip]
static BOX_ARMS: [[Stroke; 4]; 0x80] = [
    [N, L, N, L], // ─ 2500 light horizontal
    [N, H, N, H], // ━ 2501 heavy horizontal
    [L, N, L, N], // │ 2502 light vertical
    [H, N, H, N], // ┃ 2503 heavy vertical
    [N, L, N, L], // ┄ 2504 light triple dash horizontal
    [N, H, N, H], // ┅ 2505 heavy triple dash horizontal
    [L, N, L, N], // ┆ 2506 light triple dash vertical
    [H, N, H, N], // ┇ 2507 heavy triple dash vertical
    [N, L, N, L], // ┈ 2508 light quadruple dash horizontal
    [N, H, N, H], // ┉ 2509 heavy quadruple dash horizontal
    [L, N, L, N], // ┊ 250A light quadruple dash vertical
    [H, N, H, N], // ┋ 250B heavy quadruple dash vertical
    [N, L, L, N], // ┌ 250C light down and right
    [N, H, L, N], // ┍ 250D down light and right heavy
    [N, L, H, N], // ┎ 250E down heavy and right light
    [N, H, H, N], // ┏ 250F heavy down and right
    [N, N, L, L], // ┐ 2510 light down and left
    [N, N, L, H], // ┑ 2511 down light and left heavy
    [N, N, H, L], // ┒ 2512 down heavy and left light
    [N, N, H, H], // ┓ 2513 heavy down and left
    [L, L, N, N], // └ 2514 light up and right
    [L, H, N, N], // ┕ 2515 up light and right heavy
    [H, L, N, N], // ┖ 2516 up heavy and right light
    [H, H, N, N], // ┗ 2517 heavy up and right
    [L, N, N, L], // ┘ 2518 light up and left
    [L, N, N, H], // ┙ 2519 up light and left heavy
    [H, N, N, L], // ┚ 251A up heavy and left light
    [H, N, N, H], // ┛ 251B heavy up and left
    [L, L, L, N], // ├ 251C light vertical and right
    [L, H, L, N], // ┝ 251D vertical light and right heavy
    [H, L, L, N], // ┞ 251E up heavy and right down light
    [L, L, H, N], // ┟ 251F down heavy and right up light
    [H, L, H, N], // ┠ 2520 vertical heavy and right light
    [H, H, L, N], // ┡ 2521 down light and right up heavy
    [L, H, H, N], // ┢ 2522 up light and right down heavy
    [H, H, H, N], // ┣ 2523 heavy vertical and right
    [L, N, L, L], // ┤ 2524 light vertical and left
    [L, N, L, H], // ┥ 2525 vertical light and left heavy
    [H, N, L, L], // ┦ 2526 up heavy and left down light
    [L, N, H, L], // ┧ 2527 down heavy and left up light
    [H, N, H, L], // ┨ 2528 vertical heavy and left light
    [H, N, L, H], // ┩ 2529 down light and left up heavy
    [L, N, H, H], // ┪ 252A up light and left down heavy
    [H, N, H, H], // ┫ 252B heavy vertical and left
    [N, L, L, L], // ┬ 252C light down and horizontal
    [N, L, L, H], // ┭ 252D left heavy and right down light
    [N, H, L, L], // ┮ 252E right heavy and left down light
    [N, H, L, H], // ┯ 252F down light and horizontal heavy
    [N, L, H, L], // ┰ 2530 down heavy and horizontal light
    [N, L, H, H], // ┱ 2531 right light and left down heavy
    [N, H, H, L], // ┲ 2532 left light and right down heavy
    [N, H, H, H], // ┳ 2533 heavy down and horizontal
    [L, L, N, L], // ┴ 2534 light up and horizontal
    [L, L, N, H], // ┵ 2535 left heavy and right up light
    [L, H, N, L], // ┶ 2536 right heavy and left up light
    [L, H, N, H], // ┷ 2537 up light and horizontal heavy
    [H, L, N, L], // ┸ 2538 up heavy and horizontal light
    [H, L, N, H], // ┹ 2539 right light and left up heavy
    [H, H, N, L], // ┺ 253A left light and right up heavy
    [H, H, N, H], // ┻ 253B heavy up and horizontal
    [L, L, L, L], // ┼ 253C light vertical and horizontal
    [L, L, L, H], // ┽ 253D left heavy and right vertical light
    [L, H, L, L], // ┾ 253E right heavy and left vertical light
    [L, H, L, H], // ┿ 253F vertical light and horizontal heavy
    [H, L, L, L], // ╀ 2540 up heavy and down horizontal light
    [L, L, H, L], // ╁ 2541 down heavy and up horizontal light
    [H, L, H, L], // ╂ 2542 vertical heavy and horizontal light
    [H, L, L, H], // ╃ 2543 left up heavy and right down light
    [H, H, L, L], // ╄ 2544 right up heavy and left down light
    [L, L, H, H], // ╅ 2545 left down heavy and right up light
    [L, H, H, L], // ╆ 2546 right down heavy and left up light
    [H, H, L, H], // ╇ 2547 down light and up horizontal heavy
    [L, H, H, H], // ╈ 2548 up light and down horizontal heavy
    [H, L, H, H], // ╉ 2549 right light and left vertical heavy
    [H, H, H, L], // ╊ 254A left light and right vertical heavy
    [H, H, H, H], // ╋ 254B heavy vertical and horizontal
    [N, L, N, L], // ╌ 254C light double dash horizontal
    [N, H, N, H], // ╍ 254D heavy double dash horizontal
    [L, N, L, N], // ╎ 254E light double dash vertical
    [H, N, H, N], // ╏ 254F heavy double dash vertical
    [N, D, N, D], // ═ 2550 double horizontal
    [D, N, D, N], // ║ 2551 double vertical
    [N, D, L, N], // ╒ 2552 down single and right double
    [N, L, D, N], // ╓ 2553 down double and right single
    [N, D, D, N], // ╔ 2554 double down and right
    [N, N, L, D], // ╕ 2555 down single and left double
    [N, N, D, L], // ╖ 2556 down double and left single
    [N, N, D, D], // ╗ 2557 double down and left
    [L, D, N, N], // ╘ 2558 up single and right double
    [D, L, N, N], // ╙ 2559 up double and right single
    [D, D, N, N], // ╚ 255A double up and right
    [L, N, N, D], // ╛ 255B up single and left double
    [D, N, N, L], // ╜ 255C up double and left single
    [D, N, N, D], // ╝ 255D double up and left
    [L, D, L, N], // ╞ 255E vertical single and right double
    [D, L, D, N], // ╟ 255F vertical double and right single
    [D, D, D, N], // ╠ 2560 double vertical and right
    [L, N, L, D], // ╡ 2561 vertical single and left double
    [D, N, D, L], // ╢ 2562 vertical double and left single
    [D, N, D, D], // ╣ 2563 double vertical and left
    [N, D, L, D], // ╤ 2564 down single and horizontal double
    [N, L, D, L], // ╥ 2565 down double and horizontal single
    [N, D, D, D], // ╦ 2566 double down and horizontal
    [L, D, N, D], // ╧ 2567 up single and horizontal double
    [D, L, N, L], // ╨ 2568 up double and horizontal single
    [D, D, N, D], // ╩ 2569 double up and horizontal
    [L, D, L, D], // ╪ 256A vertical single and horizontal double
    [D, L, D, L], // ╫ 256B vertical double and horizontal single
    [D, D, D, D], // ╬ 256C double vertical and horizontal
    [N, L, L, N], // ╭ 256D light arc down and right
    [N, N, L, L], // ╮ 256E light arc down and left
    [L, N, N, L], // ╯ 256F light arc up and left
    [L, L, N, N], // ╰ 2570 light arc up and right
    [N, N, N, N], // ╱ 2571 light diagonal upper right to lower left
    [N, N, N, N], // ╲ 2572 light diagonal upper left to lower right
    [N, N, N, N], // ╳ 2573 light diagonal cross
    [N, N, N, L], // ╴ 2574 light left
    [L, N, N, N], // ╵ 2575 light up
    [N, L, N, N], // ╶ 2576 light right
    [N, N, L, N], // ╷ 2577 light down
    [N, N, N, H], // ╸ 2578 heavy left
    [H, N, N, N], // ╹ 2579 heavy up
    [N, H, N, N], // ╺ 257A heavy right
    [N, N, H, N], // ╻ 257B heavy down
    [N, H, N, L], // ╼ 257C light left and heavy right
    [L, N, H, N], // ╽ 257D light up and heavy down
    [N, L, N, H], // ╾ 257E heavy left and light right
    [H, N, L, N], // ╿ 257F heavy up and light down
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FONT_DEJAVU_SANS_MONO, font_system_from_fonts};

    const FG: Color = Color::WHITE;
    const BG: Color = Color([0.1, 0.1, 0.2, 1.0]);

    fn mono() -> (FontSystem, GridFont) {
        let mut font_system = font_system_from_fonts(&[FONT_DEJAVU_SANS_MONO]);
        let font = GridFont::new(&mut font_system, Family::Name("DejaVu Sans Mono"), 16.0);
        (font_system, font)
    }

    fn build(grid: &TermGrid) -> GridScene {
        let (mut font_system, mut font) = mono();
        let mut scene = GridScene::new();
        grid.build(&mut font, &mut font_system, [0.0, 0.0], &mut scene);
        scene
    }

    // ---- The model ----

    #[test]
    fn wide_chars_occupy_two_cells() {
        let mut grid = TermGrid::new(8, 1, 0);
        assert_eq!(grid.print(0, 0, "日本a", FG, None), 5);
        assert_eq!(grid.cell(0, 0).unwrap().ch, '日');
        assert!(grid.cell(1, 0).unwrap().is_continuation());
        assert_eq!(grid.cell(2, 0).unwrap().ch, '本');
        assert!(grid.cell(3, 0).unwrap().is_continuation());
        assert_eq!(grid.cell(4, 0).unwrap().ch, 'a');
        // A continuation carries its cell's colors so background runs merge.
        let mut grid = TermGrid::new(4, 1, 0);
        grid.print(0, 0, "日", FG, Some(BG));
        assert_eq!(grid.cell(1, 0).unwrap().bg, Some(BG));
    }

    #[test]
    fn a_wide_char_that_does_not_fit_leaves_the_cell_blank() {
        let mut grid = TermGrid::new(2, 1, 0);
        assert_eq!(grid.print(0, 0, "a日b", FG, None), 2);
        assert_eq!(grid.cell(0, 0).unwrap().ch, 'a');
        // The straddling cell is blanked, and printing stops there — 'b' does
        // not slide into the hole.
        assert_eq!(grid.cell(1, 0).unwrap().ch, ' ');
    }

    #[test]
    fn print_clips_at_the_row_end_and_never_wraps() {
        let mut grid = TermGrid::new(5, 2, 0);
        assert_eq!(grid.print(2, 0, "abcdefgh", FG, None), 5);
        assert_eq!(grid.cell(4, 0).unwrap().ch, 'c');
        // Nothing wrapped onto the next row.
        assert_eq!(grid.cell(0, 1).unwrap().ch, ' ');
        // Printing past the last column writes nothing at all.
        assert_eq!(grid.print(9, 1, "x", FG, None), 9);
        assert!(grid.cell(9, 1).is_none());
    }

    #[test]
    fn clusters_ride_along_with_their_base_cell() {
        let mut grid = TermGrid::new(4, 1, 0);
        grid.print(0, 0, "e\u{301}x", FG, None); // e + combining acute
        assert_eq!(grid.cell(0, 0).unwrap().ch, 'e');
        assert_eq!(grid.cluster(0, 0), Some("e\u{301}"));
        assert_eq!(grid.cell(1, 0).unwrap().ch, 'x');
        assert_eq!(grid.cluster(1, 0), None);
        // Overwriting the cell drops the cluster with it.
        grid.set_cell(0, 0, Cell::new('z', FG));
        assert_eq!(grid.cluster(0, 0), None);
    }

    #[test]
    fn scroll_up_fills_the_scrollback_ring() {
        let mut grid = TermGrid::new(4, 2, 3);
        for i in 0..6 {
            grid.push_line(&format!("l{i}"), FG, None);
        }
        // Six lines pushed, two on screen, three in a ring that holds three.
        assert_eq!(grid.scrollback_len(), 3);
        assert_eq!(grid.cell(1, 1).unwrap().ch, '5');
        assert_eq!(grid.cell(1, 0).unwrap().ch, '4');
        grid.set_view_offset(3);
        assert_eq!(grid.view_cell(1, 0).unwrap().ch, '1');
        assert_eq!(grid.view_cell(1, 1).unwrap().ch, '2');
        // Past the ring the offset clamps rather than reading garbage.
        assert_eq!(grid.set_view_offset(99), 3);
    }

    #[test]
    fn the_viewport_follows_its_text_out_of_the_ring() {
        let mut grid = TermGrid::new(4, 2, 3);
        for i in 0..5 {
            grid.push_line(&format!("l{i}"), FG, None);
        }
        grid.set_view_offset(3);
        assert_eq!(grid.view_cell(1, 0).unwrap().ch, '0');
        // One more line: the ring drops "l0", so the same view offset would
        // now show a different line — the offset moves with the text instead,
        // and the top of the viewport stays on "l1".
        grid.push_line("l5", FG, None);
        assert_eq!(grid.scrollback_len(), 3);
        assert_eq!(grid.view_offset(), 3);
        assert_eq!(grid.view_cell(1, 0).unwrap().ch, '1');
        // Rows past the scrollback come from the screen, unshifted.
        grid.set_view_offset(1);
        assert_eq!(grid.view_cell(1, 0).unwrap().ch, '3');
        assert_eq!(grid.view_cell(1, 1).unwrap().ch, '4');
    }

    #[test]
    fn a_zero_limit_ring_keeps_nothing() {
        let mut grid = TermGrid::new(4, 2, 0);
        for i in 0..4 {
            grid.push_line(&format!("l{i}"), FG, None);
        }
        assert_eq!(grid.scrollback_len(), 0);
        assert_eq!(grid.set_view_offset(2), 0);
        assert_eq!(grid.view_cell(1, 0).unwrap().ch, '2');
    }

    #[test]
    fn resize_keeps_the_bottom_and_banks_the_top() {
        let mut grid = TermGrid::new(6, 3, 10);
        for row in 0..3 {
            grid.print(0, row, &format!("row{row}"), FG, None);
        }
        grid.resize(3, 2);
        assert_eq!(grid.cols(), 3);
        assert_eq!(grid.rows(), 2);
        assert_eq!(grid.scrollback_len(), 1);
        // Columns truncate, the top row goes to scrollback.
        assert_eq!(grid.cell(2, 0).unwrap().ch, 'w');
        assert!(grid.cell(3, 0).is_none());
        grid.set_view_offset(1);
        assert_eq!(grid.view_cell(0, 0).unwrap().ch, 'r');
    }

    #[test]
    fn dirty_tracks_content_changes() {
        let mut grid = TermGrid::new(4, 2, 4);
        assert!(grid.take_dirty());
        assert!(!grid.take_dirty());
        grid.set_cell(0, 0, Cell::new('x', FG));
        assert!(grid.take_dirty());
        grid.scroll_up(1);
        assert!(grid.take_dirty());
        grid.set_view_offset(1);
        assert!(grid.take_dirty());
        assert!(!grid.dirty());
    }

    // ---- East Asian Width ----

    #[test]
    fn eaw_ranges_are_sorted_and_disjoint() {
        for pair in WIDE_RANGES.windows(2) {
            assert!(pair[0].0 <= pair[0].1);
            // Merged: neighbours never touch, or the table would be redundant.
            assert!(
                pair[0].1 + 1 < pair[1].0,
                "{:x?} then {:x?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn eaw_widths_match_known_characters() {
        for ch in ['a', ' ', 'é', '→', '│', '█', '\u{2026}', 'Ω', 'ب'] {
            assert_eq!(char_width(ch), 1, "{ch:?} should be narrow");
        }
        // U+FF71 ｱ is halfwidth katakana, class H: narrow, unlike its
        // fullwidth twin U+30A2.
        assert_eq!(char_width('ｱ'), 1);
        assert_eq!(char_width('ア'), 2);
        for ch in ['日', '本', '한', '\u{FF21}', '⌚', '🚀', '🧑', '\u{3000}'] {
            assert_eq!(char_width(ch), 2, "{ch:?} should be wide");
        }
        // Range edges, from the table's own boundaries.
        assert_eq!(char_width('\u{10FF}'), 1);
        assert_eq!(char_width('\u{1100}'), 2);
        assert_eq!(char_width('\u{115F}'), 2);
        assert_eq!(char_width('\u{1160}'), 1);
        assert_eq!(char_width('\u{FF60}'), 2);
        assert_eq!(char_width('\u{FF61}'), 1);
        // An emoji presentation selector widens its cluster.
        assert_eq!(cluster_width("⚠\u{FE0F}"), 2);
        assert_eq!(cluster_width("⚠"), 1);
    }

    // ---- Translation ----

    #[test]
    fn backgrounds_merge_into_runs() {
        let mut grid = TermGrid::new(10, 1, 0);
        grid.print(0, 0, "aaaa", FG, Some(BG));
        grid.print(4, 0, "bb", FG, Some(Color::BLACK));
        grid.print(6, 0, "cc", FG, Some(BG));
        let scene = build(&grid);
        let cell_w = mono().1.cell_size()[0];
        assert_eq!(scene.rects.len(), 3);
        assert_eq!(scene.rects[0].0[2], 4.0 * cell_w);
        assert_eq!(scene.rects[1].0[2], 2.0 * cell_w);
        assert_eq!(scene.rects[2].0[0], 6.0 * cell_w);
        // Cells with no background contribute nothing at all.
        let mut plain = TermGrid::new(10, 1, 0);
        plain.print(0, 0, "abc", FG, None);
        assert!(build(&plain).rects.is_empty());
    }

    #[test]
    fn glyphs_come_from_the_charmap_and_land_on_cell_origins() {
        let (mut font_system, mut font) = mono();
        let mut grid = TermGrid::new(4, 1, 0);
        grid.print(0, 0, "M M", FG, None);
        let mut scene = GridScene::new();
        grid.build(&mut font, &mut font_system, [10.0, 20.0], &mut scene);
        // The space draws nothing, the two Ms do.
        assert_eq!(scene.glyphs.len(), 2);
        let [cell_w, _] = font.cell_size();
        // Cells are whole pixels and the advance is not, so a glyph is
        // centered in its cell — under half a pixel from the cell origin.
        assert!((scene.glyphs[0].pos[0] - 10.0).abs() < 0.5);
        assert_eq!(
            scene.glyphs[1].pos[0] - scene.glyphs[0].pos[0],
            2.0 * cell_w
        );
        assert_eq!(scene.glyphs[0].pos[1], 20.0 + font.ascent());
        // The id is the charmap's, with no shaper in between.
        let expected = font_system
            .get_font(font.faces[0].id, fontdb::Weight::NORMAL)
            .unwrap()
            .as_swash()
            .charmap()
            .map('M');
        assert_eq!(scene.glyphs[0].glyph_id, expected);
    }

    #[test]
    fn continuations_and_procedural_cells_never_reach_the_font() {
        let mut grid = TermGrid::new(4, 1, 0);
        grid.print(0, 0, "█▒╬", FG, None);
        let scene = build(&grid);
        assert!(scene.glyphs.is_empty(), "block drawing must not shape");
        // Full block, shade, and the four corner pieces per axis of ╬.
        assert_eq!(scene.rects.len(), 10);
        assert_eq!(scene.rects[1].1.0[3], 0.5, "medium shade is half alpha");
    }

    #[test]
    fn a_charmap_miss_takes_the_fallback_lane() {
        let (mut font_system, mut font) = mono();
        // DejaVu Sans Mono has no CJK; the cell shapes through cosmic-text,
        // which finds a font that does (or drops the glyph on a box with no
        // CJK font at all — either way it must not draw .notdef from the
        // charmap).
        assert!(font.glyph(&mut font_system, 0, '日').is_none());
        let run = font.fallback(&mut font_system, 0, "日").clone();
        for glyph in &run.glyphs {
            assert_ne!(glyph.glyph_id, 0);
        }
        // The cache answers the second time without reshaping.
        assert!(font.faces[0].fallback.contains_key("日"));
    }

    #[test]
    fn decorations_merge_across_cells() {
        let mut grid = TermGrid::new(8, 1, 0);
        grid.print_styled(0, 0, "abc", FG, None, CellStyle::UNDERLINE);
        grid.print_styled(
            3,
            0,
            "de",
            FG,
            None,
            CellStyle::UNDERLINE | CellStyle::STRIKE,
        );
        let scene = build(&grid);
        let cell_w = mono().1.cell_size()[0];
        let underlines: Vec<_> = scene
            .decorations
            .iter()
            .filter(|(_, kind, _)| *kind == DecorationKind::Underline)
            .collect();
        assert_eq!(underlines.len(), 1, "one run across all five cells");
        assert_eq!(underlines[0].0[2], 5.0 * cell_w);
        assert_eq!(scene.decorations.len(), 2);
    }

    // ---- Procedural geometry ----

    /// Rects for one cell of a row of `ch`, at cell `col` of an 8×16 grid.
    fn cell_rects(ch: char, col: f32) -> Vec<(Rect, Color)> {
        let mut out = Vec::new();
        assert!(push_procedural(
            ch,
            [col * 8.0, 0.0, 8.0, 16.0],
            FG,
            &mut out
        ));
        out
    }

    #[test]
    fn box_rects_land_on_whole_pixels() {
        for ch in ['─', '━', '│', '╬', '╔', '┼', '╤', '▚', '▄', '╌'] {
            for (rect, _) in cell_rects(ch, 3.0) {
                for value in rect {
                    assert_eq!(value.fract(), 0.0, "{ch:?} rect {rect:?}");
                }
                assert!(rect[2] > 0.0 && rect[3] > 0.0, "{ch:?} rect {rect:?}");
            }
        }
    }

    /// Merge the x spans of the rects that sit at `y`, in ascending order.
    fn coverage_at(rects: &[(Rect, Color)], y: f32) -> Vec<(f32, f32)> {
        let mut spans: Vec<(f32, f32)> = rects
            .iter()
            .filter(|(r, _)| r[1] <= y && y < r[1] + r[3])
            .map(|(r, _)| (r[0], r[0] + r[2]))
            .collect();
        spans.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut merged: Vec<(f32, f32)> = Vec::new();
        for span in spans {
            match merged.last_mut() {
                Some(last) if span.0 <= last.1 => last.1 = last.1.max(span.1),
                _ => merged.push(span),
            }
        }
        merged
    }

    #[test]
    fn horizontal_runs_join_without_gaps() {
        for ch in ['─', '━', '═', '┼', '╬', '╤', '┯'] {
            let mut run = cell_rects(ch, 0.0);
            run.extend(cell_rects(ch, 1.0));
            // Every horizontal stroke crosses the cell boundary at x = 8 as
            // one merged span. A seam there is exactly the artifact font
            // outlines give at most sizes, and the reason these are drawn.
            let left = cell_rects(ch, 0.0);
            let rows: Vec<f32> = left.iter().map(|(r, _)| r[1] + 0.5).collect();
            let mut crossings = 0;
            for y in rows {
                // Only the rows where the left cell's own stroke reaches its
                // right edge: those are the ones with a neighbour to meet.
                if !coverage_at(&left, y).iter().any(|s| s.1 == 8.0) {
                    continue;
                }
                let spans = coverage_at(&run, y);
                assert!(
                    spans.iter().any(|s| s.0 < 8.0 && s.1 > 8.0),
                    "{ch:?} seam at the cell boundary, y {y}: {spans:?}"
                );
                crossings += 1;
            }
            assert!(crossings > 0, "{ch:?} drew no horizontal stroke");
        }
    }

    #[test]
    fn corners_close_and_crosses_reach_every_edge() {
        // ┌ covers the corner: the right arm and the down arm overlap.
        let corner = cell_rects('┌', 0.0);
        assert_eq!(corner.len(), 2);
        let right = corner.iter().find(|(r, _)| r[2] > r[3]).unwrap().0;
        let down = corner.iter().find(|(r, _)| r[3] > r[2]).unwrap().0;
        assert_eq!(right[0] + right[2], 8.0);
        assert_eq!(down[1] + down[3], 16.0);
        assert!(right[0] <= down[0] && down[0] + down[2] <= right[0] + right[2]);
        assert!(down[1] <= right[1] && right[1] + right[3] <= down[1] + down[3]);

        // ┼ reaches all four edges.
        let cross = cell_rects('┼', 0.0);
        let min_x = cross.iter().map(|(r, _)| r[0]).fold(f32::MAX, f32::min);
        let max_x = cross.iter().map(|(r, _)| r[0] + r[2]).fold(0.0, f32::max);
        let min_y = cross.iter().map(|(r, _)| r[1]).fold(f32::MAX, f32::min);
        let max_y = cross.iter().map(|(r, _)| r[1] + r[3]).fold(0.0, f32::max);
        assert_eq!([min_x, max_x, min_y, max_y], [0.0, 8.0, 0.0, 16.0]);
    }

    #[test]
    fn double_junctions_break_only_where_a_double_crosses() {
        // ╬: every rail is broken by the perpendicular pair, so it is four
        // corner pieces per axis.
        assert_eq!(cell_rects('╬', 0.0).len(), 8);
        // ╠: the left rail is continuous (nothing crosses it), the right rail
        // is broken in two, and two horizontal stubs go right.
        assert_eq!(cell_rects('╠', 0.0).len(), 5);
        // ═ and ║ are two full-length rails.
        assert_eq!(cell_rects('═', 0.0).len(), 2);
        assert_eq!(cell_rects('║', 0.0).len(), 2);
        // ╪: a single vertical passes straight through both rails.
        let tee = cell_rects('╪', 0.0);
        assert_eq!(tee.len(), 3);
        let vertical = tee.iter().find(|(r, _)| r[3] > r[2]).unwrap().0;
        assert_eq!([vertical[1], vertical[1] + vertical[3]], [0.0, 16.0]);
        // ╤: the single arm starts at the lower rail and runs to the bottom.
        let down = cell_rects('╤', 0.0);
        let stem = down.iter().find(|(r, _)| r[3] > r[2]).unwrap().0;
        assert_eq!(stem[1] + stem[3], 16.0);
        assert!(stem[1] > 8.0);
    }

    #[test]
    fn block_elements_fill_their_fraction_of_the_cell() {
        let full = cell_rects('█', 0.0);
        assert_eq!(full[0].0, [0.0, 0.0, 8.0, 16.0]);
        // Lower eighths grow downward from the bottom edge.
        let quarter = cell_rects('▄', 0.0)[0].0;
        assert_eq!(quarter, [0.0, 8.0, 8.0, 8.0]);
        let one_eighth = cell_rects('▁', 0.0)[0].0;
        assert_eq!(one_eighth, [0.0, 14.0, 8.0, 2.0]);
        // Left eighths grow rightward from the left edge.
        assert_eq!(cell_rects('▌', 0.0)[0].0, [0.0, 0.0, 4.0, 16.0]);
        assert_eq!(cell_rects('▏', 0.0)[0].0, [0.0, 0.0, 1.0, 16.0]);
        // Halves meet exactly.
        let upper = cell_rects('▀', 0.0)[0].0;
        let lower = cell_rects('▄', 0.0)[0].0;
        assert_eq!(upper[1] + upper[3], lower[1]);
        // Quadrants tile the cell.
        let quads = cell_rects('▟', 0.0);
        assert_eq!(quads.len(), 3);
        let area: f32 = quads.iter().map(|(r, _)| r[2] * r[3]).sum();
        assert_eq!(area, 3.0 * 4.0 * 8.0);
        // Shades are the whole cell, dimmed.
        for (i, ch) in ['░', '▒', '▓'].into_iter().enumerate() {
            let shade = cell_rects(ch, 0.0);
            assert_eq!(shade[0].0, [0.0, 0.0, 8.0, 16.0]);
            assert_eq!(shade[0].1.0[3], 0.25 * (i + 1) as f32);
        }
    }

    #[test]
    fn dashes_and_diagonals_stay_inside_the_cell() {
        assert_eq!(cell_rects('┄', 0.0).len(), 3);
        assert_eq!(cell_rects('┈', 0.0).len(), 4);
        assert_eq!(cell_rects('╌', 0.0).len(), 2);
        assert_eq!(cell_rects('┊', 0.0).len(), 4);
        for ch in ['╱', '╲', '╳'] {
            for (rect, _) in cell_rects(ch, 0.0) {
                assert!(rect[0] >= 0.0 && rect[0] + rect[2] <= 8.0, "{ch:?}");
                assert!(rect[1] >= 0.0 && rect[1] + rect[3] <= 16.0, "{ch:?}");
            }
        }
        // The cross is both diagonals.
        assert_eq!(cell_rects('╳', 0.0).len(), 2 * cell_rects('╱', 0.0).len());
    }

    // ---- On the GPU ----

    #[test]
    fn a_grid_draws_through_a_retained_block() {
        let (device, queue) = crate::testing::gpu();
        let mut renderer = TextRenderer::new(device, crate::testing::FORMAT);
        let (mut font_system, mut font) = mono();
        let block = renderer.create_block();
        let mut scene = GridScene::new();

        let mut grid = TermGrid::new(6, 2, 8);
        grid.print(0, 0, "Wm", FG, None);
        grid.print(0, 1, "██ ", Color([1.0, 0.0, 0.0, 1.0]), Some(BG));

        renderer.begin_frame();
        grid.render(
            &mut renderer,
            queue,
            &mut font_system,
            &mut font,
            &mut scene,
            block,
            [1.0, 1.0],
        );
        let [cell_w, cell_h] = font.cell_size();
        let pixels = crate::testing::render_pixels(&mut renderer, 64, 64);
        // The center of a cell, block offset included.
        let cell = |col: f32, row: f32| {
            let x = (1.0 + (col + 0.5) * cell_w) as usize;
            let y = (1.0 + (row + 0.5) * cell_h) as usize;
            let i = (y * 64 + x) * 4;
            [pixels[i], pixels[i + 1], pixels[i + 2]]
        };
        // Both full blocks are opaque red over their whole cell.
        assert_eq!(cell(0.0, 1.0), [255, 0, 0]);
        assert_eq!(cell(1.0, 1.0), [255, 0, 0]);
        // The third cell is a space over the same background run.
        assert_eq!(cell(2.0, 1.0), [25, 25, 51]);
        // Nothing at all past the printed cells.
        assert_eq!(cell(4.0, 1.0), [0, 0, 0]);
        // And the glyph row put ink down, none of it from a rect.
        let ink: u32 = (0..cell_h as usize)
            .flat_map(|y| (0..(2.0 * cell_w) as usize).map(move |x| (x, y)))
            .map(|(x, y)| {
                let i = (y * 64 + x) * 4;
                pixels[i] as u32 + pixels[i + 1] as u32 + pixels[i + 2] as u32
            })
            .sum();
        assert!(ink > 0, "the charmap glyphs drew nothing");
        assert!(renderer.upload_stats().instances > 0);
    }

    #[test]
    fn non_procedural_chars_are_left_to_the_font() {
        let mut out = Vec::new();
        for ch in ['a', '⌘', '\u{24FF}', '⯐'] {
            assert!(!push_procedural(ch, [0.0, 0.0, 8.0, 16.0], FG, &mut out));
        }
        assert!(out.is_empty());
    }
}
