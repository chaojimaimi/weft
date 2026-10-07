// Update settings content arm — split out of paint/settings.rs on the
// local_ai.rs precedent (v1.12.27b P1-03): child-module privacy reaches the
// parent's private helpers; keeps the parent within its architecture gate.
//! v1.13.0 (PLAN_v1.13.0_SPARKLE §WP2): Update tab content — three rows:
//! row 0 the check-tier cycle (Daily/Manual/Off), row 1 the framework
//! status line (R1 degrade face), row 2 the "Check Now" action button
//! (LocalAi "Test Connection" twin; dispatched by the controller, painted
//! here with the "▶" action glyph).

use crate::renderer::MetalRenderer;

impl MetalRenderer {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_update_content(
        &self,
        verts: &mut Vec<f32>,
        s: crate::overlay::SettingsDrawParams<'_>,
        content_top: f32,
        content_x0: f32,
        content_x1: f32,
        value_x: f32,
        cw: f32,
        ch: f32,
        content_cols: usize,
        bg_uv: [f32; 4],
        selection_bg: [f32; 4],
        fg: [f32; 4],
        label_c: [f32; 4],
        accent: [f32; 4],
        // v1.13.1 (PLAN_v1.13.1 §二.2): footer bound for the wrapped help
        // line's vertical clamp.
        footer_y: f32,
    ) {
        // Row 1 status: framework in place (spike R1 degrade face). Read
        // from the process-global bridge state — no App borrow needed.
        let status = if crate::updater::framework_available() {
            "Sparkle framework: available"
        } else {
            "Sparkle framework: unavailable"
        };
        let row_specs: [(&str, &str); 3] = [
            ("Check:", s.update_tier.label()),
            ("Status:", status),
            ("Check Now:", "\u{25b6} Check"),
        ];
        for (i, (label, value)) in row_specs.iter().enumerate() {
            let row_y = content_top + i as f32 * ch;
            let is_sel = i == s.selection;
            self.push_settings_row(
                verts,
                row_y,
                label,
                value,
                is_sel,
                content_x0,
                content_x1,
                value_x,
                cw,
                ch,
                content_cols,
                bg_uv,
                selection_bg,
                fg,
                label_c,
                accent,
                None,
            );
        }
        // Help line below the rows. v1.13.1 (G-5): word-wrapped within the
        // footer bound — the hard-clipped single row was the first visible
        // instance of the settings free-standing-text defect family.
        let help_y = content_top + row_specs.len() as f32 * ch;
        let help = if crate::updater::framework_available() {
            "Daily checks in the background; Off still allows manual checks."
        } else {
            "Update features require a bundled Sparkle.framework."
        };
        if let Some(budget) = crate::paint::text_wrap::line_budget(help_y, footer_y, ch) {
            self.push_text_wrapped(
                verts,
                content_x0 + cw * 2.0,
                help_y,
                help,
                label_c,
                content_cols.saturating_sub(2),
                Some(budget),
            );
        }
    }
}
