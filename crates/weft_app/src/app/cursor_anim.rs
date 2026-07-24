//! Cursor blink + spinner animation extracted from `main.rs`.
//!
//! Two independent mechanisms share the `cursor_blink_time` anchor:
//! - **Grid view**: hard on/off toggle every 530ms (unchanged v0.7 logic).
//! - **Prompt (Editor mode)**: smooth `sin()` breath over a 2400ms period
//!   (v0.8 §0.3 signature). The phase advances continuously and wraps at
//!   2π; the renderer maps it to an alpha curve 0.25↔1.0 + amber glow.

impl crate::App {
    /// Update cursor blink state. See module docs for the two mechanisms.
    pub(crate) fn update_cursor_blink(&mut self) {
        // F6: When Reduce Motion is on, freeze the cursor visible (no blink
        // toggle, no breath phase). This mirrors the spinner behavior and
        // ensures a steady, non-distracting caret for motion-sensitive users.
        if self.window_runtime.reduce_motion {
            self.window_runtime.cursor_blink_on = true;
            self.window_runtime.cursor_blink_phase = 0.0;
            return;
        }

        let now = std::time::Instant::now();
        let elapsed = now.duration_since(self.window_runtime.cursor_blink_time);

        // Grid-view hard blink: toggle every 530ms (anchor reset on toggle).
        if elapsed >= std::time::Duration::from_millis(530) {
            self.window_runtime.cursor_blink_on = !self.window_runtime.cursor_blink_on;
            self.window_runtime.cursor_blink_time = now;
        }

        // Prompt signature breath: advance phase continuously.
        // Period 2400ms → one full sin() cycle; phase stored in radians.
        const PERIOD_MS: f64 = 2400.0;
        let elapsed_ms = elapsed.as_millis() as f64;
        // Each update advances phase by (elapsed_ms / PERIOD_MS) * 2π.
        let delta = (elapsed_ms / PERIOD_MS) * std::f64::consts::TAU;
        self.window_runtime.cursor_blink_phase += delta as f32;
        // Wrap into [0, 2π) to avoid float drift over long sessions.
        if self.window_runtime.cursor_blink_phase >= std::f32::consts::TAU {
            self.window_runtime.cursor_blink_phase -= std::f32::consts::TAU;
        }
    }

    /// F3-2: Advance the running-command spinner phase based on real elapsed
    /// time. The spinner cycles every 800ms (10 braille glyphs × 80ms each).
    /// When `reduce_motion` is on, the phase is frozen at 0 so the renderer
    /// draws a static `●` instead of animating.
    pub(crate) fn update_spinner(&mut self) {
        if self.window_runtime.reduce_motion {
            self.window_runtime.spinner_phase = 0.0;
            return;
        }
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(self.window_runtime.spinner_time);
        const SPINNER_PERIOD_MS: f64 = 800.0;
        let elapsed_ms = elapsed.as_millis() as f64;
        let delta = (elapsed_ms / SPINNER_PERIOD_MS) as f32;
        self.window_runtime.spinner_phase += delta;
        if self.window_runtime.spinner_phase >= 1.0 {
            self.window_runtime.spinner_phase -= 1.0;
        }
        self.window_runtime.spinner_time = now;
    }
}
