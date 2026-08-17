//! v1.10.26 Batch A (FIX_SELECTION_CONTENT_ANCHORS): content-anchor model
//! tests. Written FIRST (TDD red) against the not-yet-existing
//! `BlockSelAnchor` / `SelectionContentSource` / `SelectionInterval` types —
//! the old row-snapshot model cannot express these assertions (its text()
//! freezes the snapshot, so an in-place rewrite or a scroll would produce
//! stale highlight/text).
//!
//! The model under test: the selection anchors live in the unified logical
//! document (`prefix ++ screen` for the live segment, block output lines for
//! finished blocks); the per-frame highlight is DERIVED from the current
//! content source, never snapshotted.

use super::{BlockSelAnchor, BlockViewSelection, SelectionContentSource};
use std::collections::HashMap;

/// In-memory content source mirroring the app-side BlockTracker document:
/// `order` is the document order (Some(id) by completion, None last), `lines`
/// holds each segment's logical lines. Line indices are STABLE across frames
/// — the "push-window" invariant the app's compose guarantees.
struct MemSource {
    order: Vec<Option<u64>>,
    lines: HashMap<Option<u64>, Vec<String>>,
}

impl MemSource {
    fn live(lines: Vec<String>) -> Self {
        Self {
            order: vec![None],
            lines: HashMap::from([(None, lines)]),
        }
    }

    fn with_blocks(blocks: Vec<(u64, Vec<String>)>) -> Self {
        let mut order: Vec<Option<u64>> = blocks.iter().map(|(id, _)| Some(*id)).collect();
        order.push(None);
        let mut lines = HashMap::new();
        for (id, ls) in blocks {
            lines.insert(Some(id), ls);
        }
        lines.insert(None, Vec::new());
        Self { order, lines }
    }

    fn rewrite(
        &mut self,
        block: Option<u64>,
        from: usize,
        to: usize,
        text: impl Fn(usize) -> String,
    ) {
        let lines = self.lines.get_mut(&block).expect("segment exists");
        for (i, line) in lines.iter_mut().enumerate() {
            if (from..=to).contains(&i) {
                *line = text(i);
            }
        }
    }
}

impl SelectionContentSource for MemSource {
    fn line_count(&self, block: Option<u64>) -> usize {
        self.lines.get(&block).map_or(0, Vec::len)
    }

    fn line_text(&self, block: Option<u64>, line: usize) -> Option<&str> {
        self.lines
            .get(&block)
            .and_then(|lines| lines.get(line))
            .map(String::as_str)
    }

    fn block_order(&self) -> &[Option<u64>] {
        &self.order
    }
}

// ── Problem 3 regression (the test that drove this refactor) ─────────────

/// Drag-select, scroll across frames, and let the TUI rewrite screen rows in
/// place. The highlight row set must stay identical to the anchor interval,
/// `text()` must read the CURRENT content, and reversing head/tail must not
/// flip the result. The old row-snapshot model is red on this: its `text()`
/// and highlight freeze the drag-start content, so an in-place rewrite
/// desynchronizes what the user sees from what copies.
#[test]
fn drag_select_through_scroll_and_in_place_rewrite_keeps_anchor_interval_exact() {
    let mut source = MemSource::live((0..24).map(|i| format!("orig-{i}")).collect());
    // Frame 1: drag from line 5 to line 12 (tail beyond the selection start).
    let sel = BlockViewSelection::new(
        BlockSelAnchor {
            block: None,
            line: 5,
            char_offset: 0,
        },
        BlockSelAnchor {
            block: None,
            line: 12,
            char_offset: 3,
        },
    );
    let interval = sel.interval(&source);
    let highlighted: Vec<usize> = (0..24)
        .filter(|&line| interval.contains(&source, None, line))
        .collect();
    assert_eq!(
        highlighted,
        (5..=12).collect::<Vec<_>>(),
        "highlight must equal the anchor interval"
    );
    let expected_frame1 = (5..=12)
        .map(|i| {
            if i == 12 {
                "orig-12".chars().take(3).collect::<String>()
            } else {
                format!("orig-{i}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        sel.text(&source),
        expected_frame1,
        "text must slice endpoint char offsets"
    );

    // Frames 2..=5: the viewport keeps scrolling while the drag is held past
    // the edge. The ANCHOR is content-based; re-deriving the interval on the
    // same (unchanged) content source must be a fixed point.
    for _ in 0..5 {
        let interval = sel.interval(&source);
        let highlighted: Vec<usize> = (0..24)
            .filter(|&line| interval.contains(&source, None, line))
            .collect();
        assert_eq!(highlighted, (5..=12).collect::<Vec<_>>());
        assert_eq!(
            sel.text(&source),
            expected_frame1,
            "scrolling must not move the anchors"
        );
    }

    // Last frames: the TUI rewrites rows 8..=12 IN PLACE (same positions,
    // new text — e.g. a status bar the app redraws). The anchors follow the
    // line POSITIONS, so the interval and text must now read the new content.
    source.rewrite(None, 8, 12, |i| format!("new-{i}"));
    let interval = sel.interval(&source);
    let highlighted: Vec<usize> = (0..24)
        .filter(|&line| interval.contains(&source, None, line))
        .collect();
    assert_eq!(
        highlighted,
        (5..=12).collect::<Vec<_>>(),
        "rewrite must not move or drop lines"
    );
    let expected_rewritten = (5..=12)
        .map(|i| {
            let text = if i >= 8 {
                format!("new-{i}")
            } else {
                format!("orig-{i}")
            };
            if i == 12 {
                text.chars().take(3).collect::<String>()
            } else {
                text
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        sel.text(&source),
        expected_rewritten,
        "text() must read the CURRENT content, not a frozen snapshot"
    );

    // Direction invariance: swapping head/tail yields the same interval and
    // the same copy — the direction the drag was made must not matter.
    let reversed = BlockViewSelection::new(sel.tail, sel.head);
    let rev_interval = reversed.interval(&source);
    for line in 0..24 {
        assert_eq!(
            rev_interval.contains(&source, None, line),
            interval.contains(&source, None, line),
            "reversed endpoints must not flip the highlighted set (line {line})"
        );
    }
    assert_eq!(
        reversed.text(&source),
        sel.text(&source),
        "reversed endpoints copy identically"
    );
}

// ── Unified index stability (push-window invariant) ───────────────────────

/// Lines that scroll out MUST keep their original indices (the prefix that
/// absorbs them keeps them addressable). Rebuilding the composed document
/// between frames — or appending new lines at the bottom (streaming) — must
/// never move an established anchor.
#[test]
fn pushed_out_lines_keep_their_indices_after_recompose_and_append() {
    let source = MemSource::live((0..20).map(|i| format!("line-{i}")).collect());
    let sel = BlockViewSelection::new(
        BlockSelAnchor {
            block: None,
            line: 5,
            char_offset: 0,
        },
        BlockSelAnchor {
            block: None,
            line: 7,
            char_offset: 4,
        },
    );
    let before = sel.text(&source);

    // Re-compose (e.g. a snapshot refresh) with identical content: the
    // anchors are absolute indices, so the interval/text must be unchanged.
    let recomposed = MemSource::live((0..20).map(|i| format!("line-{i}")).collect());
    assert_eq!(
        sel.text(&recomposed),
        before,
        "recompose must not disturb anchors"
    );
    assert!(
        sel.interval(&recomposed).contains(&recomposed, None, 6),
        "middle line stays selected after recompose"
    );

    // Streaming append (the screen scrolled, history grew at the bottom):
    // line_count(None) grows, but lines 5..=7 keep their identity.
    let grown = MemSource::live((0..30).map(|i| format!("line-{i}")).collect());
    assert_eq!(
        sel.text(&grown),
        before,
        "streaming append must not shift existing lines"
    );
    let interval = sel.interval(&grown);
    assert!(!interval.contains(&grown, None, 4));
    assert!(interval.contains(&grown, None, 5));
    assert!(interval.contains(&grown, None, 7));
    assert!(!interval.contains(&grown, None, 8));
}

/// A structural change (split / block finish / removal) makes the anchors
/// meaningless — the app clears the selection (see the app-side fingerprint
/// tests). At the model level, an anchor to a segment that no longer renders
/// must degrade safely (no highlight, empty copy), never panic.
#[test]
fn anchor_into_a_vanished_segment_degrades_safely() {
    let mut source = MemSource::with_blocks(vec![(1, vec!["a".into(), "b".into()])]);
    let sel = BlockViewSelection::new(
        BlockSelAnchor {
            block: Some(1),
            line: 0,
            char_offset: 0,
        },
        BlockSelAnchor {
            block: None,
            line: 0,
            char_offset: 1,
        },
    );
    // Segment 1 vanishes (it was split away / deleted).
    source.order = vec![None];
    source.lines.remove(&Some(1));
    let interval = sel.interval(&source);
    for line in 0..4 {
        assert!(
            !interval.contains(&source, None, line),
            "no highlight once the block is gone"
        );
    }
    assert_eq!(sel.text(&source), "", "empty copy once the block is gone");
}

// ── Document-order comparator ─────────────────────────────────────────────

/// Selection across blocks follows completion order (Some(oldest) … Some(newest)
/// then None), then line, then char_offset — never raw id or snapshot order.
#[test]
fn document_order_spans_blocks_then_live_and_ignores_raw_ids() {
    // Completion order: block 1, block 2, live (None). Note block 2 has a
    // HIGHER id than... no — ids are arbitrary; the ORDER list is the
    // authority (block 7 completes BEFORE block 3 here).
    let mut source = MemSource::with_blocks(vec![
        (7, vec!["seven-a".into(), "seven-b".into()]),
        (3, vec!["three-a".into()]),
    ]);
    source.lines.insert(
        None,
        vec![
            "live-0".into(),
            "live-1".into(),
            "live-2".into(),
            "live-3".into(),
        ],
    );

    let sel = BlockViewSelection::new(
        BlockSelAnchor {
            block: Some(7),
            line: 0,
            char_offset: 0,
        },
        BlockSelAnchor {
            block: None,
            line: 2,
            char_offset: 2,
        },
    );
    let interval = sel.interval(&source);
    // Block 7 fully inside (its 2 lines).
    assert!(interval.contains(&source, Some(7), 0));
    assert!(interval.contains(&source, Some(7), 1));
    // Block 3 fully inside (1 line) — its id is numerically smaller, but
    // completion order puts it AFTER block 7 and BEFORE live.
    assert!(interval.contains(&source, Some(3), 0));
    // Live: lines 0..=2 inside, line 3 outside.
    assert!(interval.contains(&source, None, 0));
    assert!(interval.contains(&source, None, 1));
    assert!(interval.contains(&source, None, 2));
    assert!(!interval.contains(&source, None, 3));
    // char_range on the block-3 line is FULL (it is a middle block).
    assert_eq!(
        interval.char_range(&source, Some(3), 0),
        Some((0, "three-a".chars().count()))
    );
    // char_range on the live tail endpoint slices at char_offset 2.
    assert_eq!(interval.char_range(&source, None, 2), Some((0, 2)));

    assert_eq!(
        sel.text(&source),
        "seven-a\nseven-b\nthree-a\nlive-0\nlive-1\nli",
        "copy joins segments in document order with exact endpoint slicing"
    );
}

/// Endpoint char offsets only tighten the TWO boundary lines; an endpoint
/// sharing a line with the far end (single-segment selection) slices both.
#[test]
fn same_segment_endpoints_clamp_on_both_sides() {
    let source = MemSource::live(vec!["abcdef".into(), "012345".into(), "XYZ".into()]);
    // Whole middle line + head/tail slices.
    let sel = BlockViewSelection::new(
        BlockSelAnchor {
            block: None,
            line: 0,
            char_offset: 2,
        },
        BlockSelAnchor {
            block: None,
            line: 2,
            char_offset: 1,
        },
    );
    assert_eq!(sel.text(&source), "cdef\n012345\nX");
    // Single-line selection with reversed chars.
    let one = BlockViewSelection::new(
        BlockSelAnchor {
            block: None,
            line: 1,
            char_offset: 5,
        },
        BlockSelAnchor {
            block: None,
            line: 1,
            char_offset: 2,
        },
    );
    assert_eq!(one.text(&source), "234");
    // Empty (head == tail).
    let empty = BlockViewSelection::new(
        BlockSelAnchor {
            block: None,
            line: 1,
            char_offset: 2,
        },
        BlockSelAnchor {
            block: None,
            line: 1,
            char_offset: 2,
        },
    );
    assert!(empty.is_empty());
    assert_eq!(empty.text(&source), "");
}

/// Stale anchors beyond a shrunk/rewritten segment clamp to the new edge
/// (Warp `update_selection_after_height_change` behavior) instead of
/// panicking or highlighting nothing.
#[test]
fn out_of_range_endpoints_clamp_to_segment_bounds() {
    let mut source = MemSource::live(vec!["short".into()]);
    let sel = BlockViewSelection::new(
        BlockSelAnchor {
            block: None,
            line: 900,
            char_offset: 0,
        },
        BlockSelAnchor {
            block: None,
            line: 0,
            char_offset: 2,
        },
    );
    // Both endpoints clamp into the 1-line document.
    let interval = sel.interval(&source);
    assert!(interval.contains(&source, None, 0));
    assert_eq!(sel.text(&source), "sh");

    // Segment shrinks to zero lines → no highlight, empty copy.
    source.rewrite(None, 0, 0, |_| String::new());
    source.lines.insert(None, Vec::new());
    let interval = sel.interval(&source);
    assert!(!interval.contains(&source, None, 0));
    assert_eq!(sel.text(&source), "");
}

/// Soft-wrapped rendering: the SOURCE serves whole logical lines (a wrap is a
/// rendering artifact), so a selection that visually spans chunks copies the
/// full logical line exactly once, and char offsets are against the logical
/// line, not a chunk.
#[test]
fn soft_wrapped_lines_copy_full_logical_line_and_slice_char_offsets() {
    let source = MemSource::live(vec![
        "0123456789abcdefghij".into(), // visually wrapped into 2 chunks of 10
        "tail".into(),
    ]);
    let sel = BlockViewSelection::new(
        BlockSelAnchor {
            block: None,
            line: 0,
            char_offset: 8,
        },
        BlockSelAnchor {
            block: None,
            line: 1,
            char_offset: 2,
        },
    );
    assert_eq!(
        sel.text(&source),
        "89abcdefghij\nta",
        "slice crosses the visual wrap boundary"
    );
}

/// head/tail on the same line produce a single-line interval whose char range
/// stays within that line for every layout row.
#[test]
fn single_line_selection_never_spills_to_neighboring_lines() {
    let source = MemSource::live(vec!["aaaa".into(), "bbbbbb".into(), "cccc".into()]);
    let sel = BlockViewSelection::new(
        BlockSelAnchor {
            block: None,
            line: 1,
            char_offset: 1,
        },
        BlockSelAnchor {
            block: None,
            line: 1,
            char_offset: 4,
        },
    );
    for line in 0..3 {
        let contained = sel.interval(&source).contains(&source, None, line);
        assert_eq!(
            contained,
            line == 1,
            "only the endpoint line is highlighted"
        );
    }
    assert_eq!(sel.text(&source), "bbb");
}

// ── v1.10.26 rust-reviewer S2: structural-row banding (straddle) ───────────

/// Selecting WITHIN a single segment (both endpoints inside it) must NOT band
/// that segment's structural rows — only the actually-selected lines
/// highlight. Previously "any line in segment" lit the whole header and
/// command when the user selected one word mid-block.
#[test]
fn structural_rows_unbanded_when_selection_is_wholly_inside_the_block() {
    let source =
        MemSource::with_blocks(vec![(7, vec!["aaaa".into(), "bbbb".into(), "cccc".into()])]);
    let sel = BlockViewSelection::new(
        BlockSelAnchor {
            block: Some(7),
            line: 0,
            char_offset: 1,
        },
        BlockSelAnchor {
            block: Some(7),
            line: 0,
            char_offset: 3,
        },
    );
    let iv = sel.interval(&source);
    assert!(
        !iv.block_intersects(&source, Some(7)),
        "a mid-block word must not band the block's header/command"
    );
    // The endpoint line itself is still highlighted (char-sliced).
    assert!(iv.contains(&source, Some(7), 0));
    assert_eq!(iv.char_range(&source, Some(7), 0), Some((1, 3)));
}

/// Crossing a segment boundary bands the straddling segments' structural
/// rows — the block is no longer wholly self-contained, so its header and
/// command sit inside the span (old row-snapshot straddle rule).
#[test]
fn structural_rows_banded_when_selection_straddles_segments() {
    let mut source = MemSource::with_blocks(vec![(7, vec!["a".into()]), (9, vec!["bbbb".into()])]);
    source.lines.insert(None, vec!["live-0".into()]);
    // lo in block 7, hi in block 9 → both endpoint segments straddle.
    let sel = BlockViewSelection::new(
        BlockSelAnchor {
            block: Some(7),
            line: 0,
            char_offset: 0,
        },
        BlockSelAnchor {
            block: Some(9),
            line: 0,
            char_offset: 2,
        },
    );
    let iv = sel.interval(&source);
    assert!(iv.block_intersects(&source, Some(7)), "lo block straddled");
    assert!(iv.block_intersects(&source, Some(9)), "hi block straddled");
    assert!(
        !iv.block_intersects(&source, None),
        "live segment is outside the span"
    );
    // Streaming append below never changes the straddle decision.
    source.lines.insert(
        None,
        vec!["live-0".into(), "live-1".into(), "live-2".into()],
    );
    let iv = sel.interval(&source);
    assert!(iv.block_intersects(&source, Some(7)));
    assert!(iv.block_intersects(&source, Some(9)));
    // Extending INTO the live segment bands the live (hi-endpoint) segment.
    let wider = BlockViewSelection::new(
        BlockSelAnchor {
            block: Some(7),
            line: 0,
            char_offset: 0,
        },
        BlockSelAnchor {
            block: None,
            line: 1,
            char_offset: 1,
        },
    );
    let iv = wider.interval(&source);
    assert!(iv.block_intersects(&source, Some(7)), "lo block straddled");
    assert!(
        iv.block_intersects(&source, None),
        "live tail is the hi endpoint"
    );
    // A selection entirely inside block 9 does not band block 9.
    let within = BlockViewSelection::new(
        BlockSelAnchor {
            block: Some(9),
            line: 0,
            char_offset: 0,
        },
        BlockSelAnchor {
            block: Some(9),
            line: 0,
            char_offset: 1,
        },
    );
    let iv = within.interval(&source);
    assert!(!iv.block_intersects(&source, Some(9)));
}
