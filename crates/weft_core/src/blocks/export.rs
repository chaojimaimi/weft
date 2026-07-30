//! v1.7.3: Block export — desensitized Markdown generation.
//!
//! Generates a Markdown document from a [`Block`] (with optional
//! [`BlockAnnotation`]), applying secret redaction beyond the capture-side
//! [`secrets::mask`]. The export-time redaction additionally covers
//! environment-variable assignments (`TOKEN=value`, `PASSWORD=value`),
//! `Authorization: Bearer` headers, and URL-embedded credentials — patterns
//! that are too broad for capture-side masking (which only matches known
//! token formats like `sk-…`, `ghp_…`) but are essential before sharing
//! output externally.
//!
//! ## Safety contract (V17 §5 退出标准)
//!
//! - **No execution**: exporting produces a Markdown string; it does not
//!   run anything.
//! - **Default redact**: secrets are masked by default. The caller may
//!   offer a "show raw" preview, but the default is always redacted.
//! - **Bounded output**: output is capped at [`MAX_EXPORT_OUTPUT_CHARS`]
//!   chars to prevent pathological files.

use std::sync::OnceLock;

use regex::Regex;

use crate::blocks::annotations::BlockAnnotation;
use crate::blocks::Block;
use crate::secrets;

/// Maximum output length included in the exported Markdown (chars).
/// Output beyond this is truncated with a `[output truncated]` marker.
pub const MAX_EXPORT_OUTPUT_CHARS: usize = 64 * 1024;

/// Mask value replacing secrets in exported output.
const MASK: &str = "••••••••";

/// Export-time redaction: applies [`secrets::mask`] first (known token
/// formats), then additional env-style / header / URL-credential patterns
/// that are too broad for capture-side masking.
///
/// This is a pure function — no I/O, no side effects.
pub fn redact_for_export(text: &str) -> String {
    // First pass: known token formats (sk-, ghp-, AKIA, xox, PEM blocks).
    let after_known = secrets::mask(text);

    // Second pass: env-style assignments (NAME=value where NAME contains a
    // sensitive keyword).
    let after_env = env_patterns().replace_all(&after_known, |caps: &regex::Captures| {
        let name = caps.name("name").map(|m| m.as_str()).unwrap_or("");
        format!("{name}={MASK}")
    });

    // Third pass: Authorization: Bearer <token> headers.
    let after_auth = bearer_patterns().replace_all(&after_env, |_: &regex::Captures| {
        format!("Authorization: Bearer {MASK}")
    });

    // Fourth pass: URL-embedded credentials.
    url_patterns()
        .replace_all(&after_auth, |caps: &regex::Captures| {
            let scheme = caps.name("scheme").map(|m| m.as_str()).unwrap_or("");
            let user = caps.name("user").map(|m| m.as_str()).unwrap_or("");
            format!("{scheme}://{user}:{MASK}@")
        })
        .into_owned()
}

/// Env-style redaction pattern. Matches `export NAME=value` or `NAME=value`
/// where NAME contains a sensitive keyword (TOKEN/PASSWORD/SECRET/etc.).
fn env_patterns() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?i)(?P<name>(?:export\s+)?\S*(?:TOKEN|PASSWORD|PASSWD|SECRET|API_KEY|ACCESS_KEY|PRIVATE_KEY|CREDENTIAL)\S*)\s*=\s*(?:\$'(?:\\.|[^'\\])*'|"(?:\\.|[^"\\])*"|'[^']*'|\S+)"#,
        )
        .expect("export env regex compiles")
    })
}

/// `Authorization: Bearer <token>` header pattern.
fn bearer_patterns() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)Authorization\s*:\s*Bearer\s+\S+").expect("export bearer regex compiles")
    })
}

/// URL credential pattern: `scheme://user:password@host`.
fn url_patterns() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?P<scheme>[a-z][a-z0-9+.-]*)://(?P<user>[^\s:/@]+):[^\s/@]+@")
            .expect("export url regex compiles")
    })
}

/// Generate a desensitized Markdown document from a [`Block`] and optional
/// [`BlockAnnotation`]. Output is redacted via [`redact_for_export`] and
/// capped at [`MAX_EXPORT_OUTPUT_CHARS`] chars.
pub fn export_block_as_markdown(block: &Block, annotation: Option<&BlockAnnotation>) -> String {
    let mut md = String::with_capacity(512 + block.output.len().min(MAX_EXPORT_OUTPUT_CHARS));

    // Header + command.
    md.push_str("## Command\n\n");
    md.push_str("```sh\n");
    let redacted_command = redact_for_export(&block.command);
    md.push_str(&redacted_command);
    if !redacted_command.ends_with('\n') {
        md.push('\n');
    }
    md.push_str("```\n\n");

    // Metadata.
    if let Some(cwd) = &block.cwd {
        md.push_str(&format!("**CWD:** `{cwd}`\n\n"));
    }
    if let Some(code) = block.exit_code {
        md.push_str(&format!("**Exit code:** {code}\n\n"));
    }
    if let Some(ann) = annotation {
        if !ann.tags.is_empty() {
            md.push_str(&format!("**Tags:** {}\n\n", ann.tags.join(", ")));
        }
        if let Some(note) = &ann.note {
            md.push_str("> ");
            md.push_str(note);
            md.push_str("\n\n");
        }
    }

    // Output (redacted + capped).
    md.push_str("### Output\n\n");
    md.push_str("```\n");
    let redacted = redact_for_export(&block.output);
    if redacted.chars().count() > MAX_EXPORT_OUTPUT_CHARS {
        let truncated: String = redacted.chars().take(MAX_EXPORT_OUTPUT_CHARS).collect();
        md.push_str(&truncated);
        md.push_str("\n[output truncated]\n");
    } else {
        md.push_str(&redacted);
        if !redacted.ends_with('\n') {
            md.push('\n');
        }
    }
    md.push_str("```\n");

    md
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::annotations::BlockAnnotation;
    use crate::blocks::{Block, BlockId};
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    fn mk_block(command: &str, output: &str) -> Block {
        Block {
            id: BlockId(1),
            command: command.to_string(),
            cwd: Some("/repo".to_string()),
            output: Arc::from(output),
            styled_output: None,
            exit_code: Some(0),
            started_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            finished_at: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_010)),
            collapsed: false,
        }
    }

    fn mk_annotation(bookmarked: bool, note: Option<&str>, tags: &[&str]) -> BlockAnnotation {
        BlockAnnotation {
            block_id: BlockId(1),
            bookmarked,
            note: note.map(|s| s.to_string()),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            updated_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_005),
        }
    }

    // --- redact_for_export ---

    #[test]
    fn redact_env_token() {
        let s = "export API_TOKEN=abc123secret\n";
        let r = redact_for_export(s);
        assert!(r.contains("API_TOKEN=••••••••"));
        assert!(!r.contains("abc123secret"));
    }

    #[test]
    fn redact_password_assignment() {
        let s = "DB_PASSWORD=hunter2\n";
        let r = redact_for_export(s);
        assert!(r.contains("DB_PASSWORD=••••••••"));
        assert!(!r.contains("hunter2"));
    }

    #[test]
    fn redact_quoted_secret_with_spaces() {
        let s = "export API_TOKEN=\"secret value with spaces\"\nnext=line\n";
        let r = redact_for_export(s);
        assert_eq!(r, "export API_TOKEN=••••••••\nnext=line\n");
        assert!(!r.contains("secret value with spaces"));
    }

    #[test]
    fn redact_escaped_and_ansi_c_quoted_secrets() {
        for text in [
            "API_TOKEN=$'secret value' next\n",
            "API_TOKEN=\"a\\\" b\" next\n",
        ] {
            let redacted = redact_for_export(text);
            assert_eq!(redacted, "API_TOKEN=•••••••• next\n");
        }
    }

    #[test]
    fn redact_authorization_bearer() {
        let s = "Authorization: Bearer my-secret-token\n";
        let r = redact_for_export(s);
        assert!(r.contains("••••••••"));
        assert!(!r.contains("my-secret-token"));
    }

    #[test]
    fn redact_url_credentials() {
        let s = "postgres://admin:secretpass@db.local:5432/mydb\n";
        let r = redact_for_export(s);
        assert!(r.contains("postgres://admin:••••••••@"));
        assert!(!r.contains("secretpass"));
    }

    #[test]
    fn redact_https_url_credentials() {
        let s = "https://user:pass@example.com/path\n";
        let r = redact_for_export(s);
        assert!(r.contains("https://user:••••••••@"));
    }

    #[test]
    fn redact_preserves_normal_output() {
        let s = "cargo build --release\nCompiling weft v1.7.3\nFinished\n";
        let r = redact_for_export(s);
        assert_eq!(r, s);
    }

    #[test]
    fn redact_known_token_format() {
        // Known formats (sk-…) still redacted via secrets::mask.
        let s = "OPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwxyz1234567890abcd";
        let r = redact_for_export(s);
        assert!(!r.contains("sk-abcdefghijklmnopqrstuvwxyz"));
    }

    #[test]
    fn redact_secret_keyword_in_middle_of_name() {
        let s = "MY_API_KEY_STORAGE=abcdef";
        let r = redact_for_export(s);
        assert!(r.contains("••••••••"));
        assert!(!r.contains("abcdef"));
    }

    #[test]
    fn redact_credential_keyword() {
        let s = "AWS_CREDENTIAL_FILE=/path/to/creds";
        let r = redact_for_export(s);
        // NAME contains CREDENTIAL → value masked.
        assert!(r.contains("••••••••"));
    }

    // --- export_block_as_markdown ---

    #[test]
    fn export_basic_block() {
        let block = mk_block("cargo test", "running 3 tests\nok\n");
        let md = export_block_as_markdown(&block, None);
        assert!(md.contains("## Command"));
        assert!(md.contains("```sh\ncargo test\n```"));
        assert!(md.contains("**CWD:** `/repo`"));
        assert!(md.contains("**Exit code:** 0"));
        assert!(md.contains("### Output"));
        assert!(md.contains("running 3 tests"));
    }

    #[test]
    fn export_with_annotation() {
        let block = mk_block("kubectl apply -f deploy.yaml", "deployed\n");
        let ann = mk_annotation(true, Some("production deploy"), &["deploy", "prod"]);
        let md = export_block_as_markdown(&block, Some(&ann));
        assert!(md.contains("**Tags:** deploy, prod"));
        assert!(md.contains("> production deploy"));
    }

    #[test]
    fn export_redacts_secrets_in_output() {
        let block = mk_block("env", "API_TOKEN=secret123\nPATH=/usr/bin\n");
        let md = export_block_as_markdown(&block, None);
        assert!(md.contains("API_TOKEN=••••••••"));
        assert!(!md.contains("secret123"));
        // Non-secret env vars preserved.
        assert!(md.contains("PATH=/usr/bin"));
    }

    #[test]
    fn export_redacts_secrets_in_command() {
        let block = mk_block("API_TOKEN=secret123 deploy", "ok\n");
        let md = export_block_as_markdown(&block, None);
        assert!(md.contains("API_TOKEN=•••••••• deploy"));
        assert!(!md.contains("secret123"));
    }

    #[test]
    fn export_truncates_long_output() {
        let long_output = "x".repeat(MAX_EXPORT_OUTPUT_CHARS + 1000);
        let block = mk_block("cat bigfile", &long_output);
        let md = export_block_as_markdown(&block, None);
        assert!(md.contains("[output truncated]"));
    }

    #[test]
    fn export_no_cwd_no_exit_code() {
        let mut block = mk_block("echo hi", "hi\n");
        block.cwd = None;
        block.exit_code = None;
        let md = export_block_as_markdown(&block, None);
        assert!(!md.contains("**CWD:**"));
        assert!(!md.contains("**Exit code:**"));
    }
}
