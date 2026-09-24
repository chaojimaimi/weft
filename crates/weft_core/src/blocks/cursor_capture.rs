use std::time::Instant;

use super::BlockTracker;

impl BlockTracker {
    pub fn on_set_cursor_column(&mut self, column: usize) {
        if self.is_capturing() {
            // A-class (PLAN_v11217 §3.5): bounds the retained text → reads the
            // configured cap field, never a global constant.
            self.output.set_cursor_column(column, self.output_cap);
            // Mutates the captured bytes (pad spaces / wide-char overwrite),
            // so the LiveLayoutCache version must bump — version equality
            // ⇔ byte-identical output (live_cache.rs).
            self.live_output_version = self.live_output_version.wrapping_add(1);
            self.maybe_publish_live_styled(Instant::now());
        }
    }

    pub fn on_move_cursor_columns(&mut self, delta: isize) {
        if self.is_capturing() {
            // A-class (PLAN_v11217 §3.5): bounds the retained text → reads the
            // configured cap field, never a global constant.
            self.output.move_cursor_columns(delta, self.output_cap);
            // Same contract as above; no production caller today, but keep the
            // bump so a future call site can't corrupt the layout cache key.
            self.live_output_version = self.live_output_version.wrapping_add(1);
            self.maybe_publish_live_styled(Instant::now());
        }
    }
}
