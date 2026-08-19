//! 终端选区高亮色（v1.10.22）——grid / block_view / prompt 三处共用。
//! 旧公式 `0.35×accent + 0.65×bg`（α=0.6）实测 11 主题对比率 1.27–1.75，
//! 均不足 WCAG 非文本 3:1。收口：base = `theme.selection`（死配置接线），
//! 缺失回退旧公式；painted（α 合成到主题背景）不足 3:1 时沿亮度方向二分推至达标。
//! P3b：结果按主题颜色指纹缓存（paint 每帧调用，内部二分对同一主题是纯重复）。

#[cfg(test)]
use std::cell::Cell;
use std::cell::RefCell;

use weft_core::config::Theme;
use weft_core::grid::Color;

use super::primitives::{color_to_normalized, composite_color_over, text_contrast_ratio};

/// 缓存条目：key 覆盖 `selection_colors_from` 的全部主题输入（见 `cache_key`）。
struct CacheEntry {
    key: [u8; 16],
    value: SelectionColors,
}

// 单条目缓存（容量 1：paint 路径每帧只有一个主题，换主题时重算一次即可）。
// 渲染只在主线程（winit 事件循环）运行，thread_local 无并发访问。
thread_local! {
    static SELECTION_COLORS_CACHE: RefCell<Option<CacheEntry>> = const { RefCell::new(None) };
}

/// 决定输出的全部主题输入的轻量字节指纹（4 色 × 4 通道）：
/// selection（base）、background（raw bg）、foreground（经 stripe_blended_bg
/// 派生第二条底色）、accent（None 回退分支）。任一遗漏都会在换主题后
/// 返回陈旧颜色——这是本缓存唯一的功能风险点。
fn cache_key(theme: &Theme) -> [u8; 16] {
    let mut key = [0u8; 16];
    let mut offset = 0;
    for color in [
        theme.selection,
        theme.background,
        theme.foreground,
        theme.accent,
    ] {
        key[offset..offset + 4].copy_from_slice(&[color.r, color.g, color.b, color.a]);
        offset += 4;
    }
    key
}

// 测试探针：缓存重算计数（命中不增）——证明缓存生效的观测点。
// 仅 cfg(test) 编译，生产构建零开销。
#[cfg(test)]
thread_local! {
    static RECOMPUTE_COUNT: Cell<u32> = const { Cell::new(0) };
}

#[cfg(test)]
fn recompute_count() -> u32 {
    RECOMPUTE_COUNT.with(|c| c.get())
}

/// WCAG 非文本（G207）门槛，不新增用户设置（对比保障是配色正确性的一部分，
/// 门槛常量写法同 `ui_tokens.rs` 面板选区）。
pub(crate) const SELECTION_BG_MIN_CONTRAST: f32 = 3.0;
/// 选区 quad 固定混合 alpha（与旧公式一致，保留半透明观感）。
const SELECTION_QUAD_ALPHA: f32 = 0.60;

/// 选区 quad 色（含 alpha）+ 文字对比用的 CPU 合成底色（painted 是 quad
/// 合成到主题背景后的实画色，GPU 实画与 CPU 文字对比基准一致）。
#[derive(Clone, Copy)]
pub(crate) struct SelectionColors {
    pub quad: [f32; 4],
    pub painted: [f32; 4],
}

/// base = theme.selection；缺失该键时回退旧 accent 混合公式（自定义主题
/// 缺键时继承基础主题，此处仅兜底）。painted 对 theme.background 对比率
/// < 3.0 时沿亮度方向（bg 亮→向黑、bg 暗→向白）二分推至 ≥3.0。
pub(crate) fn selection_colors(theme: &Theme) -> SelectionColors {
    let key = cache_key(theme);
    SELECTION_COLORS_CACHE.with(|cache| {
        if let Some(entry) = cache.borrow().as_ref() {
            if entry.key == key {
                return entry.value;
            }
        }
        // Compute outside the borrow: a future re-entrant call from inside
        // selection_colors_from would degrade to a recompute instead of
        // panicking on BorrowMutError (not reachable today — pure fn).
        let value = selection_colors_from(Some(theme.selection), theme);
        #[cfg(test)]
        RECOMPUTE_COUNT.with(|c| c.set(c.get() + 1));
        *cache.borrow_mut() = Some(CacheEntry { key, value });
        value
    })
}

/// 旧公式 fallback 基色：0.35×accent + 0.65×bg。
fn legacy_selection_rgb(accent: [f32; 4], bg: [f32; 4]) -> [f32; 4] {
    [
        accent[0] * 0.35 + bg[0] * 0.65,
        accent[1] * 0.35 + bg[1] * 0.65,
        accent[2] * 0.35 + bg[2] * 0.65,
        1.0,
    ]
}

fn selection_colors_from(base: Option<Color>, theme: &Theme) -> SelectionColors {
    let raw_bg = color_to_normalized(theme.background);
    // 对比保障同时按 raw bg（grid 路径默认单元格）与 stripe bg（block 行
    // canvas，向 fg 混 1.4/2.4%）两个底色取最小值评估——推亮方向不同时
    // 约束方可能互换（暗主题推白时 stripe 更紧、中灰推黑时 raw 更紧），
    // 单基准都会漏掉另一条路径。
    let bgs = [raw_bg, stripe_blended_bg(theme)];
    let base_rgb = match base {
        Some(c) => color_to_normalized(c),
        None => legacy_selection_rgb(color_to_normalized(theme.accent), raw_bg),
    };

    let quad = [base_rgb[0], base_rgb[1], base_rgb[2], SELECTION_QUAD_ALPHA];
    let mut painted = composite_color_over(quad, bgs[1]);
    if min_contrast_ratio(painted, &bgs) < SELECTION_BG_MIN_CONTRAST {
        painted = raise_painted_contrast(painted, &bgs, SELECTION_BG_MIN_CONTRAST);
    }

    // 反解 quad = (painted − (1−α)×stripe_bg)/α。α 固定时 quad 可表达的
    // painted 集是每通道 [0.4·bg, 0.6+0.4·bg]，极端推亮（如灰底黑选区推
    // 到白）会越界，clamp 后复验不足即落入下方不透明保底——该分支可达。
    let quad_rgb = [0, 1, 2].map(|i| {
        ((painted[i] - (1.0 - SELECTION_QUAD_ALPHA) * bgs[1][i]) / SELECTION_QUAD_ALPHA)
            .clamp(0.0, 1.0)
    });
    let final_quad = [quad_rgb[0], quad_rgb[1], quad_rgb[2], SELECTION_QUAD_ALPHA];
    let both_ok = bgs.iter().all(|bg| {
        text_contrast_ratio(composite_color_over(final_quad, *bg), *bg)
            >= SELECTION_BG_MIN_CONTRAST - 1e-5
    });
    if !both_ok {
        // 保底：painted 全不透明（两条底色下同色，min 评估已达标），
        // 绝不比修复前（全主题 <2:1）差。
        return SelectionColors {
            quad: [painted[0], painted[1], painted[2], 1.0],
            painted,
        };
    }
    SelectionColors {
        quad: final_quad,
        painted: composite_color_over(final_quad, raw_bg),
    }
}

/// painted（不透明目标色）对一组底色的最小 WCAG 对比率。
fn min_contrast_ratio(painted: [f32; 4], bgs: &[[f32; 4]]) -> f32 {
    bgs.iter()
        .map(|bg| text_contrast_ratio(painted, *bg))
        .fold(f32::INFINITY, f32::min)
}

/// block 视图行 canvas：向 foreground 混（surfaces.rs:78 的 1.4/2.4% 取
/// 保守值）后的底色。error/warning tone（12%/7%）与 hover 行低于此基准，
/// 为已知可接受边界；ANSI 彩色背景单元格同理是近似。
fn stripe_blended_bg(theme: &Theme) -> [f32; 4] {
    let bg = color_to_normalized(theme.background);
    let fg = color_to_normalized(theme.foreground);
    [
        bg[0] + (fg[0] - bg[0]) * 0.024,
        bg[1] + (fg[1] - bg[1]) * 0.024,
        bg[2] + (fg[2] - bg[2]) * 0.024,
        1.0,
    ]
}

/// 沿亮度方向（bg 暗→向白、bg 亮→向黑，取两底色下对比更优的极值）二分
/// 混合 painted 至对全部底色达标，混合量越少越好（保住基色 hue）。
fn raise_painted_contrast(painted: [f32; 4], bgs: &[[f32; 4]], minimum_ratio: f32) -> [f32; 4] {
    let white = [1.0, 1.0, 1.0, 1.0];
    let black = [0.0, 0.0, 0.0, 1.0];
    let ratio = |c: [f32; 4]| min_contrast_ratio(c, bgs);
    let target = if ratio(white) >= ratio(black) {
        white
    } else {
        black
    };
    let mix = |amount: f32| {
        [
            painted[0] + (target[0] - painted[0]) * amount,
            painted[1] + (target[1] - painted[1]) * amount,
            painted[2] + (target[2] - painted[2]) * amount,
            1.0,
        ]
    };
    let mut low = 0.0;
    let mut high = 1.0;
    for _ in 0..12 {
        let middle = (low + high) * 0.5;
        if ratio(mix(middle)) >= minimum_ratio {
            high = middle;
        } else {
            low = middle;
        }
    }
    mix(high)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bg(theme: &Theme) -> [f32; 4] {
        color_to_normalized(theme.background)
    }

    /// 旧实画色（selection 基色 α=0.6 合成到 bg）——方向断言的基准。
    fn old_painted(theme: &Theme) -> [f32; 4] {
        let sel = color_to_normalized(theme.selection);
        composite_color_over([sel[0], sel[1], sel[2], 0.60], bg(theme))
    }

    // 文档测试 1+3：solarized_dark（bg 暗→向亮）与 weft_light（bg 亮→向暗）
    // 都推至 ≥3.0（对 raw 与 stripe 两个底色同时成立），方向与背景亮度相反；
    // 中灰 bg 向黑推即可达标，quad 保持 α=0.6 且在 [0,1]。
    #[test]
    fn contrast_pushed_toward_luminance_opposite_direction() {
        for (theme, sign) in [(Theme::solarized_dark(), 1.0), (Theme::weft_light(), -1.0)] {
            let colors = selection_colors(&theme);
            assert!(text_contrast_ratio(colors.painted, bg(&theme)) >= 3.0);
            for (actual, baseline) in colors.painted[..3].iter().zip(old_painted(&theme)) {
                assert!(sign * (*actual - baseline) >= -1e-6);
            }
        }
        let mut theme = Theme::weft_warm();
        theme.background = Color::rgb(0x80, 0x80, 0x80);
        theme.selection = Color::rgb(0x80, 0x80, 0x80);
        let colors = selection_colors(&theme);
        assert!(text_contrast_ratio(colors.painted, bg(&theme)) >= 3.0);
        assert!((colors.quad[3] - 0.60).abs() < 1e-6);
        for channel in &colors.quad[..3] {
            assert!((0.0..=1.0).contains(channel));
        }
    }

    // 文档测试 2：weft_warm 只微推过 3.0，暖色通道次序（r>g>b）不变。
    #[test]
    fn weft_warm_nudged_past_min_hue_kept() {
        let theme = Theme::weft_warm();
        let colors = selection_colors(&theme);
        assert!(text_contrast_ratio(colors.painted, bg(&theme)) >= 3.0);
        assert!(colors.painted[0] > colors.painted[1] && colors.painted[1] > colors.painted[2]);
        let old = old_painted(&theme);
        let distance = (0..3)
            .map(|i| (colors.painted[i] - old[i]).powi(2))
            .sum::<f32>()
            .sqrt();
        assert!(distance < 0.3, "only nudged, moved {distance:.3}");
    }

    // 文档测试 4：已达标的 selection 原样通过（不推、α 保持 0.6）。
    #[test]
    fn sufficient_selection_passes_through() {
        let mut theme = Theme::solarized_dark();
        theme.selection = Color::rgb(0xff, 0xff, 0xff);
        let colors = selection_colors(&theme);
        let expected = composite_color_over([1.0, 1.0, 1.0, 0.60], bg(&theme));
        for (actual, baseline) in colors.painted.iter().zip(expected) {
            assert!((actual - baseline).abs() < 1e-6);
        }
        assert!((colors.quad[3] - 0.60).abs() < 1e-6);
        for channel in &colors.quad[..3] {
            assert!((channel - 1.0).abs() < 1e-6);
        }
    }

    // 文档测试 5：无 selection 键 → 旧 accent 公式结果。None 的结果必须等于
    // 「selection 恰好是旧公式色」的主题结果（u8 量化容差 0.02；黑 bg + 白
    // accent 上旧公式色对比 ~1.7 会被推向白，覆盖整条自适应管线）。
    #[test]
    fn missing_key_falls_back_to_legacy_formula() {
        // 黑 bg + 白 accent 主题：旧公式色 = 0.35 灰（hand-computed）
        let legacy = legacy_selection_rgb([1.0, 1.0, 1.0, 1.0], [0.0, 0.0, 0.0, 1.0]);
        assert!(legacy[..3].iter().all(|ch| (ch - 0.35).abs() < 1e-6));
        let blue = legacy_selection_rgb([0.0, 0.0, 1.0, 1.0], [1.0, 1.0, 1.0, 1.0]);
        assert!((blue[0] - 0.65).abs() < 1e-6 && (blue[2] - 1.0).abs() < 1e-6);

        let mut theme = Theme::weft_warm();
        theme.background = Color::rgb(0x00, 0x00, 0x00);
        theme.accent = Color::rgb(0xff, 0xff, 0xff);
        let colors = selection_colors_from(None, &theme);
        let mut legacy_theme = theme.clone();
        legacy_theme.selection = Color::rgb(
            (legacy[0] * 255.0).round() as u8,
            (legacy[1] * 255.0).round() as u8,
            (legacy[2] * 255.0).round() as u8,
        );
        let expected = selection_colors(&legacy_theme);
        for i in 0..4 {
            assert!((colors.quad[i] - expected.quad[i]).abs() < 0.02);
            assert!((colors.painted[i] - expected.painted[i]).abs() < 0.02);
        }
    }

    // 文档测试 6+7：数据驱动（对照实测表 1.27–1.75）——全部内置主题
    // painted ≥3.0，且 quad 合成回 bg == painted（GPU 实画一致性）。
    #[test]
    fn all_builtin_themes_meet_wcag_and_quad_consistency() {
        let themes = [
            ("weft_warm", Theme::weft_warm()),
            ("weft_light", Theme::weft_light()),
            ("warp_dark", Theme::warp_dark()),
            ("dracula", Theme::dracula()),
            ("solarized_dark", Theme::solarized_dark()),
            ("gruvbox_dark", Theme::gruvbox_dark()),
            ("nord", Theme::nord()),
            ("tokyo_night", Theme::tokyo_night()),
            ("catppuccin_mocha", Theme::catppuccin_mocha()),
            ("one_dark", Theme::one_dark()),
            ("monokai_pro", Theme::monokai_pro()),
        ];
        for (name, theme) in themes {
            let colors = selection_colors(&theme);
            let bg = bg(&theme);
            let stripe = stripe_blended_bg(&theme);
            assert!(
                text_contrast_ratio(colors.painted, bg) >= 3.0,
                "{name} below 3.0 vs raw bg"
            );
            assert!(
                text_contrast_ratio(composite_color_over(colors.quad, stripe), stripe)
                    >= 3.0 - 1e-5,
                "{name} below 3.0 vs stripe bg"
            );
            let recomposed = composite_color_over(colors.quad, bg);
            for (actual, baseline) in recomposed.iter().zip(colors.painted) {
                assert!((actual - baseline).abs() < 1e-5, "{name} quad≠painted");
            }
        }
    }

    // P3b 缓存行为：同主题重复调用必须命中（不重算）；key 任一颜色
    // （selection/background/foreground/accent）变更必须触发重算。
    #[test]
    fn cache_hits_on_same_theme_and_recomputes_on_any_key_color_change() {
        // Delta assertions (before/after counts), not absolute values: the
        // thread_local counter is per-thread and other tests on this thread
        // may have recomputed already — only the delta is this test's own.
        let theme = Theme::solarized_dark();
        let before = recompute_count();
        let first = selection_colors(&theme);
        assert_eq!(recompute_count() - before, 1, "first call must recompute");
        let second = selection_colors(&theme);
        assert_eq!(
            recompute_count() - before,
            1,
            "same theme must hit cache, no recompute"
        );
        assert_eq!(first.quad, second.quad);
        assert_eq!(first.painted, second.painted);

        let mut bg_changed = theme.clone();
        bg_changed.background = Color::rgb(0x00, 0x00, 0x00);
        let after_bg = selection_colors(&bg_changed);
        assert_eq!(
            recompute_count() - before,
            2,
            "background change must recompute"
        );
        assert_ne!(
            after_bg.painted, second.painted,
            "painted composites over raw bg"
        );

        let mut fg_changed = theme.clone();
        fg_changed.foreground = Color::rgb(0xff, 0x00, 0x00);
        let _ = selection_colors(&fg_changed);
        assert_eq!(
            recompute_count() - before,
            3,
            "foreground (stripe derivation) must recompute"
        );

        let mut accent_changed = theme.clone();
        accent_changed.accent = Color::rgb(0xff, 0x00, 0x00);
        let _ = selection_colors(&accent_changed);
        assert_eq!(recompute_count() - before, 4, "accent must recompute");

        let mut selection_changed = theme.clone();
        selection_changed.selection = Color::rgb(0xff, 0x00, 0x00);
        let after_sel = selection_colors(&selection_changed);
        assert_eq!(
            recompute_count() - before,
            5,
            "selection change must recompute"
        );
        assert_ne!(after_sel.painted, second.painted);
    }

    // 灰底黑选区（rust-reviewer 反例）：推向白后 quad 反解 >1 越界，
    // clamp 复验不足 → 不透明保底分支真实触发，且对两个底色仍 ≥3.0。
    #[test]
    fn gray_bg_black_selection_falls_back_to_opaque() {
        let mut theme = Theme::weft_warm();
        theme.background = Color::rgb(0x75, 0x75, 0x75);
        theme.selection = Color::rgb(0x00, 0x00, 0x00);
        let colors = selection_colors(&theme);
        assert!(
            (colors.quad[3] - 1.0).abs() < 1e-6,
            "fallback must be opaque"
        );
        for bg in [bg(&theme), stripe_blended_bg(&theme)] {
            assert!(text_contrast_ratio(composite_color_over(colors.quad, bg), bg) >= 3.0);
        }
    }
}
