//! v1.11.3 (PLAN_v1113 §2.4): per-row styled extraction for grid snapshots.
//! Split out of `snapshot.rs` so that file stays within its architecture-gate
//! budget — `styled_row` is a pure Row → (text + spans) transform.

use super::url_to_string;
use super::MAX_SNAPSHOT_LINK_SPANS;
use crate::blocks::{
    encode_underline_style, AttributeSpan, ColorSpan, ForegroundSpan, LinkSpan, ANSI_ATTRIBUTE_MASK,
};
use crate::grid::snapshot_line_map::snapshot_row_extent;
use crate::grid::{CellFlags, Row};
use std::sync::Arc;

pub(crate) struct SnapshotRow {
    pub(crate) text: String,
    pub(crate) foregrounds: Vec<ForegroundSpan>,
    pub(crate) backgrounds: Vec<ForegroundSpan>,
    /// v1.6.1: OSC 8 hyperlink spans.
    pub(crate) links: Vec<LinkSpan>,
    /// v1.7.0-A: ANSI attribute spans (bold/italic/underline/etc) captured
    /// from the live grid cells' flags.
    pub(crate) attributes: Vec<AttributeSpan>,
    /// v1.11.3 (§2.4): explicit SGR 58 underline colors.
    pub(crate) underline_colors: Vec<ColorSpan>,
    pub(crate) text_overflow: bool,
    pub(crate) style_overflow: bool,
}

/// PLAN_v11217 §3.5 (T4): the budget is a per-call argument derived from the
/// tracker's configured cap (`snapshot_text_budget(cap)`) — the former global
/// `SNAPSHOT_TEXT_BUDGET` constant is gone (C-class derived quantity).
pub(crate) fn push_snapshot_text(text: &mut String, source: &str, budget: usize) -> bool {
    let remaining = budget.saturating_sub(text.len());
    if source.len() <= remaining {
        text.push_str(source);
        return true;
    }
    let mut end = remaining.min(source.len());
    while end > 0 && !source.is_char_boundary(end) {
        end -= 1;
    }
    text.push_str(&source[..end]);
    false
}

/// PLAN_v11217 §3.5 (T4, round-7 reclassification B→C): the marker threshold
/// derives from the configured cap passed per call, not a global constant.
pub(crate) fn mark_snapshot_truncated(text: &mut String, text_cap: usize) {
    if text.len() <= text_cap {
        text.push(' ');
    }
}

/// Span-coalescing color pusher (ColorSpan = ForegroundSpan alias, §2.4).
pub(crate) fn push_color_span(
    spans: &mut Vec<ForegroundSpan>,
    color: crate::grid::CellColor,
    char_index: u32,
    used: &mut usize,
    budget: usize,
) -> bool {
    if color == crate::grid::CellColor::Default {
        return true;
    }
    if let Some(span) = spans.last_mut() {
        if span.end == char_index && span.color == color {
            span.end += 1;
            return true;
        }
    }
    if *used >= budget {
        return false;
    }
    spans.push(ForegroundSpan {
        start: char_index,
        end: char_index + 1,
        color,
    });
    *used += 1;
    true
}

pub(crate) fn styled_row<F>(
    row: &Row,
    num_cols: usize,
    text_budget: usize,
    style_budget: Option<usize>,
    url_resolver: &F,
) -> SnapshotRow
where
    F: Fn(u32) -> Option<Arc<str>>,
{
    let last = snapshot_row_extent(row, num_cols);
    let mut text = String::with_capacity(last.min(text_budget));
    let mut foregrounds: Vec<ForegroundSpan> = Vec::new();
    let mut backgrounds: Vec<ForegroundSpan> = Vec::new();
    // v1.6.1: collect link spans. Coalesce adjacent cells with the same URL
    // into a single span to keep the span count small.
    let mut links: Vec<LinkSpan> = Vec::new();
    // v1.7.0-A: collect ANSI attribute spans (bold/italic/underline/etc).
    // Coalesce adjacent cells with the same masked flags into a single span.
    let mut attributes: Vec<AttributeSpan> = Vec::new();
    // v1.11.3 (§2.4): explicit SGR 58 underline colors.
    let mut underline_colors: Vec<ColorSpan> = Vec::new();
    let mut char_index = 0_u32;
    let mut text_overflow = false;
    let mut style_overflow = false;
    let mut style_spans_used = 0_usize;
    for (col, cell) in row.cells.iter().take(last).enumerate() {
        if !cell.flags.contains(CellFlags::WIDE_SPACER) {
            // v1.6.0: contribute the full cluster string when EXTRA is set so
            // block-captured output preserves combining marks, ZWJ emoji, and
            // regional flags. Falls back to the lead `char` when extras are
            // missing (defensive — should not happen if EXTRA is set).
            let cluster: &str = if cell.flags.contains(CellFlags::EXTRA) {
                row.extras.grapheme_at(col).unwrap_or("")
            } else {
                ""
            };
            let push_len = if cluster.is_empty() {
                let c = if cell.character == '\0' {
                    ' '
                } else {
                    cell.character
                };
                c.len_utf8()
            } else {
                cluster.len()
            };
            if text.len() + push_len > text_budget {
                text_overflow = true;
                break;
            }
            if cluster.is_empty() {
                let c = if cell.character == '\0' {
                    ' '
                } else {
                    cell.character
                };
                text.push(c);
            } else {
                text.push_str(cluster);
            }
            if let Some(style_budget) = style_budget.filter(|_| !style_overflow) {
                // v1.11.3 (§2.4): underline colors share the span budget.
                style_overflow = !push_color_span(
                    &mut foregrounds,
                    cell.fg,
                    char_index,
                    &mut style_spans_used,
                    style_budget,
                ) || !push_color_span(
                    &mut backgrounds,
                    cell.bg,
                    char_index,
                    &mut style_spans_used,
                    style_budget,
                ) || cell.underline_color.is_some_and(|color| {
                    !push_color_span(
                        &mut underline_colors,
                        color,
                        char_index,
                        &mut style_spans_used,
                        style_budget,
                    )
                });
            }

            // v1.7.0-A: capture ANSI attribute spans (bold/italic/underline/etc).
            // Mask out grid-internal flags (DIRTY/WIDE_SPACER/CURSOR/etc) —
            // only program-emitted SGR attributes belong in the snapshot.
            // Coalesce with the preceding span when flags match and are
            // adjacent, mirroring the color span coalescing logic.
            // v1.11.3: style joins the coalesce key — Wavy vs Dotted runs
            // with equal flags must stay distinct spans.
            let masked_flags = cell.flags & ANSI_ATTRIBUTE_MASK;
            if !masked_flags.is_empty() {
                let style_u8 = encode_underline_style(cell.underline_style);
                let coalescible = attributes.last_mut().is_some_and(|last| {
                    if last.end == char_index
                        && last.flags == masked_flags
                        && last.underline_style == style_u8
                    {
                        last.end = char_index + 1;
                        true
                    } else {
                        false
                    }
                });
                if !coalescible {
                    attributes.push(AttributeSpan {
                        start: char_index,
                        end: char_index + 1,
                        flags: masked_flags,
                        underline_style: style_u8,
                    });
                }
            }
            // v1.6.1: capture hyperlink spans. Resolve the id via the url_resolver
            // closure (backed by HyperlinkRegistry). Coalesce adjacent cells
            // pointing at the same URL into a single span.
            if cell.flags.contains(CellFlags::HYPERLINK) {
                if let Some(hyperlink_id) = row.extras.hyperlink_id_at(col) {
                    if let Some(url) = url_resolver(hyperlink_id) {
                        if links.len() < MAX_SNAPSHOT_LINK_SPANS {
                            let url_string = url_to_string(url);
                            // Coalesce: extend the last span if it has the same URL.
                            if let Some(last_link) = links.last_mut() {
                                if last_link.end == char_index && last_link.url == url_string {
                                    last_link.end = char_index + 1;
                                } else {
                                    links.push(LinkSpan {
                                        start: char_index,
                                        end: char_index + 1,
                                        url: url_string,
                                    });
                                }
                            } else {
                                links.push(LinkSpan {
                                    start: char_index,
                                    end: char_index + 1,
                                    url: url_string,
                                });
                            }
                        }
                    }
                }
            }
            char_index += 1;
        }
    }
    SnapshotRow {
        text,
        foregrounds,
        backgrounds,
        links,
        attributes,
        underline_colors,
        text_overflow,
        style_overflow,
    }
}
