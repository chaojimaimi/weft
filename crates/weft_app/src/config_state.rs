//! v1.5.0: Config runtime state — raw source, effective overlay, and
//! reload fingerprint.
//!
//! Extracted from `app_state.rs` so the v1.5 profile fields
//! (`source_config`, `config_fingerprint`) can grow without pushing
//! `app_state.rs` past its architecture-gate ceiling. The 48 existing
//! `config_state.config.<field>` read sites keep working unchanged: `config`
//! is still the effective runtime config.

use weft_core::config::{Config, KeyBindings};

/// Runtime config state. Owns three views of the on-disk document:
///
/// - `config` — the **effective** config (source + active profile overlay).
///   This is what the renderer, terminal, and Settings UI read. It is
///   **never** persisted: saving `config` would flatten profile overrides
///   into the base document.
/// - `source_config` — the raw config as parsed from disk. The only form
///   that may be passed to `Config::save_to_path`. Profile overrides have
///   **not** been applied.
/// - `config_fingerprint` — stable hash of the raw file bytes. Used by the
///   reload watcher to skip no-op reloads (v1.5.3).
///
/// On startup `source_config == config` (no profile active, or the overlay
/// produces the same values). They diverge only when a profile is active.
#[derive(Clone, Debug)]
pub struct ConfigState {
    /// Effective runtime config (source + active profile overlay). Read by
    /// the 48 existing `config_state.config.<field>` call sites.
    pub config: Config,
    /// Raw source config (no profile overlay applied). The only form that
    /// may be persisted. `None` until the first successful
    /// `load_resolved` load (legacy callers that still use
    /// `Config::load()` leave this as `None`, and `config` is treated as
    /// both source and effective).
    pub source_config: Option<Config>,
    /// Stable fingerprint of the raw file bytes. `0` when no file has been
    /// loaded yet. Used by the reload watcher to skip duplicate applies.
    pub config_fingerprint: u64,
    pub keybindings: KeyBindings,
    pub path_bins: Vec<String>,
    pub theme_is_dark: bool,
    pub preferred_dark_theme: String,
    pub font_scale: f32,
}

impl ConfigState {
    /// Build a `ConfigState` from a plain `Config` (legacy path: no
    /// source/effective separation). Used by `Config::load()` startup
    /// compat and by tests that don't care about profiles.
    ///
    /// `source_config` is set to `None` — callers that need to save must
    /// use `from_loaded` instead.
    pub fn new(config: Config, path_bins: Vec<String>) -> Self {
        let preferred_dark_theme =
            if !config.theme.name.contains("light") && !config.theme.name.is_empty() {
                config.theme.name.clone()
            } else {
                config
                    .theme
                    .dark_name
                    .clone()
                    .unwrap_or_else(|| "weft-warm".into())
            };
        let keybindings = config.keybindings();
        Self {
            config,
            source_config: None,
            config_fingerprint: 0,
            keybindings,
            path_bins,
            theme_is_dark: true,
            preferred_dark_theme,
            font_scale: 1.0,
        }
    }

    /// v1.5.0: Build a `ConfigState` from a `LoadedConfig` (the preferred
    /// path). `source` and `effective` are both populated, so profile
    /// overrides are preserved across saves.
    pub fn from_loaded(loaded: weft_core::config::LoadedConfig, path_bins: Vec<String>) -> Self {
        let effective = loaded.effective.clone();
        let preferred_dark_theme =
            if !effective.theme.name.contains("light") && !effective.theme.name.is_empty() {
                effective.theme.name.clone()
            } else {
                effective
                    .theme
                    .dark_name
                    .clone()
                    .unwrap_or_else(|| "weft-warm".into())
            };
        let keybindings = effective.keybindings();
        Self {
            config: effective,
            source_config: Some(loaded.source),
            config_fingerprint: loaded.fingerprint,
            keybindings,
            path_bins,
            theme_is_dark: true,
            preferred_dark_theme,
            font_scale: 1.0,
        }
    }

    /// v1.5.0: Borrow the saveable source config. Returns `&config` when
    /// `source_config` is `None` (legacy startup path), so save call sites
    /// work regardless of which constructor was used.
    pub fn source(&self) -> &Config {
        self.source_config.as_ref().unwrap_or(&self.config)
    }

    /// v1.5.0: Replace the effective config in place (e.g. after a profile
    /// switch). `source_config` is untouched — profile switches only
    /// change which overlay is applied, not the base document.
    #[allow(dead_code)] // v1.5.1: consumed by profile switch transaction
    pub fn set_effective(&mut self, effective: Config) {
        self.keybindings = effective.keybindings();
        self.config = effective;
    }

    /// v1.5.0: Replace both source and effective (e.g. after a successful
    /// reload). Resets the fingerprint so the next watcher tick doesn't
    /// re-apply the same content.
    pub fn set_loaded(&mut self, loaded: weft_core::config::LoadedConfig) {
        self.keybindings = loaded.effective.keybindings();
        self.config = loaded.effective;
        self.source_config = Some(loaded.source);
        self.config_fingerprint = loaded.fingerprint;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_state_preserves_preferred_dark_theme() {
        let mut config = Config::default();
        config.theme.name = "solarized-dark".into();
        let state = ConfigState::new(config, vec!["cargo".into()]);
        assert_eq!(state.preferred_dark_theme, "solarized-dark");
        assert_eq!(state.path_bins, ["cargo"]);
        assert_eq!(state.font_scale, 1.0);
    }

    #[test]
    fn legacy_new_has_no_source() {
        let config = Config::default();
        let state = ConfigState::new(config, Vec::new());
        assert!(state.source_config.is_none());
        assert_eq!(state.config_fingerprint, 0);
        // source() falls back to &config.
        assert_eq!(state.source().font.family, state.config.font.family);
    }

    #[test]
    fn from_loaded_preserves_source_and_effective() {
        // Build a LoadedConfig with a profile override.
        let mut source = Config::default();
        source.font.family = "Base".into();
        source.active_profile = Some("work".into());
        source.profiles.insert(
            "work".into(),
            weft_core::config::ProfileConfig {
                font: Some(weft_core::config::FontConfig {
                    family: "Profile".into(),
                    ..weft_core::config::FontConfig::default()
                }),
                ..weft_core::config::ProfileConfig::default()
            },
        );
        let (effective, _diags) = source.resolve_active_profile().unwrap();
        let loaded = weft_core::config::LoadedConfig {
            source: source.clone(),
            effective,
            fingerprint: 12345,
            diagnostics: Vec::new(),
        };
        let state = ConfigState::from_loaded(loaded, Vec::new());
        // source retains base font.
        assert_eq!(state.source().font.family, "Base");
        // effective has profile font.
        assert_eq!(state.config.font.family, "Profile");
        assert_eq!(state.config_fingerprint, 12345);
        assert!(state.source_config.is_some());
    }

    #[test]
    fn set_effective_does_not_touch_source() {
        let mut state = ConfigState::new(Config::default(), Vec::new());
        let mut new_effective = state.config.clone();
        new_effective.font.family = "NewEffect".into();
        state.set_effective(new_effective);
        assert_eq!(state.config.font.family, "NewEffect");
        // Source is None (legacy path) — set_effective must not create one.
        assert!(state.source_config.is_none());
    }

    #[test]
    fn set_loaded_replaces_all_three() {
        let mut state = ConfigState::new(Config::default(), Vec::new());
        let mut source = Config::default();
        source.font.family = "Reloaded".into();
        let (effective, _) = source.resolve_active_profile().unwrap();
        let loaded = weft_core::config::LoadedConfig {
            source,
            effective,
            fingerprint: 999,
            diagnostics: Vec::new(),
        };
        state.set_loaded(loaded);
        assert_eq!(state.config.font.family, "Reloaded");
        assert_eq!(state.config_fingerprint, 999);
        assert!(state.source_config.is_some());
    }
}
