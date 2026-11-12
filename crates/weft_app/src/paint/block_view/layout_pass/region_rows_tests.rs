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
//!   display-row total;
//! - v1.19.2 (PLAN_v1192 §2): the prefix-sum fast path matches the legacy
//!   full-loop sum — dyn slots, wrapped slots, blank edges, and the
//!   all-blank legacy full-sum all pinned. v1.20.0 (D10): inputs pass
//!   `complete_block_overlay: false` (legacy row-stream domain).

use std::{collections::HashMap, time::SystemTime};

use weft_core::blocks::{Block, BlockId, InFlightBlock};
use weft_core::vt::Terminal;

use crate::app_state::BlockDiagnoseState;
use crate::paint::block_view::layout_pass::{compute_block_layout_pass, LaidRow, LayoutPassInput};
use crate::paint::grid_cache::BlockLayoutCache;
use crate::paint::live_cache::{count_line_chunks, LiveLayoutCache};
use crate::paint::region_cache::region_display_chunk_total;
use crate::paint::region_cache::{region_source_for, PaneRegionState, RegionLiveCache, RegionSlot};

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
        registry_generation: 0,
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
        complete_block_overlay: false, // v1.20.0 (D10): legacy domain
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
    // capture track — which starts AT B — records "alpha". This fixture's
    // `133;B` arrives while the cursor still sits ON the prompt line (no
    // newline before preexec), so v1.20.2's cursor-row anchor legitimately
    // includes it; real shells emit the newline first and the two tracks
    // align exactly (see vt::sudo_su_echo_line_excluded_from_grid_region).
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

// ── v1.19.2 (PLAN_v1192 §2): prefix-sum fast path in
// `region_display_chunk_total` ──────────────────────────────────────────
//
// The fast path (cols == state.cols && cumulative.len() == slots.len()+1
// && first < last) replaces the O(total) chunk walk with
// `cumulative[last∩slots] - cumulative[first]` plus a row-wise count of
// the trailing dyn segment. Every guard failure falls back to the legacy
// full loop, which stays in place. These tests pin the fast path's
// numbers to the legacy semantics hand-computed from `count_line_chunks`.

#[test]
fn chunk_total_fast_path_counts_dyn_slots_row_wise() {
    // NR-1: `last` crosses the slots/dyn boundary — the boundary part
    // comes from the prefix sum (clamped at slots.len()), the dyn part
    // is counted row by row.
    let st = state(
        vec![slot("alpha", 0, 1), slot("", 1, 2)],
        vec![slot("aaaaaaaaaabbbbbbbbbb", 2, 3)], // 2 chunks at cols 10
        10,
    );
    let legacy = count_line_chunks("alpha", 10)
        + count_line_chunks("", 10)
        + count_line_chunks("aaaaaaaaaabbbbbbbbbb", 10);
    assert_eq!(legacy, 4, "hand-computed: 1 + 1 + 2");
    // Display range [0,3): nothing blank-trimmed at the edges, so the
    // fast path's boundary diff + dyn rows must equal the legacy sum.
    assert_eq!(region_display_chunk_total(&st, 10), 4);
}

#[test]
fn chunk_total_fast_path_matches_boundary_sum_on_wrapped_slots() {
    // A wrapped slot (2 chunks) inside the static prefix; the trailing
    // blank slot drops out of the display range (P1-6 trimming).
    let st = state(
        vec![
            slot("aaaaaaaaaabbbbbbbbbb", 0, 1), // 2 chunks at cols 10
            slot("x", 1, 2),
            slot("", 2, 3),
        ],
        Vec::new(),
        10,
    );
    // Display range [0,2): wrapped(2) + x(1) = 3; the trailing blank is
    // excluded from BOTH the prefix-sum read and the legacy loop.
    assert_eq!(region_display_chunk_total(&st, 10), 3);
}

#[test]
fn chunk_total_trims_leading_and_trailing_blanks() {
    let st = state(
        vec![
            slot("", 0, 1),
            slot("alpha", 1, 2),
            slot("", 2, 3),
            slot("beta", 3, 4),
            slot("", 4, 5),
        ],
        Vec::new(),
        80,
    );
    // Display range [1,4): alpha + interior blank + beta — interior
    // blanks count (1 chunk each), the edge blanks are trimmed on both
    // sides. 1 + 1 + 1 = 3.
    assert_eq!(region_display_chunk_total(&st, 80), 3);
    // A cols mismatch trips the guard → legacy full loop, same number
    // (chunk counts are width-independent for these short rows).
    assert_eq!(region_display_chunk_total(&st, 7), 3);
}

#[test]
fn chunk_total_all_blank_keeps_legacy_full_sum() {
    // B1 (PLAN_v1192 §2, recorded): on an all-blank region
    // `region_display_range` returns first == last, so the fast-path
    // guard fails and the legacy full loop answers with the sum of EVERY
    // blank's one chunk. The assembly emits 0 display rows — the
    // pre-existing metrics-vs-assembly divergence stays unresolved by
    // design; this test pins the metrics side to the legacy behavior so
    // a future side-pick is a conscious change, not a drift.
    let st = state(vec![slot("", 0, 1), slot("", 1, 2)], Vec::new(), 80);
    assert_eq!(region_display_chunk_total(&st, 80), 2);
}

#[test]
fn chunk_total_fast_path_skips_leading_dyn_blanks() {
    // Round 2 verification catch: when the display core starts INSIDE the
    // dyn region, the leading dyn blanks each carry one chunk and the
    // first-side clamp to `cumulative[0]` (no immutable slots ⇒ an empty
    // table prefix) skipped them — inflating the total by the blank
    // prefix. Both ends must scan the dyn tail symmetrically.
    let st = state(
        Vec::new(),
        vec![
            slot("", 0, 1),
            slot("", 1, 2),
            slot("", 2, 3),
            slot("", 3, 4),
            slot("", 4, 5),
            slot("$ delta", 5, 6),
        ],
        80,
    );
    assert_eq!(region_display_chunk_total(&st, 80), 1);
}

// ── v1.19.2 WP4 (PLAN_v1192 §4): region track across the finalize switch ──

/// `make_input`'s 120px clip window clips the completed blocks away; the
/// finalize-equivalence test needs every laid row, so it builds its own
/// input with a tall window. Fields mirror `make_input` verbatim.
#[allow(clippy::too_many_arguments)]
fn tall_input<'a, 'b, 'c>(
    blocks: &'a [Block],
    live: Option<InFlightBlock<'a>>,
    region: Option<crate::paint::region_cache::RegionSource<'c>>,
    region_state: Option<&'a PaneRegionState>,
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
        cols: 80,
        pitch: 10.0,
        header_height: 24.0,
        content_bottom_y: 20_000.0,
        clip_top: 0.0,
        clip_bottom: 20_000.0,
        resolve_styles: false,
        styled_lookup_counter: None,
        complete_block_overlay: false, // v1.20.0 (D10): legacy domain
        block_diagnose_state,
        now: SystemTime::UNIX_EPOCH,
        region,
        region_state,
    }
}

/// Output texts, newest-first (a wrapped logical row repeats per chunk).
fn output_texts<'a>(row_data: &'a [LaidRow<'a>]) -> Vec<&'a str> {
    row_data
        .iter()
        .filter_map(|row| match row {
            LaidRow::Output { text, .. } => Some(text.as_ref()),
            _ => None,
        })
        .collect()
}

#[test]
fn region_track_completed_block_session_equivalence() {
    // A maturing session: two completed blocks (block 1 carries a blank row
    // and a wrapped long line), then a third command goes live. The region
    // track must lay the same rows as the capture track while live, hand
    // block 3's rows to the completed assembly at the finalize switch with
    // no duplicate and no lost row, and feed the scroll-metrics total
    // through the production chain (region sync → state() →
    // region_display_chunk_total → block_content_metrics_with_cache).
    // Not all-blank: the interior blank is interior (B1/N7 does not apply).
    // Filler newlines after each 133;D push the completed blocks' rows out
    // of the 6-row viewport, so at block 3's 133;B the region [begin, head)
    // covers only the live block — the production steady state. Without
    // them the region's live segment overlaps the on-screen completed
    // blocks (v1.19.0 known behavior — see the ROADMAP filing) and the
    // dual-track streams diverge by design.
    let mut terminal = Terminal::new(6, 80);
    terminal.set_live_block_grid_render(true);
    terminal.process(b"\x1b]133;A\x07$ \x1b]133;B\x07");
    terminal.process(b"alpha\n\n"); // block 1, part 1
    let long = format!("{}\n", "w".repeat(200)); // wraps into 3 chunks at 80
    terminal.process(long.as_bytes()); // block 1, part 2
    terminal.process(b"\x1b]133;D;0\x07\r\n\n\n\n\n\n\n");
    terminal.process(b"\x1b]133;A\x07$ \x1b]133;B\x07");
    terminal.process(b"gamma\nlong-line-ends-here\n"); // block 2
    terminal.process(b"\x1b]133;D;0\x07\r\n\n\n\n\n");
    terminal.process(b"\x1b]133;A\x07$ \x1b]133;B\x07");
    terminal.process(b"delta"); // block 3, live

    let diagnose = HashMap::new();
    let mut layout_cache = BlockLayoutCache::default();

    // ── live frame, both tracks ────────────────────────────────────────
    let region = region_source_for(&terminal, terminal.grid())
        .expect("flag ON + live 133;B must yield a source");
    let cache = RegionLiveCache::default();
    cache.sync(1, &region, 80);
    let state_ref = cache.state(1).expect("sync must insert the pane");

    let capture_live = terminal
        .block_tracker()
        .in_flight()
        .expect("133;B without 133;C keeps block 3 in flight");
    let blocks: Vec<Block> = terminal.block_tracker().session_blocks().to_vec();
    // Production fills the completed-block cache before laying a frame
    // (gap_tests/bench precedent); the pass's `get()` panics on a cold
    // entry, so ensure every completed block first.
    for b in &blocks {
        layout_cache.ensure_cached(b, 80);
    }
    layout_cache.build_prefix_sum(&blocks); // production fast path (N3)
    let mut capture_live_cache = LiveLayoutCache::default();
    let capture_out = compute_block_layout_pass(
        tall_input(&blocks, Some(capture_live), None, None, &diagnose),
        &layout_cache,
        &mut capture_live_cache,
    );
    let capture_live_region = terminal
        .block_tracker()
        .in_flight()
        .expect("in-flight block persists for the region track");
    let region = region_source_for(&terminal, terminal.grid()).expect("source stays valid");
    let mut region_live_cache = LiveLayoutCache::default();
    let region_out = compute_block_layout_pass(
        tall_input(
            &blocks,
            Some(capture_live_region),
            Some(region),
            Some(&*state_ref),
            &diagnose,
        ),
        &layout_cache,
        &mut region_live_cache,
    );

    // (a) The two tracks lay one-for-one identical rows — same kinds, same
    // distance stream (region_track_rows_match_capture_track_rows twin, now
    // spanning two completed blocks).
    assert_eq!(
        capture_out.rows, region_out.rows,
        "distance streams must match (live + both completed blocks)"
    );
    assert_eq!(
        capture_out.row_data.len(),
        region_out.row_data.len(),
        "row kinds must line up one-for-one"
    );
    // Own the text snapshots before the finalize frame: the laid rows
    // borrow the tracker's in-flight block, and that borrow must be
    // released before `terminal.process` (E0502). Values are identical,
    // just owned.
    let region_texts: Vec<String> = output_texts(&region_out.row_data)
        .iter()
        .map(|s| s.to_string())
        .collect();
    let capture_texts: Vec<String> = output_texts(&capture_out.row_data)
        .iter()
        .map(|s| s.to_string())
        .collect();
    // The only allowed difference: the live head row merges the prompt
    // ("$ delta") while the capture track splits it (see the 133;B commit
    // note on region_track_rows_match_capture_track_rows).
    assert_eq!(
        region_texts[0], "$ delta",
        "live head row = prompt-merged slot"
    );
    assert_eq!(capture_texts[0], "delta", "capture head row = pure output");
    assert_eq!(
        region_texts[1..],
        capture_texts[1..],
        "identical chunk streams for the completed blocks"
    );

    // ── finalize frame ─────────────────────────────────────────────────
    // (c) P1-8 in the LIVE phase: the metrics total rides the laid rows
    // through the production chain. Completed contributions are the chunked
    // output rows (5 for block 1, 2 for block 2 — the laid chunk streams
    // above); the live contribution is region_display_chunk_total (the
    // region override wins over the capture tail). Measured BEFORE the
    // finalize switch — afterwards block 3 joins session_blocks and the
    // live term disappears with CommandExecuting.
    let region_rows = region_display_chunk_total(&state_ref, 80) as usize;
    assert_eq!(region_rows, 1, "one live display slot ('$ delta')");
    assert_eq!(
        region_texts.len() - 1,
        5 + 2,
        "laid chunk rows == the metrics' completed output rows (5 + 2)"
    );
    let (live_total, _) = crate::block_component::block_content_metrics_with_cache(
        &terminal,
        80,
        2,
        Some(&layout_cache),
        Some(&region_live_cache),
        Some(region_rows),
    );
    // block 1: 5 output rows + output gap + header(2) + command wrap + sep
    // block 2: 2 output rows + output gap + header(2) + command wrap + sep
    // live:    region_rows(1) + output gap + command + separator
    assert_eq!(
        live_total,
        (5 + 1 + 2 + 1 + 1) + (2 + 1 + 2 + 1 + 1) + (1 + 1 + 2)
    );

    terminal.process(b"\x1b]133;D;0\x07");
    assert!(
        region_source_for(&terminal, terminal.grid()).is_none(),
        "133;D clears the begin anchor — the region track hands off"
    );
    let blocks_final: Vec<Block> = terminal.block_tracker().session_blocks().to_vec();
    assert_eq!(blocks_final.len(), 3, "all three blocks completed");
    for b in &blocks_final {
        layout_cache.ensure_cached(b, 80);
    }
    layout_cache.build_prefix_sum(&blocks_final);
    let mut final_live_cache = LiveLayoutCache::default();
    let final_out = compute_block_layout_pass(
        tall_input(&blocks_final, None, None, None, &diagnose),
        &layout_cache,
        &mut final_live_cache,
    );
    let final_texts = output_texts(&final_out.row_data);

    // (b) Finalize continuity: block 3's laid live rows reappear as
    // completed rows — no duplicate, no loss (chunk streams, newest-first).
    assert_eq!(
        final_texts.len(),
        region_texts.len(),
        "the switch preserves the row count"
    );
    assert_eq!(
        final_texts[0], "delta",
        "the live head row completes into pure output"
    );
    assert_eq!(
        final_texts[1..],
        region_texts[1..],
        "blocks 1-2 rows identical across the switch"
    );

    // (c) post-switch: block 3 is now a completed block — its live
    // contribution (4) is replaced by the completed shape (1 + 1 + 2 + 1 + 1)
    // and the live term disappears with CommandExecuting.
    let (final_total, _) = crate::block_component::block_content_metrics_with_cache(
        &terminal,
        80,
        2,
        Some(&layout_cache),
        None,
        None,
    );
    // Block 3 completes: its live contribution (4) is replaced by the
    // completed shape (1 + 1 + 2 + 1 + 1).
    assert_eq!(final_total, live_total + (1 + 1 + 2 + 1 + 1) - (1 + 1 + 2));
}

// ── scroll-clamp domain audit: chrome distance vs metrics total ──
// The wheel clamp derives `max_scroll` from `block_content_metrics_with_overlay`,
// while the layout pass's distance domain carries per-block chrome (gap +
// header band + separator). v1.20.6 (FIX_V1206) adds the chrome back to the
// metrics formula, so the two domains must now agree exactly: laid_px ==
// metrics_px for a plain multi-block history.
#[test]
fn overlay_chrome_distance_matches_metrics_total() {
    let mut terminal = Terminal::new(24, 80);
    for i in 0..6 {
        terminal.process(
            format!("\x1b]133;A\x07echo {i}\r\n\x1b]133;B\x07\x07out-{i}\r\n\x1b]133;D;0\x07")
                .as_bytes(),
        );
    }
    let blocks = terminal.block_tracker().session_blocks();
    assert_eq!(blocks.len(), 6, "six finished blocks");
    let mut layout_cache = BlockLayoutCache::default();
    for b in blocks {
        layout_cache.ensure_cached(b, 80);
    }
    layout_cache.build_prefix_sum(blocks);

    // Production geometry: pitch 20, header band = compact_control_row_span
    // (2 rows) * pitch = 40 px; oversized viewport so nothing is culled.
    let diagnose = std::collections::HashMap::new();
    let out = compute_block_layout_pass(
        LayoutPassInput {
            blocks,
            live: None,
            pane_session_id: 1,
            cwd: None,
            git_branch: None,
            block_scroll: 0.0,
            viewport_rows: 400,
            cols: 80,
            pitch: 20.0,
            header_height: 40.0,
            content_bottom_y: 8000.0,
            clip_top: 0.0,
            clip_bottom: 8000.0,
            resolve_styles: false,
            styled_lookup_counter: None,
            complete_block_overlay: true,
            block_diagnose_state: &diagnose,
            now: std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
            region: None,
            region_state: None,
        },
        &layout_cache,
        &mut LiveLayoutCache::default(),
    );

    // Full assembled extent: content rows store `content_bottom_y - row_top`
    // (pushed at a row's top), overlay chrome rows carry their own dist +
    // height. Take the max over both domains — the oldest block's header
    // band + separator live only in the overlay carrier.
    let laid_px = out.rows.iter().copied().fold(0.0_f32, f32::max).max(
        // overlay dist_px is the chrome piece's BOTTOM edge (pushed after
        // the cursor advance), so no extra height term.
        out.overlay_rows
            .iter()
            .map(|c| c.dist_px)
            .fold(0.0_f32, f32::max),
    );
    let (total, _) = crate::block_component::block_content_metrics_with_overlay(
        &terminal,
        80,
        2,
        Some(&layout_cache),
        None,
        None,
        true,
        None,
    );
    let metrics_px = total as f32 * 20.0;
    // Regression (v1.20.0 lockout): the clamp domain and the assembly domain
    // must agree exactly — zero drift, or `max_scroll` under-reports and a
    // short history can no longer scroll at all.
    assert_eq!(
        laid_px, metrics_px,
        "metrics total must count the chrome its distance domain carries"
    );
}
