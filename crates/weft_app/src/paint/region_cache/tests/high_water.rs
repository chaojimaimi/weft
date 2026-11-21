//! v1.20.11 (PLAN_v12011 §3 T2): ED[J collapse-immunization suite — the
//! two-layer high-water (L1 head domain / L2 display domain) that keeps the
//! live block's height stable across the openclaw spinner's split-batch
//! `\x1b[1G\x1b[J` ticks. Every grid scenario drives a real
//! `Terminal::process` byte stream at the split-batch cadence (erase half
//! and text half in separate batches, one sync each — the 4ms read-loop
//! interleave that produced the whole-window flicker).

use weft_core::vt::Terminal;

use super::{feed_lines, live_terminal, slot, snapshot, texts};
use crate::paint::region_cache::{
    region_display_chunk_total, region_head, region_source_for, trim_cap, PaneRegionState,
    RegionLiveCache,
};

// ── T2-1: split-batch oscillation (core regression) ─────────────────────

#[test]
fn split_batch_edj_erase_keeps_snapshot_and_chunk_total() {
    let mut t = live_terminal(4, 40, &["alpha", "beta", "gamma"]);
    let cache = RegionLiveCache::default();
    let pane = 11u64;

    // Tick text lands on the cursor row (viewport row 3): the spinner line
    // exists, the cursor rests on it (the plan's precondition).
    t.process(b"\r\n");
    t.process("\x1b[35m◒\x1b[39m  validating [0s]".as_bytes());
    let baseline = {
        let st = snapshot(&cache, &t, pane);
        assert_eq!(st.slots.len() + st.dyn_slots.len(), 4);
        region_display_chunk_total(&st, 40)
    };
    assert_eq!(baseline, 4, "alpha+beta+gamma+spinner, one chunk each");

    // The split-batch erase frame: CHA1+ED[J alone. The erased cursor row
    // collapses region_head (3) below the stored hw (4) — L1 must hold the
    // row in the snapshot as a blank slot, L2 must hold the display edge.
    t.process(b"\x1b[1G\x1b[J");
    {
        let st = snapshot(&cache, &t, pane);
        assert_eq!(
            texts(&st),
            ["alpha", "beta", "gamma", ""],
            "L1: the erased row stays in the snapshot as a blank slot"
        );
        assert_eq!(
            region_display_chunk_total(&st, 40),
            baseline,
            "L2: the blank slot keeps its chunk — the height must not shrink"
        );
    }

    // The text half of the tick rewrites the same row: no growth either.
    t.process("\x1b[35m◒\x1b[39m  validating [1s]".as_bytes());
    {
        let st = snapshot(&cache, &t, pane);
        assert_eq!(
            region_display_chunk_total(&st, 40),
            baseline,
            "the restore frame must not grow the layout"
        );
    }

    // Fresh log content keeps growing monotonically.
    t.process(b"\r\nnext log line");
    let st = snapshot(&cache, &t, pane);
    assert!(
        region_display_chunk_total(&st, 40) > baseline,
        "real content growth stays monotone"
    );
}

// ── T2-2: real openclaw tick bytes ──────────────────────────────────────

/// E1 ground truth (46602B capture, 603 pairs @ 9.3Hz): CHA1 + ED mode 0 +
/// SGR-styled spinner text, redrawn in place.
const REAL_TICK: &str =
    "\x1b[1G\x1b[J\x1b[35m◒\x1b[39m  \x1b[38;5;209mrequested — package-install\x1b[39m [0s]";

#[test]
fn real_openclaw_tick_bytes_keep_chunk_total_stable() {
    let mut t = live_terminal(6, 80, &["Phase: requested"]);
    // Primer: one whole tick (single batch) lands the text on the cursor
    // row — the precondition (tick text on a row, cursor resting on it).
    t.process(REAL_TICK.as_bytes());
    let cache = RegionLiveCache::default();
    let pane = 12u64;
    let baseline = {
        let st = snapshot(&cache, &t, pane);
        region_display_chunk_total(&st, 80)
    };

    // Gate assertion (P3): the openclaw workload is a linear capture —
    // ED mode 0 never trips the screen-takeover heuristic (actions_csi
    // notes primary-screen ownership on 2J/EL2 only), so the gate must
    // NOT disable the fix for its own target scenario.
    let grid = t.grid();
    let source = region_source_for(&t, grid).expect("flag on + begin sampled");
    assert!(
        source.clamp_high_water,
        "linear live block must clamp (clamp_high_water == true)"
    );

    // Three back-to-back ticks at the split-batch cadence: erase half,
    // sync, text half, sync.
    let (erase, text) = REAL_TICK.split_at("\x1b[1G\x1b[J".len());
    for _ in 0..3 {
        t.process(erase.as_bytes());
        {
            let st = snapshot(&cache, &t, pane);
            assert_eq!(
                texts(&st),
                [""],
                "L1 holds the erased spinner row across the real bytes"
            );
            assert_eq!(region_display_chunk_total(&st, 80), baseline);
        }
        t.process(text.as_bytes());
        {
            let st = snapshot(&cache, &t, pane);
            assert_eq!(region_display_chunk_total(&st, 80), baseline);
        }
    }
    let st = snapshot(&cache, &t, pane);
    assert_eq!(
        texts(&st),
        ["◒  requested — package-install [0s]"],
        "the spinner text survives the tick cycle intact"
    );
    assert_eq!(
        cache.rebuilds(),
        1,
        "tick frames ride the advance tier — the clamp must work there too"
    );
}

// ── T2-3: trim_cap re-base (L2 only) ────────────────────────────────────

#[test]
fn trim_cap_rebases_display_high_water_not_head_high_water() {
    // 30 slots, five trailing blanks: the raw display core ends at 25 and
    // the L2 clamp sits exactly there (as if a sync had raised it).
    let mut state = PaneRegionState {
        begin_position: 0,
        grid_revision: 0,
        registry_generation: 0,
        cols: 20,
        anchor_idx: 30,
        anchor_position: 30,
        slots: (0..30)
            .map(|i| {
                if i >= 25 {
                    slot("", i, i + 1)
                } else {
                    slot("row", i, i + 1)
                }
            })
            .collect(),
        dyn_slots: Vec::new(),
        cumulative: (0..=30u32).collect(),
        head_high_water_doc: 42,
        display_last_high_water: 25,
    };
    assert_eq!(state.clamped_display_range(), (0, 25));

    // A long session scrolls: trim_cap drains the 10 oldest slots.
    trim_cap(&mut state, 20);
    assert_eq!(state.slots.len(), 20);
    // L2 re-bases in lockstep: 25 - 10 = 15 (the five blanks are still the
    // tail of the surviving slots). Un-rebased it would clamp `last` to 20
    // and drag five phantom blank rows into the layout (inflated).
    assert_eq!(state.display_last_high_water, 15);
    // L1 is doc-index domain: a slot prefix drain shifts positions, not
    // document indices — no re-base.
    assert_eq!(state.head_high_water_doc, 42);
    let (first, last) = state.clamped_display_range();
    assert_eq!(
        (first, last),
        (0, 15),
        "bounded by the raw core, not inflated"
    );
    // The re-based clamp sits exactly on the raw core edge — the surviving
    // blank tail (local slots 15..20) stays outside the display domain.
    assert_eq!(state.clamped_display_range(), state.raw_display_range());
}

// ── T2-4: new-block reset (no ghost padding) ────────────────────────────

#[test]
fn new_begin_resets_high_waters_without_ghost_padding() {
    let mut t = live_terminal(4, 40, &["alpha", "beta", "gamma"]);
    t.process(b"\r\n");
    t.process(b"spinner [0s]");
    let cache = RegionLiveCache::default();
    let pane = 13u64;
    {
        let st = snapshot(&cache, &t, pane);
        assert_eq!(st.head_high_water_doc, 4, "the tick row raised the head hw");
        assert_eq!(st.display_last_high_water, 4);
    }
    // Erase state: the blank tick row is held by the clamp (hw 4 > raw
    // head 3), so the old block's hw strictly exceeds the next block's
    // initial head (the plan's precondition).
    t.process(b"\x1b[1G\x1b[J");
    {
        let st = snapshot(&cache, &t, pane);
        assert_eq!(texts(&st), ["alpha", "beta", "gamma", ""]);
        assert_eq!(st.head_high_water_doc, 4);
    }
    // A new command (133;D/A/B): begin changes → both high-waters reset
    // BEFORE the effective head is computed — the new block's snapshot
    // must not inherit the old block's padding.
    t.process(b"\x1b]133;D;0\x07\x1b]133;A\x07\x1b]133;B\x07");
    let st = snapshot(&cache, &t, pane);
    let grid = t.grid();
    assert_eq!(
        st.head_high_water_doc,
        region_head(grid),
        "reset to the fresh block's own head (4 would be the ghost)"
    );
    assert_eq!(st.display_last_high_water, 0, "no display padding either");
    assert!(
        texts(&st).is_empty(),
        "no ghost slot for the old block's erased row"
    );
}

// ── T2-5: registry drift keeps the high-waters ──────────────────────────

#[test]
fn registry_drift_wholesale_keeps_high_waters() {
    let mut t = live_terminal(4, 40, &["alpha", "beta", "gamma"]);
    t.process(b"\r\n");
    t.process(b"spinner [0s]");
    let cache = RegionLiveCache::default();
    let pane = 14u64;
    {
        let st = snapshot(&cache, &t, pane);
        assert_eq!(st.head_high_water_doc, 4);
    }
    // Erase frame: the blank row survives via the clamp.
    t.process(b"\x1b[1G\x1b[J");
    {
        let st = snapshot(&cache, &t, pane);
        assert_eq!(texts(&st), ["alpha", "beta", "gamma", ""]);
    }
    // Registry drift WITHOUT a begin change (a fresh OSC-8 registration,
    // no cell writes): every fast tier misses → wholesale. The carried
    // high-waters must survive the rebuild — otherwise the collapsed head
    // would drop the blank slot for one frame (the P2 single-frame
    // collapse window).
    let rebuilds_before = cache.rebuilds();
    t.process(b"\x1b]8;;https://weft.dev/drift\x1b\\\x1b]8;;\x1b\\");
    let st = snapshot(&cache, &t, pane);
    assert_eq!(
        cache.rebuilds(),
        rebuilds_before + 1,
        "registry drift must force the wholesale tier"
    );
    assert_eq!(
        texts(&st),
        ["alpha", "beta", "gamma", ""],
        "the blank slot survives the wholesale rebuild"
    );
    assert_eq!(st.head_high_water_doc, 4, "kept, not reset");
    assert_eq!(st.display_last_high_water, 4, "kept, not reset");
    assert_eq!(region_display_chunk_total(&st, 40), 4);
}

// ── T2-6: the screen-origin gate ────────────────────────────────────────

#[test]
fn screen_origin_frames_collapse_while_pseudo_block_clamps() {
    // Branch A — screen-origin live block: the TUI owns the screen, the
    // gate turns the clamp OFF and erase frames keep collapsing (TUI
    // redraw IS the intent — the pre-v1.20.11 semantics, preserved).
    let mut t = Terminal::new(4, 40);
    t.set_live_block_grid_render(true);
    t.process(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
    // Two absolute CUP moves → primary-screen TUI ownership →
    // begin_screen_owned_output → in_flight.screen_origin == true.
    t.process(b"\x1b[2;1H\x1b[3;1H");
    assert!(
        t.block_tracker()
            .in_flight()
            .expect("133;C puts the tracker in the executing phase")
            .screen_origin,
        "test setup: screen-owned live block"
    );
    t.process(b"header\r\nbody\r\nfooter");
    let cache = RegionLiveCache::default();
    let pane = 15u64;
    let grid = t.grid();
    let source = region_source_for(&t, grid).expect("screen-owned begin anchors the region");
    assert!(
        !source.clamp_high_water,
        "screen-origin in_flight → clamp OFF"
    );
    let baseline = {
        cache.sync(pane, &source, grid.num_cols);
        let st = cache.state(pane).expect("snapshot");
        assert_eq!(
            (st.head_high_water_doc, st.display_last_high_water),
            (0, 0),
            "a screen-origin block never raises the high-waters"
        );
        region_display_chunk_total(&st, 40)
    };
    t.process(b"\x1b[1G\x1b[J");
    let grid = t.grid();
    let source = region_source_for(&t, grid).unwrap();
    assert!(!source.clamp_high_water);
    cache.sync(pane, &source, grid.num_cols);
    let st = cache.state(pane).unwrap();
    assert!(
        region_display_chunk_total(&st, 40) < baseline,
        "the erase frame still collapses — semantics faithfully preserved"
    );

    // Branch B — pseudo-block frame (in_flight None between submit and the
    // shell's 133;B): unwrap_or(false) → the clamp applies (linear echo).
    let mut p = Terminal::new(6, 80);
    p.set_live_block_grid_render(true);
    p.process(b"\x1b]133;A\x07$ \x1b[Hecho hi\r\n");
    for ch in "tail -f app.log".chars() {
        p.editor_mut().buffer.insert_char(ch);
    }
    p.submit_command();
    assert!(
        p.block_tracker().in_flight().is_none(),
        "test setup: the pre-133;B pseudo-block window"
    );
    p.process(b"tail -f app.log\r\noutput line");
    let grid = p.grid();
    let source = region_source_for(&p, grid).expect("the primed anchor yields a source");
    assert!(
        source.clamp_high_water,
        "pseudo-block (in_flight None) → clamp ON"
    );
    let cache = RegionLiveCache::default();
    let baseline = {
        cache.sync(pane, &source, grid.num_cols);
        let st = cache.state(pane).expect("snapshot");
        region_display_chunk_total(&st, 80)
    };
    assert_eq!(baseline, 2);
    // The pseudo-block's own split-batch erase: the clamp holds the row.
    p.process(b"\x1b[1G\x1b[J");
    let grid = p.grid();
    let source = region_source_for(&p, grid).unwrap();
    cache.sync(pane, &source, grid.num_cols);
    let st = cache.state(pane).unwrap();
    assert_eq!(texts(&st), ["tail -f app.log", ""]);
    assert_eq!(region_display_chunk_total(&st, 80), baseline);
}

// ── T2-7: clamp invariants ──────────────────────────────────────────────

#[test]
fn clamped_display_range_invariants_hold() {
    // Empty snapshot degenerates to (0, 0) even with stale high-waters.
    let empty = PaneRegionState {
        begin_position: 0,
        grid_revision: 0,
        registry_generation: 0,
        cols: 20,
        anchor_idx: 0,
        anchor_position: 0,
        slots: Vec::new(),
        dyn_slots: Vec::new(),
        cumulative: vec![0],
        head_high_water_doc: 9,
        display_last_high_water: 7,
    };
    assert_eq!(empty.clamped_display_range(), (0, 0), "empty → (0, 0)");

    // A stale hw beyond total_slots clamps to the bound (never out of
    // bounds); first never exceeds last.
    let stale = PaneRegionState {
        begin_position: 0,
        grid_revision: 0,
        registry_generation: 0,
        cols: 20,
        anchor_idx: 1,
        anchor_position: 1,
        slots: vec![slot("a", 0, 1)],
        dyn_slots: vec![slot("", 1, 2)],
        cumulative: vec![0, 1, 2],
        head_high_water_doc: 9,
        display_last_high_water: 9,
    };
    assert_eq!(stale.clamped_display_range(), (0, 2));

    // Leading blanks: first follows the raw core, min-ordered against the
    // clamped last.
    let lead = PaneRegionState {
        begin_position: 0,
        grid_revision: 0,
        registry_generation: 0,
        cols: 20,
        anchor_idx: 2,
        anchor_position: 2,
        slots: vec![slot("", 0, 1), slot("a", 1, 2)],
        dyn_slots: Vec::new(),
        cumulative: vec![0, 0, 1],
        head_high_water_doc: 2,
        display_last_high_water: 2,
    };
    assert_eq!(lead.clamped_display_range(), (1, 2));

    // Monotone content (no erase): raw == clamped — zero drift on the
    // non-flicker path.
    let plain = PaneRegionState {
        begin_position: 0,
        grid_revision: 0,
        registry_generation: 0,
        cols: 20,
        anchor_idx: 2,
        anchor_position: 2,
        slots: vec![slot("a", 0, 1), slot("b", 1, 2)],
        dyn_slots: Vec::new(),
        cumulative: vec![0, 1, 2],
        head_high_water_doc: 2,
        display_last_high_water: 2,
    };
    assert_eq!(plain.clamped_display_range(), (0, 2));

    // All-blank with a stale hw stays a degenerate (empty) display core.
    let blank = PaneRegionState {
        begin_position: 0,
        grid_revision: 0,
        registry_generation: 0,
        cols: 20,
        anchor_idx: 1,
        anchor_position: 1,
        slots: vec![slot("", 0, 1)],
        dyn_slots: vec![slot("", 1, 2)],
        cumulative: vec![0, 1, 2],
        head_high_water_doc: 5,
        display_last_high_water: 5,
    };
    assert_eq!(blank.clamped_display_range(), (2, 2));
}

// ── T2-9: resize height shrink (the extent clamp) ───────────────────────

#[test]
fn resize_shrink_clamps_head_high_water_to_extent() {
    let mut t = live_terminal(6, 40, &["alpha", "beta", "gamma"]);
    t.process(b"\r\n\r\n");
    t.process(b"spinner [0s]");
    let cache = RegionLiveCache::default();
    let pane = 16u64;
    {
        let st = snapshot(&cache, &t, pane);
        assert_eq!(st.head_high_water_doc, 5, "the spinner row is doc row 4");
    }
    // Shrink 6 → 3: the reflow collapses the never-written blank rows (3
    // and 5), so the document shrinks to 4 rows — the historical head hw
    // (5) now exceeds the current extent (scrollback 1 + rows 3 = 4) and
    // must be clamped, or rebuild_dyn's scan would fail (idx >= extent →
    // None) and tear the snapshot with the revision already persisted.
    t.resize(3, 40);
    {
        let st = snapshot(&cache, &t, pane);
        let total = st.slots.len() + st.dyn_slots.len();
        assert!(
            !st.dyn_slots.is_empty(),
            "not torn: rebuild_dyn re-read a live tail inside the extent"
        );
        assert_eq!(total, 4, "alpha + beta + gamma + spinner — extent-bounded");
        let (first, last) = st.clamped_display_range();
        assert_eq!(
            (first, last),
            (0, 4),
            "the extent clamp bounds the drifted hw (5)"
        );
    }
    // The persisted revision keeps serving the same bounded snapshot.
    let grid = t.grid();
    let source = region_source_for(&t, grid).unwrap();
    cache.sync(pane, &source, grid.num_cols);
    assert_eq!(cache.rebuilds(), 1, "the post-resize frame must D7-hit");
}

// ── T2-10: scrollback eviction drift ────────────────────────────────────

#[test]
fn scrollback_eviction_drift_stays_bounded() {
    // Scrollback cap 6: after it fills, every further scroll evicts the
    // oldest row and the doc-index domain shifts downward — the stored
    // high-waters drift upward relative to the retained window (the T1-b
    // documented drift). The erase frames interleaved in the eviction
    // window keep raising the clamps; every sync must stay in bounds
    // (first <= last <= total_slots, total <= extent) — the extent clamp
    // catches the drifted head hw at use time and the display clamp's
    // total_slots bound catches the drifted display hw.
    let mut t = Terminal::new(4, 40);
    t.set_live_block_grid_render(true);
    t.process(b"\x1b]133;A\x07\x1b]133;B\x07");
    feed_lines(&mut t, &["L1", "L2", "L3", "L4"]);
    let cache = RegionLiveCache::default();
    let pane = 17u64;
    {
        let st = snapshot(&cache, &t, pane);
        assert_eq!(st.head_high_water_doc, 4, "L4 is doc row 3");
    }
    // The spinner cycle (erase + rewrite) established the clamp state:
    // the erased spinner row stays in the snapshot (4 slots, raw last 3,
    // clamped last 4) and the chunk total keeps the erased row's chunk.
    t.process(b"spinner [0s]");
    t.process(b"\x1b[1G\x1b[J");
    {
        let st = snapshot(&cache, &t, pane);
        assert_eq!(
            region_display_chunk_total(&st, 40),
            4,
            "L1-L4 with the held blank slot"
        );
    }
    // Scroll into and through the eviction window with the tick cycle
    // running: erase frames land inside the window (L11/L12 iterations).
    for n in 5..=12 {
        t.process(b"\r\n");
        t.process(format!("L{n}").as_bytes());
        t.process(b"\x1b[1G\x1b[J");
        t.process(format!("◒ tick [{n}]").as_bytes());
        let grid = t.grid();
        let extent = grid.scrollback.len() as u64 + grid.num_rows as u64;
        let st = snapshot(&cache, &t, pane);
        let total = st.slots.len() + st.dyn_slots.len();
        let (first, last) = st.clamped_display_range();
        assert!(first <= last && last <= total, "display domain in bounds");
        assert!(total > 0, "no torn empty snapshot while content exists");
        assert!(
            total as u64 <= extent,
            "the snapshot never exceeds the live grid range"
        );
    }
}
