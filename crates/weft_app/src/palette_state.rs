//! Command-palette state and its lifecycle invariants.

use std::time::Instant;

use weft_core::workflow::{Workflow, WorkflowStore};

/// A single entry in the palette results list.
#[derive(Clone)]
pub(crate) enum PaletteEntry {
    Workflow(Workflow),
    Builtin(BuiltinCmd),
}

/// Built-in commands that appear in the palette.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BuiltinCmd {
    ToggleTheme,
    SelectTheme,
    ToggleBlockPanel,
    ReloadConfig,
}

impl BuiltinCmd {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::ToggleTheme => "Toggle Theme",
            Self::SelectTheme => "Select Theme",
            Self::ToggleBlockPanel => "Toggle History Panel",
            Self::ReloadConfig => "Reload Config",
        }
    }
}

/// Active variable-fill form for a selected workflow.
#[allow(dead_code)]
pub(crate) struct WorkflowForm {
    pub(crate) workflow_id: i64,
    pub(crate) workflow_name: String,
    pub(crate) workflow_description: String,
    pub(crate) var_names: Vec<String>,
    pub(crate) var_values: Vec<String>,
    pub(crate) current_field: usize,
}

/// Sub-modes of the palette beyond normal search.
pub(crate) enum PaletteSubMode {
    Search,
    CreateWorkflow {
        step: CreateStep,
        buffer: String,
        name: String,
        command: String,
    },
    EditWorkflow {
        id: i64,
        name: String,
        buffer: String,
    },
    ConfirmDelete {
        id: i64,
        name: String,
    },
    SelectTheme {
        buffer: String,
        themes: Vec<String>,
    },
}

#[derive(PartialEq, Eq)]
pub(crate) enum CreateStep {
    Name,
    Command,
    Done,
}

pub(crate) struct PaletteState {
    pub(crate) open: bool,
    pub(crate) query: String,
    pub(crate) selection: usize,
    pub(crate) last_click: Option<(Instant, usize)>,
    pub(crate) results: Vec<PaletteEntry>,
    pub(crate) form: Option<WorkflowForm>,
    pub(crate) submode: PaletteSubMode,
    pub(crate) store: Option<WorkflowStore>,
}

impl PaletteState {
    pub(crate) fn new() -> Self {
        Self {
            open: false,
            query: String::new(),
            selection: 0,
            last_click: None,
            results: Vec::new(),
            form: None,
            submode: PaletteSubMode::Search,
            store: None,
        }
    }

    pub(crate) fn open_search(&mut self) {
        self.open = true;
        self.reset_search();
    }

    pub(crate) fn close(&mut self) {
        self.open = false;
        self.reset_search();
    }

    fn reset_search(&mut self) {
        self.query.clear();
        self.selection = 0;
        self.last_click = None;
        self.results.clear();
        self.form = None;
        self.submode = PaletteSubMode::Search;
    }
}

#[cfg(test)]
mod tests {
    use super::{PaletteEntry, PaletteState};

    #[test]
    fn open_search_starts_from_clean_transient_state() {
        let mut state = PaletteState::new();
        state.query = "stale".into();
        state.selection = 4;
        state
            .results
            .push(PaletteEntry::Builtin(super::BuiltinCmd::ToggleTheme));
        state.open_search();
        assert!(state.open);
        assert!(state.query.is_empty());
        assert_eq!(state.selection, 0);
        assert!(state.results.is_empty());
    }

    #[test]
    fn close_clears_palette_transients() {
        let mut state = PaletteState::new();
        state.open_search();
        state.query = "theme".into();
        state.close();
        assert!(!state.open);
        assert!(state.query.is_empty());
        assert!(state.form.is_none());
    }
}
