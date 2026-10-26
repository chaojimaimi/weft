//! F2 P0-2: Lightweight status hint badge rendered when the terminal is in
//! a non-editor, non-integrated passthrough state.
//!
//! Drawn as a small, semi-transparent text label at the bottom-left of the
//! content area so the user knows why the editor input box is unavailable.
//! "Non-disturbing" design: 50% alpha accent color, single text row.
//!
//! v1.5.3: also surfaces brief config errors (reload/import/export failures)
//! via the same badge so the user sees them even when Settings is closed.
//! Config errors take priority over the passthrough hint because a broken
//! config is more actionable than "passthrough mode".

use crate::paint::primitives::color_to_normalized;
use crate::renderer::MetalRenderer;
use weft_core::blocks::ShellPhase;

/// The status hint text shown for each non-editor state, in priority order
/// Returns `None` in Editor mode and alt-screen mode (full-screen TUIs own
/// every terminal cell).
fn status_hint_text(terminal: &weft_core::vt::Terminal) -> Option<&'static str> {
    if terminal.is_alt_screen_active() {
        // Full-screen TUIs own every cell; never cover their status line.
        return None;
    }
    match terminal.block_tracker().phase() {
        ShellPhase::NotIntegrated => Some("\u{25be} passthrough"),
        ShellPhase::AtPrompt | ShellPhase::CommandExecuting => None,
    }
}

impl MetalRenderer {
    /// Build vertices for the bottom-left status hint badge. Returns an empty
    /// `Vec` when no hint is applicable (Editor / AtPrompt mode).
    ///
    /// v1.5.3 priority order:
    /// 1. `config_status_hint` (set by reload/import/export failures) — shown
    ///    even over alt-screen, because a broken config is critical.
    /// 2. terminal-state passthrough hint (only when not in alt-screen /
    ///    editor / integrated-shell mode).
    pub(crate) fn build_status_hint_vertices(
        &self,
        terminal: &weft_core::vt::Terminal,
    ) -> Vec<f32> {
        let mut verts = Vec::new();
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let vp_h = self.viewport.1;
        if cw <= 0.0 || ch <= 0.0 || vp_h <= 0.0 {
            return verts;
        }
        let ctx = match self.layout_ctx {
            Some(c) => c,
            None => return verts,
        };

        // v1.5.3: config error hint takes priority — shown even over
        // alt-screen so the user always knows the config is broken.
        // Truncated to fit the available width (config errors can be long).
        if let Some(hint) = self.config_status_hint.as_deref() {
            let ui = crate::ui_tokens::UiColors::from_theme(&self.theme)
                .with_increase_contrast(self.increase_contrast);
            // Use the warning color (amber/red family) so a config error
            // stands out from the normal accent-colored passthrough hint.
            let warn = ui.warning;
            let fg = color_to_normalized(warn);
            // Position: bottom-left of the content area, one row above the
            // bottom padding (same as the passthrough hint).
            let x = ctx.left();
            let y = (ctx.bottom() - ch).max(ctx.top());
            // Truncate to fit the content width, leaving 2 cols of margin.
            // Use the `…` ellipsis to signal truncation.
            let avail_cols = ((ctx.right() - x) / cw).max(0.0) as usize;
            let max_cols = avail_cols.saturating_sub(2).max(1);
            let display = truncate_with_ellipsis(hint, max_cols);
            self.push_text(&mut verts, x, y, &display, fg, max_cols);
            return verts;
        }

        let Some(hint) = status_hint_text(terminal) else {
            return verts;
        };
        // Position: bottom-left of the content area, one row above the
        // bottom padding. Keep it subtle — 50% alpha accent color.
        let x = ctx.left();
        let y = (ctx.bottom() - ch).max(ctx.top());
        let ui = crate::ui_tokens::UiColors::from_theme(&self.theme)
            .with_increase_contrast(self.increase_contrast);
        let accent = color_to_normalized(ui.focus);
        let fg = [accent[0], accent[1], accent[2], accent[3] * 0.50];
        let max_cols = hint.chars().count();
        self.push_text(&mut verts, x, y, hint, fg, max_cols);
        verts
    }

    /// v1.11.1 (PLAN_v1111 §4.5): build vertices for the transient
    /// post-paste toast — same badge styling as the status hints but drawn
    /// at the BOTTOM-RIGHT in the warning color so it reads as feedback,
    /// not as a mode indicator.
    ///
    /// Mutual-exclusion rule: a config error badge wins ("config error
    /// priority") — while `config_status_hint` is showing, the toast is
    /// suppressed for that frame; the 1 Hz tick clears it on schedule and
    /// nothing is lost because the paste itself already succeeded.
    ///
    /// Unlike [`Self::build_status_hint_vertices`] this is drawn in Editor
    /// mode too (the toast frequently fires right after an editor-mode
    /// paste, where the prompt box suppresses the left-side hints).
    pub(crate) fn build_paste_toast_vertices(&self) -> Vec<f32> {
        let mut verts = Vec::new();
        let Some((toast, _)) = self.paste_toast.as_ref() else {
            return verts;
        };
        if self.config_status_hint.is_some() {
            return verts;
        }
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let vp_h = self.viewport.1;
        if cw <= 0.0 || ch <= 0.0 || vp_h <= 0.0 {
            return verts;
        }
        let Some(ctx) = self.layout_ctx else {
            return verts;
        };
        let ui = crate::ui_tokens::UiColors::from_theme(&self.theme)
            .with_increase_contrast(self.increase_contrast);
        let fg = color_to_normalized(ui.warning);
        // Bottom-right of the content area, same row as the left-side hints.
        let y = (ctx.bottom() - ch).max(ctx.top());
        // Cap at half the content width so a long toast never collides with
        // the passthrough badge on narrow windows.
        let content_cols = ((ctx.right() - ctx.left()) / cw).max(0.0) as usize;
        let max_cols = (content_cols / 2).clamp(8, 40);
        let display = truncate_with_ellipsis(toast, max_cols);
        // v1.14.4 (批A): column width, not char count — a CJK toast must
        // anchor its right edge exactly at ctx.right instead of overflowing
        // by one CJK width (v1.12.27a panel_query_display precedent).
        let text_cols = MetalRenderer::text_col_width(&display);
        let x = (ctx.right() - text_cols as f32 * cw).max(ctx.left());
        self.push_text(&mut verts, x, y, &display, fg, text_cols.max(1));
        verts
    }
}

/// Truncate `s` to at most `max_cols` visible columns (terminal column
/// width — ASCII 1, CJK 2, matching what `push_text` actually renders). If
/// truncation occurs, `…` is appended with its own width deducted from the
/// budget FIRST so the result never exceeds `max_cols`. Returns the original
/// string if it already fits.
///
/// v1.14.4 (批A): the budget switched from `chars().count()` to
/// [`MetalRenderer::text_col_width`] — the char-count budget truncated a
/// CJK string twice as early and let it overflow its right edge. For
/// single-column ASCII sequences the result is identical to the old
/// implementation. Shared by both badge callers (paste toast + config
/// status hint), so the unit tests cover both paths.
fn truncate_with_ellipsis(s: &str, max_cols: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    if MetalRenderer::text_col_width(s) <= max_cols {
        return s.to_string();
    }
    if max_cols == 0 {
        return String::new();
    }
    let ellipsis_cols = MetalRenderer::text_col_width("…");
    if max_cols <= ellipsis_cols {
        return "…".to_string();
    }
    // '…' width already deducted; keep whole graphemes while they fit.
    let budget = max_cols - ellipsis_cols;
    let mut truncated = String::new();
    let mut used_cols = 0usize;
    for grapheme in s.graphemes(true) {
        let width = MetalRenderer::text_col_width(grapheme);
        if used_cols + width > budget {
            break;
        }
        truncated.push_str(grapheme);
        used_cols += width;
    }
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use weft_core::blocks::ShellPhase;
    use weft_core::vt::Terminal;

    #[test]
    fn no_hint_at_prompt() {
        let mut t = Terminal::new(24, 80);
        // Simulate bootstrap + AtPrompt.
        t.process(b"\x1b]133;A\x07");
        assert_eq!(t.block_tracker().phase(), ShellPhase::AtPrompt);
        assert!(status_hint_text(&t).is_none());
    }

    #[test]
    fn hint_passthrough_when_not_integrated() {
        let t = Terminal::new(24, 80);
        assert_eq!(t.block_tracker().phase(), ShellPhase::NotIntegrated);
        assert_eq!(status_hint_text(&t), Some("\u{25be} passthrough"));
    }

    #[test]
    fn hint_alt_screen_when_active() {
        let mut t = Terminal::new(24, 80);
        t.process(b"\x1b[?1049h");
        assert!(t.is_alt_screen_active());
        assert_eq!(status_hint_text(&t), None);
    }

    #[test]
    fn running_command_uses_block_indicator_instead_of_bottom_overlay() {
        let mut t = Terminal::new(24, 80);
        t.process(b"\x1b]133;A\x07"); // AtPrompt
        t.process(b"\x1b]133;B\x07"); // CommandExecuting
        assert_eq!(t.block_tracker().phase(), ShellPhase::CommandExecuting);
        assert_eq!(status_hint_text(&t), None);
    }

    #[test]
    fn alt_screen_takes_priority_over_running() {
        let mut t = Terminal::new(24, 80);
        t.process(b"\x1b]133;A\x07"); // AtPrompt
        t.process(b"\x1b]133;B\x07"); // CommandExecuting
        t.process(b"\x1b[?1049h"); // alt screen
        assert!(t.is_alt_screen_active());
        assert_eq!(t.block_tracker().phase(), ShellPhase::CommandExecuting);
        assert_eq!(status_hint_text(&t), None);
    }

    // ── v1.5.3 truncate_with_ellipsis ───────────────────────────────

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate_with_ellipsis("abc", 10), "abc");
        assert_eq!(truncate_with_ellipsis("", 10), "");
    }

    #[test]
    fn truncate_exact_fit_no_ellipsis() {
        // String exactly fills max_cols — no truncation needed.
        assert_eq!(truncate_with_ellipsis("abcde", 5), "abcde");
    }

    #[test]
    fn truncate_long_string_gets_ellipsis() {
        // 10 chars, max_cols=8 → first 7 chars + "…"
        assert_eq!(truncate_with_ellipsis("abcdefghij", 8), "abcdefg…");
    }

    #[test]
    fn truncate_one_col_shows_just_ellipsis() {
        assert_eq!(truncate_with_ellipsis("abc", 1), "…");
    }

    #[test]
    fn truncate_zero_cols_returns_empty() {
        assert_eq!(truncate_with_ellipsis("abc", 0), "");
    }

    // ── v1.14.4 (批A) column-width truncation ────────────────────────

    #[test]
    fn truncate_cjk_respects_column_budget() {
        // 5 CJK chars = 10 cols vs budget 6: '…'(1 col) is reserved first,
        // leaving 5 → two CJK chars (4 cols); the third would need 6 > 5.
        assert_eq!(truncate_with_ellipsis("你好世界吗", 6), "你好…");
    }

    #[test]
    fn truncate_cjk_exact_fit_no_ellipsis() {
        // Exactly 6 columns of CJK fits a 6-column budget untouched.
        assert_eq!(truncate_with_ellipsis("你好世", 6), "你好世");
    }

    #[test]
    fn truncate_cjk_one_column_over_truncates() {
        // 3 CJK chars = 6 cols vs budget 5 → truncation fires and the
        // result stays within the budget (2×2 + 1 = 5 cols).
        assert_eq!(truncate_with_ellipsis("你好世", 5), "你好…");
    }

    #[test]
    fn truncate_ellipsis_width_deducted_first() {
        // Budget 2 with CJK input: '…' reserved first leaves 1 column —
        // half a CJK char, which never fits, leaving the bare ellipsis.
        assert_eq!(truncate_with_ellipsis("你好", 2), "…");
        // Budget 3: after the reserve, 2 columns remain = one CJK char.
        assert_eq!(truncate_with_ellipsis("你好", 3), "你…");
    }

    #[test]
    fn truncate_ascii_identical_to_old_char_count_budget() {
        // Single-column ASCII: the column-width budget must produce
        // byte-identical output to the old chars().count() implementation
        // (hardcoded expectations pinned against the old semantics).
        assert_eq!(truncate_with_ellipsis("abc", 10), "abc");
        assert_eq!(truncate_with_ellipsis("", 10), "");
        assert_eq!(truncate_with_ellipsis("abcde", 5), "abcde");
        assert_eq!(truncate_with_ellipsis("abcdefghij", 8), "abcdefg…");
        assert_eq!(truncate_with_ellipsis("abc", 1), "…");
        assert_eq!(truncate_with_ellipsis("abc", 0), "");
    }

    // ── v1.14.4 (批A) paste-toast right-edge anchoring ───────────────

    fn headless() -> Option<MetalRenderer> {
        if metal::Device::system_default().is_none() {
            eprintln!("skipping: no Metal device");
            return None;
        }
        Some(MetalRenderer::new_headless_paint(
            weft_core::config::Theme::weft_dark(),
        ))
    }

    /// The CJK toast's first quad must sit at exactly
    /// `ctx.right() - text_col_width(display) × cw`, i.e. the rendered
    /// right edge lands on the content-area right edge instead of
    /// overflowing by one CJK width (the old chars().count() bug).
    #[test]
    fn paste_toast_cjk_right_edge_lands_on_content_right() {
        let Some(mut renderer) = headless() else {
            return;
        };
        let ctx = renderer
            .layout_ctx
            .expect("headless build fills layout_ctx");
        let toast_text = "已粘贴 1000 行".to_string();
        renderer.set_paste_toast(Some((toast_text.clone(), std::time::Instant::now())));
        let verts = renderer.build_paste_toast_vertices();
        assert!(
            !verts.is_empty(),
            "the setter funnel warms every glyph, so the whole toast must emit"
        );
        let cw = renderer.cell_width() as f32;
        // Mirror the paint-side budget to pin that this fixture is NOT
        // truncated (else the edge assertion below would be trivially true
        // on a shortened display string).
        let content_cols = ((ctx.right() - ctx.left()) / cw).max(0.0) as usize;
        let max_cols = (content_cols / 2).clamp(8, 40);
        let cols = MetalRenderer::text_col_width(&toast_text);
        assert!(
            cols <= max_cols,
            "fixture must fit untruncated: {cols} cols vs budget {max_cols}"
        );
        // push_quad = 6 vertices × 12 floats; vertex 0 of quad 0 is x0, and
        // the first grapheme is emitted at the run origin x.
        assert!(
            (verts[0] + cols as f32 * cw - ctx.right()).abs() < 0.5,
            "toast right edge must land on ctx.right: {} + {}×{} vs {}",
            verts[0],
            cols,
            cw,
            ctx.right()
        );
    }
}
