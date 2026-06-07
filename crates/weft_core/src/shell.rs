//! Shell integration: weft.sh hook injection and command boundary detection.

/// Shell integration script for zsh.
/// Injected into the PTY at startup to send OSC 133 markers.
pub const WEFT_ZSH_HOOK: &str = r#"
__weft_precmd() {
    printf '\033]133;A\007'
}

__weft_preexec() {
    printf '\033]133;B\007'
}

__weft_postexec() {
    printf '\033]133;D;%d\007' $?
}

autoload -Uz add-zsh-hook
add-zsh-hook precmd __weft_precmd
add-zsh-hook preexec __weft_preexec
"#;

/// Shell integration script for bash.
pub const WEFT_BASH_HOOK: &str = r#"
__weft_prompt_command() {
    printf '\033]133;A\007'
}

__weft_preexec() {
    printf '\033]133;B\007'
}

__weft_postexec() {
    local __weft_exit_code=$?
    printf '\033]133;D;%d\007' $__weft_exit_code
    return $__weft_exit_code
}

PROMPT_COMMAND="__weft_prompt_command"
trap '__weft_preexec' DEBUG
"#;

/// Detect which shell is being used and return the appropriate hook script.
pub fn hook_for_shell(shell: &str) -> Option<&'static str> {
    if shell.contains("zsh") {
        Some(WEFT_ZSH_HOOK)
    } else if shell.contains("bash") {
        Some(WEFT_BASH_HOOK)
    } else {
        None
    }
}

/// Format the hook script for injection via PTY stdin.
/// Wraps the hook in a way that the shell can evaluate it.
pub fn format_injection_command(shell: &str) -> Option<String> {
    let hook = hook_for_shell(shell)?;
    // Use a heredoc-like approach: eval the hook string
    Some(format!(
        "eval '{}'{}\n",
        hook.replace('\'', "'\\''"),
        // Clear the line to avoid showing the eval command
        "\x15" // Ctrl-U: clear line
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zsh_hook_detected() {
        assert!(hook_for_shell("/bin/zsh").is_some());
        assert!(hook_for_shell("zsh").is_some());
    }

    #[test]
    fn bash_hook_detected() {
        assert!(hook_for_shell("/bin/bash").is_some());
        assert!(hook_for_shell("bash").is_some());
    }

    #[test]
    fn unknown_shell_no_hook() {
        assert!(hook_for_shell("/bin/fish").is_none());
        assert!(hook_for_shell("sh").is_none());
    }

    #[test]
    fn injection_command_format() {
        let cmd = format_injection_command("zsh");
        assert!(cmd.is_some());
        let cmd = cmd.unwrap();
        assert!(cmd.contains("weft_precmd"));
    }

    #[test]
    fn zsh_hook_contains_osc133() {
        assert!(WEFT_ZSH_HOOK.contains("133;A"));
        assert!(WEFT_ZSH_HOOK.contains("133;B"));
        assert!(WEFT_ZSH_HOOK.contains("133;D"));
    }

    #[test]
    fn bash_hook_contains_osc133() {
        assert!(WEFT_BASH_HOOK.contains("133;A"));
        assert!(WEFT_BASH_HOOK.contains("133;B"));
        assert!(WEFT_BASH_HOOK.contains("133;D"));
    }
}
