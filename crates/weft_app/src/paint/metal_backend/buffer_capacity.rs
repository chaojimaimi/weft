use std::cell::Cell;

use metal::{Buffer, DeviceRef, MTLResourceOptions};

const ALIGNMENT: u64 = 4096;
const MIN_CAPACITY: u64 = 64 * 1024;
const SHRINK_AFTER_LOW_FRAMES: u16 = 120;

pub(crate) struct LowUsageCounter(Cell<u16>);

impl LowUsageCounter {
    pub(crate) const fn new() -> Self {
        Self(Cell::new(0))
    }
}

fn capacity_for(bytes: u64) -> u64 {
    let grown = bytes.saturating_add(bytes / 2).max(MIN_CAPACITY);
    grown
        .checked_add(ALIGNMENT - 1)
        .map_or(u64::MAX, |aligned| aligned / ALIGNMENT * ALIGNMENT)
}

fn capacity_update(current: u64, bytes: u64, low_frames: u16) -> (Option<u64>, u16) {
    if current == 0 {
        return ((bytes > 0).then(|| capacity_for(bytes)), 0);
    }
    if bytes > current {
        return (Some(capacity_for(bytes)), 0);
    }
    if current > MIN_CAPACITY && bytes <= current / 4 {
        let next = low_frames.saturating_add(1);
        if next >= SHRINK_AFTER_LOW_FRAMES {
            let target = capacity_for(bytes);
            return ((target < current).then_some(target), 0);
        }
        return (None, next);
    }
    (None, 0)
}

pub(super) fn resize_ring_for_usage(
    device: &DeviceRef,
    ring: &mut Vec<Buffer>,
    capacity: &Cell<u64>,
    low_frames: &LowUsageCounter,
    bytes: u64,
) {
    let current = capacity.get();
    let (target, next_low_frames) = capacity_update(current, bytes, low_frames.0.get());
    low_frames.0.set(next_low_frames);
    if let Some(target) = target {
        if ring.is_empty() {
            for _ in 0..3 {
                ring.push(device.new_buffer(target, MTLResourceOptions::CPUCacheModeWriteCombined));
            }
        } else {
            for buffer in ring.iter_mut() {
                *buffer = device.new_buffer(target, MTLResourceOptions::CPUCacheModeWriteCombined);
            }
        }
        capacity.set(target);
    }
    while !ring.is_empty() && ring.len() < 3 {
        ring.push(device.new_buffer(
            capacity.get().max(MIN_CAPACITY),
            MTLResourceOptions::CPUCacheModeWriteCombined,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_grows_with_headroom_and_alignment() {
        let (target, low) = capacity_update(0, 100_000, 0);
        let target = target.unwrap();
        assert!(target >= 150_000);
        assert_eq!(target % ALIGNMENT, 0);
        assert_eq!(low, 0);
    }

    #[test]
    fn capacity_alignment_saturates_for_theoretical_maximum() {
        let target = capacity_for(u64::MAX);
        assert_eq!(target, u64::MAX);
    }

    #[test]
    fn transient_low_usage_does_not_shrink_ring() {
        let current = 192 * 1024 * 1024;
        let (target, low) = capacity_update(current, 2 * 1024 * 1024, 10);
        assert_eq!(target, None);
        assert_eq!(low, 11);
    }

    #[test]
    fn sustained_low_usage_releases_peak_capacity() {
        let current = 192 * 1024 * 1024;
        let (target, low) = capacity_update(current, 2 * 1024 * 1024, SHRINK_AFTER_LOW_FRAMES - 1);
        let target = target.unwrap();
        assert!(target < current / 4);
        assert!(target >= 3 * 1024 * 1024);
        assert_eq!(low, 0);
    }

    #[test]
    fn normal_usage_resets_low_water_streak() {
        let current = 8 * 1024 * 1024;
        let (target, low) = capacity_update(current, 3 * 1024 * 1024, 80);
        assert_eq!(target, None);
        assert_eq!(low, 0);
    }

    #[test]
    fn metal_ring_releases_peak_buffers_after_low_water_streak() {
        let Some(device) = metal::Device::system_default() else {
            return;
        };
        let mut ring = Vec::new();
        let capacity = Cell::new(0);
        let low = LowUsageCounter::new();
        resize_ring_for_usage(&device, &mut ring, &capacity, &low, 4 * 1024 * 1024);
        assert_eq!(ring.len(), 3);
        assert!(capacity.get() >= 6 * 1024 * 1024);

        for _ in 0..SHRINK_AFTER_LOW_FRAMES {
            resize_ring_for_usage(&device, &mut ring, &capacity, &low, 0);
        }
        assert_eq!(capacity.get(), MIN_CAPACITY);
        assert!(ring.iter().all(|buffer| buffer.length() == MIN_CAPACITY));
    }
}
