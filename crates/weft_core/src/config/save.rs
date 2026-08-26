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
pub(super) fn write_ai_section(doc: &mut toml_edit::DocumentMut, ai: &super::AiConfig) {
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
pub(super) fn write_paste_section(doc: &mut toml_edit::DocumentMut, paste: &super::PasteConfig) {
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
    write_paste_keys(doc, paste, &default);
}

/// v1.11.2 X4 (PLAN_v1112 §1.2): write the `[blocks]` section (retention
/// cap). Mirrors [`write_paste_section`]: only non-default values are
/// persisted; an all-default config leaves the file untouched.
pub(super) fn write_blocks_section(doc: &mut toml_edit::DocumentMut, blocks: &super::BlocksConfig) {
    let default = super::BlocksConfig::default();
    let dirty = blocks.retained_limit != default.retained_limit;
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
        return;
    };
    if blocks.retained_limit != default.retained_limit {
        blocks_entry["retained_limit"] =
            toml_edit::value(i64::try_from(blocks.retained_limit).unwrap_or(0));
    } else if blocks_entry.contains_key("retained_limit") {
        blocks_entry.remove("retained_limit");
    }
}

/// Write (or clear) the individual `[paste]` keys on an existing document.
fn write_paste_keys(
    doc: &mut toml_edit::DocumentMut,
    paste: &super::PasteConfig,
    default: &super::PasteConfig,
) {
    let Some(paste_entry) = doc.get_mut("paste").and_then(toml_edit::Item::as_table_mut) else {
        tracing::warn!("[paste] section is not a table; skipping paste section write");
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

// ── v1.5.0: active_profile + profiles ───────────────────────────────────

/// v1.5.0: Write the top-level `active_profile = "name"` scalar. Writes
/// `None`/empty as removal of any existing key so the file doesn't carry a
/// stale reference to a deleted profile. Preserves comments via `toml_edit`.
pub(super) fn write_active_profile(doc: &mut toml_edit::DocumentMut, active: &Option<String>) {
    match active {
        Some(name) if !name.trim().is_empty() => {
            doc["active_profile"] = toml_edit::value(name.as_str());
        }
        _ => {
            if doc.contains_key("active_profile") {
                doc.remove("active_profile");
            }
        }
    }
}

/// v1.5.0: Write the `[profiles.<name>]` tables, preserving comments and
/// unknown fields where possible.
///
/// Strategy:
/// - Walk the existing `[profiles]` table (if any) and remove entries that
///   no longer exist in `source.profiles`.
/// - For each profile in `source.profiles` (BTreeMap → stable order), write
///   or update the `[profiles.<name>]` table with the profile's sections.
/// - If `source.profiles` is empty, drop the whole `[profiles]` table so
///   reload doesn't keep stale entries.
///
/// Section writes use the same "only write non-default" helpers as the base
/// `[font]` / `[theme]` / … writers, so a profile that only overrides `[font]`
/// doesn't emit empty `[profiles.x.theme]` tables.
pub(super) fn write_profiles(doc: &mut toml_edit::DocumentMut, source: &super::Config) {
    if source.profiles.is_empty() {
        if doc.contains_key("profiles") {
            doc.remove("profiles");
        }
        return;
    }
    let profiles_entry = doc.entry("profiles").or_insert_with(toml_edit::table);
    if profiles_entry.is_none() {
        *profiles_entry = toml_edit::table();
    }
    // v1.11.0 (M6, AUDIT_v1.10.39): `profiles = "x"`（标量而非表）不再
    // panic——跳过整个 profiles 写回并告警，保留磁盘原样。
    let Some(profiles_table) = profiles_entry.as_table_mut() else {
        tracing::warn!("[profiles] section is not a table; skipping profiles write");
        return;
    };

    // Remove profiles that no longer exist in source.
    let stale: Vec<String> = profiles_table
        .iter()
        .filter_map(|(k, _)| {
            if source.profiles.contains_key(k) {
                None
            } else {
                Some(k.to_string())
            }
        })
        .collect();
    for name in stale {
        profiles_table.remove(&name);
    }

    // Write/update each profile. BTreeMap → alphabetical order.
    for (name, profile) in &source.profiles {
        let entry = profiles_table.entry(name).or_insert_with(toml_edit::table);
        if entry.is_none() {
            *entry = toml_edit::table();
        }
        // v1.11.0 (M6): 单个 profile 条目被写成标量（如 `[profiles]` 下的
        // `work = "x"`）时跳过该 profile 的写回，不再 panic。
        let Some(table) = entry.as_table_mut() else {
            tracing::warn!(profile = %name, "profile entry is not a table; skipping profile write");
            continue;
        };
        write_profile_sections(table, profile);
    }
}

/// Write the section overrides for a single profile. Only present sections
/// (the `Option<T>` is `Some`) are written; absent sections are removed so
/// a stale override doesn't survive a profile edit.
fn write_profile_sections(table: &mut toml_edit::Table, profile: &super::ProfileConfig) {
    write_profile_section(table, "font", profile.font.is_some(), |t| {
        if let Some(f) = &profile.font {
            let default = super::FontConfig::default();
            set_string_if_diff(t, "family", &f.family, &default.family);
            set_f32_if_diff(t, "size", f.size, default.size);
            set_string_if_diff(t, "cjk_family", &f.cjk_family, &default.cjk_family);
            set_string_if_diff(t, "emoji_family", &f.emoji_family, &default.emoji_family);
            set_f32_if_diff(t, "line_height", f.line_height, default.line_height);
        }
    });
    write_profile_section(table, "theme", profile.theme.is_some(), |t| {
        if let Some(th) = &profile.theme {
            let default = super::ThemeConfig::default();
            set_string_if_diff(t, "name", &th.name, &default.name);
            set_f32_if_diff(
                t,
                "minimum_contrast",
                th.minimum_contrast,
                default.minimum_contrast,
            );
            set_opt_string(t, "foreground", &th.foreground);
            set_opt_string(t, "background", &th.background);
            set_opt_string(t, "cursor", &th.cursor);
            set_opt_string(t, "selection", &th.selection);
            set_opt_string(t, "accent", &th.accent);
            set_opt_string(t, "accent_dim", &th.accent_dim);
            set_opt_string(t, "separator", &th.separator);
            if !th.palette.is_empty() {
                let mut arr = toml_edit::Array::new();
                for hex in &th.palette {
                    arr.push(hex.as_str());
                }
                t["palette"] = toml_edit::Item::Value(toml_edit::Value::Array(arr));
            } else if t.contains_key("palette") {
                t.remove("palette");
            }
            if th.follow_system {
                t["follow_system"] = toml_edit::value(true);
            } else if t.contains_key("follow_system") {
                t["follow_system"] = toml_edit::value(false);
            }
            if let Some(ln) = &th.light_name {
                t["light_name"] = toml_edit::value(ln.as_str());
            }
            if let Some(dn) = &th.dark_name {
                t["dark_name"] = toml_edit::value(dn.as_str());
            }
            write_profile_section(t, "output", th.output.is_some(), |ot| {
                if let Some(output) = &th.output {
                    if output.enabled == Some(false) {
                        ot["enabled"] = toml_edit::value(false);
                    } else {
                        ot.remove("enabled");
                    }
                    set_opt_string(ot, "output_default", &output.output_default);
                    // v1.11.0: `cwd` key removed — dead config (painter
                    // derives CWD gray from fg×0.65); see AUDIT_v1.10.39 /
                    // PLAN_v111. Not written, never cleaned.
                    set_opt_string(ot, "metadata", &output.metadata);
                    set_opt_string(ot, "success", &output.success);
                    set_opt_string(ot, "failure", &output.failure);
                }
            });
        }
    });
    write_profile_section(table, "window", profile.window.is_some(), |t| {
        if let Some(w) = &profile.window {
            let default = super::WindowConfig::default();
            set_u32_if_diff(t, "width", w.width, default.width);
            set_u32_if_diff(t, "height", w.height, default.height);
            set_string_if_diff(t, "title", &w.title, &default.title);
            set_f32_if_diff(t, "opacity", w.opacity, default.opacity);
            set_u32_if_diff(t, "padding_x", w.padding_x, default.padding_x);
            set_u32_if_diff(t, "padding_y", w.padding_y, default.padding_y);
            match w.sidebar_width {
                Some(sw) => {
                    t["sidebar_width"] = toml_edit::value(f64::from(sw));
                }
                None => {
                    if t.contains_key("sidebar_width") {
                        t.remove("sidebar_width");
                    }
                }
            }
        }
    });
    write_profile_section(table, "scrollback", profile.scrollback.is_some(), |t| {
        if let Some(s) = &profile.scrollback {
            let default = super::ScrollbackConfig::default();
            set_usize_if_diff(t, "lines", s.lines, default.lines);
        }
    });
    // v1.11.2 X4: `[blocks]` is profile-overridable like `[scrollback]`.
    write_profile_section(table, "blocks", profile.blocks.is_some(), |t| {
        if let Some(b) = &profile.blocks {
            let default = super::BlocksConfig::default();
            set_usize_if_diff(
                t,
                "retained_limit",
                b.retained_limit,
                default.retained_limit,
            );
        }
    });
    // v1.11.3: `[compat]` is profile-overridable like `[blocks]`.
    write_profile_section(table, "compat", profile.compat.is_some(), |t| {
        if let Some(c) = &profile.compat {
            if c.bold_is_bright != super::CompatConfig::default().bold_is_bright {
                t["bold_is_bright"] = toml_edit::value(c.bold_is_bright);
            } else if t.contains_key("bold_is_bright") {
                t.remove("bold_is_bright");
            }
        }
    });
    write_profile_section(table, "editor", profile.editor.is_some(), |t| {
        if let Some(e) = &profile.editor {
            if e.submit_on_ctrl_enter {
                t["submit_on_ctrl_enter"] = toml_edit::value(true);
            } else if t.contains_key("submit_on_ctrl_enter") {
                t.remove("submit_on_ctrl_enter");
            }
            if e.smart_select != super::EditorConfig::default().smart_select {
                t["smart_select"] = toml_edit::value(e.smart_select);
            } else if t.contains_key("smart_select") {
                t.remove("smart_select");
            }
        }
    });
    // v1.11.1 (PLAN_v1111 §4.2): `[profiles.x.paste]` follows the editor
    // pattern — full-section override, only non-default keys persisted.
    write_profile_section(table, "paste", profile.paste.is_some(), |t| {
        if let Some(p) = &profile.paste {
            let default = super::PasteConfig::default();
            // rust-reviewer v1.11.1 M-1: confirm_large defaults to TRUE, so
            // persist only when it differs from that default (the original
            // copy of this arm from submit_on_ctrl_enter — whose default is
            // false — had the predicate inverted and silently restored a
            // user's explicit `false` back to `true` on reload).
            if !p.confirm_large {
                t["confirm_large"] = toml_edit::value(false);
            } else if t.contains_key("confirm_large") {
                t.remove("confirm_large");
            }
            if !p.confirm_control_chars {
                t["confirm_control_chars"] = toml_edit::value(false);
            } else if t.contains_key("confirm_control_chars") {
                t.remove("confirm_control_chars");
            }
            if p.size_threshold_kib != default.size_threshold_kib {
                t["size_threshold_kib"] = toml_edit::value(i64::from(p.size_threshold_kib));
            } else if t.contains_key("size_threshold_kib") {
                t.remove("size_threshold_kib");
            }
        }
    });
    write_profile_section(table, "logo", profile.logo.is_some(), |t| {
        if let Some(l) = &profile.logo {
            let default = super::LogoConfig::default();
            if l.variant != default.variant {
                t["variant"] = toml_edit::value(l.variant.as_str());
            } else if t.contains_key("variant") {
                t.remove("variant");
            }
        }
    });
    write_profile_section(table, "keybindings", profile.keybindings.is_some(), |t| {
        if let Some(kb) = &profile.keybindings {
            if kb.is_empty() {
                if t.contains_key("keybindings") {
                    t.remove("keybindings");
                }
                return;
            }
            let mut kb_table = toml_edit::table();
            // v1.11.0 (M6): `kb_table` 是刚新建的表，as_table_mut 理论不会
            // 失败；仍改为容错形式（defense in depth，AUDIT_v1.10.39）。
            let Some(kt) = kb_table.as_table_mut() else {
                tracing::warn!(
                    "keybindings table construction failed; skipping profile keybindings write"
                );
                return;
            };
            for (binding, action) in kb {
                let action_str = super::action::action_to_str(action);
                kt.insert(binding, toml_edit::value(action_str));
            }
            // Replace wholesale — keybindings is a full-section override.
            *t.entry("keybindings")
                .or_insert_with(|| toml_edit::Item::None) = kb_table;
        }
    });
}

/// Write or remove a single profile section. When `present` is false the
/// section is removed so a profile edit that drops a section takes effect
/// on save. When true, `f` writes the section's fields.
fn write_profile_section<F>(table: &mut toml_edit::Table, name: &str, present: bool, f: F)
where
    F: FnOnce(&mut toml_edit::Table),
{
    if !present {
        if table.contains_key(name) {
            table.remove(name);
        }
        return;
    }
    let entry = table.entry(name).or_insert_with(toml_edit::table);
    if entry.is_none() {
        *entry = toml_edit::table();
    }
    // v1.11.0 (M6, AUDIT_v1.10.39): 段被写成标量（如 `[profiles.x]` 下的
    // `font = "y"`）时跳过该段写回并告警，不再 panic。
    let Some(section) = entry.as_table_mut() else {
        tracing::warn!(
            section = name,
            "profile section is not a table; skipping section write"
        );
        return;
    };
    f(section);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AiConfig;

    /// v1.8.9 regression test: turning AI off (provider = None) must remove
    /// the `provider` key from the TOML so the change survives a save/reload.
    /// Previously `set_opt_string` preserved the old value when None, so the
    /// Enabled toggle appeared stuck "on" after saving.
    #[test]
    fn write_ai_removes_provider_when_off() {
        // Simulate a config that currently has provider = "ollama".
        let mut doc = toml_edit::DocumentMut::new();
        doc["ai"] = toml_edit::table();
        {
            let table = doc["ai"].as_table_mut().unwrap();
            table["provider"] = toml_edit::value("ollama");
        }
        // User turns AI off → provider = None.
        let ai_off = AiConfig {
            provider: None,
            ..Default::default()
        };
        write_ai_section(&mut doc, &ai_off);
        let table = doc["ai"].as_table().unwrap();
        assert!(
            !table.contains_key("provider"),
            "provider key must be removed when AI is turned off"
        );
    }

    #[test]
    fn write_ai_writes_provider_when_on() {
        let mut doc = toml_edit::DocumentMut::new();
        let ai_on = AiConfig {
            provider: Some("ollama".into()),
            model: Some("llama3.1".into()),
            ..Default::default()
        };
        write_ai_section(&mut doc, &ai_on);
        let table = doc["ai"].as_table().unwrap();
        assert_eq!(
            table["provider"].as_str(),
            Some("ollama"),
            "provider must be written when AI is on"
        );
    }

    // ── v1.11.0 M6: malformed hand-written TOML must not panic the save
    // path (AUDIT_v1.10.39). Each test feeds the writer a doc where a
    // section/entry is a scalar instead of a table; the writer must skip
    // that segment (with a warn) and leave the document untouched. ──────

    #[test]
    fn write_ai_skips_non_table_ai_section_without_panic() {
        let mut doc = toml_edit::DocumentMut::new();
        // User wrote `ai = "x"` (scalar). The save path must not panic.
        doc["ai"] = toml_edit::value("x");
        let ai_on = AiConfig {
            provider: Some("ollama".into()),
            ..Default::default()
        };
        write_ai_section(&mut doc, &ai_on);
        assert_eq!(
            doc["ai"].as_str(),
            Some("x"),
            "malformed [ai] section must be left untouched"
        );
    }

    #[test]
    fn write_profiles_skips_non_table_profiles_without_panic() {
        let mut doc = toml_edit::DocumentMut::new();
        // User wrote `profiles = "x"` (scalar, not a table).
        doc["profiles"] = toml_edit::value("x");
        let cfg = crate::config::Config {
            profiles: std::collections::BTreeMap::from([(
                "work".into(),
                crate::config::ProfileConfig::default(),
            )]),
            ..crate::config::Config::default()
        };
        write_profiles(&mut doc, &cfg);
        assert_eq!(
            doc["profiles"].as_str(),
            Some("x"),
            "malformed [profiles] section must be left untouched"
        );
    }

    #[test]
    fn write_profiles_skips_scalar_profile_entry_without_panic() {
        let mut doc = toml_edit::DocumentMut::new();
        // `[profiles]` exists but `work` is a scalar.
        doc["profiles"] = toml_edit::table();
        doc["profiles"]["work"] = toml_edit::value("x");
        let cfg = crate::config::Config {
            profiles: std::collections::BTreeMap::from([(
                "work".into(),
                crate::config::ProfileConfig::default(),
            )]),
            ..crate::config::Config::default()
        };
        write_profiles(&mut doc, &cfg);
        assert_eq!(
            doc["profiles"]["work"].as_str(),
            Some("x"),
            "scalar profile entry must be left untouched"
        );
    }

    #[test]
    fn write_profile_sections_skips_scalar_section_without_panic() {
        let mut doc = toml_edit::DocumentMut::new();
        // `[profiles.work]` with `font = "y"` (scalar where a table is
        // expected) — the section writer must skip instead of panicking.
        doc["profiles"] = toml_edit::table();
        doc["profiles"]["work"] = toml_edit::table();
        doc["profiles"]["work"]["font"] = toml_edit::value("y");
        let cfg = crate::config::Config {
            profiles: std::collections::BTreeMap::from([(
                "work".into(),
                crate::config::ProfileConfig {
                    font: Some(crate::config::FontConfig::default()),
                    ..crate::config::ProfileConfig::default()
                },
            )]),
            ..crate::config::Config::default()
        };
        write_profiles(&mut doc, &cfg);
        assert_eq!(
            doc["profiles"]["work"]["font"].as_str(),
            Some("y"),
            "scalar profile section must be left untouched"
        );
    }
}
