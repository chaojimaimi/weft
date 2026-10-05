//! Owned pre-draw snapshot helpers, moved verbatim from
//! `redraw_controller.rs` (v1.12.25 3-B-2 P2-02, former :863-974).
//!
//! Zero-rewrite: bodies, docs, and the C4 ownership contract below are
//! byte-identical to the originals; only the visibility is widened to
//! `pub(crate)` because the caller (`redraw_controller.rs`) is a sibling of
//! this module, not a descendant.

use crate::palette_state::WorkflowForm;

// ── v1.11 audit (PLAN_audit_fix_batch3 C4): owned pre-draw snapshots ──
//
// run_redraw's overlay/settings inputs used to be stack locals in one
// giant function body; the borrow checker only accepted that because every
// borrowed projection stayed inline. Hoisted into per-domain helpers, they
// must return OWNED data — any reference tied to `&self` would conflict
// with the `&mut sessions` borrow (`active_mut`) held for the rest of the
// frame. 借用冲突时按 PLAN 退化为更多 owned，禁止 unsafe 绕。

/// Owned data for the Settings LocalAi tab; `AiSettingsView` is built at
/// its use point from this snapshot (复审 P2-1: never stored in a returned
/// struct).
pub(crate) struct AiSettingsSnapshot {
    pub(crate) model_names: Vec<String>,
    pub(crate) connection_label: String,
    pub(crate) base_url: String,
    pub(crate) observability: String,
}

/// Owned palette-form data + the pre-computed palette IME cursor area.
/// `palette_entries` and the banner/submode inputs are deliberately NOT
/// here: they are built inside the pane-borrowed section (C4 快照边界外，
/// 复审 P2-2).
pub(crate) struct PaletteSnapshot {
    pub(crate) form: Option<PaletteFormSnapshot>,
    pub(crate) ime_area: Option<crate::ime::ImeCursorArea>,
}

/// Owned snapshot of one active workflow form.
pub(crate) struct PaletteFormSnapshot {
    pub(crate) workflow_name: String,
    pub(crate) fields: Vec<(String, String, bool)>,
    pub(crate) current_field: usize,
}

impl crate::App {
    /// v1.8.5: Compute the IME cursor area for the Palette input box.
    /// Called every frame when the Palette is open so the macOS candidate
    /// window stays anchored to the palette input (not the terminal cursor
    /// hidden behind the popup).
    pub(crate) fn palette_ime_cursor_area(
        &self,
        ctx: crate::layout::LayoutCtx,
    ) -> Option<crate::ime::ImeCursorArea> {
        use crate::palette_component::derive_palette_layout;

        let layout = derive_palette_layout(
            &ctx,
            self.palette.results.len(),
            self.palette.selection,
            self.palette.form.as_ref().map(|f| f.var_names.len()),
            self.interaction.popup_max_rows,
            self.interaction.popup_width_scale,
        );
        let search = match layout {
            crate::palette_component::PaletteLayout::Search(s) => s,
            _ => return None,
        };
        // The input text starts at query_x. In sub-modes with a banner,
        // the actual input begins after the banner text, but for IME
        // cursor positioning the query_x is close enough — the candidate
        // window just needs to be visible near the palette.
        Some(crate::ime::ImeCursorArea {
            x: search.query_x,
            y: search.query_y,
            width: ctx.cell_w,
            height: ctx.cell_h,
        })
    }

    // ── v1.11 audit (PLAN_audit_fix_batch3 C4): snapshot helpers ─────
    //
    // 逐段移动自 run_redraw 前置段（~:182-300 旧边界），只搬不改。
    // v1.12.25 (3-B-2 P2-02)：整段再迁入 redraw/snapshots.rs，仍只搬不改。

    /// Owned data for the Settings LocalAi tab (former :220-257 block).
    pub(crate) fn ai_settings_snapshot(&self) -> AiSettingsSnapshot {
        let metrics_snap = self.ai_state.metrics_snapshot();
        AiSettingsSnapshot {
            model_names: self.ai_models.iter().map(|m| m.name.clone()).collect(),
            connection_label: self.ai_connection_status.label(),
            base_url: crate::ai::client::effective_base_url(&self.settings.draft.ai),
            // v1.8.3: Observability summary — pre-formatted so the renderer
            // only pushes one text run. Empty when no requests have been
            // made (so the row is hidden on a fresh launch). Only aggregate
            // counts + p95 latency; no prompt/response text.
            observability: if metrics_snap.requests_total == 0 {
                String::new()
            } else {
                // v1.8.3: Surface truncations when non-zero — they indicate
                // the prompt budget was hit (history/output clipped before
                // sending).
                if metrics_snap.truncations_total > 0 {
                    format!(
                        "{} req · {} ok · {} err · {} canc · {} trunc · p95 {}ms",
                        metrics_snap.requests_total,
                        metrics_snap.successes_total,
                        metrics_snap.errors_total,
                        metrics_snap.cancellations_total,
                        metrics_snap.truncations_total,
                        metrics_snap.p95_latency_ms,
                    )
                } else {
                    format!(
                        "{} req · {} ok · {} err · {} canc · p95 {}ms",
                        metrics_snap.requests_total,
                        metrics_snap.successes_total,
                        metrics_snap.errors_total,
                        metrics_snap.cancellations_total,
                        metrics_snap.p95_latency_ms,
                    )
                }
            },
        }
    }

    /// Owned computed values of the settings domain (former :192-207
    /// blocks; profile_names 归属 settings 域，复审 P2-2). Borrowed values
    /// (draft fields / error / field_errors) are projected by
    /// `SettingsState::view_params` directly off `self.settings`.
    pub(crate) fn settings_owned_snapshot(&self) -> crate::overlay::SettingsOwnedSnapshot {
        let settings_keybindings = self.settings_keybinding_views();
        crate::overlay::SettingsOwnedSnapshot {
            themes: self.settings_theme_views(),
            keybinding_conflict_count: settings_keybindings.iter().filter(|v| v.conflict).count(),
            keybindings: settings_keybindings,
            active_profile: self.active_profile_name().map(str::to_owned),
            profile_names: self.profile_names_sorted(),
        }
    }

    /// Owned palette-form snapshot + palette IME cursor area (former
    /// :271-285 and :295-300 blocks).
    pub(crate) fn palette_snapshot(&self) -> PaletteSnapshot {
        PaletteSnapshot {
            form: self.palette.form.as_ref().map(|form| PaletteFormSnapshot {
                workflow_name: form.workflow_name.clone(),
                fields: WorkflowForm::draw_fields(form),
                current_field: form.current_field,
            }),
            ime_area: self
                .renderer
                .as_ref()
                .and_then(|r| r.layout_ctx)
                .filter(|_| self.palette.open)
                .and_then(|ctx| self.palette_ime_cursor_area(ctx)),
        }
    }
}
