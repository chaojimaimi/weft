// arch-gate: allow-over-800
// Config struct + impl Config + mod tests. Already extracted theme/action/
// keybindings/sections/save/parsers to submodules; remaining is the Config
// struct + save_to_path + tests (~1140 lines of tests).
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

mod action;
mod keybindings;
mod parsers;
mod save;
mod sections;
mod theme;

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

#[cfg(test)]
use crate::grid::Color;
#[cfg(test)]
use crate::input::{KeyCode, Modifiers};

pub use action::Action;
pub use keybindings::KeyBindings;
pub use parsers::{parse_binding, parse_hex};
pub use save::ConfigSaveError;
pub use sections::{
    EditorConfig, FontConfig, LogoConfig, LogoVariant, ScrollbackConfig, SyntaxConfig, ThemeConfig,
    WindowConfig, SIDEBAR_MAX_WIDTH, SIDEBAR_MIN_WIDTH,
};
pub use theme::{SyntaxColors, Theme};

use self::save::{
    parse_existing, set_f32_if_diff, set_opt_string, set_string_if_diff, set_u32_if_diff,
    set_usize_if_diff,
};

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
    pub logo: LogoConfig,
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
            Ok(text) => {
                let mut cfg: Config = toml::from_str(&text).unwrap_or_else(|e| {
                    tracing::warn!(path = %path.display(), error = %e, "failed to parse config; using defaults");
                    Self::default()
                });
                // F3-3: clamp user-provided sidebar_width to the valid range.
                if let Some(w) = cfg.window.sidebar_width {
                    cfg.window.sidebar_width = Some(w.clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH));
                }
                cfg
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "failed to read config; using defaults");
                Self::default()
            }
        }
    }

    /// v1.0 S2: Save config to disk, preserving comments and unknown fields.
    ///
    /// Uses `toml_edit` so existing comments and layout survive the write.
    /// Only writes fields that differ from their defaults — fields the user
    /// never customized are left untouched (or omitted if the file is new).
    /// Writes atomically: the new content is written to `<path>.tmp` first,
    /// then renamed over `<path>`.
    ///
    /// Returns an error when:
    ///   - the config path can't be resolved (`HOME`/`XDG_CONFIG_HOME` unset)
    ///   - the parent directory can't be created
    ///   - the temp-file write or rename fails
    pub fn save(&self) -> Result<(), ConfigSaveError> {
        let path = Self::config_path().ok_or(ConfigSaveError::NoConfigPath)?;
        self.save_to_path(&path)
    }

    /// v1.0 S2: Save config to an explicit path. Used by [`save`] and by
    /// tests (which pass a tempdir path).
    pub fn save_to_path(&self, path: &std::path::Path) -> Result<(), ConfigSaveError> {
        let mut doc: toml_edit::DocumentMut = match std::fs::read_to_string(path) {
            Ok(existing) => parse_existing(&existing)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Default::default(),
            Err(error) => return Err(ConfigSaveError::Io(error)),
        };

        // [font] section.
        let default_font = FontConfig::default();
        let font_entry = doc.entry("font").or_insert_with(toml_edit::table);
        if font_entry.is_none() {
            *font_entry = toml_edit::table();
        }
        let font = font_entry.as_table_mut().expect("font is a table");
        set_string_if_diff(font, "family", &self.font.family, &default_font.family);
        set_f32_if_diff(font, "size", self.font.size, default_font.size);
        set_string_if_diff(
            font,
            "cjk_family",
            &self.font.cjk_family,
            &default_font.cjk_family,
        );
        set_string_if_diff(
            font,
            "emoji_family",
            &self.font.emoji_family,
            &default_font.emoji_family,
        );
        set_f32_if_diff(
            font,
            "line_height",
            self.font.line_height,
            default_font.line_height,
        );

        // [theme] section.
        let default_theme = ThemeConfig::default();
        let theme_entry = doc.entry("theme").or_insert_with(toml_edit::table);
        if theme_entry.is_none() {
            *theme_entry = toml_edit::table();
        }
        let theme = theme_entry.as_table_mut().expect("theme is a table");
        set_string_if_diff(theme, "name", &self.theme.name, &default_theme.name);
        set_opt_string(theme, "foreground", &self.theme.foreground);
        set_opt_string(theme, "background", &self.theme.background);
        set_opt_string(theme, "cursor", &self.theme.cursor);
        set_opt_string(theme, "selection", &self.theme.selection);
        set_opt_string(theme, "accent", &self.theme.accent);
        set_opt_string(theme, "accent_dim", &self.theme.accent_dim);
        set_opt_string(theme, "separator", &self.theme.separator);
        // palette: only write if non-empty (non-default).
        if !self.theme.palette.is_empty() {
            let mut arr = toml_edit::Array::new();
            for hex in &self.theme.palette {
                arr.push(hex.as_str());
            }
            theme["palette"] = toml_edit::Item::Value(toml_edit::Value::Array(arr));
        }
        // follow_system: only write if non-default (true).
        if self.theme.follow_system {
            theme["follow_system"] = toml_edit::value(true);
        } else if theme.contains_key("follow_system") {
            theme["follow_system"] = toml_edit::value(false);
        }
        // light_name / dark_name: write if set.
        if let Some(ln) = &self.theme.light_name {
            theme["light_name"] = toml_edit::value(ln.as_str());
        }
        if let Some(dn) = &self.theme.dark_name {
            theme["dark_name"] = toml_edit::value(dn.as_str());
        }
        // [theme.syntax] subsection.
        if let Some(syn) = &self.theme.syntax {
            let mut syntax_table = toml_edit::table();
            let st = syntax_table.as_table_mut().unwrap();
            set_opt_string(st, "command", &syn.command);
            set_opt_string(st, "flag", &syn.flag);
            set_opt_string(st, "path", &syn.path);
            set_opt_string(st, "string", &syn.string);
            set_opt_string(st, "number", &syn.number);
            set_opt_string(st, "variable", &syn.variable);
            set_opt_string(st, "operator", &syn.operator);
            set_opt_string(st, "comment", &syn.comment);
            set_opt_string(st, "default", &syn.default);
            // Only write the [theme.syntax] table if at least one field is
            // set (avoid emitting an empty `[theme.syntax]` section).
            if st.iter().count() > 0 {
                theme["syntax"] = syntax_table;
            }
        }

        // [window] section.
        let default_window = WindowConfig::default();
        let window_entry = doc.entry("window").or_insert_with(toml_edit::table);
        if window_entry.is_none() {
            *window_entry = toml_edit::table();
        }
        let window = window_entry.as_table_mut().expect("window is a table");
        set_u32_if_diff(window, "width", self.window.width, default_window.width);
        set_u32_if_diff(window, "height", self.window.height, default_window.height);
        set_string_if_diff(window, "title", &self.window.title, &default_window.title);
        set_f32_if_diff(
            window,
            "opacity",
            self.window.opacity,
            default_window.opacity,
        );
        set_u32_if_diff(
            window,
            "padding_x",
            self.window.padding_x,
            default_window.padding_x,
        );
        set_u32_if_diff(
            window,
            "padding_y",
            self.window.padding_y,
            default_window.padding_y,
        );
        // F3-3: sidebar_width (Option<f32>, logical points). Only write when
        // Some (user has dragged the sidebar). When None, remove any stale
        // entry so the responsive default takes effect on reload.
        match self.window.sidebar_width {
            Some(w) => {
                window["sidebar_width"] = toml_edit::value(f64::from(w));
            }
            None => {
                if window.contains_key("sidebar_width") {
                    window.remove("sidebar_width");
                }
            }
        }

        // [scrollback] section.
        let default_scrollback = ScrollbackConfig::default();
        let scrollback_entry = doc.entry("scrollback").or_insert_with(toml_edit::table);
        if scrollback_entry.is_none() {
            *scrollback_entry = toml_edit::table();
        }
        let scrollback = scrollback_entry
            .as_table_mut()
            .expect("scrollback is a table");
        set_usize_if_diff(
            scrollback,
            "lines",
            self.scrollback.lines,
            default_scrollback.lines,
        );

        if self.editor.submit_on_ctrl_enter {
            let editor_entry = doc.entry("editor").or_insert_with(toml_edit::table);
            if editor_entry.is_none() {
                *editor_entry = toml_edit::table();
            }
            let editor = editor_entry.as_table_mut().expect("editor is a table");
            editor["submit_on_ctrl_enter"] = toml_edit::value(true);
        } else if let Some(editor) = doc.get_mut("editor").and_then(|item| item.as_table_mut()) {
            editor.remove("submit_on_ctrl_enter");
        }

        // [logo] section — write variant when non-default; clear it when
        // default so a later switch back to Cool doesn't get overridden by
        // a stale `variant = "warm"` left in the file.
        let default_logo = LogoConfig::default();
        if self.logo.variant != default_logo.variant {
            let logo_entry = doc.entry("logo").or_insert_with(toml_edit::table);
            if logo_entry.is_none() {
                *logo_entry = toml_edit::table();
            }
            let logo = logo_entry.as_table_mut().expect("logo is a table");
            logo["variant"] = toml_edit::value(self.logo.variant.as_str());
        } else if let Some(logo_entry) = doc.get_mut("logo") {
            // Default variant: remove any stale `variant` key so a saved
            // non-default value doesn't override the default on next load.
            if let Some(logo) = logo_entry.as_table_mut() {
                logo.remove("variant");
                // If the [logo] table is now empty, remove it entirely to
                // keep the config file clean.
                if logo.iter().count() == 0 {
                    doc.remove("logo");
                }
            }
        }

        // [keybindings] section.
        if !self.keybindings.is_empty() {
            let mut kb_table = toml_edit::table();
            let kt = kb_table.as_table_mut().unwrap();
            for (binding, action) in &self.keybindings {
                let action_str = match action {
                    Action::Copy => "copy",
                    Action::Paste => "paste",
                    Action::ReloadConfig => "reload_config",
                    Action::ScrollPageUp => "scroll_page_up",
                    Action::ScrollPageDown => "scroll_page_down",
                    Action::ScrollLineUp => "scroll_line_up",
                    Action::ScrollLineDown => "scroll_line_down",
                    Action::ScrollToTop => "scroll_to_top",
                    Action::ScrollToBottom => "scroll_to_bottom",
                    Action::ToggleBlockPanel => "toggle_block_panel",
                    Action::ToggleCommandPalette => "toggle_command_palette",
                    Action::ZoomIn => "zoom_in",
                    Action::ZoomOut => "zoom_out",
                    Action::ZoomReset => "zoom_reset",
                    Action::FindInGrid => "find_in_grid",
                    Action::ToggleTheme => "toggle_theme",
                    Action::NewTab => "new_tab",
                    Action::CloseTab => "close_tab",
                    Action::NextTab => "next_tab",
                    Action::PrevTab => "prev_tab",
                    Action::ToggleSettings => "toggle_settings",
                };
                kt.insert(binding, toml_edit::value(action_str));
            }
            doc["keybindings"] = kb_table;
        }

        // Atomic write: <path>.tmp → rename → <path>.
        let parent = path.parent().ok_or(ConfigSaveError::NoParentDir)?;
        std::fs::create_dir_all(parent).map_err(ConfigSaveError::Io)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, doc.to_string()).map_err(ConfigSaveError::Io)?;
        std::fs::rename(&tmp, path).map_err(ConfigSaveError::Io)?;
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Generates a unique temp path for a test artifact. Combines a tag,
    /// process id, monotonic counter, and nanosecond timestamp so parallel
    /// tests (even within the same process) never collide on the same file
    /// or directory name.
    fn temp_path(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "weft-config-{tag}-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::UNIX_EPOCH
                .elapsed()
                .unwrap_or_default()
                .as_nanos(),
            id
        ))
    }

    /// Serializes tests that mutate `XDG_CONFIG_HOME` — env vars are
    /// process-global, so parallel tests that touch the same var would
    /// clobber each other's values. The original value is saved and
    /// restored around `f` to keep tests hermetic.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_xdg_config<F: FnOnce(&std::path::Path)>(tmp: &std::path::Path, f: F) {
        let _guard = ENV_LOCK.lock().unwrap();
        let old = std::env::var_os("XDG_CONFIG_HOME");
        std::fs::create_dir_all(tmp).unwrap();
        std::env::set_var("XDG_CONFIG_HOME", tmp);
        f(tmp);
        match old {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
    }

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
    fn warp_theme_resolves_by_name() {
        // v0.9 W2+: "warp" and "warp-dark" both resolve to the Warp dark theme.
        let cfg = ThemeConfig {
            name: "warp".into(),
            ..Default::default()
        };
        let theme = Theme::resolve(&cfg);
        assert_eq!(theme, Theme::warp_dark());
        // Also test the dashed alias.
        let cfg2 = ThemeConfig {
            name: "warp-dark".into(),
            ..Default::default()
        };
        assert_eq!(Theme::resolve(&cfg2), Theme::warp_dark());
    }

    #[test]
    fn warp_theme_has_coral_accent_and_distinct_syntax() {
        // v0.9 W2+: Warp's signature coral accent + 9 distinct syntax colors.
        let t = Theme::warp_dark();
        // Coral accent (#ff5d38) is the Warp signature.
        assert_eq!(t.accent, Color::rgb(0xff, 0x5d, 0x38));
        // flag == accent (consistent with weft_warm's design intent).
        assert_eq!(t.syntax.flag, t.accent);
        // default == foreground.
        assert_eq!(t.syntax.default, t.foreground);
        // All 9 syntax colors distinct from background (must be visible).
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
    }

    #[test]
    fn warp_theme_supports_inline_overrides() {
        // v0.9 W2+: inline overrides apply on top of the warp base, just like
        // weft-warm. Verifies resolve_named() applies cfg overrides to warp.
        let cfg = ThemeConfig {
            name: "warp".into(),
            accent: Some("#00ff00".into()),
            ..Default::default()
        };
        let theme = Theme::resolve(&cfg);
        assert_eq!(theme.accent, Color::rgb(0x00, 0xff, 0x00));
        // Background is still the warp default (override only touched accent).
        assert_eq!(theme.background, Color::rgb(0x1b, 0x1b, 0x28));
    }

    #[test]
    fn classic_themes_resolve_by_name() {
        // v0.9 W2+: Dracula / Solarized Dark / Gruvbox Dark resolve by name
        // and match their constructors.
        for (name, expected) in [
            ("dracula", Theme::dracula()),
            ("solarized-dark", Theme::solarized_dark()),
            ("solarized_dark", Theme::solarized_dark()),
            ("solarized", Theme::solarized_dark()),
            ("gruvbox-dark", Theme::gruvbox_dark()),
            ("gruvbox_dark", Theme::gruvbox_dark()),
            ("gruvbox", Theme::gruvbox_dark()),
        ] {
            let cfg = ThemeConfig {
                name: name.into(),
                ..Default::default()
            };
            assert_eq!(Theme::resolve(&cfg), expected, "name = {name}");
        }
    }

    #[test]
    fn classic_themes_have_distinct_accent_and_visible_syntax() {
        // v0.9 W2+: each classic theme has a signature accent and all syntax
        // colors differ from the background (must be visible).
        for (name, theme) in [
            ("dracula", Theme::dracula()),
            ("solarized-dark", Theme::solarized_dark()),
            ("gruvbox-dark", Theme::gruvbox_dark()),
        ] {
            // accent must differ from background (otherwise it's invisible).
            assert_ne!(
                theme.accent, theme.background,
                "{name}: accent must differ from background"
            );
            // every syntax color must differ from background.
            let bg = theme.background;
            for c in [
                theme.syntax.command,
                theme.syntax.flag,
                theme.syntax.path,
                theme.syntax.string,
                theme.syntax.number,
                theme.syntax.variable,
                theme.syntax.operator,
                theme.syntax.comment,
                theme.syntax.default,
            ] {
                assert_ne!(
                    c, bg,
                    "{name}: syntax color {c:?} must differ from background"
                );
            }
        }
    }

    #[test]
    fn dracula_signature_colors() {
        // v0.9 W2+: verify Dracula's signature palette values.
        let t = Theme::dracula();
        assert_eq!(t.background, Color::rgb(0x28, 0x2a, 0x36));
        assert_eq!(t.foreground, Color::rgb(0xf8, 0xf8, 0xf2));
        assert_eq!(t.accent, Color::rgb(0xbd, 0x93, 0xf9)); // purple
        assert_eq!(t.syntax.flag, Color::rgb(0xff, 0x79, 0xc6)); // pink
    }

    #[test]
    fn solarized_signature_colors() {
        // v0.9 W2+: verify Solarized Dark's signature base03/base1/blue accent.
        let t = Theme::solarized_dark();
        assert_eq!(t.background, Color::rgb(0x00, 0x2b, 0x36)); // base03
        assert_eq!(t.foreground, Color::rgb(0x93, 0xa1, 0xa1)); // base1
        assert_eq!(t.accent, Color::rgb(0x26, 0x8b, 0xd2)); // blue
    }

    #[test]
    fn gruvbox_signature_colors() {
        // v0.9 W2+: verify Gruvbox Dark's signature bg/fg/orange accent.
        let t = Theme::gruvbox_dark();
        assert_eq!(t.background, Color::rgb(0x28, 0x28, 0x28));
        assert_eq!(t.foreground, Color::rgb(0xeb, 0xdb, 0xb2));
        assert_eq!(t.accent, Color::rgb(0xfe, 0x80, 0x19)); // orange
    }

    #[test]
    fn community_themes_resolve_by_name() {
        // v0.9 W2+: Nord / Tokyo Night / Catppuccin / One Dark / Monokai Pro.
        for (name, expected) in [
            ("nord", Theme::nord()),
            ("tokyo-night", Theme::tokyo_night()),
            ("tokyo_night", Theme::tokyo_night()),
            ("catppuccin", Theme::catppuccin_mocha()),
            ("catppuccin-mocha", Theme::catppuccin_mocha()),
            ("one-dark", Theme::one_dark()),
            ("one_dark", Theme::one_dark()),
            ("onedark", Theme::one_dark()),
            ("monokai-pro", Theme::monokai_pro()),
            ("monokai_pro", Theme::monokai_pro()),
            ("monokai", Theme::monokai_pro()),
        ] {
            let cfg = ThemeConfig {
                name: name.into(),
                ..Default::default()
            };
            assert_eq!(Theme::resolve(&cfg), expected, "name = {name}");
        }
    }

    #[test]
    fn community_themes_have_visible_syntax() {
        // v0.9 W2+: each community theme has accent != bg and all syntax
        // colors differ from background.
        for (name, theme) in [
            ("nord", Theme::nord()),
            ("tokyo-night", Theme::tokyo_night()),
            ("catppuccin", Theme::catppuccin_mocha()),
            ("one-dark", Theme::one_dark()),
            ("monokai-pro", Theme::monokai_pro()),
        ] {
            assert_ne!(
                theme.accent, theme.background,
                "{name}: accent must differ from background"
            );
            let bg = theme.background;
            for c in [
                theme.syntax.command,
                theme.syntax.flag,
                theme.syntax.path,
                theme.syntax.string,
                theme.syntax.number,
                theme.syntax.variable,
                theme.syntax.operator,
                theme.syntax.comment,
                theme.syntax.default,
            ] {
                assert_ne!(c, bg, "{name}: syntax color must differ from background");
            }
        }
    }

    #[test]
    fn nord_signature_colors() {
        let t = Theme::nord();
        assert_eq!(t.background, Color::rgb(0x2e, 0x34, 0x40)); // nord0
        assert_eq!(t.foreground, Color::rgb(0xd8, 0xde, 0xe9)); // nord4
        assert_eq!(t.accent, Color::rgb(0x88, 0xc0, 0xd0)); // nord8 frost
    }

    #[test]
    fn tokyo_night_signature_colors() {
        let t = Theme::tokyo_night();
        assert_eq!(t.background, Color::rgb(0x1a, 0x1b, 0x26));
        assert_eq!(t.foreground, Color::rgb(0xa9, 0xb1, 0xd6));
        assert_eq!(t.accent, Color::rgb(0x7a, 0xa2, 0xf7)); // blue
    }

    #[test]
    fn catppuccin_signature_colors() {
        let t = Theme::catppuccin_mocha();
        assert_eq!(t.background, Color::rgb(0x1e, 0x1e, 0x2e)); // base
        assert_eq!(t.foreground, Color::rgb(0xcd, 0xd6, 0xf4)); // text
        assert_eq!(t.accent, Color::rgb(0xcb, 0xa6, 0xf7)); // mauve
    }

    #[test]
    fn one_dark_signature_colors() {
        let t = Theme::one_dark();
        assert_eq!(t.background, Color::rgb(0x28, 0x2c, 0x34));
        assert_eq!(t.foreground, Color::rgb(0xab, 0xb2, 0xbf));
        assert_eq!(t.accent, Color::rgb(0x61, 0xaf, 0xef)); // blue
    }

    #[test]
    fn monokai_pro_signature_colors() {
        let t = Theme::monokai_pro();
        assert_eq!(t.background, Color::rgb(0x2d, 0x2a, 0x2e));
        assert_eq!(t.foreground, Color::rgb(0xfc, 0xfc, 0xfa));
        assert_eq!(t.accent, Color::rgb(0xff, 0xd8, 0x66)); // yellow
    }

    #[test]
    fn load_theme_from_toml_file() {
        // v0.9 W2+: load a custom theme from a .toml file. Uses a unique
        // temp dir (process id + counter) to avoid parallel-test collisions.
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("weft-theme-test-{id}-toml"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("custom.toml"),
            "foreground = \"#abcdef\"\nbackground = \"#112233\"\naccent = \"#ff0000\"\n",
        )
        .unwrap();

        let theme = Theme::load_from_dir(&dir, "custom").expect("should load custom.toml");
        assert_eq!(theme.foreground, Color::rgb(0xab, 0xcd, 0xef));
        assert_eq!(theme.background, Color::rgb(0x11, 0x22, 0x33));
        assert_eq!(theme.accent, Color::rgb(0xff, 0x00, 0x00));
        // Unspecified fields (cursor, syntax, palette) inherit from weft_warm base.
        let warm = Theme::weft_warm();
        assert_eq!(theme.cursor, warm.cursor);
        assert_eq!(theme.syntax.command, warm.syntax.command);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_theme_from_yaml_file() {
        // v0.9 W2+: load a custom theme from a .yaml file.
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("weft-theme-test-{id}-yaml"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("custom.yaml"),
            "foreground: \"#abcdef\"\nbackground: \"#112233\"\naccent: \"#ff0000\"\n",
        )
        .unwrap();

        let theme = Theme::load_from_dir(&dir, "custom").expect("should load custom.yaml");
        assert_eq!(theme.foreground, Color::rgb(0xab, 0xcd, 0xef));
        assert_eq!(theme.background, Color::rgb(0x11, 0x22, 0x33));
        assert_eq!(theme.accent, Color::rgb(0xff, 0x00, 0x00));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_theme_missing_file_returns_none() {
        // v0.9 W2+: when no file matches, returns None (falls back to default).
        let dir = temp_path("theme-nonexistent");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let result = Theme::load_from_dir(&dir, "does-not-exist");
        assert!(result.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_theme_with_palette_override() {
        // v0.9 W2+: palette array in theme file overrides ANSI slots.
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("weft-theme-test-{id}-palette"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("pal.toml"),
            "palette = [\"#000000\", \"#ff0000\", \"#00ff00\"]\n",
        )
        .unwrap();

        let theme = Theme::load_from_dir(&dir, "pal").expect("should load pal.toml");
        assert_eq!(theme.palette[0], Color::rgb(0x00, 0x00, 0x00));
        assert_eq!(theme.palette[1], Color::rgb(0xff, 0x00, 0x00));
        assert_eq!(theme.palette[2], Color::rgb(0x00, 0xff, 0x00));

        let _ = std::fs::remove_dir_all(&dir);
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
        // Point XDG_CONFIG_HOME to a non-existent directory so Config::load()
        // cannot find a user config file, guaranteeing we hit the default path.
        // Without this isolation the test picks up the developer's real
        // config.toml and fails on the font-family assertion.
        // `with_xdg_config` serializes env mutation and restores the original
        // value so parallel tests don't clobber each other's XDG_CONFIG_HOME.
        let tmp = temp_path("load-missing");
        with_xdg_config(&tmp, |_| {
            let c = Config::load();
            assert_eq!(c.font.family, "Menlo");
        });
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

    // ── v1.0 S5: [theme.syntax] config override tests ─────────────────

    #[test]
    fn syntax_config_default_is_all_none() {
        let s = SyntaxConfig::default();
        assert!(s.command.is_none());
        assert!(s.flag.is_none());
        assert!(s.path.is_none());
        assert!(s.string.is_none());
        assert!(s.number.is_none());
        assert!(s.variable.is_none());
        assert!(s.operator.is_none());
        assert!(s.comment.is_none());
        assert!(s.default.is_none());
    }

    #[test]
    fn theme_config_syntax_defaults_none() {
        // ThemeConfig::default() should leave syntax as None (no overrides).
        let cfg = ThemeConfig::default();
        assert!(cfg.syntax.is_none());
    }

    #[test]
    fn syntax_override_parses_from_toml() {
        let toml_text = r##"
[theme]
name = "weft-warm"

[theme.syntax]
command = "#ff0000"
flag = "#00ff00"
path = "#0000ff"
"##;
        let c: Config = toml::from_str(toml_text).unwrap();
        let syn = c.theme.syntax.expect("syntax section should parse");
        assert_eq!(syn.command.as_deref(), Some("#ff0000"));
        assert_eq!(syn.flag.as_deref(), Some("#00ff00"));
        assert_eq!(syn.path.as_deref(), Some("#0000ff"));
        // Unspecified fields are None.
        assert!(syn.string.is_none());
        assert!(syn.number.is_none());
        assert!(syn.comment.is_none());
    }

    #[test]
    fn syntax_override_applies_to_resolved_theme() {
        // v1.0 S5: [theme.syntax] fields override the base theme's
        // SyntaxColors. Specified fields change; unspecified fields retain
        // the base theme's value.
        let warm = Theme::weft_warm();
        let cfg = ThemeConfig {
            name: "weft-warm".into(),
            syntax: Some(SyntaxConfig {
                command: Some("#ff0000".into()),
                flag: Some("#00ff00".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let theme = Theme::resolve(&cfg);
        // Overridden fields.
        assert_eq!(theme.syntax.command, Color::rgb(0xff, 0x00, 0x00));
        assert_eq!(theme.syntax.flag, Color::rgb(0x00, 0xff, 0x00));
        // Unspecified fields inherit from the base theme.
        assert_eq!(theme.syntax.path, warm.syntax.path);
        assert_eq!(theme.syntax.string, warm.syntax.string);
        assert_eq!(theme.syntax.comment, warm.syntax.comment);
        assert_eq!(theme.syntax.default, warm.syntax.default);
    }

    #[test]
    fn syntax_override_all_nine_fields() {
        let cfg = ThemeConfig {
            name: "weft-warm".into(),
            syntax: Some(SyntaxConfig {
                command: Some("#111111".into()),
                flag: Some("#222222".into()),
                path: Some("#333333".into()),
                string: Some("#444444".into()),
                number: Some("#555555".into()),
                variable: Some("#666666".into()),
                operator: Some("#777777".into()),
                comment: Some("#888888".into()),
                default: Some("#999999".into()),
            }),
            ..Default::default()
        };
        let theme = Theme::resolve(&cfg);
        assert_eq!(theme.syntax.command, Color::rgb(0x11, 0x11, 0x11));
        assert_eq!(theme.syntax.flag, Color::rgb(0x22, 0x22, 0x22));
        assert_eq!(theme.syntax.path, Color::rgb(0x33, 0x33, 0x33));
        assert_eq!(theme.syntax.string, Color::rgb(0x44, 0x44, 0x44));
        assert_eq!(theme.syntax.number, Color::rgb(0x55, 0x55, 0x55));
        assert_eq!(theme.syntax.variable, Color::rgb(0x66, 0x66, 0x66));
        assert_eq!(theme.syntax.operator, Color::rgb(0x77, 0x77, 0x77));
        assert_eq!(theme.syntax.comment, Color::rgb(0x88, 0x88, 0x88));
        assert_eq!(theme.syntax.default, Color::rgb(0x99, 0x99, 0x99));
    }

    #[test]
    fn syntax_override_works_with_inline_color_override() {
        // v1.0 S5: [theme.syntax] composes with inline color overrides —
        // both apply, and they touch independent fields.
        let cfg = ThemeConfig {
            name: "weft-warm".into(),
            accent: Some("#abcdef".into()),
            syntax: Some(SyntaxConfig {
                command: Some("#ff0000".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let theme = Theme::resolve(&cfg);
        // Inline accent override.
        assert_eq!(theme.accent, Color::rgb(0xab, 0xcd, 0xef));
        // Syntax override.
        assert_eq!(theme.syntax.command, Color::rgb(0xff, 0x00, 0x00));
        // Unspecified syntax fields inherit base.
        let warm = Theme::weft_warm();
        assert_eq!(theme.syntax.flag, warm.syntax.flag);
    }

    #[test]
    fn syntax_override_invalid_hex_is_silently_ignored() {
        // v1.0 S5: an invalid hex string is skipped (parse_hex returns None),
        // leaving the base theme's value intact. This matches the behavior of
        // the existing inline color overrides.
        let warm = Theme::weft_warm();
        let cfg = ThemeConfig {
            name: "weft-warm".into(),
            syntax: Some(SyntaxConfig {
                command: Some("not-a-color".into()),
                flag: Some("#00ff00".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let theme = Theme::resolve(&cfg);
        // Invalid command override → retained from base.
        assert_eq!(theme.syntax.command, warm.syntax.command);
        // Valid flag override → applied.
        assert_eq!(theme.syntax.flag, Color::rgb(0x00, 0xff, 0x00));
    }

    #[test]
    fn syntax_override_applies_to_all_themes() {
        // v1.0 S5: syntax overrides apply regardless of the base theme name.
        // Verify with warp_dark.
        let warp = Theme::warp_dark();
        let cfg = ThemeConfig {
            name: "warp".into(),
            syntax: Some(SyntaxConfig {
                command: Some("#abcdef".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let theme = Theme::resolve(&cfg);
        assert_eq!(theme.syntax.command, Color::rgb(0xab, 0xcd, 0xef));
        // Unspecified fields inherit from warp base.
        assert_eq!(theme.syntax.flag, warp.syntax.flag);
        assert_eq!(theme.syntax.path, warp.syntax.path);
    }

    // ── v1.0 S2: Config::save() tests ──────────────────────────────────

    fn unique_tmp_path(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("weft-config-save-{pid}-{id}-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("config.toml")
    }

    #[test]
    fn save_default_config_creates_empty_file() {
        // Saving a default Config should produce a parseable file (with no
        // sections, since nothing differs from defaults).
        let path = unique_tmp_path("default");
        let cfg = Config::default();
        cfg.save_to_path(&path).expect("save should succeed");
        let text = std::fs::read_to_string(&path).unwrap();
        // Default config writes nothing (all fields match defaults).
        // The file should be valid TOML (possibly empty).
        let reloaded: Config = toml::from_str(&text).unwrap();
        assert_eq!(reloaded.font.family, "Menlo");
        assert_eq!(reloaded.theme.name, "weft-warm");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn save_and_reload_roundtrip() {
        // A Config with non-default fields should survive a save → reload
        // cycle.
        let path = unique_tmp_path("roundtrip");
        let cfg = Config {
            font: FontConfig {
                family: "Monaco".into(),
                size: 16.0,
                ..Default::default()
            },
            theme: ThemeConfig {
                name: "warp".into(),
                accent: Some("#ff0000".into()),
                syntax: Some(SyntaxConfig {
                    command: Some("#abcdef".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            window: WindowConfig {
                width: 1024,
                height: 768,
                ..Default::default()
            },
            scrollback: ScrollbackConfig { lines: 50_000 },
            ..Default::default()
        };
        cfg.save_to_path(&path).expect("save should succeed");
        let text = std::fs::read_to_string(&path).unwrap();
        let reloaded: Config = toml::from_str(&text).unwrap();
        assert_eq!(reloaded.font.family, "Monaco");
        assert_eq!(reloaded.font.size, 16.0);
        assert_eq!(reloaded.theme.name, "warp");
        assert_eq!(reloaded.theme.accent.as_deref(), Some("#ff0000"));
        assert_eq!(
            reloaded.theme.syntax.as_ref().unwrap().command.as_deref(),
            Some("#abcdef")
        );
        assert_eq!(reloaded.window.width, 1024);
        assert_eq!(reloaded.window.height, 768);
        assert_eq!(reloaded.scrollback.lines, 50_000);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn save_preserves_user_comments() {
        // v1.0 S2: toml_edit preserves comments. Write a file with a comment,
        // save over it, and verify the comment survives.
        let path = unique_tmp_path("comments");
        std::fs::write(
            &path,
            "# this is my comment\n[font]\nfamily = \"Menlo\"\nsize = 14.0\n",
        )
        .unwrap();
        let cfg = Config {
            font: FontConfig {
                size: 18.0,
                ..Default::default()
            },
            ..Default::default()
        };
        cfg.save_to_path(&path).expect("save should succeed");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("# this is my comment"),
            "comment should be preserved: {text}"
        );
        assert!(text.contains("size = 18"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn save_preserves_unknown_fields() {
        // v1.0 S2: unknown fields the user added should survive the save.
        let path = unique_tmp_path("unknown");
        std::fs::write(
            &path,
            "[font]\nsize = 14.0\n\n[unknown_section]\nfoo = \"bar\"\n",
        )
        .unwrap();
        let cfg = Config::default();
        cfg.save_to_path(&path).expect("save should succeed");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("foo = \"bar\""),
            "unknown field should survive"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn save_only_writes_non_default_fields() {
        // v1.0 S2: fields that match defaults shouldn't appear in the output
        // (keeps the file clean for a fresh save).
        let path = unique_tmp_path("nondefault");
        let cfg = Config {
            font: FontConfig {
                size: 18.0,
                ..Default::default()
            },
            ..Default::default()
        };
        cfg.save_to_path(&path).expect("save should succeed");
        let text = std::fs::read_to_string(&path).unwrap();
        // size should be written (non-default).
        assert!(text.contains("size = 18"));
        // family should NOT be written (matches default "Menlo").
        assert!(
            !text.contains("family = \"Menlo\""),
            "default family should not be written"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn save_creates_parent_dir() {
        // v1.0 S2: save should create the parent directory if it doesn't
        // exist.
        let dir =
            std::env::temp_dir().join(format!("weft-config-save-nested-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let nested = dir.join("a/b/c/config.toml");
        let cfg = Config::default();
        cfg.save_to_path(&nested).expect("save should succeed");
        assert!(nested.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_error_display() {
        // v1.0 S2: ConfigSaveError should have a useful Display impl.
        let e = ConfigSaveError::NoConfigPath;
        assert!(format!("{e}").contains("HOME"));
        let e = ConfigSaveError::NoParentDir;
        assert!(format!("{e}").contains("parent"));
        let e = ConfigSaveError::Io(std::io::Error::from_raw_os_error(13));
        assert!(format!("{e}").contains("config save failed"));
    }

    #[test]
    fn toggle_settings_has_default_keybinding() {
        // v1.0 S1: Cmd+, opens Settings.
        let kb = KeyBindings::default();
        assert_eq!(
            kb.lookup(KeyCode::Char(','), Modifiers::SUPER),
            Some(Action::ToggleSettings)
        );
    }

    #[test]
    fn toggle_settings_serde_roundtrip() {
        let s = "\"toggle_settings\"";
        let back: Action = serde_json::from_str(s).unwrap();
        assert_eq!(back, Action::ToggleSettings);
    }

    // ── T2: per-field Config::save roundtrip tests ────────────────────

    #[test]
    fn save_font_size_change() {
        let path = unique_tmp_path("font-size");
        let cfg = Config {
            font: FontConfig {
                size: 13.0,
                ..Default::default()
            },
            ..Default::default()
        };
        cfg.save_to_path(&path).expect("save should succeed");
        let text = std::fs::read_to_string(&path).unwrap();
        let reloaded: Config = toml::from_str(&text).unwrap();
        assert_eq!(reloaded.font.size, 13.0);
        // Other fields retain defaults.
        assert_eq!(reloaded.font.family, "Menlo");
        assert_eq!(reloaded.theme.name, "weft-warm");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn save_window_opacity_change() {
        let path = unique_tmp_path("win-opacity");
        let cfg = Config {
            window: WindowConfig {
                opacity: 0.85,
                ..Default::default()
            },
            ..Default::default()
        };
        cfg.save_to_path(&path).expect("save should succeed");
        let text = std::fs::read_to_string(&path).unwrap();
        let reloaded: Config = toml::from_str(&text).unwrap();
        // f32 round-trip through TOML f64 — compare with small epsilon.
        assert!((reloaded.window.opacity - 0.85).abs() < 1e-6);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn save_theme_name_change() {
        let path = unique_tmp_path("theme-name");
        let cfg = Config {
            theme: ThemeConfig {
                name: "dracula".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        cfg.save_to_path(&path).expect("save should succeed");
        let text = std::fs::read_to_string(&path).unwrap();
        let reloaded: Config = toml::from_str(&text).unwrap();
        assert_eq!(reloaded.theme.name, "dracula");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn save_scrollback_lines_change() {
        let path = unique_tmp_path("scrollback");
        let cfg = Config {
            scrollback: ScrollbackConfig { lines: 25_000 },
            ..Default::default()
        };
        cfg.save_to_path(&path).expect("save should succeed");
        let text = std::fs::read_to_string(&path).unwrap();
        let reloaded: Config = toml::from_str(&text).unwrap();
        assert_eq!(reloaded.scrollback.lines, 25_000);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn save_preserves_other_fields_when_one_changes() {
        // Changing only window.width should leave font, theme, and scrollback
        // at their configured (non-default) values after a save + reload cycle.
        let path = unique_tmp_path("preserve");
        let cfg = Config {
            font: FontConfig {
                family: "Monaco".into(),
                size: 16.0,
                ..Default::default()
            },
            theme: ThemeConfig {
                name: "nord".into(),
                ..Default::default()
            },
            window: WindowConfig {
                width: 1200,
                ..Default::default()
            },
            scrollback: ScrollbackConfig { lines: 50_000 },
            ..Default::default()
        };
        cfg.save_to_path(&path).expect("save should succeed");
        let text = std::fs::read_to_string(&path).unwrap();
        let reloaded: Config = toml::from_str(&text).unwrap();
        // The field we changed.
        assert_eq!(reloaded.window.width, 1200);
        // Other configured fields must survive unchanged.
        assert_eq!(reloaded.font.family, "Monaco");
        assert_eq!(reloaded.font.size, 16.0);
        assert_eq!(reloaded.theme.name, "nord");
        assert_eq!(reloaded.scrollback.lines, 50_000);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    // ── Logo config ───────────────────────────────────────────────────

    #[test]
    fn logo_variant_default_is_cool() {
        let c = Config::default();
        assert_eq!(c.logo.variant, LogoVariant::Cool);
    }

    #[test]
    fn logo_variant_round_trip_all_variants() {
        for v in LogoVariant::ALL {
            let toml_str = format!("[logo]\nvariant = \"{}\"\n", v.as_str());
            let cfg: Config = toml::from_str(&toml_str).unwrap();
            assert_eq!(cfg.logo.variant, v, "round-trip failed for {:?}", v);
        }
    }

    #[test]
    fn logo_variant_unknown_falls_back_to_cool() {
        let toml_str = "[logo]\nvariant = \"nonexistent\"\n";
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.logo.variant, LogoVariant::Cool);
    }

    #[test]
    fn logo_variant_save_writes_non_default() {
        let tmp = temp_path("logo-save");
        let _ = std::fs::remove_file(&tmp);
        let mut cfg = Config::default();
        cfg.logo.variant = LogoVariant::Warm;
        cfg.save_to_path(&tmp).unwrap();
        let text = std::fs::read_to_string(&tmp).unwrap();
        assert!(text.contains("[logo]"), "missing [logo] section");
        assert!(
            text.contains("variant = \"warm\""),
            "missing variant = warm"
        );
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn logo_variant_default_not_written() {
        let tmp = temp_path("logo-default");
        let _ = std::fs::remove_file(&tmp);
        let cfg = Config::default();
        cfg.save_to_path(&tmp).unwrap();
        let text = std::fs::read_to_string(&tmp).unwrap();
        assert!(
            !text.contains("[logo]"),
            "default logo should not be written"
        );
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn logo_variant_from_str_round_trip() {
        for v in LogoVariant::ALL {
            assert_eq!(LogoVariant::from_str(v.as_str()), v);
        }
    }

    #[test]
    fn logo_variant_label_not_empty() {
        for v in LogoVariant::ALL {
            assert!(!v.label().is_empty());
        }
    }

    #[test]
    fn logo_variant_preserves_other_sections() {
        let tmp = temp_path("logo-preserve");
        let _ = std::fs::remove_file(&tmp);
        let initial = "[font]\nfamily = \"Monaco\"\nsize = 14.0\n\n[logo]\nvariant = \"light\"\n";
        std::fs::write(&tmp, initial).unwrap();
        let text = std::fs::read_to_string(&tmp).unwrap();
        let mut cfg: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.logo.variant, LogoVariant::Light);
        cfg.logo.variant = LogoVariant::Warm;
        cfg.save_to_path(&tmp).unwrap();
        let reloaded_text = std::fs::read_to_string(&tmp).unwrap();
        let reloaded: Config = toml::from_str(&reloaded_text).unwrap();
        assert_eq!(reloaded.font.family, "Monaco");
        assert_eq!(reloaded.font.size, 14.0);
        assert_eq!(reloaded.logo.variant, LogoVariant::Warm);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn logo_variant_save_cool_clears_stale_non_default() {
        // v1.0 fix: switching back to Cool (default) after saving a non-default
        // variant must clear the stale `variant = "warm"` from the file.
        // Otherwise the saved non-default value would override the default
        // on next load.
        let tmp = temp_path("logo-clear-stale");
        let _ = std::fs::remove_file(&tmp);
        // Step 1: save with Warm — writes [logo] variant = "warm".
        let mut cfg = Config::default();
        cfg.logo.variant = LogoVariant::Warm;
        cfg.save_to_path(&tmp).unwrap();
        let text = std::fs::read_to_string(&tmp).unwrap();
        assert!(text.contains("variant = \"warm\""));
        // Step 2: switch back to Cool and save — must remove the stale key.
        cfg.logo.variant = LogoVariant::Cool;
        cfg.save_to_path(&tmp).unwrap();
        let reloaded: Config = toml::from_str(&std::fs::read_to_string(&tmp).unwrap()).unwrap();
        assert_eq!(
            reloaded.logo.variant,
            LogoVariant::Cool,
            "stale non-default variant should be cleared on save"
        );
        let _ = std::fs::remove_file(&tmp);
    }
}
