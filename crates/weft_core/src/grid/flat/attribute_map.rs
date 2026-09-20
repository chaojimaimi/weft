//! Byte-offset interval map for grid attributes (ported from Warp
//! `attribute_map.rs`).
//!
//! Ranges are stored as `BTreeMap<end_offset, value>` with a `tail_value`
//! for everything past the last entry, so a run of cells sharing one
//! attribute value costs a single map entry (OSC 8 spans, solid-color
//! output). Lookups scan forward from an offset via `BTreeMap::range`.

use std::collections::{btree_map, BTreeMap};
use std::ops::RangeFrom;

use super::content::ByteOffset;

/// Stores and retrieves the value of one grid attribute keyed by byte offset.
///
/// The map coalesces: pushing a change only records where the value *starts*
/// differing from the current tail. Unchanged output costs nothing.
#[derive(Debug, Clone)]
pub(crate) struct AttributeMap<A> {
    /// Maps an _ending_ byte offset (inclusive) to the value in effect up to
    /// and including that offset.
    map: BTreeMap<ByteOffset, A>,
    /// The value for every offset beyond the last entry in `map`.
    tail_value: A,
}

impl<A> AttributeMap<A> {
    pub(crate) fn new(starting_value: A) -> Self {
        Self {
            map: Default::default(),
            tail_value: starting_value,
        }
    }

    /// Truncates the map tail to the given content offset.
    pub(crate) fn truncate(&mut self, new_len: ByteOffset) {
        // Split off any ranges that end past the new length; the first one
        // removed defined the value at `new_len`, so it becomes the tail.
        let mut truncated_ranges = self.map.split_off(&new_len);
        if let Some((_, tail_value)) = truncated_ranges.pop_first() {
            self.tail_value = tail_value;
        }
    }

    /// Drops every entry that ends before the given start offset. Called
    /// alongside `Content::truncate_front` when rows are evicted; surviving
    /// entries keep their (never re-based) offsets.
    pub(crate) fn truncate_front(&mut self, new_start_offset: ByteOffset) {
        self.map = self.map.split_off(&new_start_offset);
    }

    /// The end offset of the last range in the map.
    fn last_end_offset(&self) -> ByteOffset {
        self.map
            .last_key_value()
            .map(|(k, _)| *k)
            .unwrap_or_else(ByteOffset::zero)
    }
}

impl<A: PartialEq + std::fmt::Debug> AttributeMap<A> {
    /// Records that the attribute value changes at `range.start`.
    ///
    /// The start must be past the end of the last recorded range (writes are
    /// strictly append-only across the content stream).
    pub(crate) fn push_attribute_change(&mut self, range: RangeFrom<ByteOffset>, value: A) {
        if value == self.tail_value {
            return;
        }

        let prev_tail_value = std::mem::replace(&mut self.tail_value, value);

        if range.start == ByteOffset::zero() {
            debug_assert!(self.map.last_key_value().is_none());
        } else {
            debug_assert!(
                range.start > self.last_end_offset(),
                "cannot push attribute change starting at {} when last end offset is {}.  attribute map: {:?}",
                range.start,
                self.last_end_offset(),
                self.map,
            );
            // Close the previous range one byte before the new one starts.
            self.map.insert(range.start - 1, prev_tail_value);
        }
    }
}

impl<A: Copy> AttributeMap<A> {
    /// Per-byte attribute values starting at the given offset.
    pub(crate) fn iter_from(&self, start_offset: ByteOffset) -> impl Iterator<Item = A> + '_ {
        Iter::new(self, start_offset)
    }

    /// The current tail value of the attribute.
    pub(crate) fn tail(&self) -> A {
        self.tail_value
    }
}

/// Per-byte iterator over an [`AttributeMap`].
struct Iter<'a, A> {
    cur_offset: ByteOffset,
    cur_range: (ByteOffset, A),
    inner: btree_map::Range<'a, ByteOffset, A>,
    tail_value: A,
}

impl<'a, A: Copy> Iter<'a, A> {
    fn new(map: &'a AttributeMap<A>, start_offset: ByteOffset) -> Self {
        let mut inner = map.map.range(start_offset..);
        let cur_range = Self::next_range(&mut inner, map.tail_value);

        Self {
            cur_offset: start_offset,
            cur_range,
            inner,
            tail_value: map.tail_value,
        }
    }

    /// End point and value for the next range; an open range carrying the
    /// tail value once the map is exhausted.
    fn next_range(inner: &mut btree_map::Range<ByteOffset, A>, tail: A) -> (ByteOffset, A) {
        inner
            .next()
            .map(|(k, v)| (*k, *v))
            .unwrap_or((ByteOffset::from_usize(usize::MAX), tail))
    }
}

impl<A: Copy> Iterator for Iter<'_, A> {
    type Item = A;

    fn next(&mut self) -> Option<Self::Item> {
        self.nth(0)
    }

    fn nth(&mut self, n: usize) -> Option<Self::Item> {
        self.cur_offset += n;
        // Advance to the next range whenever the next byte is past the
        // current one's end.
        while self.cur_offset > self.cur_range.0 {
            self.cur_range = Self::next_range(&mut self.inner, self.tail_value);
        }

        let val = self.cur_range.1;
        self.cur_offset += 1;
        Some(val)
    }
}

#[cfg(test)]
mod tests {
    use crate::grid::cell::CellColor;

    use super::*;

    type TestAttributeMap = AttributeMap<usize>;

    #[test]
    fn iterate_over_empty_map() {
        // Values:
        // * [0, ): 0
        let map = TestAttributeMap::new(0);

        assert_eq!(map.iter_from(ByteOffset::zero()).next(), Some(0));
        assert_eq!(map.iter_from(ByteOffset::from_usize(25625)).next(), Some(0));
    }

    #[test]
    fn iterate_across_attribute_change() {
        // Values:
        // * [0, 2): 0
        // * [2, ): 1
        let mut map = TestAttributeMap::new(0);
        map.push_attribute_change(ByteOffset::from_usize(2).., 1);

        let iter = map.iter_from(ByteOffset::zero());

        let values: Vec<usize> = iter.take(4).collect();
        assert_eq!(values, vec![0, 0, 1, 1]);
    }

    #[test]
    fn truncate_front_keeps_offsets_and_tail_semantics() {
        // Values: [0, 2): 0   [2, 5): 1   [5, ): 2
        let mut map = TestAttributeMap::new(0);
        map.push_attribute_change(ByteOffset::from_usize(2).., 1);
        map.push_attribute_change(ByteOffset::from_usize(5).., 2);

        // Evict rows covering [0, 3): surviving bytes keep their offsets and
        // reads continue to return the value in effect there.
        map.truncate_front(ByteOffset::from_usize(3));

        let values: Vec<usize> = map.iter_from(ByteOffset::from_usize(3)).take(4).collect();
        assert_eq!(values, vec![1, 1, 2, 2]);
    }

    #[test]
    fn identical_values_cost_no_entries() {
        let mut map: AttributeMap<CellColor> = AttributeMap::new(CellColor::Default);
        for offset in [1usize, 4, 7] {
            map.push_attribute_change(ByteOffset::from_usize(offset).., CellColor::Default);
        }
        assert!(
            map.map.is_empty(),
            "no-op changes must not allocate entries"
        );
    }
}
