use super::*;
use crate::config::AiConfig;

/// v1.8.9 regression test: turning AI off (provider = None) must remove
/// the `provider` key from the TOML so the change survives a save/reload.
/// Previously `set_opt_string` preserved the old value when None, so the
/// Enabled toggle appeared stuck "on" after saving.
#[test]
fn write_ai_removes_provider_when_off() {
    // Simulate a config that currently has provider = "ollama".
    let mut doc = toml_edit::DocumentMut::new();
    doc["ai"] = toml_edit::table();
    {
        let table = doc["ai"].as_table_mut().unwrap();
        table["provider"] = toml_edit::value("ollama");
    }
    // User turns AI off → provider = None.
    let ai_off = AiConfig {
        provider: None,
        ..Default::default()
    };
    write_ai_section(&mut doc, &ai_off, &mut Vec::new());
    let table = doc["ai"].as_table().unwrap();
    assert!(
        !table.contains_key("provider"),
        "provider key must be removed when AI is turned off"
    );
}

#[test]
fn write_ai_writes_provider_when_on() {
    let mut doc = toml_edit::DocumentMut::new();
    let ai_on = AiConfig {
        provider: Some("ollama".into()),
        model: Some("llama3.1".into()),
        ..Default::default()
    };
    write_ai_section(&mut doc, &ai_on, &mut Vec::new());
    let table = doc["ai"].as_table().unwrap();
    assert_eq!(
        table["provider"].as_str(),
        Some("ollama"),
        "provider must be written when AI is on"
    );
}

// ── v1.11.0 M6: malformed hand-written TOML must not panic the save
// path (AUDIT_v1.10.39). Each test feeds the writer a doc where a
// section/entry is a scalar instead of a table; the writer must skip
// that segment (with a warn) and leave the document untouched. ──────

#[test]
fn write_ai_skips_non_table_ai_section_without_panic() {
    let mut doc = toml_edit::DocumentMut::new();
    // User wrote `ai = "x"` (scalar). The save path must not panic.
    doc["ai"] = toml_edit::value("x");
    let ai_on = AiConfig {
        provider: Some("ollama".into()),
        ..Default::default()
    };
    write_ai_section(&mut doc, &ai_on, &mut Vec::new());
    assert_eq!(
        doc["ai"].as_str(),
        Some("x"),
        "malformed [ai] section must be left untouched"
    );
}

#[test]
fn write_profiles_skips_non_table_profiles_without_panic() {
    let mut doc = toml_edit::DocumentMut::new();
    // User wrote `profiles = "x"` (scalar, not a table).
    doc["profiles"] = toml_edit::value("x");
    let cfg = crate::config::Config {
        profiles: std::collections::BTreeMap::from([(
            "work".into(),
            crate::config::ProfileConfig::default(),
        )]),
        ..crate::config::Config::default()
    };
    write_profiles(&mut doc, &cfg, &mut Vec::new());
    assert_eq!(
        doc["profiles"].as_str(),
        Some("x"),
        "malformed [profiles] section must be left untouched"
    );
}

#[test]
fn write_profiles_skips_scalar_profile_entry_without_panic() {
    let mut doc = toml_edit::DocumentMut::new();
    // `[profiles]` exists but `work` is a scalar.
    doc["profiles"] = toml_edit::table();
    doc["profiles"]["work"] = toml_edit::value("x");
    let cfg = crate::config::Config {
        profiles: std::collections::BTreeMap::from([(
            "work".into(),
            crate::config::ProfileConfig::default(),
        )]),
        ..crate::config::Config::default()
    };
    write_profiles(&mut doc, &cfg, &mut Vec::new());
    assert_eq!(
        doc["profiles"]["work"].as_str(),
        Some("x"),
        "scalar profile entry must be left untouched"
    );
}

#[test]
fn write_profile_sections_skips_scalar_section_without_panic() {
    let mut doc = toml_edit::DocumentMut::new();
    // `[profiles.work]` with `font = "y"` (scalar where a table is
    // expected) — the section writer must skip instead of panicking.
    doc["profiles"] = toml_edit::table();
    doc["profiles"]["work"] = toml_edit::table();
    doc["profiles"]["work"]["font"] = toml_edit::value("y");
    let cfg = crate::config::Config {
        profiles: std::collections::BTreeMap::from([(
            "work".into(),
            crate::config::ProfileConfig {
                font: Some(crate::config::FontConfig::default()),
                ..crate::config::ProfileConfig::default()
            },
        )]),
        ..crate::config::Config::default()
    };
    write_profiles(&mut doc, &cfg, &mut Vec::new());
    assert_eq!(
        doc["profiles"]["work"]["font"].as_str(),
        Some("y"),
        "scalar profile section must be left untouched"
    );
}
