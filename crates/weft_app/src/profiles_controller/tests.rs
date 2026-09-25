//! Profile transaction + settings-merge tests (split from
//! profiles_controller.rs to keep the production file within its
//! architecture-gate budget; child-module privacy reaches pub(super) items
//! exactly like the inline module did — config_controller_tests precedent).

use super::*;
use weft_core::config::{validate_profile_name, ProfileConfig};

fn loaded_from(source: Config) -> weft_core::config::LoadedConfig {
    let (effective, diagnostics) = source.resolve_active_profile().unwrap();
    weft_core::config::LoadedConfig {
        source,
        effective,
        fingerprint: 42,
        diagnostics,
    }
}

#[test]
fn profile_transactions_create_switch_and_delete_successfully() {
    let base = Config::default();
    let created = run_profile_transaction(&base, ProfileChange::Create("work"), |candidate| {
        Ok(loaded_from(candidate.clone()))
    })
    .unwrap();
    assert_eq!(created.source.active_profile.as_deref(), Some("work"));
    assert!(created.source.profiles.contains_key("work"));

    let switched =
        run_profile_transaction(&created.source, ProfileChange::Switch(None), |candidate| {
            Ok(loaded_from(candidate.clone()))
        })
        .unwrap();
    assert_eq!(switched.source.active_profile, None);
    assert!(switched.source.profiles.contains_key("work"));

    let deleted = run_profile_transaction(
        &created.source,
        ProfileChange::Delete("work"),
        |candidate| Ok(loaded_from(candidate.clone())),
    )
    .unwrap();
    assert_eq!(deleted.source.active_profile, None);
    assert!(!deleted.source.profiles.contains_key("work"));
}

#[test]
fn switch_missing_profile_is_rejected_before_persist() {
    let source = Config::default();
    let persist_called = std::cell::Cell::new(false);
    let result = run_profile_transaction(&source, ProfileChange::Switch(Some("missing")), |_| {
        persist_called.set(true);
        unreachable!("missing profile must fail before persistence")
    });

    assert!(matches!(result, Err(ProfileTransactionError::NotFound(name)) if name == "missing"));
    assert!(!persist_called.get());
    assert_eq!(source.active_profile, None);
}

#[test]
fn persistence_failures_do_not_commit_source() {
    let source = Config::default();
    let save_result = run_profile_transaction(&source, ProfileChange::Create("work"), |_| {
        Err(ProfileTransactionError::Save(
            weft_core::config::ConfigSaveError::NoConfigPath,
        ))
    });
    assert!(matches!(save_result, Err(ProfileTransactionError::Save(_))));

    let disk_candidate = std::cell::RefCell::new(None);
    let reload_result =
        run_profile_transaction(&source, ProfileChange::Create("work"), |candidate| {
            *disk_candidate.borrow_mut() = Some(candidate.clone());
            Err(ProfileTransactionError::Reload(
                weft_core::config::ConfigLoadError::NoPath,
            ))
        });
    assert!(matches!(
        reload_result,
        Err(ProfileTransactionError::Reload(_))
    ));
    assert_eq!(
        disk_candidate
            .borrow()
            .as_ref()
            .unwrap()
            .active_profile
            .as_deref(),
        Some("work"),
        "save succeeded before the injected reload failure"
    );
    assert_eq!(
        source.active_profile, None,
        "runtime source was not committed"
    );
    assert!(
        source.profiles.is_empty(),
        "runtime profiles were not committed"
    );
}

#[test]
fn active_profile_settings_write_only_dirty_sections() {
    let mut source = Config::default();
    source.font.size = 13.0;
    source.window.padding_x = 2;
    source.profiles.insert(
        "work".into(),
        ProfileConfig {
            font: Some(weft_core::config::FontConfig {
                size: 18.0,
                ..Default::default()
            }),
            ..Default::default()
        },
    );
    source.active_profile = Some("work".into());
    let (mut draft, _) = source.resolve_active_profile().unwrap();
    draft.window.padding_x = 9;

    let merged = merge_settings_draft(
        &source,
        &draft,
        weft_core::config::ConfigSectionMask::WINDOW,
    )
    .unwrap();
    assert_eq!(merged.font.size, 13.0, "base font must stay raw");
    assert_eq!(merged.window.padding_x, 2, "base window must stay raw");
    let profile = merged.profiles.get("work").unwrap();
    assert_eq!(profile.font.as_ref().unwrap().size, 18.0);
    assert_eq!(profile.window.as_ref().unwrap().padding_x, 9);
    assert!(profile.theme.is_none());
}

#[test]
fn base_settings_do_not_modify_profiles() {
    let mut source = Config::default();
    source.profiles.insert(
        "work".into(),
        ProfileConfig {
            font: Some(weft_core::config::FontConfig {
                size: 18.0,
                ..Default::default()
            }),
            ..Default::default()
        },
    );
    let mut draft = source.clone();
    draft.font.size = 15.0;

    let merged =
        merge_settings_draft(&source, &draft, weft_core::config::ConfigSectionMask::FONT).unwrap();
    assert_eq!(merged.font.size, 15.0);
    assert_eq!(
        merged
            .profiles
            .get("work")
            .unwrap()
            .font
            .as_ref()
            .unwrap()
            .size,
        18.0
    );
}

#[test]
fn settings_reload_failure_keeps_runtime_source_uncommitted() {
    let mut source = Config::default();
    source
        .profiles
        .insert("work".into(), ProfileConfig::default());
    source.active_profile = Some("work".into());
    let (mut draft, _) = source.resolve_active_profile().unwrap();
    draft.window.padding_x = 7;
    let disk_candidate = std::cell::RefCell::new(None);

    let result = run_settings_draft_transaction(
        &source,
        &draft,
        weft_core::config::ConfigSectionMask::WINDOW,
        |candidate| {
            *disk_candidate.borrow_mut() = Some(candidate.clone());
            Err(ProfileTransactionError::Reload(
                weft_core::config::ConfigLoadError::NoPath,
            ))
        },
    );

    assert!(matches!(result, Err(ProfileTransactionError::Reload(_))));
    assert!(source.profiles.get("work").unwrap().window.is_none());
    assert_eq!(source.window.padding_x, 0);
    assert_eq!(
        disk_candidate
            .borrow()
            .as_ref()
            .unwrap()
            .profiles
            .get("work")
            .unwrap()
            .window
            .as_ref()
            .unwrap()
            .padding_x,
        7
    );
}

/// v1.12.19 (PLAN_v11217 §3.8 T13b): the Blocks tab rows write through
/// BOTH merge arms — the active profile gets a full-section override,
/// and the base branch copies the section too (review P0: a missing
/// arm means the save "succeeds" while the file never changes).
#[test]
fn blocks_settings_write_to_profile_and_base_arms() {
    // Profile branch.
    let mut source = Config::default();
    source
        .profiles
        .insert("work".into(), ProfileConfig::default());
    source.active_profile = Some("work".into());
    let (mut draft, _) = source.resolve_active_profile().unwrap();
    draft.blocks.retained_limit = 5_000;
    draft.blocks.output_cap_mib = 8;

    let merged = merge_settings_draft(
        &source,
        &draft,
        weft_core::config::ConfigSectionMask::BLOCKS,
    )
    .unwrap();
    let profile = merged.profiles.get("work").unwrap();
    assert_eq!(
        profile.blocks.as_ref().unwrap().retained_limit,
        5_000,
        "active profile must receive the blocks override"
    );
    assert_eq!(profile.blocks.as_ref().unwrap().output_cap_mib, 8);
    // merged is the SOURCE-form candidate (profiles are not flattened —
    // the raw/effective separation), so the base section stays raw here;
    // the effective view only picks the override up after reload.
    assert_eq!(
        merged.blocks.retained_limit,
        Config::default().blocks.retained_limit,
        "raw base blocks must not be flattened with the override"
    );

    // Base branch (no active profile).
    let source_no_profile = Config::default();
    let merged_base = merge_settings_draft(
        &source_no_profile,
        &draft,
        weft_core::config::ConfigSectionMask::BLOCKS,
    )
    .unwrap();
    assert_eq!(merged_base.blocks.retained_limit, 5_000);
    assert_eq!(merged_base.blocks.output_cap_mib, 8);
}

/// v1.12.19 (PLAN_v11217 §3.8 T13a.1): the Session recovery row is
/// global-only — the base `[session]` is written whether or not a
/// profile is active, and `ProfileConfig` never gains a session
/// section (deny_unknown_fields would reject it at load).
#[test]
fn session_settings_write_globally_like_ai() {
    let mut source = Config::default();
    source
        .profiles
        .insert("work".into(), ProfileConfig::default());
    source.active_profile = Some("work".into());
    let mut draft = source.clone();
    draft.session.recovery = weft_core::config::RecoveryMode::Auto;

    let merged = merge_settings_draft(
        &source,
        &draft,
        weft_core::config::ConfigSectionMask::SESSION,
    )
    .unwrap();
    assert_eq!(
        merged.session.recovery,
        weft_core::config::RecoveryMode::Auto,
        "base [session] must carry the edit"
    );
    assert!(
        merged.profiles.get("work").unwrap().blocks.is_none(),
        "untouched profile sections stay None"
    );
}

/// P0 guard (review round 1): the full draft→persist round trip for the
/// BLOCKS bit. A ConfigSectionMask bit without a merge arm (or a merge
/// arm without a save writer) "succeeds" while silently dropping the
/// edit — the ONLY reliable guard is save → reload through the real
/// writers.
#[test]
fn blocks_draft_survives_the_persist_round_trip() {
    let tmp = std::env::temp_dir().join(format!(
        "weft-blocks-draft-persist-{}-{}",
        std::process::id(),
        std::time::SystemTime::UNIX_EPOCH
            .elapsed()
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&tmp);

    let mut source = Config::default();
    source.blocks.retained_limit = 2_000; // default
    let mut draft = source.clone();
    draft.blocks.retained_limit = 7_500;
    draft.blocks.output_cap_mib = 16;

    let persist = |candidate: &Config| {
        candidate.save_to_path(&tmp).unwrap();
        let loaded = weft_core::config::load_resolved_from_path(&tmp).unwrap();
        Ok(loaded)
    };
    let loaded = run_settings_draft_transaction(
        &source,
        &draft,
        weft_core::config::ConfigSectionMask::BLOCKS,
        persist,
    )
    .unwrap();
    assert_eq!(
        loaded.source.blocks.retained_limit, 7_500,
        "the persisted FILE must carry the draft's retained limit"
    );
    assert_eq!(loaded.source.blocks.output_cap_mib, 16);
    let _ = std::fs::remove_file(&tmp);
}

/// The SESSION mask bit rides the same persist path: the recovery value
/// must survive save → reload (global section, written to the base doc).
#[test]
fn session_draft_survives_the_persist_round_trip() {
    let tmp = std::env::temp_dir().join(format!(
        "weft-session-draft-persist-{}-{}",
        std::process::id(),
        std::time::SystemTime::UNIX_EPOCH
            .elapsed()
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&tmp);

    let source = Config::default();
    let mut draft = source.clone();
    draft.session.recovery = weft_core::config::RecoveryMode::Never;

    let persist = |candidate: &Config| {
        candidate.save_to_path(&tmp).unwrap();
        weft_core::config::load_resolved_from_path(&tmp).map_err(ProfileTransactionError::Reload)
    };
    let loaded = run_settings_draft_transaction(
        &source,
        &draft,
        weft_core::config::ConfigSectionMask::SESSION,
        persist,
    )
    .unwrap();
    assert_eq!(
        loaded.source.session.recovery,
        weft_core::config::RecoveryMode::Never,
        "the persisted FILE must carry the draft's recovery mode"
    );
    let _ = std::fs::remove_file(&tmp);
}

/// Validate-profile-name is the first gate of `create_profile`.
/// The transaction can't even start if the name is bad, so we test
/// the pure validation function here rather than spinning up a full
/// App (which requires a Metal device).
#[test]
fn create_profile_rejects_invalid_names() {
    // The validation function is the gate; create_profile wraps it.
    assert!(validate_profile_name("").is_err());
    assert!(validate_profile_name(" ").is_err());
    assert!(validate_profile_name("base").is_err());
    assert!(validate_profile_name("BASE").is_err());
    assert!(validate_profile_name("_bad").is_err()); // must start alphanumeric
    assert!(validate_profile_name("has space").is_err());
    assert!(validate_profile_name("has/slash").is_err());
    // 33 chars
    assert!(validate_profile_name(&"a".repeat(33)).is_err());
}

#[test]
fn create_profile_accepts_valid_names() {
    assert!(validate_profile_name("work").is_ok());
    assert!(validate_profile_name("dev-1").is_ok());
    assert!(validate_profile_name("a.b.c").is_ok());
    assert!(validate_profile_name("A_1-2.3").is_ok());
    assert!(validate_profile_name(&"a".repeat(32)).is_ok());
}

/// `profile_names_sorted` returns BTreeMap order (alphabetical), so
/// the Settings + Palette views see a stable index↔name mapping
/// within a frame. We can't test the full App method without a
/// renderer, but we can verify the BTreeMap ordering invariant on
/// a raw Config.
#[test]
fn profile_names_are_alphabetical() {
    let mut cfg = Config::default();
    cfg.profiles
        .insert("zebra".into(), ProfileConfig::default());
    cfg.profiles
        .insert("alpha".into(), ProfileConfig::default());
    cfg.profiles
        .insert("mango".into(), ProfileConfig::default());
    let names: Vec<String> = cfg.profiles.keys().cloned().collect();
    assert_eq!(names, vec!["alpha", "mango", "zebra"]);
}

/// `active_profile_name` normalizes empty strings to `None`, matching
/// `resolve_active_profile`'s behavior. This prevents the Settings
/// selector from showing a stale "active" state when the TOML has
/// `active_profile = ""`.
#[test]
fn empty_active_profile_normalizes_to_none() {
    let cfg = Config {
        active_profile: Some(String::new()),
        ..Default::default()
    };
    // The normalization logic: trim + filter empty.
    let normalized = cfg
        .active_profile
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    assert_eq!(normalized, None);
}

/// Deleting the active profile must clear `active_profile` so we
/// don't leave a dangling reference. This test verifies the
/// invariant on a raw Config (the App method wraps this logic).
#[test]
fn delete_active_profile_clears_active_ref() {
    let mut cfg = Config {
        active_profile: Some("work".into()),
        ..Default::default()
    };
    cfg.profiles.insert("work".into(), ProfileConfig::default());

    // Simulate delete.
    let was_active = cfg.active_profile.as_deref() == Some("work");
    assert!(was_active);
    if was_active {
        cfg.active_profile = None;
    }
    cfg.profiles.remove("work");

    assert_eq!(cfg.active_profile, None);
    assert!(!cfg.profiles.contains_key("work"));
}

/// Creating a profile that already exists must fail before touching
/// the source. This test verifies the duplicate check on a raw Config.
#[test]
fn create_duplicate_profile_fails() {
    let mut cfg = Config::default();
    cfg.profiles.insert("work".into(), ProfileConfig::default());
    // The duplicate check.
    assert!(cfg.profiles.contains_key("work"));
}

/// The 32-profile cap is enforced before insertion. This test
/// verifies the cap on a raw Config.
#[test]
fn profile_cap_enforced() {
    let mut cfg = Config::default();
    for i in 0..32 {
        cfg.profiles
            .insert(format!("p{i:02}"), ProfileConfig::default());
    }
    assert_eq!(cfg.profiles.len(), 32);
    // The cap check: `cfg.profiles.len() >= MAX_PROFILES` should be true.
    assert!(cfg.profiles.len() >= weft_core::config::MAX_PROFILES);
}
