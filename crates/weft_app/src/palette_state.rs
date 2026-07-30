//! Command-palette state and its lifecycle invariants.

use std::time::Instant;

use weft_core::search::SearchHit;
use weft_core::workflow::{Workflow, WorkflowStore};

/// A single entry in the palette results list.
#[derive(Clone)]
pub(crate) enum PaletteEntry {
    Workflow(Workflow),
    Builtin(BuiltinCmd),
    /// v1.5.1: A config profile entry. `name` is the profile name (or the
    /// special `"Base"` sentinel for "switch to base"). `active` marks the
    /// currently-active profile so the palette can render a checkmark and
    /// so `activate_palette_entry` can no-op on re-selection.
    Profile {
        name: String,
        active: bool,
    },
    /// v1.7.1: A search hit from the FTS5 index (history, workflow,
    /// workspace, or bookmark). Activating a history hit fills the editor
    /// with the command — no auto-execution.
    SearchHit(SearchHit),
    Runbook(weft_core::runbook::RunbookEntry),
}

impl PaletteEntry {
    pub(crate) fn accessibility_key(&self) -> String {
        match self {
            Self::Workflow(workflow) => format!("workflow/{}", workflow.id),
            Self::Builtin(command) => format!("builtin/{}", command.accessibility_key()),
            // v1.5.1: profile accessibility identity is `profile/<name>` so
            // the active-profile marker doesn't change the identity (the
            // same profile stays the same element whether or not it's
            // active). The "Base" sentinel uses `profile/Base` (the
            // reserved name "base" is rejected by validate_profile_name
            // case-insensitively, so it never collides with a real profile).
            Self::Profile { name, .. } => format!("profile/{name}"),
            // v1.7.1: Search hits use `search/<kind>/<stable_id>` so
            // results from different kinds never collide.
            Self::SearchHit(hit) => {
                format!("search/{}/{}", hit.doc.kind as u8, hit.doc.stable_id)
            }
            Self::Runbook(entry) => format!("runbook/{}", entry.command),
        }
    }
}

/// Built-in commands that appear in the palette.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BuiltinCmd {
    ToggleTheme,
    SelectTheme,
    ToggleBlockPanel,
    ReloadConfig,
    /// v1.5.2: Show NSOpenPanel to pick a `.toml` config file and atomically
    /// replace the current config (with backup). Wired in
    /// `palette_controller::activate_palette_entry`.
    ImportConfig,
    /// v1.5.2: Show NSSavePanel to export the source config document to a
    /// `.toml` file (preserving comments + unknown fields when the source
    /// file exists).
    ExportConfig,
    /// v1.6.2: Show NSSavePanel to save the current session (tabs, panes,
    /// CWDs, drafts) as a workspace YAML file.
    SaveWorkspace,
    /// v1.6.2: Show NSOpenPanel to load a workspace YAML file and restore
    /// the session tree. PTY processes are NOT restored — each leaf spawns
    /// a fresh shell in the saved cwd.
    OpenWorkspace,
    ImportRunbook,
}

impl BuiltinCmd {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::ToggleTheme => "Toggle Theme",
            Self::SelectTheme => "Select Theme",
            Self::ToggleBlockPanel => "Toggle History Panel",
            Self::ReloadConfig => "Reload Config",
            Self::ImportConfig => "Import Config",
            Self::ExportConfig => "Export Config",
            Self::SaveWorkspace => "Save Workspace",
            Self::OpenWorkspace => "Open Workspace",
            Self::ImportRunbook => "Open Runbook",
        }
    }

    fn accessibility_key(self) -> &'static str {
        match self {
            Self::ToggleTheme => "toggle-theme",
            Self::SelectTheme => "select-theme",
            Self::ToggleBlockPanel => "toggle-history-panel",
            Self::ReloadConfig => "reload-config",
            Self::ImportConfig => "import-config",
            Self::ExportConfig => "export-config",
            Self::SaveWorkspace => "save-workspace",
            Self::OpenWorkspace => "open-workspace",
            Self::ImportRunbook => "import-runbook",
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

impl WorkflowForm {
    pub(crate) fn draw_fields(&self) -> Vec<(String, String, bool)> {
        self.var_names
            .iter()
            .enumerate()
            .map(|(index, name)| {
                (
                    name.clone(),
                    self.var_values.get(index).cloned().unwrap_or_default(),
                    index == self.current_field,
                )
            })
            .collect()
    }
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
    /// v1.7.1: Background search worker. None when persistence is disabled.
    pub(crate) search_worker: Option<crate::palette_search_worker::PaletteSearchWorker>,
    /// v1.7.1: Generation of the last submitted search query.
    pub(crate) search_generation: u64,
    /// v1.7.1: True when a search query is in-flight (for "searching..." indicator).
    pub(crate) search_pending: bool,
    pub(crate) runbook_entries: Vec<weft_core::runbook::RunbookEntry>,
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
            search_worker: None,
            search_generation: 0,
            search_pending: false,
            runbook_entries: Vec::new(),
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

    pub(crate) fn accessibility_query(&self) -> &str {
        match &self.submode {
            PaletteSubMode::Search => &self.query,
            PaletteSubMode::CreateWorkflow { buffer, .. }
            | PaletteSubMode::EditWorkflow { buffer, .. }
            | PaletteSubMode::SelectTheme { buffer, .. } => buffer,
            PaletteSubMode::ConfirmDelete { .. } => "",
        }
    }

    pub(crate) fn theme_picker_contains(&self, name: &str) -> bool {
        match &self.submode {
            PaletteSubMode::SelectTheme { buffer, themes } => themes.iter().any(|theme| {
                theme == name
                    && (buffer.is_empty() || theme.to_lowercase().contains(&buffer.to_lowercase()))
            }),
            _ => false,
        }
    }

    fn reset_search(&mut self) {
        self.query.clear();
        self.selection = 0;
        self.last_click = None;
        self.results.clear();
        self.form = None;
        self.submode = PaletteSubMode::Search;
        self.runbook_entries.clear();
    }
}

pub(crate) fn theme_name_is_dark(name: &str) -> bool {
    !matches!(name, "weft-light" | "solarized-light" | "gruvbox-light")
}

#[cfg(test)]
mod tests {
    use super::{PaletteEntry, PaletteState, WorkflowForm};

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

    #[test]
    fn workflow_form_draw_fields_align_names_values_and_focus() {
        let form = WorkflowForm {
            workflow_id: 1,
            workflow_name: "deploy".into(),
            workflow_description: String::new(),
            var_names: vec!["host".into(), "port".into()],
            var_values: vec!["example.com".into()],
            current_field: 1,
        };
        assert_eq!(
            form.draw_fields(),
            vec![
                ("host".into(), "example.com".into(), false),
                ("port".into(), String::new(), true),
            ]
        );
    }

    #[test]
    fn palette_accessibility_keys_do_not_depend_on_result_index_or_label() {
        let workflow = weft_core::workflow::Workflow {
            id: 7,
            name: "Reload Config".into(),
            description: String::new(),
            steps: Vec::new(),
            variables: Vec::new(),
            source: Default::default(),
            use_count: 0,
            last_used_ms: 0,
        };
        let workflow = PaletteEntry::Workflow(workflow);
        let builtin = PaletteEntry::Builtin(super::BuiltinCmd::ReloadConfig);
        assert_eq!(workflow.accessibility_key(), "workflow/7");
        assert_eq!(builtin.accessibility_key(), "builtin/reload-config");
        assert_ne!(workflow.accessibility_key(), builtin.accessibility_key());
    }

    #[test]
    fn accessibility_query_uses_theme_picker_buffer_in_that_submode() {
        let mut state = PaletteState::new();
        state.query = "normal".into();
        assert_eq!(state.accessibility_query(), "normal");
        state.submode = super::PaletteSubMode::SelectTheme {
            buffer: "light".into(),
            themes: vec!["weft-light".into()],
        };
        assert_eq!(state.accessibility_query(), "light");
        assert!(state.theme_picker_contains("weft-light"));
        assert!(!state.theme_picker_contains("weft-warm"));
        assert!(!state.theme_picker_contains("missing-light"));

        state.submode = super::PaletteSubMode::CreateWorkflow {
            step: super::CreateStep::Name,
            buffer: "deploy".into(),
            name: String::new(),
            command: String::new(),
        };
        assert_eq!(state.accessibility_query(), "deploy");
        state.submode = super::PaletteSubMode::EditWorkflow {
            id: 7,
            name: "deploy".into(),
            buffer: "edited command".into(),
        };
        assert_eq!(state.accessibility_query(), "edited command");
        state.submode = super::PaletteSubMode::ConfirmDelete {
            id: 7,
            name: "deploy".into(),
        };
        assert_eq!(state.accessibility_query(), "");
    }

    #[test]
    fn theme_dark_classification_matches_picker_apply_contract() {
        assert!(!super::theme_name_is_dark("weft-light"));
        assert!(!super::theme_name_is_dark("solarized-light"));
        assert!(!super::theme_name_is_dark("gruvbox-light"));
        assert!(super::theme_name_is_dark("weft-warm"));
        assert!(super::theme_name_is_dark("nord"));
    }

    // ── v1.5.1: PaletteEntry::Profile tests ──────────────────────────

    #[test]
    fn profile_entry_accessibility_key_uses_profile_prefix() {
        // The accessibility identity is `profile/<name>` so the same
        // profile stays the same element whether or not it's active.
        let active = PaletteEntry::Profile {
            name: "work".into(),
            active: true,
        };
        let inactive = PaletteEntry::Profile {
            name: "work".into(),
            active: false,
        };
        assert_eq!(active.accessibility_key(), "profile/work");
        assert_eq!(inactive.accessibility_key(), "profile/work");
        // Active flag must NOT change the identity — same profile, same key.
        assert_eq!(active.accessibility_key(), inactive.accessibility_key());
    }

    #[test]
    fn profile_entry_base_uses_reserved_name() {
        // The "Base" sentinel must produce `profile/base`, which never
        // collides with a real profile (validate_profile_name rejects "base").
        let base = PaletteEntry::Profile {
            name: "Base".into(),
            active: true,
        };
        assert_eq!(base.accessibility_key(), "profile/Base");
    }

    #[test]
    fn profile_entry_distinct_names_have_distinct_keys() {
        let a = PaletteEntry::Profile {
            name: "work".into(),
            active: false,
        };
        let b = PaletteEntry::Profile {
            name: "personal".into(),
            active: false,
        };
        assert_ne!(a.accessibility_key(), b.accessibility_key());
    }

    // ── v1.7.1: PaletteEntry::SearchHit tests ───────────────────────

    #[test]
    fn search_hit_accessibility_key_uses_kind_and_stable_id() {
        let hit = weft_core::search::SearchHit {
            doc: weft_core::search::SearchDocument {
                kind: weft_core::search::SearchDocumentKind::Block,
                stable_id: "42".to_string(),
                title: "cargo build".to_string(),
                body: String::new(),
                cwd: None,
                updated_at: 0,
            },
            score: 1.0,
        };
        let entry = PaletteEntry::SearchHit(hit);
        assert_eq!(entry.accessibility_key(), "search/0/42");
    }

    #[test]
    fn search_hit_distinct_kinds_have_distinct_keys() {
        let make = |kind, id: &str| {
            PaletteEntry::SearchHit(weft_core::search::SearchHit {
                doc: weft_core::search::SearchDocument {
                    kind,
                    stable_id: id.to_string(),
                    title: String::new(),
                    body: String::new(),
                    cwd: None,
                    updated_at: 0,
                },
                score: 0.0,
            })
        };
        let block = make(weft_core::search::SearchDocumentKind::Block, "1");
        let workflow = make(weft_core::search::SearchDocumentKind::Workflow, "1");
        assert_ne!(block.accessibility_key(), workflow.accessibility_key());
    }
}
