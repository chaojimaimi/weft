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
            Regex::new(r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z0-9 ]*PRIVATE KEY-----").unwrap(),
        ]
    })
}

const MASK: &str = "••••••••";

/// Replace every secret pattern occurrence in `text` with a fixed mask.
pub fn mask(text: &str) -> String {
    let mut out = text.to_string();
    for re in patterns() {
        out = re.replace_all(&out, MASK).into_owned();
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
        let s = "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA...\n-----END RSA PRIVATE KEY-----";
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
}
