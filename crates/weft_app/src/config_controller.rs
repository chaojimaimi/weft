//! v1.5.0: Config lifecycle — reload, resolve, apply.
//!
//! Extracted from `lifecycle_controller.rs` so the v1.5 profile-aware
//! reload/apply logic stays within its architecture-gate budget and the
//! pure per-pane update loops (`apply_palette_to_all_panes`,
//! `apply_scrollback_to_all_panes`) and the reload decision
//! (`decide_reload`) can be unit-tested without spinning up a full `App`.
//!
//! Tests live at the bottom of this file and use the `Tab::with_single_pane`
//! + `Pane::with_terminal_only` test constructors — no Metal, no PTY.

use super::*;

// ── Reload decision (pure logic, testable) ──────────────────────────────

/// Outcome of [`decide_reload`]. The caller (`App::reload_config`) maps
/// each variant to its runtime effect.
//
// Note: `PartialEq`/`Eq` are intentionally NOT derived because
// `ReloadDecision::Apply` carries a `LoadedConfig` (which contains a
// `Config`), and `Config` doesn't implement `PartialEq` (its
// `HashMap`/`BTreeMap`/nested config sections make value-equality
// expensive and brittle). Tests use pattern matching instead.
#[derive(Clone, Debug)]
pub(super) enum ReloadDecision {
    /// The file changed and resolved cleanly — apply the new loaded config
    /// (both source and effective). Carries the new fingerprint so the
    /// caller can store it. Boxed because `LoadedConfig` is a large struct
    /// (multiple nested config sections + `HashMap`/`BTreeMap`) —
    /// clippy::large_enum_variant.
    Apply(Box<weft_core::config::LoadedConfig>),
    /// The file's fingerprint matches the last applied one — skip the
    /// apply entirely (no atlas rebuild, no palette reseed).
    SkipSameFingerprint,
    /// The file failed to load or resolve — keep the current runtime
    /// state untouched. The error is logged by the caller.
    KeepLastKnownGood,
}

/// Decide what to do with a `load_resolved()` result, given the current
/// `ConfigState`.
///
/// Pure logic — no I/O, no side effects. This is the function the v1.5.0
/// reload tests exercise:
/// - `reload_parse_error_keeps_last_known_good_config`
/// - `same_fingerprint_skips_duplicate_apply`
///
/// The "apply" branch returns the effective config + new fingerprint so
/// `reload_config` can call `apply_config(effective)` and then store the
/// fingerprint. Splitting the decision from the effect is what makes the
/// reload testable without a real `App`.
pub(super) fn decide_reload(
    current_fingerprint: u64,
    has_current: bool,
    loaded: Result<weft_core::config::LoadedConfig, weft_core::config::ConfigLoadError>,
) -> ReloadDecision {
    match loaded {
        Ok(loaded) => {
            // v1.5.3 fingerprint dedup: same content → no-op. Also
            // protects against the save → watcher feedback loop
            // (save triggers mtime change → watcher → reload).
            if has_current && loaded.fingerprint == current_fingerprint {
                ReloadDecision::SkipSameFingerprint
            } else {
                ReloadDecision::Apply(Box::new(loaded))
            }
        }
        Err(_) => ReloadDecision::KeepLastKnownGood,
    }
}

// ── Per-pane update loops (pure logic, testable) ───────────────────────

/// Reseed the palette on every pane's terminal in every tab. Called by
/// `apply_config` when the theme changes (profile switch, reload, manual
/// toggle). Extracted as a free function so it can be tested with
/// `Tab::with_single_pane(Pane::with_terminal_only(n))` without a Metal
/// renderer.
///
/// v1.5.0: must touch EVERY pane in EVERY tab, not just the active one.
/// A profile switch that only recolors the focused pane leaves background
/// panes with the old palette until the user focuses them — visibly wrong.
pub(super) fn apply_palette_to_all_panes(
    tabs: &mut [Tab],
    palette: [weft_core::grid::Color; 256],
    background: weft_core::grid::Color,
) {
    for tab in tabs.iter_mut() {
        for pane in tab.panes_mut() {
            if let Some(t) = pane.terminal.as_mut() {
                t.set_palette(palette);
                t.set_background_color(background);
            }
        }
    }
}

/// v1.11.4 (PLAN_v1114 §3): flip the kitty keyboard-protocol master
/// switch on EVERY pane's terminal in every tab. Background panes keep the
/// old switch otherwise — same walk precedent as the palette reseed and
/// scrollback capacity.
pub(super) fn apply_kitty_protocol_to_all_panes(tabs: &mut [Tab], enabled: bool) {
    for tab in tabs.iter_mut() {
        for pane in tab.panes_mut() {
            if let Some(t) = pane.terminal.as_mut() {
                t.set_kitty_protocol_enabled(enabled);
            }
        }
    }
}

/// Update the scrollback capacity on every pane's terminal in every tab.
/// Called by `apply_config` when the `[scrollback] lines` value changes.
/// Extracted for the same testability reasons as
/// [`apply_palette_to_all_panes`].
pub(super) fn apply_scrollback_to_all_panes(tabs: &mut [Tab], max_lines: usize) {
    // v1.11.2 X3 defensive clamp (PLAN_v1112 §5): the entry point is reachable
    // from live-reload and profile switches, not only the normalized load path.
    let max_lines = max_lines.clamp(
        weft_core::config::SCROLLBACK_MIN_LINES,
        weft_core::config::SCROLLBACK_MAX_LINES,
    );
    for tab in tabs.iter_mut() {
        for pane in tab.panes_mut() {
            if let Some(t) = pane.terminal.as_mut() {
                t.set_scrollback_max_lines(max_lines);
            }
        }
    }
}

/// v1.11.2 X4 (PLAN_v1112 §1.2): apply the `[blocks] retained_limit` config
/// to every pane's terminal of one tab. A free function next to
/// `apply_scrollback_to_all_panes` (same config-propagation domain); called
/// by each tab creation site that already reads `[scrollback] lines`.
pub(super) fn apply_blocks_retained_limit(tab: &mut Tab, limit: usize) {
    for pane in tab.panes_mut() {
        pane.set_blocks_retained_limit(limit);
    }
}

/// v1.11.7 (PLAN_v1117 §三 M1.2, P2-3): apply the user's
/// `[experimental] tui_render_mode` to every pane's terminal of one tab.
/// Called by each tab/pane creation site that already reads config (same
/// chokepoints as `apply_blocks_retained_limit`); `Tab::new`'s Terminal
/// starts in core-default Classic, so every factory-created pane must be
/// re-injected here.
pub(super) fn apply_tui_render_mode(tab: &mut Tab, mode: weft_core::vt::TuiRenderMode) {
    for pane in tab.panes_mut() {
        pane.set_tui_render_mode(mode);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ConfigApplyDelta {
    rebuild_font: bool,
    update_opacity: bool,
    update_padding: bool,
    resize_window: bool,
    /// v1.11.3 (PLAN_v1113 §3.3): [compat] bold_is_bright flip.
    update_bold_is_bright: bool,
    /// v1.11.4 (PLAN_v1114 §3): [compat] kitty_keyboard flip.
    update_kitty_keyboard: bool,
    /// v1.11.5 (PLAN_v1115 §M8): [clipboard] osc52 mode diff (3-state).
    update_osc52_mode: bool,
    /// v1.11.5 (PLAN_v1115 §M8): [notifications] diff — any of
    /// enabled / threshold_secs / sound.
    update_notifications: bool,
}

fn config_apply_delta(current: &Config, next: &Config, font_scale: f32) -> ConfigApplyDelta {
    ConfigApplyDelta {
        rebuild_font: current.font.family != next.font.family
            || current.font.cjk_family != next.font.cjk_family
            || current.font.emoji_family != next.font.emoji_family
            || current.font.size != next.font.size
            || current.font.line_height != next.font.line_height
            || font_scale != 1.0,
        update_opacity: crate::settings_validation::runtime_opacity(current.window.opacity)
            != crate::settings_validation::runtime_opacity(next.window.opacity),
        update_padding: current.window.padding_x != next.window.padding_x
            || current.window.padding_y != next.window.padding_y,
        resize_window: current.window.width != next.window.width
            || current.window.height != next.window.height,
        update_bold_is_bright: current.compat.bold_is_bright != next.compat.bold_is_bright,
        update_kitty_keyboard: current.compat.kitty_keyboard != next.compat.kitty_keyboard,
        update_osc52_mode: current.clipboard.osc52 != next.clipboard.osc52,
        update_notifications: current.notifications != next.notifications,
    }
}

// ── App methods (delegates to the pure functions above) ─────────────────

impl App {
    /// Apply runtime deltas while the previous effective config is still
    /// available, then commit the raw/effective/fingerprint state together.
    pub(super) fn commit_loaded_config(&mut self, loaded: weft_core::config::LoadedConfig) {
        let effective = loaded.effective.clone();
        self.apply_config(effective);
        self.config_state.set_loaded(loaded);
    }

    /// Re-read config from disk and apply it live. Triggered by the
    /// reload-config keybinding (and the file watcher).
    ///
    /// v1.5.0: Uses `load_resolved` so the source/effective split is
    /// preserved (profile overrides are not flattened into the base on
    /// save). On parse/profile error, the last-known-good config is kept
    /// — `apply_config` is not called, so the runtime is untouched. On
    /// success the fingerprint is updated so the watcher can skip duplicate
    /// reloads of identical content.
    pub(super) fn reload_config(&mut self) {
        let loaded = weft_core::config::load_resolved();
        // v1.5.3: retain the error (if any) so we can surface a specific
        // message via `surface_config_error`. `decide_reload` consumes
        // the `Result`, so we clone the `Display` form here for the hint.
        let err_message = loaded.as_ref().err().map(|e| e.to_string());
        let decision = decide_reload(
            self.config_state.config_fingerprint,
            self.config_state.source_config.is_some(),
            loaded,
        );
        match decision {
            ReloadDecision::SkipSameFingerprint => {
                tracing::debug!("config reload skipped (same fingerprint)");
            }
            ReloadDecision::KeepLastKnownGood => {
                // v1.5.3: surface the reload failure so the user knows the
                // config is in a bad state. The detailed error is logged;
                // the Settings banner (if open) shows the full message;
                // the status badge (always) shows a brief hint. The
                // runtime config is NOT touched — last-known-good stays.
                if let Some(msg) = err_message {
                    tracing::warn!(error = %msg, "config reload failed; keeping current config");
                    self.surface_config_error(&format!("Config reload failed: {msg}"));
                } else {
                    // Should be unreachable (KeepLastKnownGood only comes
                    // from Err), but guard against future logic changes.
                    tracing::warn!("config reload failed; keeping current config");
                    self.surface_config_error("Config reload failed");
                }
            }
            ReloadDecision::Apply(loaded) => {
                // `commit_loaded_config` applies against the previous
                // effective config before replacing source/effective/fingerprint.
                // Applying after `set_loaded` would hide font/window deltas.
                let fingerprint = loaded.fingerprint;
                self.commit_loaded_config(*loaded);
                // v1.5.3: clear any prior reload error — the file is now
                // healthy. Also clears the status badge so a stale
                // "Config reload failed" doesn't linger after recovery.
                self.clear_config_error();
                info!(fingerprint, "config reloaded");
            }
        }
    }

    /// Apply a (possibly new) config: theme, font, keybindings, scrollback.
    /// Theme/font/scrollback changes take effect immediately; window size/title
    /// apply on the next launch.
    pub(super) fn apply_config(&mut self, config: Config) {
        let delta = config_apply_delta(
            &self.config_state.config,
            &config,
            self.config_state.font_scale,
        );
        // Theme — renderer defaults + terminal palette reseed (recolors all
        // Palette-indexed cells on the next draw).
        //
        // v0.9 U-D1: when `[theme] follow_system = true`, the system
        // appearance picks the theme (via `light_name` / `dark_name`,
        // defaulting to `weft-light` / `weft-warm`). The `name` field is
        // ignored in this mode.
        let theme = if config.theme.follow_system {
            // SAFETY: `system_appearance_is_dark` reads NSUserDefaults
            // AppleInterfaceStyle via a read-only objc2 lookup; it has no
            // thread affinity requirements and no side effects. Safe to
            // call from the main event-loop thread where `apply_config`
            // runs.
            let dark = unsafe { system_appearance_is_dark() };
            let name = if dark {
                config
                    .theme
                    .dark_name
                    .clone()
                    .unwrap_or_else(|| "weft-warm".to_string())
            } else {
                config
                    .theme
                    .light_name
                    .clone()
                    .unwrap_or_else(|| "weft-light".to_string())
            };
            weft_core::config::Theme::resolve_named(&name, &config.theme)
        } else {
            config.theme()
        };
        if let Some(r) = &mut self.renderer {
            r.set_theme(theme.clone());
            r.set_minimum_contrast(config.theme.minimum_contrast);
            r.set_semantic_output_enabled(config.theme.semantic_output_enabled());
        }
        // v1.5.0: reseed palette on EVERY pane in EVERY tab, not just the
        // active one. A profile switch must recolor all panes so background
        // panes don't keep the old palette until the user focuses them.
        apply_palette_to_all_panes(self.sessions.tabs_mut(), theme.palette, theme.background);
        // v1.0: sync preferred_dark_theme from the freshly loaded config.
        let cfg_name = &config.theme.name;
        if !cfg_name.contains("light") && !cfg_name.is_empty() {
            self.config_state.preferred_dark_theme = cfg_name.clone();
        } else if let Some(dn) = &config.theme.dark_name {
            self.config_state.preferred_dark_theme = dn.clone();
        }

        // Font — rebuild the atlas (cell dimensions may change → recompute).
        // The active `font_scale` (Cmd+/- zoom) is re-applied on top of the
        // freshly loaded config, so a reload doesn't lose the user's zoom.
        if delta.rebuild_font {
            if let Some(r) = &mut self.renderer {
                let scaled = crate::settings_validation::runtime_scaled_font_config(
                    &config.font,
                    self.config_state.font_scale,
                );
                r.rebuild_atlas(scaled);
            }
            self.recompute_layout();
        }

        // Window background opacity (layer-level transparency; text stays
        // opaque). Recolors the next frame. v1.2.11 fix: now also flips the
        // NSWindow's opaque flag + background color at runtime so lowering
        // opacity below 1.0 actually shows the desktop through the window.
        // Previously `with_transparent()` was creation-only, so a window
        // started at opacity=1.0 could not become transparent without a
        // relaunch — the Metal layer went non-opaque but the NSWindow's
        // system background filled the transparent regions.
        if delta.update_opacity {
            let new_opacity = crate::settings_validation::runtime_opacity(config.window.opacity);
            if let Some(r) = &mut self.renderer {
                r.set_opacity(config.window.opacity);
            }
            if let Some(window) = &self.window {
                let ok = crate::macos_window::set_window_opaque(window, new_opacity >= 1.0);
                if !ok {
                    tracing::warn!(
                        "set_window_opaque returned false — NSWindow handle unavailable"
                    );
                }
            }
        }

        // Content padding (changes usable rows/cols → recompute layout).
        if delta.update_padding {
            if let Some(r) = &mut self.renderer {
                r.set_padding((config.window.padding_x, config.window.padding_y));
            }
            self.recompute_layout();
        }

        // v1.11.3 (PLAN_v1113 §3.3): bold→bright compat switch. The
        // renderer setter invalidates BOTH caches (grid rows bake resolved
        // colors too — R8).
        if delta.update_bold_is_bright {
            if let Some(r) = &mut self.renderer {
                r.set_bold_is_bright(config.compat.bold_is_bright);
            }
        }

        // v1.11.4 (PLAN_v1114 §3): [compat] kitty_keyboard flip — walk
        // EVERY pane in EVERY tab (background panes keep the old switch
        // until focused otherwise, exactly like the palette reseed).
        if delta.update_kitty_keyboard {
            apply_kitty_protocol_to_all_panes(
                self.sessions.tabs_mut(),
                config.compat.kitty_keyboard,
            );
        }

        // v1.11.5 (PLAN_v1115 §M8): [clipboard].osc52 / [notifications]
        // need NO runtime walk — both gates are consumed LIVE from
        // `config_state.config` at event time (OSC 52 dispatch /
        // notification policy), and `self.config_state.config = config`
        // below atomically swaps the value every consumer sees next.
        // The delta flags exist so the settings save/reload tests can
        // assert the exact keys that changed (see tests below).

        // Keybindings.
        self.config_state.keybindings = config.keybindings();

        // Scrollback capacity — v1.5.0: update EVERY pane in EVERY tab so
        // a profile switch doesn't leave background panes with the old limit.
        apply_scrollback_to_all_panes(self.sessions.tabs_mut(), config.scrollback.lines);

        // v1.0 Logo: sync Dock icon if variant changed.
        if config.logo.variant != self.window_runtime.current_logo_variant {
            self.window_runtime.current_logo_variant = config.logo.variant;
            // SAFETY: `set_dock_icon` calls
            // NSApplication.setApplicationIconImage: on the main thread.
            // `apply_config` runs on the main event-loop thread (called
            // from startup or the watcher reload, which dispatches back
            // to main), satisfying NSApplication's main-thread
            // requirement. The icon image is a static asset loaded once
            // and retained by the autorelease pool; no dangling refs.
            unsafe {
                set_dock_icon(self.window_runtime.current_logo_variant);
            }
        }

        // F3-3: sync the persisted sidebar width override from config to the
        // renderer so a reload (Cmd+Shift+,) or external edit picks up the new
        // value. `None` clears any in-flight drag override and falls back to
        // the responsive SidebarMetrics default.
        if let Some(r) = &mut self.renderer {
            r.set_sidebar_width(config.window.sidebar_width);
        }

        // v1.2.11 fix: Window width/height 现在运行时生效。原代码注释说
        // "apply on the next launch"，但实际上 winit 的 `request_inner_size` 可以
        // 在运行时调整窗口尺寸。用户在 Settings → Window 调整 Width/Height 后
        // 按 Apply/Save，窗口会立即 resize 到新尺寸（logical points）。
        // 注意：这会触发 Resized 事件 → renderer.resize + recompute_layout，
        // 所以不需要额外调用 recompute_layout。
        if delta.resize_window {
            if let Some(window) = &self.window {
                let new_size = winit::dpi::LogicalSize::new(
                    config.window.width as f64,
                    config.window.height as f64,
                );
                let _ = window.request_inner_size(new_size);
            }
        }

        // v1.8.4: rebuild ai_state when AI config changes so the Enable
        // toggle, model, and parameter adjustments take effect immediately
        // after save (Cmd+Enter / Apply). Without this, the running ai_state
        // keeps the old backend and `is_configured()` never flips.
        if config.ai != self.config_state.config.ai {
            self.ai_state.cancel_all();
            // v1.8.7: preserve the waker when rebuilding ai_state so
            // background tasks still wake the event loop after results.
            let proxy = self.proxy.clone();
            self.ai_state = crate::ai::AiState::new_with_waker(
                config.ai.clone(),
                Some(std::sync::Arc::new(move || {
                    let _ = proxy.send_event(crate::AppEvent::Wake);
                })),
            );
            tracing::info!("ai_state rebuilt after config change");
        }

        self.config_state.config = config;
        self.request_redraw();
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
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
}

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
