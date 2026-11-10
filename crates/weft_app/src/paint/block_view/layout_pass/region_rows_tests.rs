//! v1.19.0 (PLAN_v1190 §4 P1-2/P1-5/P1-6/P1-8) region-track assembly tests.
//! Mounted off `region_pass.rs` (layout_pass.rs sits at its line budget), so
//! these run inside the `region_pass` module subtree: layout_pass items are
//! reached through their `pub(super)` crate paths.
//!
//! Pinned behaviors:
//! - wrapped logical rows land as one slot chunked at `cols` (P1-5 shape);
//! - leading/trailing blank logical rows drop at assembly with display-domain
//!   renumbering (P1-6);
//! - N7: the slot's `StyledLine` borrows straight into `LaidRow::Output`;
//! - structural rows (Blank/LiveCommand/LiveHeader/Separator) keep the
//!   capture branch's distances;
//! - flag-off identity: `region: None` keeps the capture branch byte-shape;
//! - capture-vs-region track alignment on a real terminal session;
//! - `region_display_chunk_total` (P1-8 metrics) equals the assembly's
//!   display-row total.

use std::collections::HashMap;
use std::time::SystemTime;

use weft_core::blocks::{Block, BlockId, InFlightBlock};
use weft_core::vt::Terminal;

use crate::app_state::BlockDiagnoseState;
use crate::paint::block_view::layout_pass::{compute_block_layout_pass, LaidRow, LayoutPassInput};
use crate::paint::grid_cache::BlockLayoutCache;
use crate::paint::live_cache::LiveLayoutCache;
use crate::paint::region_cache::{
    region_display_chunk_total, region_source_for, PaneRegionState, RegionLiveCache, RegionSlot,
};

// ── fixtures ────────────────────────────────────────────────────────────

fn slot(text: &str, start_idx: u64, end_idx: u64) -> RegionSlot {
    RegionSlot {
        text: text.to_string(),
        style: weft_core::blocks::StyledLine::default(),
        start_idx,
        end_idx,
    }
}

/// A hand-built [`PaneRegionState`]: `slots`/`dyn_slots` verbatim, the chunk
/// prefix sums of the immutable prefix derived at `cols` (mirrors
/// `rebuild_cumulative`).
fn state(slots: Vec<RegionSlot>, dyn_slots: Vec<RegionSlot>, cols: usize) -> PaneRegionState {
    let mut cumulative = vec![0u32];
    for s in &slots {
        let last = *cumulative.last().unwrap();
        cumulative.push(last + crate::paint::live_cache::count_line_chunks(&s.text, cols));
    }
    PaneRegionState {
        begin_position: 0,
        grid_revision: 0,
        cols,
        anchor_idx: slots.last().map_or(0, |s| s.end_idx),
        anchor_position: 0,
        slots,
        dyn_slots,
        cumulative,
    }
}

fn live_block<'a>(command: &'a str, output: &'a str) -> InFlightBlock<'a> {
    InFlightBlock {
        command,
        cwd: None,
        output,
        styled_output: None,
        version: 1,
        screen_origin: false,
        min_write_offset: InFlightBlock::detached_watermark(),
        started_at: SystemTime::UNIX_EPOCH,
    }
}

#[allow(clippy::too_many_arguments)]
fn make_input<'a, 'b, 'c>(
    blocks: &'a [Block],
    live: Option<InFlightBlock<'a>>,
    region: Option<crate::paint::region_cache::RegionSource<'c>>,
    region_state: Option<&'a PaneRegionState>,
    cols: usize,
    block_diagnose_state: &'b HashMap<BlockId, BlockDiagnoseState>,
) -> LayoutPassInput<'a, 'b, 'c> {
    LayoutPassInput {
        blocks,
        live,
        pane_session_id: 1,
        cwd: None,
        git_branch: None,
        block_scroll: 0.0,
        viewport_rows: 6,
        cols,
        pitch: 10.0,
        header_height: 24.0,
        content_bottom_y: 120.0,
        clip_top: 0.0,
        clip_bottom: 120.0,
        resolve_styles: false,
        styled_lookup_counter: None,
        block_diagnose_state,
        now: SystemTime::UNIX_EPOCH,
        region,
        region_state,
    }
}

/// Output rows as `(line_text, text, line, chunk_idx)` for compact asserts.
fn output_rows<'a>(row_data: &'a [LaidRow<'a>]) -> Vec<(&'a str, &'a str, usize, usize)> {
    row_data
        .iter()
        .filter_map(|row| match row {
            LaidRow::Output {
                line_text,
                text,
                line,
                chunk_idx,
                ..
            } => Some((*line_text, text.as_ref(), *line, *chunk_idx)),
            _ => None,
        })
        .collect()
}

// ── P1-5: wrapped logical row → one slot chunked at cols ───────────────

#[test]
fn wrapped_logical_row_chunks_into_visual_rows() {
    let st = state(
        vec![slot("aaaaaaaaaabbbbbbbbbbcccccccccc", 0, 1)],
        Vec::new(),
        10,
    );
    let live = live_block("cmd", "");
    let seg = crate::paint::block_view::layout_pass::region_rows::region_live_segment(
        &live,
        &st,
        10,
        10.0,
        24.0,
        120.0,
        0.0,
        0.0,
        120.0,
        false,
        &mut |_| {},
        SystemTime::UNIX_EPOCH,
        None,
        None,
    );
    let rows = output_rows(&seg.row_data);
    assert_eq!(
        rows,
        vec![
            ("aaaaaaaaaabbbbbbbbbbcccccccccc", "aaaaaaaaaa", 0, 0),
            ("aaaaaaaaaabbbbbbbbbbcccccccccc", "bbbbbbbbbb", 0, 1),
            ("aaaaaaaaaabbbbbbbbbbcccccccccc", "cccccccccc", 0, 2),
        ],
        "one wrapped logical row must chunk into 3 visual rows, all line=0"
    );
    assert_eq!(seg.total_display_rows, 3);
    assert_eq!(seg.total_logical_rows, 1);
}

// ── P1-6: blank rows drop at assembly, display-domain renumbering ──────

#[test]
fn leading_and_trailing_blank_slots_drop_and_renumber() {
    let st = state(
        vec![
            slot("", 0, 1),
            slot("alpha", 1, 2),
            slot("beta", 2, 3),
            slot("", 3, 4),
        ],
        Vec::new(),
        80,
    );
    let live = live_block("cmd", "");
    let seg = crate::paint::block_view::layout_pass::region_rows::region_live_segment(
        &live,
        &st,
        80,
        10.0,
        24.0,
        120.0,
        0.0,
        0.0,
        120.0,
        false,
        &mut |_| {},
        SystemTime::UNIX_EPOCH,
        None,
        None,
    );
    let rows = output_rows(&seg.row_data);
    assert_eq!(
        rows,
        vec![("beta", "beta", 1, 0), ("alpha", "alpha", 0, 0)],
        "blanks drop; `line` renumbers into the display domain (0,1); \
         emission is newest-first (capture-track parity)"
    );
    assert_eq!(seg.total_display_rows, 2);
    assert_eq!(seg.total_logical_rows, 2);
}

#[test]
fn interior_blank_rows_survive_but_edge_blanks_do_not() {
    let st = state(
        vec![slot("alpha", 0, 1), slot("", 1, 2), slot("beta", 2, 3)],
        Vec::new(),
        80,
    );
    let live = live_block("cmd", "");
    let seg = crate::paint::block_view::layout_pass::region_rows::region_live_segment(
        &live,
        &st,
        80,
        10.0,
        24.0,
        120.0,
        0.0,
        0.0,
        120.0,
        false,
        &mut |_| {},
        SystemTime::UNIX_EPOCH,
        None,
        None,
    );
    let rows = output_rows(&seg.row_data);
    assert_eq!(rows.len(), 3, "interior blank keeps its (empty) chunk row");
    assert_eq!(rows[1].1, "", "interior blank is an empty chunk");
    // Newest-first emission (capture-track parity): the oldest slot lands
    // last, the interior blank keeps its middle display position.
    assert_eq!(
        rows,
        vec![
            ("beta", "beta", 2, 0),
            ("", "", 1, 0),
            ("alpha", "alpha", 0, 0),
        ]
    );
}

#[test]
fn all_blank_region_emits_only_structural_rows() {
    let st = state(vec![slot("", 0, 1), slot("", 1, 2)], Vec::new(), 80);
    let live = live_block("cmd", "");
    let seg = crate::paint::block_view::layout_pass::region_rows::region_live_segment(
        &live,
        &st,
        80,
        10.0,
        24.0,
        120.0,
        0.0,
        0.0,
        120.0,
        false,
        &mut |_| {},
        SystemTime::UNIX_EPOCH,
        None,
        None,
    );
    assert!(output_rows(&seg.row_data).is_empty());
    assert_eq!(seg.total_display_rows, 0);
    assert_eq!(seg.total_logical_rows, 0);
}

// ── N7: style adapter ──────────────────────────────────────────────────

#[test]
fn style_borrows_from_the_snapshot_only_when_resolve_styles() {
    let st = state(vec![slot("alpha", 0, 1)], Vec::new(), 80);
    let live = live_block("cmd", "");
    let run = |resolve: bool| {
        let mut bumps = 0usize;
        let seg = crate::paint::block_view::layout_pass::region_rows::region_live_segment(
            &live,
            &st,
            80,
            10.0,
            24.0,
            120.0,
            0.0,
            0.0,
            120.0,
            resolve,
            &mut |n| bumps += n,
            SystemTime::UNIX_EPOCH,
            None,
            None,
        );
        (seg, bumps)
    };
    let (seg_off, bumps_off) = run(false);
    assert!(!output_rows(&seg_off.row_data).is_empty(), "rows exist");
    for row in &seg_off.row_data {
        if let LaidRow::Output { style, .. } = row {
            assert!(style.is_none(), "resolve_styles=false must yield no style");
        }
    }
    assert_eq!(bumps_off, 0);
    let (seg_on, bumps_on) = run(true);
    for row in &seg_on.row_data {
        if let LaidRow::Output { style, .. } = row {
            let slot_style = &st.slots[0].style;
            assert!(
                style.is_some_and(|s| std::ptr::eq(s, slot_style)),
                "N7: style must borrow the snapshot's StyledLine"
            );
        }
    }
    assert!(bumps_on > 0, "resolve path bumps the styled counter");
}

// ── structural rows: shapes + distances (capture-branch parity) ────────

#[test]
fn structural_rows_match_capture_distances() {
    let st = state(vec![slot("hi", 0, 1)], Vec::new(), 80);
    let live = live_block("git status", "");
    let seg = crate::paint::block_view::layout_pass::region_rows::region_live_segment(
        &live,
        &st,
        80,
        10.0,
        24.0,
        120.0,
        0.0,
        0.0,
        120.0,
        false,
        &mut |_| {},
        SystemTime::UNIX_EPOCH,
        Some("/tmp"),
        None,
    );
    // Output(10) Blank(20) LiveCommand(30) LiveHeader(40) Separator(50) —
    // pitch 10, cwd present → header emitted.
    assert_eq!(seg.rows, vec![10.0, 20.0, 30.0, 40.0, 50.0]);
    assert_eq!(seg.row_data.len(), 5);
    assert!(matches!(seg.row_data[0], LaidRow::Output { .. }));
    assert!(matches!(seg.row_data[1], LaidRow::Blank));
    assert!(matches!(seg.row_data[2], LaidRow::LiveCommand { .. }));
    assert!(matches!(seg.row_data[3], LaidRow::LiveHeader { .. }));
    assert!(matches!(seg.row_data[4], LaidRow::Separator));
    assert_eq!(seg.cursor_dist, 50.0);
}

#[test]
fn no_cwd_no_git_branch_skips_live_header() {
    let st = state(vec![slot("hi", 0, 1)], Vec::new(), 80);
    let live = live_block("git status", "");
    let seg = crate::paint::block_view::layout_pass::region_rows::region_live_segment(
        &live,
        &st,
        80,
        10.0,
        24.0,
        120.0,
        0.0,
        0.0,
        120.0,
        false,
        &mut |_| {},
        SystemTime::UNIX_EPOCH,
        None,
        None,
    );
    // Output(10) Blank(20) LiveCommand(30) Separator(40) — no header.
    assert_eq!(seg.rows, vec![10.0, 20.0, 30.0, 40.0]);
    assert!(matches!(seg.row_data[3], LaidRow::Separator));
}

// ── branch isolation: flag-off identity (capture track untouched) ──────

#[test]
fn region_none_runs_the_capture_branch() {
    let output = "alpha\ngamma\ndelta\n";
    let live = live_block("git status", output);
    let blocks: Vec<Block> = Vec::new();
    let diagnose = HashMap::new();
    let cache = BlockLayoutCache::default();
    let mut live_cache = LiveLayoutCache::default();
    let input = make_input(&blocks, Some(live), None, None, 80, &diagnose);
    let out = compute_block_layout_pass(input, &cache, &mut live_cache);
    let rows = output_rows(&out.row_data);
    assert_eq!(
        rows,
        vec![
            ("delta", "delta", 2, 0),
            ("gamma", "gamma", 1, 0),
            ("alpha", "alpha", 0, 0),
        ],
        "region None: the capture branch must serve live.output verbatim \
         (newest-first emission)"
    );
}

#[test]
fn region_state_without_region_source_never_diverts() {
    // Guard requires BOTH region and region_state — a state alone (a caller
    // bug shape) must not divert the capture branch.
    let output = "solo\n";
    let live = live_block("git status", output);
    let st = state(vec![slot("UNRELATED", 0, 1)], Vec::new(), 80);
    let blocks: Vec<Block> = Vec::new();
    let diagnose = HashMap::new();
    let cache = BlockLayoutCache::default();
    let mut live_cache = LiveLayoutCache::default();
    let input = make_input(&blocks, Some(live), None, Some(&st), 80, &diagnose);
    let out = compute_block_layout_pass(input, &cache, &mut live_cache);
    let rows = output_rows(&out.row_data);
    assert_eq!(
        rows,
        vec![("solo", "solo", 0, 0)],
        "state without source: capture branch stays authoritative"
    );
}

// ── capture-vs-region track alignment on a real terminal session ───────

fn live_terminal(lines: &[u8]) -> Terminal {
    let mut terminal = Terminal::new(6, 80);
    terminal.set_live_block_grid_render(true);
    terminal.process(b"\x1b]133;A\x07$ \x1b]133;B\x07");
    terminal.process(lines);
    terminal
}

#[test]
fn region_track_rows_match_capture_track_rows() {
    let terminal = live_terminal(b"alpha\ngamma\ndelta\n");
    let grid = terminal.grid();
    let region = region_source_for(&terminal, grid).expect("flag ON + 133;B must yield a source");
    let cache = RegionLiveCache::default();
    cache.sync(1, &region, 80);
    let state_ref = cache.state(1).expect("sync must insert the pane");

    let capture_live = terminal
        .block_tracker()
        .in_flight()
        .expect("133;B without 133;C keeps the block in flight");
    let blocks: Vec<Block> = Vec::new();
    let diagnose = HashMap::new();
    let layout_cache = BlockLayoutCache::default();

    // Capture track (flag-off shape): region fields None.
    let mut capture_live_cache = LiveLayoutCache::default();
    let capture_out = compute_block_layout_pass(
        make_input(&blocks, Some(capture_live), None, None, 80, &diagnose),
        &layout_cache,
        &mut capture_live_cache,
    );
    // Region track: same geometry, region snapshot supplied. `InFlightBlock`
    // is not `Copy`: take a fresh view of the same in-flight block.
    let capture_live_region = terminal
        .block_tracker()
        .in_flight()
        .expect("in-flight block persists for the region track");
    let region = region_source_for(&terminal, grid).expect("source stays valid");
    let mut region_live_cache = LiveLayoutCache::default();
    let region_out = compute_block_layout_pass(
        make_input(
            &blocks,
            Some(capture_live_region),
            Some(region),
            Some(&*state_ref),
            80,
            &diagnose,
        ),
        &layout_cache,
        &mut region_live_cache,
    );

    let capture_rows = output_rows(&capture_out.row_data);
    let region_rows = output_rows(&region_out.row_data);
    // The tracks assemble the same rows EXCEPT the oldest one: the 133;B
    // in-flight line (the prompt "$ ") commits together with the first
    // output line, so the region's oldest row is "$ alpha" while the
    // capture track — which starts AT B — records "alpha". By design: the
    // command line belongs to the live view so the D6 caret can anchor to
    // it (see vt::sudo_su_cub_edit_grid_region_caret_anchor).
    assert_eq!(
        region_rows,
        vec![
            ("delta", "delta", 2, 0),
            ("gamma", "gamma", 1, 0),
            ("$ alpha", "$ alpha", 0, 0),
        ],
        "region rows: newest-first, oldest = prompt-merged command line"
    );
    assert_eq!(
        capture_rows,
        vec![
            ("delta", "delta", 2, 0),
            ("gamma", "gamma", 1, 0),
            ("alpha", "alpha", 0, 0),
        ],
        "capture rows: newest-first, oldest = pure post-B output"
    );
    assert_eq!(capture_rows.len(), 3);
    assert_eq!(
        capture_out.rows, region_out.rows,
        "distance streams must match too (Blank + structural rows)"
    );
    assert_eq!(
        capture_out.row_data.len(),
        region_out.row_data.len(),
        "row kinds must line up one-for-one"
    );
}

// ── P1-8: metrics total == assembly display rows ───────────────────────

#[test]
fn metrics_chunk_total_equals_assembly_display_rows() {
    let wrapped = state(
        vec![slot("aaaaaaaaaabbbbbbbbbbcccccccccc", 0, 1)],
        Vec::new(),
        10,
    );
    let live = live_block("cmd", "");
    let seg = crate::paint::block_view::layout_pass::region_rows::region_live_segment(
        &live,
        &wrapped,
        10,
        10.0,
        24.0,
        120.0,
        0.0,
        0.0,
        120.0,
        false,
        &mut |_| {},
        SystemTime::UNIX_EPOCH,
        None,
        None,
    );
    assert_eq!(
        region_display_chunk_total(&wrapped, 10),
        seg.total_display_rows as u32,
        "P1-8: the metrics total rides the same P1-6 display domain"
    );

    let blanked = state(
        vec![
            slot("", 0, 1),
            slot("alpha", 1, 2),
            slot("beta", 2, 3),
            slot("", 3, 4),
        ],
        Vec::new(),
        80,
    );
    let seg = crate::paint::block_view::layout_pass::region_rows::region_live_segment(
        &live,
        &blanked,
        80,
        10.0,
        24.0,
        120.0,
        0.0,
        0.0,
        120.0,
        false,
        &mut |_| {},
        SystemTime::UNIX_EPOCH,
        None,
        None,
    );
    assert_eq!(
        region_display_chunk_total(&blanked, 80),
        seg.total_display_rows as u32,
        "dropped blanks must not count in either"
    );
}
