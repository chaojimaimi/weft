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
//! minimum_contrast = 7.0         # 1.0 = exact RGB; higher = paint-only readability boost
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
mod io;
mod keybindings;
mod parsers;
mod profiles;
mod save;
mod sections;
mod theme;
mod transfer;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use serde::Deserialize;

#[cfg(test)]
use crate::grid::Color;
#[cfg(test)]
use crate::input::{KeyCode, Modifiers};

pub use action::Action;
pub use io::{
    atomic_write, fingerprint_bytes, load_resolved, load_resolved_from_path, normalize_scrollback,
    ConfigLoadError, LoadedConfig, SCROLLBACK_MAX_LINES, SCROLLBACK_MIN_LINES,
};
pub use keybindings::KeyBindings;
pub use parsers::{parse_binding, parse_hex};
pub use profiles::{
    apply_overrides, validate_profile_name, ConfigDiagnostic, ConfigSectionMask, ProfileConfig,
    ProfileError, MAX_PROFILES,
};
pub use save::ConfigSaveError;
pub use sections::{
    AiConfig, BlocksConfig, ClipboardConfig, CompatConfig, EditorConfig, FontConfig, LogoConfig,
    LogoVariant, NotificationsConfig, Osc52Mode, OutputSemanticConfig, PasteConfig,
    ScrollbackConfig, SyntaxConfig, ThemeConfig, WindowConfig, PASTE_SIZE_TIERS_KIB,
    SIDEBAR_MAX_WIDTH, SIDEBAR_MIN_WIDTH,
};
pub use theme::{OutputSemanticColors, SyntaxColors, Theme};
pub use transfer::{export_config_document, import_config_document, ConfigTransferError};

use self::save::{
    parse_existing, set_f32_if_diff, set_opt_string, set_opt_string_clear, set_string_if_diff,
    set_u32_if_diff, set_usize_if_diff,
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
    /// v1.11.1 (PLAN_v1111 §4.2): large-paste protection switches.
    pub paste: PasteConfig,
    /// v1.11.2 X4 (PLAN_v1112 §1.2): per-tab in-memory block retention cap.
    /// Config-file power-user key — no Settings UI row by design.
    pub blocks: BlocksConfig,
    /// v1.11.3 (PLAN_v1113 §3.3): terminal compatibility switches
    /// (`bold_is_bright`). Config-file key — no Settings UI row.
    pub compat: CompatConfig,
    /// v1.11.5 (PLAN_v1115 §M8): OSC 52 clipboard access mode
    /// (`[clipboard].osc52`).
    pub clipboard: ClipboardConfig,
    /// v1.11.5 (PLAN_v1115 §M8): notification gates
    /// (`[notifications]` enabled/threshold_secs/sound).
    pub notifications: NotificationsConfig,
    pub logo: LogoConfig,
    /// v1.6 AI integration. Disabled by default (`provider = None`).
    /// Config schema is parsed/serialized today so existing config files keep
    /// working; the runtime consumer (`weft_app/src/ai/`) is scaffolding-only
    /// in v1.3 (see ROADMAP DC-8).
    pub ai: AiConfig,
    /// Raw user keybinding overrides: `"cmd+x" = "copy"`. Resolved later via
    /// [`Config::keybindings`] (merged onto defaults).
    pub keybindings: HashMap<String, Action>,
    /// v1.5.0: Name of the active profile. `None` or empty string means
    /// "use base only". Resolved at load time via
    /// [`Config::resolve_active_profile`].
    pub active_profile: Option<String>,
    /// v1.5.0: Named profiles. `BTreeMap` so Settings, serialization, palette
    /// and tests all see a stable, alphabetical order. See
    /// [`profiles::ProfileConfig`] for override semantics.
    pub profiles: BTreeMap<String, ProfileConfig>,
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
                // v1.11.2 X3: same scrollback clamp as the v1.5 load path so
                // both entry points produce in-range values.
                io::normalize_scrollback(&mut cfg);
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
        set_f32_if_diff(
            theme,
            "minimum_contrast",
            self.theme.minimum_contrast,
            default_theme.minimum_contrast,
        );
        set_opt_string(theme, "foreground", &self.theme.foreground);
        set_opt_string(theme, "background", &self.theme.background);
        set_opt_string(theme, "cursor", &self.theme.cursor);
        set_opt_string(theme, "selection", &self.theme.selection);
        set_opt_string(theme, "accent", &self.theme.accent);
        set_opt_string(theme, "accent_dim", &self.theme.accent_dim);
        set_opt_string(theme, "separator", &self.theme.separator);
        // palette: only write if non-empty (non-default). 清空时显式删除磁盘
        // 旧值，避免残留（与 set_string_if_diff 同哲学）。
        if !self.theme.palette.is_empty() {
            let mut arr = toml_edit::Array::new();
            for hex in &self.theme.palette {
                arr.push(hex.as_str());
            }
            theme["palette"] = toml_edit::Item::Value(toml_edit::Value::Array(arr));
        } else if theme.contains_key("palette") {
            theme.remove("palette");
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
        // 操作现有表（若存在）而非每次创建新表，避免清空所有字段时旧表残留。
        if let Some(syn) = &self.theme.syntax {
            let syntax_entry = theme.entry("syntax").or_insert_with(toml_edit::table);
            if syntax_entry.is_none() {
                *syntax_entry = toml_edit::table();
            }
            let st = syntax_entry.as_table_mut().expect("syntax is a table");
            set_opt_string_clear(st, "command", &syn.command);
            set_opt_string_clear(st, "flag", &syn.flag);
            set_opt_string_clear(st, "argument", &syn.argument);
            set_opt_string_clear(st, "path", &syn.path);
            set_opt_string_clear(st, "string", &syn.string);
            set_opt_string_clear(st, "number", &syn.number);
            set_opt_string_clear(st, "variable", &syn.variable);
            set_opt_string_clear(st, "operator", &syn.operator);
            set_opt_string_clear(st, "comment", &syn.comment);
            set_opt_string_clear(st, "default", &syn.default);
            // 若表为空（所有字段都是 None），删除整个 [theme.syntax] 表。
            if st.iter().count() == 0 {
                theme.remove("syntax");
            }
        } else if theme.contains_key("syntax") {
            // cfg.theme.syntax = None：用户清除了整个 syntax override，删除磁盘表。
            theme.remove("syntax");
        }
        // v1.7.0-D: [theme.output] subsection — semantic output colors + toggle.
        // 操作现有表（若存在）而非每次创建新表，避免 toml_edit 增量编辑时
        // 旧键残留。修复 v1.7.5 回归：用户 on→off→on 切换时，`enabled = false`
        // 因新表为空（iter().count()==0）未被替换，残留磁盘导致 reload 仍为 false。
        if let Some(out) = &self.theme.output {
            // 确保 [theme.output] 表存在（不存在则创建）。
            let output_entry = theme.entry("output").or_insert_with(toml_edit::table);
            if output_entry.is_none() {
                *output_entry = toml_edit::table();
            }
            let ot = output_entry.as_table_mut().expect("output is a table");
            // enabled: 三态处理。
            //   Some(false) → 写入 `enabled = false`（关闭语义着色）。
            //   Some(true)  → 显式删除磁盘上的 enabled 键（默认 true，避免残留 false）。
            //   None        → 不动（保持磁盘原值）。
            match out.enabled {
                Some(false) => {
                    ot["enabled"] = toml_edit::value(false);
                }
                Some(true) => {
                    if ot.contains_key("enabled") {
                        ot.remove("enabled");
                    }
                }
                None => {}
            }
            set_opt_string_clear(ot, "output_default", &out.output_default);
            // v1.11.0: `cwd` key removed — dead config (painter derives CWD
            // gray from fg×0.65). A stale `cwd = "..."` a user wrote earlier
            // is deliberately NOT cleaned here: unknown keys on the doc are
            // preserved by the save flow, and removing it would fight the
            // "preserve unknown fields" contract. See AUDIT_v1.10.39 / PLAN_v111.
            set_opt_string_clear(ot, "metadata", &out.metadata);
            set_opt_string_clear(ot, "success", &out.success);
            set_opt_string_clear(ot, "failure", &out.failure);
            // 若表为空（所有字段都是默认值/None），删除整个 [theme.output] 表
            // 保持配置文件整洁。
            if ot.iter().count() == 0 {
                theme.remove("output");
            }
        } else if theme.contains_key("output") {
            // cfg.theme.output = None：用户清除了整个 output override，删除磁盘表。
            theme.remove("output");
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

        let default_editor = EditorConfig::default();
        if self.editor.submit_on_ctrl_enter != default_editor.submit_on_ctrl_enter
            || self.editor.smart_select != default_editor.smart_select
        {
            let editor_entry = doc.entry("editor").or_insert_with(toml_edit::table);
            if editor_entry.is_none() {
                *editor_entry = toml_edit::table();
            }
            let editor = editor_entry.as_table_mut().expect("editor is a table");
            if self.editor.submit_on_ctrl_enter {
                editor["submit_on_ctrl_enter"] = toml_edit::value(true);
            } else {
                editor.remove("submit_on_ctrl_enter");
            }
            if self.editor.smart_select != default_editor.smart_select {
                editor["smart_select"] = toml_edit::value(self.editor.smart_select);
            } else {
                editor.remove("smart_select");
            }
        } else if let Some(editor) = doc.get_mut("editor").and_then(|item| item.as_table_mut()) {
            editor.remove("submit_on_ctrl_enter");
            editor.remove("smart_select");
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

        // [ai] section — v1.6 AI integration. Implementation lives in
        // `save::write_ai_section` to keep this file within its
        // architecture-gate line budget.
        save::write_ai_section(&mut doc, &self.ai);

        // [paste] section — v1.11.1 large-paste protection
        // (PLAN_v1111 §4.2). Same line-budget rationale as [ai].
        save::write_paste_section(&mut doc, &self.paste);

        // [blocks] section — v1.11.2 X4 retention cap (PLAN_v1112 §1.2).
        save::write_blocks_section(&mut doc, &self.blocks);

        // [compat] section — v1.11.3 (PLAN_v1113 §3.3); v1.11.4 adds
        // `kitty_keyboard` (PLAN_v1114 §3). Inline like the [logo] block
        // above (save.rs is at its gate budget): only non-default values
        // are persisted.
        if self.compat.bold_is_bright != CompatConfig::default().bold_is_bright
            || self.compat.kitty_keyboard != CompatConfig::default().kitty_keyboard
        {
            let compat_entry = doc.entry("compat").or_insert_with(toml_edit::table);
            if compat_entry.is_none() {
                *compat_entry = toml_edit::table();
            }
            match compat_entry.as_table_mut() {
                Some(t) => {
                    t["bold_is_bright"] = toml_edit::value(self.compat.bold_is_bright);
                    t["kitty_keyboard"] = toml_edit::value(self.compat.kitty_keyboard);
                }
                None => {
                    tracing::warn!("[compat] section is not a table; skipping compat write");
                }
            }
        }

        // [clipboard] section — v1.11.5 (PLAN_v1115 §M8): only write when
        // the mode differs from `default`; clear a stale key otherwise so a
        // previously saved non-default value can't resurrect on reload.
        if self.clipboard.osc52 != ClipboardConfig::default().osc52 {
            let cb_entry = doc.entry("clipboard").or_insert_with(toml_edit::table);
            if cb_entry.is_none() {
                *cb_entry = toml_edit::table();
            }
            match cb_entry.as_table_mut() {
                Some(t) => {
                    t["osc52"] = toml_edit::value(self.clipboard.osc52.as_str());
                }
                None => {
                    tracing::warn!("[clipboard] section is not a table; skipping write");
                }
            }
        } else if let Some(t) = doc.get_mut("clipboard").and_then(|i| i.as_table_mut()) {
            t.remove("osc52");
            if t.iter().count() == 0 {
                doc.remove("clipboard");
            }
        }

        // [notifications] section — v1.11.5 (PLAN_v1115 §M8): write only
        // non-default keys; remove the whole table when all-default so a
        // stale file never pins a changed default.
        {
            let default_notify = NotificationsConfig::default();
            let dirty = self.notifications != default_notify;
            if dirty {
                let n_entry = doc.entry("notifications").or_insert_with(toml_edit::table);
                if n_entry.is_none() {
                    *n_entry = toml_edit::table();
                }
                match n_entry.as_table_mut() {
                    Some(t) => {
                        if self.notifications.enabled != default_notify.enabled {
                            t["enabled"] = toml_edit::value(self.notifications.enabled);
                        } else {
                            t.remove("enabled");
                        }
                        if self.notifications.threshold_secs != default_notify.threshold_secs {
                            t["threshold_secs"] =
                                toml_edit::value(self.notifications.threshold_secs as i64);
                        } else {
                            t.remove("threshold_secs");
                        }
                        if self.notifications.sound != default_notify.sound {
                            t["sound"] = toml_edit::value(self.notifications.sound);
                        } else {
                            t.remove("sound");
                        }
                        if t.iter().count() == 0 {
                            doc.remove("notifications");
                        }
                    }
                    None => {
                        tracing::warn!("[notifications] section is not a table; skipping write");
                    }
                }
            } else if let Some(t) = doc.get_mut("notifications").and_then(|i| i.as_table_mut()) {
                t.remove("enabled");
                t.remove("threshold_secs");
                t.remove("sound");
                if t.iter().count() == 0 {
                    doc.remove("notifications");
                }
            }
        }

        // [keybindings] section.
        if !self.keybindings.is_empty() {
            let mut kb_table = toml_edit::table();
            let kt = kb_table.as_table_mut().unwrap();
            for (binding, action) in &self.keybindings {
                kt.insert(binding, toml_edit::value(action::action_to_str(action)));
            }
            doc["keybindings"] = kb_table;
        }

        // v1.5.0: active_profile + profiles. Lives in `save::write_*` to
        // keep this file within its architecture-gate budget.
        save::write_active_profile(&mut doc, &self.active_profile);
        save::write_profiles(&mut doc, self);

        // Atomic write: <path>.tmp → rename → <path>.
        let parent = path.parent().ok_or(ConfigSaveError::NoParentDir)?;
        std::fs::create_dir_all(parent).map_err(ConfigSaveError::Io)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, doc.to_string()).map_err(ConfigSaveError::Io)?;
        std::fs::rename(&tmp, path).map_err(ConfigSaveError::Io)?;
        Ok(())
    }

    /// The config file path. Resolution order (v1.5.3):
    ///
    /// 1. `WEFT_CONFIG` (absolute path override). Empty values are ignored.
    ///    Relative paths log a warning and fall through to XDG/HOME so a
    ///    misconfigured `WEFT_CONFIG` can't silently point at an unintended
    ///    file (especially risky when launched from Finder where `cwd` is
    ///    indeterminate). The env var is read once at the call site; runtime
    ///    mutations after startup are picked up on the next call but the
    ///    watcher thread is not re-armed.
    /// 2. `$XDG_CONFIG_HOME/weft/config.toml`.
    /// 3. `~/.config/weft/config.toml`.
    ///
    /// Returns `None` only when none of `WEFT_CONFIG`/`XDG_CONFIG_HOME`/`HOME`
    /// is set. All consumers (watcher, save, import backup, export default
    /// directory) call this function so they consistently resolve to the
    /// same path.
    pub fn config_path() -> Option<PathBuf> {
        // v1.5.3: WEFT_CONFIG absolute path override (highest priority).
        // Empty values are ignored; relative paths warn and fall through.
        if let Some(weft_config) = std::env::var_os("WEFT_CONFIG").filter(|s| !s.is_empty()) {
            let path = PathBuf::from(weft_config);
            if path.is_absolute() {
                return Some(path);
            }
            tracing::warn!(
                path = %path.display(),
                "WEFT_CONFIG must be an absolute path; falling back to XDG/HOME"
            );
        }

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
#[path = "tests.rs"]
mod tests;
