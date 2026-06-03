//! 预设 LLM 端点配置
//!
//! 提供 GLM、DeepSeek 及 OpenAI 兼容自定义端点的快捷构造方法。

use serde::{Deserialize, Serialize};

/// LLM 端点配置
///
/// 包含提供商名称、API 地址、密钥和模型名称。
/// 适配任何 OpenAI 兼容的 chat completions API。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmEndpoint {
    /// 端点显示名称（如 "Zhipu GLM"）
    pub name: String,
    /// API 基础 URL（如 "https://open.bigmodel.cn/api/paas/v4"）
    pub base_url: String,
    /// API 密钥
    pub api_key: String,
    /// 模型名称/slug（如 "glm-4-flash"）
    pub model: String,
}

impl LlmEndpoint {
    /// 智谱 GLM 系列
    ///
    /// # Arguments
    /// * `api_key` - 智谱 AI API Key
    /// * `model` - 模型名称，如 "glm-4-flash", "glm-4-plus", "GLM-5-FP8"
    pub fn glm(api_key: &str, model: &str) -> Self {
        Self {
            name: "Zhipu GLM".into(),
            base_url: "https://open.bigmodel.cn/api/paas/v4".into(),
            api_key: api_key.into(),
            model: model.into(),
        }
    }

    /// DeepSeek 系列
    ///
    /// # Arguments
    /// * `api_key` - DeepSeek API Key
    /// * `model` - 模型名称，如 "deepseek-chat", "deepseek-reasoner"
    pub fn deepseek(api_key: &str, model: &str) -> Self {
        Self {
            name: "DeepSeek".into(),
            base_url: "https://api.deepseek.com/v1".into(),
            api_key: api_key.into(),
            model: model.into(),
        }
    }

    /// OpenAI（或任何 OpenAI 兼容的端点）
    ///
    /// # Arguments
    /// * `api_key` - OpenAI API Key
    /// * `model` - 模型名称
    pub fn openai(api_key: &str, model: &str) -> Self {
        Self {
            name: "OpenAI".into(),
            base_url: "https://api.openai.com/v1".into(),
            api_key: api_key.into(),
            model: model.into(),
        }
    }

    /// 自定义 OpenAI 兼容端点（如 Ollama、vLLM、LM Studio 等）
    ///
    /// # Arguments
    /// * `name` - 显示名称
    /// * `base_url` - API 基础 URL
    /// * `api_key` - API 密钥（可为空字符串用于本地服务）
    /// * `model` - 模型名称
    pub fn custom(name: &str, base_url: &str, api_key: &str, model: &str) -> Self {
        Self {
            name: name.into(),
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
        }
    }
}
