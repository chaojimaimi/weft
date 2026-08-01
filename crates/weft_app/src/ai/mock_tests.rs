//! v1.8.0 integration tests for the Ollama backend against a wiremock mock
//! server. Covers `/api/tags` and `/api/chat` (streaming NDJSON) success
//! and error paths. No real Ollama required.
//!
//! Run: `cargo test -p weft_app --bin weft ai::mock_tests`

#![cfg(test)]

use std::sync::Arc;
use std::time::Duration;

use super::client::{is_loopback_url, AiBackend, AiError, CancelFlag, OllamaBackend};
use super::prompt::{ChatMessage, ChatRole};
use weft_core::config::AiConfig;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Build an OllamaBackend pointing at `base_url` with model `test-model`.
fn backend_at(base_url: &str) -> OllamaBackend {
    let http = Arc::new(
        reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap(),
    );
    let cfg = AiConfig {
        provider: Some("ollama".into()),
        model: Some("test-model".into()),
        base_url: Some(base_url.into()),
        ..Default::default()
    };
    OllamaBackend::new(http, cfg).expect("backend construction should succeed")
}

fn simple_messages() -> Vec<ChatMessage> {
    vec![
        ChatMessage {
            role: ChatRole::System,
            content: "test system".into(),
        },
        ChatMessage {
            role: ChatRole::User,
            content: "test user".into(),
        },
    ]
}

// ── /api/tags ─────────────────────────────────────────────────────────

#[tokio::test]
async fn list_models_returns_models_on_success() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [
                {"name": "llama3.1", "size": 4_932_946_240_u64, "modified_at": "2026-07-01T00:00:00Z"},
                {"name": "qwen2.5", "size": 5_000_000_000_u64, "modified_at": "2026-07-15T00:00:00Z"}
            ]
        })))
        .mount(&server)
        .await;

    // wiremock binds to 127.0.0.1, so it passes the loopback check.
    let base_url = server.uri();
    assert!(
        is_loopback_url(&base_url),
        "wiremock URI should be loopback: {base_url}"
    );
    let backend = backend_at(&base_url);
    let models = backend
        .list_models()
        .await
        .expect("list_models should succeed");
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].name, "llama3.1");
    assert_eq!(models[1].name, "qwen2.5");
    assert_eq!(models[0].size, 4932946240);
}

#[tokio::test]
async fn list_models_returns_empty_when_no_models() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"models": []})))
        .mount(&server)
        .await;

    let backend = backend_at(&server.uri());
    let models = backend.list_models().await.expect("should succeed");
    assert!(models.is_empty());
}

#[tokio::test]
async fn list_models_errors_on_non_200() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
        .mount(&server)
        .await;

    let backend = backend_at(&server.uri());
    match backend.list_models().await {
        Err(AiError::Status { status, body }) => {
            assert_eq!(status, 500);
            assert!(body.contains("internal error"));
        }
        Err(other) => panic!("expected Status error, got {other:?}"),
        Ok(_) => panic!("expected error, got models"),
    }
}

#[tokio::test]
async fn list_models_errors_on_malformed_json() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json at all"))
        .mount(&server)
        .await;

    let backend = backend_at(&server.uri());
    match backend.list_models().await {
        Err(AiError::Network(_)) | Err(AiError::Parse(_)) => {}
        Err(other) => panic!("expected Network or Parse error, got {other:?}"),
        Ok(_) => panic!("expected error, got models"),
    }
}

#[tokio::test]
async fn list_models_errors_when_models_field_missing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"unrelated": "field"})),
        )
        .mount(&server)
        .await;

    let backend = backend_at(&server.uri());
    match backend.list_models().await {
        Err(AiError::Parse(msg)) => assert!(msg.contains("models")),
        Err(other) => panic!("expected Parse error, got {other:?}"),
        Ok(_) => panic!("expected error, got models"),
    }
}

#[tokio::test]
async fn list_models_errors_on_connection_refused() {
    // Bind a socket then immediately drop it to get a free port that refuses
    // connections.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let backend = backend_at(&format!("http://127.0.0.1:{port}"));
    match backend.list_models().await {
        Err(AiError::Network(_)) => {}
        Err(other) => panic!("expected Network error, got {other:?}"),
        Ok(_) => panic!("expected error, got models"),
    }
}

// ── /api/chat streaming ──────────────────────────────────────────────

#[tokio::test]
async fn chat_stream_accumulates_content_deltas() {
    let server = MockServer::start().await;
    // NDJSON: three chunks then a final done:true.
    let body = "{\"message\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"done\":false}\n\
                {\"message\":{\"role\":\"assistant\",\"content\":\", \"},\"done\":false}\n\
                {\"message\":{\"role\":\"assistant\",\"content\":\"world!\"},\"done\":true}\n";
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(body.as_bytes(), "application/x-ndjson"),
        )
        .mount(&server)
        .await;

    let backend = backend_at(&server.uri());
    let result = backend.complete(simple_messages(), CancelFlag::new()).await;
    assert_eq!(result.expect("stream should succeed"), "Hello, world!");
}

#[tokio::test]
async fn chat_stream_handles_utf8_content() {
    let server = MockServer::start().await;
    let body = "{\"message\":{\"content\":\"查找文件\"},\"done\":true}\n";
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(body.as_bytes(), "application/x-ndjson"),
        )
        .mount(&server)
        .await;

    let backend = backend_at(&server.uri());
    let result = backend.complete(simple_messages(), CancelFlag::new()).await;
    assert_eq!(result.expect("stream should succeed"), "查找文件");
}

#[tokio::test]
async fn chat_stream_errors_on_empty_completion() {
    let server = MockServer::start().await;
    let body = "{\"message\":{\"content\":\"\"},\"done\":true}\n";
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(body.as_bytes(), "application/x-ndjson"),
        )
        .mount(&server)
        .await;

    let backend = backend_at(&server.uri());
    match backend.complete(simple_messages(), CancelFlag::new()).await {
        Err(AiError::Empty) => {}
        Err(other) => panic!("expected Empty, got {other:?}"),
        Ok(s) => panic!("expected Empty error, got {s:?}"),
    }
}

#[tokio::test]
async fn chat_stream_errors_on_non_200() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
        .mount(&server)
        .await;

    let backend = backend_at(&server.uri());
    match backend.complete(simple_messages(), CancelFlag::new()).await {
        Err(AiError::Status { status, .. }) => assert_eq!(status, 404),
        Err(other) => panic!("expected Status error, got {other:?}"),
        Ok(_) => panic!("expected error, got content"),
    }
}

#[tokio::test]
async fn chat_stream_errors_on_provider_error_object() {
    let server = MockServer::start().await;
    let body = "{\"error\":\"model not found\"}\n";
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(body.as_bytes(), "application/x-ndjson"),
        )
        .mount(&server)
        .await;

    let backend = backend_at(&server.uri());
    match backend.complete(simple_messages(), CancelFlag::new()).await {
        Err(AiError::Status { body, .. }) => assert!(body.contains("model not found")),
        Err(other) => panic!("expected Status error, got {other:?}"),
        Ok(_) => panic!("expected error, got content"),
    }
}

#[tokio::test]
async fn chat_stream_skips_malformed_ndjson_line() {
    let server = MockServer::start().await;
    let body = "not valid json\n{\"message\":{\"content\":\"recovered\"},\"done\":true}\n";
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(body.as_bytes(), "application/x-ndjson"),
        )
        .mount(&server)
        .await;

    let backend = backend_at(&server.uri());
    let result = backend.complete(simple_messages(), CancelFlag::new()).await;
    assert_eq!(
        result.expect("valid line after malformed input"),
        "recovered"
    );
}

#[tokio::test]
async fn chat_stream_returns_content_when_stream_ends_without_done_flag() {
    let server = MockServer::start().await;
    let body = "{\"message\":{\"content\":\"incomplete but non-empty\"},\"done\":false}\n";
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(body.as_bytes(), "application/x-ndjson"),
        )
        .mount(&server)
        .await;

    let backend = backend_at(&server.uri());
    let result = backend.complete(simple_messages(), CancelFlag::new()).await;
    assert_eq!(
        result.expect("should return accumulated content"),
        "incomplete but non-empty"
    );
}

// ── cancellation ─────────────────────────────────────────────────────

#[tokio::test]
async fn cancel_flag_returns_cancelled_error() {
    let server = MockServer::start().await;
    let body = "{\"message\":{\"content\":\"a\"},\"done\":false}\n";
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(body.as_bytes(), "application/x-ndjson"),
        )
        .mount(&server)
        .await;

    let backend = backend_at(&server.uri());
    let cancel = CancelFlag::new();
    cancel.cancel();
    match backend.complete(simple_messages(), cancel).await {
        Err(AiError::Cancelled) => {}
        Err(other) => panic!("expected Cancelled, got {other:?}"),
        Ok(_) => panic!("expected Cancelled error, got content"),
    }
}
