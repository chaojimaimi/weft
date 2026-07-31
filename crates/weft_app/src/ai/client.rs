//! v1.8 AI integration — Ollama-only HTTP client.
//!
//! v1.3 carried scaffolding for OpenAI / Anthropic / custom OpenAI-compatible
//! backends. v1.8 collapses that to a single, local-only Ollama client per
//! `docs/V18_IMPLEMENTATION_PLAN.md` §1: no API keys, no public endpoints,
//! no cloud providers. The `AiBackend` trait is now `async` and accepts a
//! `CancellationToken`-style flag so callers can abort an in-flight stream
//! when the user closes the palette or switches blocks.
//!
//! Endpoints (both POST to `{base_url}/api/...`):
//!
//! | Endpoint     | Purpose              | Schema                                  |
//! |--------------|----------------------|-----------------------------------------|
//! | `/api/tags`  | list installed models| `{"models": [{"name","size","modified_at"}, ...]}` |
//! | `/api/chat`  | streaming chat       | NDJSON, one `{"message":{"content":Δ}}` per line, final `{"done":true}` |
//!
//! All HTTP goes through `reqwest` with `rustls-tls` so the macOS .app bundle
//! doesn't link OpenSSL. `base_url` is validated to be loopback at config
//! load time (`AiConfig::base_url`); this module additionally asserts it on
//! every call as defence in depth.

use std::sync::Arc;
use std::time::Duration;

use reqwest::Client as HttpClient;

use super::prompt::ChatMessage;

/// Hard ceiling on the response body size we'll accumulate from a single
/// `/api/chat` stream. 64 KiB is generous for shell commands + short
/// explanations and prevents a runaway model from filling memory.
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// Hard ceiling on a single NDJSON line. Ollama chunks are small, but a
/// malformed/malicious server could emit huge lines.
const MAX_LINE_BYTES: usize = 16 * 1024;

/// Errors returned by [`AiBackend::complete`]. Flattened for ergonomic
/// `match` at call sites.
#[derive(Debug, thiserror::Error)]
pub enum AiError {
    /// The configured provider id is unknown. v1.8 only accepts `"ollama"`.
    #[error("unknown AI provider: {0} (v1.8 supports only 'ollama')")]
    UnknownProvider(String),
    /// The HTTP request failed at the transport level (DNS, TLS, connect,
    /// read timeout, …).
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
    /// The provider returned a non-2xx response.
    #[error("provider returned status {status}: {body}")]
    Status { status: u16, body: String },
    /// The response body couldn't be parsed as the expected JSON shape.
    #[error("failed to parse provider response: {0}")]
    Parse(String),
    /// No model was configured. Ollama requires this.
    #[error("no model configured for provider {0}")]
    MissingModel(&'static str),
    /// The configured `base_url` is not a loopback endpoint. v1.8 refuses
    /// to talk to anything other than `127.0.0.1` / `localhost` / `::1`.
    #[error("non-loopback AI endpoint rejected: {0}")]
    NonLoopbackEndpoint(String),
    /// The provider returned an empty completion.
    #[error("provider returned an empty completion")]
    Empty,
    /// The response stream exceeded [`MAX_RESPONSE_BYTES`].
    #[error("response exceeded {0} byte limit")]
    ResponseTooLarge(usize),
    /// The caller cancelled the request via the cancel flag.
    #[error("request cancelled")]
    Cancelled,
}

/// Result alias for [`AiBackend::complete`].
pub type AiResult<T> = Result<T, AiError>;

/// A lightweight cancellation token. The background task checks `is_cancelled()`
/// between stream chunks; setting it to `true` causes the task to return
/// `Err(AiError::Cancelled)` at the next chunk boundary. Cheaper than
/// `tokio_util::sync::CancellationToken` and sufficient for our needs
/// (the v1.8 plan §3 step 4 explicitly allows this pattern).
#[derive(Clone, Default)]
pub struct CancelFlag {
    inner: Arc<std::sync::atomic::AtomicBool>,
}

impl CancelFlag {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.inner.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.inner.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// The single shape every AI backend implements. v1.8 has one impl
/// ([`OllamaBackend`]); the trait is kept so the test suite can substitute
/// a `MockBackend` without going through HTTP.
///
/// The method is `async` (no `block_on`), so the caller must be running on
/// a tokio runtime. The main `App` spawns requests via `tokio::spawn` on
/// its multi-thread runtime and drains results through a `crossbeam-channel`.
pub trait AiBackend: Send + Sync {
    /// Provider id (`"ollama"`). Used in error messages.
    #[allow(dead_code)]
    fn provider_id(&self) -> &'static str;

    /// Send `messages` to the model and return the assistant's full reply.
    /// The caller passes a [`CancelFlag`] so it can abort mid-stream.
    fn complete(
        &self,
        messages: Vec<ChatMessage>,
        cancel: CancelFlag,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = AiResult<String>> + Send + '_>>;
}

/// A discovered model from `/api/tags`.
#[derive(Debug, Clone, serde::Deserialize, PartialEq, Eq)]
#[allow(dead_code)]
pub struct TagModel {
    pub name: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub modified_at: String,
}

/// Shared builder for the [`reqwest::Client`]. Honours the `[ai] timeout_secs`
/// setting.
fn build_http_client(timeout: Duration) -> Result<HttpClient, AiError> {
    HttpClient::builder()
        .timeout(timeout)
        .build()
        .map_err(AiError::Network)
}

/// Build the concrete backend from the user's [`AiConfig`]. Returns
/// `Ok(None)` when AI is not configured (`provider = None`).
///
/// v1.8: only `"ollama"` is accepted. Any other provider id returns
/// `Err(AiError::UnknownProvider)`.
pub fn build_backend(
    cfg: &weft_core::config::AiConfig,
) -> Result<Option<Box<dyn AiBackend>>, AiError> {
    let Some(kind) = cfg.provider_kind() else {
        return Ok(None);
    };
    if kind != "ollama" {
        return Err(AiError::UnknownProvider(kind.to_string()));
    }
    let timeout = Duration::from_secs(cfg.effective_timeout_secs());
    let http = Arc::new(build_http_client(timeout)?);
    let backend = OllamaBackend::new(http, cfg.clone())?;
    Ok(Some(Box::new(backend)))
}

/// Validate that `base_url` points at loopback. Used at construction time
/// and re-checked per request as defence in depth.
pub fn is_loopback_url(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "http" {
        return false;
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return false;
    }
    matches!(
        parsed.host_str(),
        Some("127.0.0.1") | Some("localhost") | Some("[::1]") | Some("::1")
    )
}

/// v1.8.3: Resolve the effective Ollama base URL from an [`AiConfig`].
/// Falls back to `http://127.0.0.1:11434` when `base_url` is `None` or empty.
/// Does NOT validate loopback — the caller (Settings "Test Connection")
/// validates before sending a request.
pub fn effective_base_url(cfg: &weft_core::config::AiConfig) -> String {
    cfg.base_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "http://127.0.0.1:11434".to_string())
}

/// v1.8.3: `/api/tags` — list installed models from a given base URL. This
/// is a free function so the Settings "Test Connection" flow can query
/// `/api/tags` without constructing a full [`OllamaBackend`] (which requires
/// a `model` field — the user may not have picked one yet).
pub async fn fetch_ollama_models(http: &HttpClient, base_url: &str) -> AiResult<Vec<TagModel>> {
    let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
    let resp = http.get(&url).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(AiError::Status {
            status: status.as_u16(),
            body,
        });
    }
    let parsed: serde_json::Value = resp.json().await?;
    let models = parsed
        .get("models")
        .and_then(|m| m.as_array())
        .ok_or_else(|| AiError::Parse("missing 'models' array".into()))?;
    let out: Vec<TagModel> = models
        .iter()
        .filter_map(|m| serde_json::from_value(m.clone()).ok())
        .collect();
    Ok(out)
}

// ── Ollama ────────────────────────────────────────────────────────────

/// Ollama local backend. Default endpoint `http://127.0.0.1:11434`. The
/// model field is required (e.g. `"llama3.1"`). No auth.
pub struct OllamaBackend {
    http: Arc<HttpClient>,
    base_url: String,
    model: String,
}

impl OllamaBackend {
    pub fn new(
        http: Arc<HttpClient>,
        cfg: weft_core::config::AiConfig,
    ) -> Result<Self, AiError> {
        let model = cfg
            .model
            .clone()
            .filter(|m| !m.trim().is_empty())
            .ok_or(AiError::MissingModel("ollama"))?;
        let base_url = cfg
            .base_url
            .clone()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| "http://127.0.0.1:11434".to_string());
        if !is_loopback_url(&base_url) {
            return Err(AiError::NonLoopbackEndpoint(base_url));
        }
        Ok(Self {
            http,
            base_url,
            model,
        })
    }

    /// `/api/tags` — list installed models. Used by the Settings AI panel.
    /// Not part of the `AiBackend` trait because it's a discovery call, not
    /// a completion call. Delegates to [`fetch_ollama_models`].
    ///
    /// v1.8.3: Currently unused — the Settings "Test Connection" flow calls
    /// `fetch_ollama_models` directly (via `AiState::spawn_list_models`) so
    /// it can build a temporary HTTP client from a draft config without
    /// constructing a full `OllamaBackend`. Retained as a convenience method
    /// for future callers that already hold a backend handle.
    #[allow(dead_code)]
    pub async fn list_models(&self) -> AiResult<Vec<TagModel>> {
        fetch_ollama_models(&self.http, &self.base_url).await
    }

    /// `/api/chat` with `stream: true`. Returns the accumulated assistant
    /// content. Reads `bytes_stream()` and splits on newlines (NDJSON).
    async fn stream_chat(
        &self,
        messages: Vec<ChatMessage>,
        cancel: CancelFlag,
    ) -> AiResult<String> {
        let url = format!("{}/api/chat", self.base_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "model": self.model,
            "messages": messages.iter().map(|m| {
                serde_json::json!({
                    "role": m.role.as_str(),
                    "content": m.content,
                })
            }).collect::<Vec<_>>(),
            "stream": true,
        });

        let resp = self.http.post(&url).json(&body).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(AiError::Status {
                status: status.as_u16(),
                body,
            });
        }

        use futures_util::StreamExt as _;

        let mut byte_stream = resp.bytes_stream();
        let mut acc = String::new();
        let mut total_bytes = 0usize;
        // Per-call line buffer for NDJSON fragments that span chunk boundaries.
        let mut line_buf = String::new();

        while let Some(chunk_res) = byte_stream.next().await {
            if cancel.is_cancelled() {
                return Err(AiError::Cancelled);
            }
            let chunk = chunk_res?;
            total_bytes = total_bytes.saturating_add(chunk.len());
            if total_bytes > MAX_RESPONSE_BYTES {
                return Err(AiError::ResponseTooLarge(MAX_RESPONSE_BYTES));
            }
            // Append chunk to line buffer, then split out complete lines.
            line_buf.push_str(std::str::from_utf8(&chunk).map_err(|e| {
                AiError::Parse(format!("non-utf8 chunk in stream: {e}"))
            })?);
            while let Some(nl) = line_buf.find('\n') {
                let line: String = line_buf.drain(..=nl).collect();
                let line = line.trim_end_matches('\r').trim_end_matches('\n');
                if line.is_empty() {
                    continue;
                }
                if line.len() > MAX_LINE_BYTES {
                    return Err(AiError::Parse(format!(
                        "NDJSON line too long: {} bytes",
                        line.len()
                    )));
                }
                let parsed: serde_json::Value = serde_json::from_str(line)
                    .map_err(|e| AiError::Parse(format!("invalid NDJSON line: {e}")))?;
                // Check for error object.
                if let Some(err) = parsed.get("error").and_then(|e| e.as_str()) {
                    return Err(AiError::Status {
                        status: 500,
                        body: err.to_string(),
                    });
                }
                // Extract content delta.
                if let Some(content) = parsed
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_str())
                {
                    acc.push_str(content);
                }
                // `done: true` marks the final chunk.
                if parsed.get("done").and_then(|d| d.as_bool()).unwrap_or(false) {
                    if acc.trim().is_empty() {
                        return Err(AiError::Empty);
                    }
                    return Ok(acc);
                }
            }
        }
        // Stream ended without an explicit `done` — return what we have if
        // non-empty, else error.
        if acc.trim().is_empty() {
            Err(AiError::Empty)
        } else {
            Ok(acc)
        }
    }
}

impl AiBackend for OllamaBackend {
    fn provider_id(&self) -> &'static str {
        "ollama"
    }

    fn complete(
        &self,
        messages: Vec<ChatMessage>,
        cancel: CancelFlag,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = AiResult<String>> + Send + '_>> {
        Box::pin(async move { self.stream_chat(messages, cancel).await })
    }
}

// `futures_util::StreamExt` is used above to drive `resp.bytes_stream()`.
// It's a transitive dependency of reqwest 0.12 with the `stream` feature;
// declared explicitly in weft_app's Cargo.toml.

#[cfg(test)]
mod tests {
    use super::*;
    use weft_core::config::AiConfig;

    #[test]
    fn build_backend_returns_none_when_no_provider() {
        let cfg = AiConfig::default();
        let backend = build_backend(&cfg).expect("no error for default config");
        assert!(backend.is_none());
    }

    #[test]
    fn build_backend_rejects_non_ollama_provider() {
        let cfg = AiConfig {
            provider: Some("openai".into()),
            api_key: Some("sk-test".into()),
            model: Some("gpt-4o-mini".into()),
            ..Default::default()
        };
        match build_backend(&cfg) {
            Err(AiError::UnknownProvider(name)) => assert_eq!(name, "openai"),
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
        match build_backend(&cfg) {
            Err(AiError::MissingModel(p)) => assert_eq!(p, "ollama"),
            Err(other) => panic!("expected MissingModel, got {other:?}"),
            Ok(_) => panic!("expected MissingModel error, got a backend"),
        }
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
    fn build_backend_rejects_non_loopback_base_url() {
        let cfg = AiConfig {
            provider: Some("ollama".into()),
            model: Some("llama3.1".into()),
            base_url: Some("https://api.openai.com".into()),
            ..Default::default()
        };
        match build_backend(&cfg) {
            Err(AiError::NonLoopbackEndpoint(u)) => assert_eq!(u, "https://api.openai.com"),
            Err(other) => panic!("expected NonLoopbackEndpoint, got {other:?}"),
            Ok(_) => panic!("expected NonLoopbackEndpoint error, got a backend"),
        }
    }

    #[test]
    fn build_backend_accepts_loopback_variants() {
        for url in [
            "http://127.0.0.1:11434",
            "http://localhost:11434",
            "http://[::1]:11434",
        ] {
            assert!(is_loopback_url(url), "should accept {url}");
        }
    }

    #[test]
    fn build_backend_rejects_https_scheme() {
        assert!(!is_loopback_url("https://127.0.0.1:11434"));
    }

    #[test]
    fn build_backend_rejects_userinfo() {
        assert!(!is_loopback_url("http://user:pass@127.0.0.1:11434"));
    }

    #[test]
    fn build_backend_rejects_lan_host() {
        assert!(!is_loopback_url("http://192.168.1.5:11434"));
        assert!(!is_loopback_url("http://my-server.local:11434"));
    }

    #[test]
    fn cancel_flag_default_is_false() {
        let f = CancelFlag::new();
        assert!(!f.is_cancelled());
    }

    #[test]
    fn cancel_flag_round_trip() {
        let f = CancelFlag::new();
        f.cancel();
        assert!(f.is_cancelled());
    }

    #[test]
    fn cancel_flag_clone_shares_state() {
        let f = CancelFlag::new();
        let g = f.clone();
        f.cancel();
        assert!(g.is_cancelled());
    }
}
