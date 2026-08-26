use super::{BlockTracker, ShellPhase, MAX_OUTPUT_BYTES};
use crate::grid::{CellColor, CellFlags, UnderlineStyle};
use std::sync::Arc;

/// v1.7.0-A: Mask of `CellFlags` bits captured into the ANSI style RLE.
/// Excludes grid-internal bits (DIRTY/WIDE_SPACER/CURSOR/SELECTION/HYPERLINK/EXTRA)
/// that are not part of the program's emitted SGR attribute set.
pub const ANSI_ATTRIBUTE_MASK: CellFlags = CellFlags::BOLD
    .union(CellFlags::ITALIC)
    .union(CellFlags::UNDERLINE)
    .union(CellFlags::DOUBLE_UNDER)
    .union(CellFlags::STRIKETHROUGH)
    .union(CellFlags::REVERSE)
    .union(CellFlags::DIM)
    .union(CellFlags::HIDDEN);

/// v1.7.0-A: ANSI styling snapshot for a captured character range. Stored
/// alongside `OutputCapture` text so normal command blocks preserve
/// program-emitted colors after the live grid scrolls away. The style is
/// origin-preserving (`CellColor::Palette`/`Rgb`) so theme switches re-resolve
/// palette indices without baking resolved RGB into the capture.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CapturedStyle {
    pub fg: CellColor,
    pub bg: CellColor,
    pub flags: CellFlags,
    /// `true` when the program explicitly emitted any SGR affecting this
    /// range (fg/bg/flags non-default). Drives the semantic-fallback skip in
    /// v1.7.0-C: the classifier must not recolor ANSI-owned ranges.
    pub ansi_owned: bool,
    /// v1.11.3 (PLAN_v1113 §2.4): underline shape (4:x colon subparam).
    /// Wavy/Dotted/Dashed have no flag bit — this field is their carrier.
    pub underline_style: UnderlineStyle,
    /// v1.11.3 (PLAN_v1113 §2.4): explicit underline color from SGR 58.
    pub underline_color: Option<CellColor>,
}

impl CapturedStyle {
    /// Build a `CapturedStyle` from current VT `Attrs`, masking out
    /// grid-internal flags. `ansi_owned` is true when any captured field is
    /// non-default.
    pub(crate) fn from_attrs(
        fg: CellColor,
        bg: CellColor,
        flags: CellFlags,
        underline_style: UnderlineStyle,
        underline_color: Option<CellColor>,
    ) -> Self {
        let flags = flags & ANSI_ATTRIBUTE_MASK;
        let ansi_owned = fg != CellColor::Default
            || bg != CellColor::Default
            || !flags.is_empty()
            || underline_style != UnderlineStyle::Single
            || underline_color.is_some();
        Self {
            fg,
            bg,
            flags,
            ansi_owned,
            underline_style,
            underline_color,
        }
    }

    /// `true` when every field is default (no styling to store). Runs of pure
    /// default are omitted from the RLE to keep memory bounded.
    pub(crate) fn is_default(&self) -> bool {
        self.fg == CellColor::Default
            && self.bg == CellColor::Default
            && self.flags.is_empty()
            && !self.ansi_owned
            && self.underline_style == UnderlineStyle::Single
            && self.underline_color.is_none()
    }
}

/// v1.7.0-A: A run of `CapturedStyle` over a char-indexed range of captured
/// text. Char indices align with `StyledLine`'s coordinate system: wide-cell
/// spacers do not produce an extra index, and `\n` separators are not part of
/// any run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CapturedStyleRun {
    pub start_char: u32,
    pub end_char: u32,
    pub style: CapturedStyle,
}

/// v1.7.0-A: Hard caps on captured style runs. Text is still bounded by
/// `MAX_OUTPUT_BYTES`; these caps bound the parallel style RLE so a
/// pathological SGR stream cannot bloat memory. Over-limit runs are dropped
/// and `style_overflow` is set — text is unaffected.
pub const MAX_STYLE_RUNS_PER_BLOCK: usize = 16_384;
pub const MAX_STYLE_RUNS_PER_LINE: usize = 2_048;

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
    /// v1.7.0-A: ANSI attribute spans (bold/italic/underline/etc) captured
    /// from the live VT attrs during normal block output. `#[serde(default)]`
    /// keeps old SQLite snapshots loadable — lines without attributes
    /// deserialize as empty.
    #[serde(default)]
    pub attributes: Vec<AttributeSpan>,
    /// v1.11.3 (PLAN_v1113 §2.4): explicit SGR 58 underline colors captured
    /// alongside the text. `#[serde(default)]` keeps old SQLite snapshots
    /// loadable — lines without underline colors deserialize as empty.
    #[serde(default)]
    pub underline_colors: Vec<ColorSpan>,
}

impl StyledLine {
    pub fn foreground_at(&self, index: usize) -> Option<CellColor> {
        Self::color_at(&self.foregrounds, index)
    }

    pub fn background_at(&self, index: usize) -> Option<CellColor> {
        Self::color_at(&self.backgrounds, index)
    }

    /// v1.11.3 (PLAN_v1113 §2.4): resolve the underline shape for the char at
    /// `index`. Returns `UnderlineStyle::Single` when no attribute span
    /// covers the index (default attrs). Read-side compat entry point:
    /// legacy snapshots stored only the DOUBLE_UNDER flag bit — the bit wins
    /// whenever the u8 style is absent/Single (see [`compat_underline_style`]).
    pub fn underline_style_at(&self, index: usize) -> UnderlineStyle {
        let Some(span) = self.attribute_span_at(index) else {
            return UnderlineStyle::Single;
        };
        compat_underline_style(span.flags, span.underline_style)
    }

    /// v1.11.3 (PLAN_v1113 §2.4): resolve the explicit SGR 58 underline color
    /// for the char at `index` (None when unset).
    pub fn underline_color_at(&self, index: usize) -> Option<CellColor> {
        Self::color_at_cs(&self.underline_colors, index)
    }

    /// v1.7.0-A: Resolve the ANSI attribute flags for the char at `index`.
    /// Returns `CellFlags::empty()` when no attribute span covers the index
    /// (i.e. the char has default attributes).
    pub fn attributes_at(&self, index: usize) -> CellFlags {
        self.attribute_span_at(index)
            .map(|span| span.flags)
            .unwrap_or(CellFlags::empty())
    }

    fn attribute_span_at(&self, index: usize) -> Option<&AttributeSpan> {
        let Ok(index) = u32::try_from(index) else {
            return None;
        };
        let position = self.attributes.partition_point(|span| span.end <= index);
        self.attributes
            .get(position)
            .filter(|span| span.start <= index && index < span.end)
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

    fn color_at_cs(spans: &[ColorSpan], index: usize) -> Option<CellColor> {
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

/// v1.7.0-A: A run of ANSI attributes (bold/italic/underline/etc) over a
/// char-indexed range. Stored separately from `ForegroundSpan`/`BackgroundSpan`
/// because attributes are not colors — they drive glyph selection and
/// decoration rendering, not fill color.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttributeSpan {
    pub start: u32,
    pub end: u32,
    pub flags: CellFlags,
    /// v1.11.3 (PLAN_v1113 §2.4): underline shape in SGR numbering — 1=Single,
    /// 2=Double, 3=Wavy, 4=Dotted, 5=Dashed; 0 = absent/legacy and reads as
    /// Single. u8 keeps the persisted JSON shape stable across releases;
    /// `#[serde(default)]` keeps old snapshots (no field → 0) loadable.
    #[serde(default)]
    pub underline_style: u8,
}

/// v1.11.3 (PLAN_v1113 §2.4): read-side compat mapping for persisted
/// underline styles — the single entry point for AttributeSpan reads.
/// Legacy data stored only the DOUBLE_UNDER flag bit (no style field): the
/// bit wins whenever the u8 says Single/absent, mirroring the renderer's
/// dual-track priority (PLAN_v1113 §1.2: DOUBLE_UNDER 位优先).
#[must_use]
pub fn compat_underline_style(flags: CellFlags, style: u8) -> UnderlineStyle {
    if flags.contains(CellFlags::DOUBLE_UNDER) {
        return UnderlineStyle::Double;
    }
    match style {
        2 => UnderlineStyle::Double,
        3 => UnderlineStyle::Wavy,
        4 => UnderlineStyle::Dotted,
        5 => UnderlineStyle::Dashed,
        // 0 (legacy/absent) and 1 (explicit Single) both mean Single.
        _ => UnderlineStyle::Single,
    }
}

/// v1.11.3 (PLAN_v1113 §2.4): SGR-numbering encoding for the style carrier
/// stored in [`AttributeSpan`] (inverse of [`compat_underline_style`]).
#[must_use]
pub fn encode_underline_style(style: UnderlineStyle) -> u8 {
    match style {
        UnderlineStyle::Single => 1,
        UnderlineStyle::Double => 2,
        UnderlineStyle::Wavy => 3,
        UnderlineStyle::Dotted => 4,
        UnderlineStyle::Dashed => 5,
    }
}

/// v1.11.3 (PLAN_v1113 §2.4): a run of explicit underline colors over a
/// char-indexed range. Type alias of `ForegroundSpan` — the plan's
/// "span 结构复制 ForegroundSpan 模式" taken literally, so the snapshot
/// color pusher and serde JSON shape are shared, guaranteed identical.
pub type ColorSpan = ForegroundSpan;

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
            self.live_output_version = self.live_output_version.wrapping_add(1);
        }
    }

    pub fn replace_screen_snapshot(&mut self, text: &str, styled: StyledOutput) {
        if self.screen_document_start.is_none() {
            return;
        }
        self.output.replace(text, MAX_OUTPUT_BYTES);
        self.styled_output =
            (self.output.as_str() == text && styled.has_colors()).then(|| Arc::new(styled));
        // v1.10.23 (FIX_LIVE_BLOCK_SCROLL_PERF): the live block's content
        // changed — bump the version so the renderer's LiveLayoutCache
        // rebuilds instead of serving stale cumulative wrap data.
        self.live_output_version = self.live_output_version.wrapping_add(1);
    }

    pub fn defer_screen_command_end(&mut self) {
        self.phase = ShellPhase::AtPrompt;
    }

    /// v1.10.7: reverse [`defer_screen_command_end`](Self::defer_screen_command_end).
    /// A nested shell's `133;B` (internal command of a still-running
    /// screen-owned TUI) cancels the pending exit; restore `CommandExecuting`
    /// so the in-flight session block keeps capturing (via the screen
    /// snapshot) instead of splitting per internal command.
    pub fn resume_screen_command(&mut self) {
        if self.screen_document_start.is_some() {
            self.phase = ShellPhase::CommandExecuting;
        }
    }

    pub fn finish_deferred_screen_command(&mut self, exit_code: Option<i32>) {
        self.finalize(exit_code);
        self.phase = ShellPhase::AtPrompt;
    }
}

// ── v1.7.0-A: CapturedStyleRun → StyledOutput conversion ────────────────

/// Convert a flat char-indexed RLE of `CapturedStyleRun` into a line-indexed
/// `StyledOutput` by splitting `text` on `\n`. Each run is clipped to its
/// line's char range; default-styled ranges are omitted (implicit). Runs
/// spanning a `\n` boundary are split across the two lines.
///
/// This is a pure function — no allocation beyond the output `Vec`s — so it
/// can be unit-tested in isolation and stays O(runs + lines).
pub(crate) fn build_styled_output_from_runs(
    text: &str,
    runs: &[CapturedStyleRun],
) -> Option<StyledOutput> {
    if runs.is_empty() {
        return None;
    }
    let mut lines: Vec<StyledLine> = Vec::new();
    let mut line_start_char: u32 = 0;
    let mut run_idx = 0usize;

    for (line_index, line_text) in (0u32..).zip(text.split('\n')) {
        // FIX_LIVE_STYLED_OUTPUT (review s1): peek_styled makes this a ~10Hz
        // hot path; once every run is consumed no later line can produce a
        // span, so stop instead of char-counting a MiB-scale plain tail.
        if run_idx >= runs.len() {
            break;
        }
        let line_char_len = line_text.chars().count() as u32;
        let line_end_char = line_start_char + line_char_len;
        let mut foregrounds: Vec<ForegroundSpan> = Vec::new();
        let mut backgrounds: Vec<ForegroundSpan> = Vec::new();
        let mut attributes: Vec<AttributeSpan> = Vec::new();
        // v1.11.3 (PLAN_v1113 §2.4): parallel underline-color spans.
        let mut underline_colors: Vec<ColorSpan> = Vec::new();
        let mut line_run_count = 0usize;

        // Consume runs that overlap this line's char range.
        while run_idx < runs.len() {
            let run = &runs[run_idx];
            if run.start_char >= line_end_char {
                break;
            }
            let run_end_in_line = run.end_char.min(line_end_char);
            let run_start_in_line = run.start_char.max(line_start_char);
            if run_start_in_line < run_end_in_line {
                let local_start = run_start_in_line - line_start_char;
                let local_end = run_end_in_line - line_start_char;
                if run.style.fg != CellColor::Default {
                    push_or_coalesce_color(&mut foregrounds, local_start, local_end, run.style.fg);
                }
                if run.style.bg != CellColor::Default {
                    push_or_coalesce_color(&mut backgrounds, local_start, local_end, run.style.bg);
                }
                if !run.style.flags.is_empty() {
                    push_or_coalesce_flags(
                        &mut attributes,
                        local_start,
                        local_end,
                        run.style.flags,
                        // v1.11.3 (PLAN_v1113 §2.4): the style joins the
                        // coalesce key so Wavy and Dotted runs with equal
                        // flags stay distinct spans.
                        encode_underline_style(run.style.underline_style),
                    );
                }
                if let Some(color) = run.style.underline_color {
                    push_or_coalesce_color_cs(&mut underline_colors, local_start, local_end, color);
                }
                line_run_count += 1;
                if line_run_count > MAX_STYLE_RUNS_PER_LINE {
                    // Per-line cap exceeded: drop this line's styles but keep
                    // text. Subsequent lines may still recover.
                    foregrounds.clear();
                    backgrounds.clear();
                    attributes.clear();
                    underline_colors.clear();
                    // Advance past all runs in this line.
                    while run_idx < runs.len() && runs[run_idx].start_char < line_end_char {
                        run_idx += 1;
                    }
                    break;
                }
            }
            if run.end_char <= line_end_char {
                run_idx += 1;
            } else {
                // Run extends past this line — keep it for the next line.
                break;
            }
        }

        if !foregrounds.is_empty()
            || !backgrounds.is_empty()
            || !attributes.is_empty()
            || !underline_colors.is_empty()
        {
            lines.push(StyledLine {
                line: line_index,
                foregrounds,
                backgrounds,
                links: Vec::new(),
                attributes,
                underline_colors,
            });
        }
        line_start_char = line_end_char + 1; // +1 for the `\n` separator
    }

    if lines.is_empty() {
        None
    } else {
        Some(StyledOutput { lines })
    }
}

fn push_or_coalesce_color(spans: &mut Vec<ForegroundSpan>, start: u32, end: u32, color: CellColor) {
    if let Some(last) = spans.last_mut() {
        if last.end == start && last.color == color {
            last.end = end;
            return;
        }
    }
    spans.push(ForegroundSpan { start, end, color });
}

fn push_or_coalesce_color_cs(spans: &mut Vec<ColorSpan>, start: u32, end: u32, color: CellColor) {
    if let Some(last) = spans.last_mut() {
        if last.end == start && last.color == color {
            last.end = end;
            return;
        }
    }
    spans.push(ColorSpan { start, end, color });
}

fn push_or_coalesce_flags(
    spans: &mut Vec<AttributeSpan>,
    start: u32,
    end: u32,
    flags: CellFlags,
    underline_style: u8,
) {
    if let Some(last) = spans.last_mut() {
        if last.end == start && last.flags == flags && last.underline_style == underline_style {
            last.end = end;
            return;
        }
    }
    spans.push(AttributeSpan {
        start,
        end,
        flags,
        underline_style,
    });
}

#[cfg(test)]
mod style_rle_tests {
    use super::*;
    use crate::grid::{CellColor, CellFlags, Color, UnderlineStyle};

    fn fg(palette: u8) -> CapturedStyle {
        CapturedStyle::from_attrs(
            CellColor::Palette(palette),
            CellColor::Default,
            CellFlags::empty(),
            UnderlineStyle::Single,
            None,
        )
    }

    fn bold() -> CapturedStyle {
        CapturedStyle::from_attrs(
            CellColor::Default,
            CellColor::Default,
            CellFlags::BOLD,
            UnderlineStyle::Single,
            None,
        )
    }

    fn run(start: u32, end: u32, style: CapturedStyle) -> CapturedStyleRun {
        CapturedStyleRun {
            start_char: start,
            end_char: end,
            style,
        }
    }

    #[test]
    fn empty_runs_produce_none() {
        assert!(build_styled_output_from_runs("hello", &[]).is_none());
    }

    #[test]
    fn single_line_single_run() {
        let runs = vec![run(0, 5, fg(2))];
        let styled = build_styled_output_from_runs("hello", &runs).expect("styled");
        let line = styled.line(0).expect("line 0");
        assert_eq!(line.foreground_at(0), Some(CellColor::Palette(2)));
        assert_eq!(line.foreground_at(4), Some(CellColor::Palette(2)));
    }

    #[test]
    fn run_spanning_newline_splits_across_lines() {
        // A bold run from char 2 to char 8 spans the `\n` at char 5
        // (text "ab\ncdef" → chars: a=0,b=1,\n=2? NO — \n is not a char in
        // any run; char indices skip \n. Let's use "ab\ncdef": a=0,b=1,\n is
        // separator, c=0,d=1,e=2,f=3 on line 1).
        // For a flat char-indexed RLE: a=0,b=1,\n=2,c=3,d=4,e=5,f=6.
        // A run [1, 5) covers b \n c d → line 0: [1,2), line 1: [0,2).
        let runs = vec![run(1, 5, bold())];
        let styled = build_styled_output_from_runs("ab\ncdef", &runs).expect("styled");
        let l0 = styled.line(0).expect("line 0");
        let l1 = styled.line(1).expect("line 1");
        assert_eq!(l0.attributes_at(0), CellFlags::empty()); // 'a'
        assert_eq!(l0.attributes_at(1), CellFlags::BOLD); // 'b'
        assert_eq!(l1.attributes_at(0), CellFlags::BOLD); // 'c'
        assert_eq!(l1.attributes_at(1), CellFlags::BOLD); // 'd'
        assert_eq!(l1.attributes_at(2), CellFlags::empty()); // 'e'
    }

    #[test]
    fn default_style_runs_are_omitted() {
        // A default-style run should not produce any spans.
        let runs = vec![run(0, 3, CapturedStyle::default())];
        let styled = build_styled_output_from_runs("abc", &runs);
        assert!(
            styled.is_none(),
            "default-only runs produce no styled output"
        );
    }

    #[test]
    fn rgb_truecolor_preserved_in_output() {
        let style = CapturedStyle::from_attrs(
            CellColor::Rgb(Color::rgb(200, 100, 50)),
            CellColor::Default,
            CellFlags::empty(),
            UnderlineStyle::Single,
            None,
        );
        let runs = vec![run(0, 1, style)];
        let styled = build_styled_output_from_runs("x", &runs).expect("styled");
        assert_eq!(
            styled.line(0).unwrap().foreground_at(0),
            Some(CellColor::Rgb(Color::rgb(200, 100, 50)))
        );
    }

    #[test]
    fn multiline_output_indexes_lines_separately() {
        // text: "red\ngreen\nblue"
        // chars: r=0,e=1,d=2,\n=3,g=4,r=5,e=6,e=7,n=8,\n=9,b=10,l=11,u=12,e=13
        let runs = vec![run(0, 3, fg(1)), run(4, 9, fg(2)), run(10, 14, fg(4))];
        let styled = build_styled_output_from_runs("red\ngreen\nblue", &runs).expect("styled");
        assert_eq!(
            styled.line(0).unwrap().foreground_at(0),
            Some(CellColor::Palette(1))
        );
        assert_eq!(
            styled.line(1).unwrap().foreground_at(0),
            Some(CellColor::Palette(2))
        );
        assert_eq!(
            styled.line(2).unwrap().foreground_at(0),
            Some(CellColor::Palette(4))
        );
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
            attributes: Vec::new(),
            underline_colors: Vec::new(),
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
                    attributes: Vec::new(),
                    underline_colors: Vec::new(),
                },
                StyledLine {
                    line: 7,
                    foregrounds: Vec::new(),
                    backgrounds: Vec::new(),
                    links: Vec::new(),
                    attributes: Vec::new(),
                    underline_colors: Vec::new(),
                },
            ],
        };

        assert!(styled.line(1).is_none());
        assert_eq!(styled.line(2).map(|line| line.line), Some(2));
        assert_eq!(styled.line(7).map(|line| line.line), Some(7));
    }
}

#[cfg(test)]
mod v1113_compat_tests {
    use super::*;

    /// v1.11.3 (PLAN_v1113 §4.6): the exact JSON shape v1.10-1.11.2 wrote
    /// (bitflags serde = bit-value integer) must load and read as
    /// Single/None. Pins the real on-disk shape, not a hypothetical one.
    #[test]
    fn legacy_json_without_underline_fields_reads_single_and_none() {
        let old_json = r#"{"lines":[{"line":0,"foregrounds":[],"backgrounds":[],"links":[],"attributes":[{"start":0,"end":3,"flags":"UNDERLINE","underline_style":0}]}]}"#;
        let styled: StyledOutput = serde_json::from_str(old_json).unwrap();
        let line = styled.line(0).unwrap();
        assert_eq!(line.attributes_at(0), CellFlags::UNDERLINE, "flags kept");
        assert_eq!(line.underline_style_at(0), UnderlineStyle::Single);
        assert_eq!(line.underline_color_at(0), None, "no field → None");
    }

    /// v1.11.3: the legacy DOUBLE_UNDER bit-only capture (old cells set only
    /// the flag; no style carrier) maps to Double at read time.
    #[test]
    fn legacy_double_under_bit_reads_as_double_style() {
        let old_json = r#"{"lines":[{"line":0,"foregrounds":[],"backgrounds":[],"links":[],"attributes":[{"start":0,"end":2,"flags":"DOUBLE_UNDER","underline_style":0}]}]}"#;
        let styled: StyledOutput = serde_json::from_str(old_json).unwrap();
        let line = styled.line(0).unwrap();
        assert_eq!(
            line.underline_style_at(0),
            UnderlineStyle::Double,
            "DOUBLE_UNDER bit alone must map to Double (PLAN_v1113 §2.4)"
        );
    }

    /// v1.11.3 (PLAN_v1113 §4.6): new JSON round-trips Wavy style and the
    /// underline color through serde.
    #[test]
    fn new_json_roundtrip_preserves_wavy_and_color() {
        let line = StyledLine {
            line: 0,
            foregrounds: Vec::new(),
            backgrounds: Vec::new(),
            links: Vec::new(),
            attributes: vec![AttributeSpan {
                start: 0,
                end: 4,
                flags: CellFlags::UNDERLINE,
                underline_style: 3, // Wavy
            }],
            underline_colors: vec![ColorSpan {
                start: 0,
                end: 4,
                color: CellColor::Rgb(crate::grid::Color::rgb(9, 8, 7)),
            }],
        };
        let json = serde_json::to_string(&line).unwrap();
        let back: StyledLine = serde_json::from_str(&json).unwrap();
        assert_eq!(back.underline_style_at(0), UnderlineStyle::Wavy);
        assert_eq!(
            back.underline_color_at(0),
            Some(CellColor::Rgb(crate::grid::Color::rgb(9, 8, 7)))
        );
        assert!(json.contains("\"underline_style\":3"), "json: {json}");
        assert!(json.contains("underline_colors"), "json: {json}");
    }

    /// kd: the bit must also win when an (impossible-in-practice) Wavy u8
    /// conflicts with the bit — the renderer's bit-first priority is
    /// mirrored on the read side (PLAN_v1113 §1.2).
    #[test]
    fn compat_bit_wins_over_style_value() {
        assert_eq!(
            compat_underline_style(CellFlags::DOUBLE_UNDER, 3),
            UnderlineStyle::Double
        );
        assert_eq!(
            compat_underline_style(CellFlags::empty(), 0),
            UnderlineStyle::Single
        );
        assert_eq!(
            compat_underline_style(CellFlags::empty(), 1),
            UnderlineStyle::Single
        );
    }
}
