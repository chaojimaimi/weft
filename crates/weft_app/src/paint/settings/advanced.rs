// Advanced settings content arm — split out of paint/settings.rs to keep
// it within its architecture-gate ceiling (pages.rs v1.12.19 precedent:
// child-module privacy reaches the parent's private helpers).
//! v1.12.27b (P1-03): verbatim move of the `SettingsTab::Advanced` arm
//! (settings.rs baseline :495-575) — zero behavior change.

use crate::renderer::MetalRenderer;

impl MetalRenderer {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_advanced_content(
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
        // v1.5.2: Added Import Config / Export Config action rows.
        // The first two rows (Debug Logging, Experimental) carry
        // restart-required badges; the action rows carry a "▶"
        // glyph to signal that Enter triggers a panel.
        // v1.11.5 (PLAN_v1115 §M8): rows 4-7 read the LIVE
        // draft values (Notified Enabled / Threshold / Sound /
        // OSC52 Clipboard). `unrestricted` renders with a
        // warning suffix (silent clipboard reads by remote
        // programs). The Import / Export action rows carry the
        // "▶" glyph; those stay at rows 2/3.
        let notify_enabled = s.notify_enabled;
        let notify_sound = s.notify_sound;
        let osc52_label = match s.osc52_mode {
            weft_core::config::Osc52Mode::Default => "Default",
            weft_core::config::Osc52Mode::Off => "Off",
            weft_core::config::Osc52Mode::Unrestricted => "Unrestricted \u{26a0}",
        };
        let rows: [(&str, String); crate::settings_validation::ADVANCED_ROW_COUNT] = [
            ("Debug Logging:", "Off".to_string()),
            ("Experimental:", "Disabled".to_string()),
            ("Import Config:", "\u{25b6} Open\u{2026}".to_string()),
            ("Export Config:", "\u{25b6} Save\u{2026}".to_string()),
            (
                "Notify Enabled:",
                if notify_enabled { "On" } else { "Off" }.to_string(),
            ),
            (
                "Notify Threshold:",
                format!("{} s", s.notify_threshold_secs),
            ),
            (
                "Notify Sound:",
                if notify_sound { "On" } else { "Off" }.to_string(),
            ),
            ("OSC52 Clipboard:", osc52_label.to_string()),
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
            // Restart-required badge (↻) — only the first two
            // rows (Debug Logging, Experimental). The Import /
            // Export action rows don't need a restart.
            if i < 2 {
                let val_w = cw * Self::text_col_width(value) as f32;
                let badge_x = value_x + val_w + cw * 2.5;
                let badge_cols =
                    content_cols.saturating_sub(((badge_x - content_x0) / cw).max(0.0) as usize);
                if badge_cols > 5 {
                    self.push_text(
                        verts,
                        badge_x,
                        row_y,
                        "\u{21bb} restart",
                        warning_c,
                        badge_cols,
                    );
                }
            }
        }
    }
}
