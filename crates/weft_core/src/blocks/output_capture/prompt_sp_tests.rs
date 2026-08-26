//! PROMPT_SP tail-strip regressions (docs/FIX_CAPTURE_PROMPT_SP_SPACES.md).
//!
//! zsh's PROMPT_SP prompt-cleanup mechanism emits a full line width of
//! literal spaces plus `\r\r` right before painting each prompt. Those
//! bytes land inside the OSC 133;C→D capture window, so `take_styled()`
//! must strip the captured tail's trailing whitespace run at finalize —
//! otherwise every Copy Output / export / search / persisted block carries
//! a phantom line of `cols` spaces.

use super::{CapturedStyle, OutputCapture};
use crate::grid::{CellColor, CellFlags, UnderlineStyle};

fn fg_style(palette: u8) -> CapturedStyle {
    CapturedStyle::from_attrs(
        CellColor::Palette(palette),
        CellColor::Default,
        CellFlags::empty(),
        UnderlineStyle::Single,
        None,
    )
}

/// cols=99 reproduces the pty evidence: the capture tail held exactly 99
/// consecutive spaces when the terminal width was 99.
#[test]
fn trailing_prompt_sp_run_is_stripped_after_newline_terminated_output() {
    let mut output = OutputCapture::default();
    output.print_ascii(b"hi", CapturedStyle::default(), 1024);
    output.newline(1024);
    output.print_ascii(&[b' '; 99], CapturedStyle::default(), 1024);
    output.carriage_return();
    output.carriage_return();
    let (text, _styled) = output.take_styled();
    assert_eq!(text, "hi");
}

/// Output not terminated by a newline: the PROMPT_SP spaces concatenate
/// onto the last line and must be stripped from there.
#[test]
fn trailing_prompt_sp_run_is_stripped_from_unterminated_last_line() {
    let mut output = OutputCapture::default();
    output.print_ascii(b"abc", CapturedStyle::default(), 1024);
    output.print_ascii(&[b' '; 99], CapturedStyle::default(), 1024);
    output.carriage_return();
    let (text, _styled) = output.take_styled();
    assert_eq!(text, "abc");
}

/// A silent command (no output at all) whose capture window only saw the
/// PROMPT_SP emission finalizes as a truly empty block.
#[test]
fn silent_command_prompt_sp_only_becomes_empty_output() {
    let mut output = OutputCapture::default();
    output.print_ascii(&[b' '; 99], CapturedStyle::default(), 1024);
    output.carriage_return();
    let (text, styled) = output.take_styled();
    assert_eq!(text, "");
    assert!(styled.is_none(), "no styles survive an empty capture");
}

/// zsh emits PROMPT_SP before EVERY prompt, so a capture can hold two
/// space runs separated by the newline the shell wrote between them —
/// the whole trailing whitespace region goes, not just the last run.
#[test]
fn double_prompt_sp_runs_across_lines_are_stripped() {
    let mut output = OutputCapture::default();
    output.print_ascii(b"content", CapturedStyle::default(), 1024);
    output.newline(1024);
    output.print_ascii(&[b' '; 99], CapturedStyle::default(), 1024);
    output.newline(1024);
    output.print_ascii(&[b' '; 99], CapturedStyle::default(), 1024);
    output.carriage_return();
    let (text, _styled) = output.take_styled();
    assert_eq!(text, "content");
}

/// The style RLE is char-indexed over the pre-strip buffer: runs must be
/// dropped/clamped to the trimmed length so `build_styled_output_from_runs`
/// never sees indices past the text end, and styled lines for the removed
/// rows disappear.
#[test]
fn styled_runs_are_clamped_after_tail_strip() {
    let mut output = OutputCapture::default();
    output.print_ascii(b"red", fg_style(2), 1024);
    output.newline(1024);
    output.print_ascii(&[b' '; 99], CapturedStyle::default(), 1024);
    output.carriage_return();
    let (text, styled) = output.take_styled();
    assert_eq!(text, "red");
    let styled = styled.expect("colored content keeps its styled output");
    let line = styled.line(0).expect("line 0");
    for i in 0..3 {
        assert_eq!(
            line.foreground_at(i),
            Some(CellColor::Palette(2)),
            "char {i}"
        );
    }
    assert!(
        styled.line(1).is_none(),
        "stripped tail leaves no styled line"
    );
}

/// Colored trailing spaces (styled PROMPT_SP emissions) must not panic the
/// RLE clamp and must still be trimmed from the text.
#[test]
fn colored_trailing_spaces_do_not_panic_and_are_trimmed() {
    let mut output = OutputCapture::default();
    output.print_ascii(b"ab", fg_style(1), 1024);
    output.print_ascii(&[b' '; 50], fg_style(1), 1024);
    output.carriage_return();
    let (text, styled) = output.take_styled();
    assert_eq!(text, "ab");
    let styled = styled.expect("styled build succeeds after the clamp");
    let line = styled.line(0).expect("line 0");
    assert_eq!(line.foreground_at(0), Some(CellColor::Palette(1)));
    assert_eq!(line.foreground_at(1), Some(CellColor::Palette(1)));
}

/// Only the tail is stripped: interior spaces and the spaces `goto`
/// materializes when cursor-addressing past the line end survive.
#[test]
fn interior_spaces_and_goto_materialized_spaces_survive() {
    let mut output = OutputCapture::default();
    output.print_ascii(b"a", CapturedStyle::default(), 1024);
    output.newline(1024);
    output.goto(1, 2, 1024);
    output.print_ascii(b"b", CapturedStyle::default(), 1024);
    let (text, _styled) = output.take_styled();
    assert_eq!(text, "a\n  b");
}

/// The truncation marker is appended AFTER the strip, so the finalized
/// text never carries the space tail ahead of the marker.
#[test]
fn truncation_marker_appended_after_strip() {
    let mut output = OutputCapture::default();
    output.print_ascii(b"data", CapturedStyle::default(), 10);
    output.newline(10);
    // The accepted space prefix fills the buffer to exactly max_bytes; the
    // remainder trips `truncated` — the PROMPT_SP shape at the cap.
    output.print_ascii(&[b' '; 99], CapturedStyle::default(), 10);
    let (text, _styled) = output.take_styled();
    assert_eq!(text, "data\n…(output truncated, >1 MiB)");
}
