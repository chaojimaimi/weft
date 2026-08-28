//! Single source for ratio-based color derivations (v1.11.6 C2).
//!
//! All formulas are BIT-EXACT ports of the inline expressions they replace
//! (D-d): evaluation order preserved so cached vertex bytes are unchanged.
//!
//! Known limitation (v1.11.6 M5, coordinator decision pending): the
//! `mix_fg_over_bg` form `fg*(1.0-t) + bg*t` is bit-identical to the old
//! two-literal form only for t ∈ {0.30, 0.50, 0.15} — the f32 complements
//! `1.0 - 0.65f32` (= 0.35000002…, 1 ulp above the 0.35 literal) and
//! `1.0 - 0.85f32` (= 0.14999998…, 1 ulp below the 0.15 literal) do NOT
//! equal the complementary literals, so products differ in the last ulp for
//! some channel values. Sites with those weights (settings/command_surface/
//! prompt accent 0.35/0.65; block_view diagnose panel 0.15/0.85) therefore
//! stay on their original inline expressions. Surfaced because the plan's
//! "对 t∈{0.30,0.50,0.65,0.85,0.15} 的 f32 补数可精确表示…逐位一致" claim is
//! false for t=0.65/0.85 (verified empirically; see M5 stop report).
//!
//! Deliberately NOT consolidated here (backlog, per D-c): the WCAG dual
//! luminance/contrast implementations (primitives f32-domain vs ui_tokens
//! u8-domain), the dual bisection contrast raisers, and the F3 alpha-family
//! variants (uniform alpha unification is a visual change → separate task).

/// `fg` over `bg` with `bg_weight` the background's weight:
/// `fg*(1.0-bg_weight) + bg*bg_weight` (rgb; alpha = fg 的 alpha 透传，硬
/// 规则 D-d).
///
/// Source sites (v1.11.6 M5.2, all f32-domain):
/// - label_c 70/30: `paint/settings.rs` label_c, `paint/block_view.rs`
///   prompt_c (prep), `paint/palette.rs` prompt_c, `src/renderer.rs`
///   scrollbar thumb (F2; old form `fg*0.70 + bg*0.30`).
/// - 50/50: `paint/palette.rs` status/suffix colors, `paint/overlays.rs`
///   suffix color (F4; old form `fg*0.50 + bg*0.50`).
/// - 0.85/0.15: `paint/overlays.rs` label color (F4 variant; old form
///   `fg*0.85 + bg*0.15`), called with `bg_weight = 0.15`.
///
/// Bit-exactness holds for the weights used by the migrated sites
/// (0.30/0.50/0.15); see the module note for 0.65/0.85.
pub(crate) fn mix_fg_over_bg(fg: [f32; 4], bg: [f32; 4], bg_weight: f32) -> [f32; 4] {
    [
        fg[0] * (1.0 - bg_weight) + bg[0] * bg_weight,
        fg[1] * (1.0 - bg_weight) + bg[1] * bg_weight,
        fg[2] * (1.0 - bg_weight) + bg[2] * bg_weight,
        fg[3],
    ]
}

/// DIM 衰减: `[c0*0.5, c1*0.5, c2*0.5, c3]` — alpha 原样透传（D-d 硬规则）。
///
/// Source sites (F1, three identical implementations):
/// `paint/grid_instances.rs` final-fg DIM, `paint/block_view/style.rs`
/// DIM-after-contrast, `paint/underline.rs` bold→bright DIM. Old form
/// `[c[0] * 0.5, c[1] * 0.5, c[2] * 0.5, c[3]]`.
pub(crate) fn dim_half(c: [f32; 4]) -> [f32; 4] {
    [c[0] * 0.5, c[1] * 0.5, c[2] * 0.5, c[3]]
}

/// 混白: `[bg0 + (1.0-bg0)*white, ...]`（rgb；alpha 恒为 1.0，与全部旧点位
/// 一致）。**必须保持现行 `bg + (1.0-bg)*t` 表达式形式**（architect P0-1:
/// 改写为 `bg*(1-t)+1.0*t` 会改变 f32 位）。
///
/// Source sites (F5): `paint/settings.rs` popup (+8%) / sidebar (+4%),
/// `paint/command_surface.rs` shell bg (+8%) / hover (+4%),
/// `paint/block_view.rs` sticky-header bg (+8%). ui_tokens.rs raised
/// (+8%, u8 域) is NOT migrated — domain differs (backlog per D-c).
pub(crate) fn lighten_to_white(bg: [f32; 4], white: f32) -> [f32; 4] {
    [
        bg[0] + (1.0 - bg[0]) * white,
        bg[1] + (1.0 - bg[1]) * white,
        bg[2] + (1.0 - bg[2]) * white,
        1.0,
    ]
}

#[cfg(test)]
mod tests {
    use super::{dim_half, lighten_to_white, mix_fg_over_bg};

    // ── 对拍基准 = 被替换的旧内联表达式原文（禁止把基准写成新函数自身）──

    #[test]
    fn mix_matches_old_070_030_expression_bitwise() {
        let fg = [0.83137255, 0.64705884, 0.45490196, 1.0];
        let bg = [0.13333333, 0.10980392, 0.09411765, 1.0];
        let result = mix_fg_over_bg(fg, bg, 0.30);
        let old = [
            fg[0] * 0.70 + bg[0] * 0.30,
            fg[1] * 0.70 + bg[1] * 0.30,
            fg[2] * 0.70 + bg[2] * 0.30,
            1.0,
        ];
        for i in 0..4 {
            assert_eq!(result[i].to_bits(), old[i].to_bits(), "channel {i}");
        }
    }

    #[test]
    fn mix_matches_old_050_050_expression_bitwise() {
        let fg = [0.2, 0.5, 0.9, 1.0];
        let bg = [0.1, 0.3, 0.05, 1.0];
        let result = mix_fg_over_bg(fg, bg, 0.50);
        let old = [
            fg[0] * 0.50 + bg[0] * 0.50,
            fg[1] * 0.50 + bg[1] * 0.50,
            fg[2] * 0.50 + bg[2] * 0.50,
            1.0,
        ];
        for i in 0..4 {
            assert_eq!(result[i].to_bits(), old[i].to_bits(), "channel {i}");
        }
    }

    #[test]
    fn mix_matches_old_085_015_expression_bitwise() {
        // The 0.85/0.15 variant (overlays label): old `fg*0.85 + bg*0.15`
        // is expressed as bg_weight = 0.15 (fg weight = 1.0 - 0.15 = 0.85).
        let fg = [0.9, 0.6, 0.3, 1.0];
        let bg = [0.13, 0.11, 0.09, 1.0];
        let result = mix_fg_over_bg(fg, bg, 0.15);
        let old = [
            fg[0] * 0.85 + bg[0] * 0.15,
            fg[1] * 0.85 + bg[1] * 0.15,
            fg[2] * 0.85 + bg[2] * 0.15,
            1.0,
        ];
        for i in 0..4 {
            assert_eq!(result[i].to_bits(), old[i].to_bits(), "channel {i}");
        }
    }

    #[test]
    fn mix_alpha_passes_fg_alpha_through() {
        // D-d hard rule: rgb is blended, alpha = first argument's alpha.
        let fg = [0.5, 0.5, 0.5, 0.70];
        let bg = [0.1, 0.2, 0.3, 0.0];
        let result = mix_fg_over_bg(fg, bg, 0.30);
        assert_eq!(result[3].to_bits(), fg[3].to_bits());
    }

    #[test]
    fn dim_half_matches_old_inline_expression_bitwise() {
        let c = [0.83137255, 0.45490196, 0.10980392, 0.60];
        let result = dim_half(c);
        let old = [c[0] * 0.5, c[1] * 0.5, c[2] * 0.5, c[3]];
        for i in 0..4 {
            assert_eq!(result[i].to_bits(), old[i].to_bits(), "channel {i}");
        }
    }

    #[test]
    fn dim_half_keeps_alpha_unmodified() {
        let result = dim_half([0.9, 0.8, 0.7, 0.42]);
        assert_eq!(result[3].to_bits(), 0.42f32.to_bits());
        assert_eq!(result[0].to_bits(), (0.9f32 * 0.5).to_bits());
    }

    #[test]
    fn lighten_008_matches_old_popup_expression_bitwise() {
        let bg = [0.1, 0.2, 0.3, 1.0];
        let result = lighten_to_white(bg, 0.08);
        let old = [
            bg[0] + (1.0 - bg[0]) * 0.08,
            bg[1] + (1.0 - bg[1]) * 0.08,
            bg[2] + (1.0 - bg[2]) * 0.08,
            1.0,
        ];
        for i in 0..4 {
            assert_eq!(result[i].to_bits(), old[i].to_bits(), "channel {i}");
        }
        // command_surface tests.rs assertion stays valid: bg 0.1 → 0.172.
        assert!((result[0] - 0.172).abs() < 1e-4);
    }

    #[test]
    fn lighten_004_matches_old_sidebar_expression_bitwise() {
        let bg = [0.13, 0.11, 0.09, 1.0];
        let result = lighten_to_white(bg, 0.04);
        let old = [
            bg[0] + (1.0 - bg[0]) * 0.04,
            bg[1] + (1.0 - bg[1]) * 0.04,
            bg[2] + (1.0 - bg[2]) * 0.04,
            1.0,
        ];
        for i in 0..4 {
            assert_eq!(result[i].to_bits(), old[i].to_bits(), "channel {i}");
        }
    }

    #[test]
    fn lighten_white_one_is_exact_white() {
        // t=1.0 lifts every channel to exactly 1.0 (old expression's
        // boundary behavior preserved).
        let result = lighten_to_white([0.7, 0.3, 0.1, 1.0], 1.0);
        for (i, channel) in result[..3].iter().enumerate() {
            assert_eq!(channel.to_bits(), 1.0f32.to_bits(), "channel {i}");
        }
        assert_eq!(result[3].to_bits(), 1.0f32.to_bits());
    }
}
