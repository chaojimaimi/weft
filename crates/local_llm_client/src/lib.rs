//! Weft — OpenAI 兼容的本地 LLM 直连客户端
//!
//! 绕过 Warp 的服务器中转架构，直接调用 OpenAI 兼容的 chat completions API。
//! 支持 GLM、DeepSeek、Ollama 等任何兼容 `/v1/chat/completions` 的端点。

pub mod client;
pub mod protocol;
pub mod streaming;
pub mod presets;

pub use client::LocalLlmClient;
pub use presets::LlmEndpoint;
pub use protocol::{ChatChunk, ChatChunkChoice, ChatChunkDelta, ChatChoice, ChatMessage, ChatRequest, ChatResponse, ChatUsage, Role};
pub use streaming::SseStream;
