//! v1.12: 外部主题导入 + Weft 语义层推导。
//!
//! 开源终端主题（iTerm2 / wezterm / alacritty / base16）只提供
//! **终端兼容层**（fg / bg / cursor / selection / ANSI 0-15），而 Weft 的
//! `Theme` 还有 21 个自有语义角色（accent / syntax×10 / output×4 / link /
//! ui×4）。本模块负责把前者推导成后者，是「导入外部主题」的唯一入口。
//!
//! 推导规则来自 Weft 内置主题的既有约定（见 `Theme::weft_warm`，
//! `theme.rs:193-210`）：`command=ANSI 2`、`string=ANSI 5`、`number=ANSI 3`、
//! `variable=ANSI 4`、`operator=ANSI 1`、`flag=accent`、`success=ANSI 2`、
//! `failure=ANSI 1`；`accent` 取候选槽位中彩度最高且对背景对比度 ≥3:1 者，
//! 避免冷色主题配琥珀色口音色（"琥珀色 Nord" 问题）。
//!
//! 推导结果会过一遍 V17 §2.4 视觉层级校验：文字角色对背景 ≥4.5:1、
//! chrome ≥3:1，且 `command` / `argument` / `default` 两两可辨。

use crate::grid::Color;

use super::parsers::parse_hex;
use super::sections::ThemeConfig;
use super::theme::{OutputSemanticColors, SyntaxColors, Theme, ThemeUi};

/// 文字角色对背景的最低对比度（WCAG AA 正文）。
pub const TEXT_CONTRAST: f64 = 4.5;
/// Chrome（口音色/分隔线/注释）对背景的最低对比度（WCAG AA 大字号/图形）。
pub const CHROME_CONTRAST: f64 = 3.0;
/// 作者前景色的宽松下限：低于此值（基本等于"和背景同色"）才判定为退化主题
/// 并强制修复。介于 1.5 与 4.5 之间的作者色值原样保留 —— 绘制期的
/// `minimum_contrast`（默认 7.0）会兜底可读性，而悄悄改写主题的
/// foreground 会让"这个主题看起来不是我选的那套"。
const MIN_USABLE_FG_CONTRAST: f64 = 1.5;

/// accent 候选槽位，按 Weft 内置主题的偏好排序：先 cyan 槽（weft_warm 的
/// 琥珀口音色就落在 `palette[6]`），再 blue / magenta / bright blue / bright magenta。
const ACCENT_CANDIDATES: [usize; 5] = [6, 4, 5, 12, 13];

/// WCAG 2.1 sRGB 相对亮度，取值 0.0（黑）–1.0（白）。
pub fn relative_luminance(c: Color) -> f64 {
    fn linear(channel: u8) -> f64 {
        let v = f64::from(channel) / 255.0;
        if v <= 0.03928 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * linear(c.r) + 0.7152 * linear(c.g) + 0.0722 * linear(c.b)
}

/// WCAG 对比度，取值 1.0–21.0。
pub fn contrast_ratio(a: Color, b: Color) -> f64 {
    let la = relative_luminance(a);
    let lb = relative_luminance(b);
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// 线性混合：`t=0` 得 `a`，`t=1` 得 `b`。alpha 恒为 255（主题色不透明）。
pub fn mix_colors(a: Color, b: Color, t: f64) -> Color {
    let t = t.clamp(0.0, 1.0);
    let channel = |x: u8, y: u8| -> u8 {
        let v = f64::from(x) + (f64::from(y) - f64::from(x)) * t;
        v.round().clamp(0.0, 255.0) as u8
    };
    Color::rgb(channel(a.r, b.r), channel(a.g, b.g), channel(a.b, b.b))
}

/// 彩度近似（RGB 极差归一化），用于挑选最"有彩色"的口音色。
fn chroma(c: Color) -> f64 {
    let max = f64::from(c.r.max(c.g).max(c.b));
    let min = f64::from(c.r.min(c.g).min(c.b));
    (max - min) / 255.0
}

/// 把前景提升到对背景的最低对比度：朝白或朝黑二分混合（保留色相与通道
/// 序），与 `weft_app::paint::primitives::ensure_minimum_text_contrast`
/// 同算法，只是量纲为 u8。
pub fn ensure_minimum_contrast(foreground: Color, background: Color, minimum_ratio: f64) -> Color {
    let minimum_ratio = minimum_ratio.clamp(1.0, 21.0);
    if contrast_ratio(foreground, background) >= minimum_ratio {
        return foreground;
    }
    let white = Color::rgb(255, 255, 255);
    let black = Color::rgb(0, 0, 0);
    let target = if contrast_ratio(white, background) >= contrast_ratio(black, background) {
        white
    } else {
        black
    };
    let mixed = |amount: f64| mix_colors(foreground, target, amount);
    let mut low = 0.0;
    let mut high = 1.0;
    for _ in 0..12 {
        let middle = (low + high) * 0.5;
        if contrast_ratio(mixed(middle), background) >= minimum_ratio {
            high = middle;
        } else {
            low = middle;
        }
    }
    mixed(high)
}

/// 两色是否"过于接近"（亮度差与 RGB 距离同时过小）——用于视觉层级校验。
/// 两色的"可辨度"标量：亮度差权重大（人眼对明度最敏感），叠加 RGB 曼哈顿
/// 距离。`>= SEPARATION_MIN` 视为可辨。
fn color_separation(a: Color, b: Color) -> f64 {
    let luminance_gap = (relative_luminance(a) - relative_luminance(b)).abs();
    let distance = f64::from(
        u32::from(a.r.abs_diff(b.r)) + u32::from(a.g.abs_diff(b.g)) + u32::from(a.b.abs_diff(b.b)),
    );
    luminance_gap * 1000.0 + distance
}

/// 可辨阈值：等价于"亮度差 ≥0.02 或 RGB 距离 ≥40"。
const SEPARATION_MIN: f64 = 40.0;

/// 两色是否"过于接近"——用于视觉层级校验。
fn too_similar(a: Color, b: Color) -> bool {
    color_separation(a, b) < SEPARATION_MIN
}

/// 在 `bg → fg` 的插值线上挑一个与所有 `anchors` 都可辨的中间调（用于
/// `syntax.argument`）。候选按"最像中间调"排序，全都撞色时取分离度最大者。
fn pick_mid_tone(bg: Color, fg: Color, anchors: &[Color]) -> Color {
    const CANDIDATES: [f64; 5] = [0.72, 0.62, 0.82, 0.50, 0.90];
    let mut best = ensure_minimum_contrast(mix_colors(bg, fg, CANDIDATES[0]), bg, TEXT_CONTRAST);
    let mut best_score = f64::MIN;
    for t in CANDIDATES {
        let candidate = ensure_minimum_contrast(mix_colors(bg, fg, t), bg, TEXT_CONTRAST);
        let score = anchors
            .iter()
            .map(|anchor| color_separation(candidate, *anchor))
            .fold(f64::MAX, f64::min);
        if score >= SEPARATION_MIN {
            return candidate;
        }
        if score > best_score {
            best_score = score;
            best = candidate;
        }
    }
    best
}

/// 一个外部主题的**终端兼容层**字段。ANSI 0-15 顺序即终端标准顺序
/// （black red green yellow blue magenta cyan white + 对应 bright）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThemeImport {
    /// 主题名（用于日志与 UI 展示）。
    pub name: String,
    pub foreground: Color,
    pub background: Color,
    /// 缺省回落到 `foreground`。
    pub cursor: Option<Color>,
    /// 缺省回落到 `mix(bg, fg, 0.25)`。
    pub selection: Option<Color>,
    /// ANSI 0-15。
    pub ansi: [Color; 16],
}

impl ThemeImport {
    /// 便捷构造：只给 fg/bg/ANSI，cursor 与 selection 走推导。
    pub fn new(
        name: impl Into<String>,
        foreground: Color,
        background: Color,
        ansi: [Color; 16],
    ) -> Self {
        Self {
            name: name.into(),
            foreground,
            background,
            cursor: None,
            selection: None,
            ansi,
        }
    }

    pub fn with_cursor(mut self, cursor: Color) -> Self {
        self.cursor = Some(cursor);
        self
    }

    pub fn with_selection(mut self, selection: Color) -> Self {
        self.selection = Some(selection);
        self
    }

    /// 从 weft 自有 schema 主题文件的**兼容层**字段构造导入数据。
    ///
    /// 要求 `foreground` 与 `background` 同时存在（否则无从推导），`palette`
    /// 缺失的槽位沿用 xterm 标准调色板。
    pub fn from_config(name: &str, cfg: &ThemeConfig) -> Option<Self> {
        let foreground = cfg.foreground.as_deref().and_then(parse_hex)?;
        let background = cfg.background.as_deref().and_then(parse_hex)?;
        let standard = Color::standard_palette();
        let mut ansi = [Color::DEFAULT_FG; 16];
        ansi.copy_from_slice(&standard[..16]);
        for (index, slot) in ansi.iter_mut().enumerate() {
            if let Some(color) = cfg.palette.get(index).and_then(|hex| parse_hex(hex)) {
                *slot = color;
            }
        }
        Some(Self {
            name: name.to_string(),
            foreground,
            background,
            cursor: cfg.cursor.as_deref().and_then(parse_hex),
            selection: cfg.selection.as_deref().and_then(parse_hex),
            ansi,
        })
    }

    /// 从主题文件文本嗅探**外部格式**并解析为兼容层字段。支持：
    ///
    /// - wezterm TOML：`[colors]` + `ansi` / `brights` 数组
    /// - alacritty TOML：`[colors.primary]` + `normal` / `bright`
    /// - iTerm2 generic YAML：`color_01..color_16` + `foreground` / `background`
    /// - base16 / base24 YAML：`system: base16|base24` + `palette.base00..`
    ///
    /// 返回 `None` = 不是这些外部格式（调用方按 Weft 自有 schema 处理）。
    pub fn from_text(name: &str, text: &str, ext: &str) -> Option<Self> {
        match ext {
            "toml" => Self::from_toml(name, text),
            "yaml" | "yml" => Self::from_yaml(name, text),
            _ => None,
        }
    }

    fn from_toml(name: &str, text: &str) -> Option<Self> {
        let value: toml::Value = toml::from_str(text).ok()?;
        let colors = value.get("colors")?.as_table()?;
        let get = |key: &str| colors.get(key).and_then(|v| v.as_str()).and_then(parse_hex);
        if colors.contains_key("ansi") || colors.contains_key("brights") {
            // ── wezterm ──
            let foreground = get("foreground")?;
            let background = get("background")?;
            let normal = colors.get("ansi").and_then(|v| v.as_array())?;
            let brights = colors.get("brights").and_then(|v| v.as_array());
            let mut ansi = [Color::DEFAULT_FG; 16];
            for (index, slot) in ansi.iter_mut().enumerate().take(8) {
                *slot = normal.get(index)?.as_str().and_then(parse_hex)?;
            }
            for index in 0..8 {
                ansi[8 + index] = brights
                    .and_then(|b| b.get(index))
                    .and_then(|v| v.as_str())
                    .and_then(parse_hex)
                    .unwrap_or(ansi[index]);
            }
            Some(Self {
                name: name.to_string(),
                foreground,
                background,
                cursor: get("cursor_bg"),
                selection: get("selection_bg"),
                ansi,
            })
        } else if colors.contains_key("primary") {
            // ── alacritty ──
            const ORDER: [&str; 8] = [
                "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
            ];
            let primary = colors.get("primary")?.as_table()?;
            let foreground = primary
                .get("foreground")
                .and_then(|v| v.as_str())
                .and_then(parse_hex)?;
            let background = primary
                .get("background")
                .and_then(|v| v.as_str())
                .and_then(parse_hex)?;
            let normal = colors.get("normal")?.as_table()?;
            let bright = colors.get("bright").and_then(|v| v.as_table());
            let mut ansi = [Color::DEFAULT_FG; 16];
            for (index, key) in ORDER.iter().enumerate() {
                ansi[index] = normal.get(*key)?.as_str().and_then(parse_hex)?;
            }
            for (index, key) in ORDER.iter().enumerate() {
                ansi[8 + index] = bright
                    .and_then(|b| b.get(*key))
                    .and_then(|v| v.as_str())
                    .and_then(parse_hex)
                    .unwrap_or(ansi[index]);
            }
            let cursor = colors
                .get("cursor")
                .and_then(|t| t.get("cursor"))
                .and_then(|v| v.as_str())
                .and_then(parse_hex);
            let selection = colors
                .get("selection")
                .and_then(|t| t.get("background"))
                .and_then(|v| v.as_str())
                .and_then(parse_hex);
            Some(Self {
                name: name.to_string(),
                foreground,
                background,
                cursor,
                selection,
                ansi,
            })
        } else {
            None
        }
    }

    fn from_yaml(name: &str, text: &str) -> Option<Self> {
        let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(text).ok()?;
        let map = value.as_mapping()?;
        let key = |name: &str| serde_yaml_ng::Value::String(name.to_string());
        let scalar = |name: &str| {
            map.get(key(name))
                .and_then(|v| v.as_str())
                .and_then(parse_hex)
        };

        // ── iTerm2 generic YAML（color_01..color_16）──
        if map.contains_key(key("color_01")) {
            let foreground = scalar("foreground")?;
            let background = scalar("background")?;
            let mut ansi = [Color::DEFAULT_FG; 16];
            for (index, slot) in ansi.iter_mut().enumerate() {
                *slot = scalar(&format!("color_{:02}", index + 1))?;
            }
            return Some(Self {
                name: name.to_string(),
                foreground,
                background,
                cursor: scalar("cursor"),
                selection: scalar("selection"),
                ansi,
            });
        }

        // ── base16 / base24 ──
        let system = map.get(key("system"))?.as_str()?;
        if !matches!(system, "base16" | "base24") {
            return None;
        }
        let palette = map.get(key("palette"))?.as_mapping()?;
        let base = |slot: &str| {
            palette
                .get(key(slot))
                .and_then(|v| v.as_str())
                .and_then(parse_hex)
        };
        let background = base("base00")?;
        let foreground = base("base05")?;
        // tinted 官方 terminal 模板映射（与 scripts/import-themes.py 一致）
        const NORMAL: [&str; 8] = [
            "base01", "base08", "base0B", "base0A", "base0D", "base0E", "base0C", "base05",
        ];
        const BRIGHT: [&str; 8] = [
            "base03", "base08", "base0B", "base0A", "base0D", "base0E", "base0C", "base07",
        ];
        let mut ansi = [Color::DEFAULT_FG; 16];
        for (index, slot) in NORMAL.iter().enumerate() {
            ansi[index] = base(slot).unwrap_or(foreground);
        }
        for (index, slot) in BRIGHT.iter().enumerate() {
            ansi[8 + index] = if system == "base24" {
                // base24 有独立 bright 槽位 base10-base17
                base(&format!("base{:02X}", 0x10 + index)).unwrap_or(ansi[index])
            } else {
                base(slot).unwrap_or(ansi[index])
            };
        }
        Some(Self {
            name: name.to_string(),
            foreground,
            background,
            cursor: Some(foreground),
            selection: base("base02"),
            ansi,
        })
    }
}

/// 在候选槽位中选口音色：对比度达标的候选里彩度最高者；全部不达标则把
/// cyan 槽拉到 3:1。
fn pick_accent(palette: &[Color], background: Color) -> Color {
    let mut best: Option<(f64, Color)> = None;
    for &index in &ACCENT_CANDIDATES {
        let candidate = palette[index];
        if contrast_ratio(candidate, background) < CHROME_CONTRAST {
            continue;
        }
        let score = chroma(candidate);
        // MSRV 1.75：`Option::is_none_or` 要 1.82，这里保持 `map_or`。
        if best.map_or(true, |(best_score, _)| score > best_score) {
            best = Some((score, candidate));
        }
    }
    match best {
        Some((_, color)) => color,
        None => ensure_minimum_contrast(palette[6], background, CHROME_CONTRAST),
    }
}

/// v1.12: 把 `[theme]` 段的内联覆盖（hex / palette / syntax / output /
/// link / ui）叠加到**任意** base 主题上。此前这段逻辑内联在
/// `resolve_named` 中；导入外部主题时需要以导入主题为基底复用同一套
/// 覆盖语义，故抽出。
pub fn apply_overrides(mut theme: Theme, cfg: &ThemeConfig) -> Theme {
    if let Some(c) = cfg.foreground.as_deref().and_then(parse_hex) {
        theme.foreground = c;
    }
    if let Some(c) = cfg.background.as_deref().and_then(parse_hex) {
        theme.background = c;
    }
    if let Some(c) = cfg.cursor.as_deref().and_then(parse_hex) {
        theme.cursor = c;
    }
    if let Some(c) = cfg.selection.as_deref().and_then(parse_hex) {
        theme.selection = c;
    }
    if let Some(c) = cfg.accent.as_deref().and_then(parse_hex) {
        theme.accent = c;
    }
    if let Some(c) = cfg.accent_dim.as_deref().and_then(parse_hex) {
        theme.accent_dim = c;
    }
    if let Some(c) = cfg.separator.as_deref().and_then(parse_hex) {
        theme.separator = c;
    }
    for (i, hex) in cfg.palette.iter().enumerate() {
        if i >= 256 {
            break;
        }
        if let Some(c) = parse_hex(hex) {
            theme.palette[i] = c;
        }
    }
    // v1.0 S5: apply per-syntax-token color overrides on top of the base
    // theme's SyntaxColors. Each field is an optional hex string; absent
    // fields retain the base theme's value.
    if let Some(syn) = cfg.syntax.as_ref() {
        if let Some(c) = syn.command.as_deref().and_then(parse_hex) {
            theme.syntax.command = c;
        }
        if let Some(c) = syn.flag.as_deref().and_then(parse_hex) {
            theme.syntax.flag = c;
        }
        if let Some(c) = syn.path.as_deref().and_then(parse_hex) {
            theme.syntax.path = c;
        }
        if let Some(c) = syn.string.as_deref().and_then(parse_hex) {
            theme.syntax.string = c;
        }
        if let Some(c) = syn.number.as_deref().and_then(parse_hex) {
            theme.syntax.number = c;
        }
        if let Some(c) = syn.variable.as_deref().and_then(parse_hex) {
            theme.syntax.variable = c;
        }
        if let Some(c) = syn.operator.as_deref().and_then(parse_hex) {
            theme.syntax.operator = c;
        }
        if let Some(c) = syn.comment.as_deref().and_then(parse_hex) {
            theme.syntax.comment = c;
        }
        if let Some(c) = syn.argument.as_deref().and_then(parse_hex) {
            theme.syntax.argument = c;
        }
        if let Some(c) = syn.default.as_deref().and_then(parse_hex) {
            theme.syntax.default = c;
        }
    }
    // v1.7.0-B: apply output semantic color overrides.
    // v1.11.0: the `cwd` override was removed — that key was dead
    // config (the painter derives CWD gray from fg×0.65); see
    // AUDIT_v1.10.39 / PLAN_v111.
    if let Some(out) = cfg.output.as_ref() {
        if let Some(c) = out.output_default.as_deref().and_then(parse_hex) {
            theme.output.output_default = c;
        }
        if let Some(c) = out.metadata.as_deref().and_then(parse_hex) {
            theme.output.metadata = c;
        }
        if let Some(c) = out.success.as_deref().and_then(parse_hex) {
            theme.output.success = c;
        }
        if let Some(c) = out.failure.as_deref().and_then(parse_hex) {
            theme.output.failure = c;
        }
    }
    // v1.11.6 (PLAN_v1116 M6/D-f): `[theme] link` — OSC 8 hyperlink
    // underline color. Hex is u8-granular: parsed to Color then
    // /255-normalized into the f32 domain, so a user value can never
    // reproduce the exact 0.36/0.62/0.94 default (documented; P1-4).
    if let Some(c) = cfg.link.as_deref().and_then(parse_hex) {
        theme.link = [
            c.r as f32 / 255.0,
            c.g as f32 / 255.0,
            c.b as f32 / 255.0,
            1.0,
        ];
    }
    // v1.11.6 (PLAN_v1116 M6/D-f): `[theme.ui]` seed colors — a present
    // key replaces the UiColors dual-branch input downstream (the
    // 4.5-contrast gate still applies there); invalid hex falls back.
    if let Some(ui) = cfg.ui.as_ref() {
        if let Some(c) = ui.success.as_deref().and_then(parse_hex) {
            theme.ui.success = Some(c);
        }
        if let Some(c) = ui.warning.as_deref().and_then(parse_hex) {
            theme.ui.warning = Some(c);
        }
        if let Some(c) = ui.error.as_deref().and_then(parse_hex) {
            theme.ui.error = Some(c);
        }
        if let Some(c) = ui.find_match.as_deref().and_then(parse_hex) {
            theme.ui.find_match = Some(c);
        }
    }
    theme
}

/// 文件是否**显式声明**了语义层（accent / separator / syntax / output /
/// link / ui）。显式声明时尊重作者意图（以 weft_warm 为基底叠加覆盖）；
/// 未声明时语义层应由该文件自己的兼容层推导 —— 否则"只写 fg/bg/palette 的
/// 主题文件"会继承 weft_warm 的琥珀语义层，在白底主题上对比度全线不达标。
fn declares_semantic_layer(cfg: &ThemeConfig) -> bool {
    cfg.accent.is_some()
        || cfg.accent_dim.is_some()
        || cfg.separator.is_some()
        || cfg.link.is_some()
        || cfg.syntax.is_some()
        || cfg.output.is_some()
        || cfg.ui.is_some()
}

/// 解析一个主题文件文本，返回 `(base_theme, file_cfg)`：
///
/// - `base_theme`：外部格式 → `Theme::from_import`（语义层已推导）；否则 weft_warm
/// - `file_cfg`：文件内声明的内联覆盖（外部格式文件通常解析不出，取默认值）
///
/// **顺序很关键**：必须先嗅探外部格式再解析 `ThemeConfig`。base16/24 YAML 的
/// `palette` 是 mapping，与 `ThemeConfig::palette: Vec<String>` 类型冲突，若先
/// 解析 `ThemeConfig` 会直接失败，把可导入的主题误判成损坏文件。
///
/// 返回 `None` = 既不是已知外部格式，也不是合法的 weft 主题 schema。
pub(crate) fn resolve_theme_file(
    name: &str,
    text: &str,
    ext: &str,
) -> Option<(Theme, ThemeConfig)> {
    let import = ThemeImport::from_text(name, text, ext);
    let file_cfg: Option<ThemeConfig> = match ext {
        "toml" => toml::from_str(text).ok(),
        "yaml" | "yml" => serde_yaml_ng::from_str(text).ok(),
        _ => None,
    };
    match import {
        Some(import) => {
            tracing::info!(
                theme = %import.name,
                format = ext,
                "imported external theme (semantic layer derived)",
            );
            Some((Theme::from_import(&import), file_cfg.unwrap_or_default()))
        }
        None => {
            let cfg = file_cfg?;
            // 显式语义层 → 以 weft_warm 为基底尊重作者声明；否则由该文件
            // 自己的兼容层推导（"全兼容层 + 无语义层"是最常见的导入形态）。
            let base = if declares_semantic_layer(&cfg) {
                Theme::weft_warm()
            } else {
                ThemeImport::from_config(name, &cfg)
                    .map(|import| Theme::from_import(&import))
                    .unwrap_or_else(Theme::weft_warm)
            };
            Some((base, cfg))
        }
    }
}

impl Theme {
    /// 从外部主题的兼容层字段构建完整 `Theme`（含 21 个语义角色推导）。
    ///
    /// `palette[16..256]` 沿用 xterm 标准立方 + 灰阶（`Color::standard_palette`），
    /// 与内置主题一致——外部主题只需提供 16 色。
    pub fn from_import(import: &ThemeImport) -> Self {
        let mut palette = Color::standard_palette();
        palette[..16].copy_from_slice(&import.ansi);

        let bg = import.background;
        // 仅在"前景与背景基本同色"（退化主题）时强制修复；其余保留作者色值，
        // 可读性交给绘制期的 `minimum_contrast`（默认 7.0）兜底。
        let fg = if contrast_ratio(import.foreground, bg) < MIN_USABLE_FG_CONTRAST {
            ensure_minimum_contrast(import.foreground, bg, TEXT_CONTRAST)
        } else {
            import.foreground
        };
        let text = |color: Color| ensure_minimum_contrast(color, bg, TEXT_CONTRAST);
        let accent = pick_accent(&palette, bg);
        let link_source = if contrast_ratio(palette[4], bg) >= CHROME_CONTRAST {
            palette[4]
        } else {
            palette[12]
        };

        let mut theme = Self {
            foreground: fg,
            background: bg,
            cursor: import.cursor.unwrap_or(fg),
            selection: import.selection.unwrap_or_else(|| mix_colors(bg, fg, 0.25)),
            palette,
            accent,
            accent_dim: mix_colors(bg, accent, 0.45),
            separator: mix_colors(bg, fg, 0.18),
            syntax: SyntaxColors {
                command: text(palette[2]),
                flag: text(accent),
                path: text(mix_colors(palette[6], fg, 0.35)),
                string: text(palette[5]),
                number: text(palette[3]),
                variable: text(palette[4]),
                operator: text(palette[1]),
                comment: ensure_minimum_contrast(mix_colors(bg, fg, 0.55), bg, CHROME_CONTRAST),
                // argument 必须同时与 command、default 可辨 —— 取 bg→fg 线上的
                // 中间调，逐个候选验证分离度（固定 0.78 会在亮前景主题上贴着
                // default，撞色后由 repair 兜底反而容易顾此失彼）。
                argument: pick_mid_tone(bg, fg, &[text(palette[2]), fg]),
                default: fg,
            },
            output: OutputSemanticColors {
                output_default: fg,
                metadata: text(mix_colors(bg, fg, 0.68)),
                success: text(palette[2]),
                failure: text(palette[1]),
            },
            link: [
                f32::from(link_source.r) / 255.0,
                f32::from(link_source.g) / 255.0,
                f32::from(link_source.b) / 255.0,
                1.0,
            ],
            ui: ThemeUi::default(),
        };
        theme.repair_semantic_hierarchy();
        theme
    }

    /// 主题是否为暗色：按背景相对亮度判定（WCAG L<0.5），替代按名字猜。
    pub fn is_dark(&self) -> bool {
        relative_luminance(self.background) < 0.5
    }

    /// V17 §2.4 修复：`command` / `argument` / `default` 必须两两可辨。
    /// `argument` 重新回到中间调候选线，`command` 冲突时先朝口音色、再退化为
    /// 明度推法（单色主题里 accent 与前景同色，混色无效）。
    pub fn repair_semantic_hierarchy(&mut self) {
        let bg = self.background;
        let fg = self.foreground;
        if too_similar(self.syntax.command, self.syntax.default) {
            let pushed = ensure_minimum_contrast(
                mix_colors(self.syntax.command, self.accent, 0.45),
                bg,
                TEXT_CONTRAST,
            );
            self.syntax.command = if too_similar(pushed, self.syntax.default) {
                let far = if relative_luminance(bg) < 0.5 {
                    Color::rgb(255, 255, 255)
                } else {
                    Color::rgb(0, 0, 0)
                };
                ensure_minimum_contrast(
                    mix_colors(self.syntax.command, far, 0.35),
                    bg,
                    TEXT_CONTRAST,
                )
            } else {
                pushed
            };
        }
        if too_similar(self.syntax.command, self.syntax.argument) {
            self.syntax.argument = pick_mid_tone(bg, fg, &[self.syntax.command, fg]);
        }
    }

    /// 返回仍未满足视觉层级契约的项（空 vec = 全部合规）。用于导入期
    /// `warn!` 与单元测试断言。
    pub fn semantic_hierarchy_violations(&self) -> Vec<&'static str> {
        let mut violations = Vec::new();
        if too_similar(self.syntax.command, self.syntax.argument) {
            violations.push("command/argument");
        }
        if too_similar(self.syntax.command, self.syntax.default) {
            violations.push("command/default");
        }
        if too_similar(self.syntax.argument, self.syntax.default) {
            violations.push("argument/default");
        }
        let text_roles: [(&'static str, Color); 10] = [
            ("command", self.syntax.command),
            ("flag", self.syntax.flag),
            ("path", self.syntax.path),
            ("string", self.syntax.string),
            ("number", self.syntax.number),
            ("variable", self.syntax.variable),
            ("operator", self.syntax.operator),
            ("argument", self.syntax.argument),
            ("output.metadata", self.output.metadata),
            ("output.success", self.output.success),
        ];
        for (label, color) in text_roles {
            if contrast_ratio(color, self.background) < TEXT_CONTRAST - 1e-9 {
                violations.push(label);
            }
        }
        // `default` 与 `output.failure` 允许作者原色略低于 4.5（绘制期
        // `minimum_contrast` 兜底），但不得低于 3:1 可读下限。
        if contrast_ratio(self.syntax.default, self.background) < CHROME_CONTRAST - 1e-9 {
            violations.push("default");
        }
        if contrast_ratio(self.output.failure, self.background) < TEXT_CONTRAST - 1e-9 {
            violations.push("output.failure");
        }
        if contrast_ratio(self.syntax.comment, self.background) < CHROME_CONTRAST - 1e-9 {
            violations.push("comment");
        }
        violations
    }
}

#[cfg(test)]
#[path = "theme_import_tests.rs"]
mod tests;
