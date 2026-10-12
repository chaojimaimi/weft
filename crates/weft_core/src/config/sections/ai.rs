//! `[ai]` section config (v1.8 local Ollama backend), moved verbatim out
//! of `sections.rs` (v1.13.9 backlog clear zero-behavior file-budget
//! split). Re-exported from `sections` so every existing
//! `config::AiConfig` path keeps resolving unchanged.

use serde::Deserialize;

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
