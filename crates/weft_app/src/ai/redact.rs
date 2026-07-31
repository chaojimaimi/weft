//! v1.8 AI integration — secret redaction for AI-bound context.
//!
//! Thin wrapper around `weft_core::secrets::redact_for_ai`. Kept as a
//! dedicated module so the v1.8 plan's "secret 遮蔽归属模块" requirement is
//! satisfied and so prompt builders don't import the broader redaction
//! surface by accident.
//!
//! See `secrets.rs` in weft_core for the full pattern list and tests.

use weft_core::secrets;

/// Redact secrets from `input` before sending it to the local Ollama model.
/// Covers known token formats (sk-/ghp_/AKIA/xox/PEM) plus inline
/// credentials (Bearer / `password=` / `api_key=` / URL userinfo / export
/// SECRET=...).
pub fn redact_secrets(input: &str) -> String {
    secrets::redact_for_ai(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_bearer_in_curl_command() {
        let s = "curl -H 'Authorization: Bearer xyz789abc' https://api.example.com";
        let r = redact_secrets(s);
        assert!(!r.contains("xyz789abc"));
        assert!(r.contains("api.example.com"));
    }

    #[test]
    fn redact_password_in_pg_connection() {
        let s = "PGPASSWORD=hunter2 psql -h db -U alice";
        let r = redact_secrets(s);
        assert!(!r.contains("hunter2"));
    }

    #[test]
    fn redact_preserves_normal_command() {
        let s = "ls -la | grep test";
        assert_eq!(redact_secrets(s), s);
    }

    #[test]
    fn redact_preserves_cjk() {
        let s = "查找当前目录下的所有 TypeScript 文件";
        assert_eq!(redact_secrets(s), s);
    }

    #[test]
    fn redact_empty() {
        assert_eq!(redact_secrets(""), "");
    }
}
