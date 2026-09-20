//! Block-visibility binary-search bounds for the shared layout pass
//! (`prefix_sum[j]*pitch + j*header_height + clear_ps[j]*clear_pitch`
//! height formula). Extracted from layout_pass.rs (M5-b, budget split).

/// Smallest j where `prefix_sum[j]*pitch + j*header_height +
/// clear_ps[j]*clear_pitch >= threshold`; `len` if none. `clear_pitch`
/// recovers the per-frame clear-block spacer excluded from the prefix sum.
pub(super) fn lower_bound_height(
    prefix_sum: &[usize],
    clear_ps: &[usize],
    pitch: f32,
    header_height: f32,
    clear_pitch: f32,
    threshold: f32,
) -> usize {
    let mut lo = 0usize;
    let mut hi = prefix_sum.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        let height = prefix_sum[mid] as f32 * pitch
            + mid as f32 * header_height
            + clear_ps[mid] as f32 * clear_pitch;
        if height < threshold {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

/// Smallest j where the height formula is `> threshold` (strict upper
/// bound); `len` if none.
pub(super) fn upper_bound_height(
    prefix_sum: &[usize],
    clear_ps: &[usize],
    pitch: f32,
    header_height: f32,
    clear_pitch: f32,
    threshold: f32,
) -> usize {
    let mut lo = 0usize;
    let mut hi = prefix_sum.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        let height = prefix_sum[mid] as f32 * pitch
            + mid as f32 * header_height
            + clear_ps[mid] as f32 * clear_pitch;
        if height <= threshold {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}
