use faf_text::cosmic_text::{Attrs, Family};
use faf_text::{CHUNK_LINES, DocCursor, Document, FontSystem, Metrics, RETAIN_CHUNKS};

const LINE_HEIGHT: f32 = 20.0;
const WIDTH: f32 = 400.0;
const VIEWPORT: f32 = 300.0;

fn metrics() -> Metrics {
    Metrics::new(16.0, LINE_HEIGHT)
}

fn font_system() -> FontSystem {
    faf_text::font_system_from_fonts(&[faf_text::FONT_DEJAVU_SANS])
}

fn document(text: &str) -> (FontSystem, Document) {
    let mut fs = font_system();
    let mut doc = Document::new(metrics());
    doc.set_attrs(&Attrs::new().family(Family::SansSerif));
    doc.set_text(text);
    doc.set_viewport(&mut fs, WIDTH, 0.0, VIEWPORT);
    (fs, doc)
}

/// `n` short numbered lines, no trailing newline.
fn numbered(n: usize) -> String {
    (0..n)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every line of a document, as the index sees them.
fn lines(doc: &Document) -> Vec<&str> {
    (0..doc.line_count()).map(|i| doc.line(i)).collect()
}

/// Line-start byte offsets, as the index sees them.
fn starts(doc: &Document) -> Vec<usize> {
    (0..doc.line_count())
        .map(|i| doc.offset_of(DocCursor::new(i, 0)))
        .collect()
}

#[test]
fn line_index_matches_a_plain_split() {
    let text = "alpha\nbeta\n\nγάμμα δ\nlast";
    let mut doc = Document::new(metrics());
    doc.set_text(text);
    assert_eq!(lines(&doc), text.split('\n').collect::<Vec<_>>());
    assert_eq!(doc.line_count(), 5);
    assert_eq!(starts(&doc), vec![0, 6, 11, 12, 26]);
    assert_eq!(doc.text(), text);
}

#[test]
fn trailing_newline_makes_an_empty_last_line() {
    let mut doc = Document::new(metrics());
    doc.set_text("a\nb\n");
    assert_eq!(lines(&doc), vec!["a", "b", ""]);
}

#[test]
fn empty_document_has_one_line() {
    let doc = Document::new(metrics());
    assert_eq!(doc.line_count(), 1);
    assert_eq!(doc.line(0), "");
    assert_eq!(doc.chunk_count(), 1);
}

#[test]
fn incremental_append_equals_a_single_set_text() {
    // Pieces deliberately split mid-line, at a newline, and after one.
    let pieces = [
        "first line\nsecond ",
        "line\nthird\n",
        "",
        "fourth\nfifth line is a bit longer\n",
        "\nseventh ends here",
    ];
    let whole: String = pieces.concat();

    let mut appended = Document::new(metrics());
    for piece in pieces {
        appended.append(piece);
    }
    let mut at_once = Document::new(metrics());
    at_once.set_text(&whole);

    assert_eq!(appended.text(), at_once.text());
    assert_eq!(appended.line_count(), at_once.line_count());
    assert_eq!(lines(&appended), lines(&at_once));
    assert_eq!(starts(&appended), starts(&at_once));
    assert_eq!(appended.total_height(), at_once.total_height());
}

#[test]
fn append_across_chunk_boundaries_stays_equivalent() {
    let n = CHUNK_LINES * 3 + 7;
    let whole = numbered(n);
    let mut appended = Document::new(metrics());
    for (i, line) in whole.split('\n').enumerate() {
        if i > 0 {
            appended.append("\n");
        }
        appended.append(line);
    }
    let mut at_once = Document::new(metrics());
    at_once.set_text(&whole);

    assert_eq!(appended.line_count(), n);
    assert_eq!(lines(&appended), lines(&at_once));
    assert_eq!(starts(&appended), starts(&at_once));
    assert_eq!(appended.chunk_count(), at_once.chunk_count());
}

#[test]
fn append_reshapes_only_the_tail() {
    let mut fs = font_system();
    let mut doc = Document::new(metrics());
    doc.set_text(&numbered(CHUNK_LINES * 4));
    doc.set_viewport(&mut fs, WIDTH, 0.0, VIEWPORT);
    let shaped_before = doc.stats().shaped_total;
    assert!(doc.is_shaped(0));

    doc.append("\nbrand new tail line");
    // The head is untouched; only the tail chunk lost its buffer.
    assert!(doc.is_shaped(0));
    assert!(!doc.is_shaped(doc.chunk_count() - 1));
    assert_eq!(doc.stats().shaped_total, shaped_before);
    assert_eq!(doc.line(doc.line_count() - 1), "brand new tail line");
}

#[test]
fn chunk_buffers_line_up_with_document_lines() {
    let n = CHUNK_LINES * 3 + 5;
    let text = (0..n)
        .map(|i| match i % 4 {
            0 => format!("plain line {i}"),
            1 => String::new(),
            2 => format!("unicode ✓ Ελληνικά κείμενο {i}"),
            _ => format!("tabs\tand  spaces {i}"),
        })
        .collect::<Vec<_>>()
        .join("\n");

    let (_fs, doc) = document(&text);
    assert!(doc.resident_chunks().len() >= 2);
    for (chunk, view) in doc.resident_chunks().into_iter().zip(doc.visible()) {
        let (start, end) = doc.chunk_lines(chunk);
        assert_eq!(view.pos, [0.0, doc.chunk_top(chunk)]);
        assert_eq!(view.buffer.lines.len(), end - start, "chunk {chunk}");
        for (i, line) in view.buffer.lines.iter().enumerate() {
            assert_eq!(line.text(), doc.line(start + i), "chunk {chunk} line {i}");
        }
    }
}

#[test]
fn window_covers_the_viewport_and_evicts_the_rest() {
    let mut fs = font_system();
    let mut doc = Document::new(metrics());
    doc.set_text(&numbered(CHUNK_LINES * 40));
    doc.set_viewport(&mut fs, WIDTH, 0.0, VIEWPORT);

    // At the top: chunk 0 plus the one-chunk margin below it.
    assert_eq!(doc.resident_chunks(), vec![0, 1]);
    let first_pass = doc.stats().shaped_last;
    assert!(first_pass <= 3, "first paint shaped {first_pass} chunks");

    // Scroll far away; the old chunks must not survive.
    let target = doc.chunk_top(20);
    doc.set_viewport(&mut fs, WIDTH, target, VIEWPORT);
    let resident = doc.resident_chunks();
    assert!(resident.contains(&20), "{resident:?}");
    assert!(!resident.contains(&0), "{resident:?}");
    for &chunk in &resident {
        assert!(
            chunk.abs_diff(20) <= RETAIN_CHUNKS,
            "chunk {chunk} is outside the retention band: {resident:?}"
        );
    }
    assert!(doc.stats().evicted_total >= 2);

    // A one-chunk scroll keeps the overlap resident instead of re-shaping it.
    let shaped_before = doc.stats().shaped_total;
    doc.set_viewport(&mut fs, WIDTH, doc.chunk_top(21), VIEWPORT);
    assert!(doc.resident_chunks().contains(&20));
    assert!(doc.stats().shaped_total - shaped_before <= 2);
}

#[test]
fn first_paint_touches_only_the_window() {
    let mut fs = font_system();
    let mut doc = Document::new(metrics());
    doc.set_text(&numbered(CHUNK_LINES * 200));
    doc.set_viewport(&mut fs, WIDTH, 0.0, VIEWPORT);
    assert!(doc.stats().shaped_total <= 5, "{:?}", doc.stats());
    assert!(
        doc.stats().lines_shaped_total <= 5 * CHUNK_LINES,
        "{:?}",
        doc.stats()
    );
    assert_eq!(doc.line_count(), CHUNK_LINES * 200);
}

#[test]
fn heights_start_estimated_and_get_corrected() {
    // Wide glyphs, so the 0.55-of-font-size column guess is definitely wrong
    // and the correction has something to correct.
    let long = "WMWM ".repeat(30);
    // A fixed-width prefix keeps every line the same shape, so every chunk
    // estimates and measures identically.
    let text = (0..CHUNK_LINES * 8)
        .map(|i| format!("{i:04} {long}"))
        .collect::<Vec<_>>()
        .join("\n");

    let mut fs = font_system();
    let mut doc = Document::new(metrics());
    doc.set_attrs(&Attrs::new().family(Family::SansSerif));
    doc.set_text(&text);
    assert_eq!(doc.chunk_count(), 8);

    // Park the viewport at the far end, so the head of the document is sized
    // purely by estimate at the real layout width.
    doc.set_viewport(&mut fs, WIDTH, f32::MAX, VIEWPORT);
    assert!(!doc.is_shaped(0));
    assert!(!doc.is_shaped(1));
    let estimated_total = doc.total_height();
    let estimated_chunk = doc.chunk_top(1) - doc.chunk_top(0);
    assert!((doc.chunk_top(2) - doc.chunk_top(1) - estimated_chunk).abs() < 0.01);

    // Scroll to the top: chunks 0 and 1 shape and report their true heights.
    doc.set_viewport(&mut fs, WIDTH, 0.0, VIEWPORT);
    let corrected: Vec<usize> = doc.resident_chunks();
    assert_eq!(corrected, vec![0, 1]);

    let measured_chunk = doc.chunk_top(1) - doc.chunk_top(0);
    let laid_out = doc
        .visible()
        .next()
        .map(|v| {
            v.buffer
                .layout_runs()
                .map(|r| r.line_top + r.line_height)
                .fold(0.0f32, f32::max)
        })
        .unwrap();
    assert!(
        (measured_chunk - laid_out).abs() < 0.01,
        "chunk height {measured_chunk} should match the layout {laid_out}"
    );
    assert!(
        measured_chunk > estimated_chunk,
        "wide glyphs wrap more than the estimate assumes: {measured_chunk} vs {estimated_chunk}"
    );

    // total_height moved by exactly the corrected chunks' error, and the
    // untouched middle keeps its estimate.
    let expected = estimated_total + (measured_chunk - estimated_chunk) * corrected.len() as f32;
    assert!(
        (doc.total_height() - expected).abs() < 1.0,
        "{} vs {expected}",
        doc.total_height()
    );
    let middle = doc.chunk_top(3) - doc.chunk_top(2);
    assert!(
        (middle - estimated_chunk).abs() < 0.01,
        "the unvisited middle should still be an estimate"
    );
}

#[test]
fn resizing_re_estimates_and_re_shapes() {
    let long = "wrapping text ".repeat(12);
    let text = (0..CHUNK_LINES * 2)
        .map(|i| format!("{i} {long}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut fs = font_system();
    let mut doc = Document::new(metrics());
    doc.set_attrs(&Attrs::new().family(Family::SansSerif));
    doc.set_text(&text);
    doc.set_viewport(&mut fs, WIDTH, 0.0, VIEWPORT);
    let narrow = doc.total_height();

    doc.set_viewport(&mut fs, WIDTH * 3.0, 0.0, VIEWPORT);
    assert!(
        doc.total_height() < narrow,
        "a wider pane must wrap less: {} vs {narrow}",
        doc.total_height()
    );
}

#[test]
fn find_all_spans_unshaped_regions() {
    let n = CHUNK_LINES * 6;
    let text = (0..n)
        .map(|i| {
            if i % 100 == 3 {
                format!("line {i} has a NEEDLE in it")
            } else {
                format!("line {i} is ordinary")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    let (_fs, doc) = document(&text);
    // Only the window is shaped, but the search sees the whole document.
    assert!(doc.resident_chunks().len() <= 3);

    let hits = doc.find_all("NEEDLE");
    let expected: Vec<usize> = (0..n).filter(|i| i % 100 == 3).collect();
    assert_eq!(hits.len(), expected.len());
    assert!(hits.len() > 4);
    for ((a, b), line) in hits.iter().zip(&expected) {
        assert_eq!(a.line, *line);
        assert_eq!(b.line, *line);
        assert_eq!(a.byte, doc.line(*line).find("NEEDLE").unwrap());
        assert_eq!(doc.text_between(*a, *b), "NEEDLE");
    }
    // The last hits live far outside the shaped window.
    let last = hits.last().unwrap().0.line;
    assert!(!doc.is_shaped(doc.chunk_of_line(last)));

    assert!(doc.find_all("").is_empty());
    assert!(doc.find_all("no such text").is_empty());
}

#[test]
fn find_all_handles_multi_line_needles() {
    let (_fs, doc) = document("alpha\nbeta\ngamma\ndelta");
    let hits = doc.find_all("beta\ngamma");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0, DocCursor::new(1, 0));
    assert_eq!(hits[0].1, DocCursor::new(2, 5));
    assert_eq!(doc.text_between(hits[0].0, hits[0].1), "beta\ngamma");
}

#[test]
fn text_between_reads_straight_from_the_backing_text() {
    let n = CHUNK_LINES * 5;
    let text = numbered(n);
    let (_fs, doc) = document(&text);

    let a = DocCursor::new(2, 0);
    let b = DocCursor::new(n - 1, doc.line(n - 1).len());
    assert_eq!(doc.text_between(a, b), text[doc.offset_of(a)..]);
    // Order does not matter.
    assert_eq!(doc.text_between(b, a), doc.text_between(a, b));
    // A range wholly inside an unshaped chunk still resolves.
    let far = DocCursor::new(n - 3, 0);
    assert_eq!(doc.text_between(far, DocCursor::new(n - 3, 4)), "line");
}

#[test]
fn doc_cursors_map_to_chunk_local_cursors() {
    let n = CHUNK_LINES * 10;
    let mut fs = font_system();
    let mut doc = Document::new(metrics());
    doc.set_attrs(&Attrs::new().family(Family::SansSerif));
    doc.set_text(&numbered(n));

    // Park the viewport well past the first chunk so the mapping has to add a
    // real chunk offset.
    let line = CHUNK_LINES * 5 + 9;
    doc.set_viewport(&mut fs, WIDTH, doc.line_top(line), VIEWPORT);

    let hit = doc
        .hit(1.0, doc.line_top(line) + LINE_HEIGHT * 0.5)
        .expect("the shaped window covers the viewport");
    assert_eq!(hit.line, line);
    assert_eq!(hit.byte, 0);
    assert_eq!(doc.chunk_of_line(hit.line), 5);

    // Hitting past the end of a row lands on that row's last byte.
    let end = doc
        .hit(10_000.0, doc.line_top(line) + LINE_HEIGHT * 0.5)
        .unwrap();
    assert_eq!(end.line, line);
    assert_eq!(end.byte, doc.line(line).len());
    assert_eq!(doc.text_between(hit, end), doc.line(line));

    // The next visual row is the next document line.
    let next = doc
        .hit(1.0, doc.line_top(line) + LINE_HEIGHT * 1.5)
        .unwrap();
    assert_eq!(next.line, line + 1);
}

#[test]
fn selection_rects_cover_the_visible_part_only() {
    let n = CHUNK_LINES * 8;
    let (_fs, doc) = document(&numbered(n));

    // A selection running from the very top to the very bottom: only the
    // shaped window can contribute geometry.
    let all = doc.selection_rects(DocCursor::new(0, 0), DocCursor::new(n - 1, 0));
    let shaped_lines: usize = doc
        .resident_chunks()
        .iter()
        .map(|&c| {
            let (s, e) = doc.chunk_lines(c);
            e - s
        })
        .sum();
    assert!(!all.is_empty());
    assert!(
        all.len() <= shaped_lines,
        "{} rects for {shaped_lines} shaped lines",
        all.len()
    );

    // Rects are in document space: the first one sits on line 0.
    assert!(all[0][1] >= 0.0 && all[0][1] < LINE_HEIGHT);

    // A range entirely inside an unshaped region yields nothing.
    let far = doc.selection_rects(DocCursor::new(n - 5, 0), DocCursor::new(n - 1, 0));
    assert!(far.is_empty(), "{far:?}");

    // A one-line selection is one rect, and order does not matter.
    let a = DocCursor::new(1, 0);
    let b = DocCursor::new(1, 4);
    let one = doc.selection_rects(a, b);
    assert_eq!(one.len(), 1);
    assert_eq!(doc.selection_rects(b, a).len(), 1);
    assert!(one[0][2] > 0.0);
}

#[test]
fn offsets_round_trip_through_cursors() {
    let text = "alpha\n\nγάμμα\nomega";
    let mut doc = Document::new(metrics());
    doc.set_text(text);
    for (offset, _) in text.char_indices() {
        let c = doc.cursor_at_offset(offset);
        assert_eq!(doc.offset_of(c), offset, "offset {offset}");
    }
    assert_eq!(doc.cursor_at_offset(text.len()).line, 3);
    assert_eq!(doc.cursor_at_offset(usize::MAX).line, 3);
}
