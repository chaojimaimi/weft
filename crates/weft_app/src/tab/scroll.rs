use super::Tab;

impl Tab {
    pub fn block_scroll(&self) -> usize {
        self.block_scroll_offset
    }

    pub fn enter_primary_history_if_active(&mut self) -> bool {
        let active = self
            .terminal
            .as_ref()
            .is_some_and(weft_core::vt::Terminal::primary_screen_app_active);
        if active {
            self.terminal
                .as_mut()
                .expect("active primary screen has a terminal")
                .set_primary_history_view(true);
        }
        active
    }

    pub fn set_block_scroll(&mut self, offset: usize) {
        self.block_scroll_offset = offset;
        self.sync_primary_history_view();
    }

    pub fn snap_to_bottom(&mut self) {
        self.set_block_scroll(0);
    }

    pub fn scroll_up_by(&mut self, rows: usize) {
        self.set_block_scroll(self.block_scroll_offset.saturating_add(rows));
    }

    pub fn scroll_down_by(&mut self, rows: usize) {
        self.set_block_scroll(self.block_scroll_offset.saturating_sub(rows));
    }

    pub fn clamp_block_scroll(&mut self, max_scroll: usize) {
        self.set_block_scroll(self.block_scroll_offset.min(max_scroll));
    }

    fn sync_primary_history_view(&mut self) {
        if let Some(terminal) = &mut self.terminal {
            let browsing = self.block_scroll_offset > 0 && terminal.primary_screen_app_active();
            terminal.set_primary_history_view(browsing);
        }
    }
}
