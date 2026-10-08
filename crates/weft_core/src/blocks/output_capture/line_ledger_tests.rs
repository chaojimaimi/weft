//! T16a (PLAN_v11217 §3.11): invariant tests for the `text_line_count`
//! ledger — the O(1) newline count the renderer's live TUI-caret fallback
//! now reads instead of a full `output.lines().count()` scan per frame.
//!
//! Convention: same shape as the `screen_prefix` ledger invariant tests —
//! a deterministic op-sequence sweep pins `ledger == recompute` after EVERY
//! mutation, plus per-op targeted units for the non-append paths.

use super::{CapturedStyle, OutputCapture};
use crate::blocks::DEFAULT_OUTPUT_CAP;

fn recompute(output: &OutputCapture) -> usize {
    output.as_str().bytes().filter(|b| *b == b'\n').count()
}

fn assert_ledger(output: &OutputCapture, context: &str) {
    assert_eq!(
        output.text_line_count(),
        recompute(output),
        "line ledger diverged: {context} (text = {:?})",
        output.as_str()
    );
    assert_eq!(
        output.text_char_count(),
        output.as_str().chars().count(),
        "char ledger diverged: {context} (text = {:?})",
        output.as_str()
    );
}

/// Deterministic LCG fuzz over every mutation op. The ledger must equal the
/// recompute after each step — this is the drift guard the renderer's
/// per-frame O(1) read stands on.
#[test]
fn ledger_tracks_recompute_across_mutation_sequence() {
    let mut output = OutputCapture::default();
    let mut lcg: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move || {
        lcg ^= lcg << 13;
        lcg ^= lcg >> 7;
        lcg ^= lcg << 17;
        lcg
    };

    let words = ["alpha", "beta\n", "γάμμα\n", "line\n\n", "x"];
    let mut history: Vec<(usize, &'static str)> = Vec::new();
    for step in 0..2000 {
        let op = next() % 12;
        history.push((step, ""));
        match op {
            0 | 1 => {
                history.last_mut().unwrap().1 = "print_ascii";
                let w = words[(next() % words.len() as u64) as usize];
                output.print_ascii(w.as_bytes(), CapturedStyle::default(), DEFAULT_OUTPUT_CAP);
            }
            2 => {
                output.newline(DEFAULT_OUTPUT_CAP);
            }
            3 => {
                output.carriage_return();
            }
            4 => {
                output.backspace();
            }
            5 => {
                let mode = (next() % 3) as u16;
                output.erase_line(mode);
            }
            6 | 7 => {
                let delta = (next() % 7) as isize - 3;
                output.move_cursor_rows(delta);
            }
            8 => {
                output.set_cursor_column((next() % 20) as usize, DEFAULT_OUTPUT_CAP);
            }
            9 => {
                let delta = (next() % 11) as isize - 5;
                output.move_cursor_columns(delta, DEFAULT_OUTPUT_CAP);
            }
            10 => {
                let row = (next() % 6) as usize;
                let col = (next() % 10) as usize;
                output.goto(row, col, DEFAULT_OUTPUT_CAP);
            }
            _ => {
                history.last_mut().unwrap().1 = "print_cjk";
                output.print('字', CapturedStyle::default(), DEFAULT_OUTPUT_CAP);
            }
        }
        let recent: String = history
            .iter()
            .rev()
            .take(10)
            .map(|(s, name)| format!("{s}:{name} "))
            .collect();
        assert_ledger(&output, &format!("step {step}, op {op} | recent: {recent}"));
    }
}

/// `goto` must still materialize missing rows exactly as before — the
/// ledger replaced only the rescan, not the padding behavior.
#[test]
fn goto_pads_rows_and_ledger_matches() {
    let mut output = OutputCapture::default();
    output.print_ascii(b"one", CapturedStyle::default(), DEFAULT_OUTPUT_CAP);
    output.goto(3, 2, DEFAULT_OUTPUT_CAP);
    assert_eq!(output.as_str(), "one\n\n\n  ");
    assert_ledger(&output, "goto padding");
    assert_eq!(output.text_line_count(), 3);
}

/// `take_styled` drains the text and must reset the ledger with it.
#[test]
fn take_styled_resets_ledger() {
    let mut output = OutputCapture::default();
    output.print_ascii(b"a\nb\nc", CapturedStyle::default(), DEFAULT_OUTPUT_CAP);
    assert_eq!(output.text_line_count(), 2);
    let (text, _) = output.take_styled();
    assert_eq!(text, "a\nb\nc");
    assert_eq!(output.text_line_count(), 0, "text drained");
    // The capture continues fresh from zero.
    output.print_ascii(b"next", CapturedStyle::default(), DEFAULT_OUTPUT_CAP);
    assert_ledger(&output, "post-take append");
}

/// The renderer's tail-line formula (`lines().count().saturating_sub(1)`)
/// must equal `ledger − ends_with('\n')` for every trailing shape.
#[test]
fn tail_line_formula_matches_lines_count() {
    for text in [
        "",
        "abc",
        "abc\n",
        "abc\ndef",
        "abc\n\n",
        "abc\n\ndef",
        "\n",
        "\n\n",
        "字\n\nz",
    ] {
        let mut output = OutputCapture::default();
        output.print_ascii(
            text.as_bytes(),
            CapturedStyle::default(),
            DEFAULT_OUTPUT_CAP,
        );
        let expected = text.lines().count().saturating_sub(1);
        let computed = output
            .text_line_count()
            .saturating_sub(usize::from(output.as_str().ends_with('\n')));
        assert_eq!(
            computed, expected,
            "tail line mismatch for {text:?}: ledger formula {computed} vs lines().count()-1 {expected}"
        );
    }
}
