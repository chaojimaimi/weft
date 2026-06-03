//! SSE 流式响应解析器
//!
//! 将 reqwest-eventsource 的 EventSource 封装为类型安全的异步迭代器。

use anyhow::Result;
use futures::Stream;
use reqwest_eventsource::{Event, EventSource};
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::protocol::ChatChunk;

/// SSE 流式聊天响应流
///
/// # 使用示例
/// ```ignore
/// let stream = client.chat_stream(&endpoint, messages, None, None).await?;
/// let full_response = stream.collect_text().await?;
/// ```
pub struct SseStream {
    inner: EventSource,
}

impl SseStream {
    pub fn new(event_source: EventSource) -> Self {
        Self { inner: event_source }
    }

    /// 获取下一个 SSE 事件并解析为 ChatChunk
    ///
    /// 返回 `None` 表示流已结束（收到 `[DONE]` 标记）。
    pub async fn next_chunk(&mut self) -> Option<Result<ChatChunk>> {
        loop {
            match self.inner.next().await {
                Some(Ok(Event::Open)) => continue,
                Some(Ok(Event::Message(message))) => {
                    // [DONE] 标记表示流结束
                    if message.data.trim() == "[DONE]" {
                        return None;
                    }
                    // 空数据跳过（某些端点会发送心跳）
                    if message.data.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<ChatChunk>(&message.data) {
                        Ok(chunk) => return Some(Ok(chunk)),
                        Err(e) => {
                            return Some(Err(anyhow::anyhow!(
                                "Failed to parse SSE chunk: {}. Data: {}",
                                e,
                                &message.data[..message.data.len().min(200)]
                            )));
                        }
                    }
                }
                Some(Err(e)) => return Some(Err(anyhow::anyhow!("SSE stream error: {}", e))),
                None => return None,
            }
        }
    }

    /// 收集完整的文本响应（将所有 chunk 的 delta.content 拼接）
    pub async fn collect_text(mut self) -> Result<String> {
        let mut full_content = String::new();
        while let Some(result) = self.next_chunk().await {
            let chunk = result?;
            if let Some(choice) = chunk.choices.first() {
                if let Some(content) = &choice.delta.content {
                    full_content.push_str(content);
                }
            }
        }
        Ok(full_content)
    }

    /// 收集完整响应，包含文本和最终使用的模型名
    pub async fn collect_full(mut self) -> Result<StreamedResponse> {
        let mut text = String::new();
        let mut model = None;
        while let Some(result) = self.next_chunk().await {
            let chunk = result?;
            if model.is_none() {
                model = chunk.model.clone();
            }
            if let Some(choice) = chunk.choices.first() {
                if let Some(content) = &choice.delta.content {
                    text.push_str(content);
                }
            }
        }
        Ok(StreamedResponse { text, model })
    }
}

/// 流式响应的完整结果
#[derive(Debug, Clone)]
pub struct StreamedResponse {
    pub text: String,
    pub model: Option<String>,
}
