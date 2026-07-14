//! Precise trackpad scroll accumulation.
//!
//! macOS emits many small `PixelDelta` events for one gesture. Turning every
//! non-zero event into a terminal mouse-wheel notch makes Vim jump to an edge.
//! Accumulate physical pixels until one cell row has actually been crossed.

use winit::dpi::PhysicalPosition;
use winit::event::{MouseScrollDelta, TouchPhase};

#[derive(Debug, Default)]
pub(crate) struct PreciseScrollAccumulator {
    pixel_y: f64,
}

impl PreciseScrollAccumulator {
    pub(crate) fn residual_pixels(&self) -> f64 {
        self.pixel_y
    }

    fn reset(&mut self) {
        self.pixel_y = 0.0;
    }
}

/// Convert a terminal scroll event to signed cell rows.
///
/// Positive rows reveal older content (wheel-up); negative rows reveal newer
/// content (wheel-down). Line-based mouse wheels retain their existing
/// per-notch behavior. Precise trackpad pixels are accumulated across the
/// gesture and quantized by the current physical cell height.
pub(crate) fn terminal_scroll_rows(
    delta: MouseScrollDelta,
    phase: TouchPhase,
    cell_height: f64,
    accumulator: &mut PreciseScrollAccumulator,
) -> i32 {
    match delta {
        MouseScrollDelta::LineDelta(_, y) => {
            accumulator.reset();
            if y > 0.0 {
                y.ceil() as i32
            } else if y < 0.0 {
                y.floor() as i32
            } else {
                0
            }
        }
        MouseScrollDelta::PixelDelta(PhysicalPosition { y, .. }) => {
            if phase == TouchPhase::Cancelled {
                accumulator.reset();
                return 0;
            }
            if phase == TouchPhase::Started {
                accumulator.reset();
            }

            accumulator.pixel_y += y;
            let threshold = cell_height.max(1.0);
            let rows = (accumulator.pixel_y / threshold).trunc() as i32;
            accumulator.pixel_y -= rows as f64 * threshold;

            if phase == TouchPhase::Ended {
                accumulator.reset();
            }
            rows
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixels(y: f64) -> MouseScrollDelta {
        MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, y))
    }

    #[test]
    fn small_trackpad_deltas_wait_for_one_cell() {
        let mut acc = PreciseScrollAccumulator::default();
        assert_eq!(
            terminal_scroll_rows(pixels(3.0), TouchPhase::Started, 20.0, &mut acc),
            0
        );
        assert_eq!(
            terminal_scroll_rows(pixels(6.0), TouchPhase::Moved, 20.0, &mut acc),
            0
        );
        assert_eq!(
            terminal_scroll_rows(pixels(10.0), TouchPhase::Moved, 20.0, &mut acc),
            0
        );
        assert_eq!(acc.residual_pixels(), 19.0);
    }

    #[test]
    fn accumulated_trackpad_pixels_emit_rows_and_keep_remainder() {
        let mut acc = PreciseScrollAccumulator::default();
        assert_eq!(
            terminal_scroll_rows(pixels(45.0), TouchPhase::Started, 20.0, &mut acc),
            2
        );
        assert_eq!(acc.residual_pixels(), 5.0);
        assert_eq!(
            terminal_scroll_rows(pixels(15.0), TouchPhase::Moved, 20.0, &mut acc),
            1
        );
        assert_eq!(acc.residual_pixels(), 0.0);
    }

    #[test]
    fn negative_trackpad_pixels_emit_wheel_down_rows() {
        let mut acc = PreciseScrollAccumulator::default();
        assert_eq!(
            terminal_scroll_rows(pixels(-41.0), TouchPhase::Started, 20.0, &mut acc),
            -2
        );
        assert_eq!(acc.residual_pixels(), -1.0);
    }

    #[test]
    fn direction_reversal_cancels_uncommitted_pixels() {
        let mut acc = PreciseScrollAccumulator::default();
        assert_eq!(
            terminal_scroll_rows(pixels(12.0), TouchPhase::Started, 20.0, &mut acc),
            0
        );
        assert_eq!(
            terminal_scroll_rows(pixels(-8.0), TouchPhase::Moved, 20.0, &mut acc),
            0
        );
        assert_eq!(acc.residual_pixels(), 4.0);
    }

    #[test]
    fn ended_gesture_discards_sub_cell_remainder() {
        let mut acc = PreciseScrollAccumulator::default();
        assert_eq!(
            terminal_scroll_rows(pixels(7.0), TouchPhase::Started, 20.0, &mut acc),
            0
        );
        assert_eq!(
            terminal_scroll_rows(pixels(0.0), TouchPhase::Ended, 20.0, &mut acc),
            0
        );
        assert_eq!(acc.residual_pixels(), 0.0);
    }

    #[test]
    fn line_wheel_keeps_discrete_notch_count() {
        let mut acc = PreciseScrollAccumulator::default();
        assert_eq!(
            terminal_scroll_rows(
                MouseScrollDelta::LineDelta(0.0, 2.2),
                TouchPhase::Moved,
                20.0,
                &mut acc,
            ),
            3
        );
        assert_eq!(
            terminal_scroll_rows(
                MouseScrollDelta::LineDelta(0.0, -1.2),
                TouchPhase::Moved,
                20.0,
                &mut acc,
            ),
            -2
        );
    }
}
