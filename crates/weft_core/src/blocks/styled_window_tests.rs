//! v1.13.5 T16a: windowed live-styled rebuild equivalence tests.
//!
//! `build_styled_output_windowed` (candidate ① fix) must be byte-equal to
//! the full build for every line it covers, carry ABSOLUTE line indices,
//! and fall back to the full build on every degenerate shape.

use super::style::{
    build_styled_output_from_runs, build_styled_output_windowed, CapturedStyle, CapturedStyleRun,
    StyledOutput, LIVE_STYLED_WINDOW_BYTES,
};

fn red_run(start: u32, end: u32) -> CapturedStyleRun {
    CapturedStyleRun {
        start_char: start,
        end_char: end,
        style: CapturedStyle {
            fg: crate::grid::CellColor::Palette(1),
            ..CapturedStyle::default()
        },
    }
}

/// Small-window harness: the production constant is 768KB; tests pass a
/// tiny window so the windowing actually engages on kilobyte fixtures.
fn tiny_window() -> usize {
    96
}

fn newlines(text: &str) -> usize {
    text.bytes().filter(|b| *b == b'\n').count()
}

fn chars(text: &str) -> usize {
    text.chars().count()
}

/// The windowed build restricted to the tail must equal the full build's
/// tail lines, with absolute line indices preserved.
#[test]
fn windowed_matches_full_build_tail_with_absolute_indices() {
    // 60 lines of 10 chars each = 659 bytes; window 96 bytes ≈ last 9 lines.
    let mut text = String::new();
    for i in 0..60 {
        text.push_str(&format!("line{i:05}\n"));
    }
    let text = text.trim_end().to_string();
    // Style every line's number.
    let mut runs = Vec::new();
    for i in 0..60 {
        let start = (i * 10) as u32;
        runs.push(red_run(start, start + 5));
    }
    let full: StyledOutput = build_styled_output_from_runs(&text, &runs).expect("full");
    let windowed =
        build_styled_output_windowed(&text, &runs, newlines(&text), chars(&text), tiny_window())
            .expect("windowed");

    // Every windowed line must be byte-equal to the same-index full line.
    for line in &windowed.lines {
        let expected = full.line(line.line as usize).expect("full has the line");
        assert_eq!(line, expected, "windowed line {} diverged", line.line);
    }
    // The window covers only the tail — assert it's a strict subset and
    // non-trivial (the fixture is engineered so ~9 lines fit).
    assert!(
        windowed.lines.len() < full.lines.len(),
        "window must drop head lines (windowed {} vs full {})",
        windowed.lines.len(),
        full.lines.len()
    );
    assert!(windowed.lines.len() >= 5, "window must keep the tail");
    assert_eq!(
        windowed.lines.last().unwrap().line as usize,
        59,
        "last line index is absolute"
    );
}

/// Text inside the window: windowed must be byte-equal to the full build.
#[test]
fn small_text_falls_back_to_full_build() {
    let text = "alpha\nbeta\ngamma";
    let runs = vec![red_run(6, 10)];
    let full = build_styled_output_from_runs(text, &runs).expect("full");
    let windowed =
        build_styled_output_windowed(text, &runs, newlines(text), chars(text), tiny_window())
            .expect("windowed");
    assert_eq!(full, windowed);
}

/// Styles ending before the window: full-build fallback (the s1 early-stop
/// already bounds that build, and the old head styles must survive).
#[test]
fn styles_ending_before_window_fall_back_to_full_build() {
    let mut text = String::new();
    // 100 styled chars up front, then 4KB of plain tail.
    text.push_str("styled-head-");
    for _ in 0..400 {
        text.push_str("paddingxxxxx\n");
    }
    let runs = vec![red_run(0, 12)];
    let full = build_styled_output_from_runs(&text, &runs).expect("full");
    let windowed =
        build_styled_output_windowed(&text, &runs, newlines(&text), chars(&text), tiny_window())
            .expect("w");
    assert_eq!(full, windowed, "head styles must survive via full fallback");
}

/// A run spanning the window boundary contributes its in-window part with
/// correct line-local offsets.
#[test]
fn run_spanning_window_boundary_is_clipped_into_the_window() {
    // One long styled run over "AAAA…\nBBBB…\ntail" where the window starts
    // on the BBBB line: the BBBB line's span must start at its local 0.
    let a = "A".repeat(80);
    let text = format!("{a}\nBBBB-tail\nCCCC-tail");
    let total_chars = text.chars().count() as u32;
    let runs = vec![red_run(0, total_chars)];
    let windowed =
        build_styled_output_windowed(&text, &runs, newlines(&text), chars(&text), tiny_window())
            .expect("windowed");
    // Window starts at the BBBB line (absolute line 1).
    let b_line = windowed.line(1).expect("BBBB line styled");
    let fg = b_line
        .foreground_at(0)
        .expect("span covers the line's first char");
    assert_eq!(fg, crate::grid::CellColor::Palette(1));
    // And the last line too.
    assert!(windowed.line(2).is_some(), "tail line stays styled");
}

/// The production window (768KB) on a small text is just the full build.
#[test]
fn production_window_on_small_text_is_full_build() {
    let text = "a\nb\n";
    let runs = vec![red_run(0, 1)];
    let full = build_styled_output_from_runs(text, &runs);
    let windowed = build_styled_output_windowed(
        text,
        &runs,
        newlines(text),
        chars(text),
        LIVE_STYLED_WINDOW_BYTES,
    );
    assert_eq!(full, windowed);
}

/// Empty runs stay None in both paths.
#[test]
fn empty_runs_are_none_windowed_too() {
    let text = "some\ntext";
    assert!(
        build_styled_output_windowed(text, &[], newlines(text), chars(text), tiny_window())
            .is_none()
    );
}
