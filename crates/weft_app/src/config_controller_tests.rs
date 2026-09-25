// config_controller tests (split from config_controller.rs to keep it within
// its architecture-gate budget; child-module privacy reaches pub(super) items
// exactly like the inline module did).

use super::*;
use crate::pane::Pane;
use crate::tab::Tab;
use weft_core::config::{Config, ConfigLoadError, LoadedConfig};
use weft_core::grid::Color;
use weft_core::pane_layout::SplitDirection;

/// Build a tab with a live terminal but no PTY — enough for palette /
/// scrollback tests without spawning a shell.
fn tab_with_terminal(scrollback_lines: usize) -> Tab {
    Tab::with_single_pane(Pane::with_terminal_only(scrollback_lines))
}

/// v1.11.4 (PLAN_v1114 §3): the compat flip walk reaches EVERY pane —
/// background panes must not keep the old kitty_keyboard switch.
#[test]
fn kitty_protocol_walk_touches_every_pane() {
    let mut tabs = vec![tab_with_terminal(100), tab_with_terminal(100)];
    for tab in tabs.iter_mut() {
        tab.terminal.as_mut().unwrap().process(b"\x1b[>27u");
    }
    apply_kitty_protocol_to_all_panes(&mut tabs, false);
    for tab in tabs.iter_mut() {
        let t = tab.terminal.as_mut().unwrap();
        assert_eq!(t.keyboard_protocol_flags(), 0, "disabled ⇒ flags 0");
        t.process(b"\x1b[?u");
        assert_eq!(t.take_response(), b"", "disabled ⇒ ops swallowed");
    }
    apply_kitty_protocol_to_all_panes(&mut tabs, true);
    let t = tabs[0].terminal.as_mut().unwrap();
    t.process(b"\x1b[?u");
    assert_eq!(
        t.take_response(),
        b"\x1b[?19u",
        "re-enabled ⇒ previous negotiation is still live"
    );
}

// ── PLAN_v11217 §3.5 (T4): [blocks] output_cap_mib walks ────────────

/// The all-tab walk (apply_config path) must reach EVERY pane in EVERY
/// tab — live-reload and profile switches affect already-open tabs
/// (review P2a', 全 tab 生效), and the MiB value is converted to bytes.
#[test]
fn output_cap_walk_touches_every_pane_of_every_tab() {
    let mut tabs = vec![tab_with_terminal(100), tab_with_terminal(100)];
    apply_blocks_output_cap_to_all_panes(&mut tabs, 4);
    for tab in &tabs {
        let cap = tab.terminal.as_ref().unwrap().block_tracker().output_cap();
        assert_eq!(cap, 4 * 1024 * 1024, "configured 4 MiB reaches the tracker");
    }
}

/// Defensive clamp at the APPLY layer (P2a double clamp): the entry point
/// is reachable from live-reload and profile switches, so 0 → 1 and
/// 65 → 64 MiB here, independent of the config-load normalization.
#[test]
fn output_cap_walk_clamps_out_of_range_mib() {
    let mut tabs = vec![tab_with_terminal(100)];
    apply_blocks_output_cap_to_all_panes(&mut tabs, 0);
    assert_eq!(
        tabs[0]
            .terminal
            .as_ref()
            .unwrap()
            .block_tracker()
            .output_cap(),
        weft_core::blocks::OUTPUT_CAP_MIN_MIB * 1024 * 1024,
        "0 MiB → 1 MiB floor"
    );
    let mut tabs = vec![tab_with_terminal(100)];
    apply_blocks_output_cap_to_all_panes(&mut tabs, 65);
    assert_eq!(
        tabs[0]
            .terminal
            .as_ref()
            .unwrap()
            .block_tracker()
            .output_cap(),
        weft_core::blocks::OUTPUT_CAP_MAX_MIB * 1024 * 1024,
        "65 MiB → 64 MiB ceiling"
    );
}

/// Per-tab twin (creation-site chokepoints): the clamp + byte conversion
/// must behave identically so a freshly opened tab matches applied tabs.
#[test]
fn per_tab_output_cap_apply_matches_the_all_tab_walk() {
    let mut tab = tab_with_terminal(100);
    apply_blocks_output_cap(&mut tab, 8);
    assert_eq!(
        tab.terminal.as_ref().unwrap().block_tracker().output_cap(),
        8 * 1024 * 1024
    );
    let mut tabs = vec![tab_with_terminal(100)];
    apply_blocks_output_cap_to_all_panes(&mut tabs, 8);
    assert_eq!(
        tabs[0]
            .terminal
            .as_ref()
            .unwrap()
            .block_tracker()
            .output_cap(),
        tab.terminal.as_ref().unwrap().block_tracker().output_cap()
    );
}

/// v1.12.19 (PLAN_v11217 §3.8 T13b): the retained-limit walk must reach
/// EVERY pane in EVERY tab — the Settings Blocks row takes effect on
/// already-open tabs without a restart (same contract as the output-cap
/// walk, and same tab/pane construction precedent).
#[test]
fn retained_limit_walk_touches_every_pane_of_every_tab() {
    let mut tabs = vec![tab_with_terminal(100), tab_with_terminal(100)];
    apply_blocks_retained_limit_to_all_panes(&mut tabs, 750);
    for tab in &tabs {
        assert_eq!(
            tab.terminal
                .as_ref()
                .unwrap()
                .block_tracker()
                .retained_limit(),
            750,
            "configured retained limit reaches the tracker"
        );
    }
    // 0 must pass through: it means "retention disabled", not a floor.
    let mut tabs = vec![tab_with_terminal(100)];
    apply_blocks_retained_limit_to_all_panes(&mut tabs, 0);
    assert_eq!(
        tabs[0]
            .terminal
            .as_ref()
            .unwrap()
            .block_tracker()
            .retained_limit(),
        0,
        "0 disables retention (v1.11.2 semantics preserved)"
    );
}

// ── decide_reload ────────────────────────────────────────────────

#[test]
fn reload_parse_error_keeps_last_known_good_config() {
    // The critical v1.5.0 invariant: a malformed config file must NOT
    // blow away the running config. `decide_reload` returns
    // `KeepLastKnownGood` for any `Err`, and the caller leaves
    // `config_state` untouched.
    //
    // Construct a real parse error by writing bad TOML to a temp
    // file and loading it via `load_resolved_from_path`.
    let tmp = std::env::temp_dir().join(format!(
        "weft-reload-parse-{}-{}",
        std::process::id(),
        std::time::SystemTime::UNIX_EPOCH
            .elapsed()
            .unwrap_or_default()
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&tmp);
    std::fs::write(&tmp, "not = valid = toml").unwrap();
    let err = weft_core::config::load_resolved_from_path(&tmp).unwrap_err();
    let _ = std::fs::remove_file(&tmp);
    assert!(matches!(err, ConfigLoadError::Parse(_)));
    let decision = decide_reload(12345, true, Err(err));
    assert!(matches!(decision, ReloadDecision::KeepLastKnownGood));
}

#[test]
fn reload_io_error_keeps_last_known_good_config() {
    // File missing → Io error → same keep-last-known-good behavior.
    let err = ConfigLoadError::Io(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "file gone",
    ));
    let decision = decide_reload(12345, true, Err(err));
    assert!(matches!(decision, ReloadDecision::KeepLastKnownGood));
}

#[test]
fn reload_profile_error_keeps_last_known_good_config() {
    // Invalid profile schema → Profile error → keep last-known-good.
    let err = ConfigLoadError::Profile(weft_core::config::ProfileError::TooManyProfiles(33));
    let decision = decide_reload(12345, true, Err(err));
    assert!(matches!(decision, ReloadDecision::KeepLastKnownGood));
}

#[test]
fn same_fingerprint_skips_duplicate_apply() {
    // v1.5.3 fingerprint dedup: the watcher fires on mtime change,
    // but the content may be identical (e.g. touch(1) or a save that
    // wrote the same bytes). The reload must be a no-op so we don't
    // rebuild the atlas or reseed palettes for nothing.
    let mut source = Config::default();
    source.font.family = "Cached".into();
    let (effective, _) = source.resolve_active_profile().unwrap();
    let fingerprint = 9999;
    let loaded = LoadedConfig {
        source,
        effective,
        fingerprint,
        diagnostics: Vec::new(),
    };
    // Same fingerprint → skip.
    let decision = decide_reload(fingerprint, true, Ok(loaded.clone()));
    assert!(matches!(decision, ReloadDecision::SkipSameFingerprint));

    // Different fingerprint → apply.
    let decision = decide_reload(fingerprint.wrapping_add(1), true, Ok(loaded));
    match decision {
        ReloadDecision::Apply(loaded) => assert_eq!(loaded.fingerprint, fingerprint),
        other => panic!("expected Apply, got {other:?}"),
    }
}

#[test]
fn first_load_with_zero_fingerprint_applies() {
    // Edge case: the very first load has `current_fingerprint = 0`
    // and `has_current = false`. Even if `loaded.fingerprint == 0`
    // (astronomically unlikely for real file bytes, but the logic
    // must still apply on the first load).
    let mut source = Config::default();
    source.font.family = "First".into();
    let (effective, _) = source.resolve_active_profile().unwrap();
    let loaded = LoadedConfig {
        source,
        effective,
        fingerprint: 0,
        diagnostics: Vec::new(),
    };
    let decision = decide_reload(0, false, Ok(loaded));
    // First load (has_current=false) must apply even when fingerprints
    // would otherwise compare equal — `has_current` gate prevents the
    // "same fingerprint" branch from firing before the first
    // successful load.
    match decision {
        ReloadDecision::Apply(_) => {}
        other => panic!("expected Apply on first load, got {other:?}"),
    }
}

#[test]
fn apply_delta_uses_previous_config_for_runtime_updates() {
    let current = Config::default();
    let mut next = current.clone();
    next.font.family = "Monaco".into();
    next.window.opacity = 0.75;
    next.window.padding_x = 3;
    next.window.width += 120;

    assert_eq!(
        config_apply_delta(&current, &next, 1.0),
        ConfigApplyDelta {
            rebuild_font: true,
            update_opacity: true,
            update_padding: true,
            resize_window: true,
            update_bold_is_bright: false,
            update_kitty_keyboard: false,
            update_osc52_mode: false,
            update_notifications: false,
        }
    );
    assert_eq!(
        config_apply_delta(&next, &next, 1.0),
        ConfigApplyDelta {
            rebuild_font: false,
            update_opacity: false,
            update_padding: false,
            resize_window: false,
            update_bold_is_bright: false,
            update_kitty_keyboard: false,
            update_osc52_mode: false,
            update_notifications: false,
        },
        "committing loaded state before apply would hide every runtime delta"
    );

    let mut cjk = current.clone();
    cjk.font.cjk_family = "Hiragino Sans GB".into();
    assert!(config_apply_delta(&current, &cjk, 1.0).rebuild_font);

    let mut emoji = current.clone();
    emoji.font.emoji_family = "Noto Color Emoji".into();
    assert!(config_apply_delta(&current, &emoji, 1.0).rebuild_font);
}

/// v1.11.3 (PLAN_v1113 §3.3): the [compat] flip arms
/// `update_bold_is_bright` — the renderer setter then invalidates BOTH
/// caches (force_full_grid + styled bump, R8). A missed delta here
/// would leave stale palette colors after a config/profile change.
#[test]
fn apply_delta_arms_bold_is_bright_on_compat_flip() {
    let current = Config::default();
    assert!(!current.compat.bold_is_bright);
    let mut next = current.clone();
    next.compat.bold_is_bright = true;
    assert!(
        config_apply_delta(&current, &next, 1.0).update_bold_is_bright,
        "false → true must arm the renderer setter"
    );
    let third = current.clone();
    assert!(
        config_apply_delta(&next, &third, 1.0).update_bold_is_bright,
        "true → false must re-arm it"
    );
    assert!(
        !config_apply_delta(&next, &next, 1.0).update_bold_is_bright,
        "no flip → no invalidation"
    );
}

/// v1.11.4 (PLAN_v1114 §3): the [compat] kitty_keyboard flip arms
/// `update_kitty_keyboard` — apply_config then walks every pane's
/// terminal (set_kitty_protocol_enabled). A missed delta here would
/// leave background panes on the old switch until focused.
#[test]
fn apply_delta_arms_kitty_keyboard_on_compat_flip() {
    let current = Config::default();
    assert!(current.compat.kitty_keyboard, "default is on");
    let mut next = current.clone();
    next.compat.kitty_keyboard = false;
    assert!(
        config_apply_delta(&current, &next, 1.0).update_kitty_keyboard,
        "true → false must arm the pane walk"
    );
    let third = current.clone();
    assert!(
        config_apply_delta(&next, &third, 1.0).update_kitty_keyboard,
        "false → true must re-arm it"
    );
    assert!(
        !config_apply_delta(&next, &next, 1.0).update_kitty_keyboard,
        "no flip → no walk"
    );
}

// ── apply_palette_to_all_panes ──────────────────────────────────

#[test]
fn profile_switch_updates_every_tab_and_pane_palette() {
    // Two tabs, each with one pane. All terminals start with the
    // default palette. After apply_palette_to_all_panes, every
    // pane's palette must match the new one — not just the active
    // tab's pane.
    let tab0 = tab_with_terminal(100);
    let tab1 = tab_with_terminal(100);
    let original_palette = *tab0.terminal.as_ref().unwrap().palette();

    // Build a distinctly different palette.
    let mut new_palette = original_palette;
    new_palette[0] = Color {
        r: 0xAA,
        g: 0xBB,
        b: 0xCC,
        a: 0xFF,
    };

    let tabs: &mut [Tab] = &mut [tab0, tab1];
    apply_palette_to_all_panes(tabs, new_palette, weft_core::grid::Color::DEFAULT_BG);

    // Both tabs' panes must have the new palette.
    for (i, tab) in tabs.iter().enumerate() {
        let pal = tab.terminal.as_ref().unwrap().palette();
        assert_eq!(pal[0], new_palette[0], "tab {i} palette not updated");
        assert_ne!(pal[0], original_palette[0], "tab {i} palette unchanged");
    }
}

#[test]
fn apply_palette_skips_panes_without_terminal() {
    // A pane with `terminal: None` (PTY spawn failure path) must not
    // panic — apply_palette_to_all_panes silently skips it.
    let tab = Tab::empty();
    // Tab::empty has no panes at all; add a terminal-less pane isn't
    // trivial via the public API, but `Tab::empty()` itself exercises
    // the "no panes" path. Ensure no panic.
    let palette = [Color {
        r: 0,
        g: 0,
        b: 0,
        a: 0,
    }; 256];
    apply_palette_to_all_panes(&mut [tab], palette, weft_core::grid::Color::DEFAULT_BG);
}

// ── apply_scrollback_to_all_panes ────────────────────────────────

#[test]
fn profile_switch_updates_every_pane_scrollback_limit() {
    // Two tabs with different initial scrollback capacities. After
    // apply_scrollback_to_all_panes, both must reflect the new limit.
    let tab0 = tab_with_terminal(1000);
    let tab1 = tab_with_terminal(500);
    let tabs: &mut [Tab] = &mut [tab0, tab1];

    let new_limit = 5000;
    apply_scrollback_to_all_panes(tabs, new_limit);

    // Both tabs' panes must have the new scrollback capacity.
    for (i, tab) in tabs.iter().enumerate() {
        let t = tab.terminal.as_ref().unwrap();
        let grid = t.grid();
        assert_eq!(
            grid.scrollback.max_lines(),
            new_limit,
            "tab {i} scrollback not updated"
        );
    }
}

#[test]
fn profile_switch_updates_two_tabs_with_four_panes_each() {
    let mut tabs = [tab_with_terminal(100), tab_with_terminal(200)];
    for tab in &mut tabs {
        for _ in 0..3 {
            tab.split_active_pane_test(SplitDirection::Vertical, 0.5, 300)
                .unwrap();
        }
        assert_eq!(tab.pane_count(), 4);
    }

    let mut palette = *tabs[0].terminal.as_ref().unwrap().palette();
    palette[7] = Color {
        r: 0x12,
        g: 0x34,
        b: 0x56,
        a: 0xFF,
    };
    apply_palette_to_all_panes(&mut tabs, palette, weft_core::grid::Color::DEFAULT_BG);
    apply_scrollback_to_all_panes(&mut tabs, 4321);

    for (tab_index, tab) in tabs.iter_mut().enumerate() {
        let panes: Vec<_> = tab.panes_mut().collect();
        assert_eq!(panes.len(), 4);
        for (pane_index, pane) in panes.into_iter().enumerate() {
            let terminal = pane.terminal.as_ref().unwrap();
            assert_eq!(
                terminal.palette()[7],
                palette[7],
                "tab {tab_index} pane {pane_index} palette was stale"
            );
            assert_eq!(
                terminal.grid().scrollback.max_lines(),
                4321,
                "tab {tab_index} pane {pane_index} scrollback was stale"
            );
        }
    }
}

#[test]
fn apply_scrollback_skips_panes_without_terminal() {
    // Same safety check as palette: terminal-less panes must be skipped
    // silently, not panic.
    let tab = Tab::empty();
    apply_scrollback_to_all_panes(&mut [tab], 9999);
}

// ── Multi-pane coverage ─────────────────────────────────────────
//
// The `apply_palette_updates_all_panes_in_split_tab` test would verify
// that a single tab with multiple split panes (v1.3) gets every pane
// updated. However, creating a split requires an `EventLoopProxy`,
// which on macOS must be created on the main thread — tests may run
// on any thread. The `panes_mut()` iterator that drives the update
// is already exercised by `profile_switch_updates_every_tab_and_pane_palette`
// (two tabs, each with one pane), which is the same code path. A
// dedicated multi-pane-within-one-tab test would need either a test
// constructor that builds a multi-pane `Tab` without a proxy, or a
// main-thread test harness — both are out of scope for v1.5.0.

/// v1.11.5 (PLAN_v1115 §M8): the [clipboard].osc52 mode diff arms
/// `update_osc52_mode` (3-state: default→off→unrestricted both ways).
/// Consumption is live from config_state, but the delta flag pins the
/// keys the settings save/reload tests must exercise.
#[test]
fn apply_delta_arms_osc52_mode_on_mode_change() {
    use weft_core::config::Osc52Mode;
    let current = Config::default();
    assert_eq!(current.clipboard.osc52, Osc52Mode::Default);
    let mut next = current.clone();
    next.clipboard.osc52 = Osc52Mode::Off;
    assert!(config_apply_delta(&current, &next, 1.0).update_osc52_mode);
    next.clipboard.osc52 = Osc52Mode::Unrestricted;
    assert!(config_apply_delta(&current, &next, 1.0).update_osc52_mode);
    assert!(
        !config_apply_delta(&next, &next, 1.0).update_osc52_mode,
        "same mode → no change"
    );
}

/// v1.11.5 (PLAN_v1115 §M8): any [notifications] key change arms
/// `update_notifications`.
#[test]
fn apply_delta_arms_notifications_on_any_key_change() {
    use weft_core::config::NotificationsConfig;
    let current = Config::default();
    let mut next = current.clone();
    next.notifications = NotificationsConfig {
        enabled: false,
        ..current.notifications
    };
    assert!(
        config_apply_delta(&current, &next, 1.0).update_notifications,
        "enabled flip arms"
    );
    let mut next2 = current.clone();
    next2.notifications.threshold_secs = 120;
    assert!(config_apply_delta(&current, &next2, 1.0).update_notifications);
    let mut next3 = current.clone();
    next3.notifications.sound = true;
    assert!(config_apply_delta(&current, &next3, 1.0).update_notifications);
    assert!(
        !config_apply_delta(&current, &current, 1.0).update_notifications,
        "identical → no change"
    );
}
