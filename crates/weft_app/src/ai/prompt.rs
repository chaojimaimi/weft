//! v1.3 AI integration — prompt construction (pure logic, fully testable).
//!
//! These functions build the user/system messages sent to the LLM. They are
//! deliberately side-effect-free so they can be unit-tested without a network
//! or a tokio runtime. The caller is responsible for:
//!
//! 1. Pulling the raw command/output/cwd out of the terminal state.
//! 2. Calling [`mask_secrets`] (or `secrets::mask` from weft_core) *before*
//!    passing strings here — defence in depth, but the AI client also masks
//!    on its way out.
//! 3. Spawning the request on a background tokio task.
//!
//! All prompts are intentionally short (Wellft is generating shell commands,
//! not writing essays) and use a deterministic structure so the model is
//! steered toward plain-shell output rather than chatty prose.

use weft_core::secrets;

use super::redact::redact_secrets;

/// Cap the amount of block output we send to the LLM. 4 KiB matches the
/// plan in `docs/V13_IMPLEMENTATION_PLAN.md` §4.4 — enough for typical
/// error messages + a stack-frame or two, without bloating the request.
pub const MAX_OUTPUT_BYTES: usize = 4 * 1024;

/// Cap the number of recent history commands included for context.
pub const MAX_HISTORY_ENTRIES: usize = 10;

/// A request to generate a shell command from natural language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandGenPrompt {
    /// The user's natural-language request, e.g. "find all .ts files".
    pub user_query: String,
    /// Current working directory of the shell (best-effort, may be empty).
    pub cwd: String,
    /// Recent command history (most-recent first), already trimmed to
    /// [`MAX_HISTORY_ENTRIES`].
    pub recent_history: Vec<String>,
}

/// A request to diagnose a failed command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosePrompt {
    /// The command that was run, e.g. `ls /nonexistent`.
    pub command: String,
    /// Captured stdout+stderr (already truncated to [`MAX_OUTPUT_BYTES`]).
    pub output: String,
    /// Exit code reported by the shell. `-1` when unknown / signal-killed.
    pub exit_code: i32,
    /// Current working directory of the shell (best-effort).
    pub cwd: String,
}

/// A single chat message in the OpenAI-style `role`/`content` schema.
/// All three supported providers (OpenAI, Anthropic, Ollama) can be fed from
/// this minimal shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatRole {
    System,
    User,
    #[allow(dead_code)]
    Assistant,
}

impl ChatRole {
    pub fn as_str(self) -> &'static str {
        match self {
            ChatRole::System => "system",
            ChatRole::User => "user",
            ChatRole::Assistant => "assistant",
        }
    }
}

/// Mask any secret patterns in `text`. Thin wrapper around `weft_core::secrets`
/// so prompt builders don't pull in the regex crate directly.
#[allow(dead_code)]
pub fn mask_secrets(text: &str) -> String {
    secrets::mask(text)
}

/// v1.8: Broader redaction for AI-bound text. Use this (not `mask_secrets`)
/// when building prompts that will be sent to the local Ollama model. Catches
/// Bearer tokens, `password=`, `api_key=`, URL userinfo, and export SECRET=
/// in addition to the known token formats.
pub fn redact_for_prompt(text: &str) -> String {
    redact_secrets(text)
}

/// Truncate `text` to at most `max_bytes` bytes, ending on a UTF-8 char
/// boundary. Appends a `"…[truncated]"` marker when truncation occurs.
pub fn truncate_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let marker = "…[truncated]";
    let budget = max_bytes.saturating_sub(marker.len());
    let mut end = budget;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + marker.len());
    out.push_str(&text[..end]);
    out.push_str(marker);
    out
}

/// Build the chat messages for a natural-language → shell-command request.
///
/// The system prompt steers the model to:
///   - Reply with **only** the shell command (no markdown fences, no prose)
///   - Prefer portable macOS/BSD tooling (matches the Weft platform)
///   - Refuse to produce destructive commands (rm -rf /, dd to a disk, …)
///
/// Secrets in the cwd / history are masked before being sent.
pub fn build_command_gen_messages(prompt: &CommandGenPrompt) -> Vec<ChatMessage> {
    let system = "你是 macOS 终端模拟器 Weft 的 shell 命令生成器。\
将用户的自然语言请求转换为单条 POSIX shell 命令。\
只回复命令本身——不要 markdown 代码围栏、不要解释、不要 $ 前缀、不要思考过程。\
优先使用 macOS/BSD 可移植工具（用 grep -E 而非 GNU grep -P，用 find 而非 fd）。\
如果请求危险或具破坏性，回复：# refused: <原因>。\
如果请求不明确，回复：# ambiguous: <一个简短的澄清问题>。";

    let masked_cwd = redact_for_prompt(&prompt.cwd);
    let masked_history: Vec<String> = prompt
        .recent_history
        .iter()
        .map(|h| redact_for_prompt(h))
        .collect();

    let mut user = String::new();
    if !masked_cwd.is_empty() {
        user.push_str("cwd: ");
        user.push_str(&masked_cwd);
        user.push('\n');
    }
    if !masked_history.is_empty() {
        user.push_str("recent commands:\n");
        for cmd in &masked_history {
            user.push_str("  ");
            user.push_str(cmd);
            user.push('\n');
        }
    }
    user.push_str("request: ");
    user.push_str(&prompt.user_query);

    vec![
        ChatMessage {
            role: ChatRole::System,
            content: system.to_string(),
        },
        ChatMessage {
            role: ChatRole::User,
            content: user,
        },
    ]
}

/// Build the chat messages for a failed-command diagnosis request.
///
/// The system prompt asks the model to produce a short structured explanation
/// covering (1) why the command likely failed, and (2) a concrete fix or
/// next step. Output is plain text (no JSON), kept under ~200 words so the
/// result fits comfortably inside a block-view diagnostic panel.
pub fn build_diagnose_messages(prompt: &DiagnosePrompt) -> Vec<ChatMessage> {
    let system = "你是终端模拟器 Weft 的 shell 错误诊断专家。\
用户执行了一条以非零状态码退出的命令。请用中文简明扼要地解释：\
(1) 最可能的原因，(2) 具体的修复方法或下一步诊断建议。\
直接给出诊断结论，不要展示思考过程或推理步骤。\
回答控制在 200 字以内。不要逐字重复命令本身。\
不要使用 markdown 标题。如果失败与密钥/凭证相关（如 auth token），\
请指出这一点但不要回显密钥本身。";

    let masked_cmd = redact_for_prompt(&prompt.command);
    let masked_out = redact_for_prompt(&truncate_bytes(&prompt.output, MAX_OUTPUT_BYTES));
    let masked_cwd = redact_for_prompt(&prompt.cwd);

    let mut user = String::new();
    user.push_str("command: ");
    user.push_str(&masked_cmd);
    user.push('\n');
    user.push_str("exit: ");
    user.push_str(&prompt.exit_code.to_string());
    user.push('\n');
    if !masked_cwd.is_empty() {
        user.push_str("cwd: ");
        user.push_str(&masked_cwd);
        user.push('\n');
    }
    user.push_str("output:\n");
    user.push_str(&masked_out);

    vec![
        ChatMessage {
            role: ChatRole::System,
            content: system.to_string(),
        },
        ChatMessage {
            role: ChatRole::User,
            content: user,
        },
    ]
}

/// v1.8.8: Clean up a diagnosis explanation from the model. Small models
/// (e.g. gemma4:e4b) sometimes produce output with excessive whitespace,
/// scattered punctuation, or incomplete fragments. This function:
///   - Strips markdown fences if present
///   - Collapses runs of whitespace (spaces/tabs) into a single space
///   - Removes lines that are empty or whitespace-only
///   - Trims leading/trailing whitespace from each line and the overall result
///
/// Does NOT alter the semantic content — just normalises presentation.
pub fn clean_diagnose_output(raw: &str) -> String {
    let trimmed = raw.trim();
    // Strip markdown fences if the model wrapped its answer.
    let inner = if let Some(after_open) = trimmed.strip_prefix("```") {
        let after_lang = match after_open.find('\n') {
            Some(idx) => &after_open[idx + 1..],
            None => after_open,
        };
        after_lang.trim_end()
    } else {
        trimmed
    };
    let inner = inner.strip_suffix("```").unwrap_or(inner);

    let mut out = String::with_capacity(inner.len());
    for line in inner.lines() {
        // Collapse runs of whitespace within each line to a single space.
        let collapsed: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&collapsed);
    }
    out
}

/// Strip a leading `$` / `>` prompt and surrounding markdown fences from an
/// AI-generated command. Models occasionally wrap their output in ```sh … ```
/// fences despite being told not to; we want to recover a runnable command.
#[allow(clippy::manual_strip)]
pub fn clean_command_output(raw: &str) -> String {
    let trimmed = raw.trim();
    // Strip a single pair of ```lang … ``` fences if present.
    let inner = if trimmed.starts_with("```") {
        let after_open = &trimmed[3..];
        // skip optional language tag on the same line
        let after_lang = match after_open.find('\n') {
            Some(idx) => &after_open[idx + 1..],
            None => after_open,
        };
        after_lang.trim_end()
    } else {
        trimmed
    };
    let inner = inner.strip_prefix("```").unwrap_or(inner);
    // Drop a trailing closing fence if it survived.
    let inner = inner.strip_suffix("```").unwrap_or(inner);
    // Drop a leading "$ " or "> " prompt the model may have prepended.
    let inner = inner
        .strip_prefix("$ ")
        .or_else(|| inner.strip_prefix("> "))
        .unwrap_or(inner);
    inner.trim().to_string()
}

/// v1.8.1: Risk level for an AI-generated command. Used by the palette to
/// display a warning badge and by `activate_palette_entry` to decide whether
/// to insert directly or require explicit confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiRiskLevel {
    /// No destructive patterns detected.
    Safe,
    /// Privilege escalation or system modification — display a caution badge.
    Caution,
    /// Catastrophic/irreversible patterns — display a danger badge and warn.
    Dangerous,
}

impl AiRiskLevel {
    pub fn label(self) -> &'static str {
        match self {
            Self::Safe => "AI",
            Self::Caution => "AI ⚠",
            Self::Dangerous => "AI ⚠⚠",
        }
    }
}

/// v1.8.1: Classify a generated command's risk level by pattern matching.
/// Pure function — no side effects, fully testable.
///
/// Patterns (case-insensitive, word-boundary aware where practical):
/// - **Dangerous**: `rm -rf /`, `dd of=/dev/`, `mkfs`, `> /dev/sd`, `chmod 777 /`
/// - **Caution**: `sudo`, `chmod`, `chown`, `kill -9`, `shutdown`, `reboot`
/// - **Safe**: everything else
pub fn classify_command_risk(command: &str) -> AiRiskLevel {
    let lower = command.to_lowercase();
    let trimmed = lower.trim();

    // Dangerous patterns — irreversible system damage.
    let dangerous_patterns: &[&str] = &[
        "rm -rf /",
        "rm -rf /*",
        "rm -rf ~",
        "rm -rf $home",
        "of=/dev/", // dd/mkfs writing to a block device
        "mkfs",
        "> /dev/sd",
        "chmod 777 /",
        "chmod -r 777 /",
        ":(){:|:&};:",
        "fork bomb",
    ];
    for pat in dangerous_patterns {
        if trimmed.contains(pat) {
            return AiRiskLevel::Dangerous;
        }
    }
    // `rm -rf` with any path starting at root or home is dangerous.
    if trimmed.contains("rm -rf") {
        // Check if it targets root, home, or wildcard
        if trimmed.contains("rm -rf /")
            || trimmed.contains("rm -rf ~")
            || trimmed.contains("rm -rf *")
            || trimmed.contains("rm -rf .")
        {
            return AiRiskLevel::Dangerous;
        }
    }

    // Caution patterns — privilege escalation or system modification.
    let caution_patterns: &[&str] = &[
        "sudo",
        "chmod ",
        "chown ",
        "kill -9",
        "killall",
        "shutdown",
        "reboot",
        "halt",
        "launchctl",
        "nvram",
    ];
    for pat in caution_patterns {
        if trimmed.contains(pat) {
            return AiRiskLevel::Caution;
        }
    }

    AiRiskLevel::Safe
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_under_budget_returns_input_unchanged() {
        let s = "hello";
        assert_eq!(truncate_bytes(s, 100), s);
    }

    #[test]
    fn truncate_over_budget_appends_marker_on_char_boundary() {
        // 20 ASCII chars, budget 10 → cut to fit "…[truncated]" (12 chars)
        // so the budget for content is 10 - 12 = -2 → saturating_sub → 0.
        // The function then walks back to 0, so only the marker is emitted.
        let s = "0123456789abcdefghij";
        let out = truncate_bytes(s, 10);
        assert!(out.ends_with("…[truncated]"));
    }

    #[test]
    fn truncate_preserves_utf8_boundary() {
        // 2-byte chars × 20 = 40 bytes. Budget 10 must not split a char.
        let s = "αβγδεζηθικλμνξοπρστυφ"; // 20 Greek letters, 40 bytes UTF-8
        assert!(s.len() > 10);
        let out = truncate_bytes(s, 5);
        // 5 - 12 = 0 (saturating), so only the marker is emitted.
        assert_eq!(out, "…[truncated]");
        // A larger budget keeps some chars intact.
        let out = truncate_bytes(s, 20);
        assert!(out.ends_with("…[truncated]"));
        // Ensure no broken UTF-8.
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
        // The byte length is bounded.
        assert!(out.len() <= 20);
    }

    #[test]
    fn mask_secrets_strips_openai_keys_from_history() {
        let p = CommandGenPrompt {
            user_query: "find my key".into(),
            cwd: "/home/me".into(),
            recent_history: vec![
                "export OPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwxyz1234567890abcd".into(),
            ],
        };
        let msgs = build_command_gen_messages(&p);
        let user_msg = &msgs[1];
        assert!(user_msg.content.contains("••••••••"));
        assert!(!user_msg.content.contains("sk-abcdefghijklmnopqrstuvwxyz"));
    }

    #[test]
    fn command_gen_messages_have_system_first() {
        let p = CommandGenPrompt {
            user_query: "list files".into(),
            cwd: "/tmp".into(),
            recent_history: vec![],
        };
        let msgs = build_command_gen_messages(&p);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, ChatRole::System);
        assert_eq!(msgs[1].role, ChatRole::User);
        assert!(msgs[1].content.contains("cwd: /tmp"));
        assert!(msgs[1].content.contains("request: list files"));
    }

    #[test]
    fn diagnose_messages_include_command_exit_output() {
        let p = DiagnosePrompt {
            command: "ls /nonexistent".into(),
            output: "ls: /nonexistent: No such file or directory".into(),
            exit_code: 1,
            cwd: "/home/me".into(),
        };
        let msgs = build_diagnose_messages(&p);
        assert_eq!(msgs[0].role, ChatRole::System);
        assert!(msgs[1].content.contains("command: ls /nonexistent"));
        assert!(msgs[1].content.contains("exit: 1"));
        assert!(msgs[1].content.contains("cwd: /home/me"));
        assert!(msgs[1].content.contains("No such file or directory"));
    }

    #[test]
    fn diagnose_truncates_long_output() {
        let long = "x".repeat(MAX_OUTPUT_BYTES * 2);
        let p = DiagnosePrompt {
            command: "cat /big".into(),
            output: long,
            exit_code: 0,
            cwd: "".into(),
        };
        let msgs = build_diagnose_messages(&p);
        let user = &msgs[1].content;
        assert!(user.contains("…[truncated]"));
        // User message size is bounded by the truncated output + overhead.
        assert!(user.len() < MAX_OUTPUT_BYTES + 1024);
    }

    #[test]
    fn clean_command_output_strips_fences_and_prompt() {
        assert_eq!(clean_command_output("ls -la"), "ls -la");
        assert_eq!(clean_command_output("```sh\nls -la\n```"), "ls -la");
        assert_eq!(clean_command_output("```\nls -la\n```"), "ls -la");
        assert_eq!(clean_command_output("$ ls -la"), "ls -la");
        assert_eq!(clean_command_output("> ls -la"), "ls -la");
        assert_eq!(clean_command_output("  ls -la  "), "ls -la");
    }

    #[test]
    fn clean_diagnose_output_collapses_whitespace() {
        // v1.8.8: gemma4:e4b produces output like this — scattered
        // fragments with excessive spaces. The function should collapse
        // runs of whitespace into single spaces.
        let raw = "AI:             :grep           。        Linux         ，        ";
        let cleaned = clean_diagnose_output(raw);
        assert_eq!(cleaned, "AI: :grep 。 Linux ，");
    }

    #[test]
    fn clean_diagnose_output_strips_fences() {
        let raw = "```\n这是诊断结果。\n```";
        let cleaned = clean_diagnose_output(raw);
        assert_eq!(cleaned, "这是诊断结果。");
    }

    #[test]
    fn clean_diagnose_output_removes_empty_lines() {
        let raw = "第一行\n\n\n第二行\n   \n第三行";
        let cleaned = clean_diagnose_output(raw);
        assert_eq!(cleaned, "第一行\n第二行\n第三行");
    }

    #[test]
    fn clean_diagnose_output_preserves_normal_text() {
        let raw = "命令失败的原因是路径不存在。\n建议使用 ls 检查目录。";
        let cleaned = clean_diagnose_output(raw);
        assert_eq!(cleaned, raw);
    }

    #[test]
    fn clean_diagnose_output_trims_outer_whitespace() {
        let raw = "  \n  诊断内容  \n  ";
        let cleaned = clean_diagnose_output(raw);
        assert_eq!(cleaned, "诊断内容");
    }

    #[test]
    fn clean_command_output_preserves_complex_commands() {
        let cmd = "find . -name '*.ts' -mtime -1 | xargs grep 'TODO'";
        assert_eq!(clean_command_output(cmd), cmd);
        assert_eq!(clean_command_output(&format!("```sh\n{cmd}\n```")), cmd);
    }

    #[test]
    fn empty_cwd_is_omitted_from_command_gen() {
        let p = CommandGenPrompt {
            user_query: "list files".into(),
            cwd: "".into(),
            recent_history: vec![],
        };
        let msgs = build_command_gen_messages(&p);
        assert!(!msgs[1].content.contains("cwd:"));
    }

    #[test]
    fn empty_cwd_is_omitted_from_diagnose() {
        let p = DiagnosePrompt {
            command: "ls".into(),
            output: "out".into(),
            exit_code: 0,
            cwd: "".into(),
        };
        let msgs = build_diagnose_messages(&p);
        assert!(!msgs[1].content.contains("cwd:"));
    }

    // ── v1.8.1 classify_command_risk tests ──────────────────────────

    #[test]
    fn risk_safe_for_normal_commands() {
        assert_eq!(classify_command_risk("ls -la"), AiRiskLevel::Safe);
        assert_eq!(classify_command_risk("git status"), AiRiskLevel::Safe);
        assert_eq!(classify_command_risk("echo hello"), AiRiskLevel::Safe);
        assert_eq!(
            classify_command_risk("find . -name '*.ts'"),
            AiRiskLevel::Safe
        );
    }

    #[test]
    fn risk_dangerous_for_rm_rf_root() {
        assert_eq!(classify_command_risk("rm -rf /"), AiRiskLevel::Dangerous);
        assert_eq!(classify_command_risk("rm -rf /*"), AiRiskLevel::Dangerous);
        assert_eq!(classify_command_risk("rm -rf ~"), AiRiskLevel::Dangerous);
        assert_eq!(classify_command_risk("rm -rf *"), AiRiskLevel::Dangerous);
    }

    #[test]
    fn risk_dangerous_for_dd_to_device() {
        assert_eq!(
            classify_command_risk("dd if=image.iso of=/dev/disk4"),
            AiRiskLevel::Dangerous
        );
    }

    #[test]
    fn risk_dangerous_for_mkfs() {
        assert_eq!(
            classify_command_risk("mkfs.ext4 /dev/sda1"),
            AiRiskLevel::Dangerous
        );
    }

    #[test]
    fn risk_caution_for_sudo() {
        assert_eq!(
            classify_command_risk("sudo apt update"),
            AiRiskLevel::Caution
        );
        assert_eq!(
            classify_command_risk("sudo brew install ffmpeg"),
            AiRiskLevel::Caution
        );
    }

    #[test]
    fn risk_caution_for_chmod() {
        assert_eq!(
            classify_command_risk("chmod +x script.sh"),
            AiRiskLevel::Caution
        );
    }

    #[test]
    fn risk_caution_for_kill() {
        assert_eq!(classify_command_risk("kill -9 12345"), AiRiskLevel::Caution);
        assert_eq!(classify_command_risk("killall node"), AiRiskLevel::Caution);
    }

    #[test]
    fn risk_dangerous_for_chmod_777_root() {
        assert_eq!(classify_command_risk("chmod 777 /"), AiRiskLevel::Dangerous);
    }

    #[test]
    fn risk_case_insensitive() {
        assert_eq!(classify_command_risk("SUDO ls"), AiRiskLevel::Caution);
        assert_eq!(classify_command_risk("RM -RF /"), AiRiskLevel::Dangerous);
    }
}
