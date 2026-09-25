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

/// PLAN_v11217 §3.5 (T4): defensive MiB clamp + byte conversion shared by the
/// two apply entry points (live-reload / profile switch bypass the load-path
/// normalization — review P2a double clamp, `apply_scrollback` precedent).
pub(super) fn output_cap_bytes(cap_mib: usize) -> usize {
    cap_mib.clamp(
        weft_core::blocks::OUTPUT_CAP_MIN_MIB,
        weft_core::blocks::OUTPUT_CAP_MAX_MIB,
    ) * 1024
        * 1024
}

/// PLAN_v11217 §3.5 (T4): apply the `[blocks] output_cap_mib` config to every
/// pane's terminal in every tab. Called by `apply_config` (live-reload AND
/// profile switch reach already-open tabs — review P2a', 全 tab 生效).
/// Mirrors [`apply_scrollback_to_all_panes`], including its defensive clamp:
/// the MiB→bytes conversion happens once here, and the tracker clamps again
/// internally.
pub(super) fn apply_blocks_output_cap_to_all_panes(tabs: &mut [Tab], cap_mib: usize) {
    let cap_bytes = output_cap_bytes(cap_mib);
    for tab in tabs.iter_mut() {
        for pane in tab.panes_mut() {
            if let Some(t) = pane.terminal.as_mut() {
                t.set_block_output_cap(cap_bytes);
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

/// v1.12.19 (PLAN_v11217 §3.8 T13b): apply the `[blocks] retained_limit`
/// config to every pane of every tab. Closes the historical gap where
/// retained_limit was creation-time-only (`apply_blocks_retained_limit` at
/// the spawn/restore chokepoints): the Settings Blocks row must take effect
/// immediately on already-open tabs, same as the output-cap walk above.
pub(super) fn apply_blocks_retained_limit_to_all_panes(tabs: &mut [Tab], limit: usize) {
    for tab in tabs.iter_mut() {
        apply_blocks_retained_limit(tab, limit);
    }
}

/// PLAN_v11217 §3.5 (T4): apply the `[blocks] output_cap_mib` config to every
/// pane's terminal of one tab. Per-tab twin of
/// [`apply_blocks_output_cap_to_all_panes`]; called by the tab/pane creation
/// chokepoints that already apply `retained_limit` (spawn chain + new tab +
/// restore + splits) so a freshly opened tab never silently reverts to the
/// default after a live cap change.
pub(super) fn apply_blocks_output_cap(tab: &mut Tab, cap_mib: usize) {
    let cap_bytes = output_cap_bytes(cap_mib);
    for pane in tab.panes_mut() {
        pane.set_blocks_output_cap(cap_bytes);
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

        // PLAN_v11217 §3.5 (T4): same all-tab walk for the block output cap —
        // live-reload AND profile switch must reach already-open tabs
        // (review P2a', 全 tab 生效; not creation-time-only like retained_limit).
        apply_blocks_output_cap_to_all_panes(
            self.sessions.tabs_mut(),
            config.blocks.output_cap_mib,
        );

        // v1.12.19 (T13b): retained_limit joins the all-tab walk — the
        // Settings Blocks row must take effect on already-open tabs without
        // a restart (closes the creation-time-only gap noted above).
        apply_blocks_retained_limit_to_all_panes(
            self.sessions.tabs_mut(),
            config.blocks.retained_limit,
        );

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

// Tests: #[path] child module (tab.rs precedent) — keeps this file within
// its architecture-gate budget.
#[cfg(test)]
#[path = "config_controller_tests.rs"]
mod tests;
