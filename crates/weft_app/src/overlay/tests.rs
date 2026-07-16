use super::*;

fn make_layer(z: OverlayZ, policy: OverlayInputPolicy) -> OverlayLayer<'static> {
    OverlayLayer {
        kind: OverlayKind::Prompt,
        z,
        input_policy: policy,
        content: OverlayContent::Prompt(PromptDrawParams {
            cwd: None,
            lines: &[],
            cursor: (0, 0),
            preedit: None,
            preedit_cursor: None,
            search: None,
            selection: None,
            scroll_offset: 0,
            submit_on_ctrl_enter: false,
        }),
    }
}

#[test]
fn hit_region_contains_point_inside() {
    let r = HitRegion {
        x0: 10.0,
        y0: 20.0,
        x1: 50.0,
        y1: 60.0,
        target: HitTarget::CompletionItem(0),
    };
    assert!(r.contains(30.0, 40.0));
    assert!(r.contains(10.0, 20.0)); // top-left corner inclusive
    assert!(r.contains(50.0, 60.0)); // bottom-right inclusive
}

#[test]
fn hit_region_contains_point_outside() {
    let r = HitRegion {
        x0: 10.0,
        y0: 20.0,
        x1: 50.0,
        y1: 60.0,
        target: HitTarget::CompletionItem(0),
    };
    assert!(!r.contains(5.0, 30.0)); // left of region
    assert!(!r.contains(55.0, 30.0)); // right of region
    assert!(!r.contains(30.0, 10.0)); // above
    assert!(!r.contains(30.0, 70.0)); // below
}

#[test]
fn overlay_stack_layers_in_insertion_order() {
    // layers_sorted preserves push order (z-ascending).
    let stack = OverlayStack {
        layers: vec![make_layer(OverlayZ::Panel, OverlayInputPolicy::Focused)],
    };
    let mut it = stack.layers_sorted();
    assert_eq!(it.next().unwrap().z, OverlayZ::Panel);
    assert!(it.next().is_none());
}

#[test]
fn overlay_stack_topmost_modal_returns_highest_z_modal() {
    let stack = OverlayStack {
        layers: vec![
            make_layer(OverlayZ::Prompt, OverlayInputPolicy::Focused),
            make_layer(OverlayZ::Palette, OverlayInputPolicy::Modal),
        ],
    };
    let top = stack.topmost_modal();
    assert!(top.is_some());
    assert_eq!(top.unwrap().z, OverlayZ::Palette);
}

#[test]
fn overlay_stack_topmost_modal_none_when_no_modal() {
    let stack = OverlayStack {
        layers: vec![
            make_layer(OverlayZ::Prompt, OverlayInputPolicy::Focused),
            make_layer(OverlayZ::Panel, OverlayInputPolicy::Passive),
        ],
    };
    assert!(stack.topmost_modal().is_none());
}

#[test]
fn overlay_stack_topmost_modal_picks_highest_of_multiple() {
    let stack = OverlayStack {
        layers: vec![
            make_layer(OverlayZ::Completion, OverlayInputPolicy::Modal),
            make_layer(OverlayZ::Palette, OverlayInputPolicy::Modal),
        ],
    };
    let top = stack.topmost_modal();
    assert_eq!(top.unwrap().z, OverlayZ::Palette);
}

#[test]
fn overlay_warmup_prompt_collects_chars() {
    let prompt = PromptDrawParams {
        cwd: Some("/home"),
        lines: &["hello".to_string()],
        cursor: (0, 0),
        preedit: Some("あ"),
        preedit_cursor: None,
        search: None,
        selection: None,
        scroll_offset: 0,
        submit_on_ctrl_enter: false,
    };
    let mut missing = HashSet::new();
    OverlayContent::Prompt(prompt).warm_chars(&mut missing);
    assert!(missing.contains(&'/'));
    assert!(missing.contains(&'h'));
    assert!(missing.contains(&'あ'));
    assert!(missing.contains(&'❯'));
}

#[test]
fn overlay_warmup_palette_collects_query_and_entries() {
    let views: Vec<PaletteEntryView> = vec![PaletteEntryView {
        label: "git",
        description: "vcs",
        kind_label: "Builtin",
    }];
    let p = PaletteDrawParams {
        query: "gi",
        entries: &views,
        selection: 0,
        form: None,
        banner: "",
        submode_input: "",
    };
    let mut missing = HashSet::new();
    OverlayContent::CommandPalette(p).warm_chars(&mut missing);
    assert!(missing.contains(&'g'));
    assert!(missing.contains(&'i'));
}

// ── v1.0 S1: Settings overlay tests ────────────────────────────────

#[test]
fn settings_tab_labels_are_distinct() {
    let labels: Vec<&str> = SettingsTab::ALL.iter().map(|t| t.label()).collect();
    let unique: std::collections::HashSet<&str> = labels.iter().copied().collect();
    assert_eq!(labels.len(), unique.len(), "tab labels must be distinct");
    assert!(labels.contains(&"Appearance"));
    assert!(labels.contains(&"Terminal"));
    assert!(labels.contains(&"Input"));
    assert!(labels.contains(&"Keybindings"));
    assert!(labels.contains(&"Window"));
    assert!(labels.contains(&"Advanced"));
}

#[test]
fn settings_tab_all_has_six_categories() {
    // F5: Logo merged into Appearance; Font merged into Appearance.
    // New categories: Appearance, Terminal, Input, Keybindings, Window, Advanced.
    assert_eq!(SettingsTab::ALL.len(), 6);
    assert_eq!(SettingsTab::ALL[0], SettingsTab::Appearance);
    assert_eq!(SettingsTab::ALL[1], SettingsTab::Terminal);
    assert_eq!(SettingsTab::ALL[2], SettingsTab::Input);
    assert_eq!(SettingsTab::ALL[3], SettingsTab::Keybindings);
    assert_eq!(SettingsTab::ALL[4], SettingsTab::Window);
    assert_eq!(SettingsTab::ALL[5], SettingsTab::Advanced);
}

#[test]
fn settings_z_is_above_palette() {
    // Settings panel must render above the Command Palette so the
    // palette doesn't peek through when both are open (shouldn't happen
    // in practice due to mutual exclusion, but the z-order should be
    // correct regardless).
    assert!(OverlayZ::Settings > OverlayZ::Palette);
    assert!(OverlayZ::Settings > OverlayZ::ContextMenu);
}

#[test]
fn overlay_warmup_settings_collects_label_and_theme_chars() {
    let themes = vec![
        SettingsThemeView {
            name: "weft-warm",
            label: "Weft Warm",
        },
        SettingsThemeView {
            name: "warp",
            label: "Warp Dark",
        },
    ];
    let kbs = vec![SettingsKeybindingView {
        action: "Copy".to_string(),
        binding: "cmd+c".to_string(),
        conflict: false,
    }];
    let s = SettingsDrawParams {
        active_tab: SettingsTab::Appearance,
        selection: 0,
        scroll_offset: 0,
        theme_name: "weft-warm",
        themes: &themes,
        font_family: "Menlo",
        font_size: 14.0,
        line_height: 1.2,
        window_opacity: 1.0,
        window_padding_x: 0,
        window_padding_y: 0,
        scrollback_lines: 10_000,
        window_width: 800,
        window_height: 600,
        sidebar_width: None,
        submit_on_ctrl_enter: false,
        keybindings: &kbs,
        logo_variant: weft_core::config::LogoVariant::Cool,
        error: None,
        is_narrow: false,
        drill_down: false,
        keybinding_conflict_count: 0,
        field_errors: &[],
    };
    let mut missing = HashSet::new();
    OverlayContent::Settings(s).warm_chars(&mut missing);
    // Tab labels.
    assert!(missing.contains(&'S'));
    assert!(missing.contains(&'A'));
    // Theme labels.
    assert!(missing.contains(&'W'));
    // Font family.
    assert!(missing.contains(&'M'));
    // Keybinding strings.
    assert!(missing.contains(&'c'));
}

#[test]
fn settings_overlay_is_modal_and_highest_z() {
    // A stack with both a palette and a settings layer should report
    // Settings as the topmost modal.
    let stack = OverlayStack {
        layers: vec![
            make_layer(OverlayZ::Palette, OverlayInputPolicy::Modal),
            OverlayLayer {
                kind: OverlayKind::Settings,
                z: OverlayZ::Settings,
                input_policy: OverlayInputPolicy::Modal,
                content: OverlayContent::Settings(SettingsDrawParams {
                    active_tab: SettingsTab::Appearance,
                    selection: 0,
                    scroll_offset: 0,
                    theme_name: "",
                    themes: &[],
                    font_family: "",
                    font_size: 14.0,
                    line_height: 1.2,
                    window_opacity: 1.0,
                    window_padding_x: 0,
                    window_padding_y: 0,
                    scrollback_lines: 10_000,
                    window_width: 800,
                    window_height: 600,
                    sidebar_width: None,
                    submit_on_ctrl_enter: false,
                    keybindings: &[],
                    logo_variant: weft_core::config::LogoVariant::Cool,
                    error: None,
                    is_narrow: false,
                    drill_down: false,
                    keybinding_conflict_count: 0,
                    field_errors: &[],
                }),
            },
        ],
    };
    let top = stack.topmost_modal().expect("should have a modal");
    assert_eq!(top.z, OverlayZ::Settings);
    assert_eq!(top.kind, OverlayKind::Settings);
}
