use super::{BlockTracker, MAX_OUTPUT_BYTES};

impl BlockTracker {
    pub fn on_set_cursor_column(&mut self, column: usize) {
        if self.is_capturing() {
            self.output.set_cursor_column(column, MAX_OUTPUT_BYTES);
        }
    }

    pub fn on_move_cursor_columns(&mut self, delta: isize) {
        if self.is_capturing() {
            self.output.move_cursor_columns(delta, MAX_OUTPUT_BYTES);
        }
    }
}
