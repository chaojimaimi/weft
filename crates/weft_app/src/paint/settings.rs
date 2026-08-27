// arch-gate: allow-over-800
// Settings 2.0 split layout: 6 category forms + sidebar + row renderer.
// Rewritten for F5; remaining size is the 6 per-category render functions
// + shared push_settings_row helper. Splitting categories into separate
// files would duplicate the row helper and field-error lookup.
//! Settings panel vertex builder extracted from renderer.rs (A5).
//!
//! F5: Rewritten for split sidebar layout. Left sidebar shows 6 categories
//! (Appearance / Terminal / Input / Keybindings / Window / Advanced); right
//! content area renders the active category's form. Narrow viewports
//! (<640pt) switch to single-column drill-down. Field-level validation
//! errors render inline next to the offending field. Keybinding conflicts
//! get a summary badge. Advanced rows carry restart-required badges.

use crate::paint::primitives::{color_to_normalized, push_filled_triangle, push_line, push_quad};
use crate::renderer::MetalRenderer;
use crate::settings_component::settings_value_x;

/// F5: Red color for inline field validation errors.
const ERROR_COLOR: [f32; 4] = [0.85, 0.25, 0.25, 1.0];

/// Look up a field-level validation error by label.
/// Returns the error message if the label matches an entry in `errors`.
fn find_field_error<'a>(errors: &'a [(String, String)], label: &str) -> Option<&'a str> {
    errors
        .iter()
        .find(|(l, _)| l == label)
        .map(|(_, m)| m.as_str())
}

impl MetalRenderer {
    /// F5: Build the Settings panel as a centered modal overlay with a split
    /// sidebar + content layout. Logo and Font merged into Appearance.
    ///
    /// Wide mode (≥640pt):
    ///   ┌──────────────┬───────────────────────────┐
    ///   │ Settings     │                           │
    ///   │ ─────────    │  (active category form)   │
    ///   │ ▸ Appearance │                           │
    ///   │   Terminal   │                           │
    ///   │   Input      │                           │
    ///   │   Keybindings│                           │
    ///   │   Window     │                           │
    ///   │   Advanced   │                           │
    ///   ├──────────────┴───────────────────────────┤
    ///   │ ↑↓ navigate  ⏎ apply  ←→ adjust  …      │
    ///   └──────────────────────────────────────────┘
    ///
    /// Narrow mode (<640pt): single column. Sidebar-only or content-only
    /// (drill-down). Esc returns from content to sidebar.
    pub(crate) fn build_settings_vertices(
        &self,
        s: crate::overlay::SettingsDrawParams<'_>,
    ) -> Vec<f32> {
        use crate::overlay::SettingsTab;

        let mut verts = Vec::new();
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let vp_w = self.viewport.0;
        let vp_h = self.viewport.1;
        if cw <= 0.0 || ch <= 0.0 || vp_w <= 0.0 || vp_h <= 0.0 {
            return verts;
        }

        let footer_pair_widths = crate::settings_component::settings_footer_widths(cw);
        let layout = crate::layout::layout_settings(
            vp_w,
            vp_h,
            cw,
            ch,
            SettingsTab::ALL.len(),
            s.error.is_some(),
            &footer_pair_widths,
            s.is_narrow,
            s.drill_down,
        )
        .expect("positive renderer geometry produces SettingsLayout");

        let ui = crate::ui_tokens::UiColors::from_theme(&self.theme)
            .with_increase_contrast(self.increase_contrast);
        let theme_bg = color_to_normalized(ui.canvas);
        let fg = color_to_normalized(ui.text_primary);
        let accent = color_to_normalized(ui.focus);
        let separator = color_to_normalized(ui.border_subtle);
        // Muted secondary text: 70% fg + 30% bg — always readable.
        let label_c = [
            fg[0] * 0.70 + theme_bg[0] * 0.30,
            fg[1] * 0.70 + theme_bg[1] * 0.30,
            fg[2] * 0.70 + theme_bg[2] * 0.30,
            1.0,
        ];
        let (su, sv, suw, svh) = self.space_uv();
        let bg_uv = [su, sv + svh, su + suw, sv];
        let border_c = [0.5, 0.5, 0.5, 0.20];
        let popup_bg = [
            theme_bg[0] + (1.0 - theme_bg[0]) * 0.08,
            theme_bg[1] + (1.0 - theme_bg[1]) * 0.08,
            theme_bg[2] + (1.0 - theme_bg[2]) * 0.08,
            1.0,
        ];
        // Sidebar is 4% lighter than canvas (subtly distinct from popup bg).
        let sidebar_bg = [
            theme_bg[0] + (1.0 - theme_bg[0]) * 0.04,
            theme_bg[1] + (1.0 - theme_bg[1]) * 0.04,
            theme_bg[2] + (1.0 - theme_bg[2]) * 0.04,
            1.0,
        ];
        let selection_bg = [
            accent[0] * 0.35 + theme_bg[0] * 0.65,
            accent[1] * 0.35 + theme_bg[1] * 0.65,
            accent[2] * 0.35 + theme_bg[2] * 0.65,
            1.0,
        ];
        let warning_c = [0.86, 0.65, 0.31, 1.0];

        let [box_x0, box_y0, box_x1, box_y1] = layout.box_rect;
        let content_x0 = layout.content_x0;
        let content_x1 = layout.content_x1;
        let content_cols = (((content_x1 - content_x0) / cw).max(1.0)) as usize;

        let shadow_pad = ch * 0.15;
        push_quad(
            &mut verts,
            [
                box_x0 - shadow_pad,
                box_y0 - shadow_pad,
                box_x1 + shadow_pad,
                box_y1 + shadow_pad,
            ],
            bg_uv,
            [0.0; 4],
            [0.0, 0.0, 0.0, 0.15],
        );
        push_quad(
            &mut verts,
            [box_x0, box_y0, box_x1, box_y1],
            bg_uv,
            [0.0; 4],
            popup_bg,
        );
        for (bx0, by0, bx1, by1) in [
            (box_x0, box_y0, box_x1, box_y0 + 1.0),
            (box_x0, box_y1 - 1.0, box_x1, box_y1),
            (box_x0, box_y0, box_x0 + 1.0, box_y1),
            (box_x1 - 1.0, box_y0, box_x1, box_y1),
        ] {
            push_quad(&mut verts, [bx0, by0, bx1, by1], bg_uv, [0.0; 4], border_c);
        }

        // Title row.
        let title_y = box_y0 + ch;
        // F5: in narrow content mode, show a back indicator.
        let title_text = if s.is_narrow && s.drill_down {
            "‹ Settings"
        } else {
            "Settings"
        };
        self.push_text(
            &mut verts,
            box_x0 + cw * 1.5,
            title_y,
            title_text,
            label_c,
            content_cols.max((((box_x1 - box_x0) / cw) as usize).max(1)),
        );

        // ── F5: Sidebar ──────────────────────────────────────────────
        if layout.show_sidebar {
            let [sx0, sy0, sx1, _] = layout.sidebar_rect;
            let sidebar_cols = (((sx1 - sx0) / cw).max(1.0)) as usize;
            let sidebar_bottom = layout.footer_y - ch * 0.4;

            // Sidebar background.
            push_quad(
                &mut verts,
                [sx0, sy0, sx1, sidebar_bottom],
                bg_uv,
                [0.0; 4],
                sidebar_bg,
            );

            // Category labels.
            for (i, tab) in SettingsTab::ALL.iter().enumerate() {
                let row_y = layout.sidebar_top + i as f32 * ch;
                if row_y + ch > sidebar_bottom {
                    break;
                }
                let is_active = *tab == s.active_tab;
                if is_active {
                    push_quad(
                        &mut verts,
                        [sx0, row_y, sx1, row_y + ch],
                        bg_uv,
                        [0.0; 4],
                        selection_bg,
                    );
                    // Left accent bar (3px wide).
                    let bar_w = 3.0 * self.scale as f32;
                    push_quad(
                        &mut verts,
                        [sx0, row_y, sx0 + bar_w, row_y + ch],
                        bg_uv,
                        [0.0; 4],
                        accent,
                    );
                }
                let color = if is_active { fg } else { label_c };
                self.push_text(
                    &mut verts,
                    sx0 + cw * 1.5,
                    row_y,
                    tab.label(),
                    color,
                    sidebar_cols,
                );
            }

            // Separator between sidebar and content (wide mode only).
            if layout.show_content {
                push_quad(
                    &mut verts,
                    [sx1, sy0, sx1 + 1.0, sidebar_bottom],
                    bg_uv,
                    [0.0; 4],
                    separator,
                );
            }
        }

        // ── F5: Content area ─────────────────────────────────────────
        if layout.show_content {
            // v1.5.1: Profile toolbar at top, error banner above it.
            self.draw_profile_toolbar(
                &mut verts,
                s.profiles,
                layout.profile_toolbar_rect,
                layout.profile_create_button,
                layout.profile_delete_button,
                cw,
                ch,
                bg_uv,
                theme_bg,
                fg,
                label_c,
                accent,
                separator,
            );
            if let Some(err) = s.error {
                let err_y = layout.profile_toolbar_rect[1] - ch;
                push_quad(
                    &mut verts,
                    [content_x0, err_y, content_x1, err_y + ch],
                    bg_uv,
                    [0.0; 4],
                    [0.65, 0.18, 0.18, 1.0],
                );
                self.push_text(
                    &mut verts,
                    content_x0 + cw * 0.3,
                    err_y,
                    &format!("\u{26a0} {err}"),
                    [1.0; 4],
                    content_cols,
                );
            }
            let content_top = layout.content_top;
            let max_rows = layout.max_rows;
            let value_x = (content_x0 + cw * 14.0).min(content_x1 - cw * 8.0);

            match s.active_tab {
                SettingsTab::Appearance => {
                    self.render_appearance_content(
                        &mut verts,
                        s,
                        content_top,
                        content_x0,
                        content_x1,
                        value_x,
                        cw,
                        ch,
                        content_cols,
                        max_rows,
                        bg_uv,
                        selection_bg,
                        fg,
                        label_c,
                        accent,
                        theme_bg,
                    );
                }
                SettingsTab::Terminal => {
                    let rows: [(&str, String, &str); 4] = [
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
                    ];
                    for (i, (label, value, err_label)) in rows.iter().enumerate() {
                        let row_y = content_top + i as f32 * ch;
                        let is_sel = i == s.selection;
                        self.push_settings_row(
                            &mut verts,
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
                    }
                }
                SettingsTab::Input => {
                    // v1.11.1 (PLAN_v1111 §4.6): row model shared with the
                    // adjust path in settings_validation.
                    let rows = crate::settings_validation::input_page_row_values(
                        s.submit_on_ctrl_enter,
                        s.smart_select,
                        s.paste_rows,
                    );
                    for (i, (label, value)) in rows.iter().enumerate() {
                        self.push_settings_row(
                            &mut verts,
                            content_top + i as f32 * ch,
                            label,
                            value,
                            s.selection == i,
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
                    // Help text.
                    let help_y = content_top + rows.len() as f32 * ch;
                    if help_y + ch < layout.footer_y {
                        self.push_text(
                            &mut verts,
                            content_x0,
                            help_y,
                            "Cmd+Shift+Click selects; Cmd+Option+Click safely opens.",
                            label_c,
                            content_cols,
                        );
                    }
                }
                SettingsTab::Keybindings => {
                    let mut list_top = content_top;
                    let validation_error = find_field_error(s.field_errors, "Keybindings");
                    if s.keybinding_conflict_count > 0 || validation_error.is_some() {
                        let badge = validation_error.map_or_else(
                            || format!("\u{26a0} {} conflict(s)", s.keybinding_conflict_count),
                            |error| format!("\u{26a0} {error}"),
                        );
                        self.push_text(
                            &mut verts,
                            content_x0,
                            list_top,
                            &badge,
                            warning_c,
                            content_cols,
                        );
                        list_top += ch;
                    }
                    let total = s.keybindings.len();
                    let available_rows =
                        ((layout.footer_y - ch * 0.5 - list_top) / ch).max(1.0) as usize;
                    let scrollable = total > available_rows;
                    let usable_rows = if scrollable {
                        available_rows.saturating_sub(1)
                    } else {
                        available_rows
                    };
                    let visible = usable_rows.min(total);
                    let mut offset = s.scroll_offset.min(total);
                    if s.selection < offset {
                        offset = s.selection;
                    } else if s.selection >= offset + visible {
                        offset = s.selection + 1 - visible;
                    }
                    let end = (offset + visible).min(total);
                    for (i, kb) in s.keybindings[offset..end].iter().enumerate() {
                        let row_y = list_top + i as f32 * ch;
                        let is_selected = offset + i == s.selection;
                        if is_selected {
                            push_quad(
                                &mut verts,
                                [content_x0, row_y, content_x1, row_y + ch],
                                bg_uv,
                                [0.0; 4],
                                selection_bg,
                            );
                        }
                        let action_color = if kb.conflict { warning_c } else { label_c };
                        self.push_text(
                            &mut verts,
                            content_x0,
                            row_y,
                            &kb.action,
                            action_color,
                            content_cols,
                        );
                        let binding_x = content_x1 - cw * 15.0;
                        let binding_color = if kb.conflict { warning_c } else { accent };
                        self.push_text(
                            &mut verts,
                            binding_x,
                            row_y,
                            &kb.binding,
                            binding_color,
                            content_cols,
                        );
                    }
                    if scrollable {
                        let indicator_y = list_top + visible as f32 * ch;
                        let mut indicator = String::new();
                        if offset > 0 {
                            indicator.push('\u{2191}');
                        }
                        if end < total {
                            if !indicator.is_empty() {
                                indicator.push(' ');
                            }
                            indicator.push('\u{2193}');
                        }
                        if !indicator.is_empty() {
                            indicator.push_str(" more");
                            self.push_text(
                                &mut verts,
                                content_x0,
                                indicator_y,
                                &indicator,
                                label_c,
                                content_cols,
                            );
                        }
                    }
                }
                SettingsTab::Window => {
                    let sidebar_w_str = match s.sidebar_width {
                        Some(w) => format!("{:.0} pt", w),
                        None => "Default".to_string(),
                    };
                    let rows: [(&str, String, &str); 3] = [
                        ("Width:", format!("{} px", s.window_width), "Window Width"),
                        (
                            "Height:",
                            format!("{} px", s.window_height),
                            "Window Height",
                        ),
                        ("Sidebar:", sidebar_w_str, "Sidebar Width"),
                    ];
                    for (i, (label, value, err_label)) in rows.iter().enumerate() {
                        let row_y = content_top + i as f32 * ch;
                        let is_sel = i == s.selection;
                        self.push_settings_row(
                            &mut verts,
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
                    }
                }
                SettingsTab::Advanced => {
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
                            &mut verts,
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
                            let badge_cols = content_cols
                                .saturating_sub(((badge_x - content_x0) / cw).max(0.0) as usize);
                            if badge_cols > 5 {
                                self.push_text(
                                    &mut verts,
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
                // v1.8.3: LocalAi tab — 8 rows: Enabled / Model / URL /
                // Max Tokens / Timeout / Cmd Generation / Error Diagnosis /
                // Test Connection (action button). All rows except row 7 use
                // the standard `push_settings_row` helper; row 7 gets a "▶"
                // glyph like Advanced's action rows and a status line below.
                SettingsTab::LocalAi => {
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
                            &mut verts,
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
                                &mut verts,
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
                                &mut verts,
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
        }

        let footer_y = layout.footer_y;
        push_quad(
            &mut verts,
            [
                box_x0 + cw * 1.5,
                footer_y - ch * 0.4,
                content_x1.max(box_x1 - cw * 1.5),
                footer_y - ch * 0.4 + 1.0,
            ],
            bg_uv,
            [0.0; 4],
            separator,
        );
        let footer_start = if s.is_narrow {
            box_x0 + cw * 1.5
        } else {
            content_x0
        };
        let footer_end = box_x1 - cw * 1.5;
        self.push_settings_footer_hints(
            &mut verts,
            footer_start,
            footer_end,
            footer_y,
            cw,
            ch,
            bg_uv,
            accent,
            label_c,
        );

        verts
    }

    /// F5: Render the Appearance category content.
    ///
    /// Contains the theme list, Logo Variant, Font Family, Font Size, Line
    /// Height, and Window Opacity. Theme rows show vector-drawn circles
    /// (current = filled accent, others = dim ring). Adjustment rows use the
    /// standard label/value/triangle layout.
    #[allow(clippy::too_many_arguments)]
    fn render_appearance_content(
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
        max_rows: usize,
        bg_uv: [f32; 4],
        selection_bg: [f32; 4],
        fg: [f32; 4],
        label_c: [f32; 4],
        accent: [f32; 4],
        _theme_bg: [f32; 4],
    ) {
        // Reserve 6 rows for the adjustment fields after the theme list
        // (Variant, Font, Size, Line, Opacity, Semantic toggle).
        let theme_count =
            crate::settings_component::visible_appearance_theme_count(s.themes.len(), max_rows);

        // Theme list with vector-drawn circles.
        for (i, theme) in s.themes.iter().take(theme_count).enumerate() {
            let row_y = content_top + i as f32 * ch;
            let is_current = theme.name == s.theme_name;
            let is_selected = i == s.selection;
            if is_selected {
                push_quad(
                    verts,
                    [content_x0, row_y, content_x1, row_y + ch],
                    bg_uv,
                    [0.0; 4],
                    selection_bg,
                );
            }
            // Vector circle: filled (current) or dim ring (others).
            let dot_cx = content_x0 + ch * 0.35;
            let dot_cy = row_y + ch * 0.5;
            let dot_r = ch * 0.16;
            let dot_color = if is_current {
                accent
            } else {
                [fg[0] * 0.3, fg[1] * 0.3, fg[2] * 0.3, 1.0]
            };
            let dot_lw = if is_current {
                1.5 * self.scale as f32
            } else {
                1.0 * self.scale as f32
            };
            let segments = 8;
            for s_idx in 0..segments {
                let a0 = s_idx as f32 * std::f32::consts::TAU / segments as f32;
                let a1 = (s_idx + 1) as f32 * std::f32::consts::TAU / segments as f32;
                push_line(
                    verts,
                    dot_cx + dot_r * a0.cos(),
                    dot_cy + dot_r * a0.sin(),
                    dot_cx + dot_r * a1.cos(),
                    dot_cy + dot_r * a1.sin(),
                    dot_lw,
                    dot_color,
                );
            }
            if is_current {
                push_quad(
                    verts,
                    [
                        dot_cx - dot_r * 0.4,
                        dot_cy - dot_r * 0.4,
                        dot_cx + dot_r * 0.4,
                        dot_cy + dot_r * 0.4,
                    ],
                    [0.0; 4],
                    [0.0; 4],
                    accent,
                );
            }
            let label_color = if is_current { accent } else { fg };
            self.push_text(
                verts,
                content_x0 + cw * 2.0,
                row_y,
                theme.label,
                label_color,
                content_cols,
            );
        }

        // Adjustment rows: Logo Variant, Font Family, Font Size, Line Height,
        // Opacity, Semantic Colors toggle.
        let semantic_label = if s.semantic_output_enabled {
            "On"
        } else {
            "Off"
        };
        let adj_rows: [(String, String, &str); 6] = [
            (
                "Variant:".to_string(),
                s.logo_variant.label().to_string(),
                "",
            ),
            ("Font:".to_string(), s.font_family.to_string(), ""),
            (
                "Size:".to_string(),
                format!("{:.1} pt", s.font_size),
                "Font Size",
            ),
            (
                "Line:".to_string(),
                format!("{:.2}", s.line_height),
                "Line Height",
            ),
            (
                "Opacity:".to_string(),
                format!("{:.2}", s.window_opacity),
                "Window Opacity",
            ),
            (
                "Semantic:".to_string(),
                semantic_label.to_string(),
                "Semantic Output",
            ),
        ];
        for (i, (label, value, err_label)) in adj_rows.iter().enumerate() {
            let row_idx = theme_count + i;
            let row_y = content_top + row_idx as f32 * ch;
            let is_sel = row_idx == s.selection;
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
                if err_label.is_empty() {
                    None
                } else {
                    find_field_error(s.field_errors, err_label)
                },
            );
        }
    }

    /// F5: Render a standard label/value row with selection highlight,
    /// triangles on the selected row, and an optional inline field error.
    #[allow(clippy::too_many_arguments)]
    fn push_settings_row(
        &self,
        verts: &mut Vec<f32>,
        row_y: f32,
        label: &str,
        value: &str,
        is_sel: bool,
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
        error_msg: Option<&str>,
    ) {
        let value_x = settings_value_x(value_x, content_x0, cw, Self::text_col_width(label));
        if is_sel {
            push_quad(
                verts,
                [content_x0, row_y, content_x1, row_y + ch],
                bg_uv,
                [0.0; 4],
                selection_bg,
            );
        }
        let label_color = if is_sel { fg } else { label_c };
        self.push_text(verts, content_x0, row_y, label, label_color, content_cols);
        let value_color = if is_sel { accent } else { fg };
        self.push_text(verts, value_x, row_y, value, value_color, content_cols);
        // Triangles on the selected row.
        if is_sel {
            let tri_h = ch * 0.14;
            let tri_w = ch * 0.10;
            let tri_cy = row_y + ch * 0.5;
            let val_text_w = cw * Self::text_col_width(value) as f32;
            let tri_lx = value_x - cw * 0.6;
            let tri_rx = value_x + val_text_w + cw * 0.6;
            push_filled_triangle(
                verts,
                tri_lx,
                tri_cy,
                tri_lx + tri_w,
                tri_cy - tri_h,
                tri_lx + tri_w,
                tri_cy + tri_h,
                accent,
            );
            push_filled_triangle(
                verts,
                tri_rx,
                tri_cy,
                tri_rx - tri_w,
                tri_cy - tri_h,
                tri_rx - tri_w,
                tri_cy + tri_h,
                accent,
            );
        }
        // Inline field error (red text after the value/triangles).
        if let Some(err) = error_msg {
            let val_text_w = cw * Self::text_col_width(value) as f32;
            let err_x = value_x + val_text_w + cw * 2.5;
            let err_cols =
                content_cols.saturating_sub(((err_x - content_x0) / cw).max(0.0) as usize);
            if err_cols > 3 {
                let msg = format!("\u{26a0} {}", err);
                self.push_text(verts, err_x, row_y, &msg, ERROR_COLOR, err_cols);
            }
        }
    }
}
