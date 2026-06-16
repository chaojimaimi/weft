//! Configuration & theming.
//!
//! TOML config at `$XDG_CONFIG_HOME/weft/config.toml` (or
//! `~/.config/weft/config.toml`), fully optional — sensible defaults apply.
//! Parsed here into a resolved [`Config`] / [`Theme`] / [`KeyBindings`] that the
//! app layer consumes. This module is pure logic (no rendering / fs effects
//! beyond reading the file), so it is unit-testable.
//!
//! ```toml
//! [font]
//! family = "Menlo"
//! size = 14.0
//! cjk_family = "PingFang SC"
//! emoji_family = "Apple Color Emoji"
//!
//! [theme]
//! name = "weft-dark"            # weft-dark | weft-light
//! foreground = "#c8c8c8"        # optional inline overrides
//! palette = ["#1a1a2e", "#cc5555", ...]   # optional, overrides ANSI 0-15
//!
//! [window]
//! width = 800
//! height = 600
//! title = "Weft"
//! opacity = 1.0
//!
//! [scrollback]
//! lines = 10000
//!
//! [keybindings]
//! "cmd+c" = "copy"
//! "cmd+v" = "paste"
//! ```

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

use crate::grid::Color;
use crate::input::{KeyCode, Modifiers};

// ── Theme (resolved colors the renderer needs) ─────────────────────────

/// A fully-resolved theme: the colors the renderer paints with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Theme {
    pub foreground: Color,
    pub background: Color,
    pub cursor: Color,
    pub selection: Color,
    /// 256-color palette; slots 0-15 are the ANSI colors.
    pub palette: [Color; 256],
}

impl Theme {
    /// Built-in dark theme (default).
    pub fn weft_dark() -> Self {
        let mut palette = Color::standard_palette();
        // Refined ANSI 0-15 for the dark theme.
        let ansi = [
            (35, 38, 52),    // 0 black
            (204, 85, 85),   // 1 red
            (138, 204, 92),  // 2 green
            (218, 178, 92),  // 3 yellow
            (92, 158, 218),  // 4 blue
            (190, 132, 218), // 5 magenta
            (92, 204, 204),  // 6 cyan
            (200, 200, 200), // 7 white
            (90, 96, 116),   // 8 bright black
            (235, 110, 110), // 9 bright red
            (170, 220, 120), // 10 bright green
            (232, 196, 110), // 11 bright yellow
            (120, 180, 235), // 12 bright blue
            (210, 160, 235), // 13 bright magenta
            (120, 220, 220), // 14 bright cyan
            (235, 235, 235), // 15 bright white
        ];
        for (i, (r, g, b)) in ansi.iter().enumerate() {
            palette[i] = Color::rgb(*r, *g, *b);
        }
        Self {
            foreground: Color::rgb(200, 200, 200),
            background: Color::rgb(23, 25, 35),
            cursor: Color::rgb(235, 235, 235),
            selection: Color::rgb(51, 102, 204),
            palette,
        }
    }

    /// Built-in light theme.
    pub fn weft_light() -> Self {
        let mut palette = Color::standard_palette();
        let ansi = [
            (40, 42, 54),    // 0 black
            (190, 50, 50),   // 1 red
            (60, 140, 60),   // 2 green
            (170, 130, 40),  // 3 yellow
            (50, 90, 180),   // 4 blue
            (150, 70, 170),  // 5 magenta
            (40, 140, 150),  // 6 cyan
            (35, 38, 48),    // 7 white (text)
            (130, 134, 146), // 8 bright black
            (220, 90, 90),   // 9 bright red
            (90, 180, 90),   // 10 bright green
            (200, 160, 70),  // 11 bright yellow
            (80, 130, 220),  // 12 bright blue
            (180, 100, 200), // 13 bright magenta
            (80, 180, 190),  // 14 bright cyan
            (20, 22, 30),    // 15 bright white
        ];
        for (i, (r, g, b)) in ansi.iter().enumerate() {
            palette[i] = Color::rgb(*r, *g, *b);
        }
        Self {
            foreground: Color::rgb(35, 38, 48),
            background: Color::rgb(245, 245, 240),
            cursor: Color::rgb(35, 38, 48),
            selection: Color::rgb(180, 200, 235),
            palette,
        }
    }

    /// Resolve a theme from config: pick the built-in base by name (default
    /// `weft-dark`), then apply any inline hex overrides.
    pub fn resolve(cfg: &ThemeConfig) -> Self {
        let base = match cfg.name.as_str() {
            "weft-light" => Self::weft_light(),
            _ => Self::weft_dark(),
        };
        let mut theme = base;
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
        for (i, hex) in cfg.palette.iter().enumerate() {
            if i >= 256 {
                break;
            }
            if let Some(c) = parse_hex(hex) {
                theme.palette[i] = c;
            }
        }
        theme
    }
}

// ── Actions & keybindings ──────────────────────────────────────────────

/// A bindable action (the value side of a keybinding).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Deserialize)]
pub enum Action {
    #[serde(rename = "copy")]
    Copy,
    #[serde(rename = "paste")]
    Paste,
    #[serde(rename = "reload_config")]
    ReloadConfig,
    #[serde(rename = "scroll_page_up")]
    ScrollPageUp,
    #[serde(rename = "scroll_page_down")]
    ScrollPageDown,
    #[serde(rename = "scroll_to_top")]
    ScrollToTop,
    #[serde(rename = "scroll_to_bottom")]
    ScrollToBottom,
}

/// Resolved keybinding table: physical key + modifiers → action.
#[derive(Clone, Debug)]
pub struct KeyBindings {
    pub map: HashMap<(KeyCode, Modifiers), Action>,
}

impl Default for KeyBindings {
    fn default() -> Self {
        // Sensible defaults; user config merges onto (overrides) these.
        let pairs: &[(&str, Action)] = &[
            ("cmd+c", Action::Copy),
            ("cmd+v", Action::Paste),
            ("cmd+shift+comma", Action::ReloadConfig),
            ("shift+page_up", Action::ScrollPageUp),
            ("shift+page_down", Action::ScrollPageDown),
            ("cmd+home", Action::ScrollToTop),
            ("cmd+end", Action::ScrollToBottom),
        ];
        let mut map = HashMap::new();
        for (binding, action) in pairs {
            if let Some((k, m)) = parse_binding(binding) {
                map.insert((k, m), *action);
            }
        }
        Self { map }
    }
}

impl KeyBindings {
    /// Build from user overrides merged onto the defaults. Unparseable
    /// bindings are skipped (with a `warn!`).
    pub fn from_overrides(overrides: &HashMap<String, Action>) -> Self {
        let mut kb = Self::default();
        for (binding, action) in overrides {
            match parse_binding(binding) {
                Some((k, m)) => {
                    kb.map.insert((k, m), *action);
                }
                None => {
                    tracing::warn!(binding, "skipping unparseable keybinding");
                }
            }
        }
        kb
    }

    /// Look up the action for a key + modifier combo.
    pub fn lookup(&self, key: KeyCode, mods: Modifiers) -> Option<Action> {
        self.map.get(&(key, mods)).copied()
    }
}

// ── Config (deserialized from TOML) ────────────────────────────────────

/// Top-level config. Every section is optional (`#[serde(default)]`).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub font: FontConfig,
    pub theme: ThemeConfig,
    pub window: WindowConfig,
    pub scrollback: ScrollbackConfig,
    /// Raw user keybinding overrides: `"cmd+x" = "copy"`. Resolved later via
    /// [`Config::keybindings`] (merged onto defaults).
    pub keybindings: HashMap<String, Action>,
}

impl Config {
    /// Load config from the well-known path. Missing file or parse error
    /// falls back to defaults (parse errors are logged).
    pub fn load() -> Self {
        let path = Self::config_path();
        let Some(path) = path else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
                tracing::warn!(path = %path.display(), error = %e, "failed to parse config; using defaults");
                Self::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "failed to read config; using defaults");
                Self::default()
            }
        }
    }

    /// The config file path: `$XDG_CONFIG_HOME/weft/config.toml`, else
    /// `~/.config/weft/config.toml`. `None` when neither env var is set.
    pub fn config_path() -> Option<PathBuf> {
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
            return Some(PathBuf::from(xdg).join("weft").join("config.toml"));
        }
        std::env::var_os("HOME").map(|h| {
            PathBuf::from(h)
                .join(".config")
                .join("weft")
                .join("config.toml")
        })
    }

    /// Resolve the active theme.
    pub fn theme(&self) -> Theme {
        Theme::resolve(&self.theme)
    }

    /// Resolve keybindings (defaults + user overrides).
    pub fn keybindings(&self) -> KeyBindings {
        KeyBindings::from_overrides(&self.keybindings)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct FontConfig {
    pub family: String,
    pub size: f32,
    pub cjk_family: String,
    pub emoji_family: String,
    pub line_height: f32,
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            family: "Menlo".into(),
            size: 14.0,
            cjk_family: "PingFang SC".into(),
            emoji_family: "Apple Color Emoji".into(),
            line_height: 1.2,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct ThemeConfig {
    pub name: String,
    pub foreground: Option<String>,
    pub background: Option<String>,
    pub cursor: Option<String>,
    pub selection: Option<String>,
    pub palette: Vec<String>,
}

// Manual Default (deriving would give name = "").
impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            name: "weft-dark".into(),
            foreground: None,
            background: None,
            cursor: None,
            selection: None,
            palette: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct WindowConfig {
    pub width: u32,
    pub height: u32,
    pub title: String,
    pub opacity: f32,
    pub padding_x: u32,
    pub padding_y: u32,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            width: 800,
            height: 600,
            title: "Weft".into(),
            opacity: 1.0,
            padding_x: 0,
            padding_y: 0,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct ScrollbackConfig {
    pub lines: usize,
}

impl Default for ScrollbackConfig {
    fn default() -> Self {
        Self { lines: 10_000 }
    }
}

// ── Parsing helpers ────────────────────────────────────────────────────

/// Parse a hex color: `#rgb`, `#rrggbb`, or `#rrggbbaa` (case-insensitive,
/// leading `#` optional).
pub fn parse_hex(s: &str) -> Option<Color> {
    let s = s.trim().trim_start_matches('#');
    let (r, g, b, a) = match s.len() {
        3 => {
            let bytes = s.as_bytes();
            (
                hex_val(bytes[0])? * 17,
                hex_val(bytes[1])? * 17,
                hex_val(bytes[2])? * 17,
                255,
            )
        }
        6 | 8 => {
            let bytes = s.as_bytes();
            let r = hex_val(bytes[0])? * 16 + hex_val(bytes[1])?;
            let g = hex_val(bytes[2])? * 16 + hex_val(bytes[3])?;
            let b = hex_val(bytes[4])? * 16 + hex_val(bytes[5])?;
            let a = if s.len() == 8 {
                hex_val(bytes[6])? * 16 + hex_val(bytes[7])?
            } else {
                255
            };
            (r, g, b, a)
        }
        _ => return None,
    };
    Some(Color { r, g, b, a })
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Parse a keybinding spec like `"cmd+shift+page_up"` or `"cmd+,"` into a
/// `(KeyCode, Modifiers)` pair. Tokens are split on `+`; the final token is
/// the key, the rest are modifiers. Unknown tokens → `None`.
pub fn parse_binding(spec: &str) -> Option<(KeyCode, Modifiers)> {
    let tokens: Vec<&str> = spec.split('+').map(str::trim).collect();
    if tokens.is_empty() {
        return None;
    }
    let mut mods = Modifiers::empty();
    for tok in &tokens[..tokens.len() - 1] {
        match tok.to_ascii_lowercase().as_str() {
            "cmd" | "super" | "win" | "meta" => mods |= Modifiers::SUPER,
            "ctrl" | "control" => mods |= Modifiers::CONTROL,
            "alt" | "option" | "opt" => mods |= Modifiers::ALT,
            "shift" => mods |= Modifiers::SHIFT,
            _ => return None,
        }
    }
    let key = parse_key_token(tokens.last().unwrap())?;
    Some((key, mods))
}

fn parse_key_token(tok: &str) -> Option<KeyCode> {
    let lower = tok.to_ascii_lowercase();
    match lower.as_str() {
        "enter" | "return" => Some(KeyCode::Enter),
        "tab" => Some(KeyCode::Tab),
        "escape" | "esc" => Some(KeyCode::Escape),
        "backspace" => Some(KeyCode::Backspace),
        "up" => Some(KeyCode::Up),
        "down" => Some(KeyCode::Down),
        "left" => Some(KeyCode::Left),
        "right" => Some(KeyCode::Right),
        "home" => Some(KeyCode::Home),
        "end" => Some(KeyCode::End),
        "page_up" | "pageup" => Some(KeyCode::PageUp),
        "page_down" | "pagedown" => Some(KeyCode::PageDown),
        "delete" | "del" => Some(KeyCode::Delete),
        "insert" | "ins" => Some(KeyCode::Insert),
        "space" => Some(KeyCode::Char(' ')),
        "comma" => Some(KeyCode::Char(',')),
        "period" => Some(KeyCode::Char('.')),
        "minus" | "hyphen" => Some(KeyCode::Char('-')),
        "plus" => Some(KeyCode::Char('+')),
        "equals" => Some(KeyCode::Char('=')),
        _ => {
            // f1..=f12
            if let Some(n) = lower.strip_prefix('f') {
                if let Ok(n) = n.parse::<u8>() {
                    if (1..=12).contains(&n) {
                        return Some(KeyCode::F(n));
                    }
                }
            }
            // Single printable character.
            let mut chars = tok.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) if !c.is_whitespace() => Some(KeyCode::Char(c)),
                _ => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_dark_menlo_10000() {
        let c = Config::default();
        assert_eq!(c.theme.name, "weft-dark");
        assert_eq!(c.font.family, "Menlo");
        assert_eq!(c.font.size, 14.0);
        assert_eq!(c.scrollback.lines, 10_000);
        assert_eq!(c.window.width, 800);
        // L4 window fields: opaque by default, no content padding.
        assert_eq!(c.window.opacity, 1.0);
        assert_eq!(c.window.padding_x, 0);
        assert_eq!(c.window.padding_y, 0);
    }

    #[test]
    fn empty_toml_uses_defaults() {
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.font.family, "Menlo");
        assert_eq!(c.theme.name, "weft-dark");
    }

    #[test]
    fn partial_config_merges_defaults() {
        let toml_text = r#"
[font]
size = 18.0

[theme]
name = "weft-light"
"#;
        let c: Config = toml::from_str(toml_text).unwrap();
        assert_eq!(c.font.size, 18.0);
        assert_eq!(c.font.family, "Menlo"); // default retained
        assert_eq!(c.theme.name, "weft-light");
        assert_eq!(c.scrollback.lines, 10_000); // default retained
    }

    #[test]
    fn parse_hex_rrggbb() {
        assert_eq!(parse_hex("#ff8800"), Some(Color::rgb(255, 136, 0)));
        assert_eq!(parse_hex("1a2b3c"), Some(Color::rgb(0x1a, 0x2b, 0x3c)));
    }

    #[test]
    fn parse_hex_short_and_alpha() {
        assert_eq!(parse_hex("#f0a"), Some(Color::rgb(255, 0, 170)));
        let c = parse_hex("#80808080").unwrap();
        assert_eq!((c.r, c.g, c.b, c.a), (0x80, 0x80, 0x80, 0x80));
    }

    #[test]
    fn parse_hex_rejects_garbage() {
        assert_eq!(parse_hex("nope"), None);
        assert_eq!(parse_hex("#12345"), None);
    }

    #[test]
    fn theme_resolve_applies_overrides() {
        let cfg = ThemeConfig {
            name: "weft-dark".into(),
            foreground: Some("#abcdef".into()),
            palette: vec!["#112233".into(), "#445566".into()],
            ..Default::default()
        };
        let theme = Theme::resolve(&cfg);
        assert_eq!(theme.foreground, Color::rgb(0xab, 0xcd, 0xef));
        assert_eq!(theme.palette[0], Color::rgb(0x11, 0x22, 0x33));
        assert_eq!(theme.palette[1], Color::rgb(0x44, 0x55, 0x66));
    }

    #[test]
    fn theme_unknown_name_falls_back_to_dark() {
        let cfg = ThemeConfig {
            name: "nonsense".into(),
            ..Default::default()
        };
        assert_eq!(Theme::resolve(&cfg), Theme::weft_dark());
    }

    #[test]
    fn dark_and_light_differ() {
        assert_ne!(Theme::weft_dark(), Theme::weft_light());
    }

    #[test]
    fn parse_binding_modifiers() {
        let (k, m) = parse_binding("cmd+shift+c").unwrap();
        assert_eq!(k, KeyCode::Char('c'));
        assert!(m.contains(Modifiers::SUPER));
        assert!(m.contains(Modifiers::SHIFT));
        assert!(!m.contains(Modifiers::CONTROL));
    }

    #[test]
    fn parse_binding_named_key() {
        let (k, _) = parse_binding("shift+page_up").unwrap();
        assert_eq!(k, KeyCode::PageUp);
        let (k, _) = parse_binding("cmd+comma").unwrap();
        assert_eq!(k, KeyCode::Char(','));
        let (k, _) = parse_binding("cmd+f5").unwrap();
        assert_eq!(k, KeyCode::F(5));
    }

    #[test]
    fn parse_binding_rejects_unknown() {
        assert!(parse_binding("win+foobar").is_none());
    }

    #[test]
    fn keybindings_default_has_copy_paste() {
        let kb = KeyBindings::default();
        let copy = kb.lookup(KeyCode::Char('c'), Modifiers::SUPER);
        assert_eq!(copy, Some(Action::Copy));
        assert_eq!(
            kb.lookup(KeyCode::Char('v'), Modifiers::SUPER),
            Some(Action::Paste)
        );
    }

    #[test]
    fn keybindings_overrides_merge() {
        let mut overrides = HashMap::new();
        overrides.insert("cmd+x".into(), Action::Copy);
        let kb = KeyBindings::from_overrides(&overrides);
        // Override present.
        assert_eq!(
            kb.lookup(KeyCode::Char('x'), Modifiers::SUPER),
            Some(Action::Copy)
        );
        // Default retained.
        assert_eq!(
            kb.lookup(KeyCode::Char('v'), Modifiers::SUPER),
            Some(Action::Paste)
        );
    }

    #[test]
    fn config_keybindings_resolve() {
        let toml_text = r#"
[keybindings]
"cmd+x" = "copy"
"#;
        let c: Config = toml::from_str(toml_text).unwrap();
        let kb = c.keybindings();
        assert_eq!(
            kb.lookup(KeyCode::Char('x'), Modifiers::SUPER),
            Some(Action::Copy)
        );
    }

    #[test]
    fn load_missing_file_is_default() {
        // HOME/XDG unset → no path → default. (Tests run with whatever env;
        // this asserts graceful handling when the path can't be resolved.)
        let c = Config::load();
        assert_eq!(c.font.family, "Menlo");
    }
}
