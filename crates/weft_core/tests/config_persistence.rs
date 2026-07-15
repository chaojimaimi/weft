use std::sync::atomic::{AtomicUsize, Ordering};

use weft_core::config::Config;

fn config_path(tag: &str) -> std::path::PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "weft-config-persistence-{}-{id}-{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("config.toml")
}

#[test]
fn resetting_settings_fields_removes_stale_persisted_values() {
    let path = config_path("reset-defaults");
    std::fs::write(
        &path,
        r#"# preserve me
[font]
size = 18.0
line_height = 1.4

[window]
width = 1200
height = 900
opacity = 0.8
padding_x = 8
padding_y = 6

[scrollback]
lines = 50000

[editor]
submit_on_ctrl_enter = true
"#,
    )
    .unwrap();

    Config::default().save_to_path(&path).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    let reloaded: Config = toml::from_str(&text).unwrap();
    let defaults = Config::default();
    assert_eq!(reloaded.font.size, defaults.font.size);
    assert_eq!(reloaded.font.line_height, defaults.font.line_height);
    assert_eq!(reloaded.window.width, defaults.window.width);
    assert_eq!(reloaded.window.height, defaults.window.height);
    assert_eq!(reloaded.window.opacity, defaults.window.opacity);
    assert_eq!(reloaded.window.padding_x, defaults.window.padding_x);
    assert_eq!(reloaded.window.padding_y, defaults.window.padding_y);
    assert_eq!(reloaded.scrollback.lines, defaults.scrollback.lines);
    assert_eq!(
        reloaded.editor.submit_on_ctrl_enter,
        defaults.editor.submit_on_ctrl_enter
    );
    assert!(text.contains("# preserve me"));
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn malformed_existing_config_is_never_overwritten() {
    let path = config_path("malformed");
    let original = "[font\nsize = ???\n# user recovery data\n";
    std::fs::write(&path, original).unwrap();

    let error = Config::default().save_to_path(&path).unwrap_err();
    assert!(error
        .to_string()
        .contains("cannot preserve existing config"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    assert!(!path.with_extension("toml.tmp").exists());
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn schema_invalid_existing_config_is_never_overwritten() {
    let path = config_path("schema-invalid");
    let original = "[font]\nsize = \"large\"\n# user recovery data\n";
    std::fs::write(&path, original).unwrap();

    let error = Config::default().save_to_path(&path).unwrap_err();
    assert!(error
        .to_string()
        .contains("cannot preserve existing config"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    assert!(!path.with_extension("toml.tmp").exists());
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn non_table_existing_section_returns_error_instead_of_panicking() {
    let path = config_path("non-table-section");
    let original = "font = \"oops\"\n# user recovery data\n";
    std::fs::write(&path, original).unwrap();

    let result = std::panic::catch_unwind(|| Config::default().save_to_path(&path));
    assert!(result.is_ok(), "saving must not panic on a damaged schema");
    let error = result.unwrap().unwrap_err();
    assert!(error
        .to_string()
        .contains("cannot preserve existing config"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    assert!(!path.with_extension("toml.tmp").exists());
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn schema_valid_inline_section_can_be_saved_without_panicking() {
    let path = config_path("inline-section");
    std::fs::write(&path, "font = { size = 18.0 }\n").unwrap();

    let result = std::panic::catch_unwind(|| Config::default().save_to_path(&path));
    assert!(result.is_ok(), "saving must not panic on an inline table");
    result.unwrap().unwrap();
    let reloaded: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(reloaded.font.size, Config::default().font.size);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn unknown_top_level_scalar_is_preserved() {
    let path = config_path("unknown-scalar");
    std::fs::write(&path, "syntax = \"custom-tool-value\"\n").unwrap();

    Config::default().save_to_path(&path).unwrap();
    let document: toml_edit::DocumentMut = std::fs::read_to_string(&path).unwrap().parse().unwrap();
    assert_eq!(
        document.get("syntax").and_then(toml_edit::Item::as_str),
        Some("custom-tool-value")
    );
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn unreadable_target_fails_without_replacing_it() {
    let path = config_path("target-directory");
    std::fs::remove_file(&path).ok();
    std::fs::create_dir(&path).unwrap();

    assert!(Config::default().save_to_path(&path).is_err());
    assert!(path.is_dir());
    assert!(!path.with_extension("toml.tmp").exists());
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}
