//! Pure UI helper functions extracted from renderer.rs (M4 step 2).
//!
//! These are stateless utilities for panel/block display: prompt stripping,
//! command-name matching, path abbreviation, duration formatting, and string
//! truncation. None depend on `MetalRenderer` state.

use weft_core::blocks::Block;

/// Whether a block matches the panel search query (empty query = match all).
/// Shared by the renderer (list layout) and the app (selection clamping).
///
/// v0.9 fix: only match the command line, NOT the output. Matching output
/// caused false positives (e.g. searching "ls" matched `git pull` whose output
/// contained "ls"; searching "wha" matched `claude`/`git status` whose output
/// contained "wha"). Warp's history search only filters by command line.
///
/// v0.9 fix: match against the prompt-stripped command, not the raw grid
/// snapshot. `block.command` may include the full prompt line (e.g.
/// `user@host weft git:(main) % ls`) when the command was captured via
/// `snapshot_command_line` rather than the editor. Searching "git" would
/// match every command run inside a git repo. Stripping the prompt first
/// ensures only the actual command text is searched.
///
/// v0.9 fix (round 5): match only the command name (first token), via
/// case-insensitive **substring** (fuzzy) match.
///
/// Earlier iterations tried prefix/separator matching across all tokens, but
/// that conflated the command with its arguments. The cleanest semantic — and
/// the one matching Warp's history search — is: the query is a substring of
/// the command *name* (the first whitespace-separated token after stripping
/// the prompt).
///
/// Examples (query → command):
///   - "git"  → "git status"      ✅ (command name "git" contains "git")
///   - "git"  → "gitconfig"        ✅ (command name "gitconfig" contains "git")
///   - "git"  → "cd GitHub/"       ❌ (command name "cd" doesn't contain "git")
///   - "ls"   → "ls -al /test"     ✅ (command name "ls" contains "ls")
///   - "l"    → "ls -al /test"     ✅ (command name "ls" contains "l")
///   - "al"   → "ls -al /test"     ❌ ("al" is in the argument, not "ls")
///
/// Arguments are intentionally excluded: otherwise "git" would match
/// `cd GitHub/` (lowercased "github/" contains "git") — exactly the false
/// positive we're trying to avoid.
pub fn block_matches_query(block: &Block, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let q = query.to_lowercase();
    let cleaned = strip_prompt_prefix(&block.command);
    // Only the command name (first token) participates in matching.
    match cleaned.split_whitespace().next() {
        Some(cmd_name) => cmd_name.to_lowercase().contains(&q),
        None => false,
    }
}

/// F3-4: Total count of blocks matching the panel query (newest-first, no
/// truncation). Used to clamp `scroll_offset` so the list can't scroll past
/// the last filtered block.
pub(crate) fn panel_filtered_count(blocks: &[Block], query: &str) -> usize {
    blocks
        .iter()
        .rev()
        .filter(|b| block_matches_query(b, query))
        .count()
}

/// F3-4: Clamp a candidate panel scroll offset to `[0, max(0, total -
/// visible)]`. Pure so the mouse handler, controller, and tests share one
/// source of truth.
pub fn clamp_panel_scroll(total: usize, visible: usize, offset: usize) -> usize {
    offset.min(total.saturating_sub(visible))
}

/// Newest-first, query-filtered block list with `scroll_offset` blocks
/// skipped, capped to `max` entries. Shared by the warm-up pass and
/// `build_panel_vertices` so they render the same set.
pub(crate) fn panel_display<'a>(
    blocks: &'a [Block],
    query: &str,
    scroll_offset: usize,
    max: usize,
) -> Vec<&'a Block> {
    blocks
        .iter()
        .rev()
        .filter(|b| block_matches_query(b, query))
        .skip(scroll_offset)
        .take(max)
        .collect()
}

/// How many block rows fit below the title (in whole grid rows).
pub(crate) fn visible_panel_rows(viewport_h: f32, cell_h: u32) -> usize {
    if cell_h == 0 {
        return 0;
    }
    ((viewport_h / cell_h as f32) as usize).saturating_sub(2)
}

/// Abbreviate an absolute path for display: replace a `$HOME` prefix with `~`
/// (e.g. `/Users/andylee/proj` → `~/proj`). Falls back to the raw path when
/// `$HOME` is unset or isn't a prefix.
pub(crate) fn abbreviate_path(path: &str) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        if let Some(h) = home.to_str() {
            if !h.is_empty() && path.starts_with(h) {
                return format!("~{}", &path[h.len()..]);
            }
        }
    }
    path.to_string()
}

/// Human-readable elapsed time for a finished block.
pub(crate) fn block_duration_str(b: &Block) -> String {
    let Some(finished) = b.finished_at else {
        return String::new();
    };
    let ms = finished
        .duration_since(b.started_at)
        .unwrap_or_default()
        .as_millis();
    if ms < 1000 {
        format!("{}ms", ms)
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m", ms / 60_000)
    }
}

/// Truncate `s` to `max` chars, appending an ellipsis if it was cut.
pub(crate) fn truncate_str(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
    t.push('…');
    t
}

/// Truncate with the same terminal-column model used by the text renderer.
pub(crate) fn truncate_to_columns(s: &str, max_cols: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;

    if max_cols == 0 {
        return String::new();
    }
    if weft_core::grid::terminal_text_width(s) <= max_cols {
        return s.to_string();
    }
    let ellipsis_width = weft_core::grid::terminal_char_width('…');
    if ellipsis_width > max_cols {
        return String::new();
    }
    let budget = max_cols - ellipsis_width;
    let mut used = 0;
    let mut output = String::new();
    for grapheme in s.graphemes(true) {
        let width = weft_core::grid::terminal_text_width(grapheme);
        if used + width > budget {
            break;
        }
        output.push_str(grapheme);
        used += width;
    }
    output.push('…');
    output
}

/// v0.9 fix: strip a shell prompt prefix from a captured command line.
///
/// `snapshot_command_line` grabs the whole prompt row when the command wasn't
/// submitted via the editor (e.g. before shell integration is ready, or loaded
/// from an old DB). This produces strings like `andylee@AndyHQ weft % echo hi`
/// instead of just `echo hi`. We only strip when the command contains `@`
/// (a `user@host` prompt signature) — this avoids false positives on commands
/// like `echo 50% done` or `echo $HOME`. When the `@` is found, we take the
/// last `% `/`$ `/`# ` occurrence as the prompt→command boundary.
pub(crate) fn strip_prompt_prefix(command: &str) -> String {
    let trimmed = command.trim_start();
    // Only attempt stripping when a user@host prompt signature is present.
    if !trimmed.contains('@') {
        return trimmed.to_string();
    }
    let prompts = ["% ", "$ ", "# "];
    let mut best: Option<usize> = None;
    for p in &prompts {
        let mut start = 0;
        while let Some(idx) = trimmed[start..].find(p) {
            let abs = start + idx;
            best = Some(abs + p.len());
            start = abs + p.len();
        }
    }
    match best {
        Some(idx) => trimmed[idx..].trim().to_string(),
        None => trimmed.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_column_truncation_keeps_cjk_inside_budget() {
        let width = weft_core::grid::terminal_text_width;

        let text = truncate_to_columns("项目目录·运行命令", 8);
        assert_eq!(text, "项目目…");
        assert!(width(&text) <= 8);
        assert_eq!(truncate_to_columns("abcdef", 4), "abc…");
        assert!(width(&truncate_to_columns("①·…", 4)) <= 4);
        assert_eq!(truncate_to_columns("👩‍🔬abc", 3), "👩‍🔬…");
    }
    use weft_core::blocks::BlockId;

    #[test]
    fn strip_prompt_prefix_zsh() {
        assert_eq!(
            strip_prompt_prefix("andylee@AndyHQ weft % echo first"),
            "echo first"
        );
    }

    #[test]
    fn strip_prompt_prefix_bash() {
        assert_eq!(
            strip_prompt_prefix("andylee@host:~/proj $ echo hi"),
            "echo hi"
        );
    }

    #[test]
    fn strip_prompt_prefix_root() {
        assert_eq!(strip_prompt_prefix("root@host:~# ls -la"), "ls -la");
    }

    #[test]
    fn strip_prompt_prefix_already_clean() {
        // Clean commands (no prompt) pass through unchanged.
        assert_eq!(strip_prompt_prefix("echo first"), "echo first");
        assert_eq!(strip_prompt_prefix("ls -la /tmp"), "ls -la /tmp");
    }

    #[test]
    fn strip_prompt_prefix_no_at_sign_passes_through() {
        // Commands without @ (no user@host prompt) pass through unchanged,
        // even if they contain % or $ characters.
        assert_eq!(strip_prompt_prefix("echo 50% done"), "echo 50% done");
        assert_eq!(strip_prompt_prefix("# comment line"), "# comment line");
    }

    // ── block_matches_query (v0.9 round 5: command-name substring match) ──
    fn mk_block(cmd: &str) -> Block {
        Block {
            id: BlockId(1),
            command: cmd.to_string(),
            cwd: None,
            output: String::new().into(),
            styled_output: None,
            exit_code: None,
            started_at: std::time::SystemTime::UNIX_EPOCH,
            finished_at: None,
            collapsed: false,
        }
    }

    #[test]
    fn panel_search_git_matches_git_commands() {
        // "git" matches the command name "git" (exact or as substring).
        assert!(block_matches_query(&mk_block("git status"), "git"));
        assert!(block_matches_query(
            &mk_block("git push origin main"),
            "git"
        ));
        assert!(block_matches_query(&mk_block("git"), "git"));
        // Case-insensitive.
        assert!(block_matches_query(&mk_block("GIT STATUS"), "git"));
        assert!(block_matches_query(&mk_block("Git Status"), "git"));
    }

    #[test]
    fn panel_search_git_matches_fuzzy_substring() {
        // v0.9 round 5: fuzzy substring match on the command name.
        // "git" matches "gitconfig" because the command name contains "git".
        assert!(block_matches_query(&mk_block("gitconfig"), "git"));
        assert!(block_matches_query(
            &mk_block("gitconfig --global user.name"),
            "git"
        ));
        // Partial substring: "gi" matches "git status".
        assert!(block_matches_query(&mk_block("git status"), "gi"));
        // "it" matches "git status" (substring of command name "git").
        assert!(block_matches_query(&mk_block("git status"), "it"));
    }

    #[test]
    fn panel_search_git_rejects_argument_only_match() {
        // v0.9 round 5: arguments don't participate in matching.
        // "git" must NOT match "cd GitHub/" — command name is "cd", which
        // doesn't contain "git"; even though the argument "GitHub/" contains
        // "git" after lowercasing, arguments are excluded from matching.
        assert!(!block_matches_query(&mk_block("cd GitHub/"), "git"));
        assert!(!block_matches_query(&mk_block("cd github/"), "git"));
        // "git" is not in the command name "cd".
        assert!(!block_matches_query(&mk_block("cd github"), "git"));
    }

    #[test]
    fn panel_search_ls_rejects_argument_substring() {
        // v0.9 round 5: "al" is in the argument "-al", not in the command
        // name "ls" — must NOT match.
        assert!(!block_matches_query(&mk_block("ls -al /test"), "al"));
        // But "ls" matches the command name, and "l" matches as a substring.
        assert!(block_matches_query(&mk_block("ls -al /test"), "ls"));
        assert!(block_matches_query(&mk_block("ls -al /test"), "l"));
        assert!(block_matches_query(&mk_block("ls"), "ls"));
    }

    #[test]
    fn panel_search_skills_rejects_when_command_is_cd() {
        // Regression: "ls" must NOT match "cd andrej-karpathy-skills" —
        // command name is "cd", arguments are excluded from matching.
        assert!(!block_matches_query(
            &mk_block("cd andrej-karpathy-skills"),
            "ls"
        ));
    }

    #[test]
    fn panel_search_cd_matches_cd_command() {
        // "cd" matches the command name "cd" even when the argument contains
        // a coincidental substring.
        assert!(block_matches_query(&mk_block("cd GitHub/"), "cd"));
        assert!(block_matches_query(&mk_block("cd .."), "cd"));
    }

    #[test]
    fn panel_search_empty_query_matches_all() {
        assert!(block_matches_query(&mk_block("git status"), ""));
        assert!(block_matches_query(&mk_block("ls -l"), ""));
    }

    #[test]
    fn panel_search_strips_prompt_prefix() {
        // The query runs against the prompt-stripped command, so a user@host
        // prompt prefix doesn't leak into matching.
        let b = mk_block("andylee@AndyHQ weft % git status");
        assert!(block_matches_query(&b, "git"));
        // "weft" is part of the prompt (cwd), not the command — must not match
        // the stripped command "git status".
        assert!(!block_matches_query(&b, "weft"));
    }

    // ── F3-4: virtualization helpers ───────────────────────────────────

    #[test]
    fn panel_filtered_count_counts_all_matching_blocks() {
        let blocks = vec![
            Block {
                id: BlockId(1),
                command: "git status".into(),
                cwd: None,
                output: String::new().into(),
                styled_output: None,
                exit_code: Some(0),
                started_at: std::time::SystemTime::UNIX_EPOCH,
                finished_at: None,
                collapsed: false,
            },
            Block {
                id: BlockId(2),
                command: "ls".into(),
                cwd: None,
                output: String::new().into(),
                styled_output: None,
                exit_code: Some(0),
                started_at: std::time::SystemTime::UNIX_EPOCH,
                finished_at: None,
                collapsed: false,
            },
            Block {
                id: BlockId(3),
                command: "git push".into(),
                cwd: None,
                output: String::new().into(),
                styled_output: None,
                exit_code: Some(0),
                started_at: std::time::SystemTime::UNIX_EPOCH,
                finished_at: None,
                collapsed: false,
            },
        ];
        // Empty query matches all.
        assert_eq!(panel_filtered_count(&blocks, ""), 3);
        // "git" matches two blocks (git status, git push).
        assert_eq!(panel_filtered_count(&blocks, "git"), 2);
        // No matches.
        assert_eq!(panel_filtered_count(&blocks, "cargo"), 0);
    }

    #[test]
    fn panel_display_applies_scroll_offset() {
        let blocks: Vec<Block> = (1..=5)
            .map(|i| Block {
                id: BlockId(i),
                command: format!("cmd{}", i),
                cwd: None,
                output: String::new().into(),
                styled_output: None,
                exit_code: Some(0),
                started_at: std::time::SystemTime::UNIX_EPOCH,
                finished_at: None,
                collapsed: false,
            })
            .collect();
        // No scroll: newest first → [cmd5, cmd4, cmd3, cmd2, cmd1].
        let d = panel_display(&blocks, "", 0, 10);
        assert_eq!(d.len(), 5);
        assert_eq!(d[0].command, "cmd5");
        // Skip 2 from newest: [cmd3, cmd2, cmd1].
        let d = panel_display(&blocks, "", 2, 10);
        assert_eq!(d.len(), 3);
        assert_eq!(d[0].command, "cmd3");
        // Skip past the end → empty.
        let d = panel_display(&blocks, "", 10, 10);
        assert!(d.is_empty());
    }

    #[test]
    fn clamp_panel_scroll_enforces_bounds() {
        // offset within range → unchanged.
        assert_eq!(clamp_panel_scroll(10, 5, 3), 3);
        // offset exceeds max_scroll → clamped to total - visible.
        assert_eq!(clamp_panel_scroll(10, 5, 8), 5);
        // total < visible → max_scroll = 0, so offset = 0.
        assert_eq!(clamp_panel_scroll(3, 5, 2), 0);
        // offset = 0 → 0 (newest visible).
        assert_eq!(clamp_panel_scroll(10, 5, 0), 0);
    }

    /// CI performance gate for F3 history virtualization. Kept ignored in the
    /// normal suite because wall-clock assertions should run in isolation.
    #[test]
    #[ignore]
    fn perf_panel_10k_history_filter_and_virtualize() {
        let blocks: Vec<Block> = (0..10_000)
            .map(|i| Block {
                id: BlockId(i),
                command: if i % 2 == 0 {
                    format!("git status {i}")
                } else {
                    format!("cargo test {i}")
                },
                cwd: None,
                output: String::new().into(),
                styled_output: None,
                exit_code: Some(0),
                started_at: std::time::SystemTime::UNIX_EPOCH,
                finished_at: None,
                collapsed: false,
            })
            .collect();

        let started = std::time::Instant::now();
        assert_eq!(panel_filtered_count(&blocks, "git"), 5_000);
        for offset in [0, 100, 2_500, 4_920] {
            let visible = panel_display(&blocks, "git", offset, 80);
            assert_eq!(visible.len(), 80);
        }
        let elapsed = started.elapsed();
        println!("10k history filter + virtualize: {elapsed:?} (budget <100ms)");

        assert!(
            elapsed < std::time::Duration::from_millis(100),
            "10k history filtering/virtualization took {elapsed:?}, budget <100ms"
        );
    }

    /// v1.10 scale gate: locating an unfiltered visible window must remain
    /// interactive at 10k/50k/100k blocks. Construction is excluded.
    #[test]
    #[ignore]
    fn perf_panel_v110_scale_visible_window() {
        for size in [10_000usize, 50_000, 100_000] {
            let blocks: Vec<Block> = (0..size)
                .map(|i| Block {
                    id: BlockId(i as u64),
                    command: format!("command-{i}"),
                    cwd: None,
                    output: String::new().into(),
                    styled_output: None,
                    exit_code: Some(0),
                    started_at: std::time::SystemTime::UNIX_EPOCH,
                    finished_at: None,
                    collapsed: false,
                })
                .collect();
            let mut samples = Vec::with_capacity(20);
            for sample in 0..20 {
                let offset = sample * size.saturating_sub(80) / 19;
                let started = std::time::Instant::now();
                let visible = panel_display(&blocks, "", offset, 80);
                assert_eq!(visible.len(), 80);
                samples.push(started.elapsed().as_secs_f64() * 1000.0);
            }
            samples.sort_by(f64::total_cmp);
            let median = samples[samples.len() / 2];
            let p95 = samples[(samples.len() * 95 / 100).min(samples.len() - 1)];
            println!(
                "V110_METRIC name=block_visible_window size={size} median_ms={median:.3} p95_ms={p95:.3}"
            );
            assert!(
                p95 < 8.0,
                "{size} block visible-window p95 {p95:.3}ms exceeds 8ms budget"
            );
        }
    }
}
