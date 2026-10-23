//! Scene model and hit testing for the Settings panel.
//!
//! Uses a typed `Scene<SettingsTarget>` instead of renderer-retained hit
//! state. Layout comes from `layout::layout_settings`;
//! the renderer's `build_settings_vertices` and the mouse handler share the
//! same geometry product.

use crate::layout::{Rect, SettingsLayout};
use crate::overlay::SettingsTab;
use crate::scene::{FocusId, HitRegion, Scene, SemanticNode, SemanticRole};

/// A clickable region of the settings panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettingsTarget {
    /// F5: A sidebar category entry (Appearance / Terminal / Input / ...).
    SidebarCategory(SettingsTab),
    /// A theme row in the Appearance category (0-based screen-relative index).
    Theme(usize),
    /// A non-theme content row. The index matches `SettingsState::selection`.
    ContentRow(usize),
    /// Footer "⏎ apply" button.
    ApplyButton,
    /// Footer "esc close" button.
    CloseButton,
    /// Footer "⌘⏎ save" button.
    SaveButton,
    /// v1.5.1: A profile entry in the Settings profile toolbar. The index
    /// is into the per-frame sorted profile view (BTreeMap order). Click
    /// resolves the name from the same view; an out-of-bounds index is a
    /// no-op (safe hit test).
    ProfileEntry(usize),
    /// v1.5.1: The "New" button in the profile toolbar. Creates a new
    /// profile with a default name and switches to it.
    ProfileCreate,
    /// v1.5.1: The "Delete" button in the profile toolbar. Deletes the
    /// currently-active profile. The controller implements a two-click
    /// confirmation: the first click sets a pending-delete state, the
    /// second click within the same Settings session confirms.
    ProfileDelete,
    /// v1.5.2: Advanced → "Import Config:" row. Enter triggers the
    /// NSOpenPanel flow (see `App::import_config_interactive`).
    AdvancedImport,
    /// v1.5.2: Advanced → "Export Config:" row. Enter triggers the
    /// NSSavePanel flow (see `App::export_config_interactive`).
    AdvancedExport,
    /// v1.8.3: LocalAi → "Test Connection:" row. Enter/click spawns a
    /// `/api/tags` refresh and updates the connection status line.
    LocalAiTestConnection,
    /// v1.13.0: Update "Check Now" row — one-shot Sparkle check (plan D4).
    UpdateCheckNow,
}

pub(crate) const APPEARANCE_ADJUSTMENT_ROWS: usize = 6;

/// Appearance keeps all adjustment controls visible and gives the remaining
/// rows to themes. Every settings layer uses this helper so the painted row,
/// hit target, and keyboard selection cannot drift apart in short windows.
pub(crate) fn visible_appearance_theme_count(theme_count: usize, max_rows: usize) -> usize {
    theme_count.min(max_rows.saturating_sub(APPEARANCE_ADJUSTMENT_ROWS))
}

pub(crate) fn settings_scene_row_window(
    tab: SettingsTab,
    total_rows: usize,
    total_themes: usize,
    max_rows: usize,
    scroll_offset: usize,
) -> (usize, usize, usize) {
    let theme_count = if tab == SettingsTab::Appearance {
        visible_appearance_theme_count(total_themes, max_rows)
    } else {
        0
    };
    let start = if tab == SettingsTab::Keybindings {
        scroll_offset.min(total_rows)
    } else {
        0
    };
    (
        theme_count,
        total_rows.saturating_sub(start).min(max_rows),
        start,
    )
}

/// v1.13.17 S2: hover predicate for settings rows — the stored hover target
/// must equal `target`, and selection wins when both land on the same row
/// (the shared `CommandSurfaceRowState` branch order). `ContentRow`'s index
/// space is `SettingsState::selection`'s (the scene registers rows in
/// selection space; the Keybindings tab adds `scroll_offset`), so paint
/// sites pass the same row index they compare against `selection`.
pub(crate) fn settings_row_hovered(
    hovered: Option<SettingsTarget>,
    target: SettingsTarget,
    is_selected: bool,
) -> bool {
    !is_selected && hovered == Some(target)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SettingsFooterHint {
    pub keys: &'static [&'static str],
    pub description: &'static str,
}

pub(crate) const SETTINGS_FOOTER_HINTS: [SettingsFooterHint; 6] = [
    SettingsFooterHint {
        keys: &["↑", "↓"],
        description: "navigate",
    },
    SettingsFooterHint {
        keys: &["⏎"],
        description: "apply",
    },
    SettingsFooterHint {
        keys: &["⇥"],
        description: "switch",
    },
    SettingsFooterHint {
        keys: &["←", "→"],
        description: "adjust",
    },
    SettingsFooterHint {
        keys: &["esc"],
        description: "close",
    },
    SettingsFooterHint {
        keys: &["⌘", "⏎"],
        description: "save",
    },
];

pub(crate) const KEYCAP_PAD_X_CELLS: f32 = 0.35;
pub(crate) const KEYCAP_GAP_CELLS: f32 = 0.30;
pub(crate) const KEYCAP_DESCRIPTION_GAP_CELLS: f32 = 0.65;
pub(crate) const FOOTER_HINT_GAP_CELLS: f32 = 1.25;

pub(crate) fn settings_keycap_width(key: &str, cell_w: f32) -> f32 {
    let columns = if matches!(key, "↑" | "↓" | "←" | "→") {
        1.0
    } else {
        weft_core::grid::terminal_text_width(key) as f32
    };
    cell_w * (columns + KEYCAP_PAD_X_CELLS * 2.0)
}

pub(crate) fn settings_key_group_width(keys: &[&str], cell_w: f32) -> f32 {
    let key_width: f32 = keys
        .iter()
        .map(|key| settings_keycap_width(key, cell_w))
        .sum();
    key_width + cell_w * KEYCAP_GAP_CELLS * keys.len().saturating_sub(1) as f32
}

pub(crate) fn settings_footer_hint_width(hint: SettingsFooterHint, cell_w: f32) -> f32 {
    let description_cols = weft_core::grid::terminal_text_width(hint.description) as f32;
    settings_key_group_width(hint.keys, cell_w)
        + cell_w * (KEYCAP_DESCRIPTION_GAP_CELLS + description_cols)
}

pub(crate) fn settings_footer_widths(cell_w: f32) -> [f32; 6] {
    SETTINGS_FOOTER_HINTS.map(|hint| settings_footer_hint_width(hint, cell_w))
}

pub(crate) fn settings_value_x(
    base_x: f32,
    content_x0: f32,
    cell_w: f32,
    label_cols: usize,
) -> f32 {
    base_x.max(content_x0 + cell_w * (label_cols as f32 + 1.0))
}

/// Build the settings Scene from a shared layout product + dynamic state.
/// `theme_count` is the number of visible theme rows (already capped by
/// `layout.max_rows`). `tabs` is the ordered list of settings categories.
/// `cell_h` is the physical cell height (drives sidebar row heights).
/// `active_tab` is the currently-selected sidebar category (for highlight).
/// `profile_count` is the number of profile entries to register for hit
/// testing in the v1.5.1 profile toolbar (0 hides the profile entry hits
/// even when the toolbar rect is non-zero — the toolbar still draws the
/// "Base" label and +/- buttons).
pub(crate) fn build_settings_scene(
    layout: &SettingsLayout,
    tabs: &[SettingsTab],
    active_tab: SettingsTab,
    row_window: (usize, usize, usize),
    cell_h: f32,
    profile_count: usize,
) -> Scene<SettingsTarget> {
    let (theme_count, content_row_count, content_row_start) = row_window;
    let mut scene = Scene::default();
    let [box_x0, _box_y0, _box_x1, _box_y1] = layout.box_rect;

    scene.semantics.push(SemanticNode {
        role: SemanticRole::Dialog,
        label: "Settings".into(),
        bounds: layout.box_rect,
        focus: Some(FocusId::Settings),
        state: String::new(),
    });

    // F5: Sidebar category hit regions (vertical list).
    if layout.show_sidebar {
        for (i, tab) in tabs.iter().enumerate() {
            let row_y = layout.sidebar_top + i as f32 * cell_h;
            let row_rect: Rect = [
                layout.sidebar_rect[0],
                row_y,
                layout.sidebar_rect[2],
                row_y + cell_h,
            ];
            scene.hits.push(HitRegion::from_rect(
                row_rect,
                SettingsTarget::SidebarCategory(*tab),
            ));
            scene.semantics.push(SemanticNode {
                role: SemanticRole::Tab,
                label: tab.label().into(),
                bounds: row_rect,
                focus: Some(FocusId::Settings),
                state: if *tab == active_tab {
                    "selected".into()
                } else {
                    String::new()
                },
            });
        }
    }

    // v1.5.1: Profile toolbar hit regions. The toolbar sits at the top of
    // the content area (above `content_top`). Hit regions:
    // - ProfileEntry(0..profile_count): click to switch to that profile.
    //   The "Base" entry is index 0; profiles follow at 1..=profile_count.
    // - ProfileCreate: the "+" button.
    // - ProfileDelete: the "−" button.
    //
    // The hit regions are only registered when the content area is
    // visible (wide mode or narrow content view). In narrow sidebar-only
    // mode the toolbar is zero-sized and the buttons are None.
    if layout.show_content {
        let [tx0, ty0, tx1, ty1] = layout.profile_toolbar_rect;
        if tx1 > tx0 {
            // Profile entry hits: divide the toolbar (minus the button area
            // on the right) into profile_count equal-width slots. Each slot
            // is a click target. The renderer draws the names; here we only
            // register the geometry.
            if profile_count > 0 {
                let btn_w = cell_h * 1.5;
                let gap = cell_h * 0.5;
                let buttons_total = btn_w * 2.0 + gap;
                let entries_area_w = (tx1 - tx0 - buttons_total).max(0.0);
                let slot_w = entries_area_w / profile_count as f32;
                for i in 0..profile_count {
                    let slot_x0 = tx0 + i as f32 * slot_w;
                    let slot_x1 = slot_x0 + slot_w;
                    scene.hits.push(HitRegion::from_rect(
                        [slot_x0, ty0, slot_x1, ty1],
                        SettingsTarget::ProfileEntry(i),
                    ));
                }
            }
            // + and − buttons.
            if let Some(btn) = layout.profile_create_button {
                scene
                    .hits
                    .push(HitRegion::from_rect(btn, SettingsTarget::ProfileCreate));
            }
            if let Some(btn) = layout.profile_delete_button {
                scene
                    .hits
                    .push(HitRegion::from_rect(btn, SettingsTarget::ProfileDelete));
            }
            scene.semantics.push(SemanticNode {
                role: SemanticRole::List,
                label: "Profile selector".into(),
                bounds: layout.profile_toolbar_rect,
                focus: Some(FocusId::Settings),
                state: String::new(),
            });
        }
    }

    // Theme row hits (Appearance only — theme_count is 0 for other categories).
    if layout.show_content && theme_count > 0 {
        for i in 0..theme_count {
            let row_y = layout.content_top + i as f32 * cell_h;
            let row_rect: Rect = [layout.content_x0, row_y, layout.content_x1, row_y + cell_h];
            scene
                .hits
                .push(HitRegion::from_rect(row_rect, SettingsTarget::Theme(i)));
            scene.semantics.push(SemanticNode {
                role: SemanticRole::ListItem,
                label: format!("Theme {}", i + 1),
                bounds: row_rect,
                focus: None,
                state: String::new(),
            });
        }
    }

    // Standard rows share the keyboard selection index; keybindings may scroll.
    if layout.show_content {
        for visible_index in 0..content_row_count.min(layout.max_rows) {
            let row = content_row_start + visible_index;
            if active_tab == SettingsTab::Appearance && row < theme_count {
                continue;
            }
            // Advanced's first two entries are non-config placeholders and
            // rows 2/3 have dedicated action targets below.
            if active_tab == SettingsTab::Advanced {
                continue;
            }
            // LocalAi row 7 / Update row 2 (v1.13.0): action rows, dedicated targets below.
            if (active_tab == SettingsTab::LocalAi && row == 7)
                || (active_tab == SettingsTab::Update && row == 2)
            {
                continue;
            }
            let row_y = layout.content_top + visible_index as f32 * cell_h;
            let row_rect: Rect = [layout.content_x0, row_y, layout.content_x1, row_y + cell_h];
            scene.hits.push(HitRegion::from_rect(
                row_rect,
                SettingsTarget::ContentRow(row),
            ));
            scene.semantics.push(SemanticNode {
                role: SemanticRole::ListItem,
                label: format!("Setting row {}", row + 1),
                bounds: row_rect,
                focus: Some(FocusId::Settings),
                state: String::new(),
            });
        }
    }

    // v1.8.3: LocalAi → "Test Connection" action row (row 7). Enter/click
    // spawns a `/api/tags` refresh; the status line below shows the result.
    if layout.show_content && active_tab == SettingsTab::LocalAi {
        let row_idx = 7;
        if row_idx < layout.max_rows {
            let row_y = layout.content_top + row_idx as f32 * cell_h;
            let row_rect: Rect = [layout.content_x0, row_y, layout.content_x1, row_y + cell_h];
            scene.hits.push(HitRegion::from_rect(
                row_rect,
                SettingsTarget::LocalAiTestConnection,
            ));
            scene.semantics.push(SemanticNode {
                role: SemanticRole::Button,
                label: "Test AI connection".into(),
                bounds: row_rect,
                focus: Some(FocusId::Settings),
                state: String::new(),
            });
        }
    }

    // v1.13.0: Update "Check Now" hit + semantics (LocalAi row-7 twin, plan D4).
    if layout.show_content && active_tab == SettingsTab::Update && layout.max_rows > 2 {
        let row_y = layout.content_top + 2.0 * cell_h;
        let row_rect: Rect = [layout.content_x0, row_y, layout.content_x1, row_y + cell_h];
        let target = SettingsTarget::UpdateCheckNow;
        scene.hits.push(HitRegion::from_rect(row_rect, target));
        scene.semantics.push(SemanticNode {
            role: SemanticRole::Button,
            label: "Check for updates".into(),
            bounds: row_rect,
            focus: Some(FocusId::Settings),
            state: String::new(),
        });
    }

    // v1.5.2: Advanced → Import/Export action rows. Rows 2 and 3 in the
    // Advanced category trigger the NSOpenPanel/NSSavePanel flow on click
    // (matching Enter on the selected row). The first two rows (Debug
    // Logging, Experimental) are placeholders and don't register hits —
    // Enter on them is a no-op, just like before.
    if layout.show_content && active_tab == SettingsTab::Advanced {
        for (i, target) in [
            SettingsTarget::AdvancedImport,
            SettingsTarget::AdvancedExport,
        ]
        .iter()
        .enumerate()
        {
            let row_idx = 2 + i; // rows 2 and 3
            let row_y = layout.content_top + row_idx as f32 * cell_h;
            let row_rect: Rect = [layout.content_x0, row_y, layout.content_x1, row_y + cell_h];
            scene.hits.push(HitRegion::from_rect(row_rect, *target));
            scene.semantics.push(SemanticNode {
                role: SemanticRole::Button,
                label: if i == 0 {
                    "Import config".into()
                } else {
                    "Export config".into()
                },
                bounds: row_rect,
                focus: Some(FocusId::Settings),
                state: String::new(),
            });
        }
        // v1.11.5 (PLAN_v1115 §M8): rows 4..ADVANCED_ROW_COUNT are the new
        // notification / clipboard rows — clickable like standard rows
        // (select; then ←/→ adjusts; Enter saves).
        for row in 4..crate::settings_validation::ADVANCED_ROW_COUNT {
            let row_y = layout.content_top + row as f32 * cell_h;
            let row_rect: Rect = [layout.content_x0, row_y, layout.content_x1, row_y + cell_h];
            scene.hits.push(HitRegion::from_rect(
                row_rect,
                SettingsTarget::ContentRow(row),
            ));
            scene.semantics.push(SemanticNode {
                role: SemanticRole::ListItem,
                label: format!("Setting row {}", row + 1),
                bounds: row_rect,
                focus: Some(FocusId::Settings),
                state: String::new(),
            });
        }
    }

    // Footer buttons.
    if let Some(apply) = layout.footer_buttons.apply {
        scene
            .hits
            .push(HitRegion::from_rect(apply, SettingsTarget::ApplyButton));
        scene.semantics.push(SemanticNode {
            role: SemanticRole::Button,
            label: "Apply settings".into(),
            bounds: apply,
            focus: Some(FocusId::Settings),
            state: String::new(),
        });
    }
    if let Some(close) = layout.footer_buttons.close {
        scene
            .hits
            .push(HitRegion::from_rect(close, SettingsTarget::CloseButton));
        scene.semantics.push(SemanticNode {
            role: SemanticRole::Button,
            label: "Close settings".into(),
            bounds: close,
            focus: Some(FocusId::Settings),
            state: String::new(),
        });
    }
    if let Some(save) = layout.footer_buttons.save {
        scene
            .hits
            .push(HitRegion::from_rect(save, SettingsTarget::SaveButton));
        scene.semantics.push(SemanticNode {
            role: SemanticRole::Button,
            label: "Save settings".into(),
            bounds: save,
            focus: Some(FocusId::Settings),
            state: String::new(),
        });
    }

    // Suppress unused-variable warning for box_x0 (kept for clarity).
    let _ = box_x0;

    scene
}

/// Resolve a physical-pixel point to the topmost settings target.
pub(crate) fn settings_target_at(
    scene: &Scene<SettingsTarget>,
    x: f32,
    y: f32,
) -> Option<SettingsTarget> {
    scene
        .hits
        .iter()
        .find(|hit| hit.contains_half_open(x, y))
        .map(|hit| hit.target)
}

// Tests live in the gate-exempt sibling module (repo test-module
// convention, pty/tests.rs precedent) so inline test lines stay out of
// the production-file budget.
#[cfg(test)]
#[path = "settings_component/tests.rs"]
mod tests;
