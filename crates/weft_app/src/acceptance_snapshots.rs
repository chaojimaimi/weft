//! Deterministic, GPU-free acceptance snapshots for shared layout and Scene data.
//!
//! These tests freeze the geometry contracts consumed by paint, hit-testing,
//! accessibility and PTY sizing. They intentionally snapshot logical values so
//! 1x and 2x displays must produce equivalent layouts without depending on
//! font rasterization or a live macOS window server.

use std::fmt::Debug;

use serde_json::{json, Value};
use weft_core::{config::Theme, grid::Color};

use crate::{
    layout::{
        layout_block_view, layout_panel, layout_settings, layout_tab_strip, LayoutCtx,
        SettingsLayout, TabStripInput,
    },
    overlay::SettingsTab,
    paint::command_surface::{build_command_surface_shell, CommandSurfaceShell},
    panel_component::build_panel_scene,
    scene::Scene,
    settings_component::{build_settings_scene, settings_footer_widths},
    tab_bar_component::build_tab_bar_scene,
    terminal_geometry::{GridGeometry, PhysicalRect, TerminalLayout},
    ui_tokens::{contrast_ratio, ResponsiveClass, SidebarMetrics, UiColors},
};

const LAYOUT_SCENE_GOLDEN: &str = include_str!("../tests/snapshots/layout_scene.json");
const THEME_CONTRAST_GOLDEN: &str = include_str!("../tests/snapshots/theme_contrast.json");
const LAYOUT_SCENE_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/snapshots/layout_scene.json"
);
const THEME_CONTRAST_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/snapshots/theme_contrast.json"
);

fn rounded(value: f64) -> f64 {
    (value * 1_000.0).round() / 1_000.0
}

fn rect32(rect: [f32; 4]) -> Value {
    json!(rect.map(|value| rounded(f64::from(value))))
}

fn rect64(rect: PhysicalRect, scale: f64) -> Value {
    json!([
        rounded(rect.left / scale),
        rounded(rect.top / scale),
        rounded(rect.right / scale),
        rounded(rect.bottom / scale),
    ])
}

fn normalized_terminal(layout: TerminalLayout, scale: f64) -> Value {
    json!({
        "viewport": rect64(layout.viewport, scale),
        "content": rect64(layout.content, scale),
        "sidebar": layout.sidebar.map(|rect| rect64(rect, scale)),
        "cell": [rounded(layout.cell_width / scale), rounded(layout.cell_height / scale)],
        "rows": layout.rows,
        "cols": layout.cols,
    })
}

fn terminal_layout(width: f64, height: f64, scale: f64, sidebar: f64) -> TerminalLayout {
    GridGeometry {
        viewport_width: width * scale,
        viewport_height: height * scale,
        cell_width: 9.0 * scale,
        cell_height: 18.0 * scale,
        padding_x: 8.0 * scale,
        padding_y: 8.0 * scale,
        chrome_top: 36.0 * scale,
        chrome_left: sidebar * scale,
    }
    .layout()
}

fn assert_scale_equivalent(one: TerminalLayout, two: TerminalLayout) {
    assert_eq!(
        normalized_terminal(one, 1.0),
        normalized_terminal(two, 2.0),
        "logical terminal geometry must be identical at 1x and 2x"
    );
}

fn scene_snapshot<T: Debug>(scene: &Scene<T>) -> Value {
    let hits = scene
        .hits
        .iter()
        .map(|hit| {
            json!({
                "target": format!("{:?}", hit.target),
                "bounds": rect32(hit.bounds()),
            })
        })
        .collect::<Vec<_>>();
    let semantics = scene
        .semantics
        .iter()
        .map(|node| {
            json!({
                "role": format!("{:?}", node.role),
                "label": node.label,
                "bounds": rect32(node.bounds),
                "focus": node.focus.map(|focus| format!("{:?}", focus)),
                "state": node.state,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "hit_count": hits.len(),
        "semantic_count": semantics.len(),
        "hits": hits,
        "semantics": semantics,
    })
}

fn settings_json(layout: &SettingsLayout, scene: Value) -> Value {
    json!({
        "box": rect32(layout.box_rect),
        "sidebar": rect32(layout.sidebar_rect),
        "show_sidebar": layout.show_sidebar,
        "show_content": layout.show_content,
        "content_x": [rounded(f64::from(layout.content_x0)), rounded(f64::from(layout.content_x1))],
        "content_top": rounded(f64::from(layout.content_top)),
        "max_rows": layout.max_rows,
        "footer_y": rounded(f64::from(layout.footer_y)),
        "scene": scene,
    })
}

fn layout_scene_snapshot() -> Value {
    let windows = [
        ("minimum", 360.0, 240.0),
        ("regular", 800.0, 600.0),
        ("wide", 1440.0, 900.0),
    ];
    let geometry = windows.map(|(name, width, height)| {
        let class = ResponsiveClass::from_logical_width(width as f32);
        let metrics = SidebarMetrics::for_logical_width(width as f32);
        let sidebar = if class == ResponsiveClass::Compact {
            0.0
        } else {
            f64::from(metrics.push_width)
        };
        let one = terminal_layout(width, height, 1.0, sidebar);
        let two = terminal_layout(width, height, 2.0, sidebar);
        assert_scale_equivalent(one, two);
        json!({
            "name": name,
            "window": [width, height],
            "responsive": format!("{:?}", class),
            "sidebar_panel_width": rounded(f64::from(metrics.panel_width)),
            "sidebar_push_width": rounded(f64::from(metrics.push_width)),
            "logical_terminal": normalized_terminal(one, 1.0),
            "scale_equivalent": true,
        })
    });

    let tabs = [1_usize, 3, 10, 30].map(|count| {
        let strip = layout_tab_strip(TabStripInput {
            viewport_width: 800.0,
            bar_height: 36.0,
            cell_width: 9.0,
            padding_x: 8.0,
            chrome_left: 0.0,
            traffic_lights_width: 72.0,
            tab_count: count,
            requested_scroll_offset: f32::MAX,
        });
        let scene = build_tab_bar_scene(strip, count, 9.0, 18.0);
        for index in 0..count {
            let rect = strip.tab_rect(index);
            assert!(
                rect[0] <= rect[2] && rect[1] <= rect[3],
                "tab {index}/{count} produced inverted Scene bounds: {rect:?}"
            );
        }
        let indices = [0, count / 2, count - 1];
        json!({
            "count": count,
            "tab_width": rounded(f64::from(strip.tab_width)),
            "overflowing": strip.overflowing,
            "scroll_offset": rounded(f64::from(strip.scroll_offset)),
            "max_scroll": rounded(f64::from(strip.max_scroll)),
            "visible": [rounded(f64::from(strip.visible_left)), rounded(f64::from(strip.visible_right))],
            "plus": rect32(strip.plus_rect),
            "sample_tabs": indices.map(|index| json!({"index": index, "bounds": rect32(strip.tab_rect(index))})),
            "scene": scene_snapshot(&scene),
        })
    });

    let ctx = LayoutCtx {
        viewport: (800.0, 600.0),
        cell_w: 9.0,
        cell_h: 18.0,
        padding_x: 8.0,
        padding_y: 8.0,
        chrome_top: 36.0,
        chrome_left: 240.0,
        pane_origin: (0.0, 0.0),
        clip: None,
    };
    let block = layout_block_view(&ctx, 540.0, true);
    let panel = layout_panel(36.0, 9.0, 18.0, 240.0, 600.0);
    let panel_scene = build_panel_scene(
        panel.panel_rect,
        panel.search_field_rect,
        panel.list_top,
        panel.row_height,
        12,
    );

    let footer_widths = settings_footer_widths(9.0);
    let wide_settings = layout_settings(
        1440.0,
        900.0,
        9.0,
        18.0,
        3,
        false,
        &footer_widths,
        false,
        false,
    )
    .expect("wide settings layout");
    assert!(wide_settings.footer_buttons.apply.is_some());
    assert!(wide_settings.footer_buttons.close.is_some());
    assert!(wide_settings.footer_buttons.save.is_some());
    let narrow_sidebar = layout_settings(
        500.0,
        700.0,
        9.0,
        18.0,
        3,
        false,
        &footer_widths,
        true,
        false,
    )
    .expect("narrow settings sidebar layout");
    assert!(narrow_sidebar.show_sidebar);
    assert!(!narrow_sidebar.show_content);
    let narrow_content =
        layout_settings(500.0, 700.0, 9.0, 18.0, 3, true, &footer_widths, true, true)
            .expect("narrow settings content layout");
    assert!(!narrow_content.show_sidebar);
    assert!(narrow_content.show_content);
    let settings_tabs = SettingsTab::ALL;
    let wide_scene = build_settings_scene(
        &wide_settings,
        &settings_tabs,
        SettingsTab::Appearance,
        (11, 17, 0),
        18.0,
        // v1.5.1: profile_count = 0 for snapshot tests.
        0,
    );
    let narrow_sidebar_scene = build_settings_scene(
        &narrow_sidebar,
        &settings_tabs,
        SettingsTab::Appearance,
        (0, 0, 0),
        18.0,
        0,
    );
    let narrow_content_scene = build_settings_scene(
        &narrow_content,
        &settings_tabs,
        SettingsTab::Appearance,
        (11, 17, 0),
        18.0,
        0,
    );

    let shell = CommandSurfaceShell::canonical(
        [180.0, 120.0, 620.0, 420.0],
        6.0,
        true,
        [0.1, 0.08, 0.06, 1.0],
        [0.0, 0.0, 1.0, 1.0],
    );
    let mut shell_vertices = Vec::new();
    build_command_surface_shell(&mut shell_vertices, shell);

    json!({
        "schema": 1,
        "geometry": geometry,
        "tabs": tabs,
        "block_sidebar": {
            "block": {
                "pitch": rounded(f64::from(block.pitch)),
                "bounds": [rounded(f64::from(block.left)), rounded(f64::from(block.clip_top)), rounded(f64::from(block.right)), rounded(f64::from(block.clip_bottom))],
                "cols": block.cols,
                "fixed_cwd_y": rounded(f64::from(block.fixed_cwd_y)),
            },
            "panel": {
                "rect": rect32(panel.panel_rect),
                "search": rect32(panel.search_field_rect),
                "list_top": rounded(f64::from(panel.list_top)),
                "row_height": rounded(f64::from(panel.row_height)),
                "scene": scene_snapshot(&panel_scene),
            },
        },
        "command_surface": {
            "popup": rect32(shell.popup_rect),
            "shadow_pad": shell.shadow_pad,
            "resize_handles": shell.with_resize_handles,
            "border": shell.border_color,
            "background": shell.bg_color.map(|value| rounded(f64::from(value))),
            "vertex_floats": shell_vertices.len(),
        },
        "settings": {
            "wide": settings_json(&wide_settings, scene_snapshot(&wide_scene)),
            "narrow_sidebar": settings_json(&narrow_sidebar, scene_snapshot(&narrow_sidebar_scene)),
            "narrow_content": settings_json(&narrow_content, scene_snapshot(&narrow_content_scene)),
        },
    })
}

fn built_in_themes() -> [(&'static str, Theme); 11] {
    [
        ("weft-warm", Theme::weft_warm()),
        ("weft-light", Theme::weft_light()),
        ("warp-dark", Theme::warp_dark()),
        ("dracula", Theme::dracula()),
        ("solarized-dark", Theme::solarized_dark()),
        ("gruvbox-dark", Theme::gruvbox_dark()),
        ("nord", Theme::nord()),
        ("tokyo-night", Theme::tokyo_night()),
        ("catppuccin-mocha", Theme::catppuccin_mocha()),
        ("one-dark", Theme::one_dark()),
        ("monokai-pro", Theme::monokai_pro()),
    ]
}

#[test]
fn built_in_themes_keep_ansi_bright_colors_visible_on_the_canvas() {
    for (name, theme) in built_in_themes() {
        for index in 8..16 {
            let ratio = contrast_ratio(theme.palette[index], theme.background);
            assert!(
                ratio >= 1.5,
                "{name} ANSI bright slot {index} contrast {ratio:.3} is too close to its background"
            );
        }
    }
}

fn composite_over(foreground: Color, background: Color, alpha: f64) -> Color {
    let channel = |foreground: u8, background: u8| {
        (f64::from(background) + (f64::from(foreground) - f64::from(background)) * alpha).round()
            as u8
    };
    Color::rgb(
        channel(foreground.r, background.r),
        channel(foreground.g, background.g),
        channel(foreground.b, background.b),
    )
}

fn theme_contrast_snapshot() -> Value {
    let themes = built_in_themes().map(|(name, theme)| {
        let colors = UiColors::from_theme(&theme);
        let increased = colors.with_increase_contrast(true);
        let composited_find = composite_over(colors.find_match, colors.canvas, 0.50);
        let composited_increased_find = composite_over(increased.find_match, colors.canvas, 0.50);
        let text_pairs = [
            ("primary_canvas", colors.text_primary, colors.canvas),
            ("primary_chrome", colors.text_primary, colors.chrome),
            ("primary_raised", colors.text_primary, colors.raised),
            ("primary_panel", colors.text_primary, colors.panel),
            ("secondary_canvas", colors.text_secondary, colors.canvas),
            ("secondary_chrome", colors.text_secondary, colors.chrome),
            ("secondary_raised", colors.text_secondary, colors.raised),
            ("secondary_panel", colors.text_secondary, colors.panel),
            ("focus", colors.focus, colors.canvas),
            ("focus_panel", colors.focus, colors.panel),
            ("success", colors.success, colors.canvas),
            ("success_panel", colors.success, colors.panel),
            ("warning", colors.warning, colors.canvas),
            ("warning_panel", colors.warning, colors.panel),
            ("error", colors.error, colors.canvas),
            ("error_panel", colors.error, colors.panel),
            (
                "panel_selection_text",
                colors.selection_text,
                colors.selection,
            ),
        ];
        let indicator_pairs = [
            ("find_match_composited", composited_find, colors.canvas),
            (
                "increased_find_match_composited",
                composited_increased_find,
                colors.canvas,
            ),
            ("increased_border", increased.border_subtle, colors.canvas),
            ("panel_selection", colors.selection, colors.panel),
        ];
        for (pair, foreground, background) in text_pairs {
            let ratio = contrast_ratio(foreground, background);
            assert!(
                ratio >= 4.5,
                "{name} {pair} contrast {ratio:.3} is below WCAG AA 4.5:1"
            );
        }
        for (pair, foreground, background) in indicator_pairs {
            let ratio = contrast_ratio(foreground, background);
            assert!(
                ratio >= 3.0,
                "{name} {pair} contrast {ratio:.3} is below WCAG non-text 3:1"
            );
        }
        json!({
            "name": name,
            "text": text_pairs.map(|(pair, foreground, background)| json!({
                "pair": pair,
                "ratio": rounded(contrast_ratio(foreground, background)),
            })),
            "indicators": indicator_pairs.map(|(pair, foreground, background)| json!({
                "pair": pair,
                "ratio": rounded(contrast_ratio(foreground, background)),
            })),
        })
    });
    json!({"schema": 1, "wcag": {"normal_text": 4.5, "non_text": 3.0}, "themes": themes})
}

fn assert_golden(actual: Value, expected: &str, name: &str, path: &str) {
    if std::env::var_os("WEFT_UPDATE_GOLDENS").as_deref() == Some(std::ffi::OsStr::new("1")) {
        let contents = serde_json::to_string_pretty(&actual).expect("serialize golden") + "\n";
        std::fs::write(path, contents).expect("write golden");
        return;
    }
    let expected: Value = serde_json::from_str(expected)
        .unwrap_or_else(|error| panic!("invalid {name} golden JSON: {error}"));
    assert_eq!(
        actual, expected,
        "{name} golden changed; inspect geometry before accepting it"
    );
}

#[test]
fn layout_and_scene_contracts_match_golden() {
    assert_golden(
        layout_scene_snapshot(),
        LAYOUT_SCENE_GOLDEN,
        "layout/scene",
        LAYOUT_SCENE_PATH,
    );
}

#[test]
fn all_built_in_themes_meet_contrast_gate_and_match_golden() {
    assert_golden(
        theme_contrast_snapshot(),
        THEME_CONTRAST_GOLDEN,
        "theme contrast",
        THEME_CONTRAST_PATH,
    );
}
