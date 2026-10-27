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
//! name = "warp"                 # warp | warp-light | weft-warm | weft-light (weft-dark = legacy alias for weft-warm)
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

// v1.15.2: save family, moved verbatim (zero-rewrite; single-method
// writer kept intact — section-splitting is future functional work).
mod save_document;

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
