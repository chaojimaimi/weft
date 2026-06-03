//! 本地 LLM 直连客户端
//!
//! 封装 HTTP 请求和 SSE 流处理，提供统一的非流式和流式调用接口。

use anyhow::{Context, Result};
use reqwest::Client;
use reqwest_eventsource::RequestBuilderExt;

use crate::presets::LlmEndpoint;
use crate::protocol::*;
use crate::streaming::SseStream;

/// 本地 LLM 直连客户端
///
/// 直接向 OpenAI 兼容端点发起 HTTP 请求，无需任何服务器中转。
#[derive(Debug, Clone)]
pub struct LocalLlmClient {
    http: Client,
}

impl LocalLlmClient {
    /// 创建新的客户端实例
    pub fn new() -> Self {
        Self {
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .expect("Failed to create HTTP client for local LLM"),
        }
    }

    /// 使用自定义 HTTP Client 创建
    pub fn with_client(http: Client) -> Self {
        Self { http }
    }

    /// 非流式聊天请求
    ///
    /// 发送完整消息列表，等待一次性返回完整响应。
    pub async fn chat(
        &self,
        endpoint: &LlmEndpoint,
        messages: Vec<ChatMessage>,
        temperature: Option<f32>,
        max_tokens: Option<u32>,
    ) -> Result<ChatResponse> {
        let url = format!(
            "{}/chat/completions",
            endpoint.base_url.trim_end_matches('/')
        );

        let request = ChatRequest {
            model: endpoint.model.clone(),
            messages,
            temperature,
            max_tokens,
            stream: Some(false),
            tools: None,
        };

        let response = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", endpoint.api_key))
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await
            .with_context(|| format!("Failed to call LLM endpoint: {}", url))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("LLM API error ({}): {}", status, body);
        }

        let chat_response: ChatResponse = response
            .json()
            .await
            .context("Failed to parse LLM response")?;

        Ok(chat_response)
    }

    /// 流式聊天请求（SSE）
    ///
    /// 以 SSE 流方式接收响应，适合实时显示 AI 输出。
    pub async fn chat_stream(
        &self,
        endpoint: &LlmEndpoint,
        messages: Vec<ChatMessage>,
        temperature: Option<f32>,
        max_tokens: Option<u32>,
    ) -> Result<SseStream> {
        let url = format!(
            "{}/chat/completions",
            endpoint.base_url.trim_end_matches('/')
        );

        let request = ChatRequest {
            model: endpoint.model.clone(),
            messages,
            temperature,
            max_tokens,
            stream: Some(true),
            tools: None,
        };

        let event_source = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", endpoint.api_key))
            .header("Content-Type", "application/json")
            .json(&request)
            .eventsource()
            .await
            .with_context(|| format!("Failed to establish SSE stream to: {}", url))?;

        Ok(SseStream::new(event_source))
    }
}

impl Default for LocalLlmClient {
    fn default() -> Self {
        Self::new()
    }
}
