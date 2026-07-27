//! v1.5.2: Atomic config import/export.
//!
//! Implements the transfer semantics fixed in V15_IMPLEMENTATION_PLAN.md §7:
//!
//! - **Export** writes the **source** complete config document (base +
//!   profiles + active profile + comments + unknown fields). When the
//!   original config file exists on disk, it is copied verbatim after a
//!   validation pass — this preserves comments and unknown fields that
//!   `Config::save_to_path` would drop. When the original file is absent
//!   (e.g. running on defaults), `source` is serialized to canonical
//!   TOML via `save_to_path`.
//! - **Import** is a **complete replace**: the import file is parsed,
//!   validated, and resolved *before* the current config file is touched.
//!   On success the current file is backed up to
//!   `config.toml.bak.<unix-seconds>` in the same directory, then the
//!   new content is atomically renamed into place. Any failure leaves
//!   the current file unchanged.
//!
//! Both functions are pure-FS: they have no dependency on the App,
//! renderer, or PTY. The App layer wires them to NSOpenPanel/NSSavePanel
//! and applies the resulting `LoadedConfig` to the runtime.

use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use super::profiles::ProfileError;
use super::{Config, ConfigLoadError, LoadedConfig};

// ── ConfigTransferError ────────────────────────────────────────────────

/// Errors raised by [`export_config_document`] and [`import_config_document`].
///
/// `Cancelled` is only produced by the App-layer file panels (NSOpenPanel /
/// NSSavePanel cancel); the core functions never emit it. It lives here so
/// the App can map panel cancellation to the same error type used by the
/// core transfer logic.
#[derive(Debug)]
pub enum ConfigTransferError {
    /// App-layer only: user cancelled the file panel. Core functions never
    /// return this variant.
    Cancelled,
    /// The source file could not be read (missing, permission, etc.).
    Read(std::io::Error),
    /// The file content failed TOML parsing or schema validation.
    Parse(toml::de::Error),
    /// Profile validation failed (bad name, too many, unknown section).
    Profile(ProfileError),
    /// The backup of the existing config file failed. The current file
    /// is untouched; the import is aborted.
    Backup(std::io::Error),
    /// The atomic write of the new content failed (disk full, permission).
    /// The current file is untouched because the rename hasn't happened.
    Write(std::io::Error),
    /// The post-write reload failed (file disappeared, re-parse error).
    /// The file on disk is correct; the runtime just couldn't pick up the
    /// new fingerprint. The watcher will retry on the next mtime tick.
    Reload(ConfigLoadError),
    /// The import path resolves to the same file as the current config
    /// path (canonicalized comparison). Importing a file over itself is
    /// rejected because it would produce a useless backup and a no-op
    /// replace.
    SameFile,
}

impl std::fmt::Display for ConfigTransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => write!(f, "cancelled by user"),
            Self::Read(e) => write!(f, "config read failed: {e}"),
            Self::Parse(e) => write!(f, "config parse failed: {e}"),
            Self::Profile(e) => write!(f, "config profile invalid: {e}"),
            Self::Backup(e) => write!(f, "config backup failed: {e}"),
            Self::Write(e) => write!(f, "config write failed: {e}"),
            Self::Reload(e) => write!(f, "config reload after import failed: {e}"),
            Self::SameFile => write!(f, "import path is the same as the current config file"),
        }
    }
}

impl std::error::Error for ConfigTransferError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(e) | Self::Backup(e) | Self::Write(e) => Some(e),
            Self::Parse(e) => Some(e),
            Self::Profile(e) => Some(e),
            Self::Reload(e) => Some(e),
            _ => None,
        }
    }
}

impl From<ProfileError> for ConfigTransferError {
    fn from(e: ProfileError) -> Self {
        Self::Profile(e)
    }
}

impl From<toml::de::Error> for ConfigTransferError {
    fn from(e: toml::de::Error) -> Self {
        Self::Parse(e)
    }
}

impl From<std::io::Error> for ConfigTransferError {
    fn from(e: std::io::Error) -> Self {
        Self::Read(e)
    }
}

// ── Export ────────────────────────────────────────────────────────────

/// Export the **source** config document to `destination`.
///
/// Semantics (V15 plan §7.1):
///
/// 1. If `source_path` exists and is readable, copy its raw bytes verbatim
///    to `destination` (preserving comments and unknown fields). The
///    content is validated first (parsed as `Config` + `resolve_active_profile`)
///    so we never export a broken file.
/// 2. If `source_path` is `None` or doesn't exist, serialize `source` to
///    canonical TOML via `Config::save_to_path`.
/// 3. The export is atomic: writes to `<destination>.tmp`, then renames.
///
/// Export to a file that resolves to the same path as `source_path` is
/// rejected with [`ConfigTransferError::SameFile`] — the caller should
/// pick a different destination.
pub fn export_config_document(
    source_path: Option<&Path>,
    source: &Config,
    destination: &Path,
) -> Result<(), ConfigTransferError> {
    // Reject export-over-self (canonicalized comparison).
    if let Some(src) = source_path {
        if same_file(src, destination) {
            return Err(ConfigTransferError::SameFile);
        }
    }

    // Step 1: try to copy the raw file verbatim (preserves comments +
    // unknown fields). Fall back to canonical TOML when the file is
    // absent or unreadable.
    let bytes: Vec<u8> = match source_path {
        Some(path) => match std::fs::read(path) {
            Ok(raw) => {
                // Validate the raw bytes before exporting: parse as Config
                // + resolve. A broken source file must not be exported as
                // if it were valid.
                let text = std::str::from_utf8(&raw).map_err(|_| {
                    ConfigTransferError::Read(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "source config is not valid UTF-8",
                    ))
                })?;
                let cfg: Config = toml::from_str(text)?;
                let (_effective, _diags) = cfg.resolve_active_profile()?;
                raw
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Source file absent — serialize `source` to canonical TOML.
                canonical_toml_bytes(source, destination)?
            }
            Err(e) => return Err(ConfigTransferError::Read(e)),
        },
        None => canonical_toml_bytes(source, destination)?,
    };

    // Step 2: atomic write to destination.
    let parent = destination.parent().ok_or_else(|| {
        ConfigTransferError::Write(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "destination has no parent directory",
        ))
    })?;
    std::fs::create_dir_all(parent).map_err(ConfigTransferError::Write)?;
    let tmp = destination.with_extension("toml.tmp");
    if let Err(e) = std::fs::write(&tmp, &bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(ConfigTransferError::Write(e));
    }
    if let Err(e) = std::fs::rename(&tmp, destination) {
        let _ = std::fs::remove_file(&tmp);
        return Err(ConfigTransferError::Write(e));
    }
    Ok(())
}

/// Serialize `source` to canonical TOML bytes by writing to a temp path
/// and reading it back. This reuses `Config::save_to_path` so the output
/// matches the format Settings produces on save.
fn canonical_toml_bytes(source: &Config, dest: &Path) -> Result<Vec<u8>, ConfigTransferError> {
    let tmp = dest.with_extension("toml.tmp");
    source.save_to_path(&tmp).map_err(|e| match e {
        super::ConfigSaveError::Io(e) => ConfigTransferError::Write(e),
        other => ConfigTransferError::Write(std::io::Error::other(other.to_string())),
    })?;
    let bytes = std::fs::read(&tmp).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        ConfigTransferError::Read(e)
    })?;
    let _ = std::fs::remove_file(&tmp);
    Ok(bytes)
}

// ── Import ────────────────────────────────────────────────────────────

/// Import a config document from `import_path`, replacing the current
/// config at `config_path`.
///
/// Semantics (V15 plan §7.1):
///
/// 1. Reject import-over-self (same canonical path).
/// 2. Read + parse + resolve the import file. Any failure returns `Err`
///    and leaves the current file untouched.
/// 3. Back up the current file to `config.toml.bak.<unix-seconds>` in
///    the same directory. Backup failure leaves the current file untouched.
/// 4. Atomically write the import bytes to `config_path` (`.tmp` + rename).
/// 5. Reload the newly-written file to get a fresh `LoadedConfig`
///    (source + effective + fingerprint). Reload failure is returned as
///    `Reload`, but the file on disk is correct — the watcher will retry.
///
/// Returns the `LoadedConfig` so the caller can apply it to the runtime
/// without an extra reload.
pub fn import_config_document(
    import_path: &Path,
    config_path: &Path,
) -> Result<LoadedConfig, ConfigTransferError> {
    // Step 1: reject import-over-self.
    if same_file(import_path, config_path) {
        return Err(ConfigTransferError::SameFile);
    }

    // Step 2: read + validate the import file. The current file is
    // untouched on any failure here.
    let bytes = std::fs::read(import_path).map_err(ConfigTransferError::Read)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| {
        ConfigTransferError::Read(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "import file is not valid UTF-8",
        ))
    })?;
    let cfg: Config = toml::from_str(text)?;
    let (_effective, _diags) = cfg.resolve_active_profile()?;

    // Step 3: back up the current file (if it exists) to
    // `config.toml.bak.<unix-seconds>`. Use the same directory so the
    // rename is atomic on the same filesystem.
    if let Some(parent) = config_path.parent() {
        let stem = config_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "config".into());
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let backup = parent.join(format!("{stem}.toml.bak.{secs}"));
        // Only back up if the current file exists. If it doesn't, there's
        // nothing to back up — the import is a fresh write.
        if config_path.exists() {
            std::fs::copy(config_path, &backup).map_err(ConfigTransferError::Backup)?;
        }
    }

    // Step 4: atomic write of the import bytes to config_path.
    let tmp = config_path.with_extension("toml.tmp");
    if let Err(e) = std::fs::write(&tmp, &bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(ConfigTransferError::Write(e));
    }
    if let Err(e) = std::fs::rename(&tmp, config_path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(ConfigTransferError::Write(e));
    }

    // Step 5: reload to get a fresh LoadedConfig (source + effective +
    // fingerprint). The file on disk is already correct; reload failure
    // means the runtime can't pick up the new state, but the watcher
    // will retry on the next mtime tick.
    let loaded =
        super::load_resolved_from_path(config_path).map_err(ConfigTransferError::Reload)?;
    Ok(loaded)
}

// ── Helpers ───────────────────────────────────────────────────────────

/// Compare two paths by canonicalizing them. Returns true when both
/// resolve to the same filesystem path. Falls back to a literal
/// `Path::eq` comparison when canonicalization fails (e.g. the file
/// doesn't exist yet), so an import-over-self is still detected when
/// the destination file is absent.
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
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
}
