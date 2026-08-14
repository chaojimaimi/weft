//! Tab drag-to-reorder pure geometry (ghost drag, v1.11.13+).
//!
//! During a drag the `SessionManager` Vec is left untouched. The dragged tab
//! is rendered as a "ghost" pill following the pointer; every other tab slides
//! to open a one-slot gap at `insert_index`. These functions map between the
//! data order and the on-screen slot order, and resolve the gap slot from the
//! pointer position (midpoint rule — the pointer crossing a tab's midpoint
//! moves the gap one slot, never a whole-tab jump).

use super::chrome::TabStripLayout;

/// Display slot for tab `i` during a drag of tab `drag_index` with the gap
/// at `insert_index` (0..=n-1, `n` = tab count). Slots 0..n-1 cover the
/// `n-1` non-dragged tabs plus the gap.
///
/// Returns `None` for the dragged tab itself (drawn separately as the ghost)
/// and defensively when `drag_index` is out of bounds.
pub fn render_slot(i: usize, drag_index: usize, insert_index: usize, n: usize) -> Option<usize> {
    if i == drag_index || drag_index >= n || i >= n {
        return None;
    }
    let s = if i < drag_index { i } else { i - 1 };
    Some(if s < insert_index { s } else { s + 1 })
}

/// Resolve the gap slot under the pointer during a drag. Slots are the
/// uniform tab-strip slots (`tabs_start + slot*tab_width - scroll_offset`);
/// the left half of a slot yields that slot, the right half yields the next
/// (clamped to the last slot). Deterministic and independent of the current
/// gap position, so a stationary pointer never oscillates the gap.
pub fn insertion_index_for_x(strip: &TabStripLayout, x: f32, n: usize) -> usize {
    if n <= 1 {
        return 0;
    }
    let rel = (x - (strip.tabs_start - strip.scroll_offset)) / strip.tab_width;
    let slot = rel.floor().clamp(0.0, (n - 1) as f32) as usize;
    if rel.fract() > 0.5 {
        (slot + 1).min(n - 1)
    } else {
        slot
    }
}

/// True when dropping at `insert_index` leaves the tab order unchanged
/// (the gap sits in the dragged tab's original slot).
pub fn is_noop(drag_index: usize, insert_index: usize) -> bool {
    drag_index == insert_index
}

#[cfg(test)]
mod tests {
    use super::{insertion_index_for_x, is_noop, render_slot};
    use crate::layout::{layout_tab_strip, TabStripInput};

    /// 3 tabs at max width (180px): tabs_start = 0 + traffic(72) + pad(8) = 80.
    fn strip(count: usize, scroll: f32) -> crate::layout::TabStripLayout {
        layout_tab_strip(TabStripInput {
            viewport_width: 1000.0,
            bar_height: 28.0,
            cell_width: 9.0,
            padding_x: 8.0,
            chrome_left: 0.0,
            traffic_lights_width: 72.0,
            tab_count: count,
            requested_scroll_offset: scroll,
        })
    }

    #[test]
    fn render_slot_rightward_drag() {
        // n=4, drag tab 1, gap at 2 → display [t0, t2, ▢, t3]
        assert_eq!(render_slot(0, 1, 2, 4), Some(0));
        assert_eq!(render_slot(1, 1, 2, 4), None);
        assert_eq!(render_slot(2, 1, 2, 4), Some(1));
        assert_eq!(render_slot(3, 1, 2, 4), Some(3));
    }

    #[test]
    fn render_slot_leftward_drag() {
        // n=4, drag tab 2, gap at 1 → display [t0, ▢, t1, t3]
        assert_eq!(render_slot(0, 2, 1, 4), Some(0));
        assert_eq!(render_slot(1, 2, 1, 4), Some(2));
        assert_eq!(render_slot(2, 2, 1, 4), None);
        assert_eq!(render_slot(3, 2, 1, 4), Some(3));
    }

    #[test]
    fn render_slot_gap_at_ends() {
        // Gap after the last non-dragged tab.
        assert_eq!(render_slot(1, 0, 2, 3), Some(0));
        assert_eq!(render_slot(2, 0, 2, 3), Some(1));
        // Gap at the very start.
        assert_eq!(render_slot(0, 2, 0, 3), Some(1));
        assert_eq!(render_slot(1, 2, 0, 3), Some(2));
    }

    #[test]
    fn render_slot_noop_position_keeps_order() {
        // Gap in the dragged tab's original slot → order unchanged.
        assert_eq!(render_slot(0, 1, 1, 3), Some(0));
        assert_eq!(render_slot(2, 1, 1, 3), Some(2));
    }

    #[test]
    fn render_slot_defensive_out_of_bounds() {
        assert_eq!(render_slot(0, 5, 1, 3), None);
        assert_eq!(render_slot(5, 2, 1, 3), None);
    }

    #[test]
    fn render_slot_single_tab() {
        assert_eq!(render_slot(0, 0, 0, 1), None);
    }

    #[test]
    fn insertion_left_half_of_slot_yields_slot() {
        let s = strip(3, 0.0);
        // tab_width = 180 (max), tabs_start = 80; slot 0 = [80, 260), mid = 170.
        assert_eq!(insertion_index_for_x(&s, 80.0, 3), 0);
        assert_eq!(insertion_index_for_x(&s, 169.0, 3), 0); // just before midpoint
        assert_eq!(insertion_index_for_x(&s, 170.0, 3), 0); // exactly midpoint → left half
    }

    #[test]
    fn insertion_right_half_of_slot_yields_next_slot() {
        let s = strip(3, 0.0);
        assert_eq!(insertion_index_for_x(&s, 171.0, 3), 1);
        assert_eq!(insertion_index_for_x(&s, 350.0, 3), 1); // midpoint of slot 1
        assert_eq!(insertion_index_for_x(&s, 351.0, 3), 2);
    }

    #[test]
    fn insertion_clamps_at_ends() {
        let s = strip(3, 0.0);
        assert_eq!(insertion_index_for_x(&s, -50.0, 3), 0);
        assert_eq!(insertion_index_for_x(&s, 0.0, 3), 0);
        // Past the last slot's right half → last slot.
        assert_eq!(insertion_index_for_x(&s, 1000.0, 3), 2);
    }

    #[test]
    fn insertion_stationary_pointer_is_stable() {
        // Pointer in the right half of slot 1 → gap slot 2. Re-evaluating at
        // the same x must not flip back (determinism, no oscillation).
        let s = strip(3, 0.0);
        let x = 351.0; // right half of slot 1
        assert_eq!(insertion_index_for_x(&s, x, 3), 2);
        assert_eq!(insertion_index_for_x(&s, x, 3), 2);
    }

    #[test]
    fn insertion_respects_scroll_offset() {
        // Overflow strip: scroll shifts slot origins left of tabs_start.
        let s = strip(30, 400.0);
        assert!(s.overflowing);
        let x = s.tabs_start - s.scroll_offset; // slot 0 origin
        assert_eq!(insertion_index_for_x(&s, x + 10.0, 30), 0);
        let mid_slot_2 = x + 2.5 * s.tab_width;
        assert_eq!(insertion_index_for_x(&s, mid_slot_2, 30), 2);
    }

    #[test]
    fn insertion_single_tab() {
        let s = strip(1, 0.0);
        assert_eq!(insertion_index_for_x(&s, 0.0, 1), 0);
        assert_eq!(insertion_index_for_x(&s, 500.0, 1), 0);
    }

    #[test]
    fn noop_matches_original_slot() {
        assert!(is_noop(0, 0));
        assert!(is_noop(2, 2));
        assert!(!is_noop(0, 1));
        assert!(!is_noop(2, 0));
    }
}
