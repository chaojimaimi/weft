//! Inline tests for `paint/ui_helpers.rs`, extracted to keep the
//! production file under the 800-line architecture gate (repo
//! convention: `pty.rs` -> `pty/tests.rs`).

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

// ── v1.10.36: abbreviate_path — bare-`~` disambiguation + prefix boundary ──
// Deterministic: inject HOME via `abbreviate_path_with` instead of reading
// the environment, so no branch silently skips in a HOME-less CI.

#[test]
fn abbreviate_path_without_home_returns_path_verbatim() {
    assert_eq!(abbreviate_path_with("/Users/me", None), "/Users/me");
    assert_eq!(abbreviate_path_with("relative/path", None), "relative/path");
}

#[test]
fn abbreviate_path_root_home_never_abbreviates() {
    // The pre-v1.10.36 starts_with check turned "/opt" into "~opt".
    assert_eq!(abbreviate_path_with("/opt", Some("/")), "/opt");
}

#[test]
fn abbreviate_path_trailing_slash_home_still_abbreviates() {
    assert_eq!(
        abbreviate_path_with("/Users/me/proj", Some("/Users/me/")),
        "~/proj"
    );
    assert_eq!(
        abbreviate_path_with("/Users/me", Some("/Users/me/")),
        "/Users/me"
    );
}

#[test]
fn abbreviate_path_keeps_exact_home_absolute() {
    assert_eq!(
        abbreviate_path_with("/Users/me", Some("/Users/me")),
        "/Users/me",
        "a bare `~` is indistinguishable from the 'cwd unknown' header placeholder"
    );
}

#[test]
fn abbreviate_path_abbreviates_home_subdirectory() {
    assert_eq!(
        abbreviate_path_with("/Users/me/proj", Some("/Users/me")),
        "~/proj"
    );
    assert_eq!(
        abbreviate_path_with("/Users/me/proj/deep", Some("/Users/me")),
        "~/proj/deep"
    );
}

#[test]
fn abbreviate_path_leaves_home_sibling_unchanged() {
    assert_eq!(
        abbreviate_path_with("/Users/mex", Some("/Users/me")),
        "/Users/mex",
        "a directory whose name merely starts with $HOME must not be abbreviated"
    );
}

#[test]
fn abbreviate_path_leaves_unrelated_paths_unchanged() {
    assert_eq!(
        abbreviate_path_with("/opt/weft-unrelated-dir", Some("/Users/me")),
        "/opt/weft-unrelated-dir"
    );
}

// ── live_elapsed_label (P1: running-command timer, whole-second) ──
// Boundary table with a fixed clock — no wall time in the assertions.
#[test]
fn live_elapsed_label_boundary_table() {
    let epoch = std::time::SystemTime::UNIX_EPOCH;
    let at = |secs: u64| epoch + std::time::Duration::from_secs(secs);
    assert_eq!(live_elapsed_label(epoch, at(0)), "0s");
    // 999ms truncates to the whole second: "0s", not "0.9s".
    assert_eq!(
        live_elapsed_label(epoch, epoch + std::time::Duration::from_millis(999)),
        "0s"
    );
    assert_eq!(live_elapsed_label(epoch, at(1)), "1s");
    assert_eq!(live_elapsed_label(epoch, at(59)), "59s");
    assert_eq!(live_elapsed_label(epoch, at(60)), "1m00s");
    assert_eq!(live_elapsed_label(epoch, at(61)), "1m01s");
    assert_eq!(live_elapsed_label(epoch, at(3599)), "59m59s");
    assert_eq!(live_elapsed_label(epoch, at(3600)), "1h00m");
    assert_eq!(live_elapsed_label(epoch, at(3661)), "1h01m");
    // Clock set backwards while running → saturate at "0s" (no panic).
    assert_eq!(live_elapsed_label(at(10), epoch), "0s");
}

// ── live_context_label (P1: cwd/branch/elapsed single header line) ──
#[test]
fn live_context_label_without_elapsed_keeps_legacy_shape() {
    assert_eq!(
        live_context_label(Some("/Users/me/.hermes"), Some("main"), None),
        Some("/Users/me/.hermes git:(main)".into())
    );
    assert_eq!(
        live_context_label(Some("/Users/me/.hermes"), None, None),
        Some("/Users/me/.hermes".into())
    );
}

#[test]
fn live_context_label_appends_elapsed_after_separator() {
    assert_eq!(
        live_context_label(Some("/tmp"), None, Some("42s")),
        Some("/tmp · 42s".into())
    );
}

#[test]
fn live_context_label_branch_plus_elapsed() {
    assert_eq!(
        live_context_label(Some("/tmp"), Some("main"), Some("1m01s")),
        Some("/tmp git:(main) · 1m01s".into())
    );
}

#[test]
fn live_context_label_without_cwd_is_none_even_with_elapsed() {
    // No cwd → no header line at all; elapsed never renders standalone.
    assert_eq!(live_context_label(None, Some("main"), Some("42s")), None);
    assert_eq!(live_context_label(None, None, Some("42s")), None);
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
        screen_origin: false,
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
            screen_origin: false,
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
            screen_origin: false,
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
            screen_origin: false,
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
            screen_origin: false,
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

// ── v1.13.18 (P3): list-space selection helpers ─────────────────────

#[test]
fn clamp_list_selection_pins_selection_to_list_length() {
    // Within the list → unchanged.
    assert_eq!(clamp_panel_list_selection(3, 10), 3);
    // Past the end → the last row.
    assert_eq!(clamp_panel_list_selection(9, 5), 4);
    // Empty list → 0 (saturating, never 0-1).
    assert_eq!(clamp_panel_list_selection(7, 0), 0);
    assert_eq!(clamp_panel_list_selection(0, 0), 0);
}

#[test]
fn keep_visible_offset_pulls_window_up_when_selection_above() {
    // Selection moved above the window → window starts at the selection.
    assert_eq!(panel_keep_visible_offset(2, 5, 10), 2);
    assert_eq!(panel_keep_visible_offset(0, 3, 10), 0);
}

#[test]
fn keep_visible_offset_pins_window_end_when_selection_below() {
    // Selection moved below the window → window ends at the selection
    // (offset = selection − visible + 1).
    assert_eq!(panel_keep_visible_offset(15, 5, 10), 6);
    assert_eq!(panel_keep_visible_offset(10, 0, 10), 1);
}

#[test]
fn keep_visible_offset_leaves_selection_inside_window() {
    assert_eq!(panel_keep_visible_offset(7, 5, 10), 5);
    // Selection exactly on the last visible row stays.
    assert_eq!(panel_keep_visible_offset(14, 5, 10), 5);
    // Selection exactly on the first visible row stays.
    assert_eq!(panel_keep_visible_offset(5, 5, 10), 5);
}

#[test]
fn keep_visible_offset_tolerates_list_shorter_than_window() {
    // List shorter than the window: a top selection keeps offset 0.
    assert_eq!(panel_keep_visible_offset(0, 0, 10), 0);
    assert_eq!(panel_keep_visible_offset(3, 0, 10), 0);
    // Degenerate zero-height window: the math never underflows — an
    // above-window selection anchors to itself, a below-window one pins
    // just past it (the caller's clamp_panel_scroll bounds the result).
    assert_eq!(panel_keep_visible_offset(4, 9, 0), 4);
    assert_eq!(panel_keep_visible_offset(4, 0, 0), 5);
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
            screen_origin: false,
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
                screen_origin: false,
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
