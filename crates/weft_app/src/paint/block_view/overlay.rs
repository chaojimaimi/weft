//! Completed-block chrome rows emitted as screen-space overlays (v1.20.0 PLAN
//! §4/§5, S1).
//!
//! Under `experimental.complete_block_overlay = true` the inter-command gap,
//! the header band and the separator of every completed block are no longer
//! pushed into the LaidRow stream. They are described here instead and painted
//! after the content rows, directly from the same geometric source the layout
//! pass uses. Command rows (`LaidRow::Command`) and the clear-command spacer
//! `Blank` rows stay in the stream (PLAN D3/D4), and the resumed-session hint
//! pseudo-rows (`line == usize::MAX`) are guarded in the assembly pass (D7).
//!
//! Layer order (block_view.rs): content rows < overlay rows < sticky header <
//! panel highlight.

use crate::block_component::{BlockPresentation, BlockTone};
use weft_core::blocks::BlockId;

/// Distances are pixel-exact (same domain as `LayoutPassOutput.rows`).
/// `header` uses the pixel-exact header band height because
/// `block_header_band_height` is always an integer multiple of `pitch`
/// (ui_tokens::compact_control_row_span), so per-block overlay pixel cost is
/// uniform in both domains.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum OverlayChromeKind {
    /// Inter-command gap above the command row (1 row, PLAN D3 keeps it out of
    /// the stream; the emission is a vertex-free placeholder that only costs
    /// distance).
    Gap,
    /// The completed block header band (`header_height` px).
    Header,
    /// The separator line under the header band (1 row).
    Separator,
}

/// One piece of completed-block chrome described in screen space instead of
/// the laid-row stream.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OverlayChromeRow {
    pub(crate) kind: OverlayChromeKind,
    pub(crate) block_id: Option<BlockId>,
    /// Distance from the live head, in pixels (same domain as
    /// `LayoutPassOutput.rows`). Stored directly — a rows×pitch round-trip
    /// is not byte-stable in f32 and the parity tests pin exact values.
    pub(crate) dist_px: f32,
    /// Painted height in pixels: `pitch` for Gap/Separator,
    /// `header_height` for Header.
    pub(crate) height_px: f32,
    /// `paint_header` parameters (Header only; empty otherwise).
    pub(crate) header: Option<OverlayHeaderParams>,
}

/// Payload forwarded verbatim to `row_paint::paint_header`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OverlayHeaderParams {
    pub(crate) cwd: String,
    pub(crate) duration: String,
    pub(crate) status: String,
    pub(crate) tone: BlockTone,
}

impl OverlayChromeRow {
    pub(crate) fn gap(dist_px: f32, pitch: f32) -> Self {
        Self {
            kind: OverlayChromeKind::Gap,
            block_id: None,
            dist_px,
            height_px: pitch,
            header: None,
        }
    }

    pub(crate) fn separator(dist_px: f32, pitch: f32, block_id: BlockId) -> Self {
        Self {
            kind: OverlayChromeKind::Separator,
            // Flag-OFF parity: the stream separator's selection key is the
            // preceding row's block (its Header row), so the overlay
            // separator carries the block id too.
            block_id: Some(block_id),
            dist_px,
            height_px: pitch,
            header: None,
        }
    }

    pub(crate) fn header(
        dist_px: f32,
        header_height: f32,
        block_id: BlockId,
        presentation: BlockPresentation,
    ) -> Self {
        Self {
            kind: OverlayChromeKind::Header,
            block_id: Some(block_id),
            dist_px,
            height_px: header_height,
            header: Some(OverlayHeaderParams {
                cwd: presentation.cwd,
                duration: presentation.duration,
                status: presentation.status,
                tone: presentation.tone,
            }),
        }
    }
}

#[cfg(test)]
#[path = "overlay_tests.rs"]
mod overlay_tests;
