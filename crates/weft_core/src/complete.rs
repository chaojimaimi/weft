//! Command completion — pure completion engine for the editor's `Tab` handling.
//!
//! Aggregates three sources (history, filesystem paths, `$PATH` executables),
//! dedups, and ranks by source priority. The app layer supplies a
//! [`CompleteCtx`] (cwd, history, cached PATH binaries); this module is pure
//! logic so it's fully unit-testable.

/// Where the cursor sits relative to the command — decides which completion
/// sources are consulted. At a command position we want history + executables;
/// at an argument position we only want files/directories (Warp-style: Tab
/// after `cd ` lists the cwd, never history).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletePosition {
    /// Line start or after a shell operator (`| & ; > <`): consult all three
    /// sources (history, path, `$PATH` executables).
    Command,
    /// After a command + whitespace: consult filesystem paths only. No history
    /// matches, no executable matches — you don't tab-complete a second
    /// command inside an argument.
    Argument,
}

/// Which source a completion [`Match`] came from. Drives its priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    History,
    Path,
    Command,
}

impl MatchKind {
    /// Lower = higher priority. History first, then Path, then Command.
    fn priority(self) -> u8 {
        match self {
            MatchKind::History => 0,
            MatchKind::Path => 1,
            MatchKind::Command => 2,
        }
    }
}

/// A single completion candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    /// What to show in the dropdown.
    pub label: String,
    pub kind: MatchKind,
    /// Text to replace the prefix with (may equal the label, or include a path
    /// dir-part / trailing `/` for directory completion).
    pub insert: String,
    /// True when this is a filesystem directory (Path kind only). Used by the
    /// renderer to draw a 📁/📄 icon in the Warp-style completion popup.
    pub is_dir: bool,
    /// Match-quality tier within the same source kind. Lower sorts first.
    /// Command candidates: `COMMON` (0, high-frequency commands) outrank
    /// `PREFIX` (1, ordinary prefix match) so `l` shows `ls` before
    /// `lam`/languagesetup. Non-command sources keep the default 1.
    pub match_quality: u8,
}

/// Quality tiers for command candidates (lower = higher priority).
pub const MATCH_QUALITY_COMMON: u8 = 0;
pub const MATCH_QUALITY_PREFIX: u8 = 1;

impl Match {
    fn new(label: String, kind: MatchKind, insert: String) -> Self {
        Self {
            label,
            kind,
            insert,
            is_dir: false,
            match_quality: MATCH_QUALITY_PREFIX,
        }
    }
}

/// Inputs the app gathers for completion: the cwd, the command history, and
/// the cached list of `$PATH` executables (empty when the cursor isn't at a
/// command position — saves scanning).
pub struct CompleteCtx<'a> {
    pub cwd: &'a str,
    pub history: &'a [String],
    pub path_bins: &'a [String],
}

/// Produce completion candidates for `prefix` given the context and cursor
/// position. At a [`CompletePosition::Command`] position, history matches rank
/// above Path, which ranks above Command. At an [`CompletePosition::Argument`]
/// position, only filesystem paths are consulted (no history, no executables).
/// Duplicates (same label) collapse to the higher-priority one. Capped at 50
/// results.
pub fn complete(prefix: &str, ctx: &CompleteCtx, position: CompletePosition) -> Vec<Match> {
    let mut out: Vec<Match> = Vec::new();

    // History and $PATH commands only make sense at a command position.
    // After `cd ` you want files, not a historical `cd /tmp`.
    if position == CompletePosition::Command {
        out.extend(history_matches(prefix, ctx.history));
        out.extend(command_matches(prefix, ctx.path_bins));
    }

    // Filesystem paths — always consulted (both command and argument positions).
    out.extend(path_matches(prefix, ctx.cwd));

    // Dedup by label, keeping the first (highest-priority) occurrence.
    let mut seen = std::collections::HashSet::new();
    out.retain(|m| seen.insert(m.label.clone()));

    // Rank by (kind priority, match quality, label), then cap.
    out.sort_by(|a, b| {
        a.kind
            .priority()
            .cmp(&b.kind.priority())
            .then_with(|| a.match_quality.cmp(&b.match_quality))
            .then_with(|| a.label.cmp(&b.label))
    });
    out.truncate(MAX_RESULTS);
    out
}

const MAX_RESULTS: usize = 50;

/// History prefix matches (whole-line). Excludes exact duplicates of prefix
/// (entries whose length equals the prefix). Public so the v1.7.2
/// `completion::providers` module can wrap the same logic in a
/// `CompletionProvider` without duplicating it.
pub fn history_matches(prefix: &str, history: &[String]) -> Vec<Match> {
    let mut out = Vec::new();
    for h in history {
        if h.starts_with(prefix) && h.len() > prefix.len() {
            out.push(Match::new(h.clone(), MatchKind::History, h.clone()));
        }
    }
    out
}

/// High-frequency commands that should surface before alphabetically-earlier
/// candidates when the prefix is short (e.g. `l` shows `ls` before
/// `lam`/languagesetup). Keep this list small and general-purpose —
/// it is a display-priority hint, not a filter.
pub const COMMON_COMMANDS: &[&str] = &[
    "brew", "cat", "cd", "chmod", "chown", "cp", "cargo", "curl", "docker", "find", "git", "grep",
    "htop", "kill", "less", "ls", "make", "mkdir", "mv", "nano", "node", "npm", "ollama", "ping",
    "pnpm", "python", "python3", "rm", "rsync", "scp", "sed", "ssh", "sudo", "tar", "top", "touch",
    "vim", "wget", "which",
];

/// `$PATH` executable prefix matches, weighted so `COMMON_COMMANDS`
/// outrank ordinary alphabetically-earlier candidates. Public so the v1.7.2
/// `completion::providers` module can wrap the same logic.
pub fn command_matches(prefix: &str, path_bins: &[String]) -> Vec<Match> {
    let mut out = Vec::new();
    for bin in path_bins {
        if bin.starts_with(prefix) && bin.len() > prefix.len() {
            let mut m = Match::new(bin.clone(), MatchKind::Command, bin.clone());
            if COMMON_COMMANDS.contains(&bin.as_str()) {
                m.match_quality = MATCH_QUALITY_COMMON;
            }
            out.push(m);
        }
    }
    out
}

/// Resolve a directory prefix (possibly `~`-relative, absolute, or relative to
/// `cwd`) to an absolute path for `read_dir`. Public so the v1.7.2
/// `completion::FilesystemProvider` can reuse the same resolution logic.
pub fn resolve_dir(dir_part: &str, cwd: &str) -> String {
    if let Some(rest) = dir_part.strip_prefix("~") {
        // ~/x or ~ (rest may start with '/')
        let home = std::env::var_os("HOME").map(|s| s.to_string_lossy().to_string());
        match home {
            Some(h) => format!("{h}{rest}"),
            None => format!(".{rest}"),
        }
    } else if dir_part.starts_with('/') {
        dir_part.to_string()
    } else {
        // relative (incl. empty): cwd + "/" + dir_part
        let trimmed = cwd.trim_end_matches('/');
        format!("{trimmed}/{dir_part}")
    }
}

/// Filesystem path completions for `prefix` within `cwd`. Public so the
/// v1.7.2 `completion::FilesystemProvider` can reuse the same logic.
pub fn path_matches(prefix: &str, cwd: &str) -> Vec<Match> {
    let (dir_part, file_prefix) = match prefix.rfind('/') {
        Some(i) => (&prefix[..=i], &prefix[i + 1..]),
        None => ("", prefix),
    };
    let resolved = resolve_dir(dir_part, cwd);
    let entries = match std::fs::read_dir(&resolved) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let show_hidden = file_prefix.starts_with('.');
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(file_prefix) {
            continue;
        }
        let is_hidden = name.starts_with('.');
        if is_hidden && !show_hidden {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let display = if is_dir {
            format!("{name}/")
        } else {
            name.clone()
        };
        let mut m = Match::new(
            display.clone(),
            MatchKind::Path,
            format!("{dir_part}{display}"),
        );
        m.is_dir = is_dir;
        out.push(m);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// A non-existent cwd so the path source returns nothing (history-only tests).
    const NO_CWD: &str = "/weft_complete_nonexistent";

    fn path_ctx<'a>(cwd: &'a str) -> CompleteCtx<'a> {
        CompleteCtx {
            cwd,
            history: &[],
            path_bins: &[],
        }
    }

    fn scratch_dir(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut d = std::env::temp_dir();
        d.push(format!(
            "weft_complete_{}_{name}_{}",
            std::process::id(),
            id
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn history_prefix_match() {
        let history = vec![
            "ls -la".to_string(),
            "ls /tmp".to_string(),
            "cd".to_string(),
        ];
        let ctx = CompleteCtx {
            cwd: NO_CWD,
            history: &history,
            path_bins: &[],
        };
        let ms = complete("ls", &ctx, CompletePosition::Command);
        let labels: Vec<&str> = ms.iter().map(|m| m.label.as_str()).collect();
        assert_eq!(labels, vec!["ls -la", "ls /tmp"]);
        assert!(ms.iter().all(|m| m.kind == MatchKind::History));
    }

    #[test]
    fn history_no_match_returns_empty_for_history() {
        let history = vec!["ls".to_string()];
        let ctx = CompleteCtx {
            cwd: NO_CWD,
            history: &history,
            path_bins: &[],
        };
        // exact prefix (len == prefix) is excluded; a non-matching prefix too.
        let ms: Vec<_> = complete("ls", &ctx, CompletePosition::Command)
            .into_iter()
            .filter(|m| m.kind == MatchKind::History)
            .collect();
        assert!(
            ms.is_empty(),
            "exact-prefix history match should be excluded"
        );
        assert!(complete("zzz", &ctx, CompletePosition::Command)
            .iter()
            .all(|m| m.kind != MatchKind::History));
    }

    #[test]
    fn path_completion_filters_by_prefix() {
        let d = scratch_dir("path_prefix");
        fs::write(d.join("a.txt"), "").unwrap();
        fs::write(d.join("b.txt"), "").unwrap();
        fs::create_dir(d.join("sub")).unwrap();
        let ctx = path_ctx(d.to_str().unwrap());
        let ms = complete("a", &ctx, CompletePosition::Argument);
        let labels: Vec<&str> = ms.iter().map(|m| m.label.as_str()).collect();
        assert_eq!(labels, vec!["a.txt"]);
        assert!(ms[0].kind == MatchKind::Path);
        assert_eq!(ms[0].insert, "a.txt");
    }

    #[test]
    fn path_completion_dir_gets_trailing_slash() {
        let d = scratch_dir("path_dir");
        fs::create_dir(d.join("sub")).unwrap();
        let ctx = path_ctx(d.to_str().unwrap());
        let ms = complete("s", &ctx, CompletePosition::Argument);
        assert_eq!(ms[0].label, "sub/");
        assert_eq!(ms[0].insert, "sub/");
    }

    #[test]
    fn path_completion_lists_all_on_empty_prefix() {
        let d = scratch_dir("path_all");
        fs::write(d.join("a"), "").unwrap();
        fs::write(d.join("b"), "").unwrap();
        let ctx = path_ctx(d.to_str().unwrap());
        let ms = complete("", &ctx, CompletePosition::Argument);
        let labels: Vec<&str> = ms.iter().map(|m| m.label.as_str()).collect();
        assert!(labels.contains(&"a"));
        assert!(labels.contains(&"b"));
    }

    #[test]
    fn path_completion_subdir_prefix() {
        let d = scratch_dir("path_subdir");
        fs::create_dir(d.join("sub")).unwrap();
        fs::write(d.join("sub").join("x.txt"), "").unwrap();
        let ctx = path_ctx(d.to_str().unwrap());
        let ms = complete("sub/x", &ctx, CompletePosition::Argument);
        assert_eq!(ms[0].label, "x.txt");
        assert_eq!(ms[0].insert, "sub/x.txt");
    }

    #[test]
    fn hidden_files_only_when_prefix_starts_with_dot() {
        let d = scratch_dir("path_hidden");
        fs::write(d.join(".secret"), "").unwrap();
        fs::write(d.join("visible"), "").unwrap();
        let ctx = path_ctx(d.to_str().unwrap());
        assert!(complete("", &ctx, CompletePosition::Argument)
            .iter()
            .all(|m| !m.label.starts_with('.')));
        assert!(complete(".", &ctx, CompletePosition::Argument)
            .iter()
            .any(|m| m.label == ".secret"));
    }

    #[test]
    fn dedup_keeping_higher_priority() {
        // history "foo" and a path file "foo" both produce label "foo" for
        // prefix "fo"; dedup keeps the History (higher priority) one.
        let d = scratch_dir("dedup");
        fs::write(d.join("foo"), "").unwrap();
        let history = vec!["foo".to_string()];
        let ctx = CompleteCtx {
            cwd: d.to_str().unwrap(),
            history: &history,
            path_bins: &[],
        };
        let ms = complete("fo", &ctx, CompletePosition::Command);
        let foo: Vec<_> = ms.iter().filter(|m| m.label == "foo").collect();
        assert_eq!(foo.len(), 1, "duplicate labels collapse to one");
        assert_eq!(foo[0].kind, MatchKind::History, "history outranks path");
    }

    #[test]
    fn command_prefix_match() {
        let path_bins = vec!["ls".to_string(), "cat".to_string(), "grep".to_string()];
        let ctx = CompleteCtx {
            cwd: NO_CWD,
            history: &[],
            path_bins: &path_bins,
        };
        let ms = complete("l", &ctx, CompletePosition::Command);
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].label, "ls");
        assert_eq!(ms[0].kind, MatchKind::Command);
    }

    #[test]
    fn priority_ordering_history_path_command() {
        // distinct labels from each source; ranked History > Path > Command.
        let d = scratch_dir("priority");
        fs::write(d.join("ab-path"), "").unwrap();
        let history = vec!["ab-history".to_string()];
        let path_bins = vec!["ab-cmd".to_string()];
        let ctx = CompleteCtx {
            cwd: d.to_str().unwrap(),
            history: &history,
            path_bins: &path_bins,
        };
        let ms = complete("ab", &ctx, CompletePosition::Command);
        let kinds: Vec<MatchKind> = ms.iter().map(|m| m.kind).collect();
        assert_eq!(
            kinds,
            vec![MatchKind::History, MatchKind::Path, MatchKind::Command],
            "order must be History > Path > Command"
        );
    }

    // ── Bug fix tests: argument position ────────────────────────────────

    #[test]
    fn argument_position_excludes_history_and_commands() {
        // Bug 2: `cd ` + Tab should show only files/dirs, never history or
        // executables. Even if history has `cd /tmp` and path_bins has `cd`,
        // an argument-position completion with prefix "" must yield only Path
        // entries from the cwd.
        let d = scratch_dir("arg_only");
        fs::write(d.join("file_a"), "").unwrap();
        fs::create_dir(d.join("dir_b")).unwrap();

        let history = vec!["cd /tmp".to_string(), "cd ..".to_string()];
        let path_bins = vec!["cd".to_string(), "cat".to_string()];
        let ctx = CompleteCtx {
            cwd: d.to_str().unwrap(),
            history: &history,
            path_bins: &path_bins,
        };

        let ms = complete("", &ctx, CompletePosition::Argument);
        // No history, no commands — only Path.
        assert!(
            ms.iter().all(|m| m.kind == MatchKind::Path),
            "argument position must exclude history and commands"
        );
        let labels: Vec<&str> = ms.iter().map(|m| m.label.as_str()).collect();
        assert!(labels.contains(&"file_a"));
        assert!(labels.contains(&"dir_b/"));
    }

    #[test]
    fn argument_position_with_prefix_still_excludes_history() {
        // `cd s` + Tab → should match only files starting with "s", never a
        // history entry like `ssh ...`.
        let d = scratch_dir("arg_prefix");
        fs::write(d.join("src.rs"), "").unwrap();
        let history = vec!["ssh user@host".to_string()];
        let ctx = CompleteCtx {
            cwd: d.to_str().unwrap(),
            history: &history,
            path_bins: &[],
        };

        let ms = complete("s", &ctx, CompletePosition::Argument);
        assert!(ms.iter().all(|m| m.kind == MatchKind::Path));
        assert_eq!(ms[0].label, "src.rs");
    }

    #[test]
    fn command_position_still_includes_all_sources() {
        // Regression: Command position should behave as before (all sources).
        let d = scratch_dir("cmd_all");
        fs::write(d.join("ab-file"), "").unwrap();
        let history = vec!["ab-hist".to_string()];
        let path_bins = vec!["ab-bin".to_string()];
        let ctx = CompleteCtx {
            cwd: d.to_str().unwrap(),
            history: &history,
            path_bins: &path_bins,
        };

        let ms = complete("ab", &ctx, CompletePosition::Command);
        let kinds: Vec<MatchKind> = ms.iter().map(|m| m.kind).collect();
        assert!(kinds.contains(&MatchKind::History));
        assert!(kinds.contains(&MatchKind::Path));
        assert!(kinds.contains(&MatchKind::Command));
    }
}
