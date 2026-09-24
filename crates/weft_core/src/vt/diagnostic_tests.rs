//! Diagnostic experiments for OMP content-loss / border-wrap discrimination.
//!
//! Experiment 1 (content loss), two isolated scenarios, each snapshotted
//! mid-stream and after completion:
//!   a) ring-eviction isolation: 15_000 streamed lines x ~30 B (~450 KB
//!      total < 1MiB but > 10_000 physical rows => ring cap must evict the
//!      oldest head unless something else retains it).
//!   b) 1MiB-budget isolation: 6_000 lines x ~200 B (~1.2 MB total
//!      < 10_000 rows but > 1MiB => snapshot text budget must truncate the
//!      tail unless something else clips it).
//!
//! v1.10.25 (FIX_TUI_HISTORY_INCREMENTAL): exp1a/b are now REGRESSION tests —
//! they still print the raw observations (`cargo test -p weft_core diagnostic
//! -- --nocapture`) but additionally assert the fixed behavior: a) the
//! scroll-out prefix keeps line 00001 in the block text despite ring
//! eviction; b) the 1MiB split keeps lines 05991..06000 present (read across
//! the split blocks, byte-seamless). The extra tests cover the three-part
//! composed order and the capture-start rebase.
//!
//! Experiment 2 (border wrap): a box line drawn at exactly 203 grid cols,
//! then re-chunked at BlockView cols = 200 (primary TUI full-width cols
//! minus the block gutter). Observes whether the trailing `|]` folds onto a
//! continuation chunk.

use super::*;
use crate::blocks::DEFAULT_OUTPUT_CAP;
use crate::grid::terminal_text_width;

/// Grid snapshot text budget — PLAN_v11217 §3.5 (T4): derived from the
/// configured cap via `snapshot_text_budget` (the global constant is gone).
const SNAPSHOT_BUDGET: usize = crate::grid::snapshot::snapshot_text_budget(DEFAULT_OUTPUT_CAP);

fn line_a(n: usize) -> String {
    format!("line {n:05} the quick brown fox")
}

fn line_b(n: usize) -> String {
    format!("line {n:05} {}", "x".repeat(189))
}

/// Bootstrap the block tracker and activate primary-screen TUI ownership.
/// Equivalents: `repeated_primary_screen_addressing_temporarily_owns_the_grid_view`
/// and `primary_screen_history_view_snapshot_refresh_is_explicitly_coalesced`.
fn activate_primary_screen_tui(terminal: &mut Terminal) {
    terminal.process(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
    // Two absolute CUP moves => primary_screen_cursor_ops >= 2 =>
    // primary_screen_app_active() true, begin_screen_owned_output(0) runs
    // with the (pre-stream) empty grid => document_start = 0.
    terminal.process(b"\x1b[2;1H\x1b[3;1H");
    assert!(
        terminal.primary_screen_app_active(),
        "test setup: primary-screen TUI ownership not established"
    );
}

fn stream_lines(terminal: &mut Terminal, from: usize, to: usize, line_fn: fn(usize) -> String) {
    let mut buf = String::with_capacity((to - from + 1) * 64);
    for n in from..=to {
        buf.push_str(&line_fn(n));
        buf.push_str("\r\n");
    }
    terminal.process(buf.as_bytes());
}

/// Extract the `line NNNNN` number embedded in a snapshot line.
fn line_number_of(text: &str) -> Option<usize> {
    let after = text.split("line ").nth(1)?;
    after.split([' ', '\r', '\n', '\0']).next()?.parse().ok()
}

/// Observe the grid + snapshot state and the in-flight block content.
fn observe(terminal: &Terminal, tag: &str, printed: usize) {
    let doc_start = terminal.block_tracker().screen_document_start();
    let scrollback = &terminal.grid().scrollback;
    let oldest = scrollback
        .position()
        .saturating_sub(scrollback.len() as u64);
    let clamped_start = doc_start.map_or(0, |d| scrollback.index_since(d));
    let raw = doc_start.map_or_else(String::new, |d| {
        terminal
            .grid()
            .document_text_from(d, crate::blocks::DEFAULT_OUTPUT_CAP)
    });
    let first_line = raw.lines().next().unwrap_or("").to_string();
    let last_line = raw.lines().last().unwrap_or("").to_string();
    let in_flight = terminal.block_tracker().in_flight();

    println!("\n=== [{tag}] after {printed} streamed lines ===");
    println!("  scrollback.len()             = {}", scrollback.len());
    println!("  scrollback.position()        = {}", scrollback.position());
    println!("  oldest retained position     = {}", oldest);
    println!("  document_start               = {doc_start:?}");
    println!("  index_since(document_start)  = {clamped_start} (clamped; 0 means walk starts at oldest retained row)");
    println!(
        "  grid raw snapshot bytes      = {} (budget = {SNAPSHOT_BUDGET})",
        raw.len()
    );
    println!("  grid raw snapshot lines      = {}", raw.lines().count());
    println!(
        "  raw snapshot ends_with ' '   = {} (mark_snapshot_truncated space marker)",
        raw.ends_with(' ')
    );
    println!(
        "  in-flight snapshot bytes     = {:?}",
        in_flight.map(|live| live.output.len())
    );
    println!("  snapshot first line          = {:?}", first_line);
    println!("  snapshot last  line          = {:?}", last_line);
    println!(
        "  first line number            = {:?}",
        line_number_of(&first_line)
    );
    println!(
        "  last  line number            = {:?}",
        line_number_of(&last_line)
    );
    let head_present = raw.contains("line 00001");
    let tail_present = raw.contains(&format!("line {printed:05}"));
    println!("  'line 00001' in snapshot     = {head_present}");
    println!("  'line {printed:05}' (printed tail) in snapshot = {tail_present}");
    let truncated_at = line_number_of(&last_line).map(|n| n + 1);
    println!(
        "  first line number beyond snapshot = {truncated_at:?} (lines after this are missing)"
    );
}

#[test]
fn diagnostic_exp1a_scrollback_ring_eviction() {
    // Production scrollback capacity (main.rs:398 -> tab.rs:246). Terminal::new
    // already defaults to 10_000; assert it to lock the setup fact.
    let mut terminal = Terminal::new(24, 80);
    assert_eq!(
        terminal.grid().scrollback.max_lines(),
        10_000,
        "setup: production scrollback capacity must be 10_000"
    );
    activate_primary_screen_tui(&mut terminal);

    // Mid-stream: 8_000 lines x 31 B (~250 KB) < 1MiB and < 10_000 rows.
    stream_lines(&mut terminal, 1, 8_000, line_a);
    assert!(terminal.refresh_primary_history_snapshot_now());
    observe(&terminal, "exp1a mid", 8_000);

    // Full run: 15_000 lines x 31 B (~465 KB) -> ring must evict the head.
    stream_lines(&mut terminal, 8_001, 15_000, line_a);
    assert!(terminal.refresh_primary_history_snapshot_now());
    observe(&terminal, "exp1a end", 15_000);

    // v1.10.25 (FIX_TUI_HISTORY_INCREMENTAL) regression: the ring evicted
    // lines 1..4978 from the grid, but the scroll-out prefix holds them in
    // the block — ring eviction is decoupled from TUI history. The composed
    // block text must be the FULL session, in order, each line exactly once.
    let expected = full_session_text(1, 15_000, line_a);
    let live = terminal
        .block_tracker()
        .in_flight()
        .expect("in-flight block");
    assert_eq!(
        live.output,
        expected,
        "screen-owned block must keep the full session after ring eviction (head lines 00001.. in the prefix)"
    );
    assert!(live.output.contains("line 00001"));
    assert!(live.output.contains("line 15000"));
    // Settling the session finalizes the block with the same complete text.
    terminal.process(b"\x1b]133;D;0\x07");
    assert!(terminal.settle_primary_screen_exit());
    assert_eq!(
        terminal
            .block_tracker()
            .blocks()
            .last()
            .unwrap()
            .output
            .as_ref(),
        expected
    );
}

#[test]
fn diagnostic_exp1b_snapshot_budget_truncation() {
    // 200-column grid so each 200-byte line stays a single physical row
    // (no wrap), keeping rows < 10_000 while bytes > 1MiB.
    let mut terminal = Terminal::new(24, 200);
    assert_eq!(
        terminal.grid().scrollback.max_lines(),
        10_000,
        "setup: production scrollback capacity must be 10_000"
    );
    activate_primary_screen_tui(&mut terminal);

    // Mid-stream: 3_000 lines x 200 B (~600 KB) < 1MiB -> no truncation.
    stream_lines(&mut terminal, 1, 3_000, line_b);
    assert!(terminal.refresh_primary_history_snapshot_now());
    observe(&terminal, "exp1b mid", 3_000);

    // Full run: 6_000 lines x 200 B (~1.2 MB) > 1MiB -> the 1MiB split
    // settles the head as a finished block and continues the tail in the
    // in-flight block (no truncation, seamless text).
    stream_lines(&mut terminal, 3_001, 6_000, line_b);
    assert!(terminal.refresh_primary_history_snapshot_now());
    observe(&terminal, "exp1b end", 6_000);

    // v1.10.25 (FIX_TUI_HISTORY_INCREMENTAL) regression: line 05991..06000
    // must be present — read ACROSS the split blocks (cross-block
    // concatenation), with contiguous ids and byte-seamless text.
    let expected = full_session_text(1, 6_000, line_b);
    let tracker = terminal.block_tracker();
    assert!(
        !tracker.blocks().is_empty(),
        "the 1MiB split must have settled at least one finished block"
    );
    let mut concatenated = String::new();
    let mut previous_id = 0u64;
    for block in tracker.blocks() {
        assert!(
            block.id.0 > previous_id,
            "split block ids must be strictly increasing"
        );
        previous_id = block.id.0;
        concatenated.push_str(&block.output);
    }
    let live = tracker.in_flight().expect("continuation in-flight block");
    concatenated.push_str(live.output);
    assert_eq!(
        concatenated, expected,
        "finished + in-flight blocks must concatenate to the exact full session (cross-block seam)"
    );
    assert!(
        live.output.contains("line 05991")
            && live.output.contains("line 05995")
            && live.output.contains("line 06000"),
        "tail lines 05991..06000 must live in the continuation block: {}",
        &live.output[live.output.len().saturating_sub(80)..]
    );
    assert!(
        !tracker.blocks()[0].output.contains("line 05991"),
        "line 05991 must be beyond the first 1MiB block (cross-block read)"
    );
}

/// The exact expected session text for `from..=to` lines (rows joined with
/// `\n`, no trailing newline) — the strongest no-loss / no-duplication
/// oracle for the incremental capture.
fn full_session_text(from: usize, to: usize, line_fn: fn(usize) -> String) -> String {
    let mut text = String::with_capacity((to - from + 1) * 64);
    for n in from..=to {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&line_fn(n));
    }
    text
}

/// Concatenate the finished blocks and the in-flight continuation — the
/// cross-block read used by the split regression tests.
fn concatenated_block_text(terminal: &Terminal) -> String {
    let tracker = terminal.block_tracker();
    let mut concatenated = String::new();
    for block in tracker.blocks() {
        concatenated.push_str(&block.output);
    }
    concatenated.push_str(tracker.in_flight().expect("in-flight continuation").output);
    concatenated
}

/// v1.10.25 (FIX_TUI_HISTORY_INCREMENTAL): Blocker 1 — the consumed-prefix
/// byte count must not subtract the non-existent history separator when the
/// preserved-frame history is empty (the omp main scene). With the bug the
/// first split leaked the consumed '\n' back into the prefix, and the SECOND
/// refresh recomposed it at the seam — the cross-block concatenation was
/// expected + 1 byte forever (empirically 1,206,000 vs 1,205,999).
#[test]
fn split_then_second_refresh_keeps_cross_block_concatenation_byte_exact() {
    let mut terminal = Terminal::new(24, 200);
    activate_primary_screen_tui(&mut terminal);
    // 5,300 lines x 200 B: 5,276 rows pushed out of the 24-row viewport into
    // the scroll-out prefix (~1,060,475 B) — the prefix ALONE exceeds
    // DEFAULT_OUTPUT_CAP, so the first refresh must split it (history empty,
    // prefix non-empty).
    stream_lines(&mut terminal, 1, 5_300, line_b);
    assert!(terminal.refresh_primary_history_snapshot_now());
    let expected = full_session_text(1, 5_300, line_b);
    assert!(
        !terminal.block_tracker().blocks().is_empty(),
        "first refresh must settle at least one 1MiB head block"
    );
    assert_eq!(
        concatenated_block_text(&terminal),
        expected,
        "first split must be byte-seamless"
    );

    // Second refresh with no new output: the remaining prefix + viewport
    // segment is recomposed — the seam must stay exact (no leaked separator
    // byte re-emitted at the head of the remaining prefix).
    assert!(terminal.refresh_primary_history_snapshot_now());
    assert_eq!(
        concatenated_block_text(&terminal),
        expected,
        "second refresh must keep the cross-block concatenation byte-exact"
    );
}

/// v1.10.25 (FIX_TUI_HISTORY_INCREMENTAL): Blocker 2 — a head ending in '\n'
/// covers exactly `matches('\n')` line indices, not one more. The +1
/// off-by-one shifted the tail's styled lines: the tail's first line lost
/// its style and every following style landed one line up (empirically 785
/// tail text lines vs 784 styled lines). With SGR colors on every line the
/// split tail must carry one styled line per text line.
#[test]
fn split_tail_styled_line_count_matches_text_line_count() {
    let mut terminal = Terminal::new(24, 200);
    activate_primary_screen_tui(&mut terminal);
    let mut buf = String::with_capacity(5_300 * 224);
    for n in 1..=5_300 {
        buf.push_str("\x1b[31m");
        buf.push_str(&line_b(n));
        buf.push_str("\x1b[0m\r\n");
    }
    terminal.process(buf.as_bytes());
    assert!(terminal.refresh_primary_history_snapshot_now());

    let tracker = terminal.block_tracker();
    assert!(
        !tracker.blocks().is_empty(),
        "colored stream must also trigger the 1MiB split"
    );
    let live = tracker.in_flight().expect("in-flight tail");
    let text_lines = live.output.lines().count();
    let styled = live
        .styled_output
        .expect("colored tail must carry styled lines");
    assert!(
        text_lines > 0,
        "setup: tail must contain the remaining prefix rows and the viewport segment"
    );
    assert_eq!(
        styled.lines.len(),
        text_lines,
        "tail styled line count must equal tail text line count (no +1 off-by-one)"
    );
    assert!(
        styled.line(0).is_some(),
        "tail's FIRST line must keep its style (was dropped by the +1 shift)"
    );
    assert!(
        styled.line(text_lines - 1).is_some(),
        "tail's LAST line must keep its style"
    );
}

/// v1.10.25 (FIX_TUI_HISTORY_INCREMENTAL): Should-fix 3 — when the live
/// viewport segment ALONE exceeds 1MiB (no chunkable history/prefix remains),
/// splitting would cut the segment, and the next refresh re-captures the
/// whole segment — the settled rows duplicate and the composed text grows
/// without bound (empirically 1,049,780 -> 2,097,380). The split must
/// degrade to the old snapshot truncation (head-keeping, no chunking), so
/// repeated refreshes keep the total bounded.
#[test]
fn giant_viewport_segment_truncates_instead_of_growing_across_refreshes() {
    // 3,000 rows x 360 cols: the viewport holds the whole session, so no
    // scroll-out prefix exists — the composed text IS the segment, and it
    // exceeds DEFAULT_OUTPUT_CAP (~1.08 MB).
    let mut terminal = Terminal::new(3_000, 360);
    activate_primary_screen_tui(&mut terminal);
    let mut buf = String::with_capacity(3_000 * 370);
    for n in 1..=3_000 {
        buf.push_str(&format!("line {n:05} {}", "x".repeat(349)));
        buf.push_str("\r\n");
    }
    terminal.process(buf.as_bytes());
    assert!(terminal.refresh_primary_history_snapshot_now());

    // The segment must be truncated at the 1MiB budget, NOT settled into a
    // finished block: the in-flight tail holds the truncated segment (≤ MAX),
    // and the concatenation may only exceed MAX by the chunkable parts that
    // preceded it (the split drained them from the tracker, so the pre-split
    // prefix is not observable here).
    let first = concatenated_block_text(&terminal);
    let tail = terminal
        .block_tracker()
        .in_flight()
        .expect("in-flight tail");
    assert!(
        tail.output.len() <= DEFAULT_OUTPUT_CAP,
        "giant segment must be truncated at the 1MiB budget in the in-flight tail, got {} bytes",
        tail.output.len()
    );
    assert!(
        first.len() <= DEFAULT_OUTPUT_CAP + 4096,
        "concatenation must stay near the 1MiB budget (chunkable prefix parts only), got {} bytes",
        first.len()
    );
    assert!(
        first.contains("line 00001"),
        "truncation must keep the session head"
    );
    assert!(
        !first.contains("line 03000"),
        "truncation must drop the session tail"
    );

    // Another refresh with no new output must NOT double the content (the
    // pre-fix behavior settled a fresh 1MiB head every refresh: 1,049,780 ->
    // 2,097,380).
    assert!(terminal.refresh_primary_history_snapshot_now());
    let second = concatenated_block_text(&terminal);
    assert_eq!(
        second, first,
        "repeated refreshes must not grow the giant-segment truncation"
    );
}

/// v1.10.25 (FIX_TUI_HISTORY_INCREMENTAL): the three-part composed block text
/// must be preserved-frame history + scroll-out prefix + viewport snapshot in
/// that order — the frame preservation (Phase 2) and the incremental capture
/// coexist without duplication.
#[test]
fn three_part_compose_orders_frames_then_prefix_then_viewport() {
    let mut terminal = Terminal::new(5, 40);
    terminal.process(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07\x1b[H\x1b[2;1H");
    // Stream rows out of the viewport -> old row one is captured into the
    // scroll-out prefix; old rows two..six stay in the viewport.
    terminal.process(
        b"old row one\r\nold row two\r\nold row three\r\nold row four\r\nold row five\r\nold row six\r\n",
    );
    assert!(
        terminal.block_tracker().screen_prefix_len() > 0,
        "prefix must hold the pushed row"
    );
    // Synchronized full repaint with CSI 2J: the superseded viewport (old
    // rows two..six) is preserved as a history frame BEFORE the clear.
    terminal.process(b"\x1b[?2026h\x1b[2J\x1b[Hnew row one\x1b[?2026l");
    // Post-repaint streaming scrolls the new rows out -> more prefix rows.
    terminal.process(
        b"new row two\r\nnew row three\r\nnew row four\r\nnew row five\r\nnew row six\r\n",
    );
    assert!(terminal.refresh_primary_history_snapshot_now());

    let output = terminal.block_tracker().in_flight().unwrap().output;
    let frame_pos = output.find("old row two").expect("preserved frame");
    let prefix_pos = output.find("new row one").expect("prefix row");
    let snapshot_pos = output.find("new row two").expect("snapshot row");
    assert!(
        frame_pos < prefix_pos && prefix_pos < snapshot_pos,
        "composed order must be frames < prefix < viewport: {output:?}"
    );
    // The preserved frame and the prefix must not duplicate each other.
    for row in [
        "old row one",
        "old row two",
        "old row six",
        "new row one",
        "new row two",
        "new row six",
    ] {
        assert_eq!(
            output.matches(row).count(),
            1,
            "row {row} must appear exactly once"
        );
    }
}

/// v1.10.25 (FIX_TUI_HISTORY_INCREMENTAL): rows the TUI painted BEFORE the
/// capture threshold (retained owned rows at capture start) are folded into
/// the prefix by the capture-start rebase, so the composed transcript keeps
/// document order — older pre-capture rows before newer streamed rows.
#[test]
fn pre_capture_owned_rows_precede_streamed_rows_in_document_order() {
    let mut terminal = Terminal::new(5, 40);
    // Shell integration, then print rows WITHOUT cursor addressing yet.
    terminal.process(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
    terminal.process(b"pre-capture banner\r\n");
    // One cursor op only (below the 2-op threshold) — the capture has not
    // started, but the printed row is owned.
    terminal.process("\x1b[2;1H".as_bytes());
    terminal.process(b"pre-capture content\r\n");
    // Second cursor op -> the capture starts; the retained owned rows must be
    // rebased into the prefix.
    terminal.process("\x1b[3;1H".as_bytes());
    assert!(terminal.primary_screen_app_active());
    // Post-capture streaming.
    terminal.process(b"streamed row after capture\r\n");
    assert!(terminal.refresh_primary_history_snapshot_now());

    let output = terminal.block_tracker().in_flight().unwrap().output;
    let pre_pos = output.find("pre-capture banner").expect("pre-capture row");
    let streamed_pos = output
        .find("streamed row after capture")
        .expect("streamed row");
    assert!(
        pre_pos < streamed_pos,
        "pre-capture owned rows must precede newer streamed rows: {output:?}"
    );
    assert_eq!(output.matches("pre-capture banner").count(), 1);
    assert_eq!(output.matches("pre-capture content").count(), 1);
}

#[test]
fn diagnostic_exp2_border_wrap_203_cols_core() {
    let mut terminal = Terminal::new(24, 203);
    activate_primary_screen_tui(&mut terminal);

    // A single row that fills exactly 203 display columns (the primary TUI's
    // full-width grid; only content owned by the app is captured).
    let box_line = format!("[|{}|]", "x".repeat(199));
    let width = terminal_text_width(&box_line);
    assert_eq!(
        width, 203,
        "setup: box line must measure exactly 203 display columns"
    );
    terminal.process(format!("\x1b[1;1H{box_line}").as_bytes());
    assert!(terminal.primary_screen_app_active());
    assert!(terminal.refresh_primary_history_snapshot_now());

    let snapshot = terminal
        .block_tracker()
        .in_flight()
        .map(|live| live.output.to_string())
        .unwrap_or_default();
    let captured = snapshot
        .lines()
        .find(|line| line.starts_with("[|"))
        .unwrap_or("");
    println!("\n=== [exp2 core] 203-col box line in snapshot ===");
    println!("  snapshot total bytes      = {}", snapshot.len());
    println!("  captured line bytes       = {}", captured.len());
    println!(
        "  captured first 10 chars   = {:?}",
        &captured[..captured.len().min(10)]
    );
    println!(
        "  captured last 10 chars    = {:?}",
        &captured[captured.len().saturating_sub(10)..]
    );
    println!(
        "  captured terminal width   = {}",
        terminal_text_width(captured)
    );
    println!("  captured ends_with '|]'   = {}", captured.ends_with("|]"));
    println!("  captured chars            = {}", captured.chars().count());
}

/// v1.10.26 (FIX_IME_PREEDIT): the BlockView caret anchor must be derived
/// from the composed in-flight text itself (same-frame, same-source), not
/// from independently-recomputed head counts. With a scroll-out prefix
/// captured before the publish, `primary_screen_cursor_snapshot_line` must
/// resolve to exactly the grid cursor row inside the composed document.
#[test]
fn caret_anchor_tracks_composed_live_rows_with_prefix() {
    let mut terminal = Terminal::new(24, 200);
    activate_primary_screen_tui(&mut terminal);
    // ~600 KB (< 1MiB — no split). Streaming more lines than the viewport
    // pushes owned rows into the scroll-out prefix, so the composed document
    // has a real (non-empty) head the caret must be offset past.
    stream_lines(&mut terminal, 1, 3_000, line_b);
    terminal.process("\x1b[13;1H".as_bytes());
    assert!(terminal.refresh_primary_history_snapshot_now());

    let caret_line = terminal
        .primary_screen_cursor_snapshot_line()
        .expect("caret snapshot line");
    let live = terminal
        .block_tracker()
        .in_flight()
        .expect("in-flight block");
    let lines: Vec<&str> = live.output.split('\n').collect();
    assert!(
        caret_line < lines.len(),
        "caret_line {caret_line} out of the composed document ({} lines)",
        lines.len()
    );
    // The composed line at the caret must carry the grid cursor row's line —
    // the exact "caret_line == composed cursor line index" invariant.
    let grid_row = terminal.grid().row_text(12).trim_end().to_string();
    let grid_line = line_number_of(&grid_row).expect("grid cursor row has a streamed line");
    let composed_line =
        line_number_of(lines[caret_line]).expect("composed caret line has a streamed line");
    assert_eq!(
        composed_line, grid_line,
        "caret must resolve to the grid cursor row inside the composed document"
    );
}

/// v1.10.26 (FIX_IME_PREEDIT): after a 1MiB split the caret must anchor into
/// the CONTINUATION tail (the in-flight block the BlockView paints), not the
/// pre-split offset into the full text (which would point past the tail and
/// never match `tui_caret_row_matches`).
#[test]
fn caret_anchor_survives_1mb_split_in_flight_tail() {
    let mut terminal = Terminal::new(24, 200);
    activate_primary_screen_tui(&mut terminal);
    // > 1MiB total; the prefix ALONE exceeds the budget, so the first refresh
    // must settle finished blocks and continue the tail in-flight.
    stream_lines(&mut terminal, 1, 5_300, line_b);
    assert!(terminal.refresh_primary_history_snapshot_now());
    assert!(
        !terminal.block_tracker().blocks().is_empty(),
        "test setup: the 1MiB split must settle a head block"
    );
    terminal.process("\x1b[13;1H".as_bytes());
    assert!(terminal.refresh_primary_history_snapshot_now());

    let caret_line = terminal
        .primary_screen_cursor_snapshot_line()
        .expect("caret snapshot line");
    let live = terminal
        .block_tracker()
        .in_flight()
        .expect("in-flight tail");
    let lines: Vec<&str> = live.output.split('\n').collect();
    assert!(
        caret_line < lines.len(),
        "caret_line {caret_line} out of the split tail ({} lines)",
        lines.len()
    );
    let grid_row = terminal.grid().row_text(12).trim_end().to_string();
    let grid_line = line_number_of(&grid_row).expect("grid cursor row has a streamed line");
    let composed_line =
        line_number_of(lines[caret_line]).expect("composed caret line has a streamed line");
    assert_eq!(
        composed_line, grid_line,
        "after the split the caret must resolve to the grid cursor row inside the continuation tail"
    );
}

/// v1.10.26 review B1 (FIX_IME_PREEDIT): the Ctrl-C interrupt window used
/// to leave `primary_screen_cursor_segment_len` describing the PREVIOUS
/// string while the merged transcript replaced the in-flight output — a
/// mid-window keypress then sliced at a stale offset and panicked on
/// non-char-boundary under CJK content. The interrupt publish must pair
/// the output with its segment length, and the head slice must back off
/// to a char boundary as defense in depth.
#[test]
fn caret_survives_interrupt_window_with_cjk_content() {
    let mut terminal = Terminal::new(24, 200);
    activate_primary_screen_tui(&mut terminal);
    // CJK makes every offset-sensitive bug a char-boundary panic instead
    // of a silent wrong line.
    let cjk = |n: usize| format!("line {n} 快快快快快 快快快快快");
    stream_lines(&mut terminal, 1, 100, cjk);
    terminal.process(b"\x1b[13;1H".as_slice());
    assert!(terminal.refresh_primary_history_snapshot_now());

    terminal.begin_primary_screen_interrupt_capture();
    // Tail output lands inside the window. The newlines matter: a stale
    // segment pairing counts them into the head and shifts the caret.
    terminal.process("快\n快\nx\n".as_bytes());
    // Publish the merged transcript while the window is still open, then
    // move the cursor and refresh the caret from a keystroke path — this
    // exact sequence panicked at freeze.rs before the fix.
    assert!(terminal.refresh_primary_history_snapshot_now());
    terminal.process(b"\x1b[5;1H".as_slice());
    terminal.snapshot_primary_screen_output_for_caret();

    let caret_line = terminal
        .primary_screen_cursor_snapshot_line()
        .expect("caret line inside the interrupt window");
    let live = terminal.block_tracker().in_flight().expect("in-flight");
    let lines: Vec<&str> = live.output.split('\n').collect();
    assert!(
        caret_line < lines.len(),
        "caret_line {caret_line} out of the merged transcript ({} lines)",
        lines.len()
    );
    // 100 lines each followed by a trailing newline scroll the screen once
    // past it: rows 1..24 hold lines 78..100 + the cursor row, so
    // cursor row 5 (1-based) is line 82. A stale segment pairing shifts the
    // head count and lands the caret on the wrong content row even when it
    // stays in bounds.
    assert!(
        lines[caret_line].starts_with("line 82 "),
        "caret must anchor to the grid cursor row's content, got {:?} (caret_line {caret_line})",
        lines[caret_line]
    );
    terminal.cancel_primary_screen_interrupt_capture();
}

/// v1.10.26 (FIX_IME_PREEDIT): the disappearing-caret regression. The
/// scroll-out prefix grows between 50ms-rate-limited publishes but the
/// in-flight block text is STALE until the next publish. The keystroke caret
/// refresh (`snapshot_primary_screen_output_for_caret`) previously re-added
/// the grown prefix count against the stale text — the caret row jumped past
/// the last painted row, `tui_caret_row_matches` never matched, and the
/// caret + IME preedit silently vanished. The anchor must come from the
/// published text structure itself and stay in-bounds of what BlockView
/// paints.
#[test]
fn caret_keeps_matching_stale_live_rows_when_prefix_grows_between_publishes() {
    let mut terminal = Terminal::new(24, 200);
    activate_primary_screen_tui(&mut terminal);
    // First publish: 100 lines (77 pushed out into the prefix) — the composed
    // in-flight text is exactly 100 lines.
    stream_lines(&mut terminal, 1, 100, line_a);
    terminal.process("\x1b[13;1H".as_bytes());
    assert!(terminal.refresh_primary_history_snapshot_now());
    let stale_line_count = terminal
        .block_tracker()
        .in_flight()
        .expect("in-flight")
        .output
        .lines()
        .count();

    // Stream MORE output but DO NOT publish (the app's 50ms snapshot window
    // is still open): the prefix grows in the tracker while the in-flight
    // text stays at the last published frame.
    stream_lines(&mut terminal, 101, 200, line_a);
    terminal.process("\x1b[13;1H".as_bytes());
    terminal.snapshot_primary_screen_output_for_caret();

    let caret_line = terminal
        .primary_screen_cursor_snapshot_line()
        .expect("keystroke caret snapshot line");
    let live = terminal
        .block_tracker()
        .in_flight()
        .expect("in-flight block (stale, not yet re-published)");
    let lines: Vec<&str> = live.output.split('\n').collect();
    assert_eq!(
        lines.len(),
        stale_line_count,
        "a keystroke that does not publish must not mutate the in-flight text"
    );
    assert!(
        caret_line < lines.len(),
        "caret_line {caret_line} out of the stale composed text ({} lines): the caret must anchor to what BlockView paints (the stale frame), not the grown prefix count",
        lines.len()
    );
    assert!(
        lines[caret_line].starts_with("line "),
        "caret must land on a painted content row, got {:?} (caret_line {caret_line})",
        lines[caret_line]
    );
}

/// v1.10.26 post-release forensics (omp preedit still invisible): the
/// caret snapshot line is only ever SET, never cleared. When a screen-owned
/// session SETTLES and the next command streams in block view WITHOUT screen
/// ownership, `block_view_tui_cursor` (renderer.rs) prefers the stale
/// tracked line over the formula — the anchor then points past the new live
/// block and `tui_caret_row_matches` never fires, so the caret + IME preedit
/// vanish together even though the Batch C anchor math is correct.
#[test]
fn caret_snapshot_line_must_not_leak_across_settled_command_boundary() {
    let mut terminal = Terminal::new(24, 200);
    activate_primary_screen_tui(&mut terminal);
    stream_lines(&mut terminal, 1, 100, line_a);
    terminal.process("\x1b[13;1H".as_bytes());
    assert!(terminal.refresh_primary_history_snapshot_now());
    let stale = terminal
        .primary_screen_cursor_snapshot_line()
        .expect("screen-owned snapshot tracked a caret line");
    assert!(
        stale > 0,
        "setup: the settled session must leave a non-trivial line"
    );

    // The session settles (log: "settled primary-screen command finalization").
    terminal.process(b"\x1b]133;D;0\x07");
    assert!(terminal.settle_primary_screen_exit());
    assert!(
        terminal.block_tracker().in_flight().is_none(),
        "setup: settle must finalize the in-flight block"
    );

    // A NEW command streams WITHOUT screen ownership (plain output; no CUP
    // ops) — the omp input-box window in block view.
    terminal.process(b"\x1b]133;A\x07\x1b]133;B\x07next\x1b]133;C\x07");
    terminal.process(b"hello\r\nworld\r\n");
    assert!(terminal.block_tracker().in_flight().is_some());
    assert!(!terminal.primary_screen_app_active());
    assert!(terminal.show_block_view());

    // Every keystroke refreshes the caret anchor (the app calls this in
    // tab.rs for block view). For a NON-screen-owned command it must not
    // keep the settled session's line — the renderer prefers the tracked
    // value over the formula, so a stale anchor never matches the new live
    // rows and the preedit is never painted.
    terminal.snapshot_primary_screen_output_for_caret();

    let line = terminal.primary_screen_cursor_snapshot_line();
    let live = terminal.block_tracker().in_flight().expect("live block");
    let line_count = live.output.lines().count();
    assert!(
        line.is_none_or(|l| l < line_count),
        "caret snapshot line {line:?} leaked from the settled session into the new block \
         ({} lines): tui_caret_row_matches can never fire, caret + preedit invisible",
        line_count
    );
}
