// ── Tests ───────────────────────────────────────────────────────────────

use super::*;

fn sample_doc() -> WorkspaceDocument {
    WorkspaceDocument {
        version: 1,
        name: "test-project".into(),
        profile: Some("dark".into()),
        window: WorkspaceWindow {
            width: 1200,
            height: 800,
        },
        active_tab: 0,
        tabs: vec![
            WorkspaceTab {
                panes: WorkspacePaneNode::Split {
                    direction: SplitDirection::Vertical,
                    ratio: 0.5,
                    first: Box::new(WorkspacePaneNode::Pane {
                        cwd: "/home/user/src".into(),
                        draft: String::new(),
                    }),
                    second: Box::new(WorkspacePaneNode::Pane {
                        cwd: "/home/user/docs".into(),
                        draft: "cargo build".into(),
                    }),
                },
                active_pane_index: 0,
            },
            WorkspaceTab {
                panes: WorkspacePaneNode::Pane {
                    cwd: "/home/user".into(),
                    draft: String::new(),
                },
                active_pane_index: 0,
            },
        ],
    }
}

#[test]
fn round_trip_yaml_serialization() {
    let doc = sample_doc();
    let yaml = doc.to_yaml().unwrap();
    let loaded = WorkspaceDocument::from_yaml(&yaml).unwrap();
    assert_eq!(doc, loaded);
}

#[test]
fn round_trip_file_save_load() {
    let dir = std::env::temp_dir().join(format!(
        "weft-workspace-rt-{}-{}",
        std::process::id(),
        std::time::SystemTime::UNIX_EPOCH
            .elapsed()
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("workspace.yaml");

    let doc = sample_doc();
    doc.save(&path).unwrap();
    let loaded = WorkspaceDocument::load(&path).unwrap();
    assert_eq!(doc, loaded);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn unknown_version_rejected() {
    let yaml = "version: 999\nname: bad\nwindow:\n  width: 200\n  height: 200\ntabs:\n  - panes:\n      kind: Pane\n      cwd: /tmp\n      draft: \"\"\n    active_pane_index: 0\n";
    let err = WorkspaceDocument::from_yaml(yaml).unwrap_err();
    assert!(matches!(
        err,
        WorkspaceError::UnsupportedVersion { found: 999, max: 1 }
    ));
}

#[test]
fn missing_optional_fields_use_defaults() {
    // No `profile`, no `active_tab` — should default to None / 0.
    let yaml = "version: 1\nname: minimal\nwindow:\n  width: 200\n  height: 200\ntabs:\n  - panes:\n      kind: Pane\n      cwd: /tmp\n      draft: \"\"\n    active_pane_index: 0\n";
    let doc = WorkspaceDocument::from_yaml(yaml).unwrap();
    assert_eq!(doc.profile, None);
    assert_eq!(doc.active_tab, 0);
}

#[test]
fn workspace_yaml_with_nul_cwd_is_rejected() {
    // v1.12.25 (audit core P2-3): YAML `"\0"` is a legal escape; a NUL
    // cwd used to reach the PTY env builder and panic the main thread in
    // `CString::new`. `validate()` (via `from_yaml`) must reject it.
    let yaml = "version: 1\nname: nul-cwd\nwindow:\n  width: 200\n  height: 200\ntabs:\n  - panes:\n      kind: Pane\n      cwd: \"\\0\"\n      draft: \"\"\n    active_pane_index: 0\n";
    let err = WorkspaceDocument::from_yaml(yaml).unwrap_err();
    assert!(matches!(err, WorkspaceError::Validation(ref msg) if msg.contains("NUL")));
}

#[test]
fn load_file_caps_snapshot_size() {
    // v1.12.25 (audit L-3): oversized hand-edited/corrupt snapshots are
    // rejected before serde_yaml can expand them; normal small files are
    // unaffected.
    let dir = std::env::temp_dir().join(format!(
        "weft-workspace-cap-{}-{}",
        std::process::id(),
        std::time::SystemTime::UNIX_EPOCH
            .elapsed()
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("workspace.yaml");

    // A normal small file still loads.
    let doc = sample_doc();
    doc.save(&path).unwrap();
    assert!(WorkspaceDocument::load(&path).is_ok());

    // An oversized file is rejected before parsing (no .bak exists yet,
    // so `load` propagates the error).
    std::fs::write(&path, vec![b'x'; 16 * 1024 * 1024 + 1]).unwrap();
    let err = WorkspaceDocument::load(&path).unwrap_err();
    assert!(matches!(err, WorkspaceError::Validation(ref msg) if msg.contains("limit")));

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn empty_tabs_rejected() {
    let yaml = "version: 1\nname: empty\nwindow:\n  width: 200\n  height: 200\ntabs: []\n";
    let err = WorkspaceDocument::from_yaml(yaml).unwrap_err();
    assert!(matches!(err, WorkspaceError::Validation(_)));
}

#[test]
fn active_tab_out_of_bounds_rejected() {
    let yaml = "version: 1\nname: bad\nwindow:\n  width: 200\n  height: 200\nactive_tab: 5\ntabs:\n  - panes:\n      kind: Pane\n      cwd: /tmp\n      draft: \"\"\n    active_pane_index: 0\n";
    let err = WorkspaceDocument::from_yaml(yaml).unwrap_err();
    assert!(matches!(err, WorkspaceError::Validation(_)));
}

#[test]
fn active_pane_index_out_of_bounds_rejected() {
    let yaml = "version: 1\nname: bad\nwindow:\n  width: 200\n  height: 200\ntabs:\n  - panes:\n      kind: Pane\n      cwd: /tmp\n      draft: \"\"\n    active_pane_index: 5\n";
    let err = WorkspaceDocument::from_yaml(yaml).unwrap_err();
    assert!(matches!(err, WorkspaceError::Validation(_)));
}

#[test]
fn nested_split_round_trips() {
    let doc = WorkspaceDocument {
        version: 1,
        name: "nested".into(),
        profile: None,
        window: WorkspaceWindow {
            width: 800,
            height: 600,
        },
        active_tab: 0,
        tabs: vec![WorkspaceTab {
            panes: WorkspacePaneNode::Split {
                direction: SplitDirection::Horizontal,
                ratio: 0.6,
                first: Box::new(WorkspacePaneNode::Pane {
                    cwd: "/a".into(),
                    draft: "ls".into(),
                }),
                second: Box::new(WorkspacePaneNode::Split {
                    direction: SplitDirection::Vertical,
                    ratio: 0.5,
                    first: Box::new(WorkspacePaneNode::Pane {
                        cwd: "/b".into(),
                        draft: String::new(),
                    }),
                    second: Box::new(WorkspacePaneNode::Pane {
                        cwd: "/c".into(),
                        draft: String::new(),
                    }),
                }),
            },
            active_pane_index: 1,
        }],
    };
    let yaml = doc.to_yaml().unwrap();
    let loaded = WorkspaceDocument::from_yaml(&yaml).unwrap();
    assert_eq!(doc, loaded);
    assert_eq!(loaded.tabs[0].panes.pane_count(), 3);
}

#[test]
fn out_of_range_ratio_clamped_on_load() {
    let yaml = "version: 1\nname: clamp\nwindow:\n  width: 200\n  height: 200\ntabs:\n  - panes:\n      kind: Split\n      direction: Vertical\n      ratio: 5.0\n      first:\n        kind: Pane\n        cwd: /a\n        draft: \"\"\n      second:\n        kind: Pane\n        cwd: /b\n        draft: \"\"\n    active_pane_index: 0\n";
    let doc = WorkspaceDocument::load_from_str(yaml).unwrap();
    // The ratio should be clamped to 0.9.
    match &doc.tabs[0].panes {
        WorkspacePaneNode::Split { ratio, .. } => assert!((ratio - 0.9).abs() < 0.001),
        _ => panic!("expected Split"),
    }
}

#[test]
fn draft_too_large_rejected() {
    let big = "x".repeat(MAX_WORKSPACE_DRAFT_BYTES + 1);
    let yaml = format!(
        "version: 1\nname: big\nwindow:\n  width: 200\n  height: 200\ntabs:\n  - panes:\n      kind: Pane\n      cwd: /tmp\n      draft: \"{big}\"\n    active_pane_index: 0\n"
    );
    let err = WorkspaceDocument::from_yaml(&yaml).unwrap_err();
    assert!(matches!(err, WorkspaceError::DraftTooLarge { .. }));
}

#[test]
fn corrupt_file_falls_back_to_bak() {
    let dir = std::env::temp_dir().join(format!(
        "weft-workspace-bak-{}-{}",
        std::process::id(),
        std::time::SystemTime::UNIX_EPOCH
            .elapsed()
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("workspace.yaml");
    let bak = dir.join("workspace.yaml.bak");

    // Save a good doc, then corrupt the main file.
    let doc = sample_doc();
    doc.save(&path).unwrap();
    // The save backed up the (non-existent) prior file, so .bak may not
    // exist. Create it by copying the current file, then corrupt main.
    std::fs::copy(&path, &bak).unwrap();
    std::fs::write(&path, "corrupted yaml {{{{").unwrap();

    // Load should fall back to .bak.
    let loaded = WorkspaceDocument::load(&path).unwrap();
    assert_eq!(loaded, doc);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn collect_panes_dfs_order() {
    let node = WorkspacePaneNode::Split {
        direction: SplitDirection::Vertical,
        ratio: 0.5,
        first: Box::new(WorkspacePaneNode::Pane {
            cwd: "/a".into(),
            draft: "1".into(),
        }),
        second: Box::new(WorkspacePaneNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(WorkspacePaneNode::Pane {
                cwd: "/b".into(),
                draft: "2".into(),
            }),
            second: Box::new(WorkspacePaneNode::Pane {
                cwd: "/c".into(),
                draft: "3".into(),
            }),
        }),
    };
    let panes = node.collect_panes();
    assert_eq!(panes.len(), 3);
    assert_eq!(panes[0].0, Path::new("/a"));
    assert_eq!(panes[1].0, Path::new("/b"));
    assert_eq!(panes[2].0, Path::new("/c"));
}

#[test]
fn migrate_noop_for_v1() {
    let doc = sample_doc();
    let migrated = WorkspaceDocument::migrate(doc.clone()).unwrap();
    assert_eq!(doc, migrated);
}

#[test]
fn window_too_small_rejected() {
    let yaml = "version: 1\nname: small\nwindow:\n  width: 50\n  height: 200\ntabs:\n  - panes:\n      kind: Pane\n      cwd: /tmp\n      draft: \"\"\n    active_pane_index: 0\n";
    let err = WorkspaceDocument::from_yaml(yaml).unwrap_err();
    assert!(matches!(err, WorkspaceError::Validation(_)));
}

#[test]
fn window_too_large_rejected() {
    let yaml = "version: 1\nname: huge\nwindow:\n  width: 99999\n  height: 200\ntabs:\n  - panes:\n      kind: Pane\n      cwd: /tmp\n      draft: \"\"\n    active_pane_index: 0\n";
    let err = WorkspaceDocument::from_yaml(yaml).unwrap_err();
    assert!(matches!(err, WorkspaceError::Validation(_)));
}
