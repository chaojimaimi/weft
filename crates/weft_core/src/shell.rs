//! Shell integration: OSC 133 command-boundary markers, without echoing.
//!
//! ## Why not PTY-stdin injection
//! Writing the hook into the PTY stdin of an *interactive* shell echoes the
//! script source into the terminal (and a `eval '<multi-line>'` form drops zsh
//! into a `quote>` continuation). So we never touch stdin. Instead we redirect
//! the shell's own rc-file lookup so the shell sources our integration after
//! the user's config — producing no visible output.
//!
//! ## How (kitty/iTerm2 model)
//! - **zsh (macOS default, zero-config):** we point `ZDOTDIR` at a cache dir
//!   holding a generated `.zshenv`. That file restores the user's real
//!   `ZDOTDIR` (so `~/.zshrc` still loads), sources their real `.zshenv`, then
//!   appends OSC 133 `precmd`/`preexec` hooks (interactive shells only).
//! - **bash (best-effort):** bash has no clean interactive rc-redirect, so we
//!   set [`INTEGRATION_ENV`] and ship a snippet the user sources with one line
//!   in `~/.bashrc`.
//! - **other shells:** no integration (graceful passthrough).
//!
//! ## OSC 133 sequence
//! `133;A` prompt start · `133;B` command start · `133;C` output start ·
//! `133;D;<exit>` command end. precmd emits `D` (previous exit code) then `A`;
//! preexec emits `B` then `C`.

/// Env var weft sets on the child shell to signal "integration is on".
/// The generated zsh `.zshenv` and the bash snippet gate themselves on this.
pub const INTEGRATION_ENV: &str = "WEFT_SHELL_INTEGRATION";

/// Env var carrying the user's original `ZDOTDIR`, so the generated `.zshenv`
/// can restore it before `~/.zshrc` loads. Only set when the user had a
/// non-empty `ZDOTDIR`; absent otherwise (the script then `unset`s ZDOTDIR).
pub const ORIG_ZDOTDIR_ENV: &str = "WEFT_ORIG_ZDOTDIR";

/// The redirect env var name zsh honors for rc-file lookup.
const ZDOTDIR_ENV: &str = "ZDOTDIR";

/// Generated zsh `.zshenv` body. Sourced first because weft points `ZDOTDIR`
/// at the dir holding this file.
///
/// Order matters: (1) restore real ZDOTDIR, (2) source the user's real
/// `.zshenv` we intercepted, (3) register markers — interactive only.
const ZSH_ZSHENV_BODY: &str = "\
# weft shell integration — generated, do not edit.
# zsh sources this first because weft pointed ZDOTDIR here. Restore the user's
# real ZDOTDIR so ~/.zshrc / ~/.zlogin load from the right place afterward.
if [ -n \"${WEFT_ORIG_ZDOTDIR:-}\" ]; then
    ZDOTDIR=\"${WEFT_ORIG_ZDOTDIR}\"
else
    unset ZDOTDIR
fi
# Source the user's real .zshenv (we intercepted its lookup).
__weft_real_zshenv=\"${ZDOTDIR:-$HOME}/.zshenv\"
[ -r \"$__weft_real_zshenv\" ] && . \"$__weft_real_zshenv\"

# OSC 133 command-boundary markers — interactive shells only.
# `[[ -o interactive ]]` (not `[ -o interactive ]`, whose `-o` zsh parses as a
# binary OR inside `[`, yielding a too-many-arguments error).
if [[ -n \"${WEFT_SHELL_INTEGRATION:-}\" ]] && [[ -o interactive ]]; then
    __weft_precmd() {
        local __weft_rc=$?
        printf '\\033]133;D;%d\\007' \"$__weft_rc\"
        printf '\\033]133;A\\007'
        printf '\\033]7;file://%s%s\\007' \"${HOSTNAME:-$HOST}\" \"$PWD\"
        # weft renders the prompt in its input box; blank the shell's PS1 so
        # the grid doesn't show a duplicate prompt line at the shell cursor.
        PROMPT=''
    }
    __weft_preexec() {
        printf '\\033]133;B\\007'
        printf '\\033]133;C\\007'
    }
    # Prepend so our precmd runs before user hooks and captures a clean exit code.
    precmd_functions=(__weft_precmd ${precmd_functions[@]})
    preexec_functions=(__weft_preexec ${preexec_functions[@]})
fi
";

/// Bash integration snippet. bash has no interactive rc-redirect, so the user
/// sources this themselves (one line in `~/.bashrc`):
/// `[ -n "$WEFT_SHELL_INTEGRATION" ] && . ~/.cache/weft/bash-integration.sh`
///
/// Note: bash's DEBUG trap fires per simple-command, so `B` may emit more than
/// once for compound commands — best-effort, unlike the robust zsh path.
const BASH_INTEGRATION_BODY: &str = "\
# weft shell integration — source from ~/.bashrc:
#   [ -n \"$WEFT_SHELL_INTEGRATION\" ] && . ~/.cache/weft/bash-integration.sh
__weft_bash_precmd() {
    local __weft_rc=$?
    printf '\\033]133;D;%d\\007' \"$__weft_rc\"
    printf '\\033]133;A\\007'
    printf '\\033]7;file://%s%s\\007' \"$HOSTNAME\" \"$PWD\"
    PS1=''
}
__weft_bash_preexec() {
    printf '\\033]133;B\\007'
    printf '\\033]133;C\\007'
}
if [ -n \"${WEFT_SHELL_INTEGRATION:-}\" ]; then
    PROMPT_COMMAND=\"__weft_bash_precmd${PROMPT_COMMAND:+;$PROMPT_COMMAND}\"
    trap '__weft_bash_preexec' DEBUG
fi
";

/// A file the caller must write under its chosen redirect directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RcFile {
    /// Filename relative to the redirect dir, e.g. `.zshenv`.
    pub filename: &'static str,
    /// File contents.
    pub body: &'static str,
}

/// Shell-integration strategy chosen for a shell binary path.
///
/// Pure description of *what to do*; the caller (app layer) performs the file
/// writes and env wiring, keeping this module free of filesystem effects so it
/// is trivially unit-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Integration {
    /// zsh: zero-config rc-redirect. Write [`Self::rc_file`] under a dir, set
    /// [`Self::redirect_env`] to that dir on the child.
    Zsh,
    /// bash: env-var gated. Set [`INTEGRATION_ENV`]; the user sources
    /// [`Self::sourceable_snippet`] themselves.
    Bash,
    /// Unsupported shell — no integration.
    None,
}

impl Integration {
    /// Pick a strategy from a shell path or name (e.g. `/bin/zsh`, `zsh`).
    pub fn from_shell(shell: &str) -> Self {
        let base = shell.rsplit('/').next().unwrap_or(shell);
        match base {
            "zsh" => Integration::Zsh,
            "bash" => Integration::Bash,
            _ => Integration::None,
        }
    }

    /// True when weft knows how to integrate this shell.
    pub fn is_supported(self) -> bool {
        !matches!(self, Integration::None)
    }

    /// Env var (e.g. `ZDOTDIR`) the caller should set to the redirect dir, plus
    /// the file to write under it. `None` for shells without an rc-redirect.
    pub fn rc_redirect(self) -> Option<(&'static str, RcFile)> {
        match self {
            Integration::Zsh => Some((
                ZDOTDIR_ENV,
                RcFile {
                    filename: ".zshenv",
                    body: ZSH_ZSHENV_BODY,
                },
            )),
            Integration::Bash | Integration::None => None,
        }
    }

    /// A sourceable snippet for env-var-gated shells (bash). `None` otherwise.
    pub fn sourceable_snippet(self) -> Option<&'static str> {
        match self {
            Integration::Bash => Some(BASH_INTEGRATION_BODY),
            Integration::Zsh | Integration::None => None,
        }
    }

    /// Env vars to set on the child process.
    ///
    /// `orig_zdotdir` is the user's current `ZDOTDIR` value, if any — passed
    /// through [`ORIG_ZDOTDIR_ENV`] so the generated zsh `.zshenv` can restore
    /// it. Pass `None` when the user has no `ZDOTDIR`.
    pub fn child_env(self, orig_zdotdir: Option<&str>) -> Vec<(&'static str, String)> {
        let mut env = vec![(INTEGRATION_ENV, "1".to_string())];
        if let Some(zd) = orig_zdotdir.filter(|s| !s.is_empty()) {
            env.push((ORIG_ZDOTDIR_ENV, zd.to_string()));
        }
        env
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_zsh_from_path_or_name() {
        assert_eq!(Integration::from_shell("/bin/zsh"), Integration::Zsh);
        assert_eq!(Integration::from_shell("/usr/bin/zsh"), Integration::Zsh);
        assert_eq!(Integration::from_shell("zsh"), Integration::Zsh);
        assert!(Integration::Zsh.is_supported());
    }

    #[test]
    fn detects_bash_from_path_or_name() {
        assert_eq!(Integration::from_shell("/bin/bash"), Integration::Bash);
        assert_eq!(
            Integration::from_shell("/opt/homebrew/bin/bash"),
            Integration::Bash
        );
        assert_eq!(Integration::from_shell("bash"), Integration::Bash);
        assert!(Integration::Bash.is_supported());
    }

    #[test]
    fn unsupported_shells_are_none() {
        assert_eq!(Integration::from_shell("/bin/fish"), Integration::None);
        assert_eq!(Integration::from_shell("/bin/sh"), Integration::None);
        assert_eq!(Integration::from_shell("nu"), Integration::None);
        assert!(!Integration::None.is_supported());
    }

    #[test]
    fn zsh_uses_zdotdir_redirect() {
        let (env_var, file) = Integration::Zsh.rc_redirect().expect("zsh has rc redirect");
        assert_eq!(env_var, "ZDOTDIR");
        assert_eq!(file.filename, ".zshenv");
        // Restores the user's real ZDOTDIR before ~/.zshrc loads.
        assert!(file.body.contains("WEFT_ORIG_ZDOTDIR"));
        assert!(file.body.contains("unset ZDOTDIR"));
        // Sources the intercepted user .zshenv.
        assert!(file.body.contains(".zshenv"));
        // Interactive-only guard (zsh `[[ ]]` form — `[ -o ]` would misparse).
        assert!(file.body.contains("[[ -o interactive ]]"));
    }

    #[test]
    fn bash_has_no_redirect_but_ships_snippet() {
        assert!(Integration::Bash.rc_redirect().is_none());
        let snippet = Integration::Bash
            .sourceable_snippet()
            .expect("bash ships snippet");
        assert!(snippet.contains("WEFT_SHELL_INTEGRATION"));
    }

    #[test]
    fn zsh_has_no_sourceable_snippet() {
        // zsh is zero-config; no manual source step.
        assert!(Integration::Zsh.sourceable_snippet().is_none());
    }

    #[test]
    fn osc133_markers_present_in_zsh_body() {
        let (_, file) = Integration::Zsh.rc_redirect().unwrap();
        // precmd: D (exit code) then A (prompt start).
        assert!(file.body.contains("133;D;%d"));
        assert!(file.body.contains("133;A"));
        // preexec: B (command start) then C (output start).
        assert!(file.body.contains("133;B"));
        assert!(file.body.contains("133;C"));
    }

    #[test]
    fn osc133_markers_present_in_bash_snippet() {
        let snippet = Integration::Bash.sourceable_snippet().unwrap();
        assert!(snippet.contains("133;D;%d"));
        assert!(snippet.contains("133;A"));
        assert!(snippet.contains("133;B"));
        assert!(snippet.contains("133;C"));
    }

    #[test]
    fn zsh_hooks_use_function_arrays_not_overwrite() {
        let (_, file) = Integration::Zsh.rc_redirect().unwrap();
        // Appends to precmd_functions/preexec_functions (preserves user hooks),
        // rather than redefining the special `precmd`/`preexec` functions.
        assert!(file.body.contains("precmd_functions=("));
        assert!(file.body.contains("preexec_functions=("));
    }

    #[test]
    fn child_env_always_sets_integration_flag() {
        let env = Integration::Zsh.child_env(None);
        assert_eq!(
            env.iter().find(|(k, _)| *k == INTEGRATION_ENV),
            Some(&(INTEGRATION_ENV, "1".to_string()))
        );
    }

    #[test]
    fn child_env_forwards_orig_zdotdir_when_set() {
        let env = Integration::Zsh.child_env(Some("/Users/me/.config/zsh"));
        assert_eq!(
            env.iter().find(|(k, _)| *k == ORIG_ZDOTDIR_ENV),
            Some(&(ORIG_ZDOTDIR_ENV, "/Users/me/.config/zsh".to_string()))
        );
    }

    #[test]
    fn child_env_omits_orig_zdotdir_when_unset() {
        let env = Integration::Zsh.child_env(None);
        assert!(env.iter().all(|(k, _)| *k != ORIG_ZDOTDIR_ENV));

        // Empty string is treated as unset (avoids setting ZDOTDIR="" in the script).
        let env_empty = Integration::Zsh.child_env(Some(""));
        assert!(env_empty.iter().all(|(k, _)| *k != ORIG_ZDOTDIR_ENV));
    }

    #[test]
    fn child_env_identical_across_supported_shells() {
        // Both supported shells just flip the integration flag (+ zdotdir for zsh).
        let zsh = Integration::Zsh.child_env(None);
        let bash = Integration::Bash.child_env(None);
        assert_eq!(zsh.first(), bash.first());
    }

    // ── v0.5: OSC 7 cwd emission ──────────────────────────────────

    #[test]
    fn zsh_hook_emits_osc7_cwd() {
        let (_, file) = Integration::Zsh.rc_redirect().unwrap();
        // precmd emits OSC 7 with ${HOSTNAME:-$HOST} + $PWD (zsh defaults to
        // $HOST, so fall back to it when HOSTNAME is unset).
        assert!(
            file.body.contains("\\033]7;file://%s%s\\007"),
            "zsh precmd must emit OSC 7; body was:\n{}",
            file.body
        );
        assert!(file.body.contains("${HOSTNAME:-$HOST}"));
        assert!(file.body.contains("$PWD"));
        // weft renders the prompt in its input box; shell PS1 is suppressed.
        assert!(file.body.contains("PROMPT=''"));
    }

    #[test]
    fn bash_hook_emits_osc7_cwd() {
        let snippet = Integration::Bash.sourceable_snippet().unwrap();
        assert!(
            snippet.contains("\\033]7;file://%s%s\\007"),
            "bash precmd must emit OSC 7; snippet was:\n{}",
            snippet
        );
        assert!(snippet.contains("PS1=''"));
    }
}
