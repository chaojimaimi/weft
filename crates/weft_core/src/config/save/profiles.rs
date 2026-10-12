//! Profile-writer family, moved verbatim out of `save.rs` (v1.13.9 backlog
//! clear zero-behavior file-budget split). Free-function family like the
//! rest of `save.rs`; `super::` paths widen one level (`super::super::`)
//! to keep pointing at `config`.

use super::{
    set_f32_if_diff, set_opt_string, set_string_if_diff, set_u32_if_diff, set_usize_if_diff,
};

// ── v1.5.0: active_profile + profiles ───────────────────────────────────

/// v1.5.0: Write the top-level `active_profile = "name"` scalar. Writes
/// `None`/empty as removal of any existing key so the file doesn't carry a
/// stale reference to a deleted profile. Preserves comments via `toml_edit`.
///
/// Visibility: `pub(in crate::config)` so the `save.rs` re-export can keep
/// the pre-split `save::write_active_profile` path working (a `pub(super)`
/// item could not be re-exported one level wider, E0364).
pub(in crate::config) fn write_active_profile(
    doc: &mut toml_edit::DocumentMut,
    active: &Option<String>,
) {
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
///
/// Visibility: `pub(in crate::config)` for the same re-export reason as
/// [`write_active_profile`].
pub(in crate::config) fn write_profiles(
    doc: &mut toml_edit::DocumentMut,
    source: &super::super::Config,
    skipped: &mut Vec<&'static str>,
) {
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
        skipped.push("profiles");
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
            skipped.push("profiles");
            continue;
        };
        write_profile_sections(table, profile, skipped);
    }
}

/// Write the section overrides for a single profile. Only present sections
/// (the `Option<T>` is `Some`) are written; absent sections are removed so
/// a stale override doesn't survive a profile edit.
fn write_profile_sections(
    table: &mut toml_edit::Table,
    profile: &super::super::ProfileConfig,
    skipped: &mut Vec<&'static str>,
) {
    write_profile_section(table, "font", profile.font.is_some(), skipped, |t, _| {
        if let Some(f) = &profile.font {
            let default = super::super::FontConfig::default();
            set_string_if_diff(t, "family", &f.family, &default.family);
            set_f32_if_diff(t, "size", f.size, default.size);
            set_string_if_diff(t, "cjk_family", &f.cjk_family, &default.cjk_family);
            set_string_if_diff(t, "emoji_family", &f.emoji_family, &default.emoji_family);
            set_f32_if_diff(t, "line_height", f.line_height, default.line_height);
        }
    });
    write_profile_section(
        table,
        "theme",
        profile.theme.is_some(),
        skipped,
        |t, skipped| {
            if let Some(th) = &profile.theme {
                let default = super::super::ThemeConfig::default();
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
                write_profile_section(t, "output", th.output.is_some(), skipped, |ot, _| {
                    if let Some(output) = &th.output {
                        // v1.12.2 (PLAN_S2_render A1): the classifier default
                        // flipped to `false`, so the non-default value to
                        // persist is now `true`. Writing only the non-default
                        // keeps the minimal-write contract and lets the
                        // remove() branch clear a stale key on toggle (the
                        // v1.7.5 residue bug, mirrored for the new default).
                        if output.enabled == Some(true) {
                            ot["enabled"] = toml_edit::value(true);
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
                // v1.11.6 (PLAN_v1116 M6/D-f): profile-level `[profiles.x.theme]`
                // link + `[theme.ui]` override round-trip.
                set_opt_string(t, "link", &th.link);
                write_profile_section(t, "ui", th.ui.is_some(), skipped, |ut, _| {
                    if let Some(ui) = &th.ui {
                        set_opt_string(ut, "success", &ui.success);
                        set_opt_string(ut, "warning", &ui.warning);
                        set_opt_string(ut, "error", &ui.error);
                        set_opt_string(ut, "find_match", &ui.find_match);
                    }
                });
            }
        },
    );
    write_profile_section(
        table,
        "window",
        profile.window.is_some(),
        skipped,
        |t, _| {
            if let Some(w) = &profile.window {
                let default = super::super::WindowConfig::default();
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
                // v1.12.2 B2 (PLAN_S2_render): persist only the non-default
                // `true` (live-resize present-mode rollback switch).
                if w.presents_with_transaction_live_resize {
                    t["presents_with_transaction_live_resize"] = toml_edit::value(true);
                } else if t.contains_key("presents_with_transaction_live_resize") {
                    t.remove("presents_with_transaction_live_resize");
                }
            }
        },
    );
    write_profile_section(
        table,
        "scrollback",
        profile.scrollback.is_some(),
        skipped,
        |t, _| {
            if let Some(s) = &profile.scrollback {
                let default = super::super::ScrollbackConfig::default();
                set_usize_if_diff(t, "lines", s.lines, default.lines);
            }
        },
    );
    // v1.11.2 X4: `[blocks]` is profile-overridable like `[scrollback]`.
    write_profile_section(
        table,
        "blocks",
        profile.blocks.is_some(),
        skipped,
        |t, _| {
            if let Some(b) = &profile.blocks {
                let default = super::super::BlocksConfig::default();
                set_usize_if_diff(
                    t,
                    "retained_limit",
                    b.retained_limit,
                    default.retained_limit,
                );
            }
        },
    );
    // v1.11.3: `[compat]` is profile-overridable like `[blocks]`.
    write_profile_section(
        table,
        "compat",
        profile.compat.is_some(),
        skipped,
        |t, _| {
            if let Some(c) = &profile.compat {
                if c.bold_is_bright != super::super::CompatConfig::default().bold_is_bright {
                    t["bold_is_bright"] = toml_edit::value(c.bold_is_bright);
                } else if t.contains_key("bold_is_bright") {
                    t.remove("bold_is_bright");
                }
                // v1.11.4 (PLAN_v1114 §3): kitty_keyboard — persist the
                // non-default (false) only.
                if c.kitty_keyboard != super::super::CompatConfig::default().kitty_keyboard {
                    t["kitty_keyboard"] = toml_edit::value(c.kitty_keyboard);
                } else if t.contains_key("kitty_keyboard") {
                    t.remove("kitty_keyboard");
                }
            }
        },
    );
    // v1.11.5 (PLAN_v1115 §M8): `[clipboard]` / `[notifications]` are
    // profile-overridable — same only-non-default pattern as [compat].
    write_profile_section(
        table,
        "clipboard",
        profile.clipboard.is_some(),
        skipped,
        |t, _| {
            if let Some(c) = &profile.clipboard {
                if c.osc52 != super::super::ClipboardConfig::default().osc52 {
                    t["osc52"] = toml_edit::value(c.osc52.as_str());
                } else if t.contains_key("osc52") {
                    t.remove("osc52");
                }
            }
        },
    );
    write_profile_section(
        table,
        "notifications",
        profile.notifications.is_some(),
        skipped,
        |t, _| {
            if let Some(n) = &profile.notifications {
                let default = super::super::NotificationsConfig::default();
                if n.enabled != default.enabled {
                    t["enabled"] = toml_edit::value(n.enabled);
                } else if t.contains_key("enabled") {
                    t.remove("enabled");
                }
                if n.threshold_secs != default.threshold_secs {
                    t["threshold_secs"] = toml_edit::value(n.threshold_secs as i64);
                } else if t.contains_key("threshold_secs") {
                    t.remove("threshold_secs");
                }
                if n.sound != default.sound {
                    t["sound"] = toml_edit::value(n.sound);
                } else if t.contains_key("sound") {
                    t.remove("sound");
                }
            }
        },
    );
    write_profile_section(
        table,
        "editor",
        profile.editor.is_some(),
        skipped,
        |t, _| {
            if let Some(e) = &profile.editor {
                if e.submit_on_ctrl_enter {
                    t["submit_on_ctrl_enter"] = toml_edit::value(true);
                } else if t.contains_key("submit_on_ctrl_enter") {
                    t.remove("submit_on_ctrl_enter");
                }
                if e.smart_select != super::super::EditorConfig::default().smart_select {
                    t["smart_select"] = toml_edit::value(e.smart_select);
                } else if t.contains_key("smart_select") {
                    t.remove("smart_select");
                }
            }
        },
    );
    // v1.11.1 (PLAN_v1111 §4.2): `[profiles.x.paste]` follows the editor
    // pattern — full-section override, only non-default keys persisted.
    write_profile_section(table, "paste", profile.paste.is_some(), skipped, |t, _| {
        if let Some(p) = &profile.paste {
            let default = super::super::PasteConfig::default();
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
    write_profile_section(table, "logo", profile.logo.is_some(), skipped, |t, _| {
        if let Some(l) = &profile.logo {
            let default = super::super::LogoConfig::default();
            if l.variant != default.variant {
                t["variant"] = toml_edit::value(l.variant.as_str());
            } else if t.contains_key("variant") {
                t.remove("variant");
            }
        }
    });
    // The keybindings body records its own construction-failure skip, so it
    // takes `skipped` through the closure parameter (no capture conflict).
    write_profile_section(
        table,
        "keybindings",
        profile.keybindings.is_some(),
        skipped,
        |t, skipped| {
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
                    skipped.push("profiles");
                    return;
                };
                for (binding, action) in kb {
                    let action_str = super::super::action::action_to_str(action);
                    kt.insert(binding, toml_edit::value(action_str));
                }
                // Replace wholesale — keybindings is a full-section override.
                *t.entry("keybindings")
                    .or_insert_with(|| toml_edit::Item::None) = kb_table;
            }
        },
    );
}

/// Write or remove a single profile section. When `present` is false the
/// section is removed so a profile edit that drops a section takes effect
/// on save. When true, `f` writes the section's fields.
fn write_profile_section<F>(
    table: &mut toml_edit::Table,
    name: &str,
    present: bool,
    skipped: &mut Vec<&'static str>,
    f: F,
) where
    // v1.11.12 (PLAN_v11112 M-E): the writer closure receives `skipped` so
    // NESTED profile sections (theme → output/ui) can record their own
    // skips — a closure capture plus an argument borrow would conflict.
    F: FnOnce(&mut toml_edit::Table, &mut Vec<&'static str>),
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
    // v1.11.12 (PLAN_v11112 M-E): record the skip under the top-level
    // segment literal ("profiles") — the dynamic profile/section names can't
    // live in a `Vec<&'static str>`.
    let Some(section) = entry.as_table_mut() else {
        tracing::warn!(
            section = name,
            "profile section is not a table; skipping section write"
        );
        skipped.push("profiles");
        return;
    };
    f(section, skipped);
}
