//! v1.3 AI integration — HTTP client abstraction + provider implementations.
//!
//! The trait [`AiBackend`] is the single surface the rest of the app talks
//! to. It is `async` and takes a `&self`, so implementations must be
//! cheaply cloneable (we wrap a `reqwest::Client` in an `Arc`).
//!
//! Three providers are supported out of the box:
//!
//! | Provider   | Endpoint                          | Auth           |
//! |------------|-----------------------------------|----------------|
//! | `ollama`   | `http://localhost:11434/api/chat` | none           |
//! | `openai`   | `https://api.openai.com/v1/chat/completions` | Bearer key |
//! | `anthropic`| `https://api.anthropic.com/v1/messages`       | `x-api-key`  |
//!
//! Plus a `custom` provider that points at a user-supplied base URL and
//! speaks the OpenAI chat-completions schema (corporate proxies, LiteLLM,
//! etc.). All HTTP calls go through `reqwest` with `rustls-tls` so we don't
//! link OpenSSL into the macOS .app bundle.
//!
//! Errors are flattened into [`AiError`] so callers don't need to match on
//! reqwest / serde / std::io individually.

use std::sync::Arc;
use std::time::Duration;

use reqwest::Client as HttpClient;

use super::prompt::{ChatMessage, ChatRole};

/// Errors returned by [`AiBackend::complete`]. Flattened for ergonomic
/// `match` at call sites — the variant carries enough context to produce a
/// user-facing message without re-parsing.
#[derive(Debug, thiserror::Error)]
pub enum AiError {
    /// The configured provider id is unknown to Weft. Caller should suggest
    /// fixing the `[ai] provider = "..."` line in config.toml.
    #[error("unknown AI provider: {0}")]
    UnknownProvider(String),
    /// The HTTP request failed at the transport level (DNS, TLS, connect,
    /// read timeout, …). The underlying `reqwest::Error` is preserved so the
    /// caller can distinguish `is_connect()` / `is_timeout()` if desired.
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
    /// The provider returned a non-2xx response. The body is included when
    /// available because most providers put a useful error message in it
    /// (e.g. `{"error": {"message": "invalid_api_key"}}`).
    #[error("provider returned status {status}: {body}")]
    Status { status: u16, body: String },
    /// The response body couldn't be parsed as the expected JSON shape.
    /// Usually means the provider changed their API or returned an HTML
    /// error page.
    #[error("failed to parse provider response: {0}")]
    Parse(String),
    /// No API key was configured for a provider that requires one.
    #[error("missing API key for provider {0}")]
    MissingApiKey(&'static str),
    /// No model was configured. Providers require this, so we fail fast
    /// rather than letting the request go out and 400.
    #[error("no model configured for provider {0}")]
    MissingModel(&'static str),
    /// The provider returned an empty completion. Distinct from a parse
    /// error because the JSON was valid, just empty.
    #[error("provider returned an empty completion")]
    Empty,
}

/// Result alias for [`AiBackend::complete`].
pub type AiResult = Result<String, AiError>;

/// The single shape every AI backend implements. The `complete` method
/// takes the chat messages built by [`super::prompt`] and returns the
/// assistant's text response.
///
/// Implementations are expected to be cheaply cloneable — `Arc<HttpClient>`
/// inside — so the trait is `Clone + Send + Sync`. The `async` method
/// makes the trait object-safe via `async-trait`-style desugaring if we
/// ever need `Box<dyn AiBackend>`; for now we use generics.
pub trait AiBackend: Send + Sync {
    /// Provider id (`"ollama"`, `"openai"`, …). Used in error messages.
    fn provider_id(&self) -> &'static str;

    /// Send `messages` to the model and return the assistant's reply.
    fn complete(&self, messages: &[ChatMessage]) -> AiResult;
}

/// Shared builder for the [`reqwest::Client`] used by all providers.
/// Honours the `[ai] timeout_secs` setting.
fn build_http_client(timeout: Duration) -> Result<HttpClient, AiError> {
    HttpClient::builder()
        .timeout(timeout)
        .build()
        .map_err(AiError::Network)
}

/// Build the concrete backend from the user's [`AiConfig`]. Returns
/// `Ok(None)` when AI is not configured (`provider = None` or insufficient
/// credentials) so the caller can decide whether to surface a hint in the UI
/// or just hide the "✨ Ask AI" entry.
pub fn build_backend(
    cfg: &weft_core::config::AiConfig,
) -> Result<Option<Box<dyn AiBackend>>, AiError> {
    let Some(kind) = cfg.provider_kind() else {
        return Ok(None);
    };
    let timeout = Duration::from_secs(cfg.effective_timeout_secs());
    let http = Arc::new(build_http_client(timeout)?);
    let backend: Box<dyn AiBackend> = match kind {
        "ollama" => Box::new(OllamaBackend::new(http, cfg.clone())?),
        "openai" => Box::new(OpenAiBackend::new(http, cfg.clone())?),
        "anthropic" => Box::new(AnthropicBackend::new(http, cfg.clone())?),
        "custom" => Box::new(CustomBackend::new(http, cfg.clone())?),
        other => return Err(AiError::UnknownProvider(other.to_string())),
    };
    Ok(Some(backend))
}

// ── Ollama ────────────────────────────────────────────────────────────

/// Ollama local backend. Default endpoint `http://localhost:11434`. The
/// model field is required (e.g. `"llama3.1"`). No auth.
pub struct OllamaBackend {
    http: Arc<HttpClient>,
    base_url: String,
    model: String,
}

impl OllamaBackend {
    pub fn new(http: Arc<HttpClient>, cfg: weft_core::config::AiConfig) -> Result<Self, AiError> {
        let model = cfg
            .model
            .clone()
            .filter(|m| !m.trim().is_empty())
            .ok_or(AiError::MissingModel("ollama"))?;
        let base_url = cfg
            .base_url
            .clone()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| "http://localhost:11434".to_string());
        Ok(Self {
            http,
            base_url,
            model,
        })
    }
}

impl AiBackend for OllamaBackend {
    fn provider_id(&self) -> &'static str {
        "ollama"
    }

    fn complete(&self, messages: &[ChatMessage]) -> AiResult {
        // Ollama /api/chat takes `messages: [{role, content}]` and returns
        // `{"message": {"role": "assistant", "content": "..."}}`. We use
        // the non-streaming endpoint for simplicity; the main loop already
        // shows the result asynchronously via a channel.
        let url = format!("{}/api/chat", self.base_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "model": self.model,
            "messages": messages.iter().map(|m| {
                serde_json::json!({
                    "role": m.role.as_str(),
                    "content": m.content,
                })
            }).collect::<Vec<_>>(),
            "stream": false,
        });

        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|e| AiError::Parse(format!("no tokio runtime: {e}")))?;
        let outcome: Result<String, AiError> = runtime.block_on(async {
            let resp = self.http.post(&url).json(&body).send().await?;
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(AiError::Status {
                    status: status.as_u16(),
                    body,
                });
            }
            let parsed: serde_json::Value = resp.json().await?;
            let content = parsed
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_str())
                .ok_or_else(|| AiError::Parse("missing message.content".into()))?;
            if content.is_empty() {
                return Err(AiError::Empty);
            }
            Ok(content.to_string())
        });
        outcome
    }
}

// ── OpenAI ────────────────────────────────────────────────────────────

/// OpenAI chat-completions backend. Endpoint
/// `https://api.openai.com/v1/chat/completions`, `Authorization: Bearer
/// <key>`. Required config: `api_key`, `model`.
pub struct OpenAiBackend {
    http: Arc<HttpClient>,
    endpoint: String,
    api_key: String,
    model: String,
    max_tokens: u32,
}

impl OpenAiBackend {
    pub fn new(http: Arc<HttpClient>, cfg: weft_core::config::AiConfig) -> Result<Self, AiError> {
        let api_key = cfg
            .api_key
            .clone()
            .filter(|k| !k.trim().is_empty())
            .ok_or(AiError::MissingApiKey("openai"))?;
        let model = cfg
            .model
            .clone()
            .filter(|m| !m.trim().is_empty())
            .ok_or(AiError::MissingModel("openai"))?;
        let endpoint = cfg
            .base_url
            .clone()
            .filter(|u| !u.trim().is_empty())
            .map(|u| format!("{}/chat/completions", u.trim_end_matches('/')))
            .unwrap_or_else(|| "https://api.openai.com/v1/chat/completions".to_string());
        Ok(Self {
            http,
            endpoint,
            api_key,
            model,
            max_tokens: cfg.effective_max_tokens(),
        })
    }
}

impl AiBackend for OpenAiBackend {
    fn provider_id(&self) -> &'static str {
        "openai"
    }

    fn complete(&self, messages: &[ChatMessage]) -> AiResult {
        let url = self.endpoint.clone();
        let body = serde_json::json!({
            "model": self.model,
            "messages": messages.iter().map(|m| {
                serde_json::json!({
                    "role": m.role.as_str(),
                    "content": m.content,
                })
            }).collect::<Vec<_>>(),
            "max_tokens": self.max_tokens,
            "stream": false,
        });
        let api_key = self.api_key.clone();

        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|e| AiError::Parse(format!("no tokio runtime: {e}")))?;
        let outcome: Result<String, AiError> = runtime.block_on(async move {
            let resp = self
                .http
                .post(&url)
                .bearer_auth(&api_key)
                .json(&body)
                .send()
                .await?;
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(AiError::Status {
                    status: status.as_u16(),
                    body,
                });
            }
            let parsed: serde_json::Value = resp.json().await?;
            // OpenAI: choices[0].message.content
            let content = parsed
                .get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("message"))
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_str())
                .ok_or_else(|| AiError::Parse("missing choices[0].message.content".into()))?;
            if content.is_empty() {
                return Err(AiError::Empty);
            }
            Ok(content.to_string())
        });
        outcome
    }
}

// ── Anthropic ─────────────────────────────────────────────────────────

/// Anthropic Messages API backend. Endpoint
/// `https://api.anthropic.com/v1/messages`, header `x-api-key: <key>`.
/// Required config: `api_key`, `model`. The Anthropic schema separates the
/// system prompt from the message list — we lift the first `system` message
/// out of `messages` and pass it as the top-level `system` field.
pub struct AnthropicBackend {
    http: Arc<HttpClient>,
    endpoint: String,
    api_key: String,
    model: String,
    max_tokens: u32,
}

impl AnthropicBackend {
    pub fn new(http: Arc<HttpClient>, cfg: weft_core::config::AiConfig) -> Result<Self, AiError> {
        let api_key = cfg
            .api_key
            .clone()
            .filter(|k| !k.trim().is_empty())
            .ok_or(AiError::MissingApiKey("anthropic"))?;
        let model = cfg
            .model
            .clone()
            .filter(|m| !m.trim().is_empty())
            .ok_or(AiError::MissingModel("anthropic"))?;
        let endpoint = cfg
            .base_url
            .clone()
            .filter(|u| !u.trim().is_empty())
            .map(|u| format!("{}/messages", u.trim_end_matches('/')))
            .unwrap_or_else(|| "https://api.anthropic.com/v1/messages".to_string());
        Ok(Self {
            http,
            endpoint,
            api_key,
            model,
            max_tokens: cfg.effective_max_tokens(),
        })
    }
}

impl AiBackend for AnthropicBackend {
    fn provider_id(&self) -> &'static str {
        "anthropic"
    }

    fn complete(&self, messages: &[ChatMessage]) -> AiResult {
        // Anthropic expects `system` as a top-level string, with the
        // `messages` array containing only `user`/`assistant` turns.
        let mut system_text = String::new();
        let mut turns: Vec<serde_json::Value> = Vec::with_capacity(messages.len());
        for m in messages {
            match m.role {
                ChatRole::System => {
                    if !system_text.is_empty() {
                        system_text.push('\n');
                    }
                    system_text.push_str(&m.content);
                }
                ChatRole::User | ChatRole::Assistant => {
                    turns.push(serde_json::json!({
                        "role": m.role.as_str(),
                        "content": m.content,
                    }));
                }
            }
        }

        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "system": system_text,
            "messages": turns,
        });
        let endpoint = self.endpoint.clone();
        let api_key = self.api_key.clone();

        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|e| AiError::Parse(format!("no tokio runtime: {e}")))?;
        let outcome: Result<String, AiError> = runtime.block_on(async move {
            let resp = self
                .http
                .post(&endpoint)
                .header("x-api-key", &api_key)
                .header("anthropic-version", "2023-06-01")
                .json(&body)
                .send()
                .await?;
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(AiError::Status {
                    status: status.as_u16(),
                    body,
                });
            }
            let parsed: serde_json::Value = resp.json().await?;
            // Anthropic: content[0].text (array of content blocks)
            let content = parsed
                .get("content")
                .and_then(|c| c.get(0))
                .and_then(|b| b.get("text"))
                .and_then(|t| t.as_str())
                .ok_or_else(|| AiError::Parse("missing content[0].text".into()))?;
            if content.is_empty() {
                return Err(AiError::Empty);
            }
            Ok(content.to_string())
        });
        outcome
    }
}

// ── Custom (OpenAI-compatible) ────────────────────────────────────────

/// Generic OpenAI-compatible backend pointed at a user-supplied base URL.
/// Used for LiteLLM, corporate proxies, Azure OpenAI (when configured with
/// a custom deployment URL), etc. Speaks the OpenAI chat-completions
/// schema verbatim.
pub struct CustomBackend {
    http: Arc<HttpClient>,
    endpoint: String,
    api_key: Option<String>,
    model: String,
    max_tokens: u32,
}

impl CustomBackend {
    pub fn new(http: Arc<HttpClient>, cfg: weft_core::config::AiConfig) -> Result<Self, AiError> {
        let base = cfg
            .base_url
            .clone()
            .filter(|u| !u.trim().is_empty())
            .ok_or(AiError::MissingModel("custom (base_url)"))?;
        let model = cfg
            .model
            .clone()
            .filter(|m| !m.trim().is_empty())
            .ok_or(AiError::MissingModel("custom"))?;
        let endpoint = format!("{}/chat/completions", base.trim_end_matches('/'));
        Ok(Self {
            http,
            endpoint,
            api_key: cfg.api_key.clone().filter(|k| !k.trim().is_empty()),
            model,
            max_tokens: cfg.effective_max_tokens(),
        })
    }
}

impl AiBackend for CustomBackend {
    fn provider_id(&self) -> &'static str {
        "custom"
    }

    fn complete(&self, messages: &[ChatMessage]) -> AiResult {
        let body = serde_json::json!({
            "model": self.model,
            "messages": messages.iter().map(|m| {
                serde_json::json!({
                    "role": m.role.as_str(),
                    "content": m.content,
                })
            }).collect::<Vec<_>>(),
            "max_tokens": self.max_tokens,
            "stream": false,
        });
        let endpoint = self.endpoint.clone();
        let api_key = self.api_key.clone();

        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|e| AiError::Parse(format!("no tokio runtime: {e}")))?;
        let outcome: Result<String, AiError> = runtime.block_on(async move {
            let req = self.http.post(&endpoint).json(&body);
            let req = match &api_key {
                Some(k) => req.bearer_auth(k),
                None => req,
            };
            let resp = req.send().await?;
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(AiError::Status {
                    status: status.as_u16(),
                    body,
                });
            }
            let parsed: serde_json::Value = resp.json().await?;
            let content = parsed
                .get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("message"))
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_str())
                .ok_or_else(|| AiError::Parse("missing choices[0].message.content".into()))?;
            if content.is_empty() {
                return Err(AiError::Empty);
            }
            Ok(content.to_string())
        });
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use weft_core::config::AiConfig;

    #[test]
    fn build_backend_returns_none_when_no_provider() {
        let cfg = AiConfig::default();
        let backend = build_backend(&cfg).expect("no error for default config");
        assert!(
            backend.is_none(),
            "default config should produce no backend"
        );
    }

    #[test]
    fn build_backend_unknown_provider_errors() {
        let cfg = AiConfig {
            provider: Some("magic".into()),
            ..Default::default()
        };
        // `expect_err` requires `T: Debug`; `Box<dyn AiBackend>` isn't Debug.
        match build_backend(&cfg) {
            Err(AiError::UnknownProvider(name)) => assert_eq!(name, "magic"),
            Err(other) => panic!("expected UnknownProvider, got {other:?}"),
            Ok(_) => panic!("expected UnknownProvider error, got a backend"),
        }
    }

    #[test]
    fn build_backend_ollama_without_model_errors() {
        let cfg = AiConfig {
            provider: Some("ollama".into()),
            ..Default::default()
        };
        // `expect_err` needs `T: Debug`; `Box<dyn AiBackend>` isn't Debug, so
        // we match on the Ok variant and route the test that way.
        match build_backend(&cfg) {
            Err(AiError::MissingModel(p)) => assert_eq!(p, "ollama"),
            Err(other) => panic!("expected MissingModel, got {other:?}"),
            Ok(_) => panic!("expected MissingModel error, got a backend"),
        }
    }

    #[test]
    fn build_backend_openai_without_api_key_errors() {
        let cfg = AiConfig {
            provider: Some("openai".into()),
            model: Some("gpt-4o-mini".into()),
            ..Default::default()
        };
        match build_backend(&cfg) {
            Err(AiError::MissingApiKey(p)) => assert_eq!(p, "openai"),
            Err(other) => panic!("expected MissingApiKey, got {other:?}"),
            Ok(_) => panic!("expected MissingApiKey error, got a backend"),
        }
    }

    #[test]
    fn build_backend_openai_with_key_and_model_succeeds() {
        let cfg = AiConfig {
            provider: Some("openai".into()),
            api_key: Some("sk-test".into()),
            model: Some("gpt-4o-mini".into()),
            ..Default::default()
        };
        let backend = build_backend(&cfg).expect("valid config should produce a backend");
        assert!(backend.is_some());
        assert_eq!(backend.unwrap().provider_id(), "openai");
    }

    #[test]
    fn build_backend_anthropic_with_key_and_model_succeeds() {
        let cfg = AiConfig {
            provider: Some("anthropic".into()),
            api_key: Some("sk-ant-test".into()),
            model: Some("claude-3-5-sonnet".into()),
            ..Default::default()
        };
        let backend = build_backend(&cfg).expect("valid config should produce a backend");
        assert!(backend.is_some());
        assert_eq!(backend.unwrap().provider_id(), "anthropic");
    }

    #[test]
    fn build_backend_ollama_succeeds_with_just_model() {
        let cfg = AiConfig {
            provider: Some("ollama".into()),
            model: Some("llama3.1".into()),
            ..Default::default()
        };
        let backend = build_backend(&cfg).expect("ollama needs no api_key");
        assert!(backend.is_some());
        assert_eq!(backend.unwrap().provider_id(), "ollama");
    }

    #[test]
    fn build_backend_custom_requires_base_url() {
        let cfg = AiConfig {
            provider: Some("custom".into()),
            model: Some("gpt-4o".into()),
            ..Default::default()
        };
        match build_backend(&cfg) {
            Err(AiError::MissingModel(p)) => assert_eq!(p, "custom (base_url)"),
            Err(other) => panic!("expected MissingModel for custom base_url, got {other:?}"),
            Ok(_) => panic!("expected MissingModel error, got a backend"),
        }
    }

    #[test]
    fn build_backend_custom_with_base_url_succeeds() {
        let cfg = AiConfig {
            provider: Some("custom".into()),
            base_url: Some("https://internal.example.com/v1".into()),
            model: Some("gpt-4o".into()),
            ..Default::default()
        };
        let backend = build_backend(&cfg).expect("custom with base_url should build");
        assert!(backend.is_some());
        assert_eq!(backend.unwrap().provider_id(), "custom");
    }
}
