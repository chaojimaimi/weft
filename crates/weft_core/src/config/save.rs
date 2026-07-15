/// v1.0 S2: Error returned by [`Config::save`].
#[derive(Debug)]
pub enum ConfigSaveError {
    /// Neither `HOME` nor `XDG_CONFIG_HOME` is set, so the config path
    /// can't be resolved.
    NoConfigPath,
    /// The config path has no parent directory (shouldn't happen in
    /// practice, but handle it gracefully).
    NoParentDir,
    /// An existing config could not be parsed, so overwriting it would lose
    /// user content that Weft cannot safely preserve.
    InvalidExisting(String),
    /// An I/O error occurred while creating the parent dir, writing the
    /// temp file, or renaming it over the target.
    Io(std::io::Error),
}

impl std::fmt::Display for ConfigSaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoConfigPath => write!(
                f,
                "cannot save config: HOME and XDG_CONFIG_HOME are both unset"
            ),
            Self::NoParentDir => write!(f, "config path has no parent directory"),
            Self::InvalidExisting(error) => {
                write!(f, "cannot preserve existing config: {error}")
            }
            Self::Io(e) => write!(f, "config save failed: {e}"),
        }
    }
}

impl std::error::Error for ConfigSaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

pub(super) fn parse_existing(existing: &str) -> Result<toml_edit::DocumentMut, ConfigSaveError> {
    toml::from_str::<super::Config>(existing)
        .map_err(|error| ConfigSaveError::InvalidExisting(error.to_string()))?;
    let mut document: toml_edit::DocumentMut =
        existing.parse().map_err(|error: toml_edit::TomlError| {
            ConfigSaveError::InvalidExisting(error.to_string())
        })?;
    for section in [
        "font",
        "theme",
        "window",
        "scrollback",
        "editor",
        "logo",
        "keybindings",
    ] {
        let Some(item) = document.get_mut(section) else {
            continue;
        };
        let owned = std::mem::take(item);
        let table = owned
            .into_table()
            .map_err(|_| ConfigSaveError::InvalidExisting(format!("{section} must be a table")))?;
        *item = toml_edit::Item::Table(table);
    }
    Ok(document)
}

// ── toml_edit helper functions (S2) ─────────────────────────────────────

/// Set a string field in a toml_edit table. If the new value differs from
/// the default, write it; if it equals the default, remove any existing
/// entry so the default takes effect on reload.
pub(super) fn set_string_if_diff(
    table: &mut toml_edit::Table,
    key: &str,
    new: &str,
    default: &str,
) {
    if new == default {
        // v1.0 fix: remove the key so the default takes effect on reload.
        // Previously this returned early without touching the table, leaving
        // any old value in the file — so saving `name = "weft-warm"` (the
        // default) didn't clear a previously-saved `name = "solarized-dark"`.
        if table.contains_key(key) {
            table.remove(key);
        }
        return;
    }
    table.insert(key, toml_edit::value(new));
}

/// Set an optional string field. If `new` is `Some`, write it; if `None`,
/// remove any existing entry for the key (the override is cleared).
pub(super) fn set_opt_string(table: &mut toml_edit::Table, key: &str, new: &Option<String>) {
    match new {
        Some(s) => {
            table.insert(key, toml_edit::value(s.as_str()));
        }
        None => {
            // Don't forcibly remove — the user may have a comment they want
            // to keep. Just leave any existing entry in place.
        }
    }
}

pub(super) fn set_f32_if_diff(table: &mut toml_edit::Table, key: &str, new: f32, default: f32) {
    if (new - default).abs() < f32::EPSILON {
        table.remove(key);
        return;
    }
    table.insert(key, toml_edit::value(f64::from(new)));
}

pub(super) fn set_u32_if_diff(table: &mut toml_edit::Table, key: &str, new: u32, default: u32) {
    if new == default {
        table.remove(key);
        return;
    }
    table.insert(key, toml_edit::value(i64::from(new)));
}

pub(super) fn set_usize_if_diff(
    table: &mut toml_edit::Table,
    key: &str,
    new: usize,
    default: usize,
) {
    if new == default {
        table.remove(key);
        return;
    }
    table.insert(key, toml_edit::value(i64::try_from(new).unwrap_or(0)));
}
