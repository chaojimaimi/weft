//! Pure helpers + shell integration extracted from `main.rs`.
//!
//! These are free functions (not `impl App` methods) used across the app:
//! prompt/command parsing, workflow seeding, chord label formatting, cache
//! dir resolution, first-run onboarding, and shell-integration env setup.

use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{info, warn};
use weft_core::shell::Integration;

/// Strip prompt artifacts from a captured command string. When a command was
/// captured via `snapshot_command_line()` (passthrough mode, or pre-editor
/// sessions), the grid row includes the shell prompt — e.g.
/// `~/projects/foo ❯ ls -la`. This strips everything up to and including the
/// last prompt marker (❯ ❮ › $ % #) so only the command remains.
///
/// A marker is only recognized when followed by a space (so `$HOME` in a
/// command is not mistaken for a `$` prompt).
pub(crate) fn strip_prompt_prefix(s: &str) -> String {
    // Patterns: "❯ ", "❮ ", "› ", "$ ", "% ", "# " — the marker + space.
    let markers = ["❯ ", "❮ ", "› ", "$ ", "% ", "# "];
    // Find the LAST marker occurrence (prompts may contain `$`/`#` in paths).
    let mut best: Option<usize> = None;
    for marker in &markers {
        let mut search_from = 0;
        while let Some(idx) = s[search_from..].find(marker) {
            best = Some(best.map_or(search_from + idx, |b| b.max(search_from + idx)));
            search_from += idx + marker.len();
        }
    }
    if let Some(idx) = best {
        let after = s[idx..].trim_start_matches(['❯', '❮', '›', '$', '%', '#', ' ']);
        if !after.is_empty() {
            return after.to_string();
        }
    }
    s.to_string()
}

pub(crate) fn word_at(line: &str, col: usize) -> Option<(usize, usize)> {
    let chars: Vec<char> = line.chars().collect();
    if chars.is_empty() {
        return None;
    }
    let end = col.min(chars.len());
    let mut start = end;
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    if start == end {
        return None; // cursor on whitespace
    }
    Some((start, end))
}

/// Whether the word at `word_start` is in command position (line start, or
/// after a shell operator `| & ; > <`). Decides whether the `$PATH` command
/// completion source is consulted.
pub(crate) fn is_command_position(line: &str, word_start: usize) -> bool {
    let chars: Vec<char> = line.chars().collect();
    let mut i = word_start;
    while i > 0 && chars[i - 1].is_whitespace() {
        i -= 1;
    }
    if i == 0 {
        return true;
    }
    matches!(chars[i - 1], '|' | '&' | ';' | '>' | '<')
}

/// Insert built-in workflow templates on first launch (empty DB).
#[allow(clippy::type_complexity)]
pub(crate) fn seed_workflows(store: &weft_core::workflow::WorkflowStore) {
    use weft_core::workflow::{Workflow, WorkflowSource, WorkflowStep, WorkflowVar};

    let seeds: &[(&str, &str, &[&str], &[(bool, &str, &str, bool)])] = &[
        // name, description, commands, vars: (is_default, name, default, required)
        (
            "sync",
            "git pull current branch",
            &["git pull origin $(git branch --show-current)"],
            &[],
        ),
        (
            "dev",
            "start dev server",
            &["cd {{project}} && npm run dev"],
            &[(true, "project", ".", true)],
        ),
        (
            "logs",
            "tail service logs",
            &["tail -f {{file}}"],
            &[(true, "file", "/var/log/system.log", true)],
        ),
        (
            "gst",
            "git status + recent log",
            &["git status -sb", "git log --oneline -5"],
            &[],
        ),
        (
            "dclean",
            "prune dangling docker resources",
            &["docker system prune -f"],
            &[],
        ),
    ];

    for (name, desc, cmds, vars) in seeds {
        let workflow = Workflow {
            id: 0,
            name: (*name).into(),
            description: (*desc).into(),
            steps: cmds
                .iter()
                .map(|c| WorkflowStep {
                    command: (*c).into(),
                })
                .collect(),
            variables: vars
                .iter()
                .map(|(has_default, vname, vdefault, vreq)| WorkflowVar {
                    name: (*vname).into(),
                    description: String::new(),
                    default: if *has_default {
                        Some((*vdefault).into())
                    } else {
                        None
                    },
                    required: *vreq,
                })
                .collect(),
            source: WorkflowSource::Manual,
            use_count: 0,
            last_used_ms: 0,
        };
        if let Err(e) = store.insert(&workflow) {
            warn!(error = %e, workflow = name, "failed to seed workflow");
        }
    }
    info!("seeded {} built-in workflows", seeds.len());
}

pub(crate) fn resolve_text_char(text: Option<&str>, fallback: char, shift: bool) -> char {
    if let Some(s) = text {
        let mut it = s.chars();
        if let (Some(c), None) = (it.next(), it.next()) {
            if !c.is_control() {
                return c;
            }
        }
    }
    if shift {
        fallback.to_ascii_uppercase()
    } else {
        fallback
    }
}

/// v1.0 S1: Format a `(KeyCode, Modifiers)` pair as a human-readable chord
/// string (e.g. "cmd+c", "shift+page_up", "cmd+shift+t"). Used by the
/// Settings panel's Keybindings tab.
pub(crate) fn chord_label(
    key: weft_core::input::KeyCode,
    mods: weft_core::input::Modifiers,
) -> String {
    use weft_core::input::{KeyCode, Modifiers};
    let mut parts: Vec<&str> = Vec::new();
    if mods.contains(Modifiers::SUPER) {
        parts.push("cmd");
    }
    if mods.contains(Modifiers::SHIFT) {
        parts.push("shift");
    }
    if mods.contains(Modifiers::ALT) {
        parts.push("alt");
    }
    if mods.contains(Modifiers::CONTROL) {
        parts.push("ctrl");
    }
    let key_str = match key {
        KeyCode::Char(c) => {
            // Lowercase letters for chord display (cmd+c not cmd+C).
            return {
                let mut s = parts.join("+");
                if !s.is_empty() {
                    s.push('+');
                }
                s.push(c.to_ascii_lowercase());
                s
            };
        }
        KeyCode::Enter => "enter",
        KeyCode::Backspace => "backspace",
        KeyCode::Tab => "tab",
        KeyCode::Escape => "esc",
        KeyCode::Up => "up",
        KeyCode::Down => "down",
        KeyCode::Left => "left",
        KeyCode::Right => "right",
        KeyCode::Home => "home",
        KeyCode::End => "end",
        KeyCode::PageUp => "page_up",
        KeyCode::PageDown => "page_down",
        KeyCode::Delete => "delete",
        _ => "other",
    };
    parts.push(key_str);
    parts.join("+")
}

/// Resolve weft's cache dir: `$XDG_CACHE_HOME/weft`, else `~/.cache/weft`.
/// `None` when neither `XDG_CACHE_HOME` nor `HOME` is set.
pub(crate) fn weft_cache_dir() -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("weft"));
        }
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache").join("weft"))
}

/// v1.0 V13: Onboarding — first-run welcome message.
///
/// Detects first launch by checking `~/.config/weft/.first_run`. On first
/// launch, returns a `printf` command string that prints a short welcome
/// banner with core shortcuts. The caller writes this to the PTY right
/// after spawn, so it shows up in the user's first shell session. The
/// `.first_run` marker is created here (not by the caller).
///
/// Returns `None` on subsequent launches or if the config dir can't be
/// resolved (we'd rather skip onboarding than spam the user every launch).
pub(crate) fn first_run_welcome() -> Option<String> {
    use std::path::PathBuf;
    // Resolve config dir: $XDG_CONFIG_HOME/weft or ~/.config/weft
    let dir = if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            PathBuf::from(xdg).join("weft")
        } else {
            return None;
        }
    } else {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config").join("weft"))?
    };
    let marker = dir.join(".first_run");
    if marker.exists() {
        return None;
    }
    // Create marker immediately (best-effort). Even if the printf write
    // fails later, we don't want to re-show the welcome on every launch.
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(&marker, b"1");
    // Leading space + HIST_IGNORE_SPACE (default in zsh) keeps this out of
    // shell history. The printf is one-shot; it doesn't persist anywhere.
    Some(welcome_banner_command(env!("CARGO_PKG_VERSION")))
}

fn welcome_banner_command(version: &str) -> String {
    // Keep the command itself printable and let shell printf decode `\033`.
    // Rust's Debug string formatting would encode ESC as the literal text
    // `\u{1b}`, which both zsh and bash display instead of interpreting.
    let banner = format!(
        "\\033[2m# Welcome to Weft v{version}\\033[0m\\n\
         \\033[2m# Core shortcuts:\\033[0m\\n\
         \\033[2m#   Cmd+T        New tab      Cmd+W  Close pane/tab\\033[0m\\n\
         \\033[2m#   Cmd+Shift+[  Prev tab     Cmd+Shift+]  Next tab\\033[0m\\n\
         \\033[2m#   Cmd+P        Command palette (fuzzy)\\033[0m\\n\
         \\033[2m#   Cmd+F        Find         Cmd+Shift+B  Toggle sidebar\\033[0m\\n\
         \\033[2m#   Cmd+,        Settings     Cmd+Shift+T  Cycle theme\\033[0m\\n\
         \\033[2m# Block view groups commands and output. Type a command and press Enter.\\033[0m\\n"
    );
    // Leading space keeps this out of zsh history (HIST_IGNORE_SPACE default).
    format!(" printf '%b' '{banner}'\n")
}

/// Configure shell integration for the child shell and return env overrides.
///
/// - **zsh:** writes a generated `.zshenv` to `<cache>/zsh/` and points
///   `ZDOTDIR` there. The shell sources our OSC 133 hooks itself — no stdin
///   injection, no echo. The user's real `~/.zshrc` still loads (the generated
///   `.zshenv` restores `ZDOTDIR` first).
/// - **bash:** ships a snippet at `<cache>/bash-integration.sh`; the user opts
///   in with one `source` line in `~/.bashrc` (no clean interactive redirect).
///
/// Returns `KEY=VALUE` overrides to pass to the PTY. Terminal capabilities and
/// locale survive shell-integration setup failures.
pub(crate) fn shell_integration_env(shell: &str) -> Vec<(String, String)> {
    let mut env = crate::app_runtime::terminal_capability_env();

    // v1.0 fix: ensure UTF-8 locale for the child shell. When weft is launched
    // from Finder (.app bundle), the GUI environment typically lacks LANG /
    // LC_CTYPE, so the shell falls back to the `C` locale and tools like `ls`
    // render non-ASCII filenames (中文, etc.) as `?`. Force a UTF-8 locale
    // unless the user already has one set.
    let lang_ok = std::env::var("LANG").is_ok_and(|l| l.contains("UTF-8") || l.contains("utf8"));
    let lc_ctype_ok =
        std::env::var("LC_CTYPE").is_ok_and(|l| l.contains("UTF-8") || l.contains("utf8"));
    if !lang_ok && !lc_ctype_ok {
        // Prefer en_US.UTF-8 (always available on macOS); fall back to C.UTF-8.
        env.push(("LANG".to_string(), "en_US.UTF-8".to_string()));
    }

    let plan = Integration::from_shell(shell);
    if !plan.is_supported() {
        return env;
    }
    let Some(cache_root) = weft_cache_dir() else {
        warn!("HOME/XDG_CACHE_HOME unset — shell integration disabled");
        return env;
    };
    let base_len = env.len();
    let orig_zdotdir = std::env::var("ZDOTDIR").ok();
    env.extend(
        plan.child_env(orig_zdotdir.as_deref())
            .into_iter()
            .map(|(k, v)| (k.to_string(), v)),
    );

    // zsh: write the generated .zshenv and redirect ZDOTDIR at its directory.
    if let Some((redirect_var, file)) = plan.rc_redirect() {
        let dir = cache_root.join("zsh");
        if let Err(e) = std::fs::create_dir_all(&dir)
            .and_then(|_| std::fs::write(dir.join(file.filename), file.body))
        {
            warn!(error = %e, "failed to write zsh integration .zshenv; integration disabled");
            env.truncate(base_len);
            return env;
        }
        env.push((redirect_var.to_string(), dir.to_string_lossy().into_owned()));
    }

    // bash: ship the snippet so users can source it.
    if let Some(snippet) = plan.sourceable_snippet() {
        let path = cache_root.join("bash-integration.sh");
        if let Err(e) =
            std::fs::create_dir_all(&cache_root).and_then(|_| std::fs::write(&path, snippet))
        {
            warn!(error = %e, "failed to write bash integration snippet");
        } else {
            info!(
                path = %path.display(),
                "bash integration snippet written — add to ~/.bashrc: \
                 `[ -n \"$WEFT_SHELL_INTEGRATION\" ] && . \"{}\"`",
                path.display(),
            );
        }
    }

    env
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that mutate `XDG_CONFIG_HOME` — env vars are
    /// process-global, so parallel tests that touch the same var would
    /// clobber each other's values.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn word_at_picks_token_left_of_cursor() {
        assert_eq!(word_at("ls -l", 5), Some((3, 5))); // "-l"
        assert_eq!(word_at("ls", 2), Some((0, 2))); // "ls"
    }

    #[test]
    fn word_at_none_on_whitespace_or_empty() {
        assert_eq!(word_at("ls ", 3), None); // cursor on trailing space
        assert_eq!(word_at("", 0), None);
    }

    #[test]
    fn is_command_position_first_word_or_after_operator() {
        assert!(is_command_position("ls", 0));
        assert!(is_command_position("a | b", 4)); // "b" after pipe
        assert!(!is_command_position("ls -l", 3)); // "-l" is an arg
    }

    #[test]
    fn strip_prompt_prefix_removes_cwd_and_marker() {
        assert_eq!(strip_prompt_prefix("~/projects/foo ❯ ls -la"), "ls -la");
        assert_eq!(strip_prompt_prefix("❯ echo hi"), "echo hi");
    }

    #[test]
    fn strip_prompt_prefix_keeps_plain_commands() {
        assert_eq!(strip_prompt_prefix("git status"), "git status");
        assert_eq!(strip_prompt_prefix("ls"), "ls");
    }

    #[test]
    fn strip_prompt_prefix_handles_root_prompts() {
        assert_eq!(strip_prompt_prefix("# whoami"), "whoami");
        assert_eq!(strip_prompt_prefix("user@host:~$ ls"), "ls");
    }

    #[test]
    fn strip_prompt_prefix_keeps_dollar_in_command() {
        // `$HOME` should NOT be stripped (no space after $, it's part of cmd).
        assert_eq!(strip_prompt_prefix("echo $HOME"), "echo $HOME");
    }

    // ── T5: first_run marker logic + supplementary prompt tests ─────────

    /// Verify the `.first_run` marker existence logic that
    /// `first_run_welcome()` relies on: on a fresh config dir the marker is
    /// absent (→ welcome should show); after the function runs once, the
    /// marker exists (→ welcome should not show again).
    #[test]
    fn first_run_marker_created_on_first_call_only() {
        let _env = ENV_LOCK.lock().unwrap();
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let tmp = std::env::temp_dir().join(format!("weft-first-run-{pid}-{id}"));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let old_xdg = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", &tmp);

        // Before first call: marker should not exist.
        let marker = tmp.join("weft").join(".first_run");
        assert!(!marker.exists(), "marker should not exist before first run");

        // First call: should return a welcome banner and create the marker.
        let first = first_run_welcome();
        assert!(first.is_some(), "first run should return a welcome banner");
        let banner = first.unwrap();
        assert!(
            banner.contains("printf"),
            "banner should be a printf command, got: {banner:?}"
        );
        assert!(
            banner.contains("Welcome"),
            "banner should contain welcome text"
        );
        assert!(
            banner.contains(concat!("Weft v", env!("CARGO_PKG_VERSION"))),
            "banner should report the current package version: {banner:?}"
        );
        assert!(
            !banner.contains("\\u{1b}") && !banner.contains('\x1b'),
            "PTY command must contain portable printable escapes: {banner:?}"
        );
        let output = std::process::Command::new("/bin/sh")
            .args(["-c", &banner])
            .output()
            .expect("execute welcome printf");
        assert!(output.status.success());
        assert!(output.stdout.starts_with(b"\x1b[2m# Welcome"));
        assert!(output.stdout.ends_with(b"\x1b[0m\n"));
        assert!(marker.exists(), "marker should be created after first run");

        // Second call: marker now exists → should return None.
        let second = first_run_welcome();
        assert!(
            second.is_none(),
            "second run should not return welcome (marker exists)"
        );

        // Restore env and clean up.
        match old_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn strip_prompt_prefix_marker_with_no_command_keeps_input() {
        // When the line is just the marker (no command after it), the
        // function should fall back to the original input rather than
        // returning an empty string.
        let result = strip_prompt_prefix("❯ ");
        // The trimmed-after string is empty → returns the original `s`.
        assert_eq!(result, "❯ ");
    }

    #[test]
    fn strip_prompt_prefix_picks_last_marker_in_line() {
        // When the line contains multiple markers (e.g. a path with `$`),
        // the function picks the LAST one so the command after it is kept.
        // Here `$ ` appears in `a$ b` and again as the real prompt `$ ls`.
        assert_eq!(strip_prompt_prefix("echo a$ b $ ls"), "ls");
    }

    #[test]
    fn word_at_handles_multibyte_boundaries() {
        // word_at operates on chars, so multibyte positions are safe.
        // "héllo" — é is one char (two UTF-8 bytes).
        let line = "héllo";
        // Cursor at end (char index 5).
        assert_eq!(word_at(line, 5), Some((0, 5)));
        // Cursor at char index 2 (the 'l').
        assert_eq!(word_at(line, 2), Some((0, 2)));
    }

    // ── Keybindings tab: chord_label formatting ──────────────────────

    #[test]
    fn chord_label_cmd_plus_char() {
        use weft_core::input::{KeyCode, Modifiers};
        assert_eq!(chord_label(KeyCode::Char('c'), Modifiers::SUPER), "cmd+c");
    }

    #[test]
    fn chord_label_cmd_shift_t() {
        use weft_core::input::{KeyCode, Modifiers};
        assert_eq!(
            chord_label(KeyCode::Char('t'), Modifiers::SUPER | Modifiers::SHIFT),
            "cmd+shift+t"
        );
    }

    #[test]
    fn chord_label_shift_page_up() {
        use weft_core::input::{KeyCode, Modifiers};
        assert_eq!(
            chord_label(KeyCode::PageUp, Modifiers::SHIFT),
            "shift+page_up"
        );
    }

    #[test]
    fn chord_label_bare_enter() {
        use weft_core::input::{KeyCode, Modifiers};
        assert_eq!(chord_label(KeyCode::Enter, Modifiers::empty()), "enter");
    }

    #[test]
    fn chord_label_ctrl_a() {
        use weft_core::input::{KeyCode, Modifiers};
        assert_eq!(
            chord_label(KeyCode::Char('a'), Modifiers::CONTROL),
            "ctrl+a"
        );
    }

    #[test]
    fn chord_label_alt_plus_char() {
        use weft_core::input::{KeyCode, Modifiers};
        assert_eq!(chord_label(KeyCode::Char('x'), Modifiers::ALT), "alt+x");
    }

    // ── F7 context menu: hit-test geometry ───────────────────────────

    #[test]
    fn context_menu_items_count_matches_layout_const() {
        assert_eq!(
            crate::CONTEXT_MENU_ITEMS.len(),
            crate::layout::CONTEXT_MENU_ITEM_COUNT
        );
    }

    #[test]
    fn context_menu_actions_are_unique() {
        let actions: Vec<&str> = crate::CONTEXT_MENU_ITEMS.iter().map(|(_, a)| *a).collect();
        let mut sorted = actions.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), actions.len(), "duplicate action strings");
    }

    // ── config mtime hot-reload: external file change is picked up ──
    #[test]
    fn config_load_picks_up_external_theme_change() {
        let _env = ENV_LOCK.lock().unwrap();
        use weft_core::config::Config;
        let tmp = unique_temp_dir("weft-mtime-theme");
        let old_xdg = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", &tmp);

        let cfg_dir = tmp.join("weft");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let cfg_path = cfg_dir.join("config.toml");

        // Initial: theme = weft-warm.
        std::fs::write(&cfg_path, "[theme]\nname = \"weft-warm\"\n").unwrap();
        let first = Config::load();
        assert_eq!(first.theme.name, "weft-warm");

        // External edit: theme → weft-light (simulating user editing the file).
        std::fs::write(&cfg_path, "[theme]\nname = \"weft-light\"\n").unwrap();
        let second = Config::load();
        assert_eq!(second.theme.name, "weft-light");

        restore_xdg(old_xdg, &tmp);
    }

    #[test]
    fn config_load_picks_up_external_font_change() {
        let _env = ENV_LOCK.lock().unwrap();
        use weft_core::config::Config;
        let tmp = unique_temp_dir("weft-mtime-font");
        let old_xdg = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", &tmp);

        let cfg_dir = tmp.join("weft");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let cfg_path = cfg_dir.join("config.toml");

        std::fs::write(&cfg_path, "[font]\nsize = 12.0\n").unwrap();
        let first = Config::load();
        assert!((first.font.size - 12.0).abs() < 1e-6);

        std::fs::write(&cfg_path, "[font]\nsize = 16.0\n").unwrap();
        let second = Config::load();
        assert!((second.font.size - 16.0).abs() < 1e-6);

        restore_xdg(old_xdg, &tmp);
    }

    #[test]
    fn config_load_returns_default_when_file_deleted() {
        let _env = ENV_LOCK.lock().unwrap();
        use weft_core::config::Config;
        let tmp = unique_temp_dir("weft-mtime-del");
        let old_xdg = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", &tmp);

        let cfg_dir = tmp.join("weft");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let cfg_path = cfg_dir.join("config.toml");

        // File exists → load reads it.
        std::fs::write(&cfg_path, "[font]\nsize = 18.0\n").unwrap();
        let with_file = Config::load();
        assert!((with_file.font.size - 18.0).abs() < 1e-6);

        // File deleted → load falls back to defaults.
        std::fs::remove_file(&cfg_path).unwrap();
        let without_file = Config::load();
        assert!(
            (without_file.font.size - 14.0).abs() < 1e-6,
            "default font size"
        );

        restore_xdg(old_xdg, &tmp);
    }

    /// Helper: create a unique temp dir for an env-var-scoped test.
    fn unique_temp_dir(prefix: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let tmp = std::env::temp_dir().join(format!("{prefix}-{pid}-{id}"));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        tmp
    }

    /// Helper: restore XDG_CONFIG_HOME and clean up the temp dir.
    fn restore_xdg(old: Option<std::ffi::OsString>, tmp: &std::path::Path) {
        match old {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        let _ = std::fs::remove_dir_all(tmp);
    }
}

/// Reviewer MEDIUM-3 (T14): releases the prune re-entry guard even if the
/// prune thread panics mid-pass; without it a panic would wedge `weft-prune`
/// for the rest of the session (the flag is only cleared on the happy path).
pub(crate) struct PruneGuard<'a>(pub(crate) &'a AtomicBool);
impl Drop for PruneGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}
