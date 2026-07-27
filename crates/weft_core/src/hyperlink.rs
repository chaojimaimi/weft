//! OSC 8 hyperlink registry (v0.8 stage 5 — B2; v1.6.1 — size limits + RowExtras).
//!
//! Cells carry only a 1-bit `CellFlags::HYPERLINK` flag (the `Cell` struct
//! must stay at 24 bytes), so the URL→cell association is held externally in
//! a side table here. The registry deduplicates URLs by string (two cells
//! pointing at the same URL share one id), and the `(row, col) → id` map is
//! rebuilt every time the active OSC 8 hyperlink changes.
//!
//! v1.6.1: The viewport-relative `cell_map` is now a fast-path index only;
//! the authoritative hyperlink storage is `RowExtras.hyperlink_id` which
//! survives scroll/reflow/resize/persistence. This registry still owns the
//! URL→id dedup table and enforces size limits (max URL length, max total
//! entries) to prevent unbounded growth from malicious or buggy TUI apps.
//!
//! Limitations:
//!   • No per-link color cycling (kitty's `id` parameter) — every link
//!     renders with the same underline.
//!   • URL→id table is bounded; when full, new URLs are silently rejected
//!     (the cell keeps the HYPERLINK flag for underline styling but is not
//!     clickable). This matches xterm/Alacritty behavior.

use std::collections::HashMap;

/// v1.6.1: Maximum URL length accepted by [`HyperlinkRegistry::register`].
/// URLs longer than this are rejected (return id 0 = "no link"). Protects
/// against memory abuse from pathological OSC 8 sequences. 8 KiB matches
/// xterm's hard limit and covers all reasonable URLs.
pub const MAX_HYPERLINK_URL_LEN: usize = 8 * 1024;

/// v1.6.1: Maximum number of distinct URLs the registry will track. When
/// full, new URLs are rejected. 4096 is generous for terminal use (a typical
/// session sees <100 distinct links) while bounding memory to ~32 MiB worst
/// case (4096 × 8 KiB). Old URLs are not evicted — the registry is cleared
/// on alt-screen exit and session reset, which naturally bounds lifetime.
pub const MAX_HYPERLINK_ENTRIES: usize = 4096;

/// External hyperlink registry: maps `(row, col) → id → URL`.
#[derive(Default)]
pub struct HyperlinkRegistry {
    /// `id → URL`. id 0 is reserved (treated as "no link" by callers).
    urls: HashMap<u32, String>,
    /// Reverse lookup for URL dedup: `URL → id`.
    url_to_id: HashMap<String, u32>,
    /// Next id to assign when a new URL is registered.
    next_id: u32,
    /// `(row, col) → id` for cells currently tagged with HYPERLINK.
    /// Maintained by `print()` in vt.rs as cells are written.
    cell_map: HashMap<(usize, usize), u32>,
}

impl HyperlinkRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a URL (deduped by string). Returns the assigned id.
    /// Two calls with the same URL return the same id.
    ///
    /// v1.6.1: Returns 0 (no link) when the URL exceeds
    /// [`MAX_HYPERLINK_URL_LEN`] or when the registry is full
    /// ([`MAX_HYPERLINK_ENTRIES`]). This prevents unbounded memory growth
    /// from malicious or buggy TUI apps emitting pathological OSC 8 sequences.
    pub fn register(&mut self, url: String) -> u32 {
        if url.len() > MAX_HYPERLINK_URL_LEN {
            tracing::warn!(
                len = url.len(),
                max = MAX_HYPERLINK_URL_LEN,
                "OSC 8 URL rejected: exceeds max length"
            );
            return 0;
        }
        if let Some(&id) = self.url_to_id.get(&url) {
            return id;
        }
        if self.urls.len() >= MAX_HYPERLINK_ENTRIES {
            tracing::warn!(
                count = self.urls.len(),
                max = MAX_HYPERLINK_ENTRIES,
                "OSC 8 URL rejected: registry full"
            );
            return 0;
        }
        // Skip 0 (reserved) and start at 1.
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let id = self.next_id;
        self.urls.insert(id, url.clone());
        self.url_to_id.insert(url, id);
        id
    }

    /// Look up the URL registered under `id`. Returns `None` for unknown ids.
    pub fn url(&self, id: u32) -> Option<&str> {
        self.urls.get(&id).map(String::as_str)
    }

    /// Tag cell `(row, col)` with hyperlink `id`. The cell's HYPERLINK flag
    /// is set separately by the vt parser when writing the cell.
    pub fn link_cell(&mut self, row: usize, col: usize, id: u32) {
        self.cell_map.insert((row, col), id);
    }

    /// Remove the hyperlink tag from cell `(row, col)` — called when a
    /// non-hyperlink character overwrites a previously tagged cell.
    pub fn unlink_cell(&mut self, row: usize, col: usize) {
        self.cell_map.remove(&(row, col));
    }

    /// Remove every viewport-relative hyperlink mapping on one row.
    pub fn unlink_row(&mut self, row: usize) {
        self.cell_map
            .retain(|&(mapped_row, _), _| mapped_row != row);
    }

    /// Resolve the URL for cell `(row, col)`, if any.
    pub fn url_at(&self, row: usize, col: usize) -> Option<&str> {
        let id = self.cell_map.get(&(row, col))?;
        self.urls.get(id).map(String::as_str)
    }

    /// Drop all `(row, col)` mappings. Called when the viewport scrolls,
    /// resizes, or is cleared — the URL→id table is preserved so future
    /// prints of the same URL reuse the existing id (cheap dedup).
    pub fn clear_cell_map(&mut self) {
        self.cell_map.clear();
    }

    /// v1.0 P1.5-C2: Returns true if no cells are currently tagged with
    /// hyperlinks. Used by the ASCII fast path to skip per-cell unlink
    /// checks entirely (the common case — terminal output rarely has
    /// OSC 8 hyperlinks).
    pub fn cell_map_is_empty(&self) -> bool {
        self.cell_map.is_empty()
    }

    /// Number of currently tagged cells (for diagnostics / tests).
    pub fn tagged_cell_count(&self) -> usize {
        self.cell_map.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_dedups_url_strings() {
        let mut r = HyperlinkRegistry::new();
        let a = r.register("https://weft.dev".into());
        let b = r.register("https://weft.dev".into());
        assert_eq!(a, b, "same URL → same id");
        let c = r.register("https://other.dev".into());
        assert_ne!(a, c, "different URL → different id");
    }

    #[test]
    fn url_at_round_trip() {
        let mut r = HyperlinkRegistry::new();
        let id = r.register("https://weft.dev/x".into());
        r.link_cell(2, 5, id);
        assert_eq!(r.url_at(2, 5), Some("https://weft.dev/x"));
        assert_eq!(r.url_at(2, 6), None);
        assert_eq!(r.url_at(3, 5), None);
    }

    #[test]
    fn unlink_cell_removes_mapping() {
        let mut r = HyperlinkRegistry::new();
        let id = r.register("u".into());
        r.link_cell(0, 0, id);
        assert_eq!(r.tagged_cell_count(), 1);
        r.unlink_cell(0, 0);
        assert_eq!(r.tagged_cell_count(), 0);
        assert_eq!(r.url_at(0, 0), None);
    }

    #[test]
    fn unlink_row_preserves_links_on_other_rows() {
        let mut r = HyperlinkRegistry::new();
        let id = r.register("u".into());
        r.link_cell(1, 0, id);
        r.link_cell(1, 1, id);
        r.link_cell(2, 0, id);

        r.unlink_row(1);

        assert_eq!(r.url_at(1, 0), None);
        assert_eq!(r.url_at(1, 1), None);
        assert_eq!(r.url_at(2, 0), Some("u"));
    }

    #[test]
    fn clear_cell_map_preserves_url_table() {
        let mut r = HyperlinkRegistry::new();
        let id = r.register("u".into());
        r.link_cell(0, 0, id);
        r.clear_cell_map();
        assert_eq!(r.tagged_cell_count(), 0);
        // URL persists — registering again returns the same id.
        let id2 = r.register("u".into());
        assert_eq!(id, id2);
    }
}
