use faf_text::TextView;
use faf_text::cosmic_text::{Attrs, Cursor, Family, Metrics};

fn view(text: &str) -> (faf_text::FontSystem, TextView) {
    let mut fs = faf_text::font_system_from_fonts(&[faf_text::FONT_DEJAVU_SANS]);
    let mut v = TextView::new(&mut fs, Metrics::new(16.0, 24.0));
    v.set_text(&mut fs, text, &Attrs::new().family(Family::SansSerif));
    (fs, v)
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
fn find_all_locates_matches() {
    let (_fs, v) = view("the cat and the dog\nthe end");
    let matches = v.find_all("the");
    assert_eq!(matches.len(), 3);
    assert_eq!(matches[2].0.line, 1);
    assert!(v.find_all("").is_empty());
    assert!(v.find_all("zebra").is_empty());
}
