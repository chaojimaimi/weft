//! v1.7.2: Pure sort and dedupe functions for completion candidates.
//!
//! V17_IMPLEMENTATION_PLAN §4: "排序与去重为纯函数". These functions are
//! pure — no I/O, no side effects, fully testable.

use super::CompletionCandidate;

/// Maximum number of completion results to return. Matches the cap in
/// `crate::complete::MAX_RESULTS`.
const MAX_RESULTS: usize = 50;

/// Sort candidates by (source priority, match quality, label). Lower
/// priority value = higher rank.
/// PathExecutable > Workflow > WorkspaceCommand > History > Filesystem;
/// within a source, COMMON-command candidates (match_quality 0) precede
/// ordinary prefix matches (1), then label order.
pub fn sort_candidates(candidates: &mut [CompletionCandidate]) {
    candidates.sort_by(|a, b| {
        a.source
            .priority()
            .cmp(&b.source.priority())
            .then_with(|| a.match_quality.cmp(&b.match_quality))
            .then_with(|| a.label.cmp(&b.label))
    });
}

/// Remove duplicate candidates (same label), keeping the highest-priority one.
/// Input should be sorted by `sort_candidates` first for deterministic results.
pub fn dedupe_candidates(candidates: Vec<CompletionCandidate>) -> Vec<CompletionCandidate> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(candidates.len());
    for c in candidates {
        if seen.insert(c.label.clone()) {
            out.push(c);
        }
    }
    out.truncate(MAX_RESULTS);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::CompletionSource;

    fn candidate(label: &str, source: CompletionSource) -> CompletionCandidate {
        CompletionCandidate {
            label: label.to_string(),
            insert: label.to_string(),
            source,
            is_dir: false,
            match_quality: 1,
        }
    }

    fn candidate_quality(
        label: &str,
        source: CompletionSource,
        match_quality: u8,
    ) -> CompletionCandidate {
        CompletionCandidate {
            label: label.to_string(),
            insert: label.to_string(),
            source,
            is_dir: false,
            match_quality,
        }
    }

    #[test]
    fn sort_by_source_priority_then_label() {
        let mut candidates = vec![
            candidate("zzz", CompletionSource::History),
            candidate("aaa", CompletionSource::Filesystem),
            candidate("mmm", CompletionSource::History),
        ];
        sort_candidates(&mut candidates);
        assert_eq!(candidates[0].label, "mmm"); // History, m < z
        assert_eq!(candidates[1].label, "zzz"); // History
        assert_eq!(candidates[2].label, "aaa"); // Filesystem
    }

    #[test]
    fn dedupe_keeps_first_occurrence() {
        let candidates = vec![
            candidate("ls", CompletionSource::History),
            candidate("ls", CompletionSource::Filesystem),
            candidate("cat", CompletionSource::History),
        ];
        let result = dedupe_candidates(candidates);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].label, "ls");
        assert_eq!(result[0].source, CompletionSource::History); // kept the first (higher priority)
        assert_eq!(result[1].label, "cat");
    }

    #[test]
    fn dedupe_caps_at_50_results() {
        let candidates: Vec<_> = (0..100)
            .map(|i| candidate(&format!("cmd{i}"), CompletionSource::History))
            .collect();
        let result = dedupe_candidates(candidates);
        assert_eq!(result.len(), 50);
    }

    #[test]
    fn empty_input_returns_empty() {
        let result = dedupe_candidates(Vec::new());
        assert!(result.is_empty());
    }

    #[test]
    fn sort_preserves_already_sorted() {
        let mut candidates = vec![
            candidate("aaa", CompletionSource::History),
            candidate("bbb", CompletionSource::History),
        ];
        sort_candidates(&mut candidates);
        assert_eq!(candidates[0].label, "aaa");
        assert_eq!(candidates[1].label, "bbb");
    }

    #[test]
    fn dedupe_with_no_duplicates_returns_all() {
        let candidates = vec![
            candidate("ls", CompletionSource::History),
            candidate("cat", CompletionSource::Filesystem),
        ];
        let result = dedupe_candidates(candidates);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn quoting_and_space_paths_sort_correctly() {
        let mut candidates = vec![
            candidate("my file.txt", CompletionSource::Filesystem),
            candidate("my file", CompletionSource::Filesystem),
        ];
        sort_candidates(&mut candidates);
        // Shorter label sorts first
        assert_eq!(candidates[0].label, "my file");
        assert_eq!(candidates[1].label, "my file.txt");
    }

    #[test]
    fn command_outranks_history_and_files() {
        // 2.2: PATH executable beats history beats filesystem — a real command
        // must surface first even when its label sorts after others.
        let mut candidates = vec![
            candidate("aaa", CompletionSource::PathExecutable),
            candidate("mmm", CompletionSource::History),
            candidate("zzz", CompletionSource::Filesystem),
        ];
        sort_candidates(&mut candidates);
        let labels: Vec<_> = candidates.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, vec!["aaa", "mmm", "zzz"]);
        assert_eq!(candidates[0].source, CompletionSource::PathExecutable);
        assert_eq!(candidates[1].source, CompletionSource::History);
        assert_eq!(candidates[2].source, CompletionSource::Filesystem);
    }

    #[test]
    fn dedupe_keeps_command_over_history() {
        // 2.2 regression guard: after sort, the PathExecutable entry precedes
        // the History entry, so dedupe keeps the command (label "less" no
        // longer gets tagged History).
        let mut candidates = vec![
            candidate("less", CompletionSource::History),
            candidate("less", CompletionSource::PathExecutable),
        ];
        sort_candidates(&mut candidates);
        let result = dedupe_candidates(candidates);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].source, CompletionSource::PathExecutable);
    }

    #[test]
    fn common_command_outranks_alphabetically_earlier_prefix() {
        // B3: within PathExecutable, COMMON-command quality 0 beats ordinary
        // quality 1 even when its label sorts later ("ls" before "lam").
        let mut candidates = vec![
            candidate("lam", CompletionSource::PathExecutable),
            candidate_quality("ls", CompletionSource::PathExecutable, 0),
            candidate("last", CompletionSource::PathExecutable),
        ];
        sort_candidates(&mut candidates);
        let labels: Vec<_> = candidates.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, vec!["ls", "lam", "last"]);
    }

    #[test]
    fn quality_is_secondary_to_source_priority() {
        // Source priority dominates: a PathExecutable candidate (priority 0)
        // outranks a History candidate (priority 3) regardless of quality —
        // COMMON quality on a low-priority source must not leapfrog it.
        let mut candidates = vec![
            candidate_quality("zzz", CompletionSource::PathExecutable, 0),
            candidate("mmm", CompletionSource::History),
        ];
        sort_candidates(&mut candidates);
        let labels: Vec<_> = candidates.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, vec!["zzz", "mmm"]);
        assert_eq!(candidates[0].source, CompletionSource::PathExecutable);
    }
}
