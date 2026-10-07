// LocalAi settings content arm — split out of paint/settings.rs to keep
// it within its architecture-gate ceiling (pages.rs v1.12.19 precedent:
// child-module privacy reaches the parent's private helpers).
//! v1.12.27b (P1-03): verbatim move of the `SettingsTab::LocalAi` arm
//! (settings.rs baseline :576-691) — zero behavior change. The arm-leading
//! comment (v1.8.3) moved with the body.

use crate::renderer::MetalRenderer;

impl MetalRenderer {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_local_ai_content(
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
        warning_c: [f32; 4],
    ) {
        // v1.8.3: LocalAi tab — 8 rows: Enabled / Model / URL /
        // Max Tokens / Timeout / Cmd Generation / Error Diagnosis /
        // Test Connection (action button). All rows except row 7 use
        // the standard `push_settings_row` helper; row 7 gets a "▶"
        // glyph like Advanced's action rows and a status line below.
        let enabled_str = if s.ai.enabled { "On" } else { "Off" };
        let model_str = if s.ai.model.is_empty() {
            "(none — Test Connection to discover)"
        } else {
            s.ai.model
        };
        let max_tokens_str = format!("{}", s.ai.max_tokens);
        let timeout_str = format!("{} s", s.ai.timeout_secs);
        let cmd_gen_str = if s.ai.enable_command_generation {
            "On"
        } else {
            "Off"
        };
        let err_diag_str = if s.ai.enable_error_diagnosis {
            "On"
        } else {
            "Off"
        };
        // Row 7 (Test Connection) — "▶ Test" when idle, "…" when
        // testing. The status line is drawn separately below.
        let test_str = if s.ai.testing {
            "\u{2026}"
        } else {
            "\u{25b6} Test"
        };

        // Rows 0-6 use the standard row helper. Row 7 is an
        // action button — rendered with the same helper but with
        // a "▶" glyph value (matching Advanced's Import/Export).
        let row_specs: [(&str, &str); 8] = [
            ("Enabled:", enabled_str),
            ("Model:", model_str),
            ("URL:", s.ai.base_url),
            ("Max Tokens:", &max_tokens_str),
            ("Timeout:", &timeout_str),
            ("Cmd Generation:", cmd_gen_str),
            ("Error Diagnosis:", err_diag_str),
            ("Test Connection:", test_str),
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

        // Status line below row 7 — shows the connection status
        // label (e.g. "Connected (3 models)", "Failed: …"). Uses
        // `warning_c` for failures and `accent` for success so the
        // user can tell at a glance whether the test succeeded.
        // Only drawn when there's something to show (non-idle).
        if s.ai.connection_status != "Not tested" {
            let status_y = content_top + 8.0 * ch;
            let status_color = if s.ai.connection_status.starts_with("Connected") {
                accent
            } else if s.ai.connection_status.starts_with("Testing") {
                label_c
            } else {
                warning_c
            };
            let status_cols = content_cols.saturating_sub(2);
            if status_cols > 0 {
                self.push_text(
                    verts,
                    content_x0 + cw * 2.0,
                    status_y,
                    s.ai.connection_status,
                    status_color,
                    status_cols,
                );
            }
        }

        // v1.8.3: Observability row — aggregate request counts +
        // p95 latency. Drawn below the connection status line.
        // Only shown when at least one request has been made
        // (empty string on a fresh launch). Uses `label_c` so it
        // reads as secondary diagnostics, not a primary action.
        if !s.ai.observability.is_empty() {
            let obs_y = content_top + 9.0 * ch;
            let obs_cols = content_cols.saturating_sub(2);
            if obs_cols > 0 {
                self.push_text(
                    verts,
                    content_x0 + cw * 2.0,
                    obs_y,
                    s.ai.observability,
                    label_c,
                    obs_cols,
                );
            }
        }
    }
}
