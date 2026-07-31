//! v1.5.0: Named config profiles.
//!
//! A profile is a partial config that overrides one or more sections of the
//! base config. The override granularity is the **section**: when a section
//! appears in a profile, it fully replaces the base's section of the same
//! name; sections absent from the profile are inherited unchanged.
//!
//! Rules (see `docs/V15_IMPLEMENTATION_PLAN.md` §3):
//!
//! - `active_profile = "name"` selects a profile. Empty string / `None`
//!   normalizes to "use base only".
//! - `ProfileConfig` uses `#[serde(deny_unknown_fields)]` so an `ai` section
//!   (or any unknown field) inside a profile is a schema error, not a
//!   silently-ignored key.
//! - Profile names match `[A-Za-z0-9][A-Za-z0-9._-]{0,31}`, max 32 profiles,
//!   name `base` is reserved.

use std::collections::HashMap;

use bitflags::bitflags;
use serde::Deserialize;

use super::{
    Action, AiConfig, EditorConfig, FontConfig, KeyBindings, LogoConfig, ScrollbackConfig,
    ThemeConfig, WindowConfig,
};

// ── ProfileConfig ──────────────────────────────────────────────────────

/// A named profile: a set of optional section overrides.
///
/// Each `Option<T>` field is `None` (absent from the TOML) → inherit the base
/// section; `Some(value)` → replace the base section wholesale. There is no
/// field-level merge: a profile that writes `[profiles.work.font]` must
/// specify every font field it cares about (missing fields fall back to
/// `FontConfig::default()`, not to the base `[font]`).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileConfig {
    pub font: Option<FontConfig>,
    pub theme: Option<ThemeConfig>,
    pub window: Option<WindowConfig>,
    pub scrollback: Option<ScrollbackConfig>,
    pub editor: Option<EditorConfig>,
    pub logo: Option<LogoConfig>,
    /// `keybindings` is also a full-section override (no per-key merge).
    pub keybindings: Option<HashMap<String, Action>>,
    // NOTE: `ai` is intentionally absent — AI config is global only.
    // `deny_unknown_fields` rejects it as a schema error.
}

impl ProfileConfig {
    /// Returns `true` when every section is `None` (the profile is empty
    /// and contributes no overrides). Used by Settings to avoid creating
    /// empty profile entries.
    pub fn is_empty(&self) -> bool {
        self.font.is_none()
            && self.theme.is_none()
            && self.window.is_none()
            && self.scrollback.is_none()
            && self.editor.is_none()
            && self.logo.is_none()
            && self.keybindings.is_none()
    }
}

// ── ConfigSectionMask ──────────────────────────────────────────────────

bitflags! {
    /// Tracks which top-level sections have been edited in the Settings UI
    /// so a save only writes the dirty sections back to the target
    /// (base or profile). Bit assignments are stable across releases.
    #[allow(dead_code)] // v1.5.1: consumed by Settings profile save path
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct ConfigSectionMask: u32 {
        const FONT         = 1 << 0;
        const THEME        = 1 << 1;
        const WINDOW       = 1 << 2;
        const SCROLLBACK   = 1 << 3;
        const EDITOR       = 1 << 4;
        const LOGO         = 1 << 5;
        const KEYBINDINGS  = 1 << 6;
        /// v1.8.3: AI config is global only (never written into a profile).
        /// The merge writes `draft.ai` directly to the base config regardless
        /// of whether a profile is active (ProfileConfig has no `ai` field).
        const AI           = 1 << 7;
    }
}

// ── ProfileError ───────────────────────────────────────────────────────

/// Errors raised by profile validation / resolution.
///
/// `Config::resolve_active_profile` returns these when the config schema is
/// structurally invalid (bad name, too many profiles). A missing *active*
/// profile is **not** a `ProfileError` — it's a non-fatal
/// `ConfigDiagnostic` (we keep running on base).
///
/// Unknown profile fields are rejected by `#[serde(deny_unknown_fields)]`
/// at deserialization time, surfacing as `ConfigLoadError::Parse` before
/// `resolve_active_profile` ever runs — so they don't need a variant here.
#[derive(Debug)]
pub enum ProfileError {
    /// Profile name failed `[A-Za-z0-9][A-Za-z0-9._-]{0,31}`.
    InvalidName(String),
    /// Name `base` (case-insensitive) is reserved.
    ReservedName(String),
    /// More than 32 profiles declared.
    TooManyProfiles(usize),
}

impl std::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidName(n) => write!(
                f,
                "invalid profile name {:?}: must match [A-Za-z0-9][A-Za-z0-9._-]{{0,31}}",
                n
            ),
            Self::ReservedName(n) => {
                write!(f, "profile name {:?} is reserved (use a different name)", n)
            }
            Self::TooManyProfiles(n) => write!(f, "too many profiles: {} declared, max 32", n),
        }
    }
}

impl std::error::Error for ProfileError {}

// ── Name validation ────────────────────────────────────────────────────

/// Max number of profiles allowed in a single config file.
pub const MAX_PROFILES: usize = 32;

/// Validate a profile name: `[A-Za-z0-9][A-Za-z0-9._-]{0,31}`, 1..=32 chars,
/// not the reserved name `base` (case-insensitive).
pub fn validate_profile_name(name: &str) -> Result<(), ProfileError> {
    if name.eq_ignore_ascii_case("base") {
        return Err(ProfileError::ReservedName(name.to_string()));
    }
    let len = name.len();
    if len == 0 || len > 32 {
        return Err(ProfileError::InvalidName(name.to_string()));
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_alphanumeric() {
        return Err(ProfileError::InvalidName(name.to_string()));
    }
    for c in chars {
        if !c.is_ascii_alphanumeric() && c != '.' && c != '_' && c != '-' {
            return Err(ProfileError::InvalidName(name.to_string()));
        }
    }
    Ok(())
}

// ── Section-level apply ───────────────────────────────────────────────

/// Apply `profile`'s overrides onto `base` in place. Sections that are
/// `None` in the profile are left untouched (inherited). Sections that are
/// `Some` replace the base wholesale.
///
/// `base.profiles` and `base.active_profile` are never touched by this
/// function — they are metadata, not runtime state.
pub fn apply_overrides(base: &mut super::Config, profile: &ProfileConfig) {
    if let Some(font) = &profile.font {
        base.font = font.clone();
    }
    if let Some(theme) = &profile.theme {
        base.theme = theme.clone();
    }
    if let Some(window) = &profile.window {
        base.window = window.clone();
    }
    if let Some(scrollback) = &profile.scrollback {
        base.scrollback = scrollback.clone();
    }
    if let Some(editor) = &profile.editor {
        base.editor = editor.clone();
    }
    if let Some(logo) = &profile.logo {
        base.logo = logo.clone();
    }
    if let Some(keybindings) = &profile.keybindings {
        base.keybindings = keybindings.clone();
    }
}

// ── ConfigDiagnostic ───────────────────────────────────────────────────

/// Non-fatal config issues surfaced to the UI but not blocking startup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigDiagnostic {
    /// `active_profile = "x"` but no `[profiles.x]` exists. We fall back
    /// to base and continue.
    MissingActiveProfile(String),
}

impl std::fmt::Display for ConfigDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingActiveProfile(name) => {
                write!(f, "active profile {:?} not found; using base config", name)
            }
        }
    }
}

// ── Config::resolve_active_profile ────────────────────────────────────

impl super::Config {
    /// Validate all profiles and, if `active_profile` is set, produce the
    /// effective config by applying the active profile's section overrides
    /// onto a clone of the base.
    ///
    /// - `active_profile = ""` / `None` → effective == base, no diagnostics.
    /// - `active_profile = "missing"` → effective == base + one
    ///   `MissingActiveProfile` diagnostic (non-fatal).
    /// - Invalid profile name / schema / count → `ProfileError` (fatal).
    ///
    /// `self.profiles` and `self.active_profile` are preserved on the
    /// returned `effective` config so the UI can still display them, but
    /// `apply_overrides` never reads `effective.profiles`.
    pub fn resolve_active_profile(
        &self,
    ) -> Result<(super::Config, Vec<ConfigDiagnostic>), ProfileError> {
        // Validate profile count.
        if self.profiles.len() > MAX_PROFILES {
            return Err(ProfileError::TooManyProfiles(self.profiles.len()));
        }
        // Validate every profile name (even inactive ones) so a broken
        // profile doesn't silently lurk until the user switches to it.
        for name in self.profiles.keys() {
            validate_profile_name(name)?;
        }

        let mut diagnostics = Vec::new();
        let mut effective = self.clone();

        let Some(active) = self.active_profile.as_deref() else {
            // None / absent → base only.
            return Ok((effective, diagnostics));
        };
        let active = active.trim();
        if active.is_empty() {
            // Empty string normalizes to "use base only".
            return Ok((effective, diagnostics));
        }
        match self.profiles.get(active) {
            Some(profile) => {
                apply_overrides(&mut effective, profile);
            }
            None => {
                diagnostics.push(ConfigDiagnostic::MissingActiveProfile(active.to_string()));
            }
        }
        Ok((effective, diagnostics))
    }

    /// Recompute keybindings from the raw overrides. Cheap; called after
    /// `resolve_active_profile` so the effective keybindings reflect the
    /// active profile's `[keybindings]` section.
    pub fn resolved_keybindings(&self) -> KeyBindings {
        KeyBindings::from_overrides(&self.keybindings)
    }

    /// `true` when `ai` is at its default (no provider, no key, no model).
    /// Used by save logic to decide whether to drop the `[ai]` section.
    /// Exposed here as a method so profile code can reuse the check without
    /// duplicating the field list.
    pub fn ai_is_default(&self) -> bool {
        let default = AiConfig::default();
        self.ai.provider == default.provider
            && self.ai.api_key == default.api_key
            && self.ai.base_url == default.base_url
            && self.ai.model == default.model
            && self.ai.max_tokens == default.max_tokens
            && self.ai.timeout_secs == default.timeout_secs
            && self.ai.enable_error_diagnosis == default.enable_error_diagnosis
            && self.ai.enable_command_generation == default.enable_command_generation
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn empty_profile_is_empty() {
        assert!(ProfileConfig::default().is_empty());
    }

    #[test]
    fn profile_with_font_is_not_empty() {
        let p = ProfileConfig {
            font: Some(FontConfig::default()),
            ..ProfileConfig::default()
        };
        assert!(!p.is_empty());
    }

    #[test]
    fn valid_profile_names() {
        assert!(validate_profile_name("work").is_ok());
        assert!(validate_profile_name("dev-1").is_ok());
        assert!(validate_profile_name("a.b.c").is_ok());
        assert!(validate_profile_name("A_1-2.3").is_ok());
        assert!(validate_profile_name("x").is_ok());
    }

    #[test]
    fn invalid_profile_names() {
        assert!(validate_profile_name("").is_err());
        assert!(validate_profile_name("base").is_err());
        assert!(validate_profile_name("BASE").is_err());
        assert!(validate_profile_name("Base").is_err());
        assert!(validate_profile_name("_x").is_err()); // must start alphanumeric
        assert!(validate_profile_name("-x").is_err());
        assert!(validate_profile_name(".x").is_err());
        assert!(validate_profile_name("x!").is_err()); // bad char
        assert!(validate_profile_name("x y").is_err()); // space
        assert!(validate_profile_name(&"a".repeat(33)).is_err()); // too long
    }

    #[test]
    fn apply_overrides_replaces_present_sections() {
        let mut base = Config::default();
        base.font.family = "Menlo".into();
        base.font.size = 14.0;
        base.theme.name = "weft-warm".into();

        let profile = ProfileConfig {
            font: Some(FontConfig {
                family: "JetBrains Mono".into(),
                size: 16.0,
                ..FontConfig::default()
            }),
            ..ProfileConfig::default()
        };

        apply_overrides(&mut base, &profile);
        assert_eq!(base.font.family, "JetBrains Mono");
        assert_eq!(base.font.size, 16.0);
        // Theme not in profile → inherited.
        assert_eq!(base.theme.name, "weft-warm");
    }

    #[test]
    fn apply_overrides_does_not_touch_metadata() {
        let mut base = Config {
            active_profile: Some("work".into()),
            ..Config::default()
        };
        base.profiles.insert(
            "work".into(),
            ProfileConfig {
                font: Some(FontConfig {
                    family: "Mono".into(),
                    ..FontConfig::default()
                }),
                ..ProfileConfig::default()
            },
        );
        let profile = ProfileConfig {
            font: Some(FontConfig {
                family: "Other".into(),
                ..FontConfig::default()
            }),
            ..ProfileConfig::default()
        };
        apply_overrides(&mut base, &profile);
        // active_profile and profiles are preserved (not replaced by the
        // override's non-existent metadata).
        assert_eq!(base.active_profile.as_deref(), Some("work"));
        assert!(base.profiles.contains_key("work"));
    }

    #[test]
    fn resolve_none_active_returns_base() {
        let mut base = Config::default();
        base.font.family = "Base".into();
        let (eff, diags) = base.resolve_active_profile().unwrap();
        assert!(diags.is_empty());
        assert_eq!(eff.font.family, "Base");
    }

    #[test]
    fn resolve_empty_active_normalizes_to_base() {
        let base = Config {
            active_profile: Some(String::new()),
            ..Config::default()
        };
        let (eff, diags) = base.resolve_active_profile().unwrap();
        assert!(diags.is_empty());
        assert_eq!(eff.font.family, base.font.family);
    }

    #[test]
    fn resolve_missing_active_emits_diagnostic_and_uses_base() {
        let mut base = Config::default();
        base.font.family = "Base".into();
        base.active_profile = Some("missing".into());
        let (eff, diags) = base.resolve_active_profile().unwrap();
        assert_eq!(diags.len(), 1);
        assert_eq!(
            diags[0],
            ConfigDiagnostic::MissingActiveProfile("missing".into())
        );
        assert_eq!(eff.font.family, "Base");
    }

    #[test]
    fn resolve_active_applies_overrides() {
        let mut base = Config::default();
        base.font.family = "Base".into();
        base.active_profile = Some("work".into());
        base.profiles.insert(
            "work".into(),
            ProfileConfig {
                font: Some(FontConfig {
                    family: "Profile".into(),
                    ..FontConfig::default()
                }),
                ..ProfileConfig::default()
            },
        );
        let (eff, diags) = base.resolve_active_profile().unwrap();
        assert!(diags.is_empty());
        assert_eq!(eff.font.family, "Profile");
    }

    #[test]
    fn resolve_rejects_reserved_name() {
        let mut base = Config::default();
        base.profiles
            .insert("base".into(), ProfileConfig::default());
        let err = base.resolve_active_profile().unwrap_err();
        assert!(matches!(err, ProfileError::ReservedName(_)));
    }

    #[test]
    fn resolve_rejects_too_many_profiles() {
        let mut base = Config::default();
        for i in 0..33 {
            base.profiles
                .insert(format!("p{i}"), ProfileConfig::default());
        }
        let err = base.resolve_active_profile().unwrap_err();
        assert!(matches!(err, ProfileError::TooManyProfiles(33)));
    }

    #[test]
    fn resolve_preserves_profiles_on_effective() {
        let mut base = Config {
            active_profile: Some("work".into()),
            ..Config::default()
        };
        base.profiles.insert(
            "work".into(),
            ProfileConfig {
                font: Some(FontConfig {
                    family: "Profile".into(),
                    ..FontConfig::default()
                }),
                ..ProfileConfig::default()
            },
        );
        let (eff, _) = base.resolve_active_profile().unwrap();
        // effective retains profiles metadata for UI display.
        assert!(eff.profiles.contains_key("work"));
        assert_eq!(eff.active_profile.as_deref(), Some("work"));
    }
}
