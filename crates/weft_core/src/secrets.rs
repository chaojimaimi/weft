//! Secret masking — redacts known credential patterns from captured block
//! output, so secrets never land in the persisted history / search / (future)
//! AI context. Applied capture-side (at block finalize) so the live grid stays
//! raw but stored blocks are masked. Conservative patterns only; tuning lives
//! here.

use regex::Regex;
use std::sync::OnceLock;

/// Compiled secret patterns (cached after first use).
fn patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        vec![
            // OpenAI API keys: sk-... (20+ word chars)
            Regex::new(r"sk-[A-Za-z0-9_-]{20,}").unwrap(),
            // GitHub tokens: ghp_ / gho_ / ghs_ / ghu_ / ghr_ + 36+
            Regex::new(r"gh[pousr]_[A-Za-z0-9]{36,}").unwrap(),
            // AWS access key IDs: AKIA + 16 upper/digit
            Regex::new(r"AKIA[0-9A-Z]{16}").unwrap(),
            // Slack tokens: xox[baprs]-...
            Regex::new(r"xox[baprs]-[A-Za-z0-9-]{10,}").unwrap(),
            // PEM private-key blocks (incl. RSA/EC/OPENSSH/PRIVATE KEY)
            Regex::new(
                r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z0-9 ]*PRIVATE KEY-----",
            )
            .unwrap(),
        ]
    })
}

const MASK: &str = "••••••••";

/// Replace every secret pattern occurrence in `text` with a fixed mask.
pub fn mask(text: &str) -> String {
    let mut out = text.to_string();
    for re in patterns() {
        // v1.12.23 audit batch 1: a no-match round tripped as Cow::Borrowed and
        // into_owned() still cloned the whole text — keep the borrowed view.
        match re.replace_all(&out, MASK) {
            std::borrow::Cow::Borrowed(_) => {}
            std::borrow::Cow::Owned(s) => out = s,
        }
    }
    out
}

/// Compiled redaction patterns (v1.8 AI integration). Broader than `mask`:
/// catches inline credentials that would leak via AI prompts (Bearer tokens,
/// `password=`, `api_key=`, URL userinfo, env assignments). Cached after
/// first use.
fn redaction_patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        let mut v = patterns().to_vec();
        // Bearer / Token Authorization headers.
        v.push(Regex::new(r"(?i)Bearer\s+[A-Za-z0-9._\-]+").unwrap());
        v.push(Regex::new(r"(?i)token[=:]\s*[A-Za-z0-9._\-]{8,}").unwrap());
        // `api_key=...` / `api-key: ...` / `apikey=...` (8+ chars value).
        v.push(Regex::new(r"(?i)api[_-]?key[=:]\s*[A-Za-z0-9._\-]{8,}").unwrap());
        // `password=...` / `password: ...` (any non-whitespace value ≥ 4).
        v.push(Regex::new(r"(?i)password[=:]\s*\S{4,}").unwrap());
        // URL userinfo: `https://user:pass@host/...`
        v.push(Regex::new(r"(?i)(https?://[^/\s:]+:[^@/\s]+)@").unwrap());
        // `export VAR=value` where VAR looks sensitive (PW/PASSWD/SECRET/TOKEN/KEY).
        v.push(
            Regex::new(r"(?i)(export\s+(?:[A-Z0-9_]*(?:PW|PASSWD|PASSWORD|SECRET|TOKEN|KEY)[A-Z0-9_]*)=\S+)")
                .unwrap(),
        );
        v
    })
}

/// Broader redaction for AI-bound text. Applies [`mask`] first (known token
/// formats), then the broader inline-credential patterns. Used by
/// `ai::redact::redact_secrets` (v1.8) before sending any user context to
/// the local Ollama model.
pub fn redact_for_ai(text: &str) -> String {
    let mut out = mask(text);
    for re in redaction_patterns() {
        // For URL userinfo we keep the host visible but mask the credentials.
        if re.as_str().contains("https?://") {
            // v1.12.23 audit batch 1: same Cow early-skip as `mask` — a
            // no-match closure pass also cloned the whole text.
            match re.replace_all(&out, |caps: &regex::Captures| {
                let full = &caps[0];
                // Replace the `user:pass@` part with `••••••••@`.
                if let Some(at_pos) = full.rfind('@') {
                    format!("••••••••@{}", &full[at_pos + 1..])
                } else {
                    MASK.to_string()
                }
            }) {
                std::borrow::Cow::Borrowed(_) => {}
                std::borrow::Cow::Owned(s) => out = s,
            }
        } else {
            // v1.12.23 audit batch 1: Cow early-skip (see `mask`).
            match re.replace_all(&out, MASK) {
                std::borrow::Cow::Borrowed(_) => {}
                std::borrow::Cow::Owned(s) => out = s,
            }
        }
    }
    out
}

/// True if `s` contains any known secret pattern.
pub fn is_secret(s: &str) -> bool {
    patterns().iter().any(|re| re.is_match(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_openai_key() {
        let s = "export OPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwxyz1234567890abcd";
        let m = mask(s);
        assert!(m.contains("••••••••"));
        assert!(!m.contains("sk-abcdefghijklmnopqrstuvwxyz"));
    }

    #[test]
    fn masks_github_token() {
        let s = "ghp_0123456789abcdefghijklmnopqrstuvwxyzABCD"; // 40 chars after ghp_
        let m = mask(s);
        assert!(!m.contains("ghp_0123456789"));
        assert!(m.contains("••••••••"));
    }

    #[test]
    fn masks_aws_key() {
        let s = "aws_access_key_id = AKIAIOSFODNN7EXAMPLE";
        let m = mask(s);
        assert!(!m.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(is_secret(s));
    }

    #[test]
    fn masks_slack_token() {
        let s = "xoxb-1234567890123-abcdefghij";
        assert!(is_secret(s));
        assert!(mask(s).contains("••••••••"));
    }

    #[test]
    fn masks_pem_private_key_block() {
        let s =
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA...\n-----END RSA PRIVATE KEY-----";
        assert!(is_secret(s));
        let m = mask(s);
        assert!(!m.contains("MIIEowIBAAKCAQEA"));
        assert!(m.contains("••••••••"));
    }

    #[test]
    fn leaves_normal_output_untouched() {
        let s = "ls -la /tmp\necho hello\nCargo.toml  target  crates";
        assert!(!is_secret(s));
        assert_eq!(mask(s), s);
    }

    #[test]
    fn does_not_mask_short_lookalikes() {
        // too short to be a real key — left alone (avoids false positives)
        let s = "see sk-abc for details";
        assert!(!is_secret(s));
    }

    // ── v1.8 redact_for_ai tests ──────────────────────────────────────

    #[test]
    fn redact_bearer_token() {
        let s = "curl -H 'Authorization: Bearer abc123def456' https://api.example.com";
        let r = redact_for_ai(s);
        assert!(r.contains("••••••••"));
        assert!(!r.contains("abc123def456"));
        // host is preserved
        assert!(r.contains("api.example.com"));
    }

    #[test]
    fn redact_password_assignment() {
        let s = "PGPASSWORD=secret123 psql -h db";
        let r = redact_for_ai(s);
        assert!(!r.contains("secret123"));
    }

    #[test]
    fn redact_api_key_assignment() {
        let s = "api_key=sk-test-1234567890abcdef call";
        let r = redact_for_ai(s);
        assert!(!r.contains("sk-test-1234567890abcdef"));
    }

    #[test]
    fn redact_url_userinfo() {
        let s = "git clone https://alice:hunter2@github.com/org/repo.git";
        let r = redact_for_ai(s);
        assert!(!r.contains("alice:hunter2"));
        assert!(!r.contains("hunter2"));
        // host + path preserved
        assert!(r.contains("github.com/org/repo.git"));
    }

    #[test]
    fn redact_export_secret_env() {
        let s = "export AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let r = redact_for_ai(s);
        assert!(!r.contains("wJalrXUtnFEMI"));
        // export keyword may remain, value masked
    }

    #[test]
    fn redact_preserves_normal_commands() {
        let s = "ls -la /tmp\ngit status\necho hello world";
        let r = redact_for_ai(s);
        assert_eq!(r, s);
    }

    #[test]
    fn redact_preserves_cjk_text() {
        let s = "查找文件 列出当前目录的文件";
        let r = redact_for_ai(s);
        assert_eq!(r, s);
    }

    #[test]
    fn redact_empty_string() {
        assert_eq!(redact_for_ai(""), "");
    }

    #[test]
    fn redact_still_applies_known_token_patterns() {
        // mask() runs first, so sk- keys are still caught.
        let s = "OPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwxyz1234567890abcd";
        let r = redact_for_ai(s);
        assert!(!r.contains("sk-abcdefghijklmnopqrstuvwxyz"));
    }

    #[test]
    fn redact_does_not_touch_short_password_lookalikes() {
        // `password=abc` is 3 chars, below the 4-char threshold.
        let s = "password=abc";
        let r = redact_for_ai(s);
        // value preserved (too short to look like a real secret)
        assert!(r.contains("password=abc"));
    }
}
