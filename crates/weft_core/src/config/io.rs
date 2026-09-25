//! v1.5.0: Safe config loading with raw/effective separation.
//!
//! [`LoadedConfig`] carries three views of the same on-disk document:
//!
//! - `source` — exactly what the TOML says, the only form allowed to be
//!   persisted. Profile overlays are **not** flattened into it.
//! - `effective` — `source` clone + active profile overlay, used by the
//!   runtime/renderer. Never saved.
//! - `fingerprint` — process-local hash of the raw bytes, used by the
//!   live-reload watcher to skip no-op reloads (see v1.5.3).
//!
//! Errors from `load_resolved*` never produce a `LoadedConfig`. The legacy
//! [`super::Config::load`] stays as a startup-compat shim that returns
//! defaults on any error.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use super::profiles::ConfigDiagnostic;
use super::{Config, ProfileError};

// ── Scrollback normalization (v1.11.2 X3 / PLAN_v1112 §5) ──────────────

/// Lower bound for `[scrollback] lines`. Below this a terminal is unusable
/// (scroll commands do nothing), so tiny/zero user values are lifted to it.
pub const SCROLLBACK_MIN_LINES: usize = 100;
/// Upper bound for `[scrollback] lines`. A hostile or typo'd config value
/// (`lines = 10^9`) would otherwise let each pane's scrollback grow without
/// a practical memory ceiling; 1M lines × ~cols cells is already far beyond
/// interactive use.
pub const SCROLLBACK_MAX_LINES: usize = 1_000_000;

/// Clamp `config.scrollback.lines` into `[SCROLLBACK_MIN_LINES,
/// SCROLLBACK_MAX_LINES]`, warning when the on-disk value was out of range.
///
/// v1.11.2 X3 canonical clamp point: called on the parsed `source` and again
/// on the profile-resolved `effective` config (a profile override can
/// reintroduce an out-of-range value after the source was normalized).
pub fn normalize_scrollback(config: &mut Config) {
    let raw = config.scrollback.lines;
    if !(SCROLLBACK_MIN_LINES..=SCROLLBACK_MAX_LINES).contains(&raw) {
        tracing::warn!(
            raw,
            clamped = raw.clamp(SCROLLBACK_MIN_LINES, SCROLLBACK_MAX_LINES),
            "[scrollback] lines out of range; clamping"
        );
    }
    // PLAN_v1112 §5 formula: min first, then max — lifts small values to the
    // floor and caps large ones at the ceiling.
    config.scrollback.lines = raw.clamp(SCROLLBACK_MIN_LINES, SCROLLBACK_MAX_LINES);
}

/// PLAN_v11217 §3.5 (T4): clamp `config.blocks.output_cap_mib` into
/// `[OUTPUT_CAP_MIN_MIB, OUTPUT_CAP_MAX_MIB]`, warning when the on-disk value
/// was out of range. Canonical clamp point: called on the parsed `source` and
/// again on the profile-resolved `effective` config (same shape as
/// [`normalize_scrollback`]) — a profile override can reintroduce an
/// out-of-range value after the source was normalized.
pub fn normalize_blocks(config: &mut Config) {
    let raw = config.blocks.output_cap_mib;
    let clamped = raw.clamp(
        crate::blocks::OUTPUT_CAP_MIN_MIB,
        crate::blocks::OUTPUT_CAP_MAX_MIB,
    );
    if clamped != raw {
        tracing::warn!(
            raw,
            clamped,
            "[blocks] output_cap_mib out of range; clamping"
        );
    }
    config.blocks.output_cap_mib = clamped;
}

// ── LoadedConfig ───────────────────────────────────────────────────────

/// The result of loading and resolving a config document.
///
/// Construct via [`load_resolved`] / [`load_resolved_from_path`]. The fields
/// are pub so callers (ConfigState, Settings, import flow) can read them,
/// but only `source` may be passed to a save function.
#[derive(Clone, Debug)]
pub struct LoadedConfig {
    /// Raw config as parsed from disk. The only form that may be persisted.
    /// Profile overlays have **not** been applied.
    pub source: Config,
    /// `source` clone + active profile overlay. Read by the runtime/renderer.
    /// Never persist this — doing so would flatten profile overrides into
    /// the base document (see V15 plan §3.2).
    pub effective: Config,
    /// Process-local fingerprint of the raw file bytes. Equal across two
    /// loads iff the bytes were identical. Used by the reload watcher to
    /// skip duplicate applies.
    pub fingerprint: u64,
    /// Non-fatal issues (e.g. active profile name not found). Empty on the
    /// happy path. Surfaced to Settings / status hint.
    pub diagnostics: Vec<ConfigDiagnostic>,
}

// ── ConfigLoadError ────────────────────────────────────────────────────

/// Errors raised by the v1.5 load API. None of these produce a
/// `LoadedConfig`; callers must keep their previous `LoadedConfig` (or
/// defaults) on `Err`.
#[derive(Debug)]
pub enum ConfigLoadError {
    /// `Config::config_path()` returned `None` (no HOME / XDG_CONFIG_HOME).
    NoPath,
    /// File read failed. Carries the raw `io::Error`.
    Io(std::io::Error),
    /// TOML parse error. The file exists but isn't valid TOML / doesn't
    /// match the `Config` schema.
    Parse(toml::de::Error),
    /// Profile validation failed (bad name, too many, unknown section).
    /// Carries the offending [`ProfileError`].
    Profile(ProfileError),
}

impl std::fmt::Display for ConfigLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoPath => write!(f, "no config path: HOME and XDG_CONFIG_HOME are both unset"),
            Self::Io(e) => write!(f, "config read failed: {e}"),
            Self::Parse(e) => write!(f, "config parse failed: {e}"),
            Self::Profile(e) => write!(f, "config profile invalid: {e}"),
        }
    }
}

impl std::error::Error for ConfigLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Parse(e) => Some(e),
            Self::Profile(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ConfigLoadError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<toml::de::Error> for ConfigLoadError {
    fn from(e: toml::de::Error) -> Self {
        Self::Parse(e)
    }
}

impl From<ProfileError> for ConfigLoadError {
    fn from(e: ProfileError) -> Self {
        Self::Profile(e)
    }
}

// ── Fingerprint ────────────────────────────────────────────────────────

/// Compute a stable fingerprint of the raw config bytes.
///
/// Uses `DefaultHasher` (SipHash-1-3 with fixed zero keys) so the same input
/// always produces the same output — both within a process and across
/// restarts. This is fine for our use case (live-reload dedup): we only
/// compare fingerprints against each other, never against a stored value.
/// Cheap: a 4 KiB config file hashes in well under 1 ms.
pub fn fingerprint_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

// ── Load API ───────────────────────────────────────────────────────────

/// Load and resolve the config from the well-known path.
///
/// Returns `Err(NoPath)` when neither `HOME` nor `XDG_CONFIG_HOME` is set.
/// Parse / profile errors are `Err` — callers must keep their previous
/// config. This intentionally differs from `Config::load()`, which silently
/// returns defaults on any error (kept only as a startup-compat shim).
pub fn load_resolved() -> Result<LoadedConfig, ConfigLoadError> {
    let path = Config::config_path().ok_or(ConfigLoadError::NoPath)?;
    load_resolved_from_path(&path)
}

/// Load and resolve the config from an explicit path.
///
/// Reads the raw bytes, fingerprints them, parses into `Config` (the
/// `source`), then calls `resolve_active_profile` to produce `effective`
/// plus any non-fatal diagnostics.
pub fn load_resolved_from_path(path: &Path) -> Result<LoadedConfig, ConfigLoadError> {
    let bytes = std::fs::read(path)?;
    let fingerprint = fingerprint_bytes(&bytes);
    let text = std::str::from_utf8(&bytes).map_err(|e| {
        // Wrap as a parse error so callers see "invalid UTF-8" rather than
        // an opaque io error.
        ConfigLoadError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    })?;
    let mut source: Config = toml::from_str(text)?;
    // v1.11.2 X3: normalize before the profile overlay so the stored source
    // view is always in range.
    normalize_scrollback(&mut source);
    // PLAN_v11217 §3.5 (T4): same canonical clamp point for output_cap_mib.
    normalize_blocks(&mut source);
    let (mut effective, diagnostics) = source.resolve_active_profile()?;
    // A profile's [scrollback] override is applied inside
    // resolve_active_profile and can reintroduce an out-of-range value.
    normalize_scrollback(&mut effective);
    normalize_blocks(&mut effective);
    Ok(LoadedConfig {
        source,
        effective,
        fingerprint,
        diagnostics,
    })
}

// ── Atomic write helper ─────────────────────────────────────────────────

/// Atomically write `bytes` to `path`: create `<path>.tmp` at 0600, write,
/// then rename. The temp file lives in the same directory so the
/// rename is atomic on the same filesystem. Returns the path that was
/// written (== `path`).
///
/// Currently test-only — `Config::save_to_path` and the import flow each
/// own their own tmp+rename sequence — but kept as the shared helper for
/// future config writers, so the 0600 hardening (VULN-008) lives here too.
/// On error the destination is guaranteed unchanged, and the `.tmp` file is
/// removed (best-effort) so a failed write does not leak a partial temp file.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<PathBuf, std::io::Error> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path has no parent directory",
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let tmp = path.with_extension("toml.tmp");
    // VULN-008: create the tmp at 0600, eliminating the 0644 window for
    // newly created files. `mode` only applies at creation time — a stale
    // 0644 `.tmp` left by an older version is still caught by the chmod
    // below.
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut file| file.write_all(bytes));
    if let Err(e) = written {
        // Best-effort cleanup so a failed write doesn't leave a partial
        // `.tmp` behind. Ignore the remove error — the original write
        // failure is the one we want to surface.
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // VULN-008: chmod BEFORE the rename — rename(2) keeps the tmp inode's
    // permissions, so a 0600 tmp makes the 0644 window zero-length. Kept as
    // the fallback for stale tmp files created by older versions.
    if let Err(e) = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // Cleanup mirrors the failure branches above; the post-chmod tmp is
    // already 0600, so this is consistency, not exposure control.
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(path.to_path_buf())
}

#[cfg(test)]
#[path = "io_tests.rs"]
mod tests;
