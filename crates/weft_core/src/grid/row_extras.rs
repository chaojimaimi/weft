//! Sparse per-cell extension layer for multi-scalar graphemes (v1.6.0) and
//! OSC 8 hyperlink ids (v1.6.1).
//!
//! `Cell` is a fixed 24-byte struct that holds exactly one `char`. Multi-scalar
//! grapheme clusters (e + combining acute, ZWJ emoji sequences, regional flag
//! pairs, variation selectors) cannot fit in a single `char`, so the full
//! cluster string lives here in `RowExtras` — a sparse `BTreeMap<col, CellExtra>`
//! attached to each `Row`.
//!
//! v1.6.1 adds `hyperlink_id: Option<u32>` to the same `CellExtra` so OSC 8
//! links survive scroll/reflow/resize/persistence alongside graphemes. The
//! viewport-relative `HyperlinkRegistry` cell_map remains for the live
//! viewport fast path, but the authoritative storage is now `RowExtras` —
//! links in scrollback and captured Blocks resolve via `RowExtras::hyperlink_id_at`.
//!
//! Only cells with at least one of {grapheme, hyperlink_id} have an entry.
//! ASCII output and single-scalar cells without links pay zero memory
//! overhead — the fast path never touches `RowExtras`.
//!
//! Every `Row`/`Grid` operation that shifts, clears, or overwrites cells must
//! keep `RowExtras` in sync via the [`RowExtras`] transform methods on this
//! module. The `CellFlags::EXTRA` bit on `Cell` advertises the presence of
//! extra data so consumers (selection, copy, block capture, renderer) know to
//! look it up.

use std::collections::BTreeMap;
use std::sync::Arc;

/// Per-cell extension data. Stored sparsely in [`RowExtras`] — only cells
/// with at least one of {multi-scalar grapheme, hyperlink id} have an entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CellExtra {
    /// The full grapheme cluster string (e.g. `"é"` for `e\u{0301}`,
    /// `"👩‍🔬"` for `woman + ZWJ + microscope`).
    ///
    /// `None` means the cell has no multi-scalar extension (the cell's
    /// `character` field is the whole cluster).
    pub grapheme: Option<Arc<str>>,
    /// v1.6.1: OSC 8 hyperlink id. Resolved to a URL via [`HyperlinkRegistry`]
    /// (live viewport) or via a Block's link span table (captured output).
    /// `None` means the cell has no hyperlink.
    ///
    /// Stored here rather than in a viewport-relative side map so links
    /// survive scroll, reflow, resize, and persistence. The viewport
    /// registry's `cell_map` remains as a fast-path index for the live
    /// viewport; `RowExtras` is the source of truth.
    pub hyperlink_id: Option<u32>,
}

impl CellExtra {
    /// Create a `CellExtra` holding a grapheme cluster string.
    pub fn grapheme(grapheme: Arc<str>) -> Self {
        Self {
            grapheme: Some(grapheme),
            hyperlink_id: None,
        }
    }

    /// The grapheme string, if any.
    pub fn grapheme_str(&self) -> Option<&str> {
        self.grapheme.as_deref()
    }

    /// True when both `grapheme` and `hyperlink_id` are `None` — the entry
    /// should be removed from the map to keep it sparse. Called by the
    /// setter methods after they clear a field.
    fn is_empty(&self) -> bool {
        self.grapheme.is_none() && self.hyperlink_id.is_none()
    }
}

/// Sparse extension data for one [`Row`](super::Row).
///
/// Maps column index → [`CellExtra`]. Only columns with multi-scalar
/// graphemes appear; the common case (ASCII / single-scalar cells) is an
/// empty map with zero allocation.
///
/// # Invariants
///
/// - Entries are keyed by the column of the **lead cell** of a grapheme
///   cluster. For wide clusters (e.g. emoji), the `WIDE_SPACER` cell at
///   `col + 1` never has its own entry.
/// - Every column with an entry must have `CellFlags::EXTRA` set on the
///   corresponding `Cell` so consumers know to consult `RowExtras`.
/// - When a cell is overwritten, cleared, or shifted, its entry must be
///   removed or moved in the same operation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RowExtras {
    cells: BTreeMap<usize, CellExtra>,
}

impl RowExtras {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of cells with extra data (for diagnostics / tests).
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    /// True when no cells have extra data. The fast path — ASCII output,
    /// single-scalar cells — hits this on every row.
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// Look up the extra data for column `col`.
    pub fn get(&self, col: usize) -> Option<&CellExtra> {
        self.cells.get(&col)
    }

    /// The full grapheme string for column `col`, if the cell has a
    /// multi-scalar extension. Returns `None` for single-scalar cells
    /// (callers fall back to `cell.character`).
    pub fn grapheme_at(&self, col: usize) -> Option<&str> {
        self.cells.get(&col).and_then(|e| e.grapheme.as_deref())
    }

    /// v1.6.0 review M1: Clone the `Arc<str>` for column `col` instead of
    /// borrowing. Cheaper than `Arc::from(&str)` (refcount bump vs alloc +
    /// copy). Use on hot paths that need ownership (render instances).
    pub fn grapheme_arc_at(&self, col: usize) -> Option<Arc<str>> {
        self.cells.get(&col).and_then(|e| e.grapheme.clone())
    }

    /// v1.6.1: The hyperlink id for column `col`, if any. Resolved to a URL
    /// via [`HyperlinkRegistry`](crate::hyperlink::HyperlinkRegistry) (live
    /// viewport) or a Block's link span table (captured output).
    pub fn hyperlink_id_at(&self, col: usize) -> Option<u32> {
        self.cells.get(&col).and_then(|e| e.hyperlink_id)
    }

    /// Set or replace the extra data for column `col`.
    pub fn set(&mut self, col: usize, extra: CellExtra) {
        if extra.is_empty() {
            self.cells.remove(&col);
        } else {
            self.cells.insert(col, extra);
        }
    }

    /// Store a full grapheme cluster string for column `col`. Preserves
    /// `hyperlink_id` if the cell already has one (v1.6.1).
    pub fn set_grapheme(&mut self, col: usize, grapheme: Arc<str>) {
        let entry = self.cells.entry(col).or_default();
        entry.grapheme = Some(grapheme);
    }

    /// v1.6.1: Tag column `col` with hyperlink `id`. Preserves `grapheme` if
    /// the cell already has one (multi-scalar cluster + link coexist).
    /// Setting `id = None` clears the hyperlink while preserving grapheme.
    pub fn set_hyperlink(&mut self, col: usize, id: Option<u32>) {
        if let Some(id) = id {
            let entry = self.cells.entry(col).or_default();
            entry.hyperlink_id = Some(id);
        } else if let Some(entry) = self.cells.get_mut(&col) {
            entry.hyperlink_id = None;
            if entry.is_empty() {
                self.cells.remove(&col);
            }
        }
    }

    /// Append `scalar` to the grapheme at column `col`. If no entry exists
    /// yet, seed it with `base_char` (the cell's current `character`) plus
    /// the new scalar. This is the path the VT print path uses when a
    /// combining mark arrives.
    ///
    /// The `base_char` parameter is the current `Cell.character` — needed
    /// because the first scalar of the cluster was already written to the
    /// cell before the combining mark arrived.
    pub fn append_scalar(&mut self, col: usize, base_char: char, scalar: char) {
        let entry = self.cells.entry(col).or_insert_with(|| CellExtra {
            grapheme: Some(Arc::from(base_char.to_string().as_str())),
            hyperlink_id: None,
        });
        if entry.grapheme.is_none() {
            entry.grapheme = Some(Arc::from(base_char.to_string().as_str()));
        }
        if let Some(g) = entry.grapheme.take() {
            let mut s = g.to_string();
            s.push(scalar);
            entry.grapheme = Some(Arc::from(s.as_str()));
        }
    }

    /// Remove extra data for column `col` — called when a non-combining
    /// character overwrites a previously tagged cell.
    pub fn clear_cell(&mut self, col: usize) {
        self.cells.remove(&col);
    }

    /// Clear only the grapheme field for column `col`, preserving any
    /// hyperlink id. Removes the entry entirely if it becomes empty.
    /// Called when a non-combining scalar overwrites a cell that previously
    /// held a multi-scalar grapheme cluster (v1.6.0 review C1 fix).
    pub fn clear_grapheme(&mut self, col: usize) {
        if let Some(entry) = self.cells.get_mut(&col) {
            entry.grapheme = None;
            if entry.is_empty() {
                self.cells.remove(&col);
            }
        }
    }

    /// Clear grapheme fields for all columns in `[start, end)`. Preserves
    /// hyperlink ids. Used by the ASCII fast path to bulk-clean orphaned
    /// grapheme entries when overwriting a run of cells (v1.6.0 review C1).
    pub fn clear_grapheme_range(&mut self, start: usize, end: usize) {
        if self.cells.is_empty() || start >= end {
            return;
        }
        let keys: Vec<usize> = self.cells.range(start..end).map(|(k, _)| *k).collect();
        for k in keys {
            if let Some(entry) = self.cells.get_mut(&k) {
                entry.grapheme = None;
                if entry.is_empty() {
                    self.cells.remove(&k);
                }
            }
        }
    }

    /// v1.6.1: Clear only the hyperlink on column `col`, preserving the
    /// grapheme if present. Called when OSC 8 close is followed by more
    /// print at the same cell (rare — usually OSC 8 close moves the cursor).
    pub fn clear_hyperlink(&mut self, col: usize) {
        self.set_hyperlink(col, None);
    }

    /// Drop every entry — called when the whole row is cleared.
    pub fn clear(&mut self) {
        self.cells.clear();
    }

    /// v1.6.0 review M2: Remove all entries at columns `>= cols`. Used when
    /// the grid narrows so orphaned grapheme/hyperlink data doesn't accumulate.
    pub fn truncate_cols(&mut self, cols: usize) {
        if self.cells.is_empty() {
            return;
        }
        let keys: Vec<usize> = self.cells.range(cols..).map(|(k, _)| *k).collect();
        for k in keys {
            self.cells.remove(&k);
        }
    }

    /// Shift entries right by `n` starting at `col`, dropping entries that
    /// fall off the right edge. Used by `Grid::insert_blank` (CSI @).
    ///
    /// Entries at positions `< col` are untouched. Entries at
    /// `[col, cols - n)` move to `[col + n, cols)`. Entries at
    /// `[cols - n, cols)` are dropped.
    pub fn shift_right(&mut self, col: usize, n: usize, cols: usize) {
        if n == 0 || self.cells.is_empty() || col >= cols {
            return;
        }
        // Iterate descending so we can remove + reinsert without aliasing.
        let keys: Vec<usize> = self.cells.range(col..cols).map(|(k, _)| *k).collect();
        for &k in keys.iter().rev() {
            let new_col = k + n;
            let entry = self.cells.remove(&k).expect("key came from range");
            if new_col < cols {
                self.cells.insert(new_col, entry);
            }
            // else: fell off the right edge, drop it
        }
    }

    /// Shift entries left by `n` starting at `col`. Used by
    /// `Grid::delete_chars` (CSI P).
    ///
    /// Entries at positions `< col` are untouched. Entries at
    /// `[col + n, cols)` move to `[col, cols - n)`. Entries at
    /// `[col, col + n)` are dropped (overwritten by the shift).
    pub fn shift_left(&mut self, col: usize, n: usize, cols: usize) {
        if n == 0 || self.cells.is_empty() || col >= cols {
            return;
        }
        let upper = (col + n).min(cols);
        // Drop entries in [col, col+n) — they're overwritten.
        let drop_keys: Vec<usize> = self.cells.range(col..upper).map(|(k, _)| *k).collect();
        for k in drop_keys {
            self.cells.remove(&k);
        }
        // Move entries in [col+n, cols) left by n.
        let move_keys: Vec<usize> = self.cells.range(upper..cols).map(|(k, _)| *k).collect();
        for k in move_keys {
            let entry = self.cells.remove(&k).expect("key came from range");
            self.cells.insert(k - n, entry);
        }
    }

    /// Move entries from `src_cols..` to `dst_cols..`, used during reflow
    /// when a row is split. Entries in `[0, src_start)` are kept in this
    /// `RowExtras`; entries in `[src_start, cols)` are moved out and
    /// returned as a new `RowExtras` with positions shifted to start at 0.
    pub fn split_off(&mut self, src_start: usize, cols: usize) -> RowExtras {
        if self.cells.is_empty() || src_start >= cols {
            return RowExtras::new();
        }
        let mut tail = RowExtras::new();
        let keys: Vec<usize> = self.cells.range(src_start..cols).map(|(k, _)| *k).collect();
        for k in keys {
            let entry = self.cells.remove(&k).expect("key came from range");
            tail.cells.insert(k - src_start, entry);
        }
        tail
    }

    /// Merge another `RowExtras` into this one, shifting its entries right
    /// by `offset`. Used during reflow when wrapping content onto the next
    /// row. Entries that fall off the right edge (`>= cols`) are dropped.
    pub fn merge_shifted(&mut self, other: &RowExtras, offset: usize, cols: usize) {
        for (&k, v) in other.cells.iter() {
            let new_col = k + offset;
            if new_col < cols {
                self.cells.insert(new_col, v.clone());
            }
        }
    }

    /// Clone all entries — used when duplicating a row for scrollback push.
    pub fn clone_entries(&self) -> BTreeMap<usize, CellExtra> {
        self.cells.clone()
    }

    /// Iterate over `(col, &CellExtra)` pairs in column order.
    pub fn iter(&self) -> impl Iterator<Item = (usize, &CellExtra)> {
        self.cells.iter().map(|(k, v)| (*k, v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_by_default() {
        let e = RowExtras::new();
        assert!(e.is_empty());
        assert_eq!(e.len(), 0);
        assert_eq!(e.grapheme_at(0), None);
    }

    #[test]
    fn set_and_get() {
        let mut e = RowExtras::new();
        e.set_grapheme(3, Arc::from("é"));
        assert_eq!(e.len(), 1);
        assert_eq!(e.grapheme_at(3), Some("é"));
        assert_eq!(e.grapheme_at(4), None);
    }

    #[test]
    fn append_scalar_creates_entry_seeded_with_base() {
        let mut e = RowExtras::new();
        // Simulate 'e' already in the cell, then combining acute arrives.
        // The stored string is the decomposed form ("e\u{0301}"), not the
        // precomposed form ("é" U+00E9) — we preserve original scalars.
        e.append_scalar(2, 'e', '\u{0301}');
        assert_eq!(e.grapheme_at(2), Some("e\u{0301}"));
    }

    #[test]
    fn append_scalar_extends_existing_cluster() {
        let mut e = RowExtras::new();
        e.set_grapheme(0, Arc::from("e\u{0301}"));
        // Another combining mark arrives (e.g. double-grave hypothetical)
        e.append_scalar(0, 'e', '\u{0300}');
        assert_eq!(e.grapheme_at(0), Some("e\u{0301}\u{0300}"));
    }

    #[test]
    fn append_scalar_with_none_grapheme_seeds_base() {
        let mut e = RowExtras::new();
        e.set(1, CellExtra::default()); // grapheme is None
        e.append_scalar(1, 'a', '\u{0301}');
        // Decomposed form, not precomposed á (U+00E1).
        assert_eq!(e.grapheme_at(1), Some("a\u{0301}"));
    }

    #[test]
    fn clear_cell_removes_single_entry() {
        let mut e = RowExtras::new();
        e.set_grapheme(0, Arc::from("é"));
        e.set_grapheme(5, Arc::from("ç"));
        e.clear_cell(0);
        assert_eq!(e.len(), 1);
        assert_eq!(e.grapheme_at(0), None);
        assert_eq!(e.grapheme_at(5), Some("ç"));
    }

    #[test]
    fn clear_grapheme_preserves_hyperlink_and_removes_empty_entries() {
        let mut e = RowExtras::new();
        e.set_grapheme(1, Arc::from("e\u{0301}"));
        e.set_hyperlink(1, Some(7));
        e.set_grapheme(2, Arc::from("x\u{0301}"));

        e.clear_grapheme(1);
        assert_eq!(e.grapheme_at(1), None);
        assert_eq!(e.hyperlink_id_at(1), Some(7));

        e.clear_grapheme(2);
        assert_eq!(e.get(2), None, "empty sparse entries must be removed");
    }

    #[test]
    fn clear_grapheme_range_and_truncate_cols_keep_sparse_invariants() {
        let mut e = RowExtras::new();
        for col in 0..5 {
            e.set_grapheme(col, Arc::from(format!("x{col}")));
        }
        e.set_hyperlink(2, Some(9));

        e.clear_grapheme_range(1, 4);
        assert_eq!(e.grapheme_at(0), Some("x0"));
        assert_eq!(e.grapheme_at(1), None);
        assert_eq!(e.hyperlink_id_at(2), Some(9));
        assert_eq!(e.grapheme_at(4), Some("x4"));

        e.truncate_cols(3);
        assert!(e.iter().all(|(col, _)| col < 3));
        assert_eq!(e.hyperlink_id_at(2), Some(9));
    }

    #[test]
    fn clear_drops_all() {
        let mut e = RowExtras::new();
        e.set_grapheme(0, Arc::from("é"));
        e.set_grapheme(5, Arc::from("ç"));
        e.clear();
        assert!(e.is_empty());
    }

    #[test]
    fn shift_right_moves_entries_and_drops_overflow() {
        let mut e = RowExtras::new();
        e.set_grapheme(2, Arc::from("é"));
        e.set_grapheme(4, Arc::from("ç"));
        // 8-col row, shift right by 2 starting at col 2
        e.shift_right(2, 2, 8);
        assert_eq!(e.grapheme_at(2), None);
        assert_eq!(e.grapheme_at(4), Some("é"));
        assert_eq!(e.grapheme_at(6), Some("ç"));
    }

    #[test]
    fn shift_right_drops_entries_past_edge() {
        let mut e = RowExtras::new();
        e.set_grapheme(6, Arc::from("ç")); // would move to 8, but cols=8
        e.shift_right(4, 2, 8);
        assert!(e.is_empty(), "entry at 6+2=8 is dropped (>= cols)");
    }

    #[test]
    fn shift_right_preserves_entries_before_col() {
        let mut e = RowExtras::new();
        e.set_grapheme(0, Arc::from("a"));
        e.set_grapheme(3, Arc::from("b"));
        e.shift_right(2, 1, 8);
        assert_eq!(e.grapheme_at(0), Some("a"), "entry before col is untouched");
        assert_eq!(e.grapheme_at(3), None);
        assert_eq!(e.grapheme_at(4), Some("b"));
    }

    #[test]
    fn shift_right_zero_n_is_noop() {
        let mut e = RowExtras::new();
        e.set_grapheme(2, Arc::from("é"));
        e.shift_right(2, 0, 8);
        assert_eq!(e.grapheme_at(2), Some("é"));
    }

    #[test]
    fn shift_left_moves_entries_and_drops_overwritten() {
        let mut e = RowExtras::new();
        e.set_grapheme(1, Arc::from("dropped"));
        e.set_grapheme(3, Arc::from("é"));
        e.set_grapheme(5, Arc::from("ç"));
        // 8-col row, shift left by 2 starting at col 1
        e.shift_left(1, 2, 8);
        assert_eq!(e.grapheme_at(1), Some("é")); // was at 3, moved to 1
        assert_eq!(e.grapheme_at(3), Some("ç")); // was at 5, moved to 3
        assert_eq!(e.len(), 2, "entry at col 1 (in [1,3)) is dropped");
    }

    #[test]
    fn shift_left_preserves_entries_before_col() {
        let mut e = RowExtras::new();
        e.set_grapheme(0, Arc::from("a"));
        e.set_grapheme(3, Arc::from("b"));
        e.shift_left(2, 1, 8);
        assert_eq!(e.grapheme_at(0), Some("a"));
        assert_eq!(e.grapheme_at(2), Some("b"));
    }

    #[test]
    fn split_off_moves_tail_to_new_extras() {
        let mut e = RowExtras::new();
        e.set_grapheme(0, Arc::from("a"));
        e.set_grapheme(3, Arc::from("b"));
        e.set_grapheme(5, Arc::from("c"));
        let tail = e.split_off(3, 8);
        assert_eq!(e.grapheme_at(0), Some("a"));
        assert_eq!(e.len(), 1);
        assert_eq!(tail.grapheme_at(0), Some("b")); // was at 3, now at 0
        assert_eq!(tail.grapheme_at(2), Some("c")); // was at 5, now at 2
    }

    #[test]
    fn merge_shifted_appends_other_with_offset() {
        let mut e = RowExtras::new();
        e.set_grapheme(0, Arc::from("a"));
        let mut other = RowExtras::new();
        other.set_grapheme(0, Arc::from("b"));
        other.set_grapheme(1, Arc::from("c"));
        e.merge_shifted(&other, 5, 8);
        assert_eq!(e.grapheme_at(0), Some("a"));
        assert_eq!(e.grapheme_at(5), Some("b"));
        assert_eq!(e.grapheme_at(6), Some("c"));
    }

    #[test]
    fn merge_shifted_drops_overflow() {
        let mut e = RowExtras::new();
        let mut other = RowExtras::new();
        other.set_grapheme(0, Arc::from("b"));
        e.merge_shifted(&other, 7, 8); // would go to col 7, ok
        assert_eq!(e.grapheme_at(7), Some("b"));
        // Now an entry that would overflow
        let mut other2 = RowExtras::new();
        other2.set_grapheme(1, Arc::from("c"));
        e.merge_shifted(&other2, 7, 8); // would go to col 8, dropped
        assert_eq!(e.grapheme_at(8), None);
    }

    #[test]
    fn iter_returns_column_order() {
        let mut e = RowExtras::new();
        e.set_grapheme(5, Arc::from("c"));
        e.set_grapheme(0, Arc::from("a"));
        e.set_grapheme(3, Arc::from("b"));
        let cols: Vec<usize> = e.iter().map(|(c, _)| c).collect();
        assert_eq!(cols, vec![0, 3, 5]);
    }

    #[test]
    fn clone_entries_roundtrip() {
        let mut e = RowExtras::new();
        e.set_grapheme(0, Arc::from("a"));
        e.set_grapheme(3, Arc::from("b"));
        let cloned = e.clone_entries();
        assert_eq!(cloned.len(), 2);
        assert_eq!(cloned.get(&0).unwrap().grapheme_str(), Some("a"));
        assert_eq!(cloned.get(&3).unwrap().grapheme_str(), Some("b"));
    }
}
