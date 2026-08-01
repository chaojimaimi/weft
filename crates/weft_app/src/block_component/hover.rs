//! Block hover and header action resolution.
//!
//! [`hovered_block_for_row`] resolves which finalized block owns a rendered
//! row for hover highlighting. [`block_header_action_at`] resolves inline
//! header actions (copy/fold) from `HitRegion`s.

use weft_core::blocks::BlockId;
use weft_core::selection::BlockViewRowKind;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BlockHeaderAction {
    Copy(BlockId),
    ToggleFold(BlockId),
    /// v1.8.2: Diagnose a failed block via local Ollama.
    Diagnose(BlockId),
    /// v1.8.2: Close the diagnose panel for this block.
    CloseDiagnose(BlockId),
}

/// Resolve the finalized block that owns a rendered row for hover purposes.
/// Header rows must retain hover because that is where inline actions are
/// painted; otherwise moving from the command text onto an action makes the
/// action disappear before it can be clicked.
pub(crate) fn hovered_block_for_row(
    kind: &BlockViewRowKind,
    block_id: Option<BlockId>,
) -> Option<BlockId> {
    match kind {
        BlockViewRowKind::Header | BlockViewRowKind::Command | BlockViewRowKind::Output => block_id,
        // v1.8.2: Diagnose panel rows belong to their block so hover stays
        // active while the mouse is over the panel (the close button needs it).
        BlockViewRowKind::DiagnosePanel => block_id,
        BlockViewRowKind::Separator | BlockViewRowKind::LiveCommand => None,
    }
}

/// Resolve only the inline header actions. Half-open bounds ensure a point on
/// an adjacent row belongs to exactly one block and cannot fall through to
/// terminal text selection.
pub(crate) fn block_header_action_at(
    regions: &[crate::overlay::HitRegion],
    x: f32,
    y: f32,
) -> Option<BlockHeaderAction> {
    regions.iter().find_map(|region| {
        if !region.contains_half_open(x, y) {
            return None;
        }
        match region.target {
            crate::overlay::HitTarget::BlockActionCopy(id) => Some(BlockHeaderAction::Copy(id)),
            crate::overlay::HitTarget::BlockActionFold(id) => {
                Some(BlockHeaderAction::ToggleFold(id))
            }
            crate::overlay::HitTarget::BlockActionDiagnose(id) => {
                Some(BlockHeaderAction::Diagnose(id))
            }
            crate::overlay::HitTarget::BlockDiagnoseClose(id) => {
                Some(BlockHeaderAction::CloseDiagnose(id))
            }
            _ => None,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::{HitRegion, HitTarget};
    use weft_core::blocks::BlockId;
    use weft_core::selection::BlockViewRowKind;

    #[test]
    fn block_header_actions_use_half_open_row_boundaries() {
        let regions = vec![
            HitRegion {
                x0: 80.0,
                y0: 0.0,
                x1: 100.0,
                y1: 10.0,
                target: HitTarget::BlockActionCopy(BlockId(1)),
            },
            HitRegion {
                x0: 80.0,
                y0: 10.0,
                x1: 100.0,
                y1: 20.0,
                target: HitTarget::BlockActionCopy(BlockId(2)),
            },
        ];

        assert_eq!(
            block_header_action_at(&regions, 90.0, 10.0),
            Some(BlockHeaderAction::Copy(BlockId(2)))
        );
        assert_eq!(block_header_action_at(&regions, 100.0, 10.0), None);
    }

    #[test]
    fn block_header_row_retains_hover_for_inline_actions() {
        let id = BlockId(7);
        assert_eq!(
            hovered_block_for_row(&BlockViewRowKind::Header, Some(id)),
            Some(id)
        );
        assert_eq!(
            hovered_block_for_row(&BlockViewRowKind::Command, Some(id)),
            Some(id)
        );
        assert_eq!(
            hovered_block_for_row(&BlockViewRowKind::Output, Some(id)),
            Some(id)
        );
        assert_eq!(
            hovered_block_for_row(&BlockViewRowKind::Separator, Some(id)),
            None
        );
        assert_eq!(
            hovered_block_for_row(&BlockViewRowKind::LiveCommand, Some(id)),
            None
        );
    }

    #[test]
    fn block_header_action_resolver_ignores_non_actions() {
        let regions = vec![HitRegion {
            x0: 0.0,
            y0: 0.0,
            x1: 20.0,
            y1: 20.0,
            target: HitTarget::BlockFold(BlockId(1)),
        }];
        assert_eq!(block_header_action_at(&regions, 10.0, 10.0), None);
    }

    #[test]
    fn block_header_action_resolver_handles_diagnose() {
        let regions = vec![HitRegion {
            x0: 50.0,
            y0: 0.0,
            x1: 70.0,
            y1: 20.0,
            target: HitTarget::BlockActionDiagnose(BlockId(3)),
        }];
        assert_eq!(
            block_header_action_at(&regions, 60.0, 10.0),
            Some(BlockHeaderAction::Diagnose(BlockId(3)))
        );
    }
}
