use faf_text::TextView;
use faf_text::cosmic_text::{Affinity, Attrs, Cursor, Family, Metrics};

/// A ZWJ emoji cluster: one grapheme, 18 bytes, one shaped glyph.
const FAMILY: &str = "👩‍👩‍👧";

fn view(text: &str) -> (faf_text::FontSystem, TextView) {
    let mut fs = faf_text::font_system_from_fonts(&[faf_text::FONT_DEJAVU_SANS]);
    let mut v = TextView::new(&mut fs, Metrics::new(16.0, 24.0));
    v.set_text(&mut fs, text, &Attrs::new().family(Family::SansSerif));
    (fs, v)
}

/// Same, with a layout width narrow enough to soft-wrap.
fn wrapped_view(text: &str, width: f32) -> (faf_text::FontSystem, TextView) {
    let (mut fs, mut v) = view(text);
    v.set_size(&mut fs, Some(width), None);
    (fs, v)
}

fn at(c: Cursor) -> (usize, usize) {
    (c.line, c.index)
}

#[test]
fn hit_testing_round_trips() {
    let (_fs, v) = view("hello world");
    let start = v.hit(0.0, 12.0).unwrap();
    assert_eq!((start.line, start.index), (0, 0));
    let past_end = v.hit(10_000.0, 12.0).unwrap();
    assert_eq!(past_end.line, 0);
    assert_eq!(past_end.index, "hello world".len());
}

#[test]
fn selection_rects_cover_selected_text_only() {
    let (_fs, v) = view("hello world");
    let rects = v.selection_rects(Cursor::new(0, 0), Cursor::new(0, 5));
    assert_eq!(rects.len(), 1);
    let full = v.selection_rects(Cursor::new(0, 0), Cursor::new(0, 11));
    assert!(full[0][2] > rects[0][2], "wider selection, wider rect");
    // order of the cursor pair must not matter
    let rev = v.selection_rects(Cursor::new(0, 5), Cursor::new(0, 0));
    assert_eq!(rects, rev);
}

#[test]
fn selection_skips_lines_outside_cursor_range() {
    let (_fs, v) = view("first line\nsecond line\nthird line");
    // Selecting within line 0 must produce no rects on lines 1 and 2.
    let rects = v.selection_rects(Cursor::new(0, 0), Cursor::new(0, 5));
    assert_eq!(rects.len(), 1);
    assert!(rects[0][1] < 24.0, "rect must sit on the first line");
}

#[test]
fn multi_line_selection_spans_lines() {
    let (_fs, v) = view("first line\nsecond line\nthird line");
    let rects = v.selection_rects(Cursor::new(0, 6), Cursor::new(2, 5));
    assert_eq!(rects.len(), 3);
}

#[test]
fn cursor_rect_tracks_the_caret_along_a_line() {
    let (_fs, mut v) = view("hello world");
    v.pos = [10.0, 20.0];
    let start = v.cursor_rect(Cursor::new(0, 0)).unwrap();
    assert_eq!([start[0], start[1]], [10.0, 20.0], "caret at the origin");
    assert_eq!(start[3], 24.0, "caret is one line tall");
    let mid = v.cursor_rect(Cursor::new(0, 5)).unwrap();
    let end = v.cursor_rect(Cursor::new(0, 11)).unwrap();
    assert!(start[0] < mid[0] && mid[0] < end[0], "x grows with index");
    assert_eq!(start[1], end[1], "one line, one y");
}

#[test]
fn cursor_rect_follows_hard_line_breaks() {
    let (_fs, v) = view("first\nsecond");
    let a = v.cursor_rect(Cursor::new(0, 0)).unwrap();
    let b = v.cursor_rect(Cursor::new(1, 0)).unwrap();
    assert_eq!(b[1] - a[1], 24.0, "second line sits one line height down");
    assert_eq!(a[0], b[0], "both at the left margin");
    assert!(v.cursor_rect(Cursor::new(7, 0)).is_none(), "no such line");
}

#[test]
fn cursor_rect_is_affinity_aware_at_a_wrap_boundary() {
    // Wraps mid-word, so byte 6 ends row 0 and starts row 1.
    let (_fs, v) = wrapped_view("aaaaaaaaaaaaaaaaaaaa", 60.0);
    let before = v
        .cursor_rect(Cursor::new_with_affinity(0, 6, Affinity::Before))
        .unwrap();
    let after = v
        .cursor_rect(Cursor::new_with_affinity(0, 6, Affinity::After))
        .unwrap();
    assert_eq!(before[1], 0.0, "Before keeps the caret on the first row");
    assert!(before[0] > 0.0, "at the end of the first row");
    assert_eq!(after[1], 24.0, "After moves it to the next row");
    assert_eq!(after[0], 0.0, "at the start of the next row");
}

#[test]
fn cursor_rect_positions_multi_byte_and_emoji_clusters() {
    let (_fs, v) = view(&format!("é{FAMILY}家"));
    let origin = v.cursor_rect(Cursor::new(0, 0)).unwrap();
    let after_e = v.cursor_rect(Cursor::new(0, 2)).unwrap();
    let after_family = v.cursor_rect(Cursor::new(0, 20)).unwrap();
    let end = v.cursor_rect(Cursor::new(0, 23)).unwrap();
    assert!(origin[0] < after_e[0]);
    assert!(
        after_family[0] - after_e[0] > after_e[0] - origin[0],
        "the emoji cluster is wider than é"
    );
    assert!(end[0] > after_family[0]);
}

#[test]
fn move_left_and_right_step_by_grapheme_in_ascii() {
    let (_fs, v) = view("hello");
    let mut c = Cursor::new(0, 0);
    for expected in 1..=5 {
        c = v.move_right(c);
        assert_eq!(at(c), (0, expected));
        assert_eq!(c.affinity, Affinity::Before);
    }
    assert_eq!(at(v.move_right(c)), (0, 5), "clamped at the end");
    for expected in (0..5).rev() {
        c = v.move_left(c);
        assert_eq!(at(c), (0, expected));
        assert_eq!(c.affinity, Affinity::After);
    }
    assert_eq!(at(v.move_left(c)), (0, 0), "clamped at the start");
}

#[test]
fn move_left_and_right_step_over_multi_byte_scalars() {
    let (_fs, v) = view("é家x");
    let mut c = Cursor::new(0, 0);
    c = v.move_right(c);
    assert_eq!(at(c), (0, 2), "é is two bytes");
    c = v.move_right(c);
    assert_eq!(at(c), (0, 5), "家 is three bytes");
    c = v.move_right(c);
    assert_eq!(at(c), (0, 6));
    c = v.move_left(c);
    assert_eq!(at(c), (0, 5));
    c = v.move_left(c);
    assert_eq!(at(c), (0, 2));
    assert_eq!(at(v.move_left(c)), (0, 0));
}

#[test]
fn move_left_and_right_treat_a_zwj_cluster_as_one_step() {
    let text = format!("a{FAMILY}b");
    let (_fs, v) = view(&text);
    assert_eq!(FAMILY.len(), 18, "ZWJ sequence is 18 bytes");
    let c = v.move_right(Cursor::new(0, 1));
    assert_eq!(at(c), (0, 19), "one motion crosses the whole cluster");
    assert_eq!(at(v.move_left(c)), (0, 1), "and back in one motion");
    assert_eq!(at(v.move_right(c)), (0, 20), "then the trailing b");
}

#[test]
fn horizontal_motion_crosses_hard_line_breaks() {
    let (_fs, v) = view("ab\ncd");
    let c = v.move_right(Cursor::new(0, 2));
    assert_eq!(at(c), (1, 0), "past the end of a line is the next line");
    assert_eq!(at(v.move_left(c)), (0, 2), "and back");
    assert_eq!(at(v.move_right(Cursor::new(1, 2))), (1, 2), "end of text");
}

#[test]
fn word_motion_skips_whitespace_and_punctuation() {
    let (_fs, v) = view("hello world  foo");
    let mut c = Cursor::new(0, 0);
    c = v.move_word_right(c);
    assert_eq!(at(c), (0, 5), "end of the first word");
    assert_eq!(c.affinity, Affinity::Before);
    c = v.move_word_right(c);
    assert_eq!(at(c), (0, 11), "double space collapsed on the way out");
    c = v.move_word_right(c);
    assert_eq!(at(c), (0, 16));
    assert_eq!(at(v.move_word_right(c)), (0, 16), "clamped at the end");

    c = v.move_word_left(c);
    assert_eq!(at(c), (0, 13), "start of the last word");
    assert_eq!(c.affinity, Affinity::After);
    c = v.move_word_left(c);
    assert_eq!(at(c), (0, 6));
    c = v.move_word_left(c);
    assert_eq!(at(c), (0, 0));
    assert_eq!(at(v.move_word_left(c)), (0, 0), "clamped at the start");
}

#[test]
fn word_motion_handles_multi_byte_and_emoji() {
    // "wörld" is 6 bytes; the emoji is its own word-ish run.
    let (_fs, v) = view(&format!("héllo wörld {FAMILY} x"));
    let mut c = Cursor::new(0, 0);
    c = v.move_word_right(c);
    assert_eq!(at(c), (0, 6), "héllo ends after 6 bytes");
    c = v.move_word_right(c);
    assert_eq!(at(c), (0, 13), "wörld ends after 6 more plus the space");
    c = v.move_word_right(c);
    assert_eq!(at(c), (0, 32), "the whole ZWJ cluster in one step");
    assert_eq!(at(v.move_word_left(c)), (0, 14), "back to its start");
}

#[test]
fn word_motion_crosses_lines() {
    let (_fs, v) = view("one two\nthree");
    let c = v.move_word_right(Cursor::new(0, 7));
    assert_eq!(at(c), (1, 0), "off the end of a line lands on the next");
    assert_eq!(at(v.move_word_right(c)), (1, 5));
    assert_eq!(at(v.move_word_left(Cursor::new(1, 0))), (0, 7), "and back");
}

#[test]
fn vertical_motion_walks_wrapped_rows_with_sticky_x() {
    let (_fs, v) = wrapped_view("aaaaaaaaaaaaaaaaaaaa", 60.0);
    // Rows split every 6 bytes; start in the middle of the second row.
    let start = Cursor::new(0, 8);
    let sticky = v.cursor_rect(start).unwrap()[0];

    let up = v.move_up(start, sticky);
    let up_rect = v.cursor_rect(up).unwrap();
    assert_eq!(at(up), (0, 2), "same column, one row up");
    assert_eq!(up_rect[1], 0.0);
    assert!((up_rect[0] - sticky).abs() < 1.0, "sticky x is preserved");

    let down = v.move_down(start, sticky);
    let down_rect = v.cursor_rect(down).unwrap();
    assert_eq!(at(down), (0, 14), "same column, one row down");
    assert_eq!(down_rect[1], 48.0);
    assert!((down_rect[0] - sticky).abs() < 1.0);

    // Sticky x survives a short row: down twice from row 1 lands on row 3,
    // which is only two glyphs long, so the caret clamps to its end.
    let bottom = v.move_down(down, sticky);
    assert_eq!(at(bottom), (0, 20), "clamped to the end of the short row");
}

#[test]
fn vertical_motion_stops_at_the_first_and_last_row() {
    let (_fs, v) = wrapped_view("aaaaaaaaaaaaaaaaaaaa", 60.0);
    let top = Cursor::new(0, 2);
    assert_eq!(v.move_up(top, 18.0), top, "nothing above the first row");
    let bottom = Cursor::new(0, 19);
    assert_eq!(v.move_down(bottom, 8.0), bottom, "nothing below the last");
}

#[test]
fn vertical_motion_crosses_hard_line_breaks() {
    let (_fs, v) = view("first line\nsecond line");
    let start = Cursor::new(1, 6);
    let sticky = v.cursor_rect(start).unwrap()[0];
    let up = v.move_up(start, sticky);
    assert_eq!(up.line, 0);
    let up_rect = v.cursor_rect(up).unwrap();
    assert_eq!(up_rect[1], 0.0);
    // Different glyphs above, so the caret snaps to the nearest boundary
    // rather than landing exactly on sticky_x.
    assert!(
        (up_rect[0] - sticky).abs() < 10.0,
        "landed at {} for sticky {sticky}",
        up_rect[0]
    );
    assert_eq!(v.move_down(up, sticky).line, 1, "and back down");
}

#[test]
fn line_start_and_end_use_visual_rows() {
    let (_fs, v) = wrapped_view("aaaaaaaaaaaaaaaaaaaa", 60.0);
    let mid = Cursor::new(0, 8);
    let start = v.line_start(mid);
    let end = v.line_end(mid);
    assert_eq!(at(start), (0, 6), "row start, not line start");
    assert_eq!(start.affinity, Affinity::After, "stays on this row");
    assert_eq!(at(end), (0, 12), "row end, not line end");
    assert_eq!(end.affinity, Affinity::Before, "stays on this row");
    let start_rect = v.cursor_rect(start).unwrap();
    let end_rect = v.cursor_rect(end).unwrap();
    assert_eq!(start_rect[1], 24.0);
    assert_eq!(end_rect[1], 24.0, "both carets on the same row");
    assert_eq!(start_rect[0], 0.0);
    assert!(end_rect[0] > start_rect[0]);
}

#[test]
fn line_start_and_end_on_unwrapped_and_empty_lines() {
    let (_fs, v) = view("hello\n\nworld");
    assert_eq!(at(v.line_start(Cursor::new(0, 3))), (0, 0));
    assert_eq!(at(v.line_end(Cursor::new(0, 3))), (0, 5));
    assert_eq!(at(v.line_start(Cursor::new(1, 0))), (1, 0));
    assert_eq!(at(v.line_end(Cursor::new(1, 0))), (1, 0), "empty line");
    let empty = v.cursor_rect(Cursor::new(1, 0)).unwrap();
    assert_eq!([empty[0], empty[1]], [0.0, 24.0]);
}

#[test]
fn find_all_locates_matches() {
    let (_fs, v) = view("the cat and the dog\nthe end");
    let matches = v.find_all("the");
    assert_eq!(matches.len(), 3);
    assert_eq!(matches[2].0.line, 1);
    assert!(v.find_all("").is_empty());
    assert!(v.find_all("zebra").is_empty());
}
