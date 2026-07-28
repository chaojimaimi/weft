//! v1.7.3: Local Runbook — Markdown parser for fenced shell commands.
//!
//! Reads a Markdown file with [`pulldown_cmark`] (the CommonMark parser used
//! by rustdoc) and extracts fenced code blocks tagged as `sh`/`bash`/`zsh`
//! along with the preceding paragraph text as a description.
//!
//! ## Safety contract (V17 §5 退出标准)
//!
//! - **No execution**: activating a Runbook command only fills the editor
//!   buffer. The user presses Enter to run it.
//! - **No HTML**: pulldown-cmark's `default-features = false` disables raw
//!   HTML pass-through. Unknown languages are ignored, not rendered.
//! - **Bounded**: at most [`MAX_ENTRIES`] commands and [`MAX_COMMAND_CHARS`]
//!   chars per command. Overlong content is truncated, not panicked.
//! - **No file reads**: the parser only consumes the string it's given. The
//!   caller decides which file to read.

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

/// Maximum number of fenced-code entries extracted from one Runbook.
pub const MAX_ENTRIES: usize = 200;

/// Maximum length of a single command (in chars). Longer commands are
/// truncated to avoid pathological inputs.
pub const MAX_COMMAND_CHARS: usize = 4096;

/// A single Runbook entry — a shell command plus the description text that
/// preceded it in the Markdown source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunbookEntry {
    /// Human-readable description gathered from the paragraph(s) above the
    /// fenced block. May be empty if the block had no preceding text.
    pub description: String,
    /// The shell command text (already trimmed, truncated to
    /// [`MAX_COMMAND_CHARS`]).
    pub command: String,
    /// The fenced-block language tag as written (e.g. `sh`, `bash`).
    pub language: String,
}

/// Parse a Markdown string into a list of [`RunbookEntry`]s.
///
/// Only fenced code blocks with a recognized shell language tag are
/// extracted. All other Markdown content (paragraphs, headings, lists,
/// inline HTML) is consumed safely by the parser but not emitted as
/// entries — preceding text becomes the `description` for the next command.
pub fn parse_runbook(markdown: &str) -> Vec<RunbookEntry> {
    let opts = Options::empty();
    let parser = Parser::new_ext(markdown, opts);

    let mut entries = Vec::new();
    let mut pending_desc = String::new();
    let mut in_code_block = false;
    let mut code_lang = String::new();
    let mut code_buf = String::new();

    for event in parser {
        match event {
            Event::Start(Tag::CodeBlock(pulldown_cmark::CodeBlockKind::Fenced(lang))) => {
                // pulldown_cmark::CodeBlockKind::Fenced has the language
                // string; Indented blocks have no language and are ignored
                // (they're usually inline code samples, not runnable commands).
                let lang_str = lang.into_string();
                if is_shell_language(&lang_str) {
                    in_code_block = true;
                    code_lang = lang_str;
                    code_buf.clear();
                }
                // Note: do NOT clear pending_desc here — it's the description
                // for this code block. It's consumed and cleared on End.
            }
            Event::Start(Tag::CodeBlock(_)) => {
                // Indented code blocks (no language) — ignore. Description is
                // cleared on End to prevent carry-forward.
            }
            Event::End(TagEnd::CodeBlock) => {
                if in_code_block {
                    let command = code_buf.trim().to_string();
                    if !command.is_empty() && entries.len() < MAX_ENTRIES {
                        let truncated = truncate_chars(&command, MAX_COMMAND_CHARS);
                        entries.push(RunbookEntry {
                            description: pending_desc.trim().to_string(),
                            command: truncated,
                            language: std::mem::take(&mut code_lang),
                        });
                    }
                    in_code_block = false;
                    code_buf.clear();
                    code_lang.clear();
                }
                pending_desc.clear();
            }
            Event::Text(text) if in_code_block => {
                code_buf.push_str(&text);
            }
            Event::Text(text) => {
                // Accumulate paragraph text as description for the next
                // code block. Keep it bounded.
                if pending_desc.len() < 512 {
                    if !pending_desc.is_empty() {
                        pending_desc.push(' ');
                    }
                    pending_desc.push_str(text.trim());
                }
            }
            Event::Code(code) => {
                // Inline code — treat as description text.
                if pending_desc.len() < 512 {
                    if !pending_desc.is_empty() {
                        pending_desc.push(' ');
                    }
                    pending_desc.push_str(code.as_ref().trim());
                }
            }
            // All other events (headings, lists, HTML, etc.) are consumed
            // but don't contribute to descriptions or commands. A paragraph
            // break resets the pending description.
            Event::End(TagEnd::Paragraph) => {
                // Keep the description — it carries to the next code block.
            }
            _ => {}
        }
    }

    entries
}

/// Check if a fenced-block language tag denotes a shell language.
/// Recognized: `sh`, `bash`, `zsh`, `shell`, `shell-session`.
fn is_shell_language(lang: &str) -> bool {
    matches!(
        lang.trim().to_lowercase().as_str(),
        "sh" | "bash" | "zsh" | "shell" | "shell-session"
    )
}

/// Truncate a string to at most `max_chars` Unicode scalar values.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    s.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_runbook() {
        let md = "\
# Deploy Runbook

Build the project:

```sh
cargo build --release
```

Run tests:

```bash
cargo test --workspace
```
";
        let entries = parse_runbook(md);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].command, "cargo build --release");
        assert_eq!(entries[0].language, "sh");
        assert!(entries[0].description.contains("Build the project"));
        assert_eq!(entries[1].command, "cargo test --workspace");
        assert_eq!(entries[1].language, "bash");
        assert!(entries[1].description.contains("Run tests"));
    }

    #[test]
    fn ignores_non_shell_languages() {
        let md = "\
```python
print('hello')
```

```js
console.log('hi')
```

```sh
echo hello
```
";
        let entries = parse_runbook(md);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].command, "echo hello");
    }

    #[test]
    fn ignores_indented_code_blocks() {
        let md = "\
This is a paragraph.

    indented code block
    should be ignored

```sh
echo real
```
";
        let entries = parse_runbook(md);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].command, "echo real");
    }

    #[test]
    fn empty_code_block_skipped() {
        let md = "\
```sh
```
";
        let entries = parse_runbook(md);
        assert!(entries.is_empty());
    }

    #[test]
    fn whitespace_only_code_block_skipped() {
        let md = "\
```sh

  \t

```
";
        let entries = parse_runbook(md);
        assert!(entries.is_empty());
    }

    #[test]
    fn description_empty_when_no_preceding_text() {
        let md = "\
```sh
echo hello
```
";
        let entries = parse_runbook(md);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].description.is_empty());
    }

    #[test]
    fn description_resets_after_code_block() {
        let md = "\
First description.

```sh
echo one
```

Second description.

```sh
echo two
```
";
        let entries = parse_runbook(md);
        assert_eq!(entries.len(), 2);
        assert!(entries[0].description.contains("First"));
        assert!(!entries[0].description.contains("Second"));
        assert!(entries[1].description.contains("Second"));
    }

    #[test]
    fn max_entries_enforced() {
        let mut md = String::new();
        for i in 0..(MAX_ENTRIES + 50) {
            md.push_str(&format!("```sh\necho {i}\n```\n"));
        }
        let entries = parse_runbook(&md);
        assert_eq!(entries.len(), MAX_ENTRIES);
    }

    #[test]
    fn long_command_truncated() {
        let long_cmd = "x".repeat(MAX_COMMAND_CHARS + 1000);
        let md = format!("```sh\n{long_cmd}\n```");
        let entries = parse_runbook(&md);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].command.chars().count() <= MAX_COMMAND_CHARS);
    }

    #[test]
    fn html_not_executed_or_passed_through() {
        // pulldown-cmark with default-features=false does not emit raw HTML
        // events. HTML tags are treated as text.
        let md = "\
<script>alert('xss')</script>

```sh
echo safe
```
";
        let entries = parse_runbook(md);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].command, "echo safe");
        // The HTML tag text may appear in the description, but it's just
        // text — not executable.
    }

    #[test]
    fn multiline_command_preserved() {
        let md = "\
```sh
cargo build && \
cargo test && \
cargo clippy
```
";
        let entries = parse_runbook(md);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].command.contains("cargo build"));
        assert!(entries[0].command.contains("cargo test"));
        assert!(entries[0].command.contains("cargo clippy"));
    }

    #[test]
    fn shell_session_language_recognized() {
        let md = "\
```shell-session
$ echo hello
hello
```
";
        let entries = parse_runbook(md);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].language, "shell-session");
    }

    #[test]
    fn zsh_language_recognized() {
        let md = "\
```zsh
echo $ZSH_VERSION
```
";
        let entries = parse_runbook(md);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].language, "zsh");
    }

    #[test]
    fn empty_markdown_returns_empty() {
        assert!(parse_runbook("").is_empty());
        assert!(parse_runbook("Just text, no code blocks.").is_empty());
    }

    #[test]
    fn is_shell_language_case_insensitive() {
        assert!(is_shell_language("SH"));
        assert!(is_shell_language("Bash"));
        assert!(is_shell_language("ZSH"));
        assert!(!is_shell_language("python"));
        assert!(!is_shell_language(""));
    }
}
