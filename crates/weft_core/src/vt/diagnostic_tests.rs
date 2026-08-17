//! Diagnostic experiments for OMP content-loss / border-wrap discrimination.
//!
//! OBSERVATION ONLY: these tests print raw values (run with
//! `cargo test -p weft_core diagnostic -- --nocapture`) and assert only
//! objective facts (setup invariants, measured byte counts). They do NOT
//! lock any hypothesis about whether content loss comes from scrollback ring
//! eviction vs the 1MiB snapshot budget, or where `|]` lands after re-wrap.
//! No production behavior is modified.
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
//! Experiment 2 (border wrap): a box line drawn at exactly 203 grid cols,
//! then re-chunked at BlockView cols = 200 (primary TUI full-width cols
//! minus the block gutter). Observes whether the trailing `|]` folds onto a
//! continuation chunk.

use super::*;
use crate::blocks::MAX_OUTPUT_BYTES;
use crate::grid::terminal_text_width;

/// Grid snapshot text budget (`grid::snapshot::SNAPSHOT_TEXT_BUDGET`).
const SNAPSHOT_BUDGET: usize = MAX_OUTPUT_BYTES + 4;

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
    after
        .split([' ', '\r', '\n', '\0'])
        .next()?
        .parse()
        .ok()
}

/// Observe the grid + snapshot state and the in-flight block content.
fn observe(terminal: &Terminal, tag: &str, printed: usize) {
    let doc_start = terminal.block_tracker().screen_document_start();
    let scrollback = &terminal.grid().scrollback;
    let oldest = scrollback.position().saturating_sub(scrollback.len() as u64);
    let clamped_start = doc_start.map_or(0, |d| scrollback.index_since(d));
    let raw = doc_start.map_or_else(String::new, |d| terminal.grid().document_text_from(d));
    let first_line = raw.lines().next().unwrap_or("").to_string();
    let last_line = raw.lines().last().unwrap_or("").to_string();
    let in_flight = terminal.block_tracker().in_flight();

    println!("\n=== [{tag}] after {printed} streamed lines ===");
    println!("  scrollback.len()             = {}", scrollback.len());
    println!("  scrollback.position()        = {}", scrollback.position());
    println!("  oldest retained position     = {}", oldest);
    println!("  document_start               = {doc_start:?}");
    println!("  index_since(document_start)  = {clamped_start} (clamped; 0 means walk starts at oldest retained row)");
    println!("  grid raw snapshot bytes      = {} (budget = {SNAPSHOT_BUDGET})", raw.len());
    println!("  grid raw snapshot lines      = {}", raw.lines().count());
    println!("  raw snapshot ends_with ' '   = {} (mark_snapshot_truncated space marker)", raw.ends_with(' '));
    println!("  in-flight snapshot bytes     = {:?}", in_flight.map(|live| live.output.len()));
    println!("  snapshot first line          = {:?}", first_line);
    println!("  snapshot last  line          = {:?}", last_line);
    println!("  first line number            = {:?}", line_number_of(&first_line));
    println!("  last  line number            = {:?}", line_number_of(&last_line));
    let head_present = raw.contains("line 00001");
    let tail_present = raw.contains(&format!("line {printed:05}"));
    println!("  'line 00001' in snapshot     = {head_present}");
    println!("  'line {printed:05}' (printed tail) in snapshot = {tail_present}");
    let truncated_at = line_number_of(&last_line).map(|n| n + 1);
    println!("  first line number beyond snapshot = {truncated_at:?} (lines after this are missing)");
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

    // Mid-stream: 3_000 lines x 201 B (~603 KB) < 1MiB -> no truncation.
    stream_lines(&mut terminal, 1, 3_000, line_b);
    assert!(terminal.refresh_primary_history_snapshot_now());
    observe(&terminal, "exp1b mid", 3_000);

    // Full run: 6_000 lines x 201 B (~1.2 MB) > 1MiB -> budget truncates.
    stream_lines(&mut terminal, 3_001, 6_000, line_b);
    assert!(terminal.refresh_primary_history_snapshot_now());
    observe(&terminal, "exp1b end", 6_000);
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
    println!("  captured first 10 chars   = {:?}", &captured[..captured.len().min(10)]);
    println!(
        "  captured last 10 chars    = {:?}",
        &captured[captured.len().saturating_sub(10)..]
    );
    println!("  captured terminal width   = {}", terminal_text_width(captured));
    println!("  captured ends_with '|]'   = {}", captured.ends_with("|]"));
    println!("  captured chars            = {}", captured.chars().count());
}