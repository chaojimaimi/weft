use crate::blocks::ShellPhase;

/// Effective input routing at any given moment.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum InputMode {
    /// Bytes flow straight to the PTY (current behavior).
    Passthrough,
    /// Keys drive the editor; submit writes the command.
    Editor,
}

/// Decide passthrough vs. editor. See DECISION-input-architecture.md §5.2.
/// `command_from_editor` covers the Enter→preexec window: after submit but
/// before `133;B`, we must pass through so trailing keystrokes reach the
/// program rather than a frozen editor.
pub fn effective_mode(
    shell_phase: ShellPhase,
    alt_active: bool,
    bootstrap_ready: bool,
    command_from_editor: bool,
) -> InputMode {
    if alt_active || !bootstrap_ready || command_from_editor {
        return InputMode::Passthrough;
    }
    match shell_phase {
        ShellPhase::NotIntegrated | ShellPhase::CommandExecuting => InputMode::Passthrough,
        ShellPhase::AtPrompt => InputMode::Editor,
    }
}
