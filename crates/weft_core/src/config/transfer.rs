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

// Tests live in the gate-exempt sibling module (repo test-module
// convention, pty/tests.rs precedent) so inline test lines stay out of
// the production-file budget.
#[cfg(test)]
#[path = "transfer/tests.rs"]
mod tests;
