// ── Tests ─────────────────────────────────────────────────────────────

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn tmp(tag: &str) -> PathBuf {
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "weft-transfer-{tag}-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::UNIX_EPOCH
            .elapsed()
            .unwrap_or_default()
            .as_nanos(),
        id
    ))
}

fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

// ── Export ────────────────────────────────────────────────────────

#[test]
fn export_copies_raw_bytes_when_source_file_exists() {
    // Source file with comments + unknown fields.
    let src = tmp("export-src");
    let dest = tmp("export-dest");
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dest);
    let content = r#"# user comment
[font]
family = "Menlo"
size = 14.0

# unknown field must survive
[unknown_section]
key = "value"
"#;
    write(&src, content);

    let source = Config::default();
    export_config_document(Some(&src), &source, &dest).unwrap();

    let exported = std::fs::read_to_string(&dest).unwrap();
    assert!(exported.contains("# user comment"));
    assert!(exported.contains("[unknown_section]"));
    assert!(exported.contains("key = \"value\""));
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dest);
}

#[test]
fn export_serializes_canonical_toml_when_source_file_absent() {
    // No source file — export `source` as canonical TOML.
    let dest = tmp("export-canonical");
    let _ = std::fs::remove_file(&dest);
    let mut source = Config::default();
    // Use non-default values so save_to_path actually writes them.
    source.font.family = "JetBrains Mono".into();
    source.font.size = 15.0;

    export_config_document(None, &source, &dest).unwrap();

    let exported = std::fs::read_to_string(&dest).unwrap();
    assert!(exported.contains("JetBrains Mono"));
    assert!(exported.contains("15.0"));
    let _ = std::fs::remove_file(&dest);
}

#[test]
fn export_rejects_same_file_as_source() {
    let src = tmp("export-self");
    let _ = std::fs::remove_file(&src);
    write(&src, "[font]\nfamily = \"X\"\n");

    let source = Config::default();
    let err = export_config_document(Some(&src), &source, &src).unwrap_err();
    assert!(matches!(err, ConfigTransferError::SameFile));
    let _ = std::fs::remove_file(&src);
}

#[test]
fn export_rejects_invalid_source_file() {
    // Source file is invalid TOML — export must refuse to copy it.
    let src = tmp("export-invalid");
    let dest = tmp("export-invalid-dest");
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dest);
    write(&src, "not = valid = toml");

    let source = Config::default();
    let err = export_config_document(Some(&src), &source, &dest).unwrap_err();
    assert!(matches!(err, ConfigTransferError::Parse(_)));
    // Destination must not be created.
    assert!(!dest.exists());
    let _ = std::fs::remove_file(&src);
}

// ── VULN-008: api_key must never ride along in an export ─────────────

#[test]
fn export_redacts_top_level_api_key_and_keeps_comments() {
    let src = tmp("export-redact-src");
    let dest = tmp("export-redact-dest");
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dest);
    let content = "# user comment\n[font]\nfamily = \"Menlo\"\n\n[ai]\napi_key = \"sk-secret\"\n";
    write(&src, content);

    export_config_document(Some(&src), &Config::default(), &dest).unwrap();

    let exported = std::fs::read_to_string(&dest).unwrap();
    assert!(!exported.contains("api_key"), "key must be stripped");
    assert!(!exported.contains("sk-secret"));
    assert!(exported.contains("# user comment"), "comments survive");
    assert!(
        exported.contains("family = \"Menlo\""),
        "unrelated lines survive verbatim"
    );
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dest);
}

#[test]
fn export_redacts_inline_ai_table_api_key() {
    // `ai = { api_key = "x" }` — the inline-table form of the same leak.
    let src = tmp("export-inline-src");
    let dest = tmp("export-inline-dest");
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dest);
    write(
        &src,
        "[font]\nfamily = \"Menlo\"\n\nai = { provider = \"ollama\", api_key = \"x\" }\n",
    );

    export_config_document(Some(&src), &Config::default(), &dest).unwrap();

    let exported = std::fs::read_to_string(&dest).unwrap();
    assert!(!exported.contains("api_key"));
    assert!(!exported.contains("\"x\""));
    assert!(
        exported.contains("provider = \"ollama\""),
        "sibling inline keys stay"
    );
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dest);
}

#[test]
fn export_without_api_key_is_byte_identical() {
    // No api_key anywhere ⇒ zero diff: comments and formatting must be
    // preserved byte-for-byte (the redaction path is not taken at all).
    let src = tmp("export-nodiff-src");
    let dest = tmp("export-nodiff-dest");
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dest);
    let content = "# top comment\n[font]\nfamily = \"Menlo\"\n\n[unknown_section]\nkey = \"v\"\n";
    write(&src, content);

    export_config_document(Some(&src), &Config::default(), &dest).unwrap();

    let exported = std::fs::read(&dest).unwrap();
    assert_eq!(exported, content.as_bytes(), "no api_key ⇒ zero diff");
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dest);
}

#[test]
fn export_canonical_output_has_no_api_key() {
    // Canonical branch (no source file): `save_to_path` never writes
    // api_key (save.rs only persists non-default values and the field
    // is skip_serializing), so the canonical export is clean by
    // construction — pinned here so a save-side regression is caught.
    let dest = tmp("export-canonical-redact");
    let _ = std::fs::remove_file(&dest);

    export_config_document(None, &Config::default(), &dest).unwrap();

    let exported = std::fs::read_to_string(&dest).unwrap();
    assert!(!exported.contains("api_key"));
    let _ = std::fs::remove_file(&dest);
}

// ── Import ───────────────────────────────────────────────────────

#[test]
fn import_replaces_config_and_returns_loaded() {
    let import = tmp("import-valid");
    let config = tmp("config-valid");
    let _ = std::fs::remove_file(&import);
    let _ = std::fs::remove_file(&config);
    // Existing config file (will be backed up).
    write(&config, "[font]\nfamily = \"Old\"\n");
    // Import file with new content.
    write(&import, "[font]\nfamily = \"New\"\nsize = 16.0\n");

    let loaded = import_config_document(&import, &config).unwrap();

    // Loaded config reflects the import.
    assert_eq!(loaded.source.font.family, "New");
    assert_eq!(loaded.source.font.size, 16.0);
    // File on disk is the import content.
    let disk = std::fs::read_to_string(&config).unwrap();
    assert!(disk.contains("New"));
    assert!(disk.contains("16.0"));
    // Backup file exists in the same directory. The backup name is
    // `<config-stem>.toml.bak.<secs>`. Since tmp() returns a path
    // without extension, the stem is the full filename. We filter by
    // the `.bak.` suffix to find it regardless of the stem.
    let config_name = config.file_name().unwrap().to_string_lossy().into_owned();
    let parent = config.parent().unwrap();
    let backups: Vec<_> = std::fs::read_dir(parent)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.starts_with(&config_name) && name.contains(".bak.")
        })
        .collect();
    assert!(!backups.is_empty(), "backup file must exist");
    // Backup contains the old content.
    let backup_content = std::fs::read_to_string(backups[0].path()).unwrap();
    assert!(backup_content.contains("Old"));
    // Cleanup.
    let _ = std::fs::remove_file(&import);
    let _ = std::fs::remove_file(&config);
    for b in backups {
        let _ = std::fs::remove_file(b.path());
    }
}

#[test]
fn import_rejects_invalid_toml_without_touching_current() {
    let import = tmp("import-bad-toml");
    let config = tmp("config-bad-toml");
    let _ = std::fs::remove_file(&import);
    let _ = std::fs::remove_file(&config);
    write(&config, "[font]\nfamily = \"Original\"\n");
    write(&import, "not = valid = toml");

    let err = import_config_document(&import, &config).unwrap_err();
    assert!(matches!(err, ConfigTransferError::Parse(_)));

    // Current file is unchanged.
    let disk = std::fs::read_to_string(&config).unwrap();
    assert!(disk.contains("Original"));
    // No backup was created (import failed before backup step).
    let config_name = config.file_name().unwrap().to_string_lossy().into_owned();
    let parent = config.parent().unwrap();
    let backups: Vec<_> = std::fs::read_dir(parent)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.starts_with(&config_name) && name.contains(".bak.")
        })
        .collect();
    assert!(
        backups.is_empty(),
        "no backup should exist on parse failure"
    );
    let _ = std::fs::remove_file(&import);
    let _ = std::fs::remove_file(&config);
}

#[test]
fn import_rejects_invalid_profile_without_touching_current() {
    let import = tmp("import-bad-profile");
    let config = tmp("config-bad-profile");
    let _ = std::fs::remove_file(&import);
    let _ = std::fs::remove_file(&config);
    write(&config, "[font]\nfamily = \"Original\"\n");
    // Reserved profile name "base" is rejected by validate_profile_name.
    write(
        &import,
        "[profiles.base]\n[profiles.base.font]\nfamily = \"X\"\n",
    );

    let err = import_config_document(&import, &config).unwrap_err();
    assert!(matches!(err, ConfigTransferError::Profile(_)));

    // Current file is unchanged.
    let disk = std::fs::read_to_string(&config).unwrap();
    assert!(disk.contains("Original"));
    let _ = std::fs::remove_file(&import);
    let _ = std::fs::remove_file(&config);
}

#[test]
fn import_rejects_missing_file_without_touching_current() {
    let import = tmp("import-missing");
    let config = tmp("config-import-missing");
    let _ = std::fs::remove_file(&import);
    let _ = std::fs::remove_file(&config);
    write(&config, "[font]\nfamily = \"Original\"\n");

    let err = import_config_document(&import, &config).unwrap_err();
    assert!(matches!(err, ConfigTransferError::Read(_)));

    let disk = std::fs::read_to_string(&config).unwrap();
    assert!(disk.contains("Original"));
    let _ = std::fs::remove_file(&config);
}

#[test]
fn import_rejects_same_file() {
    let path = tmp("import-self");
    let _ = std::fs::remove_file(&path);
    write(&path, "[font]\nfamily = \"X\"\n");

    let err = import_config_document(&path, &path).unwrap_err();
    assert!(matches!(err, ConfigTransferError::SameFile));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn import_creates_config_when_no_existing_file() {
    // No existing config file — import is a fresh write, no backup.
    let import = tmp("import-fresh-src");
    let config = tmp("import-fresh-dest");
    let _ = std::fs::remove_file(&import);
    let _ = std::fs::remove_file(&config);
    write(&import, "[font]\nfamily = \"Fresh\"\n");

    let loaded = import_config_document(&import, &config).unwrap();
    assert_eq!(loaded.source.font.family, "Fresh");

    // File exists with the new content.
    assert!(config.exists());
    let disk = std::fs::read_to_string(&config).unwrap();
    assert!(disk.contains("Fresh"));

    // No backup file (there was nothing to back up).
    let parent = config.parent().unwrap();
    let backups: Vec<_> = std::fs::read_dir(parent)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("import-fresh-dest.toml.bak.")
        })
        .collect();
    assert!(backups.is_empty());

    let _ = std::fs::remove_file(&import);
    let _ = std::fs::remove_file(&config);
}

#[test]
fn import_preserves_comments_and_unknown_fields() {
    // The import file's raw bytes are copied verbatim (not re-serialized),
    // so comments and unknown fields survive.
    let import = tmp("import-comments");
    let config = tmp("config-comments");
    let _ = std::fs::remove_file(&import);
    let _ = std::fs::remove_file(&config);
    let content = r#"# my config
[font]
family = "Menlo"

[unknown_section]
key = "preserved"
"#;
    write(&import, content);
    write(&config, "[font]\nfamily = \"Old\"\n");

    import_config_document(&import, &config).unwrap();

    let disk = std::fs::read_to_string(&config).unwrap();
    assert!(disk.contains("# my config"));
    assert!(disk.contains("[unknown_section]"));
    assert!(disk.contains("preserved"));

    let _ = std::fs::remove_file(&import);
    let _ = std::fs::remove_file(&config);
}

#[test]
fn import_preserves_profiles_and_active() {
    // The import file's profile table + active_profile survive the
    // verbatim copy.
    let import = tmp("import-profiles");
    let config = tmp("config-profiles");
    let _ = std::fs::remove_file(&import);
    let _ = std::fs::remove_file(&config);
    let content = r#"
active_profile = "work"

[font]
family = "Base"

[profiles.work.font]
family = "Profile"
"#;
    write(&import, content);
    write(&config, "[font]\nfamily = \"Old\"\n");

    let loaded = import_config_document(&import, &config).unwrap();
    assert_eq!(loaded.source.active_profile.as_deref(), Some("work"));
    assert!(loaded.source.profiles.contains_key("work"));
    assert_eq!(loaded.effective.font.family, "Profile");

    let _ = std::fs::remove_file(&import);
    let _ = std::fs::remove_file(&config);
}

// ── Failure-path: backup and write failures ──────────────────────
//
// V15 plan §7.4 requires: "backup 成功、backup 失败、temp write 失败、
// rename 失败均有测试". The "backup 成功" case is covered by
// `import_replaces_config_and_returns_loaded` (it asserts the backup
// file exists). Below we cover backup failure and temp-write failure.
//
// Rename failure is deliberately not isolated: on POSIX, `rename(2)`
// and `write(2)` to the same parent directory check the same write-
// permission bit, so a directory that allows the temp write to
// succeed also allows the rename. Triggering `rename(2)` failure
// without mocking would require either a cross-device rename (EXDEV)
// — which needs two filesystems — or removing the temp file between
// the write and the rename (a race condition). The cleanup path on
// rename failure is identical to the temp-write cleanup (both
// `remove_file(&tmp)` + return `Write`), so the temp-write test
// implicitly exercises the same invariant: "current file untouched".

/// Backup failure: the parent dir of `config_path` is read-only, so
/// `std::fs::copy` to create the backup file fails with EACCES. The
/// import must abort at the `Backup` step, leaving the current file
/// untouched and writing no temp file.
#[test]
fn import_backup_failure_leaves_current_untouched() {
    // Use a dedicated subdirectory so we can chmod it without
    // affecting other tests in std::env::temp_dir().
    let dir = std::env::temp_dir().join(format!(
        "weft-transfer-backup-fail-{}-{}",
        std::process::id(),
        std::time::SystemTime::UNIX_EPOCH
            .elapsed()
            .unwrap_or_default()
            .as_nanos(),
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let import = dir.join("import.toml");
    let config = dir.join("config.toml");
    std::fs::write(&import, "[font]\nfamily = \"New\"\n").unwrap();
    std::fs::write(&config, "[font]\nfamily = \"Original\"\n").unwrap();

    // Make the parent dir read-only (0555). Now `std::fs::copy`
    // cannot create the backup file in it.
    set_readonly(&dir);

    let err = import_config_document(&import, &config).unwrap_err();
    // Either Backup or Write — both prove the import aborted at the
    // filesystem boundary. On most POSIX systems the backup is the
    // first operation that needs to create a file in the read-only
    // dir, so we expect Backup.
    assert!(
        matches!(
            err,
            ConfigTransferError::Backup(_) | ConfigTransferError::Write(_)
        ),
        "expected Backup or Write error on read-only parent, got {err:?}"
    );

    // Restore writability so the cleanup can run.
    set_writable(&dir);

    // Current file is unchanged.
    let disk = std::fs::read_to_string(&config).unwrap();
    assert!(disk.contains("Original"));

    // No temp file left behind.
    assert!(!config.with_extension("toml.tmp").exists());

    let _ = std::fs::remove_dir_all(&dir);
}

/// Temp write failure: the `.tmp` file is pre-created as a read-only
/// file (chmod 0444), so `std::fs::write` to it fails with EACCES.
/// The backup succeeds (parent dir is writable), then the write
/// aborts. The current file must be unchanged, the backup must exist,
/// and no temp file must remain.
#[test]
fn import_temp_write_failure_leaves_current_untouched() {
    let dir = std::env::temp_dir().join(format!(
        "weft-transfer-write-fail-{}-{}",
        std::process::id(),
        std::time::SystemTime::UNIX_EPOCH
            .elapsed()
            .unwrap_or_default()
            .as_nanos(),
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let import = dir.join("import.toml");
    let config = dir.join("config.toml");
    std::fs::write(&import, "[font]\nfamily = \"New\"\n").unwrap();
    std::fs::write(&config, "[font]\nfamily = \"Original\"\n").unwrap();

    // Pre-create the .tmp file as read-only. `with_extension` on
    // "config.toml" replaces "toml" → "config.toml.tmp".
    let tmp = config.with_extension("toml.tmp");
    std::fs::write(&tmp, "stale").unwrap();
    set_readonly(&tmp);

    let err = import_config_document(&import, &config).unwrap_err();
    assert!(
        matches!(err, ConfigTransferError::Write(_)),
        "expected Write error on read-only .tmp, got {err:?}"
    );

    // The cleanup removed the .tmp file — no need to restore its
    // permissions. The dir is still writable, so `remove_dir_all`
    // at the end will succeed.

    // Current file is unchanged.
    let disk = std::fs::read_to_string(&config).unwrap();
    assert!(disk.contains("Original"));
    assert!(!disk.contains("New"));

    // The cleanup removed the .tmp file.
    assert!(!tmp.exists(), "temp file must be removed on write failure");

    // A backup was created (backup step succeeded before the write).
    let parent = config.parent().unwrap();
    let config_name = config.file_name().unwrap().to_string_lossy().into_owned();
    let backups: Vec<_> = std::fs::read_dir(parent)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.starts_with(&config_name) && name.contains(".bak.")
        })
        .collect();
    assert!(
        !backups.is_empty(),
        "backup must exist on temp-write failure"
    );
    // Backup contains the old content.
    let backup_content = std::fs::read_to_string(backups[0].path()).unwrap();
    assert!(backup_content.contains("Original"));

    let _ = std::fs::remove_dir_all(&dir);
}

// ── VULN-008: transfer write paths land at 0600 ──────────────────────

#[test]
fn export_lands_0600() {
    use std::os::unix::fs::PermissionsExt;
    let src = tmp("perm-export-src");
    let dest = tmp("perm-export-dest");
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dest);
    write(&src, "[font]\nfamily = \"Menlo\"\n");

    export_config_document(Some(&src), &Config::default(), &dest).unwrap();

    let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "export output must be 0600");
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dest);
}

#[test]
fn import_lands_0600_and_backup_is_0600() {
    use std::os::unix::fs::PermissionsExt;
    let import = tmp("perm-import-src");
    let config = tmp("perm-import-dest");
    let _ = std::fs::remove_file(&import);
    let _ = std::fs::remove_file(&config);
    write(&config, "[font]\nfamily = \"Old\"\n");
    write(&import, "[font]\nfamily = \"New\"\n");

    import_config_document(&import, &config).unwrap();

    let mode = std::fs::metadata(&config).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "imported config must be 0600");
    // The backup carries the old (possibly key-bearing) content and is
    // never cleaned up — it must be 0600 too, not the source's mode.
    let config_name = config.file_name().unwrap().to_string_lossy().into_owned();
    let parent = config.parent().unwrap();
    let backups: Vec<_> = std::fs::read_dir(parent)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.starts_with(&config_name) && name.contains(".bak.")
        })
        .collect();
    assert!(!backups.is_empty(), "backup file must exist");
    let backup_mode = backups[0].metadata().unwrap().permissions().mode();
    assert_eq!(backup_mode & 0o777, 0o600, "backup must be 0600");
    let _ = std::fs::remove_file(&import);
    let _ = std::fs::remove_file(&config);
    for b in backups {
        let _ = std::fs::remove_file(b.path());
    }
}

// ── redact_api_keys (pure) ───────────────────────────────────────────

#[test]
fn redact_api_keys_covers_top_level_nested_inline_and_dotted() {
    let redact = |text: &str| {
        let mut doc = toml_edit::DocumentMut::from_str(text).unwrap();
        redact_api_keys(&mut doc)
    };
    // Top-level table + nested profile table in one document.
    // NOTE: `[profiles.work.ai]` cannot appear in a REAL config (the
    // schema rejects `ai` inside a profile via deny_unknown_fields) —
    // exercised here directly because export's validation would
    // fail-closed on it before redaction ever runs.
    assert_eq!(
        redact("[ai]\napi_key = \"a\"\n\n[profiles.work.ai]\napi_key = \"b\"\n"),
        2,
        "top-level and nested ai tables both redacted"
    );
    assert_eq!(
        redact("ai = { api_key = \"x\", provider = \"ollama\" }\n[font]\nfamily = \"M\"\n"),
        1,
        "inline-table form"
    );
    assert_eq!(redact("ai.api_key = \"d\"\n"), 1, "dotted-key form");
    assert_eq!(
        redact("[font]\nfamily = \"M\"\napi_key = \"untouched\"\n"),
        0,
        "api_key outside an `ai` table is not ours to touch"
    );
}

#[test]
fn redact_api_keys_removes_empty_and_whitespace_keys() {
    let mut doc = toml_edit::DocumentMut::from_str("[ai]\napi_key = \"\"\n").unwrap();
    assert_eq!(redact_api_keys(&mut doc), 1, "empty key is removed");
    let mut doc = toml_edit::DocumentMut::from_str("[ai]\napi_key = \"   \"\n").unwrap();
    assert_eq!(redact_api_keys(&mut doc), 1, "whitespace key is removed");
    assert!(
        doc.to_string().contains("[ai]"),
        "the section header itself stays"
    );
}

/// Helper: set a path to read-only (mode 0555 for dirs, 0444 for files).
fn set_readonly(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let is_dir = path.is_dir();
    let mode = if is_dir { 0o555 } else { 0o444 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// Helper: restore writability (mode 0755 for dirs, 0644 for files).
fn set_writable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let is_dir = path.is_dir();
    let mode = if is_dir { 0o755 } else { 0o644 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}
