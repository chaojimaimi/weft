//! Pure presentation model for BlockView metadata.

use crate::paint::ui_helpers::{abbreviate_path, block_duration_str};
use weft_core::blocks::{Block, BlockId};
use weft_core::selection::BlockViewRowKind;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BlockTone {
    Success,
    Error,
    Warning,
    /// F3-2: A command is currently executing (in-flight block).
    /// Never returned by `block_presentation` (finished blocks always have
    /// an exit code); kept for the exhaustive color match in header rendering
    /// and future use when live blocks gain headers.
    #[allow(dead_code)]
    Running,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BlockPresentation {
    pub(crate) label: String,
    pub(crate) tone: BlockTone,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BlockHeaderAction {
    Copy(BlockId),
    ToggleFold(BlockId),
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
            _ => None,
        }
    })
}

pub(crate) fn block_presentation(block: &Block, output_lines: usize) -> BlockPresentation {
    let duration = block_duration_str(block);
    let status = match block.exit_code {
        Some(code) => format!("exit {code}"),
        None => "interrupted".to_string(),
    };
    let tone = match block.exit_code {
        Some(0) => BlockTone::Success,
        Some(_) => BlockTone::Error,
        None => BlockTone::Warning,
    };

    let mut parts = if block.collapsed {
        let unit = if output_lines == 1 { "line" } else { "lines" };
        vec![format!("{output_lines} {unit}")]
    } else {
        vec![block
            .cwd
            .as_deref()
            .map(abbreviate_path)
            .unwrap_or_else(|| "~".to_string())]
    };
    if !duration.is_empty() {
        parts.push(duration);
    }
    parts.push(status);

    BlockPresentation {
        label: parts.join(" · "),
        tone,
    }
}

/// OpenCode 1.18.x clears its TUI but emits no session card on exit (verified
/// from the raw PTY stream). Surface stable CLI recovery commands without
/// inventing a session id or coupling Weft to OpenCode's private database.
pub(crate) fn command_resume_hints(block: &Block) -> &'static [&'static str] {
    const OPENCODE_HINTS: &[&str] = &[
        "Continue last session: opencode -c",
        "Choose a session: opencode session list; opencode -s <session-id>",
    ];
    let executable = block
        .command
        .split_whitespace()
        .next()
        .and_then(|part| part.rsplit('/').next());
    if block.exit_code != Some(0) && executable == Some("opencode") {
        OPENCODE_HINTS
    } else {
        &[]
    }
}

/// F3-2: Braille spinner glyphs for the running-command activity indicator.
/// Cycled left-to-right by `spinner_phase` (see `spinner_char_for_phase`).
pub(crate) const SPINNER_CHARS: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// F3-2: Map a normalized phase [0, 1) to a braille spinner glyph.
/// Returns `●` (static dot) when `reduce_motion` is true or the phase is
/// negative (disabled). Pure logic — unit-tested.
pub(crate) fn spinner_char_for_phase(phase: f32, reduce_motion: bool) -> char {
    if reduce_motion || phase < 0.0 {
        return '●';
    }
    let idx = ((phase * SPINNER_CHARS.len() as f32) as usize) % SPINNER_CHARS.len();
    SPINNER_CHARS[idx]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};
    use weft_core::blocks::{Block, BlockId};

    fn block(exit_code: Option<i32>, collapsed: bool) -> Block {
        let started_at = SystemTime::UNIX_EPOCH;
        Block {
            id: BlockId(1),
            command: "cargo test".into(),
            cwd: Some("/tmp/weft".into()),
            output: "one\ntwo\nthree\n".into(),
            exit_code,
            started_at,
            finished_at: Some(started_at + Duration::from_millis(1200)),
            collapsed,
        }
    }

    #[test]
    fn collapsed_block_uses_one_line_summary() {
        let presentation = block_presentation(&block(Some(0), true), 3);
        assert_eq!(presentation.label, "3 lines · 1.2s · exit 0");
        assert_eq!(presentation.tone, BlockTone::Success);
    }

    #[test]
    fn expanded_block_keeps_context_and_surfaces_failure() {
        let presentation = block_presentation(&block(Some(7), false), 3);
        assert_eq!(presentation.label, "/tmp/weft · 1.2s · exit 7");
        assert_eq!(presentation.tone, BlockTone::Error);
    }

    #[test]
    fn interrupted_block_has_warning_tone() {
        let presentation = block_presentation(&block(None, true), 1);
        assert_eq!(presentation.label, "1 line · 1.2s · interrupted");
        assert_eq!(presentation.tone, BlockTone::Warning);
    }

    #[test]
    fn interrupted_opencode_block_surfaces_stable_resume_commands() {
        let mut interrupted = block(None, false);
        interrupted.command = "/Users/me/.opencode/bin/opencode".into();
        assert_eq!(
            command_resume_hints(&interrupted),
            [
                "Continue last session: opencode -c",
                "Choose a session: opencode session list; opencode -s <session-id>",
            ]
        );

        interrupted.exit_code = Some(130);
        assert!(!command_resume_hints(&interrupted).is_empty());
        interrupted.exit_code = Some(0);
        assert!(command_resume_hints(&interrupted).is_empty());
    }

    #[test]
    fn spinner_char_returns_static_dot_for_reduce_motion() {
        assert_eq!(spinner_char_for_phase(0.0, true), '●');
        assert_eq!(spinner_char_for_phase(0.5, true), '●');
    }

    #[test]
    fn spinner_char_returns_static_dot_for_negative_phase() {
        assert_eq!(spinner_char_for_phase(-1.0, false), '●');
    }

    #[test]
    fn spinner_char_cycles_through_all_glyphs() {
        let n = SPINNER_CHARS.len();
        for (i, expected) in SPINNER_CHARS.iter().enumerate() {
            let phase = i as f32 / n as f32;
            assert_eq!(spinner_char_for_phase(phase, false), *expected);
        }
    }

    #[test]
    fn spinner_char_wraps_around_at_one() {
        // Phase exactly 1.0 should wrap to index 0.
        assert_eq!(spinner_char_for_phase(1.0, false), SPINNER_CHARS[0]);
        // Phase slightly less than 1.0 should be the last glyph.
        let last_idx = SPINNER_CHARS.len() - 1;
        let last = last_idx as f32 / SPINNER_CHARS.len() as f32;
        assert_eq!(
            spinner_char_for_phase(last + 0.001, false),
            SPINNER_CHARS[last_idx]
        );
    }

    #[test]
    fn block_header_actions_use_half_open_row_boundaries() {
        use crate::overlay::{HitRegion, HitTarget};

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
        use crate::overlay::{HitRegion, HitTarget};

        let regions = vec![HitRegion {
            x0: 0.0,
            y0: 0.0,
            x1: 20.0,
            y1: 20.0,
            target: HitTarget::BlockFold(BlockId(1)),
        }];
        assert_eq!(block_header_action_at(&regions, 10.0, 10.0), None);
    }
}
