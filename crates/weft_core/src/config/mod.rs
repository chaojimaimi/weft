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
mod theme_import;
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
    AiConfig, BlocksConfig, ClipboardConfig, CompatConfig, EditorConfig, ExperimentalConfig,
    FontConfig, LogoConfig, LogoVariant, NotificationsConfig, Osc52Mode, OutputSemanticConfig,
    PasteConfig, RecoveryMode, ScrollbackConfig, SessionConfig, SyntaxConfig, ThemeConfig,
    UiConfig, UpdateCheckTier, UpdateConfig, WindowConfig, PASTE_SIZE_TIERS_KIB, SIDEBAR_MAX_WIDTH,
    SIDEBAR_MIN_WIDTH,
};
pub use theme::{OutputSemanticColors, SyntaxColors, Theme, ThemeUi};
pub use theme_import::{
    contrast_ratio, ensure_minimum_contrast, mix_colors, relative_luminance, ThemeImport,
    CHROME_CONTRAST, TEXT_CONTRAST,
};
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
    /// v1.11.7 (PLAN_v1117_SHADOW_BLOCK_VIEW §三 M1.2, D-d):
    /// `[experimental]` switches — `tui_render_mode` (default
    /// `noninteractive`) is the primary-screen TUI render tier injected into
    /// every constructed Terminal (P2-3).
    pub experimental: ExperimentalConfig,
    /// v1.12.19 (PLAN_v11217 §3.8 T13a): session lifecycle switches
    /// (`[session].recovery` — crash-recovery prompt behavior). Global only
    /// (`ProfileConfig` has no `session` field, like `[ai]`).
    pub session: SessionConfig,
    /// v1.13.0 (PLAN_v1.13.0_SPARKLE §WP2): Sparkle update-check tier
    /// (`[update].check`). Global only (`ProfileConfig` has no `update`
    /// field, like `[session]`).
    pub update: UpdateConfig,
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
                // PLAN_v11217 §3.5 (T4): same clamp for output_cap_mib.
                io::normalize_blocks(&mut cfg);
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
        // v1.12: 主题元数据（变体/作者/来源/许可）。有则写、无则删，保证
        // 差量写回不留陈旧署名。
        for (key, value) in [
            ("variant", &self.theme.variant),
            ("author", &self.theme.author),
            ("source", &self.theme.source),
            ("license", &self.theme.license),
        ] {
            if let Some(v) = value {
                theme[key] = toml_edit::value(v.as_str());
            } else if theme.contains_key(key) {
                theme.remove(key);
            }
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
            // enabled: 三态处理（v1.12.2 PLAN_S2_render A1：默认值翻转为 false）。
            //   Some(true)  → 写入 `enabled = true`（显式开启语义着色，现为新默认下的非默认值）。
            //   Some(false) → 显式删除磁盘上的 enabled 键（默认 false，避免残留 true）。
            //   None        → 不动（保持磁盘原值）。
            match out.enabled {
                Some(true) => {
                    ot["enabled"] = toml_edit::value(true);
                }
                // v1.11.16: the inner `if` is folded into a match guard
                // (clippy::collapsible_match is a hard error under CI's
                // `-D warnings`). Semantics unchanged: delete the on-disk
                // `enabled` key only when it actually exists.
                Some(false) if ot.contains_key("enabled") => {
                    ot.remove("enabled");
                }
                Some(false) => {}
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
        // v1.11.6 (PLAN_v1116 M6/D-f): `[theme] link` — write if set,
        // remove when cleared (same diff philosophy as the hex keys above).
        if let Some(link) = &self.theme.link {
            theme["link"] = toml_edit::value(link.as_str());
        } else if theme.contains_key("link") {
            theme.remove("link");
        }
        // v1.11.6 (PLAN_v1116 M6/D-f): `[theme.ui]` subsection. 操作现有
        // 表（若存在）而非每次创建新表，避免清空所有字段时旧表残留。
        if let Some(ui) = &self.theme.ui {
            let ui_entry = theme.entry("ui").or_insert_with(toml_edit::table);
            if ui_entry.is_none() {
                *ui_entry = toml_edit::table();
            }
            let uit = ui_entry.as_table_mut().expect("ui is a table");
            set_opt_string_clear(uit, "success", &ui.success);
            set_opt_string_clear(uit, "warning", &ui.warning);
            set_opt_string_clear(uit, "error", &ui.error);
            set_opt_string_clear(uit, "find_match", &ui.find_match);
            if uit.iter().count() == 0 {
                theme.remove("ui");
            }
        } else if theme.contains_key("ui") {
            // cfg.theme.ui = None：用户清除了整个 ui override，删除磁盘表。
            theme.remove("ui");
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
        // v1.12.2 B2 (PLAN_S2_render): live-resize present-mode rollback
        // switch. `false` is the default — only persist an explicit `true`
        // so the minimal-write contract holds and a stale key can't override
        // a future default change.
        if self.window.presents_with_transaction_live_resize {
            window["presents_with_transaction_live_resize"] = toml_edit::value(true);
        } else if window.contains_key("presents_with_transaction_live_resize") {
            window.remove("presents_with_transaction_live_resize");
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

        // v1.11.12 (PLAN_v11112 M-E): skip ledger — nested defensive skips
        // (a hand-written scalar where a table belongs) are collected by the
        // save:: writers and surfaced once below; the external save signature
        // is unchanged (F9).
        let mut skipped: Vec<&'static str> = Vec::new();

        // [ai] section — v1.6 AI integration. Implementation lives in
        // `save::write_ai_section` to keep this file within its
        // architecture-gate line budget.
        save::write_ai_section(&mut doc, &self.ai, &mut skipped);

        // [paste] section — v1.11.1 large-paste protection
        // (PLAN_v1111 §4.2). Same line-budget rationale as [ai].
        save::write_paste_section(&mut doc, &self.paste, &mut skipped);

        // [blocks] section — v1.11.2 X4 retention cap (PLAN_v1112 §1.2).
        save::write_blocks_section(&mut doc, &self.blocks, &mut skipped);

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

        // [experimental] section — v1.11.7 (PLAN_v1117 §三 M1.2, D-d): only
        // write `tui_render_mode` when it differs from the factory default
        // (noninteractive); remove the key/table otherwise so a stale file
        // can never pin a changed default.
        {
            let default_exp = ExperimentalConfig::default();
            if self.experimental.tui_render_mode != default_exp.tui_render_mode {
                let exp_entry = doc.entry("experimental").or_insert_with(toml_edit::table);
                if exp_entry.is_none() {
                    *exp_entry = toml_edit::table();
                }
                match exp_entry.as_table_mut() {
                    Some(t) => {
                        t["tui_render_mode"] =
                            toml_edit::value(self.experimental.tui_render_mode.as_str());
                    }
                    None => {
                        tracing::warn!("[experimental] section is not a table; skipping write");
                    }
                }
            } else if let Some(t) = doc.get_mut("experimental").and_then(|i| i.as_table_mut()) {
                t.remove("tui_render_mode");
                if t.iter().count() == 0 {
                    doc.remove("experimental");
                }
            }
        }

        // [session] section — v1.12.19 (PLAN_v11217 §3.8 T13a): only write
        // `recovery` when it differs from the default (ask); remove the
        // key/table otherwise so a stale file never pins a changed default
        // (same minimal-write contract as [clipboard] above).
        if self.session.recovery != SessionConfig::default().recovery {
            let session_entry = doc.entry("session").or_insert_with(toml_edit::table);
            if session_entry.is_none() {
                *session_entry = toml_edit::table();
            }
            match session_entry.as_table_mut() {
                Some(t) => {
                    t["recovery"] = toml_edit::value(self.session.recovery.as_str());
                }
                None => {
                    tracing::warn!("[session] section is not a table; skipping write");
                }
            }
        } else if let Some(t) = doc.get_mut("session").and_then(|i| i.as_table_mut()) {
            t.remove("recovery");
            if t.iter().count() == 0 {
                doc.remove("session");
            }
        }

        // [update] section — v1.13.0 (PLAN_v1.13.0_SPARKLE §WP2): Sparkle
        // check tier. Writer lives in save.rs (line budget; plan R4: 勿内联).
        save::write_update_section(&mut doc, &self.update);

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
        save::write_profiles(&mut doc, self, &mut skipped);

        // v1.11.12 (PLAN_v11112 M-E): surface any skipped segments. Only
        // reachable with a hand-broken TOML file (parse_existing already
        // rejects scalar TOP-LEVEL sections), so this is diagnostics, not a
        // normal path — the skipped content survives on disk untouched.
        if !skipped.is_empty() {
            tracing::warn!("config save skipped sections: {:?}", skipped);
        }

        // Atomic write: <path>.tmp → rename → <path>.
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let parent = path.parent().ok_or(ConfigSaveError::NoParentDir)?;
        std::fs::create_dir_all(parent).map_err(ConfigSaveError::Io)?;
        let tmp = path.with_extension("toml.tmp");
        // VULN-008: create the tmp at 0600, eliminating the 0644 window for
        // newly created files. `mode` only applies at creation time — a
        // stale 0644 `.tmp` from an older version is still caught by the
        // chmod below.
        let written = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .and_then(|mut file| file.write_all(doc.to_string().as_bytes()));
        written.map_err(ConfigSaveError::Io)?;
        // VULN-008: chmod BEFORE the rename — rename(2) keeps the tmp
        // inode's permissions, so a 0600 tmp means config.toml never lands
        // world-readable (a chmod after the rename would reopen a 0644
        // window on every save).
        if let Err(e) = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)) {
            let _ = std::fs::remove_file(&tmp);
            return Err(ConfigSaveError::Io(e));
        }
        // Cleanup aligns with the export/import failure branches; the
        // post-chmod tmp is already 0600, so this is consistency, not
        // exposure control.
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(ConfigSaveError::Io(e));
        }
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
