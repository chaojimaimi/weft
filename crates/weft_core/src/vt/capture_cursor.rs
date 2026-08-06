use super::Terminal;

/// `CSI 1G` is a progress rewrite; other absolute addresses remain evidence.
///
/// v1.10.4: relative cursor moves (`A`/`B`/`C`/`D` — CUU/CUD/CUF/CUB) are
/// also TUI evidence. Primary-screen TUIs like openclaw redraw their whole
/// viewport with relative moves + EL/ED (they never use absolute CUP),
/// whereas a plain shell command streams text without repeated cursor
/// addressing. Counting relative moves (reset by each OSC 133;A/B prompt
/// marker) lets such TUIs take the TUI-safe scroll path.
pub(super) fn is_primary_screen_addressing(action: char, first_param: u16) -> bool {
    matches!(action, 'H' | 'f' | 'd')
        || (action == 'G' && first_param > 1)
        || matches!(action, 'A' | 'B' | 'C' | 'D')
}

/// v1.10.4: whether a primary-screen addressing op positions the cursor
/// ABSOLUTELY (CUP `H`/`f`, VPA `d`). Full-viewport TUIs (Claude Code,
/// OpenCode) repaint with absolute addresses, so they need the live grid;
/// relative-only TUIs (openclaw) repaint in place and render correctly
/// in the BlockView. `show_block_view()` uses this to keep relative-only
/// TUIs in the BlockView instead of flipping to the live grid the first time
/// a keystroke crosses the TUI-detection threshold (the openclaw
/// "jump to top" symptom).
///
/// v1.10.5: CHR (`G`) is EXCLUDED — it positions only the cursor column
/// (horizontal), never rows, so it cannot be full-viewport repaint
/// evidence. pi (coding-agent CLI) redraws its input line per keystroke
/// with EL + `CSI <col>G`; counting that as absolute addressing flipped it
/// to the live grid on the first keystroke and made the command blocks
/// disappear. `G` still counts toward `primary_screen_cursor_ops` (TUI
/// evidence for the scroll path) — only the absolute classification dropped.
pub(super) fn is_absolute_primary_screen_addressing(action: char, _first_param: u16) -> bool {
    matches!(action, 'H' | 'f' | 'd')
}

impl Terminal {
    pub(super) fn capture_block_cursor_column(&mut self, column: usize) {
        if !self.capabilities.alt_active {
            self.block_tracker.on_set_cursor_column(column);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn horizontal_absolute_is_not_full_screen_evidence() {
        assert!(!is_primary_screen_addressing('G', 1));
        assert!(is_primary_screen_addressing('G', 6));
        assert!(is_primary_screen_addressing('H', 1));
    }

    #[test]
    fn relative_moves_are_primary_screen_evidence() {
        // v1.10.4: TUIs like openclaw redraw with relative moves (CUU/CUD/
        // CUB) + EL/ED, never absolute CUP. These must count as TUI
        // addressing so the TUI-safe scroll path engages.
        assert!(is_primary_screen_addressing('A', 3));
        assert!(is_primary_screen_addressing('B', 1));
        assert!(is_primary_screen_addressing('C', 6));
        assert!(is_primary_screen_addressing('D', 999));
        // Column-positioning G to column 1 is a progress rewrite, not TUI
        // evidence (unchanged from before).
        assert!(!is_primary_screen_addressing('G', 1));
    }

    #[test]
    fn absolute_vs_relative_addressing_classification() {
        // Absolute: CUP/VPA — the full-viewport repaint pattern.
        assert!(is_absolute_primary_screen_addressing('H', 1));
        assert!(is_absolute_primary_screen_addressing('H', 15));
        assert!(is_absolute_primary_screen_addressing('f', 3));
        assert!(is_absolute_primary_screen_addressing('d', 2));
        // Relative: CUU/CUD/CUF/CUB — in-place partial redraws (openclaw).
        assert!(!is_absolute_primary_screen_addressing('A', 915));
        assert!(!is_absolute_primary_screen_addressing('B', 1));
        assert!(!is_absolute_primary_screen_addressing('C', 2));
        assert!(!is_absolute_primary_screen_addressing('D', 999));
        // v1.10.5: CHR (G) is horizontal-only — never full-viewport
        // evidence. pi redraws its input line with EL + `CSI <col>G`.
        assert!(!is_absolute_primary_screen_addressing('G', 2));
        assert!(!is_absolute_primary_screen_addressing('G', 80));
        // G still counts as TUI addressing evidence for the scroll path.
        assert!(is_primary_screen_addressing('G', 2));
        assert!(!is_primary_screen_addressing('G', 1));
    }
}
