use super::{BlockTracker, ShellPhase, MAX_OUTPUT_BYTES};
use crate::grid::CellColor;
use std::sync::Arc;

/// Detached cell colors for a finalized primary-screen command transcript.
/// Text remains authoritative for search/copy; this optional parallel model
/// restores terminal colors when the Block view renders TUI history.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StyledOutput {
    pub lines: Vec<StyledLine>,
}

impl StyledOutput {
    pub fn line(&self, index: usize) -> Option<&StyledLine> {
        let index = u32::try_from(index).ok()?;
        self.lines
            .binary_search_by_key(&index, |line| line.line)
            .ok()
            .map(|position| &self.lines[position])
    }

    pub fn has_colors(&self) -> bool {
        !self.lines.is_empty()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StyledLine {
    pub line: u32,
    pub foregrounds: Vec<ForegroundSpan>,
}

impl StyledLine {
    pub fn foreground_at(&self, index: usize) -> Option<CellColor> {
        let index = u32::try_from(index).ok()?;
        let position = self.foregrounds.partition_point(|span| span.end <= index);
        self.foregrounds
            .get(position)
            .and_then(|span| (span.start <= index && index < span.end).then_some(span.color))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ForegroundSpan {
    pub start: u32,
    pub end: u32,
    pub color: CellColor,
}

impl BlockTracker {
    /// Expand a screen-owned document toward an earlier logical row.
    pub fn include_screen_document_position(&mut self, position: u64) {
        if let Some(document_start) = &mut self.screen_document_start {
            *document_start = (*document_start).min(position);
        }
    }

    pub fn set_screen_document_start(&mut self, position: u64) {
        if let Some(document_start) = &mut self.screen_document_start {
            *document_start = position;
        }
    }

    pub fn replace_screen_output(&mut self, snapshot: &str) {
        if self.screen_document_start.is_some() {
            self.output.replace(snapshot, MAX_OUTPUT_BYTES);
            self.styled_output = None;
        }
    }

    pub fn replace_screen_snapshot(&mut self, text: &str, styled: StyledOutput) {
        if self.screen_document_start.is_none() {
            return;
        }
        self.output.replace(text, MAX_OUTPUT_BYTES);
        self.styled_output =
            (self.output.as_str() == text && styled.has_colors()).then(|| Arc::new(styled));
    }

    pub fn defer_screen_command_end(&mut self) {
        self.phase = ShellPhase::AtPrompt;
    }

    pub fn finish_deferred_screen_command(&mut self, exit_code: Option<i32>) {
        self.finalize(exit_code);
        self.phase = ShellPhase::AtPrompt;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rle_foreground_lookup_respects_span_boundaries() {
        let line = StyledLine {
            line: 3,
            foregrounds: vec![
                ForegroundSpan {
                    start: 1,
                    end: 3,
                    color: CellColor::Palette(2),
                },
                ForegroundSpan {
                    start: 5,
                    end: 6,
                    color: CellColor::Palette(4),
                },
            ],
        };

        assert_eq!(line.foreground_at(0), None);
        assert_eq!(line.foreground_at(1), Some(CellColor::Palette(2)));
        assert_eq!(line.foreground_at(2), Some(CellColor::Palette(2)));
        assert_eq!(line.foreground_at(3), None);
        assert_eq!(line.foreground_at(5), Some(CellColor::Palette(4)));
        assert_eq!(line.foreground_at(6), None);
    }

    #[test]
    fn sparse_styled_lines_use_their_original_output_indices() {
        let styled = StyledOutput {
            lines: vec![
                StyledLine {
                    line: 2,
                    foregrounds: Vec::new(),
                },
                StyledLine {
                    line: 7,
                    foregrounds: Vec::new(),
                },
            ],
        };

        assert!(styled.line(1).is_none());
        assert_eq!(styled.line(2).map(|line| line.line), Some(2));
        assert_eq!(styled.line(7).map(|line| line.line), Some(7));
    }
}
