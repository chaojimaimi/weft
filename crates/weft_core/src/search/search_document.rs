//! v1.7.1: Unified search document model.
//!
//! `SearchDocument` is the common currency type for the unified local search
//! system (V17_IMPLEMENTATION_PLAN §3). All searchable entities — blocks,
//! workflows, workspaces, bookmarks — are mapped into this shape before
//! indexing. The index itself lives in [`search_index::SearchIndex`].
//!
//! Design constraints (V17 §3 "固定架构"):
//! - `stable_id` is a string (not integer) so heterogeneous ID spaces
//!   (block u64, workflow i64, workspace string) share one type.
//! - `updated_at` is milliseconds since Unix epoch (matches `BlockStore`'s
//!   `started_ms`/`finished_ms` convention).
//! - The document is `Send + Sync` so it can cross thread boundaries into
//!   the search worker.

use std::time::{SystemTime, UNIX_EPOCH};

/// v1.7.1: The kind of searchable entity a [`SearchDocument`] represents.
/// Stored as the FTS5 `kind` column for type-filtered queries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[repr(u8)]
pub enum SearchDocumentKind {
    /// A terminal command block (history entry).
    Block = 0,
    /// A saved workflow (variable-filled command template).
    Workflow = 1,
    /// A workspace (saved split + tab layout).
    Workspace = 2,
    /// A user-created bookmark/note on a block (v1.7.3).
    Bookmark = 3,
}

impl SearchDocumentKind {
    /// String label for display in the palette. Matches the `kind_label`
    /// convention used by `PaletteEntryView`.
    pub fn label(self) -> &'static str {
        match self {
            SearchDocumentKind::Block => "History",
            SearchDocumentKind::Workflow => "Workflow",
            SearchDocumentKind::Workspace => "Workspace",
            SearchDocumentKind::Bookmark => "Bookmark",
        }
    }

    /// Parse from a stored integer. Returns `None` for unknown values so
    /// future additions don't corrupt the index.
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Block),
            1 => Some(Self::Workflow),
            2 => Some(Self::Workspace),
            3 => Some(Self::Bookmark),
            _ => None,
        }
    }
}

/// v1.7.1: A unified search document. One document per searchable entity.
///
/// Fields mirror V17 §3: `kind`, `stable_id`, `title`, `body`, `cwd`,
/// `updated_at`. The FTS5 index stores `title` and `body` as searchable
/// text; the other fields are unindexed columns used for display and
/// ranking.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchDocument {
    /// Entity type (Block/Workflow/Workspace/Bookmark).
    pub kind: SearchDocumentKind,
    /// Stable identifier within the kind's ID space. For blocks this is
    /// the `BlockId` as a decimal string; for workflows the SQLite rowid.
    pub stable_id: String,
    /// Short title — for blocks this is the command line; for workflows
    /// the workflow name. Displayed as the primary label in the palette.
    pub title: String,
    /// Full searchable body — for blocks this is the output text; for
    /// workflows the command template. May be empty for entities without
    /// body content (e.g. workspace).
    pub body: String,
    /// Working directory context. Used for CWD-aware ranking (V17 §3:
    /// "排序函数只使用 exact/prefix、CWD、使用频率和 recency").
    pub cwd: Option<String>,
    /// Milliseconds since Unix epoch. Used for recency ranking.
    pub updated_at: i64,
}

impl SearchDocument {
    /// Construct from a block's fields. `title` = command, `body` = output
    /// (truncated to avoid bloating the FTS index).
    pub fn from_block(
        id: u64,
        command: &str,
        output: &str,
        cwd: Option<&str>,
        started_ms: i64,
    ) -> Self {
        // Cap body at 4 KiB to keep the FTS index lean — full output is
        // available from BlockStore on demand.
        const MAX_BODY_BYTES: usize = 4096;
        let body = if output.len() > MAX_BODY_BYTES {
            // Truncate at a char boundary to avoid splitting UTF-8.
            let mut end = MAX_BODY_BYTES;
            while !output.is_char_boundary(end) && end > 0 {
                end -= 1;
            }
            &output[..end]
        } else {
            output
        };
        Self {
            kind: SearchDocumentKind::Block,
            stable_id: id.to_string(),
            title: command.to_string(),
            body: body.to_string(),
            cwd: cwd.map(|s| s.to_string()),
            updated_at: started_ms,
        }
    }

    /// v1.7.3: Construct from a block annotation (bookmark/note). The
    /// `stable_id` is the block id so activating the hit can navigate to
    /// the block. `title` prefers the note (or the block command when the
    /// note is empty) so the palette shows something useful.
    pub fn from_bookmark(
        block_id: u64,
        note: Option<&str>,
        block_command: &str,
        tags: &[String],
        cwd: Option<&str>,
        updated_ms: i64,
    ) -> Self {
        let title = note
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .unwrap_or(block_command)
            .to_string();
        // Body = note + tags joined, so both are searchable.
        let mut body_parts: Vec<String> = Vec::new();
        if let Some(n) = note {
            let n = n.trim();
            if !n.is_empty() {
                body_parts.push(n.to_string());
            }
        }
        if !tags.is_empty() {
            body_parts.push(tags.join(" "));
        }
        Self {
            kind: SearchDocumentKind::Bookmark,
            stable_id: block_id.to_string(),
            title,
            body: body_parts.join(" "),
            cwd: cwd.map(|s| s.to_string()),
            updated_at: updated_ms,
        }
    }

    /// Construct a searchable document from a saved workflow.
    pub fn from_workflow(workflow: &crate::workflow::Workflow) -> Self {
        let mut body = workflow.description.clone();
        for step in &workflow.steps {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(&step.command);
        }
        Self {
            kind: SearchDocumentKind::Workflow,
            stable_id: workflow.id.to_string(),
            title: workflow.name.clone(),
            body,
            cwd: None,
            updated_at: workflow.last_used_ms,
        }
    }

    /// Construct a searchable document from a saved workspace file.
    pub fn from_workspace(
        stable_path: &str,
        workspace: &crate::workspace::WorkspaceDocument,
        updated_ms: i64,
    ) -> Self {
        let mut body = String::new();
        for tab in &workspace.tabs {
            for (cwd, draft) in tab.panes.collect_panes() {
                if !body.is_empty() {
                    body.push('\n');
                }
                body.push_str(&cwd.to_string_lossy());
                if !draft.is_empty() {
                    body.push(' ');
                    body.push_str(draft);
                }
            }
        }
        Self {
            kind: SearchDocumentKind::Workspace,
            stable_id: stable_path.to_string(),
            title: workspace.name.clone(),
            body,
            cwd: workspace.tabs.first().and_then(|tab| {
                tab.panes
                    .collect_panes()
                    .first()
                    .map(|(cwd, _)| cwd.to_string_lossy().into_owned())
            }),
            updated_at: updated_ms,
        }
    }

    /// Current time as milliseconds since Unix epoch. Convenience for
    /// documents without an explicit timestamp.
    pub fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}

/// v1.7.1: A search hit — the document's display fields plus a relevance
/// score from the FTS5 ranking function.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchHit {
    /// The document that matched.
    pub doc: SearchDocument,
    /// FTS5 `rank` value (lower = more relevant). 0.0 when using the
    /// substring fallback (no ranking available).
    pub score: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_label_round_trip() {
        for k in [
            SearchDocumentKind::Block,
            SearchDocumentKind::Workflow,
            SearchDocumentKind::Workspace,
            SearchDocumentKind::Bookmark,
        ] {
            let label = k.label();
            assert!(!label.is_empty());
        }
    }

    #[test]
    fn kind_from_u8_round_trip() {
        for k in [
            SearchDocumentKind::Block,
            SearchDocumentKind::Workflow,
            SearchDocumentKind::Workspace,
            SearchDocumentKind::Bookmark,
        ] {
            assert_eq!(SearchDocumentKind::from_u8(k as u8), Some(k));
        }
        assert_eq!(SearchDocumentKind::from_u8(255), None);
    }

    #[test]
    fn from_block_basic() {
        let doc = SearchDocument::from_block(42, "ls -la", "total 0\n", Some("/tmp"), 1700000000);
        assert_eq!(doc.kind, SearchDocumentKind::Block);
        assert_eq!(doc.stable_id, "42");
        assert_eq!(doc.title, "ls -la");
        assert_eq!(doc.body, "total 0\n");
        assert_eq!(doc.cwd.as_deref(), Some("/tmp"));
        assert_eq!(doc.updated_at, 1700000000);
    }

    #[test]
    fn from_block_truncates_long_output() {
        let long_output = "x".repeat(10_000);
        let doc = SearchDocument::from_block(1, "cat", &long_output, None, 0);
        assert!(
            doc.body.len() <= 4096,
            "body should be truncated, got {}",
            doc.body.len()
        );
        assert!(doc.body.ends_with('x'));
    }

    #[test]
    fn from_block_truncates_at_char_boundary() {
        // 3-byte UTF-8 char repeated. Truncation must not split a char.
        let long_output = "中".repeat(2000); // 6000 bytes
        let doc = SearchDocument::from_block(1, "cat", &long_output, None, 0);
        assert!(doc.body.len() <= 4096);
        // Verify it's valid UTF-8 (no panic = valid).
        assert!(doc.body.chars().all(|c| c == '中'));
    }

    #[test]
    fn from_block_no_cwd() {
        let doc = SearchDocument::from_block(1, "echo hi", "hi\n", None, 100);
        assert!(doc.cwd.is_none());
    }

    #[test]
    fn from_bookmark_with_note() {
        let tags = vec!["deploy".to_string(), "prod".to_string()];
        let doc = SearchDocument::from_bookmark(
            42,
            Some("deploy script"),
            "kubectl apply -f deploy.yaml",
            &tags,
            Some("/repo"),
            1_700_000_000,
        );
        assert_eq!(doc.kind, SearchDocumentKind::Bookmark);
        assert_eq!(doc.stable_id, "42");
        assert_eq!(doc.title, "deploy script");
        assert_eq!(doc.body, "deploy script deploy prod");
        assert_eq!(doc.cwd.as_deref(), Some("/repo"));
    }

    #[test]
    fn from_bookmark_note_empty_falls_back_to_command() {
        let doc = SearchDocument::from_bookmark(7, None, "git status", &[], None, 0);
        assert_eq!(doc.title, "git status");
        assert_eq!(doc.body, "");
    }

    #[test]
    fn from_bookmark_tags_only() {
        let tags = vec!["rust".to_string(), "cli".to_string()];
        let doc = SearchDocument::from_bookmark(1, None, "cargo build", &tags, None, 0);
        assert_eq!(doc.title, "cargo build");
        assert_eq!(doc.body, "rust cli");
    }

    #[test]
    fn now_ms_is_reasonable() {
        let now = SearchDocument::now_ms();
        // Should be > 1.7 trillion (year 2023+) and < 2 trillion (year 2033).
        assert!(now > 1_700_000_000_000, "now_ms too small: {now}");
        assert!(now < 2_000_000_000_000, "now_ms too large: {now}");
    }

    #[test]
    fn from_workflow_indexes_description_and_commands() {
        let workflow = crate::workflow::Workflow {
            id: 7,
            name: "deploy".to_string(),
            description: "Deploy the current service".to_string(),
            steps: vec![crate::workflow::WorkflowStep {
                command: "kubectl apply -f deploy.yaml".to_string(),
            }],
            variables: Vec::new(),
            source: crate::workflow::WorkflowSource::Manual,
            use_count: 3,
            last_used_ms: 1234,
        };
        let doc = SearchDocument::from_workflow(&workflow);
        assert_eq!(doc.kind, SearchDocumentKind::Workflow);
        assert_eq!(doc.stable_id, "7");
        assert_eq!(doc.title, "deploy");
        assert!(doc.body.contains("Deploy the current service"));
        assert!(doc.body.contains("kubectl apply"));
    }

    #[test]
    fn from_workspace_indexes_name_cwds_and_drafts() {
        let workspace = crate::workspace::WorkspaceDocument {
            version: crate::workspace::WORKSPACE_VERSION,
            name: "backend".to_string(),
            profile: None,
            window: crate::workspace::WorkspaceWindow {
                width: 1200,
                height: 800,
            },
            tabs: vec![crate::workspace::WorkspaceTab {
                panes: crate::workspace::WorkspacePaneNode::Pane {
                    cwd: std::path::PathBuf::from("/repo/backend"),
                    draft: "cargo test".to_string(),
                },
                active_pane_index: 0,
            }],
            active_tab: 0,
        };
        let doc = SearchDocument::from_workspace("/tmp/backend.yaml", &workspace, 5678);
        assert_eq!(doc.kind, SearchDocumentKind::Workspace);
        assert_eq!(doc.stable_id, "/tmp/backend.yaml");
        assert!(doc.body.contains("/repo/backend"));
        assert!(doc.body.contains("cargo test"));
    }
}
