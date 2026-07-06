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
//! name = "weft-warm"            # weft-warm | weft-light (weft-dark = legacy alias for weft-warm)
//! foreground = "#e0d4c4"        # optional inline overrides
//! accent = "#d4a574"            # v0.8: signature accent (amber)
//! palette = ["#2a2420", "#c86858", ...]   # optional, overrides ANSI 0-15
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

/// Syntax-highlight color palette (9 colors). Theme-driven so every theme
/// can define its own command/flag/path/string colors; replaces the hardcoded
/// `syntax_color()` from renderer.rs v0.5. Conventions match the "Warm
/// Terminal" direction (v0.8 §0.3) but each theme fills its own values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyntaxColors {
    /// Command name (first word, or after `|` / `&&` / `;`).
    pub command: Color,
    /// A flag: `-x` / `--flag`.
    pub flag: Color,
    /// A filesystem path (any word containing `/`).
    pub path: Color,
    /// A quoted string (`"…"` / `'…'`), including the quotes.
    pub string: Color,
    /// A numeric literal (`^[+-]?\d+(\.\d+)?$`).
    pub number: Color,
    /// A variable reference: `$VAR` / `${VAR}`.
    pub variable: Color,
    /// A shell operator: `|` `>` `<` `>>` `&&` `||` `;` `&`.
    pub operator: Color,
    /// A shell comment: `#` to end of line.
    pub comment: Color,
    /// Anything else (arguments, values) — usually == theme.foreground.
    pub default: Color,
}

/// A fully-resolved theme: the colors the renderer paints with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Theme {
    pub foreground: Color,
    pub background: Color,
    pub cursor: Color,
    pub selection: Color,
    /// 256-color palette; slots 0-15 are the ANSI colors.
    pub palette: [Color; 256],
    /// Signature accent color — prompt `❯`, scrollbar thumb, cursor glow.
    /// Warm themes use amber (#d4a574); cool themes use blue/cyan.
    pub accent: Color,
    /// Dimmed accent — chevrons `▸▾`, completion hover, secondary chrome.
    /// Quieter than `accent`; the "Quiet" direction uses this for UI skeleton
    /// so the bones recede and text becomes the protagonist.
    pub accent_dim: Color,
    /// Block separator color. Subtle (low contrast) in the Quiet direction
    /// — blocks separate by whitespace first, the line is barely visible.
    pub separator: Color,
    /// Syntax-highlight palette (replaces hardcoded syntax_color in renderer).
    pub syntax: SyntaxColors,
}

impl Theme {
    /// Built-in dark theme — **weft-warm** (v0.8 default).
    ///
    /// Warm-toned (deep brown bg `#221c18` + amber accent `#d4a574`), the
    /// "Warm Terminal" direction (v0.8 §0.3). `weft_dark` is kept as a name
    /// alias for backward compatibility (old configs that say `weft-dark`).
    pub fn weft_dark() -> Self {
        Self::weft_warm()
    }

    /// v0.8 default theme: warm-toned dark. The weft visual identity.
    pub fn weft_warm() -> Self {
        let mut palette = Color::standard_palette();
        // Refined ANSI 0-15 — warm-tuned (softer than pure primaries, with a
        // slight amber bias to match the accent).
        let ansi = [
            (0x2a, 0x24, 0x20), // 0 black   (warm-tinted)
            (0xc8, 0x68, 0x58), // 1 red     (brick, not pure red)
            (0xb8, 0xc8, 0x78), // 2 green   (olive, not pure green)
            (0xd4, 0xa5, 0x43), // 3 yellow  (warm honey)
            (0xc4, 0xa0, 0xc8), // 4 blue    (dusty purple-blue for warmth)
            (0xd4, 0x88, 0x70), // 5 magenta (warm coral)
            (0xd4, 0xa5, 0x74), // 6 cyan    (amber — matches accent)
            (0xe0, 0xd4, 0xc4), // 7 white   (warm cream)
            (0x4a, 0x3f, 0x35), // 8 bright black (warm dark brown)
            (0xe0, 0x88, 0x78), // 9 bright red
            (0xd0, 0xe0, 0x90), // 10 bright green
            (0xe8, 0xc8, 0x70), // 11 bright yellow
            (0xd8, 0xc0, 0xe0), // 12 bright blue
            (0xe8, 0xa8, 0x90), // 13 bright magenta
            (0xe8, 0xc8, 0x9c), // 14 bright cyan
            (0xf0, 0xe8, 0xdc), // 15 bright white
        ];
        for (i, (r, g, b)) in ansi.iter().enumerate() {
            palette[i] = Color::rgb(*r, *g, *b);
        }
        Self {
            foreground: Color::rgb(0xe0, 0xd4, 0xc4), // warm cream
            background: Color::rgb(0x22, 0x1c, 0x18), // deep warm brown
            cursor: Color::rgb(0xf0, 0xd4, 0xa8),     // amber-tinted white (glow anchor)
            // Selection: warm amber tint at higher saturation than the
            // previous #4a3825 (which was nearly indistinguishable from
            // the #221c18 background — selection highlight was effectively
            // invisible). v0.8 user testing flagged this.
            selection: Color::rgb(0x8a, 0x5c, 0x28),
            palette,
            accent: Color::rgb(0xd4, 0xa5, 0x74), // amber — signature
            accent_dim: Color::rgb(0x7a, 0x6a, 0x58), // warm gray (chevrons, dim text)
            separator: Color::rgb(0x4a, 0x3f, 0x35), // barely-visible warm dark
            syntax: SyntaxColors {
                command: Color::rgb(0xb8, 0xc8, 0x78),  // olive
                flag: Color::rgb(0xd4, 0xa5, 0x74),     // amber (== accent)
                path: Color::rgb(0xc8, 0x98, 0x58),     // terracotta
                string: Color::rgb(0xd4, 0x88, 0x70),   // warm coral
                number: Color::rgb(0xd4, 0xa5, 0x43),   // warm yellow
                variable: Color::rgb(0xc4, 0xa0, 0xc8), // dusty purple
                operator: Color::rgb(0xc8, 0x68, 0x58), // brick red
                comment: Color::rgb(0x7a, 0x6a, 0x58),  // warm gray (== accent_dim)
                default: Color::rgb(0xe0, 0xd4, 0xc4),  // == foreground
            },
        }
    }

    /// Built-in light theme — warm-toned light variant of weft-warm.
    pub fn weft_light() -> Self {
        let mut palette = Color::standard_palette();
        let ansi = [
            (0x40, 0x38, 0x30), // 0 black
            (0xa8, 0x48, 0x38), // 1 red
            (0x6a, 0x80, 0x40), // 2 green
            (0xa8, 0x78, 0x20), // 3 yellow
            (0x80, 0x60, 0x90), // 4 blue
            (0xa8, 0x60, 0x50), // 5 magenta
            (0xa8, 0x78, 0x40), // 6 cyan (amber-ish)
            (0x3a, 0x32, 0x28), // 7 white (text)
            (0x80, 0x70, 0x60), // 8 bright black
            (0xc0, 0x58, 0x48), // 9 bright red
            (0x80, 0x98, 0x50), // 10 bright green
            (0xc0, 0x88, 0x30), // 11 bright yellow
            (0x98, 0x78, 0xa8), // 12 bright blue
            (0xc0, 0x78, 0x68), // 13 bright magenta
            (0xc0, 0x98, 0x50), // 14 bright cyan
            (0x28, 0x20, 0x18), // 15 bright white
        ];
        for (i, (r, g, b)) in ansi.iter().enumerate() {
            palette[i] = Color::rgb(*r, *g, *b);
        }
        Self {
            foreground: Color::rgb(0x3a, 0x32, 0x28), // warm dark brown text
            background: Color::rgb(0xf5, 0xf0, 0xe8), // warm cream-white
            cursor: Color::rgb(0x8a, 0x60, 0x30),     // warm amber-brown
            selection: Color::rgb(0xe0, 0xd0, 0xb8),  // warm tan
            palette,
            accent: Color::rgb(0xa8, 0x70, 0x30), // amber (darker for light bg)
            accent_dim: Color::rgb(0x8a, 0x78, 0x68), // warm gray
            separator: Color::rgb(0xd0, 0xc4, 0xb0), // warm light gray
            syntax: SyntaxColors {
                command: Color::rgb(0x5a, 0x78, 0x30),  // olive green
                flag: Color::rgb(0xa8, 0x70, 0x30),     // amber (== accent)
                path: Color::rgb(0x9a, 0x68, 0x20),     // terracotta
                string: Color::rgb(0xa8, 0x50, 0x40),   // warm coral
                number: Color::rgb(0x9a, 0x70, 0x20),   // warm yellow
                variable: Color::rgb(0x70, 0x50, 0x90), // dusty purple
                operator: Color::rgb(0xa8, 0x48, 0x38), // brick red
                comment: Color::rgb(0x8a, 0x78, 0x68),  // warm gray (== accent_dim)
                default: Color::rgb(0x3a, 0x32, 0x28),  // == foreground
            },
        }
    }

    /// Resolve a theme from config: pick the built-in base by `cfg.name`,
    /// then apply any inline hex overrides.
    ///
    /// Recognized names: `weft-warm` / `weft-dark` (alias) / `weft-light`.
    /// Unknown names fall back to `weft-warm` (the v0.8 default).
    pub fn resolve(cfg: &ThemeConfig) -> Self {
        Self::resolve_named(&cfg.name, cfg)
    }

    /// Resolve a theme by explicit name (v0.9 U-D1 — used by system-theme
    /// follow to pick light/dark by appearance, ignoring `cfg.name`).
    /// Applies the same inline overrides as [`resolve`].
    pub fn resolve_named(name: &str, cfg: &ThemeConfig) -> Self {
        let base = match name {
            "weft-light" => Self::weft_light(),
            // Both the v0.8 name and the legacy v0.7 name map to the warm
            // default — old configs that say `weft-dark` keep working but
            // now get the warm palette (the new visual identity).
            "weft-warm" | "weft-dark" | "weft_dark" => Self::weft_warm(),
            _ => Self::weft_warm(),
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
    #[serde(rename = "toggle_block_panel")]
    ToggleBlockPanel,
    #[serde(rename = "toggle_command_palette")]
    ToggleCommandPalette,
    /// Increase font size (Cmd+=). Multiplies the active font size by 1.1,
    /// clamped to 3× the configured base.
    #[serde(rename = "zoom_in")]
    ZoomIn,
    /// Decrease font size (Cmd+-). Divides the active font size by 1.1,
    /// clamped to 0.5× the configured base.
    #[serde(rename = "zoom_out")]
    ZoomOut,
    /// Reset font size to the configured base (Cmd+0).
    #[serde(rename = "zoom_reset")]
    ZoomReset,
    /// Open the in-grid search bar (Cmd+F). Typing debounces 150ms then
    /// scans visible content + recent scrollback for matches.
    #[serde(rename = "find_in_grid")]
    FindInGrid,
    /// Toggle between dark and light themes at runtime (Cmd+Shift+T).
    /// Independent of `ReloadConfig` (Cmd+Shift+,): reload re-reads the
    /// config file and resets the theme to whatever's named there, while
    /// ToggleTheme flips the in-memory `theme_is_dark` flag without
    /// touching disk.
    #[serde(rename = "toggle_theme")]
    ToggleTheme,
    /// Open a new tab (Cmd+T). Spawns a fresh shell session and switches
    /// to it.
    #[serde(rename = "new_tab")]
    NewTab,
    /// Close the current tab (Cmd+W). If this was the last tab, the app
    /// exits.
    #[serde(rename = "close_tab")]
    CloseTab,
    /// Switch to the next tab (Cmd+Shift+] or Cmd+Shift+Right).
    #[serde(rename = "next_tab")]
    NextTab,
    /// Switch to the previous tab (Cmd+Shift+[ or Cmd+Shift+Left).
    #[serde(rename = "prev_tab")]
    PrevTab,
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
            // v0.9 fix: Cmd+Shift+V also pastes (common terminal convention;
            // matches macOS "Paste and Match Style" habit).
            ("cmd+shift+v", Action::Paste),
            ("cmd+shift+comma", Action::ReloadConfig),
            ("shift+page_up", Action::ScrollPageUp),
            ("shift+page_down", Action::ScrollPageDown),
            ("cmd+home", Action::ScrollToTop),
            ("cmd+end", Action::ScrollToBottom),
            ("cmd+shift+b", Action::ToggleBlockPanel),
            ("cmd+p", Action::ToggleCommandPalette),
            ("cmd+equals", Action::ZoomIn),
            ("cmd+minus", Action::ZoomOut),
            ("cmd+0", Action::ZoomReset),
            ("cmd+f", Action::FindInGrid),
            ("cmd+shift+t", Action::ToggleTheme),
            // v0.9 H1: tab management shortcuts.
            ("cmd+t", Action::NewTab),
            ("cmd+w", Action::CloseTab),
            ("cmd+shift+right_bracket", Action::NextTab),
            ("cmd+shift+left_bracket", Action::PrevTab),
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
    pub editor: EditorConfig,
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
    /// v0.8: signature accent (prompt ❯, scrollbar thumb, cursor glow).
    pub accent: Option<String>,
    /// v0.8: dimmed accent (chevrons, completion hover, secondary chrome).
    pub accent_dim: Option<String>,
    /// v0.8: block separator color.
    pub separator: Option<String>,
    pub palette: Vec<String>,
    /// v0.9 U-D1: follow macOS system appearance (light/dark). When true,
    /// `light_name` / `dark_name` override `name` based on the current
    /// system appearance. Manual `Cmd+Shift+T` toggle is a no-op while
    /// this is enabled (the system overrides it on the next poll).
    pub follow_system: bool,
    /// v0.9 U-D1: theme name to use when system appearance is Light.
    /// Defaults to "weft-light" when None.
    pub light_name: Option<String>,
    /// v0.9 U-D1: theme name to use when system appearance is Dark.
    /// Defaults to "weft-warm" when None.
    pub dark_name: Option<String>,
}

// Manual Default (deriving would give name = "").
impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            name: "weft-warm".into(),
            foreground: None,
            background: None,
            cursor: None,
            selection: None,
            accent: None,
            accent_dim: None,
            separator: None,
            palette: Vec::new(),
            follow_system: false,
            light_name: None,
            dark_name: None,
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

/// Editor (input-box) options.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct EditorConfig {
    /// If true, `Ctrl+Enter` submits and plain `Enter` inserts a newline
    /// (Warp default). If false (default), `Enter` submits and `Shift+Enter`
    /// inserts a newline.
    pub submit_on_ctrl_enter: bool,
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
        "left_bracket" | "lbracket" => Some(KeyCode::Char('[')),
        "right_bracket" | "rbracket" => Some(KeyCode::Char(']')),
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
        assert_eq!(c.theme.name, "weft-warm");
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
        assert_eq!(c.theme.name, "weft-warm");
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
            name: "weft-dark".into(), // legacy alias → resolves to weft-warm
            foreground: Some("#abcdef".into()),
            palette: vec!["#112233".into(), "#445566".into()],
            ..Default::default()
        };
        let theme = Theme::resolve(&cfg);
        assert_eq!(theme.foreground, Color::rgb(0xab, 0xcd, 0xef));
        assert_eq!(theme.palette[0], Color::rgb(0x11, 0x22, 0x33));
        assert_eq!(theme.palette[1], Color::rgb(0x44, 0x55, 0x66));
        // v0.8: weft-dark alias resolves to weft-warm (amber accent #d4a574).
        assert_eq!(theme.accent, Color::rgb(0xd4, 0xa5, 0x74));
    }

    #[test]
    fn weft_warm_default_has_warm_palette() {
        // v0.8 visual identity: warm brown bg + amber accent.
        let t = Theme::weft_warm();
        assert_eq!(t.background, Color::rgb(0x22, 0x1c, 0x18)); // warm brown
        assert_eq!(t.accent, Color::rgb(0xd4, 0xa5, 0x74)); // amber
        assert_eq!(t.accent_dim, Color::rgb(0x7a, 0x6a, 0x58)); // warm gray
                                                                // Syntax: all 9 colors distinct from background.
        let bg = t.background;
        for c in [
            t.syntax.command,
            t.syntax.flag,
            t.syntax.path,
            t.syntax.string,
            t.syntax.number,
            t.syntax.variable,
            t.syntax.operator,
            t.syntax.comment,
            t.syntax.default,
        ] {
            assert_ne!(c, bg, "syntax color must differ from background");
        }
        // flag == accent (design intent: flags carry the signature color).
        assert_eq!(t.syntax.flag, t.accent);
        // comment == accent_dim (Quiet: comments recede like dim chrome).
        assert_eq!(t.syntax.comment, t.accent_dim);
        // default == foreground.
        assert_eq!(t.syntax.default, t.foreground);
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
        // v0.9 fix: Cmd+Shift+V also pastes.
        assert_eq!(
            kb.lookup(KeyCode::Char('v'), Modifiers::SUPER | Modifiers::SHIFT),
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

    #[test]
    fn editor_submit_on_ctrl_enter_parses() {
        let c: Config = toml::from_str("[editor]\nsubmit_on_ctrl_enter = true\n").unwrap();
        assert!(c.editor.submit_on_ctrl_enter);
        // default is false
        let d: Config = toml::from_str("").unwrap();
        assert!(!d.editor.submit_on_ctrl_enter);
    }

    #[test]
    fn zoom_actions_have_default_keybindings() {
        let kb = KeyBindings::default();
        // Cmd+= → ZoomIn, Cmd+- → ZoomOut, Cmd+0 → ZoomReset.
        assert_eq!(
            kb.lookup(KeyCode::Char('='), Modifiers::SUPER),
            Some(Action::ZoomIn)
        );
        assert_eq!(
            kb.lookup(KeyCode::Char('-'), Modifiers::SUPER),
            Some(Action::ZoomOut)
        );
        assert_eq!(
            kb.lookup(KeyCode::Char('0'), Modifiers::SUPER),
            Some(Action::ZoomReset)
        );
    }

    #[test]
    fn zoom_actions_serde_roundtrip() {
        // The serde rename must match what users would write in config.toml.
        // Action only derives Deserialize (config is read-only), so we
        // round-trip via a serde_json string (matching the rename attribute).
        for (action, name) in [
            (Action::ZoomIn, "zoom_in"),
            (Action::ZoomOut, "zoom_out"),
            (Action::ZoomReset, "zoom_reset"),
        ] {
            let s = format!("\"{name}\"");
            let back: Action = serde_json::from_str(&s).unwrap();
            assert_eq!(back, action, "serde roundtrip failed for {name}");
        }
    }
}
