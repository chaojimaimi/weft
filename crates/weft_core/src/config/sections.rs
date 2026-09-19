use crate::vt::TuiRenderMode;
use serde::{Deserialize, Serialize};

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
    /// v1.10.22: terminal selection highlight base color — now actually
    /// wired to the grid/block/prompt selection renderer (was dead config).
    /// `weft_app::paint::selection_color` guarantees WCAG 3:1 against the
    /// theme background regardless of this value. `None` keeps the built-in
    /// theme's selection.
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
    /// v1.11.6 (PLAN_v1116 M6/D-f): OSC 8 hyperlink underline color
    /// override (`"#rrggbb"`). Parsed to `Color` then /255-normalized into
    /// the f32-domain `Theme::link` — u8-granular by nature (architect
    /// P1-4). Invalid hex silently falls back to the base theme's value.
    pub link: Option<String>,
    /// v1.11.6 (PLAN_v1116 M6/D-f): `[theme.ui]` seed-color overrides.
    /// `None` keys keep the `UiColors` light/dark dual-branch defaults.
    pub ui: Option<UiConfig>,
    /// v1.12: 主题变体（`"dark"` / `"light"`）。导入外部主题时由转档脚本写入；
    /// 缺省时运行时按背景相对亮度推导（`Theme::is_dark`）。
    pub variant: Option<String>,
    /// v1.12: 主题作者（署名用，来自上游 `CREDITS.md` / base16 `author`）。
    pub author: Option<String>,
    /// v1.12: 上游来源（仓库或文件路径），便于追溯与更新。
    pub source: Option<String>,
    /// v1.12: 单个主题的许可标识（上游集合许可不等于单主题许可）。
    pub license: Option<String>,
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
///
/// v1.11.0: the `cwd` field was removed — dead config (painter derives
/// CWD gray from fg×0.65). A leftover `cwd = "..."` in user TOML is
/// silently ignored (serde's default unknown-key behavior).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct OutputSemanticConfig {
    /// v1.7.0-D: Master toggle for semantic fallback classification.
    /// `false` disables the classifier; ANSI styling is always preserved.
    pub enabled: Option<bool>,
    pub output_default: Option<String>,
    pub metadata: Option<String>,
    pub success: Option<String>,
    pub failure: Option<String>,
}

/// v1.11.6 (PLAN_v1116 M6/D-f): TOML-facing `[theme.ui]` seed-color
/// overrides — mirrors the keys of `ThemeUi`. All fields optional; absent
/// keys fall back to the `UiColors::from_theme` light/dark dual-branch
/// hardcodes (zero-config visuals unchanged). Present values replace the
/// dual-branch INPUT and still pass through the `ensure_contrast(4.5)`
/// gate — a user hex is not necessarily the final painted color.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub success: Option<String>,
    pub warning: Option<String>,
    pub error: Option<String>,
    pub find_match: Option<String>,
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
            link: None,
            ui: None,
            variant: None,
            author: None,
            source: None,
            license: None,
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
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct EditorConfig {
    /// If true, `Ctrl+Enter` submits and plain `Enter` inserts a newline
    /// (Warp default). If false (default), `Enter` submits and `Shift+Enter`
    /// inserts a newline.
    pub submit_on_ctrl_enter: bool,
    /// Enable semantic target selection with Cmd+Shift+Click. Safe external
    /// opening is a separate explicit Cmd+Option+Click gesture.
    pub smart_select: bool,
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self {
            submit_on_ctrl_enter: false,
            smart_select: true,
        }
    }
}

/// v1.11.1 (PLAN_v1111 §4.2): large-paste protection switches.
///
/// Both confirmations default ON; turning both off is the documented
/// one-switch rollback to the pre-v1.11.1 pass-through behavior. A
/// hand-edited `size_threshold_kib` outside [`PASTE_SIZE_TIERS_KIB`] is not
/// rejected at parse time — `weft_app::settings_validation::runtime_paste_config`
/// falls back to 16 KiB when the value is consumed (same philosophy as the
/// other `runtime_*` clamps).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct PasteConfig {
    /// Ask for confirmation when a paste exceeds `size_threshold_kib`.
    pub confirm_large: bool,
    /// Ask for confirmation when a paste contains dangerous control chars
    /// (ESC/NUL/DEL — see `weft_core::input::contains_dangerous_control_chars`).
    pub confirm_control_chars: bool,
    /// Size threshold in KiB. Legal tiers: [`PASTE_SIZE_TIERS_KIB`].
    pub size_threshold_kib: u32,
}

impl Default for PasteConfig {
    fn default() -> Self {
        Self {
            confirm_large: true,
            confirm_control_chars: true,
            size_threshold_kib: 16,
        }
    }
}

/// v1.11.1: legal Settings-UI steps for the paste size threshold (KiB).
/// The ←/→ row cycles this list; out-of-list stored values fall back to 16
/// at consumption time (`runtime_paste_config`, PLAN_v1111 §4.2).
pub const PASTE_SIZE_TIERS_KIB: [u32; 6] = [8, 16, 32, 64, 128, 256];

/// v1.11.2 X4 (PLAN_v1112 §1.2): in-memory command-block retention cap per
/// tab. Blocks beyond the limit are evicted from memory (oldest first) but
/// stay in SQLite and remain referenced by per-tab snapshots; the panel's
/// "load older" action pages them back in. `0` disables retention entirely.
///
/// This is a config-file power-user key on purpose: there is NO Settings UI
/// row in v1.11.2 (deliberate scope cut — PLAN_v1112 §1.2), so hand-editing
/// the TOML is the only way to change it. A value outside sane bounds is not
/// rejected at parse time; `0..2000` simply evicts more aggressively.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct BlocksConfig {
    pub retained_limit: usize,
}

impl Default for BlocksConfig {
    fn default() -> Self {
        Self {
            retained_limit: crate::blocks::retention::DEFAULT_BLOCKS_RETAINED_LIMIT,
        }
    }
}

/// v1.11.3 (PLAN_v1113 §3.3): terminal compatibility switches.
///
/// `bold_is_bright` (X11/XTerm convention): SGR 1 bold on an ANSI fg
/// palette index < 8 resolves to the bright variant (palette[i+8]) instead
/// of a separate bold weight/fake-bold. Default **false** — Weft keeps bold
/// as a weight so `ls --color` directories stay deep blue while text is
/// genuinely bold. No Settings UI row in v1.11.3 (deliberate — the plan's
/// (c) item is config-first); hand-edit the TOML or use a profile.
///
/// v1.11.4 (PLAN_v1114 §3): `kitty_keyboard` — the kitty keyboard protocol
/// master switch. Default **true**; `false` swallows all four `CSI ...u`
/// negotiation ops AND zeroes the encoder feed — a one-click rollback to
/// the pre-v1.11.4 byte stream (config_controller walks every pane).
///
/// No validation/clamp: plain bools.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct CompatConfig {
    /// Default false (derive) — Weft keeps bold as a weight, matching the
    /// pre-v1.11.3 rendering bit-for-bit.
    pub bold_is_bright: bool,
    /// Default true — the protocol is on unless the user disables it.
    pub kitty_keyboard: bool,
}

impl Default for CompatConfig {
    fn default() -> Self {
        Self {
            bold_is_bright: false,
            kitty_keyboard: true,
        }
    }
}

/// v1.11.5 (PLAN_v1115 §M8): OSC 52 clipboard access mode.
///
/// - `Default` — writes to the system clipboard pass through; read
///   requests prompt once per deny-cooldown window.
/// - `Off` — both directions are swallowed at the app gate (parsing still
///   happens; the events are dropped with a trace).
/// - `Unrestricted` — writes AND reads pass through silently. Warning: any
///   program inside the terminal (including an ssh remote) can then read
///   the clipboard without asking.
///
/// Unknown/illegal TOML values fall back to `Default` — a typo must never
/// fail the whole config parse (same philosophy as other runtime_* clamps).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Osc52Mode {
    #[default]
    Default,
    Off,
    Unrestricted,
}

impl Osc52Mode {
    /// Canonical TOML spelling (lowercase).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Off => "off",
            Self::Unrestricted => "unrestricted",
        }
    }

    /// Parse a TOML value; anything unrecognized → `Default` (never fails).
    pub fn parse(s: &str) -> Self {
        match s {
            "off" => Self::Off,
            "unrestricted" => Self::Unrestricted,
            _ => Self::Default,
        }
    }
}

impl<'de> serde::Deserialize<'de> for Osc52Mode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Ok(Self::parse(&raw))
    }
}

/// v1.11.5 (PLAN_v1115 §M8): `[clipboard]` section.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ClipboardConfig {
    pub osc52: Osc52Mode,
}

impl Default for ClipboardConfig {
    fn default() -> Self {
        Self {
            osc52: Osc52Mode::Default,
        }
    }
}

/// v1.11.5 (PLAN_v1115 §M8): `[notifications]` section.
///
/// - `enabled` — master switch (总闸). All four notification paths
///   (block completion, OSC 9, OSC 777) honor it.
/// - `threshold_secs` — minimum command runtime (block completion) before
///   a notification may post; exact equality hits (X7 window gate is
///   focus-based: only while the window is out of focus).
/// - `sound` — play the system sound alongside the banner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct NotificationsConfig {
    pub enabled: bool,
    pub threshold_secs: u64,
    pub sound: bool,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold_secs: 30,
            sound: false,
        }
    }
}

/// v1.11.7 (PLAN_v1117_SHADOW_BLOCK_VIEW §三 M1.2, D-d): `[experimental]`
/// section — experimental switches that are not yet stable enough for
/// Settings UI rows (config-file keys, like `[blocks]`/`[compat]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ExperimentalConfig {
    /// Primary-screen TUI render tier (`noninteractive|all|classic`, serde
    /// default `noninteractive` — the factory default; `Terminal::new` stays
    /// Classic and the app injects this value at construction, P2-3).
    /// `noninteractive` fixes the reported uv/ollama progress-bar class while
    /// keeping interactive TUIs on the classic takeover path; `all` is the
    /// Warp-terminal dogfood tier; `classic` is the v1.11.6 one-key rollback.
    pub tui_render_mode: TuiRenderMode,
}

impl Default for ExperimentalConfig {
    fn default() -> Self {
        Self {
            tui_render_mode: TuiRenderMode::Noninteractive,
        }
    }
}

#[cfg(test)]
mod paste_config_tests {
    use super::{PasteConfig, PASTE_SIZE_TIERS_KIB};
    #[test]
    fn defaults_confirm_both_risks_at_16kib() {
        let cfg = PasteConfig::default();
        assert!(cfg.confirm_large);
        assert!(cfg.confirm_control_chars);
        assert_eq!(cfg.size_threshold_kib, 16);
    }

    #[test]
    fn missing_section_deserializes_to_defaults() {
        let cfg: PasteConfig = toml::from_str("[paste]").unwrap();
        assert_eq!(cfg, PasteConfig::default());
    }

    #[test]
    fn threshold_tier_list_is_the_six_documented_steps() {
        assert_eq!(PASTE_SIZE_TIERS_KIB, [8, 16, 32, 64, 128, 256]);
        // The default must be one of the cycle steps so the Settings row can
        // round-trip its position.
        assert!(PASTE_SIZE_TIERS_KIB.contains(&PasteConfig::default().size_threshold_kib));
    }
}

#[cfg(test)]
mod blocks_config_tests {
    use super::BlocksConfig;

    /// v1.11.2 X4 (PLAN_v1112 §1.2): default matches the tracker default and
    /// the section is optional in TOML.
    #[test]
    fn defaults_to_tracker_retention_cap() {
        assert_eq!(
            BlocksConfig::default().retained_limit,
            crate::blocks::retention::DEFAULT_BLOCKS_RETAINED_LIMIT
        );
        let cfg: BlocksConfig = toml::from_str("[blocks]").unwrap();
        assert_eq!(cfg, BlocksConfig::default());
    }

    #[test]
    fn parses_hand_written_value_including_zero_disable() {
        // Deserialize from the section's own table body.
        let cfg: BlocksConfig = toml::from_str("retained_limit = 500").unwrap();
        assert_eq!(cfg.retained_limit, 500);
        let cfg: BlocksConfig = toml::from_str("retained_limit = 0").unwrap();
        assert_eq!(cfg.retained_limit, 0, "0 = retention disabled");
    }
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
    /// Max output tokens for a single completion. `None` ⇒ 4096.
    pub max_tokens: Option<u32>,
    /// Idle read timeout in seconds. The timer resets after every response
    /// chunk, so a healthy long generation may exceed it. `None` ⇒ 30.
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

    /// Effective idle read timeout (seconds). The timer resets after each
    /// successful read and falls back to 30s.
    pub fn effective_timeout_secs(&self) -> u64 {
        self.timeout_secs.unwrap_or(30) as u64
    }

    /// Effective max_tokens. Falls back to 4096. v1.8.8: raised from 1024
    /// because thinking-capable models (qwen3.5, gemma4) spend a large
    /// fraction of their token budget on internal reasoning before emitting
    /// visible content; 1024 was exhausted by thinking alone, yielding an
    /// empty completion. 4096 leaves ample room for thinking + answer.
    pub fn effective_max_tokens(&self) -> u32 {
        self.max_tokens.unwrap_or(4096)
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
        assert_eq!(cfg.effective_max_tokens(), 4096);
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
