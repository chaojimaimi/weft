//! v1.12: `theme_import` 的单元测试（拆出以满足 800 行架构门禁）。
//!
//! 覆盖：颜色工具（亮度/对比度/混合/保底对比度）、语义层推导（Dracula 实测色值、
//! Catppuccin Latte 亮色、黑底黑字退化、全灰单色层级修复）、多格式嗅探
//! （wezterm / alacritty / iTerm2 generic YAML / base16-24）与 weft 自有 schema 回归。

use super::*;

/// Dracula（iTerm2-Color-Schemes 实测值，wezterm/Dracula.toml）。
fn dracula() -> ThemeImport {
    let hex = |s: &str| crate::config::parse_hex(s).expect("valid hex");
    ThemeImport::new(
        "Dracula",
        hex("#f8f8f2"),
        hex("#282a36"),
        [
            hex("#21222c"),
            hex("#ff5555"),
            hex("#50fa7b"),
            hex("#f1fa8c"),
            hex("#bd93f9"),
            hex("#ff79c6"),
            hex("#8be9fd"),
            hex("#f8f8f2"),
            hex("#6272a4"),
            hex("#ff6e6e"),
            hex("#69ff94"),
            hex("#ffffa5"),
            hex("#d6acff"),
            hex("#ff92df"),
            hex("#a4ffff"),
            hex("#ffffff"),
        ],
    )
    .with_cursor(hex("#f8f8f2"))
    .with_selection(hex("#44475a"))
}

/// Catppuccin Latte（浅色，用于验证亮色分支）。
fn latte() -> ThemeImport {
    let hex = |s: &str| crate::config::parse_hex(s).expect("valid hex");
    ThemeImport::new(
        "Catppuccin Latte",
        hex("#4c4f69"),
        hex("#eff1f5"),
        [
            hex("#5c5f77"),
            hex("#d20f39"),
            hex("#40a02b"),
            hex("#df8e1d"),
            hex("#1e66f5"),
            hex("#ea76cb"),
            hex("#04a5e5"),
            hex("#acb0be"),
            hex("#6c6f85"),
            hex("#e64553"),
            hex("#40a02b"),
            hex("#df8e1d"),
            hex("#1e66f5"),
            hex("#ea76cb"),
            hex("#04a5e5"),
            hex("#bcc0cc"),
        ],
    )
}

#[test]
fn contrast_ratio_matches_wcag_reference() {
    let black = Color::rgb(0, 0, 0);
    let white = Color::rgb(255, 255, 255);
    assert!((contrast_ratio(white, black) - 21.0).abs() < 1e-9);
    assert!((contrast_ratio(black, black) - 1.0).abs() < 1e-9);
}

#[test]
fn mix_colors_endpoints_and_midpoint() {
    let black = Color::rgb(0, 0, 0);
    let white = Color::rgb(255, 255, 255);
    assert_eq!(mix_colors(black, white, 0.0), black);
    assert_eq!(mix_colors(black, white, 1.0), white);
    assert_eq!(mix_colors(black, white, 0.5), Color::rgb(128, 128, 128));
}

#[test]
fn ensure_minimum_contrast_is_idempotent_and_effective() {
    let bg = Color::rgb(0x28, 0x2a, 0x36);
    let dim = Color::rgb(0x30, 0x32, 0x3e);
    let raised = ensure_minimum_contrast(dim, bg, TEXT_CONTRAST);
    assert!(contrast_ratio(raised, bg) >= TEXT_CONTRAST - 1e-6);
    // 已达标时原样返回（幂等）。
    assert_eq!(ensure_minimum_contrast(raised, bg, TEXT_CONTRAST), raised);
}

#[test]
fn dracula_import_fills_semantic_layer() {
    let theme = Theme::from_import(&dracula());
    assert_eq!(
        theme.foreground,
        crate::config::parse_hex("#f8f8f2").unwrap()
    );
    assert_eq!(
        theme.background,
        crate::config::parse_hex("#282a36").unwrap()
    );
    assert_eq!(theme.cursor, crate::config::parse_hex("#f8f8f2").unwrap());
    assert_eq!(
        theme.selection,
        crate::config::parse_hex("#44475a").unwrap()
    );
    // 16 色原样落位，16..255 沿用标准立方。
    assert_eq!(
        theme.palette[1],
        crate::config::parse_hex("#ff5555").unwrap()
    );
    assert_eq!(theme.palette[255], Color::standard_palette()[255]);
    // accent 取候选里彩度最高者：#ff79c6（magenta 槽）> #8be9fd。
    assert_eq!(theme.accent, crate::config::parse_hex("#ff79c6").unwrap());
    // command/string/number 走 ANSI 槽位约定。
    assert_eq!(
        theme.syntax.command,
        crate::config::parse_hex("#50fa7b").unwrap()
    );
    assert_eq!(
        theme.syntax.string,
        crate::config::parse_hex("#ff79c6").unwrap()
    );
    assert_eq!(
        theme.syntax.number,
        crate::config::parse_hex("#f1fa8c").unwrap()
    );
    assert_eq!(theme.output.success, theme.syntax.command);
    assert_eq!(
        theme.output.failure,
        crate::config::parse_hex("#ff5555").unwrap()
    );
}

#[test]
fn dracula_import_passes_hierarchy_contract() {
    let theme = Theme::from_import(&dracula());
    assert!(
        theme.semantic_hierarchy_violations().is_empty(),
        "violations: {:?}",
        theme.semantic_hierarchy_violations()
    );
    assert!(theme.is_dark());
}

#[test]
fn light_theme_is_not_dark_and_stays_readable() {
    let theme = Theme::from_import(&latte());
    assert!(!theme.is_dark());
    assert!(
        theme.semantic_hierarchy_violations().is_empty(),
        "violations: {:?}",
        theme.semantic_hierarchy_violations()
    );
    // 亮色主题下口音色仍需 3:1。
    assert!(contrast_ratio(theme.accent, theme.background) >= CHROME_CONTRAST - 1e-9);
}

#[test]
fn degenerate_black_on_black_theme_is_repaired() {
    // fg == bg 的极端主题：推导必须把文字拉到可读，而不是产出隐形字。
    let degenerate = ThemeImport::new(
        "degenerate",
        Color::rgb(0, 0, 0),
        Color::rgb(0, 0, 0),
        [Color::rgb(0, 0, 0); 16],
    );
    let theme = Theme::from_import(&degenerate);
    assert!(contrast_ratio(theme.syntax.default, theme.background) >= TEXT_CONTRAST - 1e-6);
    assert!(contrast_ratio(theme.syntax.command, theme.background) >= TEXT_CONTRAST - 1e-6);
    assert!(contrast_ratio(theme.syntax.comment, theme.background) >= CHROME_CONTRAST - 1e-6);
}

#[test]
fn monochrome_palette_still_yields_distinguishable_roles() {
    // 全灰 ANSI：command/argument/default 极易撞色，必须被 repair 拆开。
    let grey = |v: u8| Color::rgb(v, v, v);
    let mono = ThemeImport::new(
        "mono",
        grey(0xcc),
        grey(0x22),
        [
            grey(0x22),
            grey(0xcc),
            grey(0xcc),
            grey(0xcc),
            grey(0xcc),
            grey(0xcc),
            grey(0xcc),
            grey(0xcc),
            grey(0xcc),
            grey(0xcc),
            grey(0xcc),
            grey(0xcc),
            grey(0xcc),
            grey(0xcc),
            grey(0xcc),
            grey(0xcc),
        ],
    );
    let theme = Theme::from_import(&mono);
    assert!(
        theme.semantic_hierarchy_violations().is_empty(),
        "violations: {:?}",
        theme.semantic_hierarchy_violations()
    );
}

#[test]
fn link_color_falls_back_to_bright_blue_when_blue_is_unreadable() {
    let hex = |s: &str| crate::config::parse_hex(s).expect("valid hex");
    let mut import = dracula();
    import.ansi[4] = import.background; // blue 与背景同色
    let theme = Theme::from_import(&import);
    // 回落到 bright blue（palette[12] = #d6acff）。
    assert_eq!(
        theme.link,
        [
            f32::from(hex("#d6acff").r) / 255.0,
            f32::from(hex("#d6acff").g) / 255.0,
            f32::from(hex("#d6acff").b) / 255.0,
            1.0
        ]
    );
}

// ── 多格式嗅探（P2：把 wezterm/alacritty/base16 文件直接丢进 themes 目录）──

const WEZTERM_TOML: &str = r##"
[colors]
foreground = "#f8f8f2"
background = "#282a36"
cursor_bg = "#f8f8f2"
cursor_fg = "#282a36"
selection_bg = "#44475a"
selection_fg = "#ffffff"
ansi = ["#21222c","#ff5555","#50fa7b","#f1fa8c","#bd93f9","#ff79c6","#8be9fd","#f8f8f2"]
brights = ["#6272a4","#ff6e6e","#69ff94","#ffffa5","#d6acff","#ff92df","#a4ffff","#ffffff"]
"##;

const ALACRITTY_TOML: &str = r##"
[colors.primary]
background = '#282a36'
foreground = '#f8f8f2'
[colors.normal]
black = '#21222c'
red = '#ff5555'
green = '#50fa7b'
yellow = '#f1fa8c'
blue = '#bd93f9'
magenta = '#ff79c6'
cyan = '#8be9fd'
white = '#f8f8f2'
[colors.bright]
black = '#6272a4'
red = '#ff6e6e'
green = '#69ff94'
yellow = '#ffffa5'
blue = '#d6acff'
magenta = '#ff92df'
cyan = '#a4ffff'
white = '#ffffff'
[colors.cursor]
cursor = '#f8f8f2'
text = '#282a36'
[colors.selection]
background = '#44475a'
text = '#ffffff'
"##;

const GENERIC_YAML: &str = r##"
name: "Dracula"
color_01: "#21222c"
color_02: "#ff5555"
color_03: "#50fa7b"
color_04: "#f1fa8c"
color_05: "#bd93f9"
color_06: "#ff79c6"
color_07: "#8be9fd"
color_08: "#f8f8f2"
color_09: "#6272a4"
color_10: "#ff6e6e"
color_11: "#69ff94"
color_12: "#ffffa5"
color_13: "#d6acff"
color_14: "#ff92df"
color_15: "#a4ffff"
color_16: "#ffffff"
cursor: "#f8f8f2"
foreground: "#f8f8f2"
background: "#282a36"
selection: "#44475a"
"##;

const BASE16_YAML: &str = r##"
system: "base16"
name: "Dracula"
variant: "dark"
palette:
  base00: "#282a36"
  base01: "#21222c"
  base02: "#44475A"
  base03: "#6272a4"
  base04: "#9ea8c7"
  base05: "#f8f8f2"
  base06: "#f8f8f2"
  base07: "#ffffff"
  base08: "#ff5555"
  base0A: "#f1fa8c"
  base0B: "#50fa7b"
  base0C: "#8be9fd"
  base0D: "#bd93f9"
  base0E: "#ff79c6"
"##;

fn expect_dracula(import: &ThemeImport) {
    let hex = |s: &str| crate::config::parse_hex(s).expect("valid hex");
    assert_eq!(import.foreground, hex("#f8f8f2"));
    assert_eq!(import.background, hex("#282a36"));
    assert_eq!(import.ansi[0], hex("#21222c"));
    assert_eq!(import.ansi[1], hex("#ff5555"));
    assert_eq!(import.ansi[4], hex("#bd93f9"));
    assert_eq!(import.ansi[8], hex("#6272a4"));
    assert_eq!(import.ansi[15], hex("#ffffff"));
}

#[test]
fn sniff_wezterm_toml() {
    let import = ThemeImport::from_text("Dracula", WEZTERM_TOML, "toml").expect("wezterm");
    expect_dracula(&import);
    assert_eq!(
        import.cursor,
        Some(crate::config::parse_hex("#f8f8f2").unwrap())
    );
    assert_eq!(
        import.selection,
        Some(crate::config::parse_hex("#44475a").unwrap())
    );
}

#[test]
fn sniff_alacritty_toml() {
    let import = ThemeImport::from_text("Dracula", ALACRITTY_TOML, "toml").expect("alacritty");
    expect_dracula(&import);
    assert_eq!(
        import.cursor,
        Some(crate::config::parse_hex("#f8f8f2").unwrap())
    );
    assert_eq!(
        import.selection,
        Some(crate::config::parse_hex("#44475a").unwrap())
    );
}

#[test]
fn sniff_generic_iterm_yaml() {
    let import = ThemeImport::from_text("Dracula", GENERIC_YAML, "yaml").expect("generic yaml");
    expect_dracula(&import);
}

#[test]
fn sniff_base16_yaml() {
    let import = ThemeImport::from_text("dracula", BASE16_YAML, "yml").expect("base16");
    let hex = |s: &str| crate::config::parse_hex(s).expect("valid hex");
    assert_eq!(import.background, hex("#282a36"));
    assert_eq!(import.foreground, hex("#f8f8f2"));
    // base16 → ANSI：0=base01 1=base08 2=base0B 4=base0D 8=base03 15=base07
    assert_eq!(import.ansi[0], hex("#21222c"));
    assert_eq!(import.ansi[1], hex("#ff5555"));
    assert_eq!(import.ansi[2], hex("#50fa7b"));
    assert_eq!(import.ansi[4], hex("#bd93f9"));
    assert_eq!(import.ansi[8], hex("#6272a4"));
    assert_eq!(import.ansi[15], hex("#ffffff"));
    assert_eq!(import.selection, Some(hex("#44475a")));
}

#[test]
fn sniff_returns_none_for_weft_native_schema() {
    // Weft 自有 schema（扁平键、无 [colors]）必须走原有路径，不能被误判。
    let native = "background = \"#1e1e2e\"\nforeground = \"#cdd6f4\"\npalette = [\"#1e1e2e\"]\n";
    assert!(ThemeImport::from_text("mine", native, "toml").is_none());
}

#[test]
fn sniffed_import_produces_valid_theme() {
    let import = ThemeImport::from_text("Dracula", WEZTERM_TOML, "toml").expect("wezterm");
    let theme = Theme::from_import(&import);
    assert!(theme.is_dark());
    assert!(theme.semantic_hierarchy_violations().is_empty());
}

#[test]
fn cursor_and_selection_default_to_derived_values() {
    let mut import = dracula();
    import.cursor = None;
    import.selection = None;
    let theme = Theme::from_import(&import);
    assert_eq!(theme.cursor, theme.foreground);
    assert_eq!(
        theme.selection,
        mix_colors(theme.background, theme.foreground, 0.25)
    );
}
