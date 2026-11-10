//! v1.19.0 (PLAN_v1190 §3 P0-2, D1/D4/D5): the live-block grid-region read
//! model. The in-flight block's display source switches from the print
//! capture stream to a *row-range snapshot* over the unified document
//! (`scrollback ++ viewport`); archived blocks keep `Block.output`/finalize
//! unchanged.
//!
//! Range semantics (D4): the region is the half-open row range
//! `[index_since(begin_position), head)` over the unified document index
//! space (viewport row `r` is `scrollback.len() + r`), where `head` is
//! `document_index(bottom-up-first-non-empty-row) + 1` — the inclusive-end
//! correction guarantees the last content row is inside the region.
//! `head = None` is an extension slot (v1.19.0 always passes `Some`).
//!
//! Line shape (D1): consecutive physical rows whose wrapped flag is set are
//! merged into one logical line, every logical line is `trim_end`ed, and
//! leading/trailing empty logical lines are KEPT (dropping them is the app
//! layer's job, P1-6). Interior empty rows stay as empty logical lines.
//!
//! Defense (D4): `begin_position > scrollback.position()` — the buffer was
//! cleared and recreated — yields an EMPTY region, mirroring
//! `FlatStorage::index_since`'s guard instead of clamping; a `head` below
//! `start` likewise yields empty (no `max(begin, head)` clamp).
//!
//! Reflow (D5): a resize rewraps the document, so a sampled
//! `begin_position` may point at a different physical row afterwards (begin
//! drift — quantified as s15 evidence in the equivalence suite). Callers
//! rebuild the region wholesale when [`Grid::revision`] changes; this module
//! only reads whatever the current document holds.

use super::snapshot::snapshot_row::{styled_row, SnapshotRow};
use super::snapshot::MAX_SNAPSHOT_COLOR_SPANS;
use super::snapshot_line_map::snapshot_row_extent;
use super::{walk_cells, Grid, Row};
use crate::blocks::{AttributeSpan, ColorSpan, ForegroundSpan, LinkSpan, StyledLine};
use std::sync::Arc;

/// Where a live block's display region starts (and ends) in the unified
/// document. Stateless: every read re-derives the row range from the grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridRegionSource {
    /// Scrollback `position()` sampled at `133;B` (or the screen-owned
    /// `screen_document_start`); see `BlockTracker::region_begin_position`.
    pub begin_position: u64,
    /// Exclusive end as a unified document index. `None` extends to the
    /// document tail (extension slot; v1.19.0 always passes `Some`).
    pub head: Option<u64>,
}

/// One logical line of the region: merged wrapped rows, `trim_end`ed.
#[derive(Debug)]
pub struct RegionLine {
    pub text: String,
    /// Snapshot-side style runs over `text` (walk-step indexed — wide-cell
    /// spacers produce no extra index, the [`StyledLine`] coordinate
    /// system). `line` is always `0`; consumers index the `Vec<RegionLine>`
    /// position. Spans are budget-bounded like snapshots
    /// (`MAX_SNAPSHOT_COLOR_SPANS` per physical row); overflow drops spans,
    /// never text.
    pub style_runs: StyledLine,
    /// True when the region cut mid-logical-line (the end boundary fell
    /// inside a wrapped run while document rows continue past it).
    pub wrapped_tail: bool,
}

/// Materialize the region as logical lines (D1 shape above). A pure read
/// over the grid — nothing is allocated before construction; the returned
/// `Vec` owns its lines.
pub fn logical_lines<F>(grid: &Grid, source: GridRegionSource, url_resolver: F) -> Vec<RegionLine>
where
    F: Fn(u32) -> Option<Arc<str>>,
{
    // Buffer cleared and recreated (flat position reset): the anchor is
    // stale — empty region, never a clamp (D4).
    if source.begin_position > grid.scrollback.position() {
        return Vec::new();
    }
    let total = grid.scrollback.len() as u64 + grid.num_rows as u64;
    let start = grid.scrollback.index_since(source.begin_position) as u64;
    let end = source.head.unwrap_or(total).min(total);
    if start >= end {
        return Vec::new();
    }
    let mut lines: Vec<RegionLine> = Vec::new();
    let mut pending_text = String::new();
    let mut pending_styles = StyledLine::default();
    let mut pending_steps = 0_u32;
    let mut last_row_wrapped = false;
    for idx in start..end {
        let idx = idx as usize;
        let sb_len = grid.scrollback.len();
        // Same row-walk rule as capture_snapshot_equivalence: scrollback
        // rows keep their encoded wrap flag, viewport rows their
        // `Row::wrapped`; undecodable rows are skipped (filter_map rule).
        let decoded: Option<Row> = if idx < sb_len {
            grid.scrollback.get(idx)
        } else {
            None
        };
        let (row, wrapped): (&Row, bool) = if idx < sb_len {
            match decoded.as_ref() {
                Some(decoded_row) => (decoded_row, grid.scrollback.row_wraps(idx)),
                None => continue,
            }
        } else {
            match grid.viewport.get(idx - sb_len) {
                Some(view_row) => (view_row, view_row.wrapped),
                None => continue,
            }
        };
        // No capture text budget: the region is geometry-bounded (grid rows
        // × cols), unlike the tracker's byte-capped snapshot walk.
        let snap = styled_row(
            row,
            grid.num_cols,
            usize::MAX,
            Some(MAX_SNAPSHOT_COLOR_SPANS),
            &url_resolver,
        );
        pending_text.push_str(&snap.text);
        merge_styles(&mut pending_styles, &snap, pending_steps);
        pending_steps = pending_steps.saturating_add(row_walk_steps(row, grid.num_cols));
        last_row_wrapped = wrapped;
        if !wrapped {
            // Close the logical line. An empty physical row closes an empty
            // logical line — interior empty rows are preserved (s14).
            lines.push(RegionLine {
                text: pending_text.trim_end().to_owned(),
                style_runs: std::mem::take(&mut pending_styles),
                wrapped_tail: false,
            });
            pending_text.clear();
            pending_steps = 0;
        }
    }
    // The region ends mid-logical-line (end inside a wrapped run): flush
    // the tail. `wrapped_tail` requires document rows past `end`.
    if !pending_text.is_empty() {
        lines.push(RegionLine {
            text: pending_text.trim_end().to_owned(),
            style_runs: pending_styles,
            wrapped_tail: last_row_wrapped && end < total,
        });
    }
    lines
}

/// Walk-step count of a row — the offset unit for merging style spans of
/// wrapped siblings (each segment's spans index that segment's non-spacer
/// cells; concatenating the walk-step coordinate keeps spans continuous).
fn row_walk_steps(row: &Row, num_cols: usize) -> u32 {
    let extent = snapshot_row_extent(row, num_cols);
    u32::try_from(walk_cells(&row.cells, &row.extras, extent).count()).unwrap_or(u32::MAX)
}

/// Append one physical row's spans to the accumulating logical line,
/// shifted by `offset` walk steps. Spans are not coalesced across the row
/// boundary (adjacent same-style runs stay separate spans — addressable,
/// render-identical).
fn merge_styles(acc: &mut StyledLine, row: &SnapshotRow, offset: u32) {
    if offset == 0 {
        acc.foregrounds.extend_from_slice(&row.foregrounds);
        acc.backgrounds.extend_from_slice(&row.backgrounds);
        acc.underline_colors
            .extend_from_slice(&row.underline_colors);
        acc.attributes.extend_from_slice(&row.attributes);
        acc.links.extend_from_slice(&row.links);
        return;
    }
    acc.foregrounds
        .extend(row.foregrounds.iter().map(|span| ForegroundSpan {
            start: span.start + offset,
            end: span.end + offset,
            ..*span
        }));
    acc.backgrounds
        .extend(row.backgrounds.iter().map(|span| ForegroundSpan {
            start: span.start + offset,
            end: span.end + offset,
            ..*span
        }));
    acc.underline_colors
        .extend(row.underline_colors.iter().map(|span| ColorSpan {
            start: span.start + offset,
            end: span.end + offset,
            ..*span
        }));
    acc.attributes
        .extend(row.attributes.iter().map(|span| AttributeSpan {
            start: span.start + offset,
            end: span.end + offset,
            ..*span
        }));
    acc.links.extend(row.links.iter().map(|span| LinkSpan {
        start: span.start + offset,
        end: span.end + offset,
        ..span.clone()
    }));
}
