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
        "ai",
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

/// v1.6 AI integration: write the `[ai]` section to a `toml_edit` document.
///
/// Only persists non-default values — an entirely-default `[ai]` section is
/// removed so the file stays clean when AI is unconfigured. The plaintext
/// `api_key` is written only when set (Settings UI prefers Keychain, but
/// we keep the toml path for tests / quick local setups).
///
/// Kept in `save.rs` (rather than `mod.rs`) so the parent file's line count
/// stays within its architecture-gate budget.
pub(super) fn write_ai_section(doc: &mut toml_edit::DocumentMut, ai: &super::AiConfig) {
    let default_ai = super::AiConfig::default();
    let ai_dirty = ai.provider != default_ai.provider
        || ai.api_key != default_ai.api_key
        || ai.base_url != default_ai.base_url
        || ai.model != default_ai.model
        || ai.max_tokens != default_ai.max_tokens
        || ai.timeout_secs != default_ai.timeout_secs
        || ai.enable_error_diagnosis != default_ai.enable_error_diagnosis
        || ai.enable_command_generation != default_ai.enable_command_generation;
    if ai_dirty {
        let ai_entry = doc.entry("ai").or_insert_with(toml_edit::table);
        if ai_entry.is_none() {
            *ai_entry = toml_edit::table();
        }
        let table = ai_entry.as_table_mut().expect("ai is a table");
        set_opt_string(table, "provider", &ai.provider);
        set_opt_string(table, "api_key", &ai.api_key);
        set_opt_string(table, "base_url", &ai.base_url);
        set_opt_string(table, "model", &ai.model);
        if let Some(mt) = ai.max_tokens {
            table["max_tokens"] = toml_edit::value(i64::from(mt));
        } else if table.contains_key("max_tokens") {
            table.remove("max_tokens");
        }
        if let Some(ts) = ai.timeout_secs {
            table["timeout_secs"] = toml_edit::value(i64::from(ts));
        } else if table.contains_key("timeout_secs") {
            table.remove("timeout_secs");
        }
        // Booleans: only write when non-default.
        if ai.enable_error_diagnosis {
            table["enable_error_diagnosis"] = toml_edit::value(true);
        } else if table.contains_key("enable_error_diagnosis") {
            table.remove("enable_error_diagnosis");
        }
        if !ai.enable_command_generation {
            table["enable_command_generation"] = toml_edit::value(false);
        } else if table.contains_key("enable_command_generation") {
            table.remove("enable_command_generation");
        }
    } else if let Some(ai_entry) = doc.get_mut("ai") {
        // Whole section is at defaults — drop it so reload doesn't keep
        // stale overrides (e.g. a previous api_key).
        if ai_entry.as_table().is_some_and(|t| t.iter().count() == 0) {
            doc.remove("ai");
        } else {
            // Section has stale content. Replace with an empty table so
            // we don't lose the user's comments, but ensure no fields
            // remain that would override defaults.
            if let Some(table) = ai_entry.as_table_mut() {
                for key in [
                    "provider",
                    "api_key",
                    "base_url",
                    "model",
                    "max_tokens",
                    "timeout_secs",
                    "enable_error_diagnosis",
                    "enable_command_generation",
                ] {
                    table.remove(key);
                }
                if table.iter().count() == 0 {
                    doc.remove("ai");
                }
            }
        }
    }
}
