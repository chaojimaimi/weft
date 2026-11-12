//! v1.20.3 WP4 regression probes: the tail-inserted script fallback pool and
//! the `probed_missing` memo. Same organization as `tests.rs` — device-free
//! where possible, Metal-gated (`Device::system_default()`) for real
//! rasterization end-to-end probes, environment-adaptive skip messages so a
//! machine without a covering fallback font still passes.
#![cfg(test)]

use super::*;

/// v1.20.3 WP4 (回归案①): a genuinely blind char (Ainu kana ㇰ U+31F0 —
/// outside every ladder family's coverage on stock macOS; the v1.13.15
/// range gate even refuses to load SDG for it) must resolve through the
/// script fallback pool instead of locking onto the blind primary's all-zero
/// bitmap. Facts pinned rather than assumed: the probe only asserts pool
/// behavior when (a) the char really is blind on THIS machine and (b) some
/// pool font really covers it — otherwise it skips with an explanation, so
/// the test stays green on exotic machines while still failing on a real
/// regression (pool bypassed → `probed_missing` would swallow the char).
#[test]
fn blind_char_resolves_through_script_fallback_pool() {
    let Some(device) = metal::Device::system_default() else {
        eprintln!("skipping: no Metal device");
        return;
    };
    let mut atlas =
        crate::glyph::GlyphAtlas::new(&device, &weft_core::config::FontConfig::default(), 2.0);
    let ch = '\u{31F0}'; // ㇰ AINU LETTER KU

    // Fact: every narrow-ladder rung lacks the char on this machine.
    assert!(
        atlas.primary_font.glyph_for_char(ch).is_none(),
        "fact precondition: primary lacks U+31F0"
    );
    assert!(
        atlas
            .symbol_font
            .as_ref()
            .map_or(true, |f| f.glyph_for_char(ch).is_none()),
        "fact precondition: Apple Symbols lacks U+31F0"
    );
    assert!(
        atlas
            .menlo_font
            .as_ref()
            .map_or(true, |f| f.glyph_for_char(ch).is_none()),
        "fact precondition: Menlo lacks U+31F0"
    );

    // Environment-adaptive gate: only meaningful if some pool font covers it.
    atlas.get_or_rasterize(ch, GlyphStyle::REGULAR);
    let pool_font = atlas
        .script_fallback_pool
        .as_ref()
        .and_then(|pool| pool.iter().find(|f| f.glyph_for_char(ch).is_some()));
    let Some(pool_font) = pool_font else {
        eprintln!("skipping: no system font covers U+31F0 on this machine");
        return;
    };

    // The pool carried the char: it must NOT be memoized as missing, and a
    // repeat query must resolve from the cache (same Some glyph, no rescan).
    assert!(
        !atlas.probed_missing.contains(&ch),
        "盲区命中: U+31F0 由 fallback 池字体 {} 覆盖,不得记入 probed_missing(否则=退回 blind primary 全零位图)",
        pool_font.family_name()
    );
    let scans_after_hit = atlas.fallback_pool_scans.get();
    let again = atlas
        .get_or_rasterize(ch, GlyphStyle::REGULAR)
        .expect("pool-hit char must rasterize");
    let _ = again;
    assert_eq!(
        atlas.fallback_pool_scans.get(),
        scans_after_hit,
        "cache 快径: 池命中的字符二次查询不得重扫 fallback 池"
    );
}

/// v1.20.3 WP4 (回归案②): a char NO system font covers (U+0378 — unassigned
/// Unicode codepoint, guaranteed chain-wide miss) must be memoized into
/// `probed_missing` after the first full scan, and a second query must NOT
/// re-scan the pool (the memo short-circuits). The public behavior stays
/// exactly the pre-WP4 one: `get_or_rasterize` still returns `Some` (the
/// blind primary's blank glyph — the golden-zero-drift invariant).
#[test]
fn probed_missing_memoizes_chain_wide_miss_and_skips_rescan() {
    let Some(device) = metal::Device::system_default() else {
        eprintln!("skipping: no Metal device");
        return;
    };
    let mut atlas =
        crate::glyph::GlyphAtlas::new(&device, &weft_core::config::FontConfig::default(), 2.0);
    let ch = '\u{0378}'; // unassigned — no font anywhere can carry it

    let first = atlas
        .get_or_rasterize(ch, GlyphStyle::REGULAR)
        .expect("blind path keeps returning Some (blank primary glyph)");
    let _ = first;
    assert!(
        atlas.probed_missing.contains(&ch),
        "全链 miss 必须记入 probed_missing(字符级 miss 备忘)"
    );
    let scans_after_first = atlas.fallback_pool_scans.get();
    assert!(
        scans_after_first >= 1,
        "首次 miss 必须真的扫过一次 fallback 池(计数器 ≥1)"
    );

    let second = atlas
        .get_or_rasterize(ch, GlyphStyle::REGULAR)
        .expect("repeat query resolves from cache");
    let _ = second;
    assert_eq!(
        atlas.fallback_pool_scans.get(),
        scans_after_first,
        "probed_missing 短路: 第二次查询不得重扫 fallback 池(计数器不变)"
    );
    assert!(
        atlas.probed_missing.contains(&ch),
        "备忘必须持续存活(内存态,不落盘)"
    );
}
