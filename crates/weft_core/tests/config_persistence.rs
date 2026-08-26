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

// ── v1.11.1 [paste] section (PLAN_v1111 §4.2) ──────────────────────────

#[test]
fn paste_section_round_trips_non_default_values() {
    let path = config_path("paste-roundtrip");
    std::fs::write(
        &path,
        "[paste]\nconfirm_large = false\nsize_threshold_kib = 64\n",
    )
    .unwrap();

    let cfg: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(!cfg.paste.confirm_large);
    assert!(
        cfg.paste.confirm_control_chars,
        "absent key keeps default true"
    );
    assert_eq!(cfg.paste.size_threshold_kib, 64);

    // Save back with defaults: stale non-default keys must be removed so
    // the defaults take effect on reload (same semantics as [editor]).
    Config::default().save_to_path(&path).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    let reloaded: Config = toml::from_str(&text).unwrap();
    assert_eq!(reloaded.paste, weft_core::config::PasteConfig::default());
    let document: toml_edit::DocumentMut = text.parse().unwrap();
    let table = document.get("paste").and_then(toml_edit::Item::as_table);
    assert!(
        table.is_none_or(|t| t.iter().count() == 0),
        "all-default [paste] must not persist any key"
    );
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn paste_section_profile_override_round_trip() {
    use weft_core::config::ProfileConfig;

    let mut cfg = Config::default();
    cfg.profiles.insert(
        "quiet".into(),
        ProfileConfig {
            paste: Some(weft_core::config::PasteConfig {
                confirm_control_chars: false,
                ..weft_core::config::PasteConfig::default()
            }),
            ..ProfileConfig::default()
        },
    );
    cfg.active_profile = Some("quiet".into());

    let path = config_path("paste-profile");
    cfg.save_to_path(&path).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("[profiles.quiet.paste]"),
        "profile override persisted:\n{text}"
    );

    let reloaded: Config = toml::from_str(&text).unwrap();
    let profile = reloaded.profiles.get("quiet").unwrap();
    let paste = profile.paste.as_ref().unwrap();
    assert!(!paste.confirm_control_chars);
    assert!(paste.confirm_large);
    assert_eq!(paste.size_threshold_kib, 16);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// rust-reviewer v1.11.1 M-1 regression: `confirm_large` defaults to TRUE,
/// so an explicit `false` in a profile override is the non-default value
/// that MUST be persisted (the original predicate was inverted and dropped
/// the key, silently restoring `true` on reload).
#[test]
fn paste_profile_confirm_large_false_survives_round_trip() {
    use weft_core::config::ProfileConfig;

    let mut cfg = Config::default();
    cfg.profiles.insert(
        "noprompt".into(),
        ProfileConfig {
            paste: Some(weft_core::config::PasteConfig {
                confirm_large: false,
                ..weft_core::config::PasteConfig::default()
            }),
            ..ProfileConfig::default()
        },
    );
    cfg.active_profile = Some("noprompt".into());

    let path = config_path("paste-profile-false");
    cfg.save_to_path(&path).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("confirm_large = false"),
        "explicit false must be persisted:\n{text}"
    );

    let reloaded: Config = toml::from_str(&text).unwrap();
    let paste = reloaded
        .profiles
        .get("noprompt")
        .unwrap()
        .paste
        .as_ref()
        .unwrap();
    assert!(
        !paste.confirm_large,
        "explicit confirm_large=false lost in round-trip"
    );
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}
