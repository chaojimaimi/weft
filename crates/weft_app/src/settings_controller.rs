//! Settings overlay controller, validation, and narrow-window navigation.

#[path = "settings_geometry.rs"]
mod settings_geometry;

use super::*;
use crate::settings_validation::{
    adjust_finite_value, detect_keybinding_conflicts, directional_bool, validate_settings,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettingsEnterAction {
    DrillDown,
    Apply,
}

fn settings_enter_action(is_narrow: bool, drill_down: bool) -> SettingsEnterAction {
    if is_narrow && !drill_down {
        SettingsEnterAction::DrillDown
    } else {
        SettingsEnterAction::Apply
    }
}

impl App {
    pub(super) fn open_settings(&mut self) {
        self.settings.open_from(&self.config_state.config);
        self.refresh_settings_validation();
    }

    /// v1.5.1: `pub(super)` so `profiles_controller` can call this after a
    /// profile transaction reseeds the Settings draft.
    pub(super) fn refresh_settings_validation(&mut self) {
        self.settings.field_errors = validate_settings(&self.settings.draft);
        self.settings.error = (!self.settings.field_errors.is_empty()).then(|| {
            format!(
                "{} field(s) need attention",
                self.settings.field_errors.len()
            )
        });
    }

    pub(super) fn handle_settings_mouse_press(&mut self, x: f32, y: f32) {
        use crate::settings_component::SettingsTarget;

        match self.settings_target_at(x, y) {
            Some(SettingsTarget::SidebarCategory(tab)) => {
                if self.settings.tab != tab {
                    self.settings.tab = tab;
                    self.settings.selection = 0;
                    self.settings.scroll_offset = 0;
                }
                if self.settings_is_narrow() {
                    self.settings.drill_down = true;
                }
                self.request_redraw();
            }
            Some(SettingsTarget::Theme(i)) => {
                self.settings.selection = i;
                self.apply_settings_selection();
                self.request_redraw();
            }
            Some(SettingsTarget::ContentRow(row)) => {
                self.settings.selection = row.min(self.settings_tab_row_count().saturating_sub(1));
                self.request_redraw();
            }
            Some(SettingsTarget::CloseButton) => {
                self.close_settings();
                self.request_redraw();
            }
            Some(SettingsTarget::SaveButton) => {
                self.save_settings_draft(true);
                self.request_redraw();
            }
            Some(SettingsTarget::ApplyButton) => {
                self.save_settings_draft(false);
                self.request_redraw();
            }
            // v1.5.1: Profile toolbar clicks. The index is into the
            // per-frame sorted profile view (Base at 0, profiles at 1..).
            // Out-of-bounds is a no-op (the view may have shrunk between
            // the hit test and this dispatch).
            Some(SettingsTarget::ProfileEntry(i)) => {
                self.handle_profile_entry_click(i);
                self.request_redraw();
            }
            Some(SettingsTarget::ProfileCreate) => {
                self.handle_profile_create_click();
                self.request_redraw();
            }
            Some(SettingsTarget::ProfileDelete) => {
                self.handle_profile_delete_click();
                self.request_redraw();
            }
            // v1.5.2: Advanced → Import/Export action rows. Click triggers
            // the same flow as Enter (see `handle_settings_key`). The draft
            // is NOT saved first — Import replaces the config atomically,
            // and Export reads from the on-disk source file, not the draft.
            Some(SettingsTarget::AdvancedImport) => {
                self.settings.selection = 2;
                if let Err(e) = self.import_config_interactive() {
                    warn!(error = %e, "Settings: import panel failed");
                }
                self.request_redraw();
            }
            Some(SettingsTarget::AdvancedExport) => {
                self.settings.selection = 3;
                if let Err(e) = self.export_config_interactive() {
                    warn!(error = %e, "Settings: export panel failed");
                }
                self.request_redraw();
            }
            // v1.8.3: LocalAi → "Test Connection" action row. Click selects
            // row 7 and spawns a `/api/tags` refresh against the draft
            // config (without saving first). Same flow as Enter on row 7.
            Some(SettingsTarget::LocalAiTestConnection) => {
                self.settings.selection = 7;
                self.test_ai_connection_from_draft();
                self.request_redraw();
            }
            None if !self.point_inside_settings_box(x, y) => {
                self.close_settings();
                self.request_redraw();
            }
            None => {}
        }
    }

    /// F5: Handle a key while the Settings panel is open. Returns true if
    /// consumed. Modal — captures all non-modifier-chord keys so the panel
    /// owns keyboard input while visible.
    ///
    /// Navigation model:
    /// - **Wide mode** (split layout): Tab cycles sidebar categories; ↑↓
    ///   navigates within the content form; Enter applies; ←/→ adjusts.
    /// - **Narrow mode** (drill-down): When showing sidebar, ↑↓ navigates
    ///   categories and Enter drills into one. When showing content, ↑↓
    ///   navigates the form and Esc returns to the sidebar.
    /// - `Esc` closes the panel (from sidebar in narrow mode, or from wide
    ///   mode). In narrow content mode, Esc first returns to the sidebar.
    /// - `Cmd+Enter` saves draft to disk & closes.
    pub(super) fn handle_settings_key(
        &mut self,
        key: KeyCode,
        mods: Modifiers,
        _text: Option<&str>,
    ) -> bool {
        use crate::overlay::SettingsTab;

        // Cmd+Enter: save draft to disk & close.
        if mods.contains(Modifiers::SUPER) && key == KeyCode::Enter {
            self.save_settings_draft(true);
            self.request_redraw();
            return true;
        }

        // Let other modifier chords fall through (so Cmd+, can toggle
        // closed, Cmd+Q still quits, etc.).
        if mods.intersects(Modifiers::SUPER | Modifiers::CONTROL | Modifiers::ALT) {
            return false;
        }

        let is_narrow = self.settings_is_narrow();

        match key {
            KeyCode::Escape => {
                if is_narrow && self.settings.drill_down {
                    // Narrow content mode: Esc returns to sidebar.
                    self.settings.drill_down = false;
                    self.settings.selection = 0;
                    self.request_redraw();
                    true
                } else {
                    // Wide mode or narrow sidebar mode: close without saving.
                    self.close_settings();
                    self.request_redraw();
                    true
                }
            }
            KeyCode::Tab => {
                // Cycle to the next sidebar category (wraps around).
                let tabs = SettingsTab::ALL;
                let idx = tabs
                    .iter()
                    .position(|t| *t == self.settings.tab)
                    .unwrap_or(0);
                self.settings.tab = tabs[(idx + 1) % tabs.len()];
                self.settings.selection = 0;
                self.settings.scroll_offset = 0;
                if is_narrow {
                    // Tab always drills into the content view.
                    self.settings.drill_down = true;
                }
                self.request_redraw();
                true
            }
            KeyCode::Up => {
                if is_narrow && !self.settings.drill_down {
                    // Narrow sidebar mode: navigate categories.
                    let max = SettingsTab::ALL.len();
                    if max > 0 {
                        let idx = SettingsTab::ALL
                            .iter()
                            .position(|t| *t == self.settings.tab)
                            .unwrap_or(0);
                        let new_idx = (idx + max - 1) % max;
                        self.settings.tab = SettingsTab::ALL[new_idx];
                    }
                } else {
                    // Content mode: navigate rows with wrap-around.
                    let max = self.settings_tab_row_count();
                    if max > 0 {
                        self.settings.selection = (self.settings.selection + max - 1) % max;
                    }
                }
                self.request_redraw();
                true
            }
            KeyCode::Down => {
                if is_narrow && !self.settings.drill_down {
                    // Narrow sidebar mode: navigate categories.
                    let max = SettingsTab::ALL.len();
                    if max > 0 {
                        let idx = SettingsTab::ALL
                            .iter()
                            .position(|t| *t == self.settings.tab)
                            .unwrap_or(0);
                        let new_idx = (idx + 1) % max;
                        self.settings.tab = SettingsTab::ALL[new_idx];
                    }
                } else {
                    // Content mode: navigate rows with wrap-around.
                    let max = self.settings_tab_row_count();
                    if max > 0 {
                        self.settings.selection = (self.settings.selection + 1) % max;
                    }
                }
                self.request_redraw();
                true
            }
            KeyCode::Enter => {
                match settings_enter_action(is_narrow, self.settings.drill_down) {
                    SettingsEnterAction::DrillDown => {
                        self.settings.drill_down = true;
                        self.settings.selection = 0;
                    }
                    SettingsEnterAction::Apply => {
                        // v1.5.2: Advanced → Import/Export rows are action
                        // buttons. They trigger the file panel directly
                        // WITHOUT saving the draft first (Import replaces
                        // the config atomically; Export reads from the
                        // on-disk source file). All other rows follow the
                        // usual apply + save flow.
                        if self.settings.tab == SettingsTab::Advanced {
                            match self.settings.selection {
                                2 => {
                                    if let Err(e) = self.import_config_interactive() {
                                        warn!(error = %e, "Settings: import panel failed");
                                    }
                                }
                                3 => {
                                    if let Err(e) = self.export_config_interactive() {
                                        warn!(error = %e, "Settings: export panel failed");
                                    }
                                }
                                _ => {
                                    self.apply_settings_selection();
                                    self.save_settings_draft(false);
                                }
                            }
                        } else if self.settings.tab == SettingsTab::LocalAi
                            && self.settings.selection == 7
                        {
                            // v1.8.3: LocalAi row 7 — "Test Connection" action
                            // button. Spawns a `/api/tags` refresh against the
                            // draft config (without saving first, so the user
                            // can test before committing).
                            self.test_ai_connection_from_draft();
                        } else {
                            self.apply_settings_selection();
                            self.save_settings_draft(false);
                        }
                    }
                }
                self.request_redraw();
                true
            }
            // ←/→ nudges the value of the selected row (no-op for pick-list
            // categories like Appearance themes and Keybindings).
            KeyCode::Left | KeyCode::Right => {
                let delta = if key == KeyCode::Left { -1 } else { 1 };
                self.adjust_settings_value(delta);
                self.request_redraw();
                true
            }
            _ => false,
        }
    }

    /// F5: Detect whether the viewport is narrow (<640pt logical). The
    /// settings panel switches to single-column drill-down mode.
    pub(super) fn settings_is_narrow(&self) -> bool {
        let Some(renderer) = &self.renderer else {
            return false;
        };
        let (vp_w, _) = renderer.viewport();
        let scale = renderer.scale() as f32;
        if scale <= 0.0 {
            return false;
        }
        let logical_w = vp_w / scale;
        logical_w < crate::layout::SETTINGS_NARROW_THRESHOLD
    }

    /// v1.0 S2: Persist the working draft to disk and reload the live config
    /// so theme/font/padding changes take effect immediately. When `close`
    /// is true the panel is dismissed; the Apply button passes false so the
    /// user can keep editing. Failures (e.g. unset `HOME`, read-only config
    /// dir) are surfaced via [`settings_error`] instead of just being logged.
    ///
    /// F5: Runs field-level validation before saving. If validation fails,
    /// `field_errors` is populated and the save is aborted.
    pub(super) fn save_settings_draft(&mut self, close: bool) {
        self.refresh_settings_validation();
        if !self.settings.field_errors.is_empty() {
            // v1.8.5 fix: previously this silently returned with no feedback,
            // so Cmd+Enter appeared to do nothing. Now surface a visible
            // error so the user knows why the save was blocked.
            let count = self.settings.field_errors.len();
            self.settings.error = Some(format!(
                "Cannot save: {count} field(s) need attention. Fix them or press Esc to discard."
            ));
            self.request_redraw();
            return;
        }

        if self.settings.dirty {
            let loaded = match crate::profiles_controller::persist_settings_draft(
                self.config_state.source(),
                &self.settings.draft,
                self.settings.dirty_sections,
            ) {
                Ok(loaded) => loaded,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to save settings draft");
                    self.settings.error = Some(e.to_string());
                    return;
                }
            };
            self.commit_loaded_config(loaded);
            self.settings.dirty = false;
            self.settings.dirty_sections = weft_core::config::ConfigSectionMask::empty();
            self.settings.error = None;
        }
        if close {
            self.close_settings();
        }
    }

    /// F5: Adjust the value of the currently-selected row in the active
    /// category by `delta` (±1). Numeric rows map to config fields; the
    /// Font family / Logo variant rows cycle through fixed lists.
    pub(super) fn adjust_settings_value(&mut self, delta: i32) {
        use crate::overlay::SettingsTab;
        match self.settings.tab {
            SettingsTab::Appearance => {
                let theme_count = self.settings_appearance_theme_count();
                let row = self.settings.selection;
                // Rows 0..theme_count are the theme pick-list (←/→ no-op).
                if row < theme_count {
                    return;
                }
                let r = row - theme_count;
                match r {
                    0 => {
                        // Logo Variant: cycle Cool → Warm → Light → Transparent.
                        let variants = weft_core::config::LogoVariant::ALL;
                        let cur = variants
                            .iter()
                            .position(|v| *v == self.settings.draft.logo.variant)
                            .unwrap_or(0);
                        let next = (cur as i32 + delta).rem_euclid(variants.len() as i32) as usize;
                        self.settings.draft.logo.variant = variants[next];
                        self.settings
                            .mark_dirty(weft_core::config::ConfigSectionMask::LOGO);
                    }
                    1 => {
                        // Only cycle installed programming fonts. A custom
                        // current family stays selectable even if Core Text
                        // cannot resolve it yet (for example before install).
                        let families = crate::settings_validation::available_programming_fonts(
                            &self.settings.draft.font.family,
                        );
                        let cur = families
                            .iter()
                            .position(|family| family == &self.settings.draft.font.family)
                            .unwrap_or(0);
                        let next = (cur as i32 + delta).rem_euclid(families.len() as i32) as usize;
                        self.settings.draft.font.family = families[next].clone();
                        self.settings
                            .mark_dirty(weft_core::config::ConfigSectionMask::FONT);
                    }
                    2 => {
                        // Font Size: ±0.5 pt, clamped to [8.0, 24.0].
                        self.settings.draft.font.size = adjust_finite_value(
                            self.settings.draft.font.size,
                            delta,
                            0.5,
                            8.0,
                            24.0,
                            14.0,
                        );
                        self.settings
                            .mark_dirty(weft_core::config::ConfigSectionMask::FONT);
                    }
                    3 => {
                        // Line height: ±0.05, clamped to [1.0, 1.5].
                        self.settings.draft.font.line_height = adjust_finite_value(
                            self.settings.draft.font.line_height,
                            delta,
                            0.05,
                            1.0,
                            1.5,
                            1.2,
                        );
                        self.settings
                            .mark_dirty(weft_core::config::ConfigSectionMask::FONT);
                    }
                    4 => {
                        // Window opacity: ±0.05, clamped to [0.5, 1.0].
                        self.settings.draft.window.opacity = adjust_finite_value(
                            self.settings.draft.window.opacity,
                            delta,
                            0.05,
                            0.5,
                            1.0,
                            1.0,
                        );
                        self.settings
                            .mark_dirty(weft_core::config::ConfigSectionMask::WINDOW);
                    }
                    5 => {
                        // Directional toggle preserves semantic color overrides.
                        let current = self.settings.draft.theme.semantic_output_enabled();
                        let next = directional_bool(current, delta);
                        if next != current {
                            let prev = self.settings.draft.theme.output.take().unwrap_or_default();
                            self.settings.draft.theme.output =
                                Some(weft_core::config::OutputSemanticConfig {
                                    enabled: Some(next),
                                    ..prev
                                });
                            self.settings
                                .mark_dirty(weft_core::config::ConfigSectionMask::THEME);
                        }
                    }
                    _ => {}
                }
            }
            SettingsTab::Terminal => {
                // v1.12.19 (T13a/T13c): the row→field mapping lives in
                // `settings_validation::adjust_terminal_row` (Input tab
                // precedent) so all five rows — including the new Session
                // recovery cycle and the io-clamped scrollback range — stay
                // headless-tested.
                let draft = &mut self.settings.draft;
                let mask = crate::settings_validation::adjust_terminal_row(
                    &mut draft.scrollback.lines,
                    &mut draft.window.padding_x,
                    &mut draft.window.padding_y,
                    &mut draft.theme.minimum_contrast,
                    &mut draft.session.recovery,
                    self.settings.selection,
                    delta,
                );
                if let Some(mask) = mask {
                    self.settings.mark_dirty(mask);
                }
            }
            // v1.12.19 (T13b): Blocks tab — the row→field mapping lives in
            // `settings_validation::adjust_blocks_row` so all four rows
            // (T13: retention/output cap; T14: history age/size gates) stay
            // headless-tested (Input tab precedent).
            SettingsTab::Blocks => {
                if let Some(mask) = crate::settings_validation::adjust_blocks_row(
                    &mut self.settings.draft.blocks.retained_limit,
                    &mut self.settings.draft.blocks.output_cap_mib,
                    &mut self.settings.draft.blocks.history_max_age_days,
                    &mut self.settings.draft.blocks.history_max_db_mb,
                    self.settings.selection,
                    delta,
                ) {
                    self.settings.mark_dirty(mask);
                }
            }
            SettingsTab::Input => {
                // v1.11.1 (PLAN_v1111 §4.6): the row→field mapping lives in
                // `settings_validation::adjust_input_row` so all five rows
                // stay headless-tested; rows 0-1 mark EDITOR dirty and rows
                // 2-4 mark PASTE.
                if let Some(mask) = crate::settings_validation::adjust_input_row(
                    &mut self.settings.draft.editor,
                    &mut self.settings.draft.paste,
                    self.settings.selection,
                    delta,
                ) {
                    self.settings.mark_dirty(mask);
                }
            }
            SettingsTab::Keybindings => {
                // Read-only list — ←/→ has no meaning here.
            }
            SettingsTab::Window => match self.settings.selection {
                0 => {
                    // Window width: ±20 px, clamped to [400, 4000].
                    self.settings.draft.window.width =
                        ((self.settings.draft.window.width as i32 + delta * 20).max(400) as u32)
                            .min(4000);
                    self.settings
                        .mark_dirty(weft_core::config::ConfigSectionMask::WINDOW);
                }
                1 => {
                    // Window height: ±20 px, clamped to [300, 4000].
                    self.settings.draft.window.height =
                        ((self.settings.draft.window.height as i32 + delta * 20).max(300) as u32)
                            .min(4000);
                    self.settings
                        .mark_dirty(weft_core::config::ConfigSectionMask::WINDOW);
                }
                2 => {
                    // Sidebar width: ±10pt, clamped to [240, 360]. None → start at 280.
                    let next = adjust_finite_value(
                        self.settings.draft.window.sidebar_width.unwrap_or(280.0),
                        delta,
                        10.0,
                        weft_core::config::SIDEBAR_MIN_WIDTH,
                        weft_core::config::SIDEBAR_MAX_WIDTH,
                        280.0,
                    );
                    self.settings.draft.window.sidebar_width = Some(next);
                    self.settings
                        .mark_dirty(weft_core::config::ConfigSectionMask::WINDOW);
                }
                _ => {}
            },
            // v1.8.3: LocalAi — enable toggle, model cycle, max_tokens,
            // timeout, cmd-generation toggle, error-diagnosis toggle. Row 7
            // (Test Connection) is an action button, not a ←/→ adjustment.
            SettingsTab::LocalAi => match self.settings.selection {
                0 => {
                    // v1.8.4 fix: use directional_bool (← = off, → = on)
                    // instead of a toggle, for consistency with rows 5/6
                    // and predictability. Previously both ← and → toggled,
                    // which confused users trying to turn it off.
                    let want_on = directional_bool(self.settings.draft.ai.is_configured(), delta);
                    self.settings.draft.ai.provider = if want_on {
                        Some("ollama".to_string())
                    } else {
                        None
                    };
                    self.settings
                        .mark_dirty(weft_core::config::ConfigSectionMask::AI);
                }
                1 => {
                    // Model: cycle through cached model names. If the current
                    // model isn't in the list, ←/→ jumps to the first entry.
                    let models: Vec<String> =
                        self.ai_models.iter().map(|m| m.name.clone()).collect();
                    if models.is_empty() {
                        return; // no models to cycle; user should Test Connection first
                    }
                    let cur = self.settings.draft.ai.model.as_deref();
                    let idx = models
                        .iter()
                        .position(|m| Some(m.as_str()) == cur)
                        .unwrap_or(0);
                    let next = (idx as i32 + delta).rem_euclid(models.len() as i32) as usize;
                    self.settings.draft.ai.model = Some(models[next].clone());
                    self.settings
                        .mark_dirty(weft_core::config::ConfigSectionMask::AI);
                }
                2 => {
                    // URL: read-only display (loopback-only, edit via config.toml).
                    // ←/→ is a no-op.
                }
                3 => {
                    // Max Tokens: ±256, clamped to [256, 8192]. None → 1024.
                    let cur = self.settings.draft.ai.effective_max_tokens() as i32;
                    let next = (cur + delta * 256).clamp(256, 8192) as u32;
                    self.settings.draft.ai.max_tokens = Some(next);
                    self.settings
                        .mark_dirty(weft_core::config::ConfigSectionMask::AI);
                }
                4 => {
                    // Timeout: ±5s, clamped to [5, 120]. None → 30.
                    let cur = self.settings.draft.ai.effective_timeout_secs() as i32;
                    let next = (cur + delta * 5).clamp(5, 120) as u32;
                    self.settings.draft.ai.timeout_secs = Some(next);
                    self.settings
                        .mark_dirty(weft_core::config::ConfigSectionMask::AI);
                }
                5 => {
                    // Cmd Generation: toggle on/off.
                    self.settings.draft.ai.enable_command_generation =
                        directional_bool(self.settings.draft.ai.enable_command_generation, delta);
                    self.settings
                        .mark_dirty(weft_core::config::ConfigSectionMask::AI);
                }
                6 => {
                    // Error Diagnosis: toggle on/off.
                    self.settings.draft.ai.enable_error_diagnosis =
                        directional_bool(self.settings.draft.ai.enable_error_diagnosis, delta);
                    self.settings
                        .mark_dirty(weft_core::config::ConfigSectionMask::AI);
                }
                // Row 7 (Test Connection) is an action button — Enter/click
                // triggers it, ←/→ is a no-op.
                _ => {}
            },
            SettingsTab::Advanced => match self.settings.selection {
                // v1.11.5 (PLAN_v1115 §M8): rows 4-7 are the new
                // notification / clipboard rows; rows 0-3 (Debug Logging,
                // Experimental, Import, Export) stay non-adjustable.
                4 => {
                    // Notify Enabled: toggle.
                    self.settings.draft.notifications.enabled =
                        directional_bool(self.settings.draft.notifications.enabled, delta);
                    self.settings
                        .mark_dirty(weft_core::config::ConfigSectionMask::NOTIFICATIONS);
                }
                5 => {
                    // Notify Threshold: cycle 10s/30s/60s/120s.
                    self.settings.draft.notifications.threshold_secs =
                        crate::settings_validation::cycled_notify_threshold(
                            self.settings.draft.notifications.threshold_secs,
                            delta,
                        );
                    self.settings
                        .mark_dirty(weft_core::config::ConfigSectionMask::NOTIFICATIONS);
                }
                6 => {
                    // Notify Sound: toggle.
                    self.settings.draft.notifications.sound =
                        directional_bool(self.settings.draft.notifications.sound, delta);
                    self.settings
                        .mark_dirty(weft_core::config::ConfigSectionMask::NOTIFICATIONS);
                }
                7 => {
                    // OSC52 Clipboard: cycle default/off/unrestricted.
                    self.settings.draft.clipboard.osc52 =
                        crate::settings_validation::cycled_osc52_mode(
                            self.settings.draft.clipboard.osc52,
                            delta,
                        );
                    self.settings
                        .mark_dirty(weft_core::config::ConfigSectionMask::CLIPBOARD);
                }
                _ => {}
            },
        }
        if self.settings.dirty {
            self.refresh_settings_validation();
            // v1.2.11 实时预览：调整后立即 apply 视觉设置到 renderer，无需等
            // Apply/Save。仅当字段校验通过时才预览，避免无效值导致渲染崩溃。
            // 预览只动 renderer，不修改 config_state.config，不写盘；关闭面板
            // 时 draft 被丢弃，下次打开重新从 config 克隆，行为一致。
            if self.settings.field_errors.is_empty() {
                self.apply_settings_preview();
            }
        }
    }

    /// v1.2.11: 实时预览 draft 中的视觉设置到 renderer。
    ///
    /// 这是 `apply_config` 的"只动 renderer"子集：不修改 `config_state.config`，
    /// 不写盘，不影响 keybindings/scrollback 等非视觉设置。设计目的：
    /// 让用户按 ←/→ 调整 Font/Size/Line Height/Opacity/Padding 时立即看到效果，
    /// 而不是必须按 Enter/Apply 才生效。
    ///
    /// 关闭面板（Esc/点外面）时 `close_settings` 会调用
    /// `revert_renderer_to_config` 立即把 renderer 回滚到 config 状态。
    /// 如果用户按 Apply/Save，`save_settings_draft` → `reload_config` →
    /// `apply_config` 会把 draft 持久化到 config，renderer 状态保持一致。
    fn apply_settings_preview(&mut self) {
        // Clone the relevant fields out of `draft` so we don't hold an
        // immutable borrow of `self` while calling `&mut self` methods.
        //
        // v1.2.11 fix (P0-2): 不做 diff 判断，总是 apply。原来的 diff 基准是
        // `config_state.config`，但预览只改 renderer 不改 config，导致用户
        // "调回原值"时 diff 为空、renderer 停在中间状态。移除 diff 后，每次
        // adjust 都会 apply，确保 renderer 始终与 draft 一致。rebuild_atlas
        // 开销 ~10ms，按 ←/→ 的频率（人手速度）完全可接受。
        let draft_font = self.settings.draft.font.clone();
        let draft_opacity = self.settings.draft.window.opacity;
        let draft_padding_x = self.settings.draft.window.padding_x;
        let draft_padding_y = self.settings.draft.window.padding_y;
        let draft_sidebar_width = self.settings.draft.window.sidebar_width;
        let draft_minimum_contrast = self.settings.draft.theme.minimum_contrast;
        let draft_semantic_output = self.settings.draft.theme.semantic_output_enabled();

        // Font — always rebuild atlas with draft values.
        if let Some(r) = &mut self.renderer {
            let scaled = crate::settings_validation::runtime_scaled_font_config(
                &draft_font,
                self.config_state.font_scale,
            );
            r.rebuild_atlas(scaled);
        }
        self.recompute_layout();

        // Opacity — always flip Metal layer + NSWindow opaque flag.
        let new_opacity = crate::settings_validation::runtime_opacity(draft_opacity);
        if let Some(r) = &mut self.renderer {
            r.set_opacity(draft_opacity);
        }
        if let Some(window) = &self.window {
            let _ = crate::macos_window::set_window_opaque(window, new_opacity >= 1.0);
        }

        // Padding — always update renderer padding + recompute layout.
        if let Some(r) = &mut self.renderer {
            r.set_padding((draft_padding_x, draft_padding_y));
        }
        self.recompute_layout();

        // Sidebar width — always update renderer override.
        if let Some(r) = &mut self.renderer {
            r.set_sidebar_width(draft_sidebar_width);
            r.set_minimum_contrast(draft_minimum_contrast);
            r.set_semantic_output_enabled(draft_semantic_output);
        }

        self.request_redraw();
    }

    /// v1.2.11 fix (P0-1): 强制把 renderer 回滚到 `config_state.config` 的状态。
    ///
    /// 用于 `close_settings` 丢弃 draft 时把 renderer 从预览状态恢复回来。
    /// 与 `apply_config` 不同，这里不做 diff（因为 `config_state.config` 没变，
    /// diff 永远为空），而是无条件重 apply font/opacity/padding/sidebar 四项
    /// 视觉设置到 renderer，确保 renderer 与 config 重新对齐。
    pub(super) fn revert_renderer_to_config(&mut self) {
        let config = self.config_state.config.clone();

        // Font — rebuild atlas with config values (reverts preview).
        if let Some(r) = &mut self.renderer {
            let scaled = crate::settings_validation::runtime_scaled_font_config(
                &config.font,
                self.config_state.font_scale,
            );
            r.rebuild_atlas(scaled);
        }
        self.recompute_layout();

        // Opacity — restore Metal layer + NSWindow opaque flag.
        let cfg_opacity = crate::settings_validation::runtime_opacity(config.window.opacity);
        if let Some(r) = &mut self.renderer {
            r.set_opacity(config.window.opacity);
        }
        if let Some(window) = &self.window {
            let _ = crate::macos_window::set_window_opaque(window, cfg_opacity >= 1.0);
        }

        // Padding — restore renderer padding + recompute layout.
        if let Some(r) = &mut self.renderer {
            r.set_padding((config.window.padding_x, config.window.padding_y));
        }
        self.recompute_layout();

        // Sidebar width — restore renderer override.
        if let Some(r) = &mut self.renderer {
            r.set_sidebar_width(config.window.sidebar_width);
            r.set_minimum_contrast(config.theme.minimum_contrast);
            r.set_semantic_output_enabled(config.theme.semantic_output_enabled());
        }

        self.request_redraw();
    }

    /// F5: Number of selectable rows in the active Settings category.
    /// The tab→count mapping lives in
    /// `settings_validation::settings_tab_row_count` (headless-tested; the
    /// two dynamic counts arrive as parameters).
    pub(super) fn settings_tab_row_count(&self) -> usize {
        crate::settings_validation::settings_tab_row_count(
            self.settings.tab,
            self.settings_appearance_theme_count()
                + crate::settings_component::APPEARANCE_ADJUSTMENT_ROWS,
            self.settings_keybinding_views().len(),
        )
    }

    fn settings_appearance_theme_count(&self) -> usize {
        crate::settings_component::visible_appearance_theme_count(
            self.settings_theme_views().len(),
            self.settings_max_visible_rows(),
        )
    }

    /// F5: Apply the currently-selected row in the active category to the
    /// draft. Only the Appearance theme list is interactive via Enter (theme
    /// selection); all other rows use ←/→ for adjustments.
    pub(super) fn apply_settings_selection(&mut self) {
        use crate::overlay::SettingsTab;
        match self.settings.tab {
            SettingsTab::Appearance => {
                let theme_count = self.settings_appearance_theme_count();
                if self.settings.selection < theme_count {
                    if let Some(view) = self
                        .settings_theme_views()
                        .get(self.settings.selection)
                        .cloned()
                    {
                        if self.settings.draft.theme.name != view.name {
                            self.settings.draft.theme.name = view.name.clone();
                            self.settings.draft.theme.follow_system = false;
                            // v1.12: 按解析后的背景相对亮度判定（此前按名字里
                            // 有没有 "light"，导入主题必然误判）。
                            if weft_core::config::Theme::resolve_named(
                                &view.name,
                                &self.settings.draft.theme,
                            )
                            .is_dark()
                            {
                                self.config_state.preferred_dark_theme = view.name;
                            }
                            self.settings
                                .mark_dirty(weft_core::config::ConfigSectionMask::THEME);
                            self.refresh_settings_validation();
                        }
                    }
                }
                // Non-theme rows: Enter is a no-op (←/→ handles adjustments).
            }
            SettingsTab::Terminal
            | SettingsTab::Blocks
            | SettingsTab::Input
            | SettingsTab::Keybindings
            | SettingsTab::Window
            | SettingsTab::LocalAi
            | SettingsTab::Advanced => {
                // ←/→ handles adjustments; Enter is a no-op for standard rows.
                // (LocalAi row 7 / Advanced rows 2-3 are action buttons —
                // handled in `handle_settings_key`'s Enter branch.)
            }
        }
    }

    /// v1.8.3: Spawn a `/api/tags` model-list refresh against the draft AI
    /// config (without saving it first — the user can test before committing).
    /// Sets the connection status to `Testing` and records the in-flight id so
    /// `poll_ai_results` can route the eventual `ModelsRefreshed` event back
    /// here. If the draft is not configured for Ollama, or the base URL fails
    /// loopback validation, the status is set to `Failed` immediately with a
    /// descriptive message — no async result is coming.
    pub(super) fn test_ai_connection_from_draft(&mut self) {
        // Reject re-entrancy: if a refresh is already in flight, ignore
        // additional clicks until it resolves. The button is also visually
        // disabled while `Testing`, but keyboard Enter bypasses the visual
        // gate so this is the authoritative guard.
        if self.ai_connection_status.is_testing() {
            return;
        }

        let draft_ai = &self.settings.draft.ai;
        if !draft_ai.is_configured() {
            self.ai_connection_status = crate::app_state::AiConnectionStatus::Failed(
                "Enable Local AI (provider = ollama) first".into(),
            );
            return;
        }

        let base_url = crate::ai::client::effective_base_url(draft_ai);
        if !crate::ai::client::is_loopback_url(&base_url) {
            self.ai_connection_status = crate::app_state::AiConnectionStatus::Failed(format!(
                "Refusing non-loopback URL: {base_url}"
            ));
            return;
        }

        // `spawn_list_models` builds a temporary HTTP client from the draft
        // config (NOT `self.ai_state.backend`) so the user can test a draft
        // base_url / timeout before saving. Returns the assigned request id.
        match self.ai_state.spawn_list_models(draft_ai) {
            Some(id) => {
                self.ai_models_request_id = Some(id);
                self.ai_connection_status = crate::app_state::AiConnectionStatus::Testing;
            }
            None => {
                // spawn_list_models only returns None after the
                // is_configured + loopback checks above, so reaching here
                // means the HTTP client builder failed (e.g. TLS backend
                // init error). Surface a generic failure.
                self.ai_connection_status = crate::app_state::AiConnectionStatus::Failed(
                    "Failed to build HTTP client".into(),
                );
            }
        }
    }

    /// v1.0 S1: The list of built-in themes for the Appearance category. The
    /// order matches `Theme::resolve_named`'s match arms so the list
    /// stays in sync with the resolver.
    /// v1.12: 内置主题在前（ curated 顺序），其后追加 `~/.config/weft/themes/`
    /// 下发现的自定义主题——此前设置界面只有内置项，自定义主题只能从命令
    /// 面板进入，两处行为不一致。
    pub(super) fn settings_theme_views(&self) -> Vec<crate::overlay::SettingsThemeView> {
        use crate::overlay::SettingsThemeView;
        const BUILTINS: [(&str, &str); 11] = [
            ("weft-warm", "Weft Warm (default)"),
            ("weft-light", "Weft Light"),
            ("warp", "Warp Dark"),
            ("dracula", "Dracula"),
            ("solarized-dark", "Solarized Dark"),
            ("gruvbox-dark", "Gruvbox Dark"),
            ("nord", "Nord"),
            ("tokyo-night", "Tokyo Night"),
            ("catppuccin", "Catppuccin Mocha"),
            ("one-dark", "One Dark"),
            ("monokai-pro", "Monokai Pro"),
        ];
        let mut views: Vec<SettingsThemeView> = BUILTINS
            .iter()
            .map(|(name, label)| SettingsThemeView {
                name: (*name).to_string(),
                label: (*label).to_string(),
            })
            .collect();
        for name in self.available_theme_names() {
            if views.iter().any(|v| v.name == name) {
                continue;
            }
            views.push(SettingsThemeView {
                label: name.clone(),
                name,
            });
        }
        views
    }

    /// F5: The list of keybinding rows for the Keybindings category. Each
    /// row carries a `conflict` flag set by [`detect_keybinding_conflicts`].
    pub(super) fn settings_keybinding_views(&self) -> Vec<crate::overlay::SettingsKeybindingView> {
        use crate::overlay::SettingsKeybindingView;
        let mut views = Vec::new();
        for (chord, action) in &self.config_state.keybindings.map {
            let label = match action {
                weft_core::config::Action::Copy => "Copy",
                weft_core::config::Action::Paste => "Paste",
                weft_core::config::Action::ReloadConfig => "Reload Config",
                weft_core::config::Action::ScrollPageUp => "Scroll Page Up",
                weft_core::config::Action::ScrollPageDown => "Scroll Page Down",
                weft_core::config::Action::ScrollLineUp => "Scroll Line Up",
                weft_core::config::Action::ScrollLineDown => "Scroll Line Down",
                weft_core::config::Action::ScrollToTop => "Scroll To Top",
                weft_core::config::Action::ScrollToBottom => "Scroll To Bottom",
                weft_core::config::Action::ToggleBlockPanel => "Toggle Block Panel",
                weft_core::config::Action::ToggleCommandPalette => "Command Palette",
                weft_core::config::Action::ZoomIn => "Zoom In",
                weft_core::config::Action::ZoomOut => "Zoom Out",
                weft_core::config::Action::ZoomReset => "Zoom Reset",
                weft_core::config::Action::FindInGrid => "Find In Grid",
                weft_core::config::Action::ToggleTheme => "Toggle Theme",
                weft_core::config::Action::NewTab => "New Tab",
                weft_core::config::Action::CloseTab => "Close Tab",
                weft_core::config::Action::NextTab => "Next Tab",
                weft_core::config::Action::PrevTab => "Previous Tab",
                weft_core::config::Action::ToggleSettings => "Settings",
                weft_core::config::Action::SplitHorizontal => "Split Horizontal",
                weft_core::config::Action::SplitVertical => "Split Vertical",
                weft_core::config::Action::FocusNextPane => "Focus Next Pane",
                weft_core::config::Action::FocusPrevPane => "Focus Previous Pane",
                weft_core::config::Action::ClosePane => "Close Pane",
                weft_core::config::Action::TogglePaneZoom => "Toggle Pane Zoom",
                weft_core::config::Action::FocusPaneUp => "Focus Pane Up",
                weft_core::config::Action::FocusPaneDown => "Focus Pane Down",
                weft_core::config::Action::FocusPaneLeft => "Focus Pane Left",
                weft_core::config::Action::FocusPaneRight => "Focus Pane Right",
                weft_core::config::Action::GenerateCommand => "Generate Command (AI)",
                weft_core::config::Action::InsertAiSuggestion => "Insert AI Suggestion",
                weft_core::config::Action::CancelAiRequest => "Cancel AI Request",
                weft_core::config::Action::DiagnoseBlock => "Diagnose Block (AI)",
            };
            views.push(SettingsKeybindingView {
                action: label.to_string(),
                binding: chord_label(chord.0, chord.1),
                conflict: false,
            });
        }
        // F5: detect conflicts and mark conflicting rows.
        let conflicts = detect_keybinding_conflicts(&views);
        for v in &mut views {
            if conflicts.contains(v.binding.as_str()) {
                v.conflict = true;
            }
        }
        // Sort by action label for stable display.
        views.sort_by(|a, b| a.action.cmp(&b.action));
        views
    }
}

#[cfg(test)]
#[path = "settings_controller_tests.rs"]
mod tests;
