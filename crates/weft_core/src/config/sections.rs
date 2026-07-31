use serde::Deserialize;

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
    /// Minimum contrast ratio applied to terminal text at paint time.
    /// `1.0` preserves the resolved ANSI/truecolor RGB exactly; higher values
    /// adjust only display lightness while stored color origins stay intact.
    pub minimum_contrast: f32,
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
    /// v1.0 S5: per-syntax-token color overrides. Each field is an optional
    /// hex string (`"#rrggbb"`); when present it overrides the base theme's
    /// `SyntaxColors` field of the same name. Applied after the inline
    /// color overrides in [`Theme::resolve_named`].
    pub syntax: Option<SyntaxConfig>,
    /// v1.7.0-B: output semantic color overrides. Each field is an optional
    /// hex string; when present it overrides the base theme's
    /// `OutputSemanticColors` field. Applied after syntax overrides.
    pub output: Option<OutputSemanticConfig>,
}

/// v1.0 S5: TOML-facing syntax color overrides. All fields optional; absent
/// fields inherit from the resolved base theme. Mirrors the fields of
/// [`SyntaxColors`].
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct SyntaxConfig {
    pub command: Option<String>,
    pub flag: Option<String>,
    pub path: Option<String>,
    pub string: Option<String>,
    pub number: Option<String>,
    pub variable: Option<String>,
    pub operator: Option<String>,
    pub comment: Option<String>,
    /// v1.7.0-B: plain argument color override.
    pub argument: Option<String>,
    pub default: Option<String>,
}

/// v1.7.0-B: TOML-facing output semantic color overrides. All fields
/// optional; absent fields inherit from the resolved base theme. Mirrors
/// the fields of [`OutputSemanticColors`].
///
/// v1.7.0-D: `enabled` controls the semantic fallback classifier. When
/// `false`, the classifier is skipped and unstyled output uses
/// `output_default` only. ANSI-styled output is NEVER affected by this
/// toggle. Defaults to `true` (enabled).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct OutputSemanticConfig {
    /// v1.7.0-D: Master toggle for semantic fallback classification.
    /// `false` disables the classifier; ANSI styling is always preserved.
    pub enabled: Option<bool>,
    pub output_default: Option<String>,
    pub cwd: Option<String>,
    pub metadata: Option<String>,
    pub success: Option<String>,
    pub failure: Option<String>,
}

// Manual Default (deriving would give name = "").
impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            name: "weft-warm".into(),
            minimum_contrast: 7.0,
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
            syntax: None,
            output: None,
        }
    }
}

impl ThemeConfig {
    /// v1.7.0-D: Returns whether the semantic output fallback classifier is
    /// enabled. Defaults to `true` when the `[theme.output]` section or
    /// `enabled` field is absent. When `false`, the classifier is skipped
    /// and unstyled output uses `output_default` only; ANSI styling is
    /// NEVER affected by this toggle.
    pub fn semantic_output_enabled(&self) -> bool {
        self.output.as_ref().and_then(|o| o.enabled).unwrap_or(true)
    }
}

/// F3-3: Sidebar width bounds in logical points. Applied at config load and
/// during drag so the persisted value stays in a usable range regardless of
/// where it's written/read. Shared with weft_app via the crate re-export.
pub const SIDEBAR_MIN_WIDTH: f32 = 240.0;
pub const SIDEBAR_MAX_WIDTH: f32 = 360.0;

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct WindowConfig {
    pub width: u32,
    pub height: u32,
    pub title: String,
    pub opacity: f32,
    pub padding_x: u32,
    pub padding_y: u32,
    /// F3-3: User-overridden sidebar width in logical points. `None` when
    /// the user hasn't dragged the sidebar (falls back to the responsive
    /// `SidebarMetrics::for_logical_width`). Clamped to [240, 360] on load.
    pub sidebar_width: Option<f32>,
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
            sidebar_width: None,
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

/// v1.0 Logo variant — the app icon shown in the Dock / app switcher.
/// Not theme-bound: the user picks a preferred variant in Settings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LogoVariant {
    /// Cool dark — `#0b0e14` bg + neon cyan W.
    /// Also the fallback for unknown config values.
    #[default]
    Cool,
    /// Warm dark — `#221c18` bg + amber W.
    Warm,
    /// Light — `#f5f5f7` bg + deep cyan W.
    Light,
    /// Transparent — no bg fill, only grid + W.
    Transparent,
}

impl<'de> Deserialize<'de> for LogoVariant {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(LogoVariant::from_str(&s))
    }
}

impl LogoVariant {
    /// All variants in display order.
    pub const ALL: [LogoVariant; 4] = [
        LogoVariant::Cool,
        LogoVariant::Warm,
        LogoVariant::Light,
        LogoVariant::Transparent,
    ];

    /// Human-readable label for the Settings UI.
    pub fn label(self) -> &'static str {
        match self {
            LogoVariant::Cool => "Cool (dark cyan)",
            LogoVariant::Warm => "Warm (dark amber)",
            LogoVariant::Light => "Light (pale cyan)",
            LogoVariant::Transparent => "Transparent",
        }
    }

    /// Identifier used in config.toml `[logo] variant = "..."`.
    pub fn as_str(self) -> &'static str {
        match self {
            LogoVariant::Cool => "cool",
            LogoVariant::Warm => "warm",
            LogoVariant::Light => "light",
            LogoVariant::Transparent => "transparent",
        }
    }

    /// Parse from a config string. Unknown values fall back to `Cool`.
    /// Infallible by design (never returns Err / always yields a valid variant).
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s.trim() {
            "warm" => LogoVariant::Warm,
            "light" => LogoVariant::Light,
            "transparent" => LogoVariant::Transparent,
            _ => LogoVariant::Cool,
        }
    }
}

/// v1.0 Logo config — Dock icon variant selection.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct LogoConfig {
    /// Selected logo variant.
    pub variant: LogoVariant,
}

impl Default for LogoConfig {
    fn default() -> Self {
        Self {
            variant: LogoVariant::Cool,
        }
    }
}

/// v1.8 AI integration: configuration for the local Ollama backend.
///
/// v1.3 carried an `api_key` field and supported OpenAI / Anthropic / custom
/// backends. v1.8 collapses to a single local Ollama provider per
/// `docs/V18_IMPLEMENTATION_PLAN.md` §1: no API keys, no public endpoints.
/// The `api_key` field is removed; the `base_url` is validated to be
/// loopback at construction time. The TOML writer (config/save.rs) only
/// persists non-default values, so an empty `[ai]` section stays empty.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default)]
pub struct AiConfig {
    /// Provider id. v1.8 only accepts `"ollama"`; any other value is
    /// rejected at backend construction. `None` ⇒ AI features disabled.
    pub provider: Option<String>,
    /// v1.8 REMOVED: `api_key` is no longer supported. The field is kept
    /// for TOML deserialization backwards-compat (ignored if present in
    /// old config files) so v1.7 configs don't fail to load on upgrade.
    #[serde(default, skip_serializing)]
    pub api_key: Option<String>,
    /// Ollama base URL. Must be loopback (`http://127.0.0.1:11434`,
    /// `http://localhost:11434`, or `http://[::1]:11434`). When `None`,
    /// defaults to `http://127.0.0.1:11434`.
    pub base_url: Option<String>,
    /// Model id, e.g. `"llama3.1"`, `"qwen2.5"`.
    pub model: Option<String>,
    /// Max output tokens for a single completion. `None` ⇒ 1024.
    pub max_tokens: Option<u32>,
    /// Request timeout in seconds. `None` ⇒ 30.
    pub timeout_secs: Option<u32>,
    /// Auto-diagnose failed blocks (exit_code != 0). Off by default —
    /// the user triggers diagnosis manually via the block action button
    /// until they opt in here.
    pub enable_error_diagnosis: bool,
    /// Enable natural-language → command generation in the palette. On by
    /// default so the "✨ Ask AI" entry is visible as soon as a provider
    /// is configured.
    pub enable_command_generation: bool,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            provider: None,
            api_key: None,
            base_url: None,
            model: None,
            max_tokens: None,
            timeout_secs: None,
            enable_error_diagnosis: false,
            enable_command_generation: true,
        }
    }
}

impl AiConfig {
    /// True when the AI features can be considered "configured". v1.8 only
    /// accepts `"ollama"`; old provider values (`openai`/`anthropic`/`custom`)
    /// are treated as unconfigured so the user sees a hint to switch.
    pub fn is_configured(&self) -> bool {
        match self.provider.as_deref() {
            None => false,
            Some("ollama") => true,
            _ => false,
        }
    }

    /// Effective request timeout (seconds). Falls back to 30s.
    pub fn effective_timeout_secs(&self) -> u64 {
        self.timeout_secs.unwrap_or(30) as u64
    }

    /// Effective max_tokens. Falls back to 1024 (a reasonable default for
    /// short shell-command generation / diagnosis).
    pub fn effective_max_tokens(&self) -> u32 {
        self.max_tokens.unwrap_or(1024)
    }

    /// Provider id, lowercased and trimmed, for `match` dispatch.
    pub fn provider_kind(&self) -> Option<&str> {
        self.provider
            .as_deref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
    }
}

#[cfg(test)]
mod ai_config_tests {
    use super::*;

    #[test]
    fn unconfigured_when_no_provider() {
        let cfg = AiConfig::default();
        assert!(!cfg.is_configured());
        assert_eq!(cfg.provider_kind(), None);
        assert_eq!(cfg.effective_timeout_secs(), 30);
        assert_eq!(cfg.effective_max_tokens(), 1024);
    }

    #[test]
    fn ollama_configured_without_api_key() {
        let cfg = AiConfig {
            provider: Some("ollama".into()),
            ..Default::default()
        };
        assert!(cfg.is_configured());
        assert_eq!(cfg.provider_kind(), Some("ollama"));
    }

    #[test]
    fn openai_no_longer_configured_in_v18() {
        // v1.8: only "ollama" is accepted. Old configs with "openai" /
        // "anthropic" / "custom" should be treated as unconfigured so the
        // user sees a hint to switch rather than a silent breakage.
        let cfg = AiConfig {
            provider: Some("openai".into()),
            api_key: Some("sk-test".into()),
            ..Default::default()
        };
        assert!(!cfg.is_configured());
    }

    #[test]
    fn anthropic_no_longer_configured_in_v18() {
        let cfg = AiConfig {
            provider: Some("anthropic".into()),
            api_key: Some("sk-ant-test".into()),
            ..Default::default()
        };
        assert!(!cfg.is_configured());
    }

    #[test]
    fn custom_no_longer_configured_in_v18() {
        let cfg = AiConfig {
            provider: Some("custom".into()),
            base_url: Some("https://internal.example.com/v1".into()),
            ..Default::default()
        };
        assert!(!cfg.is_configured());
    }

    #[test]
    fn empty_api_key_treated_as_unset() {
        // v1.8: api_key is ignored entirely, but the field is kept for
        // backwards-compat deserialization. Any value is "unset".
        let cfg = AiConfig {
            provider: Some("anthropic".into()),
            api_key: Some("   ".into()),
            ..Default::default()
        };
        assert!(!cfg.is_configured());
    }

    #[test]
    fn defaults_command_generation_on_diagnosis_off() {
        let cfg = AiConfig::default();
        assert!(cfg.enable_command_generation);
        assert!(!cfg.enable_error_diagnosis);
    }
}
