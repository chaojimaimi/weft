//! Pure-logic grapheme cluster assembly (v1.6.0).
//!
//! [`GraphemeAssembler`] buffers incoming scalars and identifies grapheme
//! cluster boundaries using Unicode TR29 rules (via `unicode-segmentation`).
//! When a new cluster starts, the previous cluster's string is returned so
//! the caller can store it in [`RowExtras`](crate::grid::RowExtras).
//!
//! The assembler is pure logic — it has no dependency on `Grid`, `Row`, or
//! the VT parser. This keeps the boundary detection testable in isolation
//! and lets the VT print path swap in different storage strategies.
//!
//! # Examples
//!
//! ```
//! use weft_core::grapheme::GraphemeAssembler;
//!
//! let mut asm = GraphemeAssembler::new();
//! // 'e' starts a cluster
//! assert_eq!(asm.feed('e'), None);
//! // combining acute extends the cluster
//! assert_eq!(asm.feed('\u{0301}'), None);
//! // 'a' starts a new cluster — previous cluster is returned in its
//! // original decomposed form ("e\u{0301}"), not the precomposed é (U+00E9).
//! assert_eq!(asm.feed('a'), Some("e\u{0301}".to_string()));
//! ```

use unicode_segmentation::UnicodeSegmentation;

/// Streaming grapheme cluster assembler.
///
/// Feed scalars one at a time via [`feed`](Self::feed). When a scalar starts
/// a new grapheme cluster, the previous cluster string is returned. Call
/// [`flush`](Self::flush) at the end of input (line break, cursor move, etc.)
/// to retrieve the final pending cluster.
#[derive(Debug, Default, Clone)]
pub struct GraphemeAssembler {
    buffer: String,
}

impl GraphemeAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a scalar.
    ///
    /// Returns:
    /// - `None` if `c` extends the current cluster (e.g. a combining mark
    ///   following a base char).
    /// - `Some(prev_cluster)` if `c` starts a new cluster. The returned
    ///   string is the complete previous cluster; the new cluster begins
    ///   with `c` in the assembler's buffer.
    pub fn feed(&mut self, c: char) -> Option<String> {
        if self.buffer.is_empty() {
            self.buffer.push(c);
            return None;
        }
        let prev_len = self.buffer.len();
        self.buffer.push(c);
        // Check if there's a grapheme boundary at `prev_len` (between the
        // previous content and the new char). If yes, the previous cluster
        // is complete and c starts a new one.
        let graphemes: Vec<&str> = self.buffer.graphemes(true).collect();
        if graphemes.len() >= 2 {
            // The previous cluster is graphemes[0]; the rest belongs to the
            // new cluster (normally just graphemes[1], but defensive concat
            // handles any edge case where c itself triggered a split).
            let prev_cluster = graphemes[0].to_string();
            self.buffer = graphemes[1..].concat();
            Some(prev_cluster)
        } else {
            // Still one cluster — c extended the current cluster.
            let _ = prev_len; // suppress unused warning in release builds
            None
        }
    }

    /// Flush the current pending cluster. Call this at end-of-input (line
    /// break, cursor move, ESC sequence) to retrieve the final cluster.
    pub fn flush(&mut self) -> Option<String> {
        if self.buffer.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.buffer))
        }
    }

    /// Reset the assembler, discarding any pending cluster.
    pub fn reset(&mut self) {
        self.buffer.clear();
    }

    /// The pending cluster string (the partial cluster in the buffer, not
    /// yet committed). Useful for diagnostics.
    pub fn pending(&self) -> &str {
        &self.buffer
    }

    /// True when no cluster is pending.
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }
}

/// Pure function: does `c` extend the grapheme cluster that ends with
/// `prev_str`?
///
/// `prev_str` is the full previous cluster string (from `RowExtras` if
/// present, or just the previous cell's `character`). Returns `true` when
/// there is NO grapheme boundary between `prev_str` and `c`.
///
/// This is the per-step check the VT print path uses when it doesn't want
/// to maintain a streaming [`GraphemeAssembler`] (e.g. when the cursor
/// position is known and the previous cluster is already stored).
pub fn extends_grapheme(prev_str: &str, c: char) -> bool {
    if prev_str.is_empty() {
        return false;
    }
    let mut combined = String::with_capacity(prev_str.len() + c.len_utf8());
    combined.push_str(prev_str);
    combined.push(c);
    let mut graphemes = combined.graphemes(true);
    let first = graphemes.next();
    // If there's only one grapheme, c extended the cluster.
    // If there are two, c started a new cluster.
    first.is_some() && graphemes.next().is_none()
}

/// Pure function: return the first scalar of a grapheme cluster string.
///
/// Used to populate `Cell.character` (which holds one `char`) when the full
/// cluster is stored in `RowExtras`. For a single-scalar cluster, returns
/// the scalar itself.
pub fn first_scalar(grapheme: &str) -> Option<char> {
    grapheme.chars().next()
}

/// Pure function: classify a scalar as a "combining" scalar that always
/// extends the previous cluster.
///
/// This is a fast-path check used before the more expensive
/// `extends_grapheme` call. Covers:
/// - Combining marks (Unicode general category Mn/Me) — detected by
///   `unicode-width` returning 0
/// - ZWJ (U+200D) and variation selectors (U+FE0E, U+FE0F)
/// - Emoji modifiers (U+1F3FB–U+1F3FF)
/// - Regional indicator second-of-pair (handled by `extends_grapheme`)
pub fn is_combining_scalar(c: char) -> bool {
    // ZWJ and variation selectors
    if c == '\u{200D}' || c == '\u{FE0E}' || c == '\u{FE0F}' {
        return true;
    }
    // Emoji modifiers (skin tone)
    if ('\u{1F3FB}'..='\u{1F3FF}').contains(&c) {
        return true;
    }
    // Combining marks have unicode width 0
    unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── GraphemeAssembler tests ───────────────────────────────────────

    #[test]
    fn single_scalar_feeds_none_then_flushes() {
        let mut asm = GraphemeAssembler::new();
        assert_eq!(asm.feed('a'), None);
        assert_eq!(asm.flush(), Some("a".to_string()));
        assert_eq!(asm.flush(), None);
    }

    #[test]
    fn combining_mark_extends_cluster() {
        let mut asm = GraphemeAssembler::new();
        assert_eq!(asm.feed('e'), None);
        assert_eq!(asm.feed('\u{0301}'), None);
        // Note: the assembler preserves the original scalar sequence
        // ("e\u{0301}" decomposed), NOT the precomposed form ("é" U+00E9).
        assert_eq!(asm.flush(), Some("e\u{0301}".to_string()));
    }

    #[test]
    fn new_base_char_returns_previous_cluster() {
        let mut asm = GraphemeAssembler::new();
        assert_eq!(asm.feed('e'), None);
        assert_eq!(asm.feed('\u{0301}'), None);
        // 'a' starts a new cluster → previous "e\u{0301}" is returned
        assert_eq!(asm.feed('a'), Some("e\u{0301}".to_string()));
        assert_eq!(asm.flush(), Some("a".to_string()));
    }

    #[test]
    fn zwj_emoji_sequence_is_one_cluster() {
        let mut asm = GraphemeAssembler::new();
        // 👩‍🔬 = woman + ZWJ + microscope
        assert_eq!(asm.feed('\u{1F469}'), None); // woman
        assert_eq!(asm.feed('\u{200D}'), None); // ZWJ
        assert_eq!(asm.feed('\u{1F52C}'), None); // microscope
        assert_eq!(asm.flush(), Some("👩\u{200d}🔬".to_string()));
    }

    #[test]
    fn variation_selector_extends_cluster() {
        let mut asm = GraphemeAssembler::new();
        // ⭐️ = star + variation selector-16 (emoji presentation)
        assert_eq!(asm.feed('\u{2B50}'), None); // star
        assert_eq!(asm.feed('\u{FE0F}'), None); // VS16
        assert_eq!(asm.flush(), Some("\u{2B50}\u{FE0F}".to_string()));
    }

    #[test]
    fn skin_tone_modifier_extends_cluster() {
        let mut asm = GraphemeAssembler::new();
        // 👍🏽 = thumbs up + skin tone modifier (medium)
        assert_eq!(asm.feed('\u{1F44D}'), None); // thumbs up
        assert_eq!(asm.feed('\u{1F3FD}'), None); // skin tone
        assert_eq!(asm.flush(), Some("\u{1F44d}\u{1f3fd}".to_string()));
    }

    #[test]
    fn regional_indicator_pair_is_one_cluster() {
        let mut asm = GraphemeAssembler::new();
        // 🇺🇸 = regional indicator U + regional indicator S
        assert_eq!(asm.feed('\u{1F1FA}'), None); // RI 'U'
        assert_eq!(asm.feed('\u{1F1F8}'), None); // RI 'S'
                                                 // Third char should return the flag pair
        assert_eq!(asm.feed('x'), Some("\u{1F1FA}\u{1F1F8}".to_string()));
        assert_eq!(asm.flush(), Some("x".to_string()));
    }

    #[test]
    fn multiple_combining_marks_accumulate() {
        let mut asm = GraphemeAssembler::new();
        // e + combining acute + combining grave (hypothetical stack)
        assert_eq!(asm.feed('e'), None);
        assert_eq!(asm.feed('\u{0301}'), None); // acute
        assert_eq!(asm.feed('\u{0300}'), None); // grave
        assert_eq!(asm.flush(), Some("e\u{0301}\u{0300}".to_string()));
    }

    #[test]
    fn reset_clears_buffer() {
        let mut asm = GraphemeAssembler::new();
        asm.feed('a');
        asm.reset();
        assert!(asm.is_empty());
        assert_eq!(asm.flush(), None);
    }

    #[test]
    fn pending_shows_current_buffer() {
        let mut asm = GraphemeAssembler::new();
        asm.feed('e');
        asm.feed('\u{0301}');
        assert_eq!(asm.pending(), "e\u{0301}");
    }

    // ── extends_grapheme tests ────────────────────────────────────────

    #[test]
    fn extends_grapheme_combining_mark() {
        assert!(extends_grapheme("e", '\u{0301}'));
        assert!(!extends_grapheme("e", 'a'));
    }

    #[test]
    fn extends_grapheme_zwj() {
        assert!(extends_grapheme("\u{1F469}", '\u{200D}'));
        assert!(extends_grapheme("\u{1F469}\u{200D}", '\u{1F52C}'));
        assert!(!extends_grapheme("\u{1F469}\u{200D}\u{1F52C}", 'a'));
    }

    #[test]
    fn extends_grapheme_variation_selector() {
        assert!(extends_grapheme("\u{2B50}", '\u{FE0F}'));
        assert!(!extends_grapheme("\u{2B50}\u{FE0F}", 'x'));
    }

    #[test]
    fn extends_grapheme_skin_tone() {
        assert!(extends_grapheme("\u{1F44D}", '\u{1F3FD}'));
        assert!(!extends_grapheme("\u{1F44D}\u{1F3FD}", 'x'));
    }

    #[test]
    fn extends_grapheme_regional_indicator_pair() {
        assert!(extends_grapheme("\u{1F1FA}", '\u{1F1F8}'));
        assert!(!extends_grapheme("\u{1F1FA}\u{1F1F8}", '\u{1F1FA}'));
    }

    #[test]
    fn extends_grapheme_empty_prev() {
        assert!(!extends_grapheme("", 'a'));
    }

    // ── first_scalar tests ────────────────────────────────────────────

    #[test]
    fn first_scalar_single() {
        assert_eq!(first_scalar("a"), Some('a'));
        assert_eq!(first_scalar("é"), Some('é')); // precomposed
    }

    #[test]
    fn first_scalar_multi() {
        assert_eq!(first_scalar("e\u{0301}"), Some('e'));
        assert_eq!(
            first_scalar("\u{1F469}\u{200D}\u{1F52C}"),
            Some('\u{1F469}')
        );
    }

    #[test]
    fn first_scalar_empty() {
        assert_eq!(first_scalar(""), None);
    }

    // ── is_combining_scalar tests ─────────────────────────────────────

    #[test]
    fn is_combining_scalar_true_for_combining_marks() {
        assert!(is_combining_scalar('\u{0300}')); // grave
        assert!(is_combining_scalar('\u{0301}')); // acute
        assert!(is_combining_scalar('\u{0302}')); // circumflex
    }

    #[test]
    fn is_combining_scalar_true_for_zwj_and_vs() {
        assert!(is_combining_scalar('\u{200D}')); // ZWJ
        assert!(is_combining_scalar('\u{FE0E}')); // VS15
        assert!(is_combining_scalar('\u{FE0F}')); // VS16
    }

    #[test]
    fn is_combining_scalar_true_for_skin_tone() {
        assert!(is_combining_scalar('\u{1F3FB}'));
        assert!(is_combining_scalar('\u{1F3FF}'));
    }

    #[test]
    fn is_combining_scalar_false_for_base_chars() {
        assert!(!is_combining_scalar('a'));
        assert!(!is_combining_scalar('中'));
        assert!(!is_combining_scalar('\u{1F44D}')); // thumbs up base
        assert!(!is_combining_scalar('\u{1F1FA}')); // regional indicator
    }

    #[test]
    fn is_combining_scalar_false_for_ascii() {
        for c in 'a'..='z' {
            assert!(!is_combining_scalar(c));
        }
        for c in 'A'..='Z' {
            assert!(!is_combining_scalar(c));
        }
        for c in '0'..='9' {
            assert!(!is_combining_scalar(c));
        }
    }

    // ── Boundary cases from the plan §3 ──────────────────────────────

    #[test]
    fn plan_e_plus_combining_acute() {
        let mut asm = GraphemeAssembler::new();
        asm.feed('e');
        asm.feed('\u{0301}');
        // Decomposed form (e + combining acute), not precomposed é (U+00E9).
        assert_eq!(asm.flush(), Some("e\u{0301}".to_string()));
    }

    #[test]
    fn plan_variation_selector() {
        let mut asm = GraphemeAssembler::new();
        asm.feed('\u{2B50}'); // star
        asm.feed('\u{FE0F}'); // VS16
        assert_eq!(asm.flush(), Some("\u{2B50}\u{FE0F}".to_string()));
    }

    #[test]
    fn plan_skin_tone_modifier() {
        let mut asm = GraphemeAssembler::new();
        asm.feed('\u{1F44D}'); // thumbs up
        asm.feed('\u{1F3FD}'); // medium skin tone
        assert_eq!(asm.flush(), Some("\u{1F44D}\u{1F3FD}".to_string()));
    }

    #[test]
    fn plan_regional_flag() {
        let mut asm = GraphemeAssembler::new();
        asm.feed('\u{1F1FA}'); // RI U
        asm.feed('\u{1F1F8}'); // RI S
        assert_eq!(asm.flush(), Some("\u{1F1FA}\u{1F1F8}".to_string()));
    }

    #[test]
    fn plan_zwj_family() {
        let mut asm = GraphemeAssembler::new();
        // 👨‍👩‍👧 = man + ZWJ + woman + ZWJ + girl
        asm.feed('\u{1F468}'); // man
        asm.feed('\u{200D}'); // ZWJ
        asm.feed('\u{1F469}'); // woman
        asm.feed('\u{200D}'); // ZWJ
        asm.feed('\u{1F467}'); // girl
        let cluster = asm.flush().unwrap();
        assert_eq!(cluster, "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}");
        // The whole ZWJ family is one grapheme cluster
        let graphemes: Vec<&str> = cluster.graphemes(true).collect();
        assert_eq!(graphemes.len(), 1);
    }

    #[test]
    fn plan_zwj_professional() {
        let mut asm = GraphemeAssembler::new();
        // 👩‍🔬 = woman + ZWJ + microscope
        asm.feed('\u{1F469}'); // woman
        asm.feed('\u{200D}'); // ZWJ
        asm.feed('\u{1F52C}'); // microscope
        let cluster = asm.flush().unwrap();
        assert_eq!(cluster, "\u{1F469}\u{200D}\u{1F52C}");
        let graphemes: Vec<&str> = cluster.graphemes(true).collect();
        assert_eq!(graphemes.len(), 1);
    }
}
