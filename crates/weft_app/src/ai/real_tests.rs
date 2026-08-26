//! Opt-in integration test against a real local Ollama daemon.
//!
//! Run with a model that is already installed locally:
//! `WEFT_OLLAMA_MODEL=qwen3.5:9b cargo test -p weft_app --bin weft \
//!   ai::real_tests::real_ollama_lists_models_and_completes -- --ignored --nocapture`

#![cfg(test)]

use std::sync::Arc;
use std::time::Duration;

use super::client::{fetch_ollama_models, is_loopback_url, AiBackend, CancelFlag, OllamaBackend};
use super::prompt::{ChatMessage, ChatRole};
use weft_core::config::AiConfig;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a running local Ollama daemon and WEFT_OLLAMA_MODEL"]
async fn real_ollama_lists_models_and_completes() {
    let model = std::env::var("WEFT_OLLAMA_MODEL")
        .expect("set WEFT_OLLAMA_MODEL to an installed local Ollama model");
    let base_url = std::env::var("WEFT_OLLAMA_BASE_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:11434".to_string());
    assert!(
        is_loopback_url(&base_url),
        "WEFT_OLLAMA_BASE_URL must remain an HTTP loopback endpoint"
    );

    let http = Arc::new(
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("build local Ollama client"),
    );
    let backend = OllamaBackend::new(
        http.clone(),
        AiConfig {
            provider: Some("ollama".into()),
            model: Some(model.clone()),
            base_url: Some(base_url.clone()),
            max_tokens: Some(64),
            ..Default::default()
        },
    )
    .expect("construct local Ollama backend");

    // v1.11.0: `OllamaBackend::list_models` was removed as dead code
    // (AUDIT_v1.10.39 / PLAN_v111); the discovery flow calls
    // `fetch_ollama_models` directly — the real-Ollama smoke test does
    // the same.
    let models = fetch_ollama_models(&http, &base_url)
        .await
        .expect("list models from local Ollama");
    assert!(
        models.iter().any(|candidate| candidate.name == model),
        "configured model {model:?} was not returned by /api/tags"
    );

    let reply = backend
        .complete(
            vec![
                ChatMessage {
                    role: ChatRole::System,
                    content: "Reply with exactly the word READY.".into(),
                },
                ChatMessage {
                    role: ChatRole::User,
                    content: "Confirm that the local model is responding.".into(),
                },
            ],
            CancelFlag::new(),
        )
        .await
        .expect("complete through the real local Ollama stream");
    assert!(!reply.trim().is_empty(), "Ollama returned an empty reply");
}
