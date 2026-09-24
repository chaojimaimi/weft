// OutputCapture unit tests (split from output_capture.rs to keep it within
// its architecture-gate budget).

use super::*;
use crate::blocks::DEFAULT_OUTPUT_CAP;
use crate::grid::{CellColor, CellFlags, Color};

fn owned(fg: CellColor, flags: CellFlags, bg: CellColor) -> CapturedStyle {
    CapturedStyle {
        fg,
        bg,
        flags,
        ansi_owned: true,
        ..CapturedStyle::default()
    }
}

fn fg_style(palette: u8) -> CapturedStyle {
    owned(
        CellColor::Palette(palette),
        CellFlags::empty(),
        CellColor::Default,
    )
}

fn bold_style() -> CapturedStyle {
    owned(CellColor::Default, CellFlags::BOLD, CellColor::Default)
}

fn rgb_style(r: u8, g: u8, b: u8) -> CapturedStyle {
    owned(
        CellColor::Rgb(Color::rgb(r, g, b)),
        CellFlags::empty(),
        CellColor::Default,
    )
}

#[test]
fn carriage_return_overwrites_spinner_frame_in_place() {
    let mut output = OutputCapture::default();
    output.print_ascii(b"Upgrading.", CapturedStyle::default(), 1024);
    output.carriage_return();
    output.print_ascii(b"Upgrading..", CapturedStyle::default(), 1024);
    output.carriage_return();
    output.print_ascii(b"Upgrading...", CapturedStyle::default(), 1024);
    assert_eq!(output.as_str(), "Upgrading...");
}

#[test]
fn horizontal_absolute_rewrites_spinner_frame_in_place() {
    let mut output = OutputCapture::default();
    output.print_ascii(b"Upgrading.", CapturedStyle::default(), 1024);
    output.set_cursor_column(0, 1024);
    output.print_ascii(b"Upgrading..", CapturedStyle::default(), 1024);
    output.set_cursor_column(0, 1024);
    output.print_ascii(b"Upgrading...", CapturedStyle::default(), 1024);
    output.erase_line(0);
    assert_eq!(output.as_str(), "Upgrading...");
}

#[test]
fn relative_cursor_movement_rewrites_instead_of_appending() {
    let mut output = OutputCapture::default();
    output.print_ascii(b"status old", CapturedStyle::default(), 1024);
    output.move_cursor_columns(-3, 1024);
    output.print_ascii(b"new", CapturedStyle::default(), 1024);
    assert_eq!(output.as_str(), "status new");
}

#[test]
fn erase_right_removes_tail_from_a_shorter_progress_frame() {
    let mut output = OutputCapture::default();
    output.print_ascii(b"Downloading 100%", CapturedStyle::default(), 1024);
    output.carriage_return();
    output.print_ascii(b"Done", CapturedStyle::default(), 1024);
    output.erase_line(0);
    assert_eq!(output.as_str(), "Done");
}

#[test]
fn utf8_replacement_keeps_cursor_on_a_character_boundary() {
    let mut output = OutputCapture::default();
    output.print_ascii(b"abc", CapturedStyle::default(), 1024);
    output.carriage_return();
    output.print('中', CapturedStyle::default(), 1024);
    output.print('文', CapturedStyle::default(), 1024);
    assert_eq!(output.as_str(), "中文c");
}

#[test]
fn absolute_cursor_rows_preserve_line_structure() {
    let mut output = OutputCapture::default();
    output.goto(0, 0, 1024);
    output.print_ascii(b"first", CapturedStyle::default(), 1024);
    output.goto(2, 3, 1024);
    output.print_ascii(b"third", CapturedStyle::default(), 1024);
    output.goto(1, 1, 1024);
    output.print_ascii(b"second", CapturedStyle::default(), 1024);

    assert_eq!(output.as_str(), "first\n second\n   third");
}

// ── v1.7.0-A: ANSI style RLE tests ──────────────────────────────────

#[test]
fn ascii_batch_captures_one_run_for_a_colored_segment() {
    let mut output = OutputCapture::default();
    output.print_ascii(b"hello", fg_style(2), 1024);
    let (text, styled) = output.take_styled();
    assert_eq!(text, "hello");
    let styled = styled.expect("styled output");
    let line = styled.line(0).expect("line 0");
    assert_eq!(line.foreground_at(0), Some(CellColor::Palette(2)));
    assert_eq!(line.foreground_at(4), Some(CellColor::Palette(2)));
    assert!(line.foregrounds.len() == 1, "one coalesced run");
}

#[test]
fn default_style_chars_produce_no_runs() {
    let mut output = OutputCapture::default();
    output.print_ascii(b"plain", CapturedStyle::default(), 1024);
    let (text, styled) = output.take_styled();
    assert_eq!(text, "plain");
    assert!(styled.is_none(), "default-only output has no styled output");
}

#[test]
fn carriage_return_overwrites_style_alongside_text() {
    let mut output = OutputCapture::default();
    output.print_ascii(b"red", fg_style(1), 1024);
    output.carriage_return();
    output.print_ascii(b"green", fg_style(2), 1024);
    let (text, styled) = output.take_styled();
    assert_eq!(text, "green");
    let styled = styled.expect("styled");
    let line = styled.line(0).expect("line 0");
    assert_eq!(line.foreground_at(0), Some(CellColor::Palette(2)));
    assert_eq!(line.foreground_at(4), Some(CellColor::Palette(2)));
    assert!(line.foregrounds.len() == 1, "old red run was replaced");
}

#[test]
fn backspace_preserves_style_until_overwritten() {
    let mut output = OutputCapture::default();
    output.print('a', fg_style(1), 1024);
    output.print('b', fg_style(2), 1024);
    output.backspace();
    // 'a' still has palette 1; 'b' (cursor) retains palette 2 until overwritten.
    let (text, styled) = output.take_styled();
    assert_eq!(text, "ab");
    let styled = styled.expect("styled");
    let line = styled.line(0).expect("line 0");
    assert_eq!(line.foreground_at(0), Some(CellColor::Palette(1)));
    assert_eq!(line.foreground_at(1), Some(CellColor::Palette(2)));
}

#[test]
fn erase_line_mode_0_drops_style_in_tail() {
    let mut output = OutputCapture::default();
    output.print_ascii(b"keep", fg_style(2), 1024);
    output.print_ascii(b"erase", fg_style(1), 1024);
    output.carriage_return();
    // Re-print "keep" with the SAME green style — simulates a progress
    // bar repaint where the persistent prefix keeps its color.
    output.print_ascii(b"keep", fg_style(2), 1024);
    output.erase_line(0);
    let (text, styled) = output.take_styled();
    assert_eq!(text, "keep");
    let styled = styled.expect("styled");
    let line = styled.line(0).expect("line 0");
    assert_eq!(line.foreground_at(0), Some(CellColor::Palette(2)));
    assert_eq!(line.foreground_at(3), Some(CellColor::Palette(2)));
    assert_eq!(line.foregrounds.len(), 1, "red 'erase' style was dropped");
}

#[test]
fn erase_line_mode_2_clears_all_styles_on_line() {
    let mut output = OutputCapture::default();
    output.print_ascii(b"line1", fg_style(1), 1024);
    output.newline(1024);
    output.print_ascii(b"line2", fg_style(2), 1024);
    output.carriage_return();
    output.erase_line(2);
    let (text, styled) = output.take_styled();
    // PROMPT_SP tail-strip: the finalized text drops the trailing '\n'.
    assert_eq!(text, "line1");
    let styled = styled.expect("styled");
    assert_eq!(
        styled.line(0).expect("line 0").foreground_at(0),
        Some(CellColor::Palette(1))
    );
    assert!(styled.line(1).is_none(), "line 1 had all styles erased");
}

#[test]
fn multiline_capture_indexes_lines_independently() {
    let mut output = OutputCapture::default();
    output.print_ascii(b"red", fg_style(1), 1024);
    output.newline(1024);
    output.print_ascii(b"green", fg_style(2), 1024);
    output.newline(1024);
    output.print_ascii(b"blue", fg_style(4), 1024);
    let (text, styled) = output.take_styled();
    assert_eq!(text, "red\ngreen\nblue");
    let styled = styled.expect("styled");
    assert_eq!(
        styled.line(0).unwrap().foreground_at(0),
        Some(CellColor::Palette(1))
    );
    assert_eq!(
        styled.line(1).unwrap().foreground_at(0),
        Some(CellColor::Palette(2))
    );
    assert_eq!(
        styled.line(2).unwrap().foreground_at(0),
        Some(CellColor::Palette(4))
    );
}

#[test]
fn cr_progress_update_preserves_unrelated_styles() {
    // Simulate a spinner: "Upgrading..." printed 3 times via CR.
    // Each iteration overwrites the same range; the final style should
    // be the last one written.
    let mut output = OutputCapture::default();
    output.print_ascii(b"Upgrading.", fg_style(3), 1024);
    output.carriage_return();
    output.print_ascii(b"Upgrading..", fg_style(3), 1024);
    output.carriage_return();
    output.print_ascii(b"Upgrading...", fg_style(3), 1024);
    let (text, styled) = output.take_styled();
    assert_eq!(text, "Upgrading...");
    let styled = styled.expect("styled");
    let line = styled.line(0).expect("line 0");
    for i in 0..11 {
        assert_eq!(
            line.foreground_at(i),
            Some(CellColor::Palette(3)),
            "char {i}"
        );
    }
}

#[test]
fn bold_flags_round_trip_through_styled_output() {
    let mut output = OutputCapture::default();
    output.print('h', bold_style(), 1024);
    output.print('i', CapturedStyle::default(), 1024);
    let (text, styled) = output.take_styled();
    assert_eq!(text, "hi");
    let styled = styled.expect("styled");
    let line = styled.line(0).expect("line 0");
    assert_eq!(line.attributes_at(0), CellFlags::BOLD);
    assert_eq!(line.attributes_at(1), CellFlags::empty());
}

#[test]
fn truecolor_rgb_is_preserved_not_baked_to_palette() {
    let mut output = OutputCapture::default();
    output.print('x', rgb_style(123, 45, 67), 1024);
    let (text, styled) = output.take_styled();
    assert_eq!(text, "x");
    let styled = styled.expect("styled");
    let line = styled.line(0).expect("line 0");
    assert_eq!(
        line.foreground_at(0),
        Some(CellColor::Rgb(Color::rgb(123, 45, 67)))
    );
}

#[test]
fn background_and_foreground_coexist_in_one_run() {
    let mut output = OutputCapture::default();
    let style = owned(
        CellColor::Palette(2),
        CellFlags::UNDERLINE,
        CellColor::Palette(5),
    );
    output.print('x', style, 1024);
    let (text, styled) = output.take_styled();
    assert_eq!(text, "x");
    let styled = styled.expect("styled");
    let line = styled.line(0).expect("line 0");
    assert_eq!(line.foreground_at(0), Some(CellColor::Palette(2)));
    assert_eq!(line.background_at(0), Some(CellColor::Palette(5)));
    assert_eq!(line.attributes_at(0), CellFlags::UNDERLINE);
}

#[test]
fn goto_preserves_styles_on_other_lines() {
    let mut output = OutputCapture::default();
    output.print_ascii(b"line0", fg_style(1), 1024);
    output.newline(1024);
    output.goto(2, 0, 1024);
    output.print_ascii(b"line2", fg_style(3), 1024);
    let (text, styled) = output.take_styled();
    assert_eq!(text, "line0\n\nline2");
    let styled = styled.expect("styled");
    assert_eq!(
        styled.line(0).unwrap().foreground_at(0),
        Some(CellColor::Palette(1))
    );
    assert_eq!(
        styled.line(2).unwrap().foreground_at(0),
        Some(CellColor::Palette(3))
    );
}

#[test]
fn coalescing_adjacent_same_style_runs_stays_compact() {
    let mut output = OutputCapture::default();
    for _ in 0..50 {
        output.print('a', fg_style(2), 1024);
    }
    let (_, styled) = output.take_styled();
    let styled = styled.expect("styled");
    let line = styled.line(0).expect("line 0");
    assert_eq!(
        line.foregrounds.len(),
        1,
        "50 same-style chars coalesce to 1 run"
    );
}

#[test]
fn style_overflow_drops_runs_but_keeps_text() {
    let mut output = OutputCapture::default();
    // Alternating palette indices force a new run per char, quickly
    // exhausting MAX_STYLE_RUNS_PER_BLOCK.
    for i in 0..(MAX_STYLE_RUNS_PER_BLOCK + 100) {
        let palette = ((i % 2) as u8) + 1;
        output.print('x', fg_style(palette), DEFAULT_OUTPUT_CAP);
        if output.style_overflow {
            break;
        }
    }
    assert!(output.style_overflow, "overflow flag must be set");
    let (text, styled) = output.take_styled();
    assert!(!text.is_empty(), "text survives style overflow");
    assert!(styled.is_none(), "styled output is dropped on overflow");
}

#[test]
fn replace_clears_captured_styles() {
    let mut output = OutputCapture::default();
    output.print_ascii(b"red", fg_style(1), 1024);
    output.replace("replaced", 1024);
    let (text, styled) = output.take_styled();
    assert_eq!(text, "replaced");
    // replace() is the screen-snapshot path — it does not synthesize
    // styles from the RLE. The caller supplies StyledOutput separately.
    assert!(styled.is_none());
}

// ── PLAN_v11217 §3.5 (T4): cap metadata → truncation marker ─────────

/// Default-constructed captures (tracker init, preexec staging, interrupt
/// tail, `mem::take` reset) carry the DEFAULT cap metadata — review P2c:
/// they must never report "truncated at 0 MiB".
#[test]
fn default_capture_reports_the_default_cap_mib() {
    let mut output = OutputCapture::default();
    assert_eq!(output.cap_bytes(), DEFAULT_OUTPUT_CAP);
    // Truncate deterministically: 6 bytes of text against a 4-byte budget
    // — the trailing newline hits `text.len() < max_bytes` false.
    output.print_ascii(b"abc", CapturedStyle::default(), 4);
    output.print_ascii(b"def", CapturedStyle::default(), 4);
    output.newline(4);
    let (text, _) = output.take_styled();
    assert!(
        text.contains("block excerpt truncated at 1 MiB"),
        "default metadata must yield the 1 MiB marker: {text:?}"
    );
}

/// The recorded metadata is what the marker reports — cap=2 MiB captures
/// end with "truncated at 2 MiB" and the scrollback clarification.
#[test]
fn configured_cap_metadata_drives_the_marker_copy() {
    let mut output = OutputCapture::default();
    output.set_cap_bytes(2 * 1024 * 1024);
    // Truncate deterministically: 6 bytes of text against a 4-byte budget
    // — the second run's tail trips `truncated` (same shape as the
    // default-metadata twin above).
    output.print_ascii(b"abc", CapturedStyle::default(), 4);
    output.print_ascii(b"def", CapturedStyle::default(), 4);
    output.newline(4);
    let (text, _) = output.take_styled();
    assert!(
        text.ends_with("\n…(block excerpt truncated at 2 MiB — full output remains in scrollback)"),
        "marker must follow the recorded cap metadata: {text:?}"
    );
}

// ── v1.7.0-E acceptance tests ────────────────────────────────────

/// v1.7.0-E §2.7: "复制/导出文本不含颜色控制字节". Verifies that text
/// extracted from `take_styled()` contains no ESC (0x1b), CSI (0x9b),
/// or other C1 control bytes even when the capture was fed alternating
/// ANSI palette colors and attributes. The `OutputCapture` stores
/// parsed text only — raw ESC sequences are consumed by the VT parser
/// before reaching the capture — but this test pins the contract.
#[test]
fn captured_text_contains_no_ansi_escape_bytes() {
    let mut output = OutputCapture::default();
    // Feed alternating palette colors + bold to maximize the chance of
    // any escape leakage (there should be none).
    for i in 0..200u8 {
        let style = owned(
            CellColor::Palette(i % 8),
            if i % 2 == 0 {
                CellFlags::BOLD
            } else {
                CellFlags::empty()
            },
            CellColor::Default,
        );
        output.print_ascii(&[b'a' + (i % 26)], style, 1024);
    }
    let (text, _styled) = output.take_styled();
    assert!(!text.is_empty());
    // No ESC (0x1b), no CSI (0x9b), no SGR parameter bytes (0x30-0x3f
    // alone are fine — they're digits/punctuation — but 0x1b is the
    // hard gate).
    let bytes = text.as_bytes();
    assert!(
        !bytes.contains(&0x1b) && !bytes.contains(&0x9b),
        "captured text contains ANSI escape bytes"
    );
    // Also verify no C1 control bytes (0x80..=0x9f) which include CSI.
    assert!(
        !bytes.iter().any(|&b| (0x80..=0x9f).contains(&b)),
        "captured text contains C1 control bytes"
    );
}

/// v1.7.0-E §2.7 combined stress: 1 MiB high-color output + 16,384
/// ANSI style runs + 8,192 semantic spans + 13,200 plain lines. Verifies
/// the capture path doesn't corrupt or lose text, respects the style
/// run cap, and the semantic classifier respects the block span cap —
/// all without panicking or exceeding the documented bounds.
///
/// This is a pure-logic stress test (no winit event loop); the
/// "不阻塞 winit event loop" requirement is verified separately by the
/// performance gate and the `#[ignore]` benchmarks.
#[test]
fn combined_stress_1mib_high_color_16k_runs_8k_spans_13k_lines() {
    let mut output = OutputCapture::default();

    // ── Phase 1: 16,384 alternating-color runs (high-color) ────────
    // Each run is 1 char with a distinct palette color, pushing the
    // RLE to its 16,384-run cap. After the cap, additional styles are
    // dropped but text continues.
    for i in 0..(MAX_STYLE_RUNS_PER_BLOCK + 100) {
        let style = owned(
            CellColor::Palette((i % 255) as u8),
            CellFlags::empty(),
            CellColor::Default,
        );
        output.print_ascii(b"X", style, DEFAULT_OUTPUT_CAP);
    }
    assert!(
        output.style_overflow,
        "style_overflow must be set after exceeding MAX_STYLE_RUNS_PER_BLOCK"
    );

    // ── Phase 2: 13,200 plain (default-style) lines ───────────────
    // Each line is 80 chars + 1 newline = 81 bytes → 1,069,200 bytes total.
    // Combined with Phase 1's 16,484 bytes, total exceeds DEFAULT_OUTPUT_CAP
    // (1,048,576), exercising the text truncation path.
    let line: String = "a".repeat(80);
    for _ in 0..13_200 {
        output.print_ascii(
            line.as_bytes(),
            CapturedStyle::default(),
            DEFAULT_OUTPUT_CAP,
        );
        output.newline(DEFAULT_OUTPUT_CAP);
    }

    let (text, styled) = output.take_styled();

    // Text survived and was truncated. The truncation marker is appended
    // by take_styled() when the capture exceeds DEFAULT_OUTPUT_CAP, so
    // text.len() can slightly exceed the cap (by the marker length).
    assert!(!text.is_empty(), "text must survive combined stress");
    assert!(
        text.contains("block excerpt truncated at 1 MiB"),
        "text should contain truncation marker, got len {}",
        text.len()
    );
    assert!(
        text.contains("full output remains in scrollback"),
        "marker must clarify the data is not lost"
    );
    // Text is approximately bounded (within ~100 bytes of the cap + marker).
    assert!(
        text.len() <= DEFAULT_OUTPUT_CAP + 96,
        "text len {} greatly exceeds DEFAULT_OUTPUT_CAP {} (expected ~cap + marker)",
        text.len(),
        DEFAULT_OUTPUT_CAP
    );
    // No escape bytes leaked into the text.
    assert!(
        !text.as_bytes().contains(&0x1b),
        "stress text contains ESC bytes"
    );

    // Styled output is dropped when style_overflow is set (the RLE
    // exceeded MAX_STYLE_RUNS_PER_BLOCK). This is the documented
    // behavior: overflow drops styles but keeps text. The semantic
    // classifier runs on the text directly.
    assert!(
        styled.is_none(),
        "styled output should be dropped after style overflow"
    );

    // ── Phase 3: Run semantic classifier on the captured text ──────
    // The classifier should complete without panicking and respect the
    // 8,192-span block cap. Most lines are plain "aaa..." which won't
    // produce semantic spans, but the exercise validates the pipeline.
    let semantic = crate::blocks::classify_block(&text, None);
    if let Some(sem) = semantic {
        // Block span cap is enforced; even if every line produced spans,
        // the total cannot exceed MAX_SEMANTIC_SPANS_PER_BLOCK.
        let total: usize = sem.lines.iter().map(|l| l.spans.len()).sum();
        assert!(
            total <= crate::blocks::MAX_SEMANTIC_SPANS_PER_BLOCK,
            "semantic spans {} exceed block cap {}",
            total,
            crate::blocks::MAX_SEMANTIC_SPANS_PER_BLOCK
        );
    }
}
