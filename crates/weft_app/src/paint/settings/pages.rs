// Terminal + Blocks Settings content arms — split out of paint/settings.rs
// to keep it within its architecture-gate ceiling (config_controller_tests
// split precedent). Child-module privacy reaches the parent's private
// helpers (`push_settings_row`, `find_field_error`) without duplication.
//! v1.12.19 (PLAN_v11217 §3.8 T13): Terminal gains the Session recovery
//! row; Blocks is a new tab (Retained limit / Output cap).

use crate::renderer::MetalRenderer;
use crate::settings_component::settings_value_x;

use super::find_field_error;

impl MetalRenderer {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_terminal_content(
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
    ) {
        // v1.12.19 (PLAN_v11217 §3.8 T13a): row 4 is the Session
        // recovery three-state cycle (ask/auto/never).
        let recovery_label = crate::settings_validation::recovery_mode_label(s.recovery_mode);
        let rows: [(&str, String, &str); 5] = [
            (
                "Scrollback:",
                format!("{} lines", s.scrollback_lines),
                "Scrollback",
            ),
            (
                "Padding X:",
                format!("{} cells", s.window_padding_x),
                "Padding X",
            ),
            (
                "Padding Y:",
                format!("{} cells", s.window_padding_y),
                "Padding Y",
            ),
            (
                "Contrast:",
                format!("{:.1}:1", s.minimum_contrast),
                "Minimum Contrast",
            ),
            ("Session recovery:", recovery_label.to_string(), ""),
        ];
        for (i, (label, value, err_label)) in rows.iter().enumerate() {
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
                find_field_error(s.field_errors, err_label),
            );
            // Row 4 side note: the gate is read once at startup,
            // so an edit only takes effect on the next launch.
            if i == 4 {
                self.push_settings_side_note(
                    verts,
                    row_y,
                    label,
                    value,
                    "applies on next launch",
                    label_c,
                    value_x,
                    content_x0,
                    cw,
                    content_cols,
                );
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_blocks_content(
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
        let retained_label = if s.blocks_retained_limit == 0 {
            "Unlimited".to_string()
        } else {
            format!("{} blocks", s.blocks_retained_limit)
        };
        // T14 (PLAN_v11217 §3.9): 0 = Off on both history-prune gates
        // (both Off → the prune never runs, behavior identical to pre-T14).
        let age_label = if s.blocks_history_max_age_days == 0 {
            "Off".to_string()
        } else {
            format!("{} days", s.blocks_history_max_age_days)
        };
        let size_label = if s.blocks_history_max_db_mb == 0 {
            "Off".to_string()
        } else {
            format!("{} MiB", s.blocks_history_max_db_mb)
        };
        let rows: [(&str, String); 4] = [
            ("Retained limit:", retained_label),
            ("Output cap:", format!("{} MiB", s.blocks_output_cap_mib)),
            ("History max age:", age_label),
            ("History max size:", size_label),
        ];
        for (i, (label, value)) in rows.iter().enumerate() {
            let value = value.as_str();
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
            // Memory warning (review P3): 0 disables retention
            // entirely — every finished block stays in memory.
            if i == 0 && s.blocks_retained_limit == 0 {
                self.push_settings_side_note(
                    verts,
                    row_y,
                    label,
                    value,
                    "0 keeps every block in memory — raise only with care",
                    warning_c,
                    value_x,
                    content_x0,
                    cw,
                    content_cols,
                );
            }
            // T14 orphan semantics (§3.9 4): pruned blocks also disappear
            // from the restored history of saved sessions.
            if i == 2 && s.blocks_history_max_age_days != 0 {
                self.push_settings_side_note(
                    verts,
                    row_y,
                    label,
                    value,
                    "pruned blocks vanish from restored sessions' history",
                    warning_c,
                    value_x,
                    content_x0,
                    cw,
                    content_cols,
                );
            }
        }
    }

    /// v1.12.19: a dim/warning side note rendered after the row's value
    /// (same positioning as the Advanced restart badge). Used for the
    /// Session recovery "applies on next launch" note and the Blocks
    /// retention memory warning.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn push_settings_side_note(
        &self,
        verts: &mut Vec<f32>,
        row_y: f32,
        label: &str,
        value: &str,
        note: &str,
        color: [f32; 4],
        value_x: f32,
        content_x0: f32,
        cw: f32,
        content_cols: usize,
    ) {
        // push_settings_row re-derives the value x from the label width —
        // mirror that here so the note follows the ACTUAL value position.
        let value_x = settings_value_x(value_x, content_x0, cw, Self::text_col_width(label));
        let val_text_w = cw * Self::text_col_width(value) as f32;
        let note_x = value_x + val_text_w + cw * 2.5;
        let note_cols = content_cols.saturating_sub(((note_x - content_x0) / cw).max(0.0) as usize);
        if note_cols > 3 {
            self.push_text(verts, note_x, row_y, note, color, note_cols);
        }
    }
}
