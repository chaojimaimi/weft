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
    let (mut effective, diagnostics) = source.resolve_active_profile()?;
    // A profile's [scrollback] override is applied inside
    // resolve_active_profile and can reintroduce an out-of-range value.
    normalize_scrollback(&mut effective);
    Ok(LoadedConfig {
        source,
        effective,
        fingerprint,
        diagnostics,
    })
}

// ── Atomic write helper ─────────────────────────────────────────────────

/// Atomically write `bytes` to `path`: write to `<path>.tmp` then rename.
/// The temp file lives in the same directory so the rename is atomic on the
/// same filesystem. Returns the path that was written (== `path`).
///
/// Used by save and import flows. On error the destination is guaranteed
/// unchanged, and the `.tmp` file is removed (best-effort) so a failed
/// write does not leak a partial temp file.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<PathBuf, std::io::Error> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path has no parent directory",
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let tmp = path.with_extension("toml.tmp");
    if let Err(e) = std::fs::write(&tmp, bytes) {
        // Best-effort cleanup so a failed write doesn't leave a partial
        // `.tmp` behind. Ignore the remove error — the original write
        // failure is the one we want to surface.
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, path)?;
    Ok(path.to_path_buf())
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::profiles::ProfileError;
    use std::io::Write;

    fn tmp(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static C: AtomicUsize = AtomicUsize::new(0);
        let id = C.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "weft-io-{tag}-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::UNIX_EPOCH
                .elapsed()
                .unwrap_or_default()
                .as_nanos(),
            id
        ))
    }

    #[test]
    fn load_missing_file_is_io_error() {
        let path = tmp("missing");
        let _ = std::fs::remove_file(&path);
        let err = load_resolved_from_path(&path).unwrap_err();
        assert!(matches!(err, ConfigLoadError::Io(_)));
    }

    #[test]
    fn load_invalid_toml_is_parse_error() {
        let path = tmp("bad-toml");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, "not = valid = toml").unwrap();
        let err = load_resolved_from_path(&path).unwrap_err();
        assert!(matches!(err, ConfigLoadError::Parse(_)));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_invalid_utf8_is_error() {
        let path = tmp("bad-utf8");
        let _ = std::fs::remove_file(&path);
        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(&[0xFF, 0xFE, 0x00]).unwrap();
        }
        let err = load_resolved_from_path(&path).unwrap_err();
        // Either Io (InvalidData) — both are non-fatal load errors.
        assert!(matches!(err, ConfigLoadError::Io(_)));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_resolved_returns_source_and_effective() {
        let path = tmp("ok");
        let _ = std::fs::remove_file(&path);
        let toml = r#"
active_profile = "work"

[font]
family = "Base-Mono"

[profiles.work.font]
family = "Profile-Mono"
size = 18.0
"#;
        std::fs::write(&path, toml).unwrap();
        let loaded = load_resolved_from_path(&path).unwrap();
        // source has base font (not overridden).
        assert_eq!(loaded.source.font.family, "Base-Mono");
        // effective has profile font.
        assert_eq!(loaded.effective.font.family, "Profile-Mono");
        assert_eq!(loaded.effective.font.size, 18.0);
        assert!(loaded.diagnostics.is_empty());
        // fingerprint is stable for the same bytes.
        let fp2 = fingerprint_bytes(toml.as_bytes());
        assert_eq!(loaded.fingerprint, fp2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_resolved_missing_active_is_diagnostic_not_error() {
        let path = tmp("missing-active");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, r#"active_profile = "nope""#).unwrap();
        let loaded = load_resolved_from_path(&path).unwrap();
        assert_eq!(loaded.diagnostics.len(), 1);
        // effective == source (no override applied).
        assert_eq!(loaded.effective.font.family, loaded.source.font.family);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_resolved_reserved_profile_name_is_error() {
        let path = tmp("reserved");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, "[profiles.base]\n[profiles.base.font]\n").unwrap();
        let err = load_resolved_from_path(&path).unwrap_err();
        assert!(matches!(
            err,
            ConfigLoadError::Profile(ProfileError::ReservedName(_))
        ));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_resolved_unknown_profile_field_is_error() {
        let path = tmp("unknown-field");
        let _ = std::fs::remove_file(&path);
        // `ai` is not a valid profile section.
        std::fs::write(&path, "[profiles.work.ai]\nprovider = \"x\"\n").unwrap();
        let err = load_resolved_from_path(&path).unwrap_err();
        // deny_unknown_fields produces a toml de error.
        assert!(matches!(err, ConfigLoadError::Parse(_)));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_resolved_empty_active_normalizes_to_base() {
        let path = tmp("empty-active");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, "active_profile = \"\"\n[font]\nfamily = \"X\"\n").unwrap();
        let loaded = load_resolved_from_path(&path).unwrap();
        assert!(loaded.diagnostics.is_empty());
        assert_eq!(loaded.effective.font.family, "X");
        let _ = std::fs::remove_file(&path);
    }

    // ── v1.11.2 X4: [blocks] retained_limit round-trip (PLAN_v1112 §1.2)

    #[test]
    fn blocks_retained_limit_round_trips_through_save_and_profile() {
        let path = tmp("blocks-roundtrip");
        let _ = std::fs::remove_file(&path);
        let toml = r#"
[blocks]
retained_limit = 500

[profiles.big.blocks]
retained_limit = 42
"#;
        std::fs::write(&path, toml).unwrap();
        let loaded = load_resolved_from_path(&path).unwrap();
        assert_eq!(loaded.source.blocks.retained_limit, 500);

        // Save the source back out and reload — the key must survive.
        let path2 = tmp("blocks-roundtrip-2");
        let _ = std::fs::remove_file(&path2);
        loaded.source.save_to_path(&path2).unwrap();
        let reloaded = load_resolved_from_path(&path2).unwrap();
        assert_eq!(reloaded.source.blocks.retained_limit, 500);

        // Profile override applies to effective.
        let with_active = r#"
active_profile = "big"

[profiles.big.blocks]
retained_limit = 42
"#;
        let path3 = tmp("blocks-roundtrip-3");
        let _ = std::fs::remove_file(&path3);
        std::fs::write(&path3, with_active).unwrap();
        let active = load_resolved_from_path(&path3).unwrap();
        assert_eq!(active.effective.blocks.retained_limit, 42);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&path2);
        let _ = std::fs::remove_file(&path3);
    }

    // ── v1.11.2 X3: [scrollback] lines clamp (PLAN_v1112 §5) ───────────
    #[test]
    fn normalize_scrollback_clamps_default_into_range() {
        let mut cfg = Config::default();
        assert_eq!(cfg.scrollback.lines, 10_000);
        normalize_scrollback(&mut cfg);
        assert_eq!(cfg.scrollback.lines, 10_000, "in-range value untouched");
    }

    #[test]
    fn load_huge_scrollback_lines_is_clamped_to_max() {
        let path = tmp("scroll-huge");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, "[scrollback]\nlines = 1_000_000_000\n").unwrap();
        let loaded = load_resolved_from_path(&path).unwrap();
        assert_eq!(loaded.source.scrollback.lines, SCROLLBACK_MAX_LINES);
        assert_eq!(loaded.effective.scrollback.lines, SCROLLBACK_MAX_LINES);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_zero_scrollback_lines_is_lifted_to_min() {
        let path = tmp("scroll-zero");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, "[scrollback]\nlines = 0\n").unwrap();
        let loaded = load_resolved_from_path(&path).unwrap();
        assert_eq!(loaded.source.scrollback.lines, SCROLLBACK_MIN_LINES);
        assert_eq!(loaded.effective.scrollback.lines, SCROLLBACK_MIN_LINES);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_profile_scrollback_override_is_clamped_too() {
        let path = tmp("scroll-profile");
        let _ = std::fs::remove_file(&path);
        let toml = r#"
active_profile = "big"

[profiles.big.scrollback]
lines = 500_000_000
"#;
        std::fs::write(&path, toml).unwrap();
        let loaded = load_resolved_from_path(&path).unwrap();
        // Base source was in-range (default), only the overlay went wild.
        assert_eq!(loaded.source.scrollback.lines, 10_000);
        assert_eq!(
            loaded.effective.scrollback.lines, SCROLLBACK_MAX_LINES,
            "profile override must be clamped after resolve_active_profile"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn fingerprint_is_stable_for_same_input() {
        // Same input → same output within one process.
        assert_eq!(fingerprint_bytes(b""), fingerprint_bytes(b""));
        assert_eq!(fingerprint_bytes(b"abc"), fingerprint_bytes(b"abc"));
        // Different input → different output (probabilistically safe).
        assert_ne!(fingerprint_bytes(b""), fingerprint_bytes(b"x"));
        assert_ne!(fingerprint_bytes(b"abc"), fingerprint_bytes(b"abcd"));
    }

    #[test]
    fn atomic_write_replaces_file() {
        let path = tmp("atomic");
        let _ = std::fs::remove_file(&path);
        atomic_write(&path, b"hello").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        atomic_write(&path, b"world").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "world");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn atomic_write_no_parent_is_error() {
        // `/nonexistent/dir/` doesn't exist, so create_dir_all must fail.
        // On macOS this is typically NotFound or PermissionDenied depending
        // on the exact path. We just assert that it fails (any io::Error).
        let path = PathBuf::from("/nonexistent/dir/weft-test/file.toml");
        let result = atomic_write(&path, b"x");
        assert!(result.is_err(), "writing to a missing parent dir must fail");
    }

    #[test]
    fn source_roundtrip_preserves_profiles_and_active() {
        let path = tmp("roundtrip");
        let _ = std::fs::remove_file(&path);
        let toml = r#"
active_profile = "work"

[font]
family = "Base"

[profiles.work.font]
family = "Profile"
"#;
        std::fs::write(&path, toml).unwrap();
        let loaded = load_resolved_from_path(&path).unwrap();
        // Save source back to a new path and reload.
        let path2 = tmp("roundtrip-2");
        let _ = std::fs::remove_file(&path2);
        loaded.source.save_to_path(&path2).unwrap();
        let loaded2 = load_resolved_from_path(&path2).unwrap();
        assert_eq!(loaded2.source.active_profile.as_deref(), Some("work"));
        assert!(loaded2.source.profiles.contains_key("work"));
        assert_eq!(loaded2.effective.font.family, "Profile");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&path2);
    }

    #[test]
    fn resolved_config_cannot_flatten_into_source_on_save() {
        // The critical invariant: saving `effective` (not `source`) would
        // flatten the profile's font into the base [font] section,
        // breaking the raw/effective separation. This test documents that
        // `source` (the only saveable form) does NOT contain the override.
        let path = tmp("no-flatten");
        let _ = std::fs::remove_file(&path);
        let toml = r#"
active_profile = "work"

[font]
family = "Base"

[profiles.work.font]
family = "Profile"
"#;
        std::fs::write(&path, toml).unwrap();
        let loaded = load_resolved_from_path(&path).unwrap();
        // source.font.family is "Base" (no override applied to source).
        assert_eq!(loaded.source.font.family, "Base");
        // effective.font.family is "Profile" (override applied).
        assert_eq!(loaded.effective.font.family, "Profile");
        // Saving source preserves the profile table.
        let path2 = tmp("no-flatten-2");
        let _ = std::fs::remove_file(&path2);
        loaded.source.save_to_path(&path2).unwrap();
        let saved = std::fs::read_to_string(&path2).unwrap();
        assert!(saved.contains("[profiles.work"));
        assert!(saved.contains("Base"));
        // And the base [font] is NOT the profile's font.
        let reloaded = load_resolved_from_path(&path2).unwrap();
        assert_eq!(reloaded.source.font.family, "Base");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&path2);
    }

    #[test]
    fn profile_order_is_deterministic() {
        let path = tmp("order");
        let _ = std::fs::remove_file(&path);
        // BTreeMap → alphabetical key order regardless of TOML order.
        let toml = r#"
[profiles.zebra.font]
family = "Z"

[profiles.alpha.font]
family = "A"

[profiles.mango.font]
family = "M"
"#;
        std::fs::write(&path, toml).unwrap();
        let loaded = load_resolved_from_path(&path).unwrap();
        let names: Vec<&String> = loaded.source.profiles.keys().collect();
        assert_eq!(
            names.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "mango", "zebra"]
        );
        let _ = std::fs::remove_file(&path);
    }
}
