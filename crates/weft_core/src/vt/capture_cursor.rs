use super::Terminal;

/// `CSI 1G` is a progress rewrite; other absolute addresses remain evidence.
pub(super) fn is_primary_screen_addressing(action: char, first_param: u16) -> bool {
    matches!(action, 'H' | 'f' | 'd') || (action == 'G' && first_param > 1)
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
}
