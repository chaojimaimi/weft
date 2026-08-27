//! v1.6.3: Type-safe NSAlert wrapper for the crash recovery prompt.
//!
//! Shows a modal dialog when Weft detects an unclean shutdown and a
//! recovery snapshot is available. The user can choose to:
//!
//! - **Restore** — rebuild the session from the snapshot.
//! - **Ignore** — start fresh without restoring; the ignored snapshot is
//!   superseded by the current session's auto-snapshot (the pre-ignore copy
//!   survives only as one `.bak` generation — it is NOT kept for later
//!   recovery or manual loading; that promise cannot hold, see
//!   docs/FIX_RECOVERY_DESIGN_ALIGNMENT.md Fix 3).
//! - **Delete** — start fresh; the snapshot is permanently deleted.
//!
//! The prompt is modal (`runModal`) and must be invoked on the main thread
//! (enforced via `MainThreadMarker`). Cancel (Esc / Cmd+.) is treated as
//! "Ignore" — the user explicitly chose not to restore right now.

use objc2_app_kit::{NSAlert, NSAlertStyle, NSModalResponse};
use objc2_foundation::{MainThreadMarker, NSString};

/// The user's response to the recovery prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryPromptResponse {
    /// Restore the session from the snapshot.
    Restore,
    /// Start fresh; the ignored snapshot is superseded by the current
    /// session's auto-snapshot (not kept for later recovery).
    Ignore,
    /// Start fresh; permanently delete the snapshot.
    Delete,
}

/// v1.10.23: The finalized recovery decision delivered back to the app via
/// `AppEvent::RecoveryChosen`. Kept separate from
/// [`RecoveryPromptResponse`] so `AppEvent` (main.rs) stays decoupled from
/// the alert module's raw response type, and so the "prompt failed / not on
/// main thread" fallback can be expressed as a plain choice instead of a
/// `Result`. Pure type — unit-testable without any AppKit state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryChoice {
    /// Restore the session from the snapshot.
    Restore,
    /// Start fresh; the ignored snapshot is superseded by the current
    /// session's auto-snapshot (not kept for later recovery).
    Ignore,
    /// Start fresh; permanently delete the snapshot.
    Delete,
}

/// v1.10.23: Map a prompt response to the app-facing recovery choice.
///
/// Total (no error arm): the error/abort fallback is decided by the caller
/// (which maps it to [`RecoveryChoice::Ignore`], matching the pre-v1.10.23
/// behavior where a failed prompt fell through to the normal tab restore
/// with the snapshot preserved).
pub fn recovery_choice_from_response(response: RecoveryPromptResponse) -> RecoveryChoice {
    match response {
        RecoveryPromptResponse::Restore => RecoveryChoice::Restore,
        RecoveryPromptResponse::Ignore => RecoveryChoice::Ignore,
        RecoveryPromptResponse::Delete => RecoveryChoice::Delete,
    }
}

/// Errors raised by the alert wrapper.
#[derive(Debug)]
pub enum AlertError {
    /// Not on the main thread. NSAlert is a UI API and must be invoked
    /// from the main thread.
    #[allow(dead_code)]
    NotMainThread,
}

impl std::fmt::Display for AlertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotMainThread => write!(f, "alert must be invoked on the main thread"),
        }
    }
}

impl std::error::Error for AlertError {}

/// Show the crash recovery prompt.
///
/// - `snapshot_age_secs` — age of the recovery snapshot in seconds (shown
///   in the informative text to help the user decide).
/// - `tab_count` — number of tabs in the snapshot (shown for context).
///
/// The alert has three buttons:
///
/// 1. **Restore** (default) — returns [`RecoveryPromptResponse::Restore`].
/// 2. **Ignore** — returns [`RecoveryPromptResponse::Ignore`].
/// 3. **Delete** — returns [`RecoveryPromptResponse::Delete`].
///
/// If the user dismisses the alert via Esc / Cmd+. (cancel), it's treated
/// as **Ignore** — a normal session starts; the old snapshot is superseded
/// by the current session's auto-snapshot.
pub fn show_recovery_prompt(
    mtm: MainThreadMarker,
    snapshot_age_secs: u64,
    tab_count: usize,
) -> Result<RecoveryPromptResponse, AlertError> {
    // SAFETY: NSAlert::new requires MainThreadMarker; we have it.
    let alert = unsafe { NSAlert::new(mtm) };
    unsafe {
        // Title: "Weft Closed Unexpectedly"
        let title = NSString::from_str("Weft Closed Unexpectedly");
        alert.setMessageText(&title);

        // Informative text with snapshot details.
        let age_text = format_snapshot_age(snapshot_age_secs);
        let info = NSString::from_str(&format!(
            "A previous session ({}, {} tabs) is available. Would you like to restore it?\n\n\
             Restoring will reopen your tabs and panes in their saved directories. \
             Editor drafts are restored but never auto-executed.",
            age_text, tab_count
        ));
        alert.setInformativeText(&info);

        // Warning style (not critical — no data loss).
        alert.setAlertStyle(NSAlertStyle::Warning);

        // Button order (right-to-left on macOS, but we add left-to-right
        // and let AppKit handle layout):
        // 1. Restore (default) — first button is the default
        let restore = NSString::from_str("Restore");
        alert.addButtonWithTitle(&restore);

        // 2. Ignore — start fresh; the current session's first autosave
        //    supersedes the old snapshot (pre-ignore copy survives only as
        //    one .bak generation — not kept for later recovery)
        let ignore = NSString::from_str("Ignore");
        alert.addButtonWithTitle(&ignore);

        // 3. Delete — permanently delete snapshot
        let delete = NSString::from_str("Delete Snapshot");
        alert.addButtonWithTitle(&delete);
    }

    // Run modally.
    let response = unsafe { alert.runModal() };

    // Map NSModalResponse to our enum.
    // NSAlert uses NSModalResponse for button indices:
    //   NSAlertFirstButtonReturn = 1000
    //   NSAlertSecondButtonReturn = 1001
    //   NSAlertThirdButtonReturn = 1002
    //   NSAlertCancelReturn = (varies, but typically the Esc/Cancel action)
    //
    // For a 3-button alert:
    //   First button (Restore)  → NSAlertFirstButtonReturn (1000)
    //   Second button (Ignore)  → NSAlertSecondButtonReturn (1001)
    //   Third button (Delete)   → NSAlertThirdButtonReturn (1002)
    //   Esc / Cmd+.             → NSAlertSecondButtonReturn (1001) — the
    //                            cancel button is the second one (Ignore)
    //                            per AppKit's cancel-button heuristic.
    //
    // This means Esc = Ignore, which is the desired behavior (a fresh
    // session starts; the old snapshot is superseded by autosave).
    Ok(map_modal_response(response))
}

/// v1.11.1 (PLAN_v1111 §4.3): the user's response to the large-paste
/// confirmation. Mirrors [`RecoveryChoice`]'s role: the finalized decision
/// delivered back through `AppEvent::PasteDecided`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PastePromptResponse {
    /// Paste this one time only.
    Once,
    /// Paste and suppress further prompts for this session (both risk
    /// classes; resets when the app quits — PLAN_v1111 §7).
    AlwaysSession,
    /// Drop the paste without writing anywhere (also Esc / any failure).
    Cancel,
}

/// v1.11.1: Pure modal-code → response mapping so the button/Esc contract is
/// headless-testable (same pattern as `map_modal_response` +
/// `recovery_choice_from_response`).
///
/// Button order (added left-to-right; AppKit renders right-to-left):
///   1000 "Paste Once"        → Once   (first = default = Return)
///   1001 "Cancel"            → Cancel (second = Esc per the AppKit cancel
///                                       heuristic documented on
///                                       `show_recovery_prompt`)
///   1002 "Always This Session" → AlwaysSession
///
/// Fail-closed: ANY other code (including the recovery-style Esc fallback)
/// lands on Cancel — an unrecognized response must never write to the PTY.
pub fn paste_response_from_modal(code: NSModalResponse) -> PastePromptResponse {
    const NS_ALERT_FIRST_BUTTON_RETURN: NSModalResponse = 1000;
    const NS_ALERT_SECOND_BUTTON_RETURN: NSModalResponse = 1001;
    const NS_ALERT_THIRD_BUTTON_RETURN: NSModalResponse = 1002;

    match code {
        NS_ALERT_FIRST_BUTTON_RETURN => PastePromptResponse::Once,
        // Explicit (not fall-through) so the Esc contract stays greppable:
        // the second button is "Cancel" per AppKit's cancel heuristic.
        NS_ALERT_SECOND_BUTTON_RETURN => PastePromptResponse::Cancel,
        NS_ALERT_THIRD_BUTTON_RETURN => PastePromptResponse::AlwaysSession,
        _ => PastePromptResponse::Cancel,
    }
}

/// v1.11.1: informative text for the paste confirmation — the preview as a
/// monospace-feel quote block plus a key legend. Pure function (tested).
fn paste_alert_informative_text(preview: &str) -> String {
    let mut text = String::new();
    for line in preview.lines() {
        text.push_str("> ");
        text.push_str(line);
        text.push('\n');
    }
    text.push('\n');
    text.push_str("回车：粘贴一次 · Esc：取消");
    text
}

/// Sanitize a raw preview for display inside the NSAlert: control characters
/// are rendered as visible placeholders so a control-char-flagged paste
/// shows WHY it was flagged instead of corrupting the dialog layout.
fn sanitize_preview_for_display(text: &str) -> String {
    text.chars()
        .map(|ch| match ch {
            '\n' | '\r' => ch,
            '\t' => ' ',
            c if c.is_control() => '\u{2400}', // ␀-style control pictures
            c => c,
        })
        .collect()
}

/// v1.11.1 (PLAN_v1111 §4.3): show the large/dangerous-paste confirmation.
///
/// MUST be invoked from a `dispatch2::DispatchQueue::main().exec_async`
/// block (never directly inside a winit handler — see
/// docs/FIX_RECOVERY_MODAL_SPIN.md); the choice returns through the event
/// loop proxy as `AppEvent::PasteDecided`. Esc maps to Cancel and Enter
/// (default button) maps to Once.
pub fn show_large_paste_prompt(
    mtm: MainThreadMarker,
    headline: String,
    preview: String,
) -> Result<PastePromptResponse, AlertError> {
    let alert = unsafe { NSAlert::new(mtm) };
    unsafe {
        alert.setMessageText(&NSString::from_str(&headline));
        alert.setInformativeText(&NSString::from_str(&paste_alert_informative_text(
            &sanitize_preview_for_display(&preview),
        )));
        alert.setAlertStyle(NSAlertStyle::Warning);

        // Order matters: first button = default (Return) = "Paste Once";
        // second = "Cancel" so AppKit's cancel heuristic routes Esc there
        // (see map_modal_response / show_recovery_prompt comments);
        // third = session-wide allowance.
        alert.addButtonWithTitle(&NSString::from_str("Paste Once"));
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));
        alert.addButtonWithTitle(&NSString::from_str("Always This Session"));

        // Belt-and-braces: bind Escape explicitly to the Cancel button in
        // case AppKit's implicit heuristic changes across macOS versions.
        if let Some(cancel) = alert.buttons().get(1) {
            cancel.setKeyEquivalent(&NSString::from_str("\u{1b}"));
        }
    }

    Ok(paste_response_from_modal(unsafe { alert.runModal() }))
}

pub fn show_block_export_preview(mtm: MainThreadMarker, markdown: &str) -> bool {
    let alert = unsafe { NSAlert::new(mtm) };
    unsafe {
        alert.setMessageText(&NSString::from_str("Export Redacted Block"));
        alert.setInformativeText(&NSString::from_str(&export_preview_excerpt(markdown)));
        alert.setAlertStyle(NSAlertStyle::Informational);
        alert.addButtonWithTitle(&NSString::from_str("Continue"));
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    }
    (unsafe { alert.runModal() }) == 1000
}

pub fn show_running_process_close_prompt(
    mtm: MainThreadMarker,
    title: &str,
    commands: &[String],
) -> bool {
    let alert = unsafe { NSAlert::new(mtm) };
    let info = running_process_prompt_text(commands);
    unsafe {
        alert.setMessageText(&NSString::from_str(title));
        alert.setInformativeText(&NSString::from_str(&info));
        alert.setAlertStyle(NSAlertStyle::Warning);
        alert.addButtonWithTitle(&NSString::from_str("Close Anyway"));
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    }
    close_prompt_response_allows_close(unsafe { alert.runModal() })
}

fn close_prompt_response_allows_close(response: NSModalResponse) -> bool {
    response == 1000
}

fn running_process_prompt_text(commands: &[String]) -> String {
    const MAX_COMMANDS: usize = 6;
    let count = commands.len();
    let noun = if count == 1 {
        "command is"
    } else {
        "commands are"
    };
    let mut text = format!(
        "{count} foreground {noun} still running. Closing will terminate {}.",
        if count == 1 { "it" } else { "them" }
    );
    for command in commands.iter().take(MAX_COMMANDS) {
        let command: String = command
            .chars()
            .take(120)
            .map(|ch| if ch.is_control() { ' ' } else { ch })
            .collect();
        text.push_str("\n\n• ");
        text.push_str(&command);
    }
    if count > MAX_COMMANDS {
        text.push_str(&format!("\n\n…and {} more", count - MAX_COMMANDS));
    }
    text
}

fn export_preview_excerpt(markdown: &str) -> String {
    const MAX_PREVIEW_CHARS: usize = 1200;
    let mut preview: String = markdown.chars().take(MAX_PREVIEW_CHARS).collect();
    if markdown.chars().count() > MAX_PREVIEW_CHARS {
        preview.push_str("\n\n[preview truncated]");
    }
    preview
}

fn map_modal_response(response: NSModalResponse) -> RecoveryPromptResponse {
    // NSAlertFirstButtonReturn = 1000
    const NS_ALERT_FIRST_BUTTON_RETURN: NSModalResponse = 1000;
    // NSAlertSecondButtonReturn = 1001
    const NS_ALERT_SECOND_BUTTON_RETURN: NSModalResponse = 1001;
    // NSAlertThirdButtonReturn = 1002
    const NS_ALERT_THIRD_BUTTON_RETURN: NSModalResponse = 1002;

    match response {
        NS_ALERT_FIRST_BUTTON_RETURN => RecoveryPromptResponse::Restore,
        NS_ALERT_SECOND_BUTTON_RETURN => RecoveryPromptResponse::Ignore,
        NS_ALERT_THIRD_BUTTON_RETURN => RecoveryPromptResponse::Delete,
        // Any other response (e.g. cancel) defaults to Ignore — a fresh
        // session starts; the old snapshot is superseded by autosave.
        _ => RecoveryPromptResponse::Ignore,
    }
}

/// Format a snapshot age in seconds as a human-readable string.
fn format_snapshot_age(secs: u64) -> String {
    if secs < 60 {
        format!("from {} seconds ago", secs)
    } else if secs < 3600 {
        format!("from {} minutes ago", secs / 60)
    } else if secs < 86400 {
        format!("from {} hours ago", secs / 3600)
    } else {
        format!("from {} days ago", secs / 86400)
    }
}

/// v1.11.5 (PLAN_v1115 §M3): show the OSC 52 clipboard-read permission
/// prompt.
///
/// MUST be invoked from a `dispatch2::DispatchQueue::main().exec_async`
/// block (same FIX_RECOVERY_MODAL_SPIN discipline as
/// `show_large_paste_prompt` / `show_recovery_prompt`); the answer returns
/// through the event-loop proxy as `AppEvent::Osc52ReadDecided`.
///
/// The headline deliberately says 「终端中的程序」 — weft cannot distinguish
/// a local child from an ssh remote, so claiming "SSH/远端" would be false
/// certainty (PLAN_v1115 §M3). Esc maps to Deny (the cancel heuristic);
/// Enter (default button) allows.
pub fn show_osc52_read_prompt(mtm: MainThreadMarker) -> Result<bool, AlertError> {
    let alert = unsafe { NSAlert::new(mtm) };
    unsafe {
        alert.setMessageText(&NSString::from_str("终端中的程序请求读取剪贴板"));
        alert.setInformativeText(&NSString::from_str(
            "允许后，剪贴板内容将被发送给终端里正在请求的程序（含 ssh 远端）。\n\
             拒绝后 30 秒内的读取请求不会再询问。",
        ));
        alert.setAlertStyle(NSAlertStyle::Warning);

        // First button = default (Return) = allow; second = Deny so Esc
        // routes there via AppKit's cancel heuristic (same order rationale
        // as show_large_paste_prompt).
        alert.addButtonWithTitle(&NSString::from_str("允许"));
        alert.addButtonWithTitle(&NSString::from_str("拒绝"));
        if let Some(cancel) = alert.buttons().get(1) {
            cancel.setKeyEquivalent(&NSString::from_str("\u{1b}"));
        }
    }

    // Button 0 = allow (NSAlertFirstButtonReturn = 1000), 1 = deny;
    // any other response fails closed to deny.
    Ok(unsafe { alert.runModal() } == 1000)
}

#[cfg(test)]
mod export_preview_tests {
    use super::{
        close_prompt_response_allows_close, export_preview_excerpt, running_process_prompt_text,
    };

    #[test]
    fn preview_is_bounded_on_unicode_boundary() {
        let markdown = "中".repeat(1300);
        let preview = export_preview_excerpt(&markdown);
        assert!(preview.ends_with("[preview truncated]"));
        assert!(preview.starts_with(&"中".repeat(1200)));
    }

    #[test]
    fn running_process_prompt_lists_and_bounds_commands() {
        let commands = vec!["opencode upgrade".to_owned(), "中".repeat(200)];
        let text = running_process_prompt_text(&commands);
        assert!(text.starts_with("2 foreground commands are still running"));
        assert!(text.contains("• opencode upgrade"));
        assert!(!text.contains(&"中".repeat(121)));
    }

    #[test]
    fn close_prompt_only_accepts_the_explicit_first_button() {
        assert!(close_prompt_response_allows_close(1000));
        assert!(!close_prompt_response_allows_close(1001));
        assert!(!close_prompt_response_allows_close(-1000));
        assert!(!close_prompt_response_allows_close(42));
    }

    #[test]
    fn recovery_choice_maps_every_prompt_response() {
        use super::{recovery_choice_from_response, RecoveryChoice, RecoveryPromptResponse};
        assert_eq!(
            recovery_choice_from_response(RecoveryPromptResponse::Restore),
            RecoveryChoice::Restore
        );
        assert_eq!(
            recovery_choice_from_response(RecoveryPromptResponse::Ignore),
            RecoveryChoice::Ignore
        );
        assert_eq!(
            recovery_choice_from_response(RecoveryPromptResponse::Delete),
            RecoveryChoice::Delete
        );
    }

    #[test]
    fn recovery_esc_and_unknown_responses_map_to_ignore() {
        // v1.10.23: Esc / Cmd+. resolves to NSModalResponse 1001 (the cancel
        // button is the second one per AppKit's cancel heuristic), and any
        // other response defaults to Ignore — both must land on
        // RecoveryChoice::Ignore so the snapshot is preserved.
        use super::{map_modal_response, recovery_choice_from_response, RecoveryChoice};
        for response in [1001, -1000, 42, 0] {
            assert_eq!(
                recovery_choice_from_response(map_modal_response(response)),
                RecoveryChoice::Ignore,
                "response {response} must fall through to Ignore"
            );
        }
    }

    #[test]
    fn running_process_prompt_handles_count_boundaries() {
        let empty = running_process_prompt_text(&[]);
        assert!(empty.starts_with("0 foreground commands are"));

        let one = running_process_prompt_text(&["sleep 5".to_owned()]);
        assert!(one.starts_with("1 foreground command is"));

        let six: Vec<String> = (1..=6).map(|index| format!("command-{index}")).collect();
        let six_text = running_process_prompt_text(&six);
        assert_eq!(six_text.matches("\n\n• ").count(), 6);
        assert!(!six_text.contains("and 1 more"));

        let mut seven = six;
        seven.push("command-7".to_owned());
        let seven_text = running_process_prompt_text(&seven);
        assert_eq!(seven_text.matches("\n\n• ").count(), 6);
        assert!(seven_text.contains("…and 1 more"));
    }

    #[test]
    fn prompt_bounds_and_sanitizes_command_summaries() {
        let exactly_120 = "a".repeat(120);
        let over_120 = "b".repeat(121);
        let text = running_process_prompt_text(&[
            exactly_120.clone(),
            over_120.clone(),
            "line1\r\nline2\t\u{7}".to_owned(),
        ]);
        assert!(text.contains(&exactly_120));
        assert!(!text.contains(&over_120));
        assert!(!text.contains('\r'));
        assert!(!text.contains('\t'));
        assert!(!text.contains('\u{7}'));
    }

    // ── v1.11.1 paste confirmation (PLAN_v1111 §4.3 / §5.3) ────────────

    use super::{paste_alert_informative_text, paste_response_from_modal, PastePromptResponse};

    #[test]
    fn paste_modal_codes_map_to_the_three_documented_buttons() {
        // 1000 = default (Return) → Once; 1001 = Cancel (Esc);
        // 1002 = Always This Session.
        assert_eq!(paste_response_from_modal(1000), PastePromptResponse::Once);
        assert_eq!(paste_response_from_modal(1001), PastePromptResponse::Cancel);
        assert_eq!(
            paste_response_from_modal(1002),
            PastePromptResponse::AlwaysSession
        );
    }

    #[test]
    fn paste_modal_unknown_codes_fail_closed_to_cancel() {
        // Fail-closed contract: any unrecognized response must NOT paste.
        for code in [-1000, -1, 0, 42, 999, 1003] {
            assert_eq!(
                paste_response_from_modal(code),
                PastePromptResponse::Cancel,
                "response {code} must fail closed to Cancel"
            );
        }
    }

    #[test]
    fn paste_informative_text_quotes_each_line_and_documents_keys() {
        let text = paste_alert_informative_text("first\nsecond");
        assert!(text.starts_with("> first\n> second\n"));
        assert!(text.ends_with("回车：粘贴一次 · Esc：取消"));
        // Single-line preview still gets the quote prefix + legend.
        let single = paste_alert_informative_text("only");
        assert!(single.starts_with("> only\n"));
        assert!(single.contains("Esc"));
    }

    #[test]
    fn paste_preview_display_sanitizes_control_chars() {
        use super::sanitize_preview_for_display;
        let sanitized = sanitize_preview_for_display("ok\u{1b}[2J\tend");
        assert!(sanitized.starts_with("ok"));
        assert!(
            !sanitized.contains('\u{1b}'),
            "ESC must not reach the dialog"
        );
        assert!(
            sanitized.contains('\u{2400}'),
            "control chars become visible pictures"
        );
        assert!(sanitized.contains(' '), "tab renders as a space");
        // Newlines survive so multi-line previews keep their shape.
        assert_eq!(
            sanitize_preview_for_display("a\nb"),
            "a\nb",
            "newlines are structural, not dangerous"
        );
    }
}
