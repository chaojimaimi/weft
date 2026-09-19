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

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::str::FromStr;
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
                // VULN-008 (second half): the legacy `api_key` is ignored by
                // the app (Ollama-only, no key needed) but still deserialized
                // for compat — it must never ride along in a shareable
                // export. Redaction is targeted (any `ai` table) via
                // toml_edit, so a no-key config stays byte-identical.
                let mut doc = toml_edit::DocumentMut::from_str(text).map_err(|e| {
                    // `Parse` carries toml::de::Error; wrap the toml_edit
                    // error. Fail CLOSED: a document we cannot redact is
                    // never exported as raw bytes.
                    ConfigTransferError::Parse(<toml::de::Error as serde::de::Error>::custom(
                        e.to_string(),
                    ))
                })?;
                let redacted = redact_api_keys(&mut doc);
                if redacted > 0 {
                    tracing::warn!(
                        count = redacted,
                        "exported config redacted api_key value(s)"
                    );
                    doc.to_string().into_bytes()
                } else {
                    raw
                }
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
    // VULN-008: create the tmp at 0600, eliminating the 0644 window for
    // newly created files. `mode` only applies at creation time — a stale
    // 0644 `.tmp` from an older version is still caught by the chmod below.
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut file| file.write_all(&bytes));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(ConfigTransferError::Write(e));
    }
    // VULN-008: land the export at 0600, BEFORE the rename (rename(2) keeps
    // the tmp inode's permissions). Failure is downgraded to a warning: a
    // user-chosen destination (e.g. exFAT) may not support POSIX modes, and
    // a successful export beats permission hardening here.
    if let Err(e) = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)) {
        tracing::warn!(
            error = %e,
            "could not chmod exported config to 0600; exporting anyway"
        );
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
            // Backups are never cleaned up and can carry a legacy api_key —
            // keep them 0600 like the live config (`fs::copy` follows the
            // source's mode, which may be looser). VULN-008.
            if let Err(e) =
                std::fs::set_permissions(&backup, std::fs::Permissions::from_mode(0o600))
            {
                let _ = std::fs::remove_file(&backup);
                return Err(ConfigTransferError::Backup(e));
            }
        }
    }

    // Step 4: atomic write of the import bytes to config_path.
    let tmp = config_path.with_extension("toml.tmp");
    // VULN-008: create the tmp at 0600, eliminating the 0644 window for
    // newly created files. `mode` only applies at creation time — a stale
    // 0644 `.tmp` from an older version is still caught by the chmod below.
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut file| file.write_all(&bytes));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(ConfigTransferError::Write(e));
    }
    // VULN-008: chmod BEFORE the rename — rename(2) keeps the tmp inode's
    // permissions, so the imported config never lands world-readable.
    if let Err(e) = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)) {
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

// ── Export redaction (VULN-008) ───────────────────────────────────────

/// Remove every `api_key` key found under a table named `ai` from `doc`,
/// returning the number of keys removed. Pure: touches only `doc`.
///
/// Recurses through nested tables so `[profiles.<name>.ai]` is covered, and
/// handles both inline (`ai = { api_key = "x" }`) and dotted (`ai.api_key =
/// "x"`) forms. Empty/whitespace keys are removed too (harmless and tidier).
/// Nested recursion is REQUIRED: `save_to_path` preserves an existing
/// `[profiles.*.ai] api_key` on disk, so real config files carry it.
fn redact_api_keys(doc: &mut toml_edit::DocumentMut) -> usize {
    redact_ai_keys_in_table(doc.as_table_mut())
}

/// Walk one table, redacting under `ai`-named children and recursing
/// everywhere else.
fn redact_ai_keys_in_table(table: &mut toml_edit::Table) -> usize {
    let mut removed = 0;
    for (key, item) in table.iter_mut() {
        if key == "ai" {
            removed += redact_ai_item(item);
        } else {
            removed += redact_descendant(item);
        }
    }
    removed
}

/// `item` is the value of an `ai` key: strip its `api_key`, then keep
/// recursing (an `ai` table can nest further `ai` tables).
fn redact_ai_item(item: &mut toml_edit::Item) -> usize {
    let mut removed = 0;
    if let Some(t) = item.as_table_mut() {
        // Covers `[ai]`, `[profiles.*.ai]`, and the dotted form — toml_edit
        // models `ai.api_key = "x"` as a dotted table.
        if t.remove("api_key").is_some() {
            removed += 1;
        }
        removed += redact_ai_keys_in_table(t);
    } else if let Some(inline) = item.as_value_mut().and_then(|v| v.as_inline_table_mut()) {
        if inline.remove("api_key").is_some() {
            removed += 1;
        }
        removed += redact_ai_keys_in_inline(inline);
    }
    removed
}

/// Walk one non-`ai` item, recursing into any nested tables.
///
/// Premise: TOML array-of-tables entries (`[[x]]`, an `ArrayOfTables`) are
/// NOT recursed into — they are neither a table nor an inline value here,
/// so an `ai` table nested under one would be missed. The current `Config`
/// schema has no array-of-tables field that can carry an `ai` table (and
/// the document is deserialized into `Config` before redaction anyway); if
/// such a field is ever added, this function needs an array-of-tables
/// branch.
fn redact_descendant(item: &mut toml_edit::Item) -> usize {
    let mut removed = 0;
    if let Some(t) = item.as_table_mut() {
        removed += redact_ai_keys_in_table(t);
    } else if let Some(inline) = item.as_value_mut().and_then(|v| v.as_inline_table_mut()) {
        removed += redact_ai_keys_in_inline(inline);
    }
    removed
}

/// Inline-table twin of [`redact_ai_keys_in_table`].
fn redact_ai_keys_in_inline(inline: &mut toml_edit::InlineTable) -> usize {
    let mut removed = 0;
    for (key, value) in inline.iter_mut() {
        let is_ai = key == "ai";
        if let Some(nested) = value.as_inline_table_mut() {
            if is_ai && nested.remove("api_key").is_some() {
                removed += 1;
            }
            removed += redact_ai_keys_in_inline(nested);
        }
    }
    removed
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

    // ── VULN-008: api_key must never ride along in an export ─────────────

    #[test]
    fn export_redacts_top_level_api_key_and_keeps_comments() {
        let src = tmp("export-redact-src");
        let dest = tmp("export-redact-dest");
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dest);
        let content =
            "# user comment\n[font]\nfamily = \"Menlo\"\n\n[ai]\napi_key = \"sk-secret\"\n";
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
        let content =
            "# top comment\n[font]\nfamily = \"Menlo\"\n\n[unknown_section]\nkey = \"v\"\n";
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
}
