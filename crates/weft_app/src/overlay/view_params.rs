//! Per-overlay view-parameter groups for [`build_overlay_stack`]
//! (v1.11 audit P3, PLAN_audit_fix_batch3 C1).
//!
//! The old flat 60-parameter signature is clustered into five structs whose
//! field names keep the original parameter names, so the draw params each
//! group feeds are greppable back to their pre-C1 names. `_viewport_width`
//! was unused and is deliberately NOT carried. The settings group is gated
//! behind `Option` at the call site: with the panel closed, its whole
//! construction chain (String collection included) is skipped.
//!
//! This file also hosts [`SettingsOwnedSnapshot`] and
//! [`SettingsState::view_params`] (PLAN_audit_fix_batch3 C4): the snapshot
//! carries the settings-domain *owned* computed values captured before
//! `run_redraw` mutably borrows the active tab, while `view_params` projects
//! the *borrowed* values directly off `SettingsState` — its `&self` receiver
//! keeps every borrow on `App::settings`, genuinely disjoint from the
//! `&mut sessions` pane borrow held for the rest of the frame.

use super::{
    AiSettingsView, PaletteFormView, SettingsKeybindingView, SettingsProfileView, SettingsTab,
    SettingsThemeView,
};
use weft_core::blocks::BlockId;

/// History sidebar panel (Cmd+Shift+B) view inputs.
#[derive(Debug, Clone, Copy)]
pub struct PanelViewParams<'a> {
    pub panel_width: f32,
    pub panel_open: bool,
    pub panel_query: &'a str,
    /// v1.12.26 (P1-03): active IME composition for the panel search box.
    pub panel_ime_preedit: &'a str,
    pub panel_selection: usize,
    pub panel_expanded: Option<BlockId>,
    pub panel_search_focused: bool,
    pub panel_scroll_offset: usize,
}

/// IME preedit overlay inputs (TUI passthrough preedit + ownership gate).
#[derive(Debug, Clone, Copy)]
pub struct ImeViewParams<'a> {
    pub ime_preedit: &'a str,
    pub ime_preedit_cursor: Option<(usize, usize)>,
    pub terminal_owns_ime: bool,
}

/// Command Palette (v0.7) view inputs. No derives: `palette_form` holds
/// `&PaletteFormView`, which implements neither Debug nor Copy.
pub struct PaletteViewParams<'a> {
    pub palette_open: bool,
    pub palette_query: &'a str,
    pub palette_selection: usize,
    pub palette_entries: &'a [(String, String, &'a str)],
    pub palette_banner: &'a str,
    pub palette_submode_input: &'a str,
    pub palette_ime_preedit: &'a str,
    pub palette_ime_preedit_cursor: Option<(usize, usize)>,
    pub palette_form: Option<&'a PaletteFormView<'a>>,
}

/// Prompt input box view inputs (the box itself is built from the terminal's
/// editor state; these are the two call-site-provided values).
#[derive(Debug, Clone, Copy)]
pub struct PromptViewParams {
    pub prompt_selection: Option<((usize, usize), (usize, usize))>,
    pub submit_on_ctrl_enter: bool,
}

/// Settings panel (Cmd+,) view inputs — the ~34 former `settings_*`
/// parameters, held together exactly like the renderer's `SettingsDrawParams`
/// (评审定案：同构内聚，不再拆分). Passed as `Option<&SettingsViewParams>`
/// so a closed panel skips construction entirely.
#[derive(Clone)]
pub struct SettingsViewParams<'a> {
    pub settings_tab: SettingsTab,
    pub settings_selection: usize,
    pub settings_scroll_offset: usize,
    pub settings_theme_name: &'a str,
    pub settings_themes: &'a [SettingsThemeView],
    pub settings_font_family: &'a str,
    pub settings_font_size: f32,
    pub settings_line_height: f32,
    pub settings_window_opacity: f32,
    pub settings_window_padding_x: u32,
    pub settings_window_padding_y: u32,
    pub settings_scrollback_lines: usize,
    pub settings_minimum_contrast: f32,
    pub settings_window_width: u32,
    pub settings_window_height: u32,
    pub settings_sidebar_width: Option<f32>,
    pub settings_submit_on_ctrl_enter: bool,
    pub settings_smart_select: bool,
    pub settings_paste_rows: crate::settings_validation::PasteRowsView,
    pub settings_keybindings: &'a [SettingsKeybindingView],
    pub settings_logo_variant: weft_core::config::LogoVariant,
    pub settings_error: Option<&'a str>,
    pub settings_is_narrow: bool,
    pub settings_drill_down: bool,
    pub settings_keybinding_conflict_count: usize,
    pub settings_field_errors: &'a [(String, String)],
    /// v1.5.1: profile toolbar views. Owned Vec of views that borrow the
    /// snapshot's profile names — built inside `view_params` (they cannot
    /// point at a caller local without a self-referential borrow).
    pub settings_profiles: Vec<SettingsProfileView<'a>>,
    pub settings_semantic_output_enabled: bool,
    pub settings_ai: AiSettingsView<'a>,
    pub settings_notify_enabled: bool,
    pub settings_notify_threshold_secs: u64,
    pub settings_notify_sound: bool,
    pub settings_osc52_mode: weft_core::config::Osc52Mode,
    /// v1.12.19 (PLAN_v11217 §3.8 T13a): Terminal row 5 draft value.
    pub settings_recovery_mode: weft_core::config::RecoveryMode,
    /// v1.12.19 (PLAN_v11217 §3.8 T13b): Blocks tab draft values. Draft
    /// projections like `settings_scrollback_lines` (:163 precedent) — NOT
    /// snapshot fields, or ←/→ steps would not repaint the value.
    pub settings_blocks_retained_limit: usize,
    pub settings_blocks_output_cap_mib: usize,
    /// T14 (PLAN_v11217 §3.9): history-prune gate draft values, same
    /// draft-projection discipline as the T13 fields above.
    pub settings_blocks_history_max_age_days: u32,
    pub settings_blocks_history_max_db_mb: u32,
    /// v1.13.0 (PLAN_v1.13.0_SPARKLE §WP2): Update tab draft tier (draft
    /// projection, same discipline as the T13/T14 fields).
    pub settings_update_tier: weft_core::config::UpdateCheckTier,
}

/// v1.11 audit (PLAN_audit_fix_batch3 C4): settings-domain *owned* computed
/// values, captured by `App::settings_owned_snapshot` while `self` is only
/// shared-borrowed. Draft/error/field-error projections are NOT here — those
/// are borrowed by [`SettingsState::view_params`] straight off
/// `App::settings`, which stays disjoint from `App::sessions`.
pub struct SettingsOwnedSnapshot {
    pub themes: Vec<SettingsThemeView>,
    pub keybindings: Vec<SettingsKeybindingView>,
    pub keybinding_conflict_count: usize,
    pub active_profile: Option<String>,
    pub profile_names: Vec<String>,
}

impl crate::app_state::SettingsState {
    /// v1.11 audit (PLAN_audit_fix_batch3 C4): build the settings view
    /// params. Receiver narrowed to `&SettingsState` so the borrowed
    /// projections (`theme_name`, `error`, `field_errors`, draft fields)
    /// land on `App::settings` alone — callable while `run_redraw` holds
    /// `&mut sessions` via the active pane. Owned computed values arrive
    /// via the snapshot.
    pub(crate) fn view_params<'a>(
        &'a self,
        owned: &'a SettingsOwnedSnapshot,
        ai: AiSettingsView<'a>,
        settings_is_narrow: bool,
    ) -> SettingsViewParams<'a> {
        // v1.5.1: profile toolbar views, rebuilt from the owned snapshot.
        // Index 0 is always "Base" (is_active = no active profile).
        let mut settings_profiles: Vec<SettingsProfileView<'a>> =
            Vec::with_capacity(owned.profile_names.len() + 1);
        settings_profiles.push(SettingsProfileView {
            name: "Base",
            is_active: owned.active_profile.is_none(),
        });
        for name in &owned.profile_names {
            settings_profiles.push(SettingsProfileView {
                name: name.as_str(),
                is_active: owned.active_profile.as_deref() == Some(name.as_str()),
            });
        }
        SettingsViewParams {
            settings_tab: self.tab,
            settings_selection: self.selection,
            settings_scroll_offset: self.scroll_offset,
            settings_theme_name: &self.draft.theme.name,
            settings_themes: &owned.themes,
            settings_font_family: &self.draft.font.family,
            settings_font_size: self.draft.font.size,
            settings_line_height: self.draft.font.line_height,
            settings_window_opacity: self.draft.window.opacity,
            settings_window_padding_x: self.draft.window.padding_x,
            settings_window_padding_y: self.draft.window.padding_y,
            settings_scrollback_lines: self.draft.scrollback.lines,
            settings_minimum_contrast: self.draft.theme.minimum_contrast,
            settings_window_width: self.draft.window.width,
            settings_window_height: self.draft.window.height,
            settings_sidebar_width: self.draft.window.sidebar_width,
            settings_submit_on_ctrl_enter: self.draft.editor.submit_on_ctrl_enter,
            settings_smart_select: self.draft.editor.smart_select,
            settings_paste_rows: crate::settings_validation::PasteRowsView {
                confirm_large: self.draft.paste.confirm_large,
                confirm_control_chars: self.draft.paste.confirm_control_chars,
                size_threshold_kib: self.draft.paste.size_threshold_kib,
            },
            settings_keybindings: &owned.keybindings,
            settings_logo_variant: self.draft.logo.variant,
            settings_error: self.error.as_deref(),
            settings_is_narrow,
            settings_drill_down: self.drill_down,
            settings_keybinding_conflict_count: owned.keybinding_conflict_count,
            settings_field_errors: &self.field_errors,
            settings_profiles,
            settings_semantic_output_enabled: self.draft.theme.semantic_output_enabled(),
            settings_ai: ai,
            settings_notify_enabled: self.draft.notifications.enabled,
            settings_notify_threshold_secs: self.draft.notifications.threshold_secs,
            settings_notify_sound: self.draft.notifications.sound,
            settings_osc52_mode: self.draft.clipboard.osc52,
            settings_recovery_mode: self.draft.session.recovery,
            settings_blocks_retained_limit: self.draft.blocks.retained_limit,
            settings_blocks_output_cap_mib: self.draft.blocks.output_cap_mib,
            settings_blocks_history_max_age_days: self.draft.blocks.history_max_age_days,
            settings_blocks_history_max_db_mb: self.draft.blocks.history_max_db_mb,
            settings_update_tier: self.draft.update.check,
        }
    }
}
