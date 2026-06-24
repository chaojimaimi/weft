//! Command completion — pure completion engine for the editor's `Tab` handling.
//!
//! Aggregates three sources (history, filesystem paths, `$PATH` executables),
//! dedups, and ranks by source priority. The app layer supplies a
//! [`CompleteCtx`] (cwd, history, cached PATH binaries); this module is pure
//! logic so it's fully unit-testable.

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
}

impl Match {
    fn new(label: String, kind: MatchKind, insert: String) -> Self {
        Self { label, kind, insert }
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

/// Produce completion candidates for `prefix` given the context. History
/// matches rank above Path, which ranks above Command; duplicates (same label)
/// collapse to the higher-priority one. Capped at 50 results.
pub fn complete(prefix: &str, ctx: &CompleteCtx) -> Vec<Match> {
    let mut out: Vec<Match> = Vec::new();

    // History: whole-line prefix matches (excluding exact duplicates of prefix).
    for h in ctx.history {
        if h.starts_with(prefix) && h.len() > prefix.len() {
            out.push(Match::new(h.clone(), MatchKind::History, h.clone()));
        }
    }

    // Filesystem paths.
    out.extend(path_matches(prefix, ctx.cwd));

    // (Command source added in Task 7.)

    // Dedup by label, keeping the first (highest-priority) occurrence.
    let mut seen = std::collections::HashSet::new();
    out.retain(|m| seen.insert(m.label.clone()));

    // Rank by (kind priority, label), then cap.
    out.sort_by(|a, b| {
        a.kind
            .priority()
            .cmp(&b.kind.priority())
            .then_with(|| a.label.cmp(&b.label))
    });
    out.truncate(MAX_RESULTS);
    out
}

const MAX_RESULTS: usize = 50;

/// Resolve a directory prefix (possibly `~`-relative, absolute, or relative to
/// `cwd`) to an absolute path for `read_dir`.
fn resolve_dir(dir_part: &str, cwd: &str) -> String {
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

/// Filesystem path completions for `prefix` within `cwd`.
fn path_matches(prefix: &str, cwd: &str) -> Vec<Match> {
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
        out.push(Match::new(
            display.clone(),
            MatchKind::Path,
            format!("{dir_part}{display}"),
        ));
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
        let mut d = std::env::temp_dir();
        d.push(format!("weft_complete_{}_{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn history_prefix_match() {
        let history = vec!["ls -la".to_string(), "ls /tmp".to_string(), "cd".to_string()];
        let ctx = CompleteCtx { cwd: NO_CWD, history: &history, path_bins: &[] };
        let ms = complete("ls", &ctx);
        let labels: Vec<&str> = ms.iter().map(|m| m.label.as_str()).collect();
        assert_eq!(labels, vec!["ls -la", "ls /tmp"]);
        assert!(ms.iter().all(|m| m.kind == MatchKind::History));
    }

    #[test]
    fn history_no_match_returns_empty_for_history() {
        let history = vec!["ls".to_string()];
        let ctx = CompleteCtx { cwd: NO_CWD, history: &history, path_bins: &[] };
        // exact prefix (len == prefix) is excluded; a non-matching prefix too.
        let ms: Vec<_> = complete("ls", &ctx)
            .into_iter()
            .filter(|m| m.kind == MatchKind::History)
            .collect();
        assert!(ms.is_empty(), "exact-prefix history match should be excluded");
        assert!(complete("zzz", &ctx).iter().all(|m| m.kind != MatchKind::History));
    }

    #[test]
    fn path_completion_filters_by_prefix() {
        let d = scratch_dir("path_prefix");
        fs::write(d.join("a.txt"), "").unwrap();
        fs::write(d.join("b.txt"), "").unwrap();
        fs::create_dir(d.join("sub")).unwrap();
        let ctx = path_ctx(d.to_str().unwrap());
        let ms = complete("a", &ctx);
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
        let ms = complete("s", &ctx);
        assert_eq!(ms[0].label, "sub/");
        assert_eq!(ms[0].insert, "sub/");
    }

    #[test]
    fn path_completion_lists_all_on_empty_prefix() {
        let d = scratch_dir("path_all");
        fs::write(d.join("a"), "").unwrap();
        fs::write(d.join("b"), "").unwrap();
        let ctx = path_ctx(d.to_str().unwrap());
        let ms = complete("", &ctx);
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
        let ms = complete("sub/x", &ctx);
        assert_eq!(ms[0].label, "x.txt");
        assert_eq!(ms[0].insert, "sub/x.txt");
    }

    #[test]
    fn hidden_files_only_when_prefix_starts_with_dot() {
        let d = scratch_dir("path_hidden");
        fs::write(d.join(".secret"), "").unwrap();
        fs::write(d.join("visible"), "").unwrap();
        let ctx = path_ctx(d.to_str().unwrap());
        assert!(complete("", &ctx).iter().all(|m| !m.label.starts_with('.')));
        assert!(complete(".", &ctx).iter().any(|m| m.label == ".secret"));
    }

    #[test]
    fn dedup_keeping_higher_priority() {
        // history "foo" and a path file "foo" both produce label "foo" for
        // prefix "fo"; dedup keeps the History (higher priority) one.
        let d = scratch_dir("dedup");
        fs::write(d.join("foo"), "").unwrap();
        let history = vec!["foo".to_string()];
        let ctx = CompleteCtx { cwd: d.to_str().unwrap(), history: &history, path_bins: &[] };
        let ms = complete("fo", &ctx);
        let foo: Vec<_> = ms.iter().filter(|m| m.label == "foo").collect();
        assert_eq!(foo.len(), 1, "duplicate labels collapse to one");
        assert_eq!(foo[0].kind, MatchKind::History, "history outranks path");
    }
}
