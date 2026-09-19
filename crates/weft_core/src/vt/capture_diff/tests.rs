//! Differential harness for the crate-internal capture channels (staging and
//! interrupt). See `mod.rs` for the channel contract.

use crate::vt::Terminal;

/// trim_end every line, then drop leading/trailing empty lines — the same
/// line-domain canonical form as `tests/capture_snapshot_equivalence.rs`
/// (integration binaries cannot share items, so this is a local copy).
fn normalize(mut lines: Vec<String>) -> Vec<String> {
    for line in &mut lines {
        *line = line.trim_end().to_string();
    }
    while lines.first().is_some_and(|line| line.is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}

/// Live primary-grid truth: `displayed_row_text` over the whole viewport
/// (scroll_offset stays 0 in these scenarios).
fn grid_lines(terminal: &Terminal) -> Vec<String> {
    let grid = terminal.grid();
    normalize(
        (0..grid.num_rows)
            .map(|row| grid.displayed_row_text(row))
            .collect(),
    )
}

fn text_lines(text: &str) -> Vec<String> {
    normalize(text.split('\n').map(str::to_string).collect())
}

/// Staging channel differential (FIX_ORPHAN_PARSE_ERROR_OUTPUT path): the
/// bytes printed between editor submission and the orphan `133;D` are
/// mirrored into `preexec_staging` AND the Grid by the same print events.
/// All three truths must agree: staged text == Grid rows == synthesized
/// block output.
#[test]
fn staging_channel_mirrors_the_grid_print_stream() {
    let mut terminal = Terminal::new(6, 40);
    terminal.process(b"\x1b]133;A\x07");
    // Editor submission arms staging (command_from_editor = Some, AtPrompt).
    let _submit_bytes = terminal.run_command("badcmd");
    // The shell rejects the line before preexec — no `133;B` ever arrives.
    terminal.process(b"zsh: bad pattern\ndetail column here");

    // Differential, pre-finalize: staged mirror == grid print stream.
    let staged = terminal.preexec_staging.as_str().to_string();
    assert_eq!(
        text_lines(&staged),
        grid_lines(&terminal),
        "staging must mirror the grid print stream byte-faithfully"
    );

    // The closing `133;D` finds no pending command and synthesizes the block
    // from the staged bytes — the third truth.
    terminal.process(b"\x1b]133;D;127\x07");
    let block = terminal
        .block_tracker()
        .blocks()
        .last()
        .expect("orphan 133;D must synthesize a block");
    assert_eq!(block.command, "badcmd");
    assert_eq!(block.exit_code, Some(127));
    assert_eq!(
        block.output.as_ref(),
        staged.trim_end(),
        "block output must be the staged text (finalize trim_end only)"
    );
    assert_eq!(text_lines(block.output.as_ref()), grid_lines(&terminal));
}

/// Interrupt channel differential (Ctrl-C frozen transcript): prints inside
/// the interrupt window are double-fed — the Grid AND the interrupt tail —
/// while the frozen half is the snapshot façade at the interrupt instant.
/// Tail == post-freeze grid rows, frozen text == pre-freeze frame, and the
/// merged product (via the pub refresh) carries both.
#[test]
fn interrupt_tail_matches_the_plain_print_stream() {
    let mut terminal = Terminal::new(6, 40);
    terminal.process(b"\x1b]133;A\x07\x1b]133;B\x07tui\x1b]133;C\x07");
    // Two absolute CUPs → screen ownership (plain print capture switches off;
    // the snapshot channel becomes the block's content source).
    terminal.process(b"\x1b[H\x1b[2;1H");
    assert!(
        terminal.block_tracker().screen_document_start().is_some(),
        "precondition: screen ownership engaged"
    );
    terminal.process(b"frame one\nframe two");

    terminal.begin_primary_screen_interrupt_capture();
    assert!(terminal.primary_screen_interrupt_capture_active());

    // Double-fed window: every byte below reaches the Grid AND the tail sink.
    terminal.process(b"\ntail A\ntail B");

    let capture = terminal
        .capabilities
        .primary_screen_interrupt_capture
        .as_ref()
        .expect("interrupt capture active");

    // Truth 1 — the frozen half is the interrupt-instant document: the
    // command echo ("tui", printed before screen ownership) plus the frame.
    assert_eq!(
        text_lines(&capture.frozen_text),
        vec![
            "tui".to_string(),
            "frame one".to_string(),
            "frame two".to_string()
        ],
        "frozen text must be the interrupt-instant snapshot"
    );
    // Truth 2 — the tail is the post-freeze print stream, equal to the grid
    // rows written after the freeze. The frame CUP put the cursor on row 1,
    // so the frame occupies rows 1-2 and the tail lands on rows 3-4.
    assert_eq!(
        text_lines(capture.tail.as_str()),
        vec!["tail A".to_string(), "tail B".to_string()],
        "tail must mirror the post-freeze print stream"
    );
    let grid = terminal.grid();
    let post_freeze = normalize(
        (3..5)
            .map(|row| grid.displayed_row_text(row))
            .collect::<Vec<_>>(),
    );
    assert_eq!(text_lines(capture.tail.as_str()), post_freeze);

    // Truth 3 — the merged product the block actually renders (pub refresh
    // path) carries both halves.
    assert!(
        terminal.refresh_primary_history_snapshot_now(),
        "screen-owned snapshot must refresh inside the interrupt window"
    );
    let live = terminal
        .block_tracker()
        .in_flight()
        .expect("screen-owned command stays in flight");
    assert!(
        live.output.contains("frame one") && live.output.contains("tail B"),
        "merged transcript must carry frozen frame AND tail: {:?}",
        live.output
    );

    terminal.cancel_primary_screen_interrupt_capture();
    assert!(!terminal.primary_screen_interrupt_capture_active());
}
