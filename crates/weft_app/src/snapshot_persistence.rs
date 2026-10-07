use weft_core::persistence::TabSnapshot;

#[derive(Default)]
pub(crate) struct SnapshotPersistenceState {
    last_saved: Option<Vec<TabSnapshot>>,
}

impl SnapshotPersistenceState {
    pub(crate) fn should_save(&self, current: &[TabSnapshot]) -> bool {
        self.last_saved.as_deref() != Some(current)
    }

    pub(crate) fn record_saved(&mut self, snapshots: Vec<TabSnapshot>) {
        self.last_saved = Some(snapshots);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(cwd: &str, scroll: usize) -> TabSnapshot {
        TabSnapshot {
            position: 0,
            active: true,
            cwd: Some(cwd.to_string()),
            block_scroll_offset: scroll,
            editor_buffer: "{}".to_string(),
            shell_phase: "AtPrompt".to_string(),
            block_ids: Vec::new(),
            panes: None,
        }
    }

    #[test]
    fn complete_snapshot_comparison_catches_unmarked_cwd_and_scroll_mutations() {
        let mut state = SnapshotPersistenceState::default();
        state.record_saved(vec![snapshot("/old", 0)]);
        assert!(!state.should_save(&[snapshot("/old", 0)]));
        assert!(state.should_save(&[snapshot("/tmp", 0)]));
        assert!(state.should_save(&[snapshot("/old", 7)]));
    }
}
