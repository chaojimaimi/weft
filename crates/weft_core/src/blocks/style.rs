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
    #[serde(default)]
    pub foregrounds: Vec<ForegroundSpan>,
    /// Non-default cell backgrounds (prompt bands, selections, and other
    /// TUI-owned emphasis) captured alongside detached primary-screen text.
    /// Defaulting keeps previously persisted snapshots backward compatible.
    #[serde(default)]
    pub backgrounds: Vec<ForegroundSpan>,
    /// v1.6.1: OSC 8 hyperlink spans captured alongside the text. Each span
    /// carries the full URL (deduped via `Arc<str>` to share storage across
    /// lines that link to the same URL). `#[serde(default)]` keeps old
    /// SQLite snapshots loadable — lines without links deserialize as empty.
    #[serde(default)]
    pub links: Vec<LinkSpan>,
}

impl StyledLine {
    pub fn foreground_at(&self, index: usize) -> Option<CellColor> {
        Self::color_at(&self.foregrounds, index)
    }

    pub fn background_at(&self, index: usize) -> Option<CellColor> {
        Self::color_at(&self.backgrounds, index)
    }

    /// v1.6.1: Resolve the URL for the char at `index`, if any.
    pub fn link_at(&self, index: usize) -> Option<&str> {
        let index = u32::try_from(index).ok()?;
        let position = self.links.partition_point(|span| span.end <= index);
        self.links
            .get(position)
            .and_then(|span| (span.start <= index && index < span.end).then_some(span.url.as_str()))
    }

    fn color_at(spans: &[ForegroundSpan], index: usize) -> Option<CellColor> {
        let index = u32::try_from(index).ok()?;
        let position = spans.partition_point(|span| span.end <= index);
        spans
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

/// v1.6.1: A hyperlink span in a captured [`StyledLine`]. `start`/`end` are
/// char indices into the line's text (same coordinate as `ForegroundSpan`).
/// `url` is the full URL string. Stored as `String` (not `Arc<str>`) so serde
/// can derive `Deserialize` — `Arc<str>` doesn't implement `Deserialize`.
/// Memory overhead is negligible: links are rare (<10/line) and URLs are short.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LinkSpan {
    pub start: u32,
    pub end: u32,
    pub url: String,
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
            backgrounds: vec![ForegroundSpan {
                start: 0,
                end: 2,
                color: CellColor::Palette(6),
            }],
            links: Vec::new(),
        };

        assert_eq!(line.foreground_at(0), None);
        assert_eq!(line.foreground_at(1), Some(CellColor::Palette(2)));
        assert_eq!(line.foreground_at(2), Some(CellColor::Palette(2)));
        assert_eq!(line.foreground_at(3), None);
        assert_eq!(line.foreground_at(5), Some(CellColor::Palette(4)));
        assert_eq!(line.foreground_at(6), None);
        assert_eq!(line.background_at(0), Some(CellColor::Palette(6)));
        assert_eq!(line.background_at(1), Some(CellColor::Palette(6)));
        assert_eq!(line.background_at(2), None);
    }

    #[test]
    fn sparse_styled_lines_use_their_original_output_indices() {
        let styled = StyledOutput {
            lines: vec![
                StyledLine {
                    line: 2,
                    foregrounds: Vec::new(),
                    backgrounds: Vec::new(),
                    links: Vec::new(),
                },
                StyledLine {
                    line: 7,
                    foregrounds: Vec::new(),
                    backgrounds: Vec::new(),
                    links: Vec::new(),
                },
            ],
        };

        assert!(styled.line(1).is_none());
        assert_eq!(styled.line(2).map(|line| line.line), Some(2));
        assert_eq!(styled.line(7).map(|line| line.line), Some(7));
    }
}
