//! Inline tests for `workflow.rs`, extracted to keep the production
//! file under the 800-line architecture gate (repo convention:
//! `pty.rs` -> `pty/tests.rs`).

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

static TEMP_STORE_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn wf(name: &str, command: &str, vars: Vec<(&str, Option<&str>, bool)>) -> Workflow {
    Workflow {
        id: 0,
        name: name.to_string(),
        description: format!("Test workflow {name}"),
        steps: vec![WorkflowStep {
            command: command.to_string(),
        }],
        variables: vars
            .into_iter()
            .map(|(n, d, r)| WorkflowVar {
                name: n.to_string(),
                description: String::new(),
                default: d.map(String::from),
                required: r,
            })
            .collect(),
        source: WorkflowSource::Manual,
        use_count: 0,
        last_used_ms: 0,
    }
}

// ── Render engine tests ───────────────────────────────────────────

#[test]
fn render_replaces_variables() {
    let w = wf(
        "deploy",
        "docker up --scale {{scale}}",
        vec![("scale", Some("2"), true)],
    );
    let mut values = HashMap::new();
    values.insert("scale".to_string(), "5".to_string());
    let rendered = w.render(&values).unwrap();
    assert_eq!(rendered, vec!["docker up --scale 5"]);
}

#[test]
fn render_uses_default_when_value_absent() {
    let w = wf("dev", "cd {{dir}}", vec![("dir", Some("."), true)]);
    let rendered = w.render(&HashMap::new()).unwrap();
    assert_eq!(rendered, vec!["cd ."]);
}

#[test]
fn render_substitutes_undeclared_vars_from_command() {
    // Workflows created via the UI editor have variables: vec![] but
    // may contain {{var}} placeholders. render() must still substitute
    // them using values from the form.
    let w = Workflow {
        id: 0,
        name: "test".into(),
        description: String::new(),
        steps: vec![WorkflowStep {
            command: "echo {{msg}}".into(),
        }],
        variables: vec![], // empty — no declared variables
        source: WorkflowSource::Manual,
        use_count: 0,
        last_used_ms: 0,
    };
    let mut values = HashMap::new();
    values.insert("msg".to_string(), "hello".to_string());
    let rendered = w.render(&values).unwrap();
    assert_eq!(rendered, vec!["echo hello"]);
}

#[test]
fn render_undeclared_var_without_value_becomes_empty() {
    // Undeclared var with no value → empty string (not required, no default).
    let w = Workflow {
        id: 0,
        name: "test".into(),
        description: String::new(),
        steps: vec![WorkflowStep {
            command: "echo [{{opt}}]".into(),
        }],
        variables: vec![],
        source: WorkflowSource::Manual,
        use_count: 0,
        last_used_ms: 0,
    };
    let rendered = w.render(&HashMap::new()).unwrap();
    assert_eq!(rendered, vec!["echo []"]);
}

#[test]
fn render_errors_on_missing_required() {
    let w = wf("test", "echo {{x}}", vec![("x", None, true)]);
    let err = w.render(&HashMap::new()).unwrap_err();
    assert_eq!(err, RenderError::MissingRequired("x".to_string()));
}

#[test]
fn render_empty_string_for_optional_missing() {
    let w = wf("test", "echo [{{opt}}]", vec![("opt", None, false)]);
    let rendered = w.render(&HashMap::new()).unwrap();
    assert_eq!(rendered, vec!["echo []"]);
}

#[test]
fn render_multi_step_shares_variables() {
    let w = Workflow {
        id: 0,
        name: "multi".into(),
        description: String::new(),
        steps: vec![
            WorkflowStep {
                command: "echo {{x}}".into(),
            },
            WorkflowStep {
                command: "echo {{x}} again".into(),
            },
        ],
        variables: vec![WorkflowVar {
            name: "x".into(),
            description: String::new(),
            default: Some("v".into()),
            required: true,
        }],
        source: WorkflowSource::Manual,
        use_count: 0,
        last_used_ms: 0,
    };
    let rendered = w.render(&HashMap::new()).unwrap();
    assert_eq!(rendered, vec!["echo v", "echo v again"]);
}

// ── extract_var_names tests ───────────────────────────────────────

#[test]
fn extract_var_names_finds_placeholders() {
    let w = wf("test", "cd {{project}} && npm run {{script}}", vec![]);
    let names = w.extract_var_names();
    assert_eq!(names, vec!["project", "script"]);
}

#[test]
fn extract_var_names_handles_no_vars() {
    let w = wf("test", "git status", vec![]);
    assert!(w.extract_var_names().is_empty());
}

#[test]
fn extract_var_names_deduplicates() {
    let w = Workflow {
        id: 0,
        name: "t".into(),
        description: String::new(),
        steps: vec![
            WorkflowStep {
                command: "echo {{x}}".into(),
            },
            WorkflowStep {
                command: "echo {{x}}".into(),
            },
        ],
        variables: vec![],
        source: WorkflowSource::Manual,
        use_count: 0,
        last_used_ms: 0,
    };
    let names = w.extract_var_names();
    assert_eq!(names, vec!["x"]);
}

#[test]
fn all_var_names_merges_declared_and_discovered() {
    let w = Workflow {
        id: 0,
        name: "t".into(),
        description: String::new(),
        steps: vec![WorkflowStep {
            command: "echo {{declared}} {{hidden}}".into(),
        }],
        variables: vec![WorkflowVar {
            name: "declared".into(),
            description: String::new(),
            default: None,
            required: true,
        }],
        source: WorkflowSource::Manual,
        use_count: 0,
        last_used_ms: 0,
    };
    let names = w.all_var_names();
    assert_eq!(names, vec!["declared", "hidden"]);
}

// ── WorkflowStore tests ───────────────────────────────────────────

fn temp_store() -> WorkflowStore {
    let path = std::env::temp_dir().join(format!(
        "weft-workflow-{}-{}-{}.db",
        std::process::id(),
        TEMP_STORE_COUNTER.fetch_add(1, Ordering::Relaxed),
        std::time::SystemTime::UNIX_EPOCH
            .elapsed()
            .unwrap_or_default()
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&path);
    WorkflowStore::open(&path).expect("open temp store")
}

#[test]
fn store_insert_and_find_by_name() {
    let store = temp_store();
    let w = wf("deploy", "docker up", vec![]);
    let id = store.insert(&w).unwrap();
    assert!(id > 0);

    let found = store.find_by_name("deploy").unwrap().unwrap();
    assert_eq!(found.name, "deploy");
    assert_eq!(found.steps.len(), 1);
    assert_eq!(found.steps[0].command, "docker up");
}

#[test]
fn store_update_changes_fields() {
    let store = temp_store();
    let id = store.insert(&wf("dev", "npm run", vec![])).unwrap();
    let mut updated = store.find_by_name("dev").unwrap().unwrap();
    updated.description = "Updated description".into();
    updated.steps[0].command = "npm run dev".into();
    store.update(&updated).unwrap();

    let found = store.find_by_name("dev").unwrap().unwrap();
    assert_eq!(found.description, "Updated description");
    assert_eq!(found.steps[0].command, "npm run dev");
    assert_eq!(found.id, id);
}

#[test]
fn store_delete_removes_workflow() {
    let store = temp_store();
    let id = store.insert(&wf("temp", "echo hi", vec![])).unwrap();
    assert!(store.find_by_name("temp").unwrap().is_some());
    store.delete(id).unwrap();
    assert!(store.find_by_name("temp").unwrap().is_none());
}

#[test]
fn store_list_sorted_by_use_count() {
    let store = temp_store();
    // Insert two workflows, bump one's use_count.
    let id_a = store.insert(&wf("alpha", "echo a", vec![])).unwrap();
    let _id_b = store.insert(&wf("beta", "echo b", vec![])).unwrap();
    store.bump_use_count(id_a).unwrap();
    store.bump_use_count(id_a).unwrap();

    let list = store.list().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].name, "alpha"); // used more
    assert_eq!(list[0].use_count, 2);
    assert_eq!(list[1].name, "beta");
    assert_eq!(list[1].use_count, 0);
}

#[test]
fn store_search_matches_name_or_description() {
    let store = temp_store();
    // Note: wf() sets description to "Test workflow {name}".
    store
        .insert(&wf("deploy", "docker compose up", vec![]))
        .unwrap();
    store.insert(&wf("logs", "tail -f log", vec![])).unwrap();

    // Search by name substring.
    let results = store.search("dep", 10).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "deploy");

    // Search by description substring ("Test workflow deploy").
    let results = store.search("deploy", 10).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "deploy"); // matches description

    // "logs" only in name, not description of other workflow.
    let results = store.search("log", 10).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "logs");
}

#[test]
fn store_bump_use_count_increments() {
    let store = temp_store();
    let id = store.insert(&wf("test", "echo", vec![])).unwrap();
    store.bump_use_count(id).unwrap();
    let found = store.find_by_name("test").unwrap().unwrap();
    assert_eq!(found.use_count, 1);
    assert!(found.last_used_ms > 0);
}

#[test]
fn store_count_reflects_insertions() {
    let store = temp_store();
    assert_eq!(store.count().unwrap(), 0);
    store.insert(&wf("a", "echo", vec![])).unwrap();
    store.insert(&wf("b", "echo", vec![])).unwrap();
    assert_eq!(store.count().unwrap(), 2);
}

// ── YAML import/export tests ──────────────────────────────────────

#[test]
fn yaml_export_roundtrip() {
    let store = temp_store();
    let id = store
        .insert(&wf(
            "deploy",
            "docker up --scale {{n}}",
            vec![("n", Some("2"), true)],
        ))
        .unwrap();
    let yaml = store.export_yaml(id).unwrap();
    assert!(yaml.contains("name: deploy"));
    assert!(yaml.contains("docker up"));

    // Re-import into a fresh store.
    let store2 = temp_store();
    let id2 = store2.import_yaml(&yaml).unwrap();
    let found = store2.find_by_name("deploy").unwrap().unwrap();
    assert_eq!(found.id, id2);
    assert_eq!(found.steps[0].command, "docker up --scale {{n}}");
    assert_eq!(found.variables.len(), 1);
    assert_eq!(found.variables[0].name, "n");
    assert_eq!(found.variables[0].default.as_deref(), Some("2"));
}

#[test]
fn yaml_import_updates_existing_by_name() {
    let store = temp_store();
    store.insert(&wf("dev", "npm run", vec![])).unwrap();

    let yaml = "name: dev\ndescription: Updated\nsteps:\n  - command: npm run dev\nvariables: []\n";
    let id = store.import_yaml(yaml).unwrap();

    let found = store.find_by_name("dev").unwrap().unwrap();
    assert_eq!(found.id, id);
    assert_eq!(found.description, "Updated");
    assert_eq!(found.steps[0].command, "npm run dev");
}

#[test]
fn yaml_minimal_valid() {
    let store = temp_store();
    let yaml = "name: sync\ndescription: git pull\nsteps:\n  - command: git pull\nvariables: []\n";
    let id = store.import_yaml(yaml).unwrap();
    let found = store.find_by_name("sync").unwrap().unwrap();
    assert_eq!(found.id, id);
    assert_eq!(found.steps.len(), 1);
}
