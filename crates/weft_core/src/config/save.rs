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
        "paste",
        "blocks",
        "compat",
        "logo",
        "ai",
        "update",
        "keybindings",
        "profiles",
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
        // any old value in the file — so saving `name = "warp"` (the v1.15
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

/// Set an optional string field with "clear on None" semantics.
/// Unlike [`set_opt_string`], when `new` is `None` this forcibly removes
/// any existing disk entry. Use this for override fields where `None`
/// means "clear the override and fall back to default" (e.g. syntax
/// colors, output colors) — leaving a stale disk value would silently
/// override the default.
pub(super) fn set_opt_string_clear(table: &mut toml_edit::Table, key: &str, new: &Option<String>) {
    match new {
        Some(s) => {
            table.insert(key, toml_edit::value(s.as_str()));
        }
        None => {
            if table.contains_key(key) {
                table.remove(key);
            }
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

/// v1.8 AI integration: write the `[ai]` section to a `toml_edit` document.
///
/// Only persists non-default values — an entirely-default `[ai]` section is
/// removed so the file stays clean when AI is unconfigured. v1.8 removes the
/// `api_key` field entirely (no cloud providers); if an old config has a
/// stale `api_key` line, it's removed on next save.
///
/// Kept in `save.rs` (rather than `mod.rs`) so the parent file's line count
/// stays within its architecture-gate budget.
pub(super) fn write_ai_section(
    doc: &mut toml_edit::DocumentMut,
    ai: &super::AiConfig,
    skipped: &mut Vec<&'static str>,
) {
    let default_ai = super::AiConfig::default();
    let ai_dirty = ai.provider != default_ai.provider
        || ai.base_url != default_ai.base_url
        || ai.model != default_ai.model
        || ai.max_tokens != default_ai.max_tokens
        || ai.timeout_secs != default_ai.timeout_secs
        || ai.enable_error_diagnosis != default_ai.enable_error_diagnosis
        || ai.enable_command_generation != default_ai.enable_command_generation;
    // v1.8.9 fix: even when the new config is all-default (ai_dirty = false),
    // we must still enter the write path if the document already has an [ai]
    // section — otherwise stale values like `provider = "ollama"` survive a
    // save and the Enabled toggle appears stuck "on". This mirrors the
    // existing booleans logic (remove when default) for the provider key.
    let has_existing_ai = doc.get("ai").is_some();
    if ai_dirty || has_existing_ai {
        let ai_entry = doc.entry("ai").or_insert_with(toml_edit::table);
        if ai_entry.is_none() {
            *ai_entry = toml_edit::table();
        }
        // v1.11.0 (M6, AUDIT_v1.10.39): 用户手写的畸形 TOML（如 `ai = "x"`
        // 标量，或 toml_edit 宽容解析出的非表条目）不能再 panic——
        // 保存路径必须容错：跳过该段写回并告警，保留磁盘原样。
        let Some(table) = ai_entry.as_table_mut() else {
            tracing::warn!("[ai] section is not a table; skipping AI section write");
            skipped.push("ai");
            return;
        };
        // v1.8.9 fix: provider must use _clear variant so that turning AI off
        // (provider = None) actually removes the key from the TOML. The
        // non-clear variant preserves the old value when None, which caused
        // the Enabled toggle to appear stuck "on" after saving — the old
        // `provider = "ollama"` survived the save and was reloaded next launch.
        set_opt_string_clear(table, "provider", &ai.provider);
        // v1.8: api_key is never written. Remove any stale value from old configs.
        if table.contains_key("api_key") {
            table.remove("api_key");
        }
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
        // stale overrides.
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

/// v1.11.1 (PLAN_v1111 §4.2): write the `[paste]` section.
///
/// Only non-default values are persisted; an all-default config removes any
/// existing keys so stale switches can't survive a "reset to defaults" save
/// (same semantics as `[editor]` / `[ai]`). Lives here instead of `mod.rs`
/// to keep the parent file within its architecture-gate budget, mirroring
/// [`write_ai_section`].
pub(super) fn write_paste_section(
    doc: &mut toml_edit::DocumentMut,
    paste: &super::PasteConfig,
    skipped: &mut Vec<&'static str>,
) {
    let default = super::PasteConfig::default();
    let dirty = paste.confirm_large != default.confirm_large
        || paste.confirm_control_chars != default.confirm_control_chars
        || paste.size_threshold_kib != default.size_threshold_kib;
    if !dirty && !doc.contains_key("paste") {
        return;
    }
    // v1.11.0-style tolerance: ensure the table exists and skip gracefully
    // if a hand-written `paste = "x"` scalar can't be turned into one
    // (write_paste_keys warns and leaves the disk value untouched).
    let entry = doc.entry("paste").or_insert_with(toml_edit::table);
    if entry.is_none() {
        *entry = toml_edit::table();
    }
    write_paste_keys(doc, paste, &default, skipped);
}

/// v1.11.2 X4 (PLAN_v1112 §1.2): write the `[blocks]` section (retention cap;
/// PLAN_v11217 §3.5 adds output_cap_mib). Only non-default values persist.
pub(super) fn write_blocks_section(
    doc: &mut toml_edit::DocumentMut,
    blocks: &super::BlocksConfig,
    skipped: &mut Vec<&'static str>,
) {
    let default = super::BlocksConfig::default();
    let dirty = blocks != &default;
    if !dirty && !doc.contains_key("blocks") {
        return;
    }
    let entry = doc.entry("blocks").or_insert_with(toml_edit::table);
    if entry.is_none() {
        *entry = toml_edit::table();
    }
    let Some(blocks_entry) = doc
        .get_mut("blocks")
        .and_then(toml_edit::Item::as_table_mut)
    else {
        tracing::warn!("[blocks] section is not a table; skipping blocks section write");
        skipped.push("blocks");
        return;
    };
    // PLAN_v11217 §3.5 (T4): output_cap_mib joins the non-default-only write.
    // T14 (§3.9): history_max_age_days / history_max_db_mb are written here
    // EXPLICITLY — the per-key loop never picks up new fields on its own
    // (a missed key = settings save succeeds but the file never changes).
    let (b, d) = (blocks, &default);
    for (key, value, default_value) in [
        (
            "retained_limit",
            i64::try_from(b.retained_limit).unwrap_or(0),
            i64::try_from(d.retained_limit).unwrap_or(0),
        ),
        (
            "output_cap_mib",
            i64::try_from(b.output_cap_mib).unwrap_or(0),
            i64::try_from(d.output_cap_mib).unwrap_or(0),
        ),
        (
            "history_max_age_days",
            i64::from(b.history_max_age_days),
            i64::from(d.history_max_age_days),
        ),
        (
            "history_max_db_mb",
            i64::from(b.history_max_db_mb),
            i64::from(d.history_max_db_mb),
        ),
    ] {
        if value != default_value {
            blocks_entry[key] = toml_edit::value(value);
        } else if blocks_entry.contains_key(key) {
            blocks_entry.remove(key);
        }
    }
}

/// Write (or clear) the individual `[paste]` keys on an existing document.
fn write_paste_keys(
    doc: &mut toml_edit::DocumentMut,
    paste: &super::PasteConfig,
    default: &super::PasteConfig,
    skipped: &mut Vec<&'static str>,
) {
    let Some(paste_entry) = doc.get_mut("paste").and_then(toml_edit::Item::as_table_mut) else {
        tracing::warn!("[paste] section is not a table; skipping paste section write");
        skipped.push("paste");
        return;
    };
    if paste.confirm_large != default.confirm_large {
        paste_entry["confirm_large"] = toml_edit::value(paste.confirm_large);
    } else if paste_entry.contains_key("confirm_large") {
        paste_entry.remove("confirm_large");
    }
    if paste.confirm_control_chars != default.confirm_control_chars {
        paste_entry["confirm_control_chars"] = toml_edit::value(paste.confirm_control_chars);
    } else if paste_entry.contains_key("confirm_control_chars") {
        paste_entry.remove("confirm_control_chars");
    }
    if paste.size_threshold_kib != default.size_threshold_kib {
        paste_entry["size_threshold_kib"] = toml_edit::value(i64::from(paste.size_threshold_kib));
    } else if paste_entry.contains_key("size_threshold_kib") {
        paste_entry.remove("size_threshold_kib");
    }
}

/// v1.13.0 (PLAN_v1.13.0_SPARKLE §WP2): write the `[update]` section
/// (Sparkle check tier). Global only — `ProfileConfig` has no `update`
/// field (the SESSION/AI precedent). Only non-default values persist; a
/// stale `check` key is removed when the tier returns to `daily` so the
/// factory default always wins on reload. Lives here (not `mod.rs`) per
/// plan R4 — the parent file stays within its architecture-gate budget.
pub(super) fn write_update_section(doc: &mut toml_edit::DocumentMut, update: &super::UpdateConfig) {
    let default = super::UpdateConfig::default();
    if update.check != default.check {
        let entry = doc.entry("update").or_insert_with(toml_edit::table);
        if entry.is_none() {
            *entry = toml_edit::table();
        }
        // v1.11.0-style tolerance: a hand-written `update = "x"` scalar is
        // skipped with a warn instead of panicking the save path.
        match entry.as_table_mut() {
            Some(table) => {
                table["check"] = toml_edit::value(update.check.as_str());
            }
            None => {
                tracing::warn!("[update] section is not a table; skipping update section write");
            }
        }
    } else if let Some(table) = doc.get_mut("update").and_then(|item| item.as_table_mut()) {
        table.remove("check");
        if table.iter().count() == 0 {
            doc.remove("update");
        }
    }
}

// Profile-writer family (write_active_profile / write_profiles /
// write_profile_sections / write_profile_section) lives in the child module
// `save/profiles.rs` (v1.13.9 file-budget split, pure code motion). The two
// config-entry points are re-exported here so the existing
// `save::write_active_profile` / `save::write_profiles` call sites in
// `mod.rs` and `config/tests.rs` keep resolving unchanged.
mod profiles;
pub(super) use profiles::{write_active_profile, write_profiles};

// Tests live in the gate-exempt sibling module (repo test-module
// convention, pty/tests.rs precedent) so inline test lines stay out of
// the production-file budget.
#[cfg(test)]
#[path = "save/tests.rs"]
mod tests;
