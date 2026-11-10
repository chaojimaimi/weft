//! v1.19.0 (PLAN_v1190 §4): region snapshot cache tests — truth tables for
//! the D6 caret mapping and the P1-6 display range, plus Terminal-driven
//! scenarios pinning the D8 append path (slots/dyn_slots tiling, advance
//! counting), the wholesale fallbacks, and the P1-4 version-source switch.
//! Every grid scenario drives a real `Terminal::process` byte stream, the
//! same discipline as `weft_core::grid::region_tests`.

use std::cell::Ref;

use weft_core::blocks::StyledLine;
use weft_core::vt::Terminal;

use super::{
    document_index_of_cursor, live_output_version_source, region_display_range, region_head,
    region_source_for, trim_cap, PaneRegionState, RegionLiveCache, RegionSlot,
};

// ── test-only helpers (moved here v1.19.2, PLAN_v1192 task 0) ───────────

/// D6 logical-row lookup: the region row that owns the cursor's physical
/// row (mid-wrap-chain rows resolve to their owning logical row). `slots`
/// is the P1-6-dropped display sequence so the returned index matches the
/// assembled `LaidRow::Output::line` domain.
/// (Test-only: production uses `PaneRegionState::display_logical_row_of_cursor`,
/// which layers the P1-6 display domain on top of this bare lookup.)
#[cfg(test)]
pub(crate) fn region_logical_row_of_cursor(
    slots: &[RegionSlot],
    cursor_doc_idx: u64,
) -> Option<usize> {
    slots
        .iter()
        .position(|slot| slot.start_idx <= cursor_doc_idx && cursor_doc_idx < slot.end_idx)
}

/// P1-6: drop leading/trailing blank logical rows at assembly time (the core
/// region keeps them so the D8 append boundary stays stable).
/// (Test-only: production slices through `region_display_range` directly.)
#[cfg(test)]
pub(crate) fn region_display_slots(slots: &[RegionSlot]) -> &[RegionSlot] {
    let (first, last) = region_display_range(slots.len(), |i| slots[i].text.is_empty());
    &slots[first..last]
}

// ── fixtures ────────────────────────────────────────────────────────────

fn slot(text: &str, start: u64, end: u64) -> RegionSlot {
    RegionSlot {
        text: text.to_string(),
        style: StyledLine::default(),
        start_idx: start,
        end_idx: end,
    }
}

fn feed_lines(terminal: &mut Terminal, lines: &[&str]) {
    for (i, line) in lines.iter().enumerate() {
        terminal.process(line.as_bytes());
        if i + 1 < lines.len() {
            terminal.process(b"\r\n");
        }
    }
}

/// A live command whose `133;B` begin anchor was sampled before `lines`
/// printed; the grid-render flag is ON. With `rows = 2`, feeding n lines
/// scrolls the first `n - 2` into the scrollback and leaves the last two in
/// the viewport.
fn live_terminal(rows: usize, cols: usize, lines: &[&str]) -> Terminal {
    let mut terminal = Terminal::new(rows, cols);
    terminal.set_live_block_grid_render(true);
    terminal.process(b"\x1b]133;A\x07\x1b]133;B\x07");
    feed_lines(&mut terminal, lines);
    terminal
}

/// Sync the cache for `pane` from `terminal`'s current state and return the
/// snapshot borrow (borrows `cache`; `terminal` is only read during sync).
fn snapshot<'a>(
    cache: &'a RegionLiveCache,
    terminal: &Terminal,
    pane: u64,
) -> Ref<'a, PaneRegionState> {
    let grid = terminal.grid();
    let source = region_source_for(terminal, grid).expect("flag on + begin sampled");
    cache.sync(pane, &source, grid.num_cols);
    cache.state(pane).expect("snapshot inserted")
}

fn texts(st: &PaneRegionState) -> Vec<String> {
    st.all_slots().map(|s| s.text.clone()).collect()
}

// ── RegionSource gating ─────────────────────────────────────────────────

#[test]
fn region_source_requires_flag_and_begin() {
    // flag OFF, begin sampled.
    let mut t = live_terminal(2, 20, &["alpha"]);
    t.set_live_block_grid_render(false);
    let grid = t.grid();
    assert!(
        region_source_for(&t, grid).is_none(),
        "flag off → no region"
    );

    // flag ON, no begin anchor (no 133;B).
    let mut t = Terminal::new(2, 20);
    t.set_live_block_grid_render(true);
    feed_lines(&mut t, &["alpha"]);
    let grid = t.grid();
    assert!(
        region_source_for(&t, grid).is_none(),
        "no 133;B → no region"
    );

    // flag ON + begin sampled → Some (even when the region is empty).
    let t = live_terminal(2, 20, &[]);
    let grid = t.grid();
    assert!(region_source_for(&t, grid).is_some());
}

// ── D8 append / tiling ──────────────────────────────────────────────────

#[test]
fn capture_scrolls_into_slots_and_viewport_into_dyn() {
    let t = live_terminal(2, 20, &["alpha", "beta", "gamma", "delta"]);
    let cache = RegionLiveCache::default();
    {
        let st = snapshot(&cache, &t, 1);
        // alpha/beta scrolled in; gamma sits on viewport row 0 (non-empty)
        // → head = scrollback.len() + 0 + 1 = 3.
        assert_eq!(
            st.slots.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(),
            ["alpha", "beta"]
        );
        assert_eq!(
            st.dyn_slots
                .iter()
                .map(|s| s.text.as_str())
                .collect::<Vec<_>>(),
            ["gamma", "delta"]
        );
        // Slot boundaries tile the document domain contiguously.
        assert_eq!((st.slots[0].start_idx, st.slots[0].end_idx), (0, 1));
        assert_eq!((st.slots[1].start_idx, st.slots[1].end_idx), (1, 2));
        assert_eq!((st.dyn_slots[0].start_idx, st.dyn_slots[0].end_idx), (2, 3));
        assert_eq!((st.dyn_slots[1].start_idx, st.dyn_slots[1].end_idx), (3, 4));
        // The immutable-prefix chunk table stays in lockstep.
        assert_eq!(st.cumulative, vec![0u32, 1, 2]);
        assert_eq!(cache.rebuilds(), 1);
        assert_eq!(cache.advances(), 1, "wholesale rebuild advances the anchor");
    }
    // A second sync at the same revision/cols is a D7 hit.
    let grid = t.grid();
    let source = region_source_for(&t, grid).unwrap();
    cache.sync(1, &source, grid.num_cols);
    assert_eq!(cache.rebuilds(), 1, "key hit must not rebuild");
    assert_eq!(cache.advances(), 1);
}

#[test]
fn head_extends_only_to_last_content_row() {
    // gamma sits on viewport row 1; the row below is blank → head = 1+1+1 = 3.
    let t = live_terminal(2, 20, &["alpha", "beta", "gamma"]);
    let grid = t.grid();
    assert_eq!(region_head(grid), 3);

    // An all-empty viewport collapses the head to the scrollback prefix.
    let mut t = live_terminal(2, 20, &["alpha", "beta"]);
    t.process(b"\x1b[2J");
    let grid = t.grid();
    assert_eq!(region_head(grid), grid.scrollback.len() as u64);
    let cache = RegionLiveCache::default();
    let st = snapshot(&cache, &t, 1);
    // At rows=2 neither alpha nor beta ever scrolled into the scrollback
    // (the \r\n between them only moved the cursor down one row), so 2J
    // blanks the entire document the region can see.
    assert!(
        texts(&st).is_empty(),
        "cleared viewport drops the tail; the head collapses to the scrollback prefix"
    );
}

#[test]
fn new_scrollback_transfers_via_advance_not_rebuild() {
    let mut t = live_terminal(2, 20, &["alpha", "beta", "gamma"]);
    let cache = RegionLiveCache::default();
    snapshot(&cache, &t, 1);

    // Push delta/epsilon through: beta and gamma scroll into the scrollback
    // (immutable), so the next sync must ADVANCE the anchor, not rebuild.
    // The leading \r\n commits gamma's line first — the cursor was still
    // parked on it after feed_lines' final line.
    t.process(b"\r\ndelta\r\nepsilon");
    let st = snapshot(&cache, &t, 1);
    assert_eq!(
        st.slots.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(),
        ["alpha", "beta", "gamma"]
    );
    assert_eq!(
        st.dyn_slots
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>(),
        ["delta", "epsilon"]
    );
    assert_eq!(cache.rebuilds(), 1);
    assert_eq!(cache.advances(), 2);
    assert_eq!(
        st.cumulative,
        vec![0u32, 1, 2, 3],
        "prefix sums grow with slots"
    );

    // Review N1 regression: the advance tier must persist grid_revision so
    // the next unchanged-frame sync hits the D7 fast path instead of
    // rescanning the anchor every idle frame.
    drop(st); // release the snapshot Ref before the next sync's borrow_mut
    let before = (cache.rebuilds(), cache.advances());
    snapshot(&cache, &t, 1);
    assert_eq!(
        (cache.rebuilds(), cache.advances()),
        before,
        "idle frame after a persisted advance must D7-hit"
    );
}

#[test]
fn viewport_in_place_rewrite_rereads_dyn_only() {
    let mut t = live_terminal(2, 20, &["alpha", "beta", "gamma"]);
    let cache = RegionLiveCache::default();
    snapshot(&cache, &t, 1);
    // Rewrite the gamma row in place (gamma is viewport row 1 → ESC[2;1H).
    t.process(b"\x1b[2;1HOMEGA");
    let st = snapshot(&cache, &t, 1);
    assert_eq!(texts(&st), ["alpha", "beta", "OMEGA"]);
    assert_eq!(cache.rebuilds(), 1, "viewport rewrite must not rebuild");
}

#[test]
fn cols_only_change_rechunks_without_grid_reread() {
    let t = live_terminal(2, 20, &["alpha", "beta", "gamma", "delta"]);
    let cache = RegionLiveCache::default();
    snapshot(&cache, &t, 1);
    let grid = t.grid();
    let source = region_source_for(&t, grid).unwrap();
    cache.sync(1, &source, 5);
    assert_eq!(cache.rebuilds(), 1);
    assert_eq!(cache.advances(), 1);
    let st = cache.state(1).unwrap();
    assert_eq!(st.cols, 5);
    assert_eq!(
        texts(&st),
        ["alpha", "beta", "gamma", "delta"],
        "texts unchanged"
    );
    // At cols=5 each 5-char cached line still fits one chunk.
    assert_eq!(st.cumulative, vec![0u32, 1, 2]);
}

// ── v1.19.1 borrow contract ─────────────────────────────────────────────

#[test]
fn sync_panics_while_a_state_ref_is_alive_borrow_contract() {
    // v1.19.1 (field crash 2026-11-10, v1.19.0): `sync` unconditionally
    // `borrow_mut`s the pane map — even on a D7 hit — so NO caller may hold
    // a `state()` Ref across a `sync` call. draw_phases.rs violated this on
    // every 133;B command frame (the D6 caret anchor was evaluated with the
    // Ref alive, then `build_block_view_vertices` re-synced beneath it →
    // "RefCell already borrowed" at region_cache.rs:152, exit 101). The fix
    // drops the Ref before building the paint model; this test freezes the
    // contract so a future tolerant `sync` (try_borrow) is a deliberate,
    // reviewed semantic change — not an accident that silently re-opens the
    // crash window.
    let t = live_terminal(2, 20, &["alpha", "beta", "gamma", "delta"]);
    let cache = RegionLiveCache::default();
    snapshot(&cache, &t, 1);
    let grid = t.grid();
    let source = region_source_for(&t, grid).unwrap();
    let guard: Ref<PaneRegionState> = cache.state(1).expect("state after sync");
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // D7-hit inputs (same begin/revision/cols) — still borrow_mut → panic.
        cache.sync(1, &source, 20);
    }));
    drop(guard);
    assert!(
        outcome.is_err(),
        "sync must panic while a state() Ref is alive (borrow contract)"
    );
}

#[test]
fn new_begin_anchor_rebuilds_wholesale() {
    let mut t = live_terminal(2, 20, &["alpha", "beta", "gamma"]);
    let cache = RegionLiveCache::default();
    snapshot(&cache, &t, 1);
    // A fresh command: finalize the live one (133;D clears the anchor),
    // then 133;A/133;B re-samples the begin anchor.
    t.process(b"\x1b]133;D;0\x07\x1b]133;A\x07\x1b]133;B\x07");
    let st = snapshot(&cache, &t, 1);
    assert_eq!(cache.rebuilds(), 2, "begin change → wholesale");
    assert_eq!(st.begin_position, t.grid().scrollback.position());
    // The new region starts at the fresh anchor (scrollback holds alpha):
    // beta/gamma (still the viewport content) remain.
    assert_eq!(texts(&st), ["beta", "gamma"]);
}

// ── wrapped-line merging ────────────────────────────────────────────────

#[test]
fn wrapped_line_merges_into_one_slot() {
    let long = "abcdefghijklmnopqrstuvwxyz"; // 26 chars > cols=10 → 3 physical rows
    let t = live_terminal(3, 10, &[long]);
    let cache = RegionLiveCache::default();
    let st = snapshot(&cache, &t, 1);
    // Nothing scrolled in; the single wrap chain lives in the live tail.
    assert!(st.slots.is_empty());
    assert_eq!(st.dyn_slots.len(), 1);
    let s = &st.dyn_slots[0];
    assert_eq!(s.text, long, "wrapped rows merge into one logical row");
    assert_eq!(
        (s.start_idx, s.end_idx),
        (0, 3),
        "slot spans the wrap chain"
    );
}

// ── P1-6 display range ──────────────────────────────────────────────────

#[test]
fn display_range_truth_table() {
    assert_eq!(
        region_display_range(5, |i| ["", "a", "", "b", ""][i].is_empty()),
        (1, 4)
    );
    assert_eq!(region_display_range(0, |_| true), (0, 0));
    assert_eq!(
        region_display_range(3, |_| true),
        (3, 3),
        "all blank → empty"
    );
    assert_eq!(
        region_display_range(2, |i| ["a", "b"][i].is_empty()),
        (0, 2)
    );
    assert_eq!(region_display_range(2, |i| ["a", ""][i].is_empty()), (0, 1));
}

#[test]
fn display_slots_drops_leading_and_trailing_blanks() {
    let slots = vec![
        slot("", 0, 1),
        slot("a", 1, 2),
        slot("", 2, 3),
        slot("b", 3, 4),
        slot("", 4, 5),
    ];
    let kept = region_display_slots(&slots);
    assert_eq!(
        kept.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(),
        ["a", "", "b"]
    );
}

// ── D6 caret mapping ────────────────────────────────────────────────────

#[test]
fn d6_logical_row_truth_table() {
    // Slots: a | b | a 3-row wrap chain | c. Boundaries are document idx.
    let slots = vec![
        slot("a", 0, 1),
        slot("b", 1, 2),
        slot("wrap", 2, 5),
        slot("c", 5, 6),
    ];
    let expected = [
        (0u64, Some(0usize)),
        (1, Some(1)),
        (2, Some(2)),
        (3, Some(2)),
        (4, Some(2)),
        (5, Some(3)),
    ];
    for (doc, want) in expected {
        assert_eq!(
            region_logical_row_of_cursor(&slots, doc),
            want,
            "cursor at document row {doc}"
        );
    }
    assert_eq!(
        region_logical_row_of_cursor(&slots, 6),
        None,
        "past the region"
    );
}

#[test]
fn d6_display_domain_skips_dropped_blanks() {
    // The same physical layout, but with blank rows at both ends: the
    // display-domain index must stay pinned to the assembled `line` values.
    let state = PaneRegionState {
        begin_position: 0,
        grid_revision: 0,
        registry_generation: 0,
        cols: 20,
        anchor_idx: 0,
        anchor_position: 0,
        slots: vec![slot("", 0, 1), slot("a", 1, 2)],
        dyn_slots: vec![
            slot("", 2, 3),
            slot("wrap", 3, 6),
            slot("c", 6, 7),
            slot("", 7, 8),
        ],
        cumulative: vec![0u32, 0, 1, 1, 2, 3, 3],
    };
    assert_eq!(state.display_logical_row_of_cursor(1), Some(0), "a");
    // Mid-wrap-chain cursors resolve to their owning logical row; interior
    // blanks OCCUPY their display row (only edge blanks are dropped), so
    // the wrap row lands at display index 2 and "c" at 3.
    assert_eq!(
        state.display_logical_row_of_cursor(4),
        Some(2),
        "wrap middle (interior blank shifts it)"
    );
    assert_eq!(state.display_logical_row_of_cursor(6), Some(3), "c");
    // Dropped edge blanks own no cursor.
    assert_eq!(state.display_logical_row_of_cursor(0), None);
    assert_eq!(state.display_logical_row_of_cursor(7), None);
    assert_eq!(state.display_logical_row_of_cursor(99), None);
}

#[test]
fn d6_document_index_lives_in_scrollback_domain() {
    // Six short lines through a 3-row viewport: the first three scroll into
    // the scrollback; parking the cursor on viewport row 2 holds "zeta".
    let mut t = live_terminal(3, 20, &["a", "b", "c", "d", "e", "zeta"]);
    t.process(b"\x1b[3;1H");
    let grid = t.grid();
    assert_eq!(grid.scrollback.len(), 3);
    // document_index = scrollback.len() + cursor.row = 3 + 2 = 5 — the
    // "viewport-domain direct subtraction" answer (2) would be wrong.
    assert_eq!(document_index_of_cursor(grid, 2), 5);
}

#[test]
fn caret_finds_row_through_cache_snapshot() {
    let mut t = live_terminal(2, 20, &["alpha", "beta", "gamma", "delta"]);
    t.process(b"\x1b[1;1H"); // park the cursor on gamma's viewport row (row 0)
    let cache = RegionLiveCache::default();
    let st = snapshot(&cache, &t, 1);
    let grid = t.grid();
    let doc = document_index_of_cursor(grid, 0);
    assert_eq!(doc, 2);
    assert_eq!(
        st.display_logical_row_of_cursor(doc),
        Some(2),
        "assembled line index for gamma"
    );
}

// ── cap / prefix-sum invariants ─────────────────────────────────────────

#[test]
fn trim_cap_preserves_suffix_and_rebases_prefix_sums() {
    let mut state = PaneRegionState {
        begin_position: 0,
        grid_revision: 0,
        registry_generation: 0,
        cols: 20,
        anchor_idx: 30,
        anchor_position: 30,
        slots: (0..30)
            .map(|i| slot(&format!("row{i}"), i, i + 1))
            .collect(),
        dyn_slots: Vec::new(),
        cumulative: (0..=30u32).collect(),
    };
    trim_cap(&mut state, 20);
    assert_eq!(state.slots.len(), 20);
    assert_eq!(state.slots[0].text, "row10", "the newest cap rows survive");
    assert_eq!(
        state.slots.iter().map(|s| s.start_idx).collect::<Vec<_>>(),
        (10u64..30).collect::<Vec<_>>()
    );
    assert_eq!(state.cumulative.len(), 21);
    assert_eq!(state.cumulative, (0..=20u32).collect::<Vec<_>>());
}

// ── P1-4 version source ─────────────────────────────────────────────────

#[test]
fn live_output_version_source_switches_with_flag() {
    let mut t = live_terminal(2, 20, &["alpha", "beta"]);
    let revision = t.grid().revision;
    assert_eq!(
        live_output_version_source(&t),
        revision,
        "flag ON → grid.revision"
    );
    t.set_live_block_grid_render(false);
    assert_eq!(
        live_output_version_source(&t),
        t.block_tracker().live_output_version(),
        "flag OFF → capture-stream version"
    );
}

// ── empty-region safety ─────────────────────────────────────────────────

#[test]
fn empty_region_syncs_to_an_empty_snapshot() {
    // 133;B with zero output bytes: the snapshot must exist (the layout
    // guard consumes it) but stay empty.
    let t = live_terminal(2, 20, &[]);
    let cache = RegionLiveCache::default();
    let st = snapshot(&cache, &t, 1);
    assert_eq!(texts(&st), Vec::<String>::new());
    assert_eq!(st.slots.len() + st.dyn_slots.len(), 0);
}

// ── v1.19.2 WP3-A (PLAN_v1192 §3): hyperlink resolver ──────────────────

#[test]
fn hyperlinks_resolve_into_slot_styles_on_both_sync_tiers() {
    // `RegionSource.hyperlinks` threads the terminal's real OSC-8 registry
    // into `logical_lines`, so synced slots carry resolved LinkSpans in
    // `style` — on the wholesale rebuild tier AND on the D8 advance tier
    // (byte-style precedent: vt/tests.rs
    // osc8_link_at_block_capture_resolves_url).
    let mut t = live_terminal(2, 40, &[]);
    t.process(b"\x1b]8;;https://weft.dev/region\x1b\\anchor\x1b]8;;\x1b\\\r\n");

    // Wholesale tier: the first sync rebuilds from scratch; the linked row
    // (the only non-blank one) lands in the viewport tail as a dyn slot.
    let cache = RegionLiveCache::default();
    {
        let st = snapshot(&cache, &t, 1);
        assert_eq!(cache.rebuilds(), 1);
        let linked = st
            .dyn_slots
            .first()
            .expect("the linked row lives in the viewport tail");
        assert_eq!(linked.text, "anchor");
        let link = linked
            .style
            .links
            .first()
            .expect("the real resolver must fill the slot's style links");
        assert_eq!(link.url, "https://weft.dev/region");
        assert_eq!((link.start, link.end), (0, 6));
    }

    // Advance tier: scroll the linked row into the scrollback; the next
    // sync must transfer it into `slots` with its links intact and still
    // not rebuild (drop the Ref first — borrow contract).
    t.process(b"\r\nfiller\r\nmore");
    let st = snapshot(&cache, &t, 1);
    assert_eq!(
        cache.rebuilds(),
        1,
        "the scroll sync must advance, not rebuild"
    );
    assert_eq!(cache.advances(), 2, "wholesale + one advance");
    let linked = st
        .slots
        .first()
        .expect("the linked row scrolled into slots");
    assert_eq!(linked.text, "anchor");
    let link = linked
        .style
        .links
        .first()
        .expect("the advance tier keeps resolved links");
    assert_eq!(link.url, "https://weft.dev/region");
    assert_eq!((link.start, link.end), (0, 6));
}

// ── v1.19.3 WP2 (PLAN_v1193 §2.2): whitespace-aware region head ────────

#[test]
fn region_head_ignores_ideographic_space_rows() {
    // U+3000 is Unicode White_Space: an ideographic-space-only viewport row
    // must not count as content (the pre-v1.19.3 `!= ' '` predicate wrongly
    // extended the head past it).
    let mut t = live_terminal(2, 20, &["alpha"]);
    t.process("\u{1b}[2;1H\u{3000}".as_bytes());
    assert_eq!(
        region_head(t.grid()),
        1,
        "the U+3000 row must not extend the head past the scrollback"
    );

    // Positive control: real text in the same row still extends the head.
    t.process(b"\x1b[2;1Hbeta");
    assert_eq!(region_head(t.grid()), 2, "real content must still extend");
}

#[test]
fn region_head_collapses_an_all_ideographic_space_viewport() {
    // Every viewport row is U+3000-only → the region collapses to the
    // scrollback prefix (the `map_or(sb_len)` branch): head == sb length.
    let mut t = live_terminal(2, 20, &["alpha"]);
    t.process("\u{1b}[1;1H\u{3000}".as_bytes());
    t.process("\u{1b}[2;1H\u{3000}".as_bytes());
    assert_eq!(
        region_head(t.grid()),
        1,
        "all-U+3000 viewport must collapse the head to the scrollback"
    );
}

#[test]
fn region_head_collapses_an_all_nbsp_viewport() {
    // U+00A0 is White_Space too (width 1 — no spacer half involved).
    let mut t = live_terminal(2, 20, &["alpha"]);
    t.process("\u{1b}[1;1H\u{a0}".as_bytes());
    t.process("\u{1b}[2;1H\u{a0}".as_bytes());
    assert_eq!(
        region_head(t.grid()),
        1,
        "all-NBSP viewport must collapse the head to the scrollback"
    );
}

#[test]
fn region_head_still_collapses_plain_space_rows() {
    // Regression guard: the fix must not make plain spaces count as
    // content — the pre-v1.19.3 behavior for `' '` rows is preserved.
    let mut t = live_terminal(2, 20, &["alpha"]);
    t.process(b"\x1b[1;1H ");
    t.process(b"\x1b[2;1H ");
    assert_eq!(
        region_head(t.grid()),
        1,
        "plain-space rows must stay non-content"
    );
}

// ── v1.19.3 WP3-B (PLAN_v1193 §3): registry generation sync key ────────

#[test]
fn cell_map_churn_does_not_drift_the_sync_key() {
    // v1.19.3 verification fix (PLAN_v1193 WP3): the cell map is
    // viewport-relative index state — vt/screen_exit/scroll.rs clears it on
    // every scroll — so tracking its churn in `generation` would make the
    // advance tier unreachable for link-bearing viewports (the regression
    // the v1.19.2 scroll-advance test caught). Resolution reads the
    // append-only `urls` table (review N7), so a registry-only cell-map
    // clear must leave the sync key AND the resolved links untouched: the
    // next sync stays a D7 hit.
    let mut t = live_terminal(2, 40, &[]);
    t.process(b"\x1b]8;;https://weft.dev/region\x1b\\anchor\x1b]8;;\x1b\\");

    let cache = RegionLiveCache::default();
    {
        let st = snapshot(&cache, &t, 1);
        assert_eq!(cache.rebuilds(), 1);
        let linked = st.dyn_slots.first().expect("linked dyn slot");
        assert_eq!(
            linked.style.links.first().expect("resolved").url,
            "https://weft.dev/region"
        );
    }

    let revision_before = t.grid().revision;
    t.clear_hyperlink_cell_map();
    assert_eq!(
        t.grid().revision,
        revision_before,
        "the mutation must be registry-only (revision untouched)"
    );
    {
        let st = snapshot(&cache, &t, 1);
        assert_eq!(
            cache.rebuilds(),
            1,
            "index churn must not rebuild — the sync stays a D7 hit"
        );
        assert_eq!(
            cache.advances(),
            1,
            "and no advance either — a plain D7 hit"
        );
        let linked = st.dyn_slots.first().expect("text preserved");
        assert_eq!(linked.text, "anchor", "texts must not change");
        let link = linked
            .style
            .links
            .first()
            .expect("links keep resolving — the urls table is append-only");
        assert_eq!(link.url, "https://weft.dev/region");
    }
}

#[test]
fn registry_drift_during_scroll_forces_wholesale_not_advance() {
    // Revision and generation change together (scroll + a fresh OSC-8
    // registration): the advance tier must be skipped — the wholesale
    // rebuild re-supplies the whole link chain instead (contrast
    // hyperlinks_resolve_into_slot_styles_on_both_sync_tiers, whose
    // revision-only scroll still advances).
    let mut t = live_terminal(2, 40, &[]);
    t.process(b"\x1b]8;;https://weft.dev/one\x1b\\first\x1b]8;;\x1b\\\r\n");

    let cache = RegionLiveCache::default();
    {
        let st = snapshot(&cache, &t, 1);
        assert_eq!(cache.rebuilds(), 1);
        assert_eq!(st.dyn_slots.first().expect("dyn slot").text, "first");
    }

    // One process call: registers a new URL (generation bump), prints
    // "second", scrolls "first" into the scrollback (revision bump), and
    // prints "third".
    t.process(b"\x1b]8;;https://weft.dev/two\x1b\\second\x1b]8;;\x1b\\\r\nthird");

    {
        let st = snapshot(&cache, &t, 1);
        assert_eq!(
            cache.rebuilds(),
            2,
            "generation drift must force a wholesale even mid-scroll"
        );
        assert_eq!(cache.advances(), 2, "each wholesale still anchors");
        assert_eq!(
            texts(&st),
            &["first", "second", "third"],
            "texts must not change"
        );
        assert_eq!(
            st.slots
                .first()
                .expect("scrolled slot")
                .style
                .links
                .first()
                .expect("wholesale re-resolves scrolled links")
                .url,
            "https://weft.dev/one"
        );
        assert_eq!(
            st.dyn_slots
                .first()
                .expect("viewport slot")
                .style
                .links
                .first()
                .expect("wholesale re-resolves viewport links")
                .url,
            "https://weft.dev/two"
        );
    }
}
