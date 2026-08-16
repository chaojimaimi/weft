use super::{BlockTracker, MAX_OUTPUT_BYTES};

impl BlockTracker {
    pub fn on_set_cursor_column(&mut self, column: usize) {
        if self.is_capturing() {
            self.output.set_cursor_column(column, MAX_OUTPUT_BYTES);
            // Mutates the captured bytes (pad spaces / wide-char overwrite),
            // so the LiveLayoutCache version must bump — version equality
            // ⇔ byte-identical output (live_cache.rs).
            self.live_output_version = self.live_output_version.wrapping_add(1);
        }
    }

    pub fn on_move_cursor_columns(&mut self, delta: isize) {
        if self.is_capturing() {
            self.output.move_cursor_columns(delta, MAX_OUTPUT_BYTES);
            // Same contract as above; no production caller today, but keep the
            // bump so a future call site can't corrupt the layout cache key.
            self.live_output_version = self.live_output_version.wrapping_add(1);
        }
    }
}
