//! v1.19.0 (PLAN_v1190 §4 P1-2/O3/D7/D8): region-track snapshot cache.
//!
//! The live-block grid render track replaces the captured-output document
//! (`live.output`) with a logical-row snapshot read straight off the grid
//! (`weft_core::grid::region::logical_lines`). [`PaneRegionState`] holds the
//! immutable scrollback part of the region in `slots` (D8: once a logical
//! row has scrolled into the scrollback it can no longer be rewritten, so it
//! is transferred once and never re-decoded), while the viewport tail
//! (`dyn_slots`) is re-read on every mutation. The snapshot is keyed by
//! `(grid.revision, begin_position)` (D7) plus the chunk width; `cols`
//! changes only re-chunk the cached texts.
//!
//! This cache is deliberately separate from [`crate::paint::live_cache`]
//! `LiveLayoutCache` (the capture-track cache): the region track borrows
//! `&RegionSlot.style` into `LaidRow::Output` across the layout pass, which
//! requires an independent `RefCell` so the two tracks' guards can coexist.

#[cfg(test)]
use std::cell::Cell;
use std::cell::{Ref, RefCell};
use std::collections::HashMap;

use weft_core::blocks::StyledLine;
use weft_core::grid::region::{logical_lines, GridRegionSource};
use weft_core::grid::Grid;
use weft_core::hyperlink::HyperlinkRegistry;
use weft_core::vt::Terminal;

use crate::paint::grid_cache::MAX_LAYOUT_LINES_LIVE;
use crate::paint::live_cache::count_line_chunks;

/// Region input source: the grid plus the half-open document-index window
/// `[index_since(begin_position), head)` (D4 head = bottom-up first
/// non-empty viewport row's document index + 1).
pub(crate) struct RegionSource<'g> {
    pub(crate) grid: &'g Grid,
    pub(crate) begin_position: u64,
    pub(crate) head: u64,
    /// v1.19.2 (WP3-A): the terminal's OSC-8 registry, threaded into the
    /// `logical_lines` calls so synced slots carry resolved `LinkSpan`s.
    pub(crate) hyperlinks: &'g HyperlinkRegistry,
    /// v1.19.3 (WP3-B): the registry's mutation generation at build time —
    /// a sync fast-tier key so registry drift alone forces a wholesale.
    pub(crate) registry_generation: u64,
}

/// One logical row of the region snapshot. `start_idx`/`end_idx` delimit the
/// logical row's physical rows in the document-index domain (half-open), so
/// the D6 caret mapping can locate the row that owns the cursor even when
/// the cursor sits mid-wrap-chain.
#[derive(Debug)]
pub(crate) struct RegionSlot {
    pub(crate) text: String,
    pub(crate) style: StyledLine,
    pub(crate) start_idx: u64,
    pub(crate) end_idx: u64,
}

/// Per-pane region snapshot (O3): immutable scrollback prefix (`slots`),
/// per-frame viewport tail (`dyn_slots`), and the chunk prefix sums of the
/// immutable prefix at the sync-time `cols`.
#[derive(Debug)]
pub(crate) struct PaneRegionState {
    pub(crate) begin_position: u64,
    pub(crate) grid_revision: u64,
    /// v1.19.3 (WP3-B): OSC-8 registry generation this snapshot was built
    /// from (part of every sync fast-tier key).
    pub(crate) registry_generation: u64,
    pub(crate) cols: usize,
    /// First physical row NOT covered by `slots` (document-index domain),
    /// always positioned on a logical-row boundary (its row does not extend
    /// a wrap chain), so `slots` and `dyn_slots` tile the region seamlessly.
    pub(crate) anchor_idx: u64,
    /// `anchor_idx`'s scrollback position-domain twin (feeds the per-frame
    /// `logical_lines` call whose `begin_position` is position-domain).
    pub(crate) anchor_position: u64,
    pub(crate) slots: Vec<RegionSlot>,
    pub(crate) dyn_slots: Vec<RegionSlot>,
    /// Chunk prefix sums over `slots` at `cols` (`len == slots.len() + 1`).
    /// The viewport tail is chunk-counted per frame by the assembly.
    pub(crate) cumulative: Vec<u32>,
}

impl PaneRegionState {
    /// Every logical row of the region: immutable prefix then live tail.
    /// (Test-only: production walks `slots`/`dyn_slots` separately.)
    #[cfg(test)]
    pub(crate) fn all_slots(&self) -> impl Iterator<Item = &RegionSlot> {
        self.slots.iter().chain(self.dyn_slots.iter())
    }

    /// D6 logical-row lookup in the P1-6 display domain: the index that the
    /// assembly stamps onto `LaidRow::Output::line` for the row owning
    /// `cursor_doc_idx`. `None` when the cursor is outside the region or on
    /// a dropped blank row.
    pub(crate) fn display_logical_row_of_cursor(&self, cursor_doc_idx: u64) -> Option<usize> {
        let total = self.slots.len() + self.dyn_slots.len();
        let slot_at = |i: usize| -> &RegionSlot {
            if i < self.slots.len() {
                &self.slots[i]
            } else {
                &self.dyn_slots[i - self.slots.len()]
            }
        };
        let (first, last) = region_display_range(total, |i| slot_at(i).text.is_empty());
        (first..last).position(|i| {
            let slot = slot_at(i);
            slot.start_idx <= cursor_doc_idx && cursor_doc_idx < slot.end_idx
        })
    }

    /// Display width of the logical row at `display_row` (the P1-6 display
    /// domain [`Self::display_logical_row_of_cursor`] returns). Feeds the D6
    /// wrap-pending clamp; zero when the row is out of range.
    pub(crate) fn logical_row_width(&self, display_row: usize) -> usize {
        let total = self.slots.len() + self.dyn_slots.len();
        let slot_at = |i: usize| -> &RegionSlot {
            if i < self.slots.len() {
                &self.slots[i]
            } else {
                &self.dyn_slots[i - self.slots.len()]
            }
        };
        let (first, last) = region_display_range(total, |i| slot_at(i).text.is_empty());
        let absolute = first + display_row;
        if absolute >= last {
            return 0;
        }
        weft_core::grid::terminal_text_width(&slot_at(absolute).text)
    }
}

/// Region-track snapshot cache (renderer-level, one entry per pane).
pub(crate) struct RegionLiveCache {
    panes: RefCell<HashMap<u64, PaneRegionState>>,
    #[cfg(test)]
    rebuilds: Cell<usize>,
    #[cfg(test)]
    advances: Cell<usize>,
}

impl Default for RegionLiveCache {
    fn default() -> Self {
        Self {
            panes: RefCell::new(HashMap::new()),
            #[cfg(test)]
            rebuilds: Cell::new(0),
            #[cfg(test)]
            advances: Cell::new(0),
        }
    }
}

impl RegionLiveCache {
    /// Bring the pane's snapshot up to date with `source` at chunk width
    /// `cols`. Hit: `(revision, begin, cols, registry_generation)` unchanged
    /// (D7). Mutating only the revision transfers newly scrolled-in scrollback
    /// rows into `slots` (D8 append) and re-reads the viewport tail; mutating
    /// `cols` only re-chunks the cached texts; registry drift falls through
    /// every tier to the wholesale rebuild; mutating `begin` rebuilds wholesale.
    pub(crate) fn sync(&self, pane_session_id: u64, source: &RegionSource, cols: usize) {
        let revision = source.grid.revision;
        let mut map = self.panes.borrow_mut();
        if map.get(&pane_session_id).is_some_and(|st| {
            st.begin_position == source.begin_position
                && st.grid_revision == revision
                && st.cols == cols
                && st.registry_generation == source.registry_generation
        }) {
            return; // D7 hit.
        }
        if map.get(&pane_session_id).is_some_and(|st| {
            st.begin_position == source.begin_position
                && st.grid_revision == revision
                && st.registry_generation == source.registry_generation
        }) {
            // cols-only change: re-chunk cached texts, no grid re-read.
            let st = map.get_mut(&pane_session_id).unwrap();
            st.cols = cols;
            rebuild_cumulative(st);
            return;
        }
        if map.get(&pane_session_id).is_some_and(|st| {
            st.begin_position == source.begin_position
                && st.cols == cols
                && st.registry_generation == source.registry_generation
        }) {
            let st = map.get_mut(&pane_session_id).unwrap();
            if advance_anchor(st, source.grid, source.head, source.hyperlinks) {
                // Persist the revision or the D7 hit tier above becomes
                // unreachable: every subsequent idle frame would rescan the
                // anchor and re-decode the viewport (review N1).
                st.grid_revision = revision;
                rebuild_dyn(st, source.grid, source.head, source.hyperlinks);
                #[cfg(test)]
                self.advances.set(self.advances.get() + 1);
                return;
            }
            // Transfer failed (decode mismatch): fall through to a wholesale
            // rebuild rather than serving a torn snapshot.
        }
        let mut st = PaneRegionState {
            begin_position: source.begin_position,
            grid_revision: revision,
            registry_generation: source.registry_generation,
            cols,
            anchor_idx: source.grid.scrollback.index_since(source.begin_position) as u64,
            anchor_position: source.begin_position,
            slots: Vec::new(),
            dyn_slots: Vec::new(),
            cumulative: vec![0],
        };
        // Normalize anchor_position to anchor_idx's position-domain twin so
        // the per-frame logical_lines calls start exactly at the anchor row
        // even when `begin_position` predates the scrollback window.
        st.anchor_position = position_of(source.grid, st.anchor_idx);
        if advance_anchor(&mut st, source.grid, source.head, source.hyperlinks) {
            #[cfg(test)]
            self.advances.set(self.advances.get() + 1);
        } else {
            // Defensive: keep the snapshot empty rather than torn; the next
            // revision bump retries.
            st.slots.clear();
            st.cumulative.clear();
            st.cumulative.push(0);
            st.anchor_idx = source.grid.scrollback.index_since(source.begin_position) as u64;
            st.anchor_position = position_of(source.grid, st.anchor_idx);
        }
        rebuild_dyn(&mut st, source.grid, source.head, source.hyperlinks);
        rebuild_cumulative(&mut st);
        map.insert(pane_session_id, st);
        #[cfg(test)]
        self.rebuilds.set(self.rebuilds.get() + 1);
    }

    /// Immutable borrow of the pane's snapshot (layout/caret lifetime domain).
    pub(crate) fn state(&self, pane_session_id: u64) -> Option<Ref<'_, PaneRegionState>> {
        Ref::filter_map(self.panes.borrow(), |map| map.get(&pane_session_id)).ok()
    }

    #[cfg(test)]
    pub(crate) fn rebuilds(&self) -> usize {
        self.rebuilds.get()
    }

    #[cfg(test)]
    pub(crate) fn advances(&self) -> usize {
        self.advances.get()
    }
}

/// D4 head: bottom-up first non-empty viewport row's document index + 1; an
/// all-empty viewport collapses the region to the scrollback prefix. Empty
/// means "no non-whitespace cell" — `char::is_whitespace`, the same Unicode
/// White_Space set the core reference's `trim_end().is_empty()` trims.
pub(crate) fn region_head(grid: &Grid) -> u64 {
    let sb_len = grid.scrollback.len() as u64;
    (0..grid.num_rows)
        .rev()
        .find(|&row| {
            grid.viewport
                .get(row)
                .is_some_and(|r| r.cells.iter().any(|c| !c.character.is_whitespace()))
        })
        .map_or(sb_len, |row| sb_len + row as u64 + 1)
}

/// D6 (PLAN B2 v2): document-index unify — `document_index(cursor.row) =
/// scrollback.len() + cursor.row`. The cursor row is viewport-domain and the
/// slot boundaries are scrollback-oldest-domain; never subtract directly.
pub(crate) fn document_index_of_cursor(grid: &Grid, cursor_row: usize) -> u64 {
    grid.scrollback.len() as u64 + cursor_row as u64
}

/// P1-6 shared boundary: `(first, last)` (half-open) of the non-blank core
/// of a logical-row sequence — leading/trailing blank rows dropped. The
/// assembly consumes slots through this range so the caret mapping and the
/// laid-out `line` indices stay in one domain.
pub(crate) fn region_display_range(len: usize, is_blank: impl Fn(usize) -> bool) -> (usize, usize) {
    let first = (0..len).find(|&i| !is_blank(i)).unwrap_or(len);
    let last = (first..len)
        .rposition(|i| !is_blank(i))
        .map_or(first, |rel| first + rel + 1);
    (first, last)
}

/// P1-4/O2 (zoom fingerprint): the `live_output_version` component's value
/// source switches track under the flag — grid revision (what the region
/// track renders from) instead of the capture-stream version.
pub(crate) fn live_output_version_source(terminal: &Terminal) -> u64 {
    if terminal.is_live_block_grid_render() {
        terminal.grid().revision
    } else {
        terminal.block_tracker().live_output_version()
    }
}

/// P1-8 (block_scroll_metrics): the visual-row total the region assembly
/// emits for the live segment — P1-6 display domain, chunked at `cols`,
/// exactly the `rows.len()` the layout pass produces (minus the structural
/// LiveCommand/LiveHeader/Separator rows, which the metrics live term adds
/// separately as `+2` + cwd). Slot texts are already `trim_end`ed by the
/// core, so blank detection is `text.is_empty()`.
pub(crate) fn region_display_chunk_total(state: &PaneRegionState, cols: usize) -> u32 {
    let total_slots = state.slots.len() + state.dyn_slots.len();
    let slot_at = |i: usize| -> &RegionSlot {
        if i < state.slots.len() {
            &state.slots[i]
        } else {
            &state.dyn_slots[i - state.slots.len()]
        }
    };
    let (first, last) = region_display_range(total_slots, |i| slot_at(i).text.is_empty());
    // v1.19.2 (PLAN_v1192 §2) fast path: slots contribute through the
    // maintained `cumulative` prefix sums (O(1) each side) and only the
    // viewport tail (`dyn_slots`, viewport-bounded) counts row by row. The
    // guard is exactly the invariant that makes the table authoritative:
    // same `cols` (the prefix sums are cols-keyed), table covering every
    // slot, and a non-empty display core. ANY miss — including the all-blank
    // boundary (`first == last == total_slots`, review B1: the original loop
    // returns the whole chunk sum there) — routes to the full scan below;
    // no silent side-picking. NR-1: when `last` crosses the slots/dyn
    // boundary the slots term clamps to `cumulative[slots.len()]` and dyn
    // rows count individually.
    if cols == state.cols && state.cumulative.len() == state.slots.len() + 1 && first < last {
        // Both ends clamp to the table (NR-1) then scan the dyn tail:
        // leading dyn blanks carry one chunk each (WP4 session-test catch).
        let mut cum_last = state.cumulative[last.min(state.slots.len())];
        for i in state.slots.len()..last {
            cum_last += count_line_chunks(&slot_at(i).text, cols);
        }
        let mut cum_first = state.cumulative[first.min(state.slots.len())];
        for i in state.slots.len()..first {
            cum_first += count_line_chunks(&slot_at(i).text, cols);
        }
        return cum_last.saturating_sub(cum_first);
    }
    let mut acc: u32 = 0;
    let mut cum_first = 0u32;
    let mut cum_last = 0u32;
    for i in 0..total_slots {
        if i == first {
            cum_first = acc;
        }
        acc += count_line_chunks(&slot_at(i).text, cols);
        if i + 1 == last {
            cum_last = acc;
        }
    }
    cum_last.saturating_sub(cum_first)
}

/// Region source for a pane's live segment, `None` when the track is off
/// (flag OFF — the capture path stays byte-identical) or no begin anchor is
/// sampled (`133;B` lifecycle).
pub(crate) fn region_source_for<'a>(
    terminal: &'a Terminal,
    grid: &'a Grid,
) -> Option<RegionSource<'a>> {
    if !terminal.is_live_block_grid_render() {
        return None;
    }
    let begin_position = terminal.block_tracker().region_begin_position()?;
    Some(RegionSource {
        grid,
        begin_position,
        head: region_head(grid),
        hyperlinks: terminal.hyperlinks(),
        registry_generation: terminal.hyperlinks().generation(),
    })
}

// ── internals ───────────────────────────────────────────────────────────

/// Position-domain twin of a document-index row: `position() - (len - idx)`
/// for retained rows, `position() + (idx - len)` for virtual rows
/// (v1.20.1: viewport-domain anchors) — the exact inverse of
/// `FlatStorage::index_since`.
fn position_of(grid: &Grid, idx: u64) -> u64 {
    let sb_len = grid.scrollback.len() as u64;
    grid.scrollback
        .position()
        .saturating_sub(sb_len.saturating_sub(idx))
        .saturating_add(idx.saturating_sub(sb_len))
}

/// `Row::wrapped` of physical row `idx` (document-index domain): scrollback
/// rows decode through `FlatStorage::get`, viewport rows come off
/// `Grid::viewport` — the same wrapped sources `logical_lines` reads.
fn physical_row_wrapped(grid: &Grid, idx: u64) -> Option<bool> {
    let sb_len = grid.scrollback.len() as u64;
    if idx < sb_len {
        grid.scrollback.get(idx as usize).map(|row| row.wrapped)
    } else {
        grid.viewport
            .get((idx - sb_len) as usize)
            .map(|row| row.wrapped)
    }
}

/// Walk physical rows `[start_idx, end_idx)` folding wrap chains into
/// logical-row ranges. Returns `(ranges, ok)`; `ok == false` on decode
/// failure (defensive — callers keep the previous snapshot).
fn scan_logical_ranges(grid: &Grid, start_idx: u64, end_idx: u64) -> (Vec<(u64, u64)>, bool) {
    let mut ranges = Vec::new();
    let mut cur_start: Option<u64> = None;
    let mut idx = start_idx;
    while idx < end_idx {
        let wrapped = match physical_row_wrapped(grid, idx) {
            Some(wrapped) => wrapped,
            None => return (ranges, false),
        };
        if cur_start.is_none() {
            cur_start = Some(idx);
        }
        if !wrapped {
            ranges.push((cur_start.take().unwrap(), idx + 1));
        }
        idx += 1;
    }
    if let Some(start) = cur_start {
        ranges.push((start, end_idx));
    }
    (ranges, true)
}

/// D8 append: transfer the logical rows that have scrolled fully into the
/// scrollback (immutable domain) from the per-frame tail into `slots`.
/// `anchor_idx` only advances onto logical-row boundaries, so `slots` and
/// the rebuilt `dyn_slots` keep tiling the region seamlessly.
fn advance_anchor(
    state: &mut PaneRegionState,
    grid: &Grid,
    head: u64,
    hyperlinks: &HyperlinkRegistry,
) -> bool {
    let sb_len = grid.scrollback.len() as u64;
    let limit = head.min(sb_len);
    if limit <= state.anchor_idx {
        return true;
    }
    let (ranges, ok) = scan_logical_ranges(grid, state.anchor_idx, limit);
    if !ok {
        return false;
    }
    // A trailing range whose chain is still open (its last physical row
    // wraps) has not scrolled in completely — leave it to the live tail.
    let mut transferable: &[(u64, u64)] = &ranges[..];
    if let Some(last) = ranges.last() {
        if last.1 == limit && physical_row_wrapped(grid, limit - 1) == Some(true) {
            transferable = &ranges[..ranges.len() - 1];
        }
    }
    let Some(end_doc) = transferable.last().map(|range| range.1) else {
        return true;
    };
    let lines = logical_lines(
        grid,
        GridRegionSource {
            begin_position: state.anchor_position,
            head: Some(end_doc),
        },
        // WP3-A: real resolver — slot styles carry resolved URLs. It only
        // feeds `style_runs.links`; row splitting/counting stays wrapped-only.
        |id| hyperlinks.url(id).map(std::sync::Arc::<str>::from),
    );
    if lines.len() != transferable.len() {
        return false;
    }
    for (line, (start, end)) in lines.into_iter().zip(transferable.iter().copied()) {
        // Keep the chunk prefix sums in lockstep with `slots` — the assembly
        // re-uses this table for the immutable prefix (O3).
        let last = state.cumulative.last().copied().unwrap_or(0);
        state
            .cumulative
            .push(last + count_line_chunks(&line.text, state.cols));
        state.slots.push(RegionSlot {
            text: line.text,
            style: line.style_runs,
            start_idx: start,
            end_idx: end,
        });
    }
    state.anchor_idx = end_doc;
    state.anchor_position = position_of(grid, end_doc);
    trim_cap(state, MAX_LAYOUT_LINES_LIVE);
    true
}

/// Re-read the viewport tail `[anchor_idx, head)` (D8: every mutation, the
/// viewport segment is re-taken wholesale).
fn rebuild_dyn(
    state: &mut PaneRegionState,
    grid: &Grid,
    head: u64,
    hyperlinks: &HyperlinkRegistry,
) {
    state.dyn_slots.clear();
    if head <= state.anchor_idx {
        return;
    }
    let (ranges, ok) = scan_logical_ranges(grid, state.anchor_idx, head);
    if !ok {
        return;
    }
    let lines = logical_lines(
        grid,
        GridRegionSource {
            begin_position: state.anchor_position,
            head: Some(head),
        },
        // WP3-A: real resolver (see advance_anchor).
        |id| hyperlinks.url(id).map(std::sync::Arc::<str>::from),
    );
    if lines.len() != ranges.len() {
        return;
    }
    for (line, (start, end)) in lines.into_iter().zip(ranges) {
        state.dyn_slots.push(RegionSlot {
            text: line.text,
            style: line.style_runs,
            start_idx: start,
            end_idx: end,
        });
    }
}

/// Cap the immutable prefix at `cap` logical rows (production passes the
/// capture-track live budget; testable via the parameter). The chunk prefix
/// sums are re-based in lockstep so `cumulative.len() == slots.len() + 1`
/// holds.
fn trim_cap(state: &mut PaneRegionState, cap: usize) {
    let excess = state.slots.len().saturating_sub(cap);
    if excess > 0 {
        let base = state.cumulative[excess];
        state.slots.drain(..excess);
        state.cumulative.drain(..excess);
        for c in &mut state.cumulative {
            *c -= base;
        }
    }
}

/// Re-chunk the immutable prefix's texts at the state's `cols`.
fn rebuild_cumulative(state: &mut PaneRegionState) {
    state.cumulative.clear();
    state.cumulative.push(0);
    for slot in &state.slots {
        let last = state.cumulative.last().copied().unwrap_or(0);
        state
            .cumulative
            .push(last + count_line_chunks(&slot.text, state.cols));
    }
}

#[cfg(test)]
mod tests;
