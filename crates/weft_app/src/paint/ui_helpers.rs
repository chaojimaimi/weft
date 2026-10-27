//! Pure UI helper functions extracted from renderer.rs (M4 step 2).
//!
//! These are stateless utilities for panel/block display: prompt stripping,
//! command-name matching, path abbreviation, duration formatting, and string
//! truncation. None depend on `MetalRenderer` state.

use std::cell::Cell;
use std::time::SystemTime;

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
    block_matches_query_lowered(block, &query.to_lowercase())
}

/// v1.15.3 P2 (PLAN_v1153 §4): single-block core of
/// [`block_matches_query`] with the query already lowercased — the list
/// scan paths hoist the `to_lowercase` allocation out of the per-block
/// loop (n allocations → 1). Signature-identical semantics.
fn block_matches_query_lowered(block: &Block, lowered_query: &str) -> bool {
    let cleaned = strip_prompt_prefix(&block.command);
    // Only the command name (first token) participates in matching.
    match cleaned.split_whitespace().next() {
        Some(cmd_name) => cmd_name.to_lowercase().contains(lowered_query),
        None => false,
    }
}

/// F3-4: Total count of blocks matching the panel query (newest-first, no
/// truncation). Used to clamp `scroll_offset` so the list can't scroll past
/// the last filtered block.
///
/// v1.15.3 P2: production reads go through [`PanelFilteredCache`] (which
/// rebuilds via [`panel_filtered_indices`]); this pure form stays as the
/// unit-test + perf-gate oracle (`perf_panel_10k_history_filter_and_
/// virtualize` measures it directly).
#[cfg_attr(not(test), allow(dead_code))]
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

/// v1.13.18 P3: clamp the panel's list-space selection to the filtered list
/// length (0 = newest). An empty list collapses every selection to 0. Pure
/// so the controller and tests share one source of truth.
pub(crate) fn clamp_panel_list_selection(selection: usize, total: usize) -> usize {
    selection.min(total.saturating_sub(1))
}

/// v1.13.18 P3: keep-visible scroll-offset math for the panel's list-space
/// selection. Returns the window start (`scroll_offset`) that keeps
/// `selection` inside the visible window `[offset, offset + visible)`:
/// the selection above the window pulls the window up to itself, a
/// selection below the window pins the window's end to itself, and a
/// selection already inside leaves the offset unchanged. Pure — unit-tested
/// against the four plan boundaries; the caller clamps via
/// [`clamp_panel_scroll`] afterwards.
pub(crate) fn panel_keep_visible_offset(
    selection: usize,
    scroll_offset: usize,
    visible: usize,
) -> usize {
    if selection < scroll_offset {
        selection
    } else if selection >= scroll_offset.saturating_add(visible) {
        selection.saturating_add(1).saturating_sub(visible)
    } else {
        scroll_offset
    }
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

/// v1.15.3 P2: newest-first indices of the blocks matching `query` — the
/// one-scan rebuild behind [`PanelFilteredCache`].
pub(crate) fn panel_filtered_indices(blocks: &[Block], query: &str) -> Vec<usize> {
    if query.is_empty() {
        return (0..blocks.len()).rev().collect();
    }
    let lowered = query.to_lowercase();
    blocks
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, b)| block_matches_query_lowered(b, &lowered))
        .map(|(i, _)| i)
        .collect()
}

/// Virtualized window over precomputed filtered indices — the cached twin
/// of [`panel_display`] (identical output when fed
/// [`panel_filtered_indices`] output for the same query).
pub(crate) fn panel_display_from_indices<'a>(
    blocks: &'a [Block],
    indices: &[usize],
    scroll_offset: usize,
    max: usize,
) -> Vec<&'a Block> {
    indices
        .iter()
        .skip(scroll_offset)
        .take(max)
        .map(|&i| &blocks[i])
        .collect()
}

/// Cache key for [`PanelFilteredCache`]: block count + newest block id +
/// raw query. Finalized blocks are immutable (the tracker's `command` never
/// changes after `133;C`), and retention eviction changes `len`, so any of
/// the three changing means the index list is stale.
type PanelCacheKey = (usize, Option<u64>, String);

/// v1.15.3 P2 (PLAN_v1153 §4): newest-first filtered-index cache for the
/// history panel. `build_panel_vertices` runs the O(n) query filter twice
/// per frame (count + display) — with hundreds of blocks that is dead work
/// on every frame the panel stays open. The cache rebuilds only when
/// `(blocks.len(), last_block_id, query)` changes; `PanelState::close()`
/// clears it so the index vector never outlives the panel surface.
///
/// Not `Copy`/`Clone` on purpose — mutation goes through
/// [`get_or_rebuild`](Self::get_or_rebuild) so the rebuild counter stays
/// truthful for the invalidation tests.
#[derive(Debug, Default)]
pub(crate) struct PanelFilteredCache {
    key: Option<PanelCacheKey>,
    indices: Vec<usize>,
    /// Test observability: number of full O(n) rebuilds performed.
    rebuilds: Cell<u64>,
}

impl PanelFilteredCache {
    /// Cached newest-first filtered indices; rebuilds on key mismatch.
    /// The caller wraps this in a `RefCell` borrow (frame-cache precedent:
    /// `cached_panel_scroll_metrics`).
    pub(crate) fn get_or_rebuild(&mut self, blocks: &[Block], query: &str) -> &[usize] {
        let key: PanelCacheKey = (
            blocks.len(),
            blocks.last().map(|b| b.id.0),
            query.to_string(),
        );
        if self.key.as_ref() != Some(&key) {
            self.indices = panel_filtered_indices(blocks, query);
            self.key = Some(key);
            self.rebuilds.set(self.rebuilds.get() + 1);
        }
        &self.indices
    }

    /// Drops the cached indices (panel closed — memory must not linger).
    pub(crate) fn clear(&mut self) {
        self.key = None;
        self.indices = Vec::new();
    }

    /// Rebuild count so far (invalidation-test oracle).
    #[cfg(test)]
    pub(crate) fn rebuild_count(&self) -> u64 {
        self.rebuilds.get()
    }
}

// v1.13.18 P4: `visible_panel_rows`（`(vp_h/ch) − 2` 近似）已删除——面板
// 可见行预算的唯一事实源是 `layout::panel_visible_row_budget`（从
// PanelLayout 几何推导，含 footer 带让位），五处消费方全部改调。

/// Abbreviate an absolute path for display: replace a `$HOME` prefix with `~`
/// (e.g. `/Users/andylee/proj` → `~/proj`). Falls back to the raw path when
/// `$HOME` is unset or isn't a prefix.
///
/// v1.10.36: `$HOME` itself keeps its absolute form — a bare `~` is
/// indistinguishable from the block header's "cwd unknown" placeholder.
/// The prefix match also requires a `/` boundary, so a sibling directory
/// whose name merely starts with `$HOME` (e.g. `/Users/andyleex`) is not
/// mis-abbreviated to `~x`.
pub(crate) fn abbreviate_path(path: &str) -> String {
    let home = std::env::var_os("HOME");
    abbreviate_path_with(path, home.as_deref().and_then(|h| h.to_str()))
}

/// Pure core of [`abbreviate_path`] with an injectable `$HOME`, so tests can
/// drive the boundary branches (`None`, `/`, trailing slash) deterministically
/// instead of skipping when the environment lacks `HOME`.
fn abbreviate_path_with(path: &str, home: Option<&str>) -> String {
    let Some(h) = home else {
        return path.to_string();
    };
    // A trailing slash in $HOME (abnormal but seen in the wild) would leave
    // `rest` without its leading `/` and defeat every abbreviation below;
    // trimming also makes HOME=`/` a no-op instead of the old `~opt` mangling.
    let h = h.trim_end_matches('/');
    if h.is_empty() {
        return path.to_string();
    }
    let Some(rest) = path.strip_prefix(h) else {
        return path.to_string();
    };
    if rest.is_empty() {
        // A bare `~` is indistinguishable from the "cwd unknown" header
        // placeholder, so $HOME itself keeps its absolute form.
        return path.to_string();
    }
    if rest.starts_with('/') {
        return format!("~{rest}");
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

/// Whole-second elapsed label for a RUNNING command's live header (P1):
/// `<1s → "0s"`, `<60s → "{s}s"`, `<1h → "{m}m{ss}s"`, else `"{h}h{mm}m"`.
/// Truncates to whole seconds (a half-run command never shows "0m59s" as
/// "59.5s"); a backwards-set clock (negative elapsed) saturates at `"0s"`
/// instead of panicking on `Duration` underflow.
pub(crate) fn live_elapsed_label(started_at: SystemTime, now: SystemTime) -> String {
    let secs = now.duration_since(started_at).unwrap_or_default().as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// The live block's single header line: abbreviated cwd (+ git branch when
/// present), with the running elapsed label appended after a `·` separator.
/// `cwd: None` → `None` — the header line only exists when there is a cwd,
/// so the elapsed label never renders standalone (P1).
pub(crate) fn live_context_label(
    cwd: Option<&str>,
    git_branch: Option<&str>,
    elapsed: Option<&str>,
) -> Option<String> {
    let cwd = cwd.map(abbreviate_path).filter(|cwd| !cwd.is_empty())?;
    let mut label = match git_branch {
        Some(branch) if !branch.is_empty() => format!("{cwd} git:({branch})"),
        _ => cwd,
    };
    if let Some(elapsed) = elapsed {
        label.push_str(" · ");
        label.push_str(elapsed);
    }
    Some(label)
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

// Tests live in the gate-exempt sibling module (repo test-module
// convention, pty/tests.rs precedent) so inline test lines stay out of
// the production-file budget.
#[cfg(test)]
#[path = "ui_helpers/tests.rs"]
mod tests;
