//! Session, tab, configuration, and theme lifecycle controller.

use super::*;

impl App {
    /// Re-read config from disk and apply it live. Triggered by the
    /// reload-config keybinding (and the file watcher).
    pub(super) fn reload_config(&mut self) {
        let config = Config::load();
        self.apply_config(config);
        info!("config reloaded");
    }

    // ── v0.9 H1: Tab management ────────────────────────────────────────

    /// Get the current (rows, cols) of the active tab's terminal, falling
    /// back to the renderer's viewport estimate if the terminal is not yet
    /// initialized.
    pub(super) fn current_size(&self) -> (usize, usize) {
        if let Some(t) = self
            .sessions
            .tab(self.sessions.active_idx())
            .and_then(|tab| tab.terminal.as_ref())
        {
            let grid = t.grid();
            return (grid.num_rows, grid.num_cols);
        }
        // Fallback must use the same chrome-aware geometry as startup and
        // resize; deriving directly from the full renderer viewport would
        // recreate the hidden-last-row bug for a tab whose PTY failed.
        let (rows, cols) = self.grid_dims();
        if rows > 0 && cols > 0 {
            return (rows, cols);
        }
        (24, 80)
    }

    /// Cmd+T — open a new tab with a fresh shell session and switch to it.
    pub(super) fn new_tab(&mut self) {
        self.reset_ime_context("new tab");
        let (rows, cols) = self.current_size();
        // New tabs inherit the weft process's cwd (None = no chdir).
        let idx = self.sessions.open_tab(
            rows,
            cols,
            self.config_state.config.scrollback.lines,
            &self.proxy,
            None,
        );
        // Apply the current theme palette to the new terminal so it matches
        // the window's renderer theme (the atlas is shared per-window, not
        // per-tab — no atlas rebuild needed).
        if let Some(t) = self.sessions.active_mut().terminal.as_mut() {
            if let Some(r) = &self.renderer {
                t.set_palette(r.theme().palette);
            }
        }
        info!(tab_idx = idx, "new tab created");
        self.refresh_find_for_active_tab();
        self.scroll_active_tab_into_view();
        self.request_redraw();
    }

    /// Cmd+W — close the current tab and return the ordered side effects the
    /// application shell must drain. Closing the last tab requests exit;
    /// otherwise the previous tab becomes active and a redraw is requested.
    pub(super) fn close_tab(&mut self) -> Vec<Effect> {
        if self.sessions.len() <= 1 {
            info!("closing last tab, exiting app");
            let removed_idx = self.sessions.active_idx();
            // new_active is irrelevant when exiting; pass 0 as a sentinel.
            return effect::close_tab_effects(removed_idx, 0, true);
        }
        self.reset_ime_context("tab closed");
        let removed_idx = self.sessions.active_idx();
        let _is_last = self.sessions.close_active();
        let new_active = self.sessions.active_idx();
        // v0.9 W1+: clear hover state — tab indices shift after removal, so
        // a stale hovered_tab would point at the wrong tab. The next
        // CursorMoved will recompute it.
        self.tab_bar.hovered_tab = None;
        info!(closed = removed_idx, active = new_active, "tab closed");
        self.clamp_tab_scroll();
        self.scroll_active_tab_into_view();
        self.refresh_find_for_active_tab();
        effect::close_tab_effects(removed_idx, new_active, false)
    }

    /// v1.0 H4: Serialize all live tabs to the SQLite `tabs` table so the
    /// session layout (cwd + editor drafts) survives restarts. Best-effort:
    /// failures are logged but don't interrupt the caller. The PTY itself
    /// is NOT persisted (impossible to revive); only UI state is saved.
    pub(super) fn save_all_tabs(&self) {
        let Some(store) = self.sessions.block_store() else {
            return;
        };
        let snaps: Vec<_> = self
            .sessions
            .tabs()
            .iter()
            .enumerate()
            .filter_map(|(i, tab)| tab.to_snapshot(i))
            .collect();
        if let Err(e) = store.save_tabs(&snaps) {
            tracing::warn!(error = %e, "failed to save tab snapshots");
        }
    }

    /// v0.9 H1 Stage 4: re-bind the global find state to the active tab.
    ///
    /// Find query / regex / case-sensitive flags stay global (convenient for
    /// searching the same term across tabs), but the matches must come from
    /// the active tab's content — otherwise switching tabs shows the previous
    /// tab's match coordinates, which point at the wrong rows/blocks.
    ///
    /// Called from `new_tab` / `close_tab` / `next_tab` / `prev_tab`. When
    /// the find bar is closed or the query is empty this is a no-op.
    pub(super) fn refresh_find_for_active_tab(&mut self) {
        if !self.find.open || self.find.query.is_empty() {
            return;
        }
        // Drop any results still queued from the old tab's scan. Without this
        // drain, `poll_find_worker_results` would apply the stale `Partial`/
        // `Complete` results to the new active tab on the next frame.
        while self.find.worker.try_recv_result().is_some() {}
        self.find.worker_busy = false;
        // Clear stale matches so the FindUI count resets to 0 until the new
        // search completes.
        self.find.matches.clear();
        self.find.truncated = false;
        self.find.index = 0;
        self.find.block_matches.clear();
        self.find.block_truncated = false;
        self.find.block_index = 0;
        self.find.regex_error = None;
        // Arm the debounce so `maybe_refresh_find_results` re-submits the
        // query against the new active tab's snapshot on the next redraw.
        self.find.last_key = Some(std::time::Instant::now());
    }

    /// Cmd+Shift+] — switch to the next tab (wraps around). Returns effects
    /// for the shell to drain: a declarative `TabSwitched` event (for future
    /// consumers) plus a redraw request.
    pub(super) fn next_tab(&mut self) -> Vec<Effect> {
        if self.sessions.len() <= 1 {
            return Vec::new();
        }
        self.reset_ime_context("next tab");
        let (new, prev) = self.sessions.next();
        info!(active = new, "switched to next tab");
        self.refresh_find_for_active_tab();
        self.scroll_active_tab_into_view();
        vec![
            Effect::TabSwitched {
                new_idx: new,
                prev_idx: prev,
            },
            Effect::RequestRedraw,
        ]
    }

    /// Cmd+Shift+[ — switch to the previous tab (wraps around). Returns
    /// effects for the shell to drain.
    pub(super) fn prev_tab(&mut self) -> Vec<Effect> {
        if self.sessions.len() <= 1 {
            return Vec::new();
        }
        self.reset_ime_context("previous tab");
        let (new, prev) = self.sessions.prev();
        info!(active = new, "switched to prev tab");
        self.refresh_find_for_active_tab();
        self.scroll_active_tab_into_view();
        vec![
            Effect::TabSwitched {
                new_idx: new,
                prev_idx: prev,
            },
            Effect::RequestRedraw,
        ]
    }

    pub(super) fn tab_strip_layout(&self) -> Option<crate::layout::TabStripLayout> {
        let renderer = self.renderer.as_ref()?;
        let chrome_left = self.tab_bar_chrome_left();
        Some(crate::layout::layout_tab_strip(
            crate::layout::TabStripInput {
                viewport_width: renderer.viewport_width(),
                bar_height: renderer.tab_bar_height(),
                cell_width: renderer.cell_width() as f32,
                padding_x: renderer.padding_x(),
                chrome_left,
                traffic_lights_width: renderer.traffic_lights_width(),
                tab_count: self.sessions.len(),
                requested_scroll_offset: self.tab_bar.scroll_offset,
            },
        ))
    }

    pub(super) fn tab_bar_chrome_left(&self) -> f32 {
        let Some(renderer) = self.renderer.as_ref() else {
            return 0.0;
        };
        crate::ui_tokens::sidebar_placement(
            self.panel.open,
            renderer.sidebar_width(),
            renderer.sidebar_push_width(),
        )
        .tab_chrome_left
    }

    /// v1.2: Clamp `tab_scroll_offset` using the same layout product the
    /// renderer consumes.
    pub(super) fn clamp_tab_scroll(&mut self) {
        if let Some(layout) = self.tab_strip_layout() {
            self.tab_bar.scroll_offset = layout.scroll_offset;
        }
    }

    /// v1.2: Scroll the tab bar so the active tab is visible. Called after
    /// tab switch, new tab, close tab. If the active tab is already visible,
    /// no scroll happens.
    pub(super) fn scroll_active_tab_into_view(&mut self) {
        if let Some(layout) = self.tab_strip_layout() {
            self.tab_bar.scroll_offset = layout.scroll_offset_for_tab(self.sessions.active_idx());
        }
    }

    /// Build the `TabBarDrawState` for the renderer from the current tab list.
    /// Tab labels are the cwd basename (or "Tab N" when no cwd is set).
    pub(super) fn tab_bar_state(&self) -> TabBarDrawState {
        let labels: Vec<String> = self
            .sessions
            .tabs()
            .iter()
            .enumerate()
            .map(|(i, tab)| {
                // v1.0 fix: show full path when short (≤ 20 chars, e.g.
                // "/tmp", "/usr/local"), otherwise show basename only to
                // keep the tab label compact. Root "/" is kept as-is.
                let cwd = tab.terminal.as_ref().and_then(|t| t.cwd()).map(|c| {
                    if c.len() <= 20 {
                        c.to_string()
                    } else {
                        let base = c.rsplit('/').next().unwrap_or("");
                        if base.is_empty() {
                            "/".to_string()
                        } else {
                            base.to_string()
                        }
                    }
                });
                // v0.9 W1: when a command is running, show "cwd · cmd". When
                // idle, show only the cwd basename. (Previous version showed
                // the last completed command, but that kept the label stuck
                // on "cwd · cmd" forever after any command — e.g. "sleep 5"
                // never reverted, and "cd /" showed "/ · cd /". The running
                // indicator is enough; completed commands live in the history
                // panel and block view.)
                let cmd: Option<String> = tab
                    .terminal
                    .as_ref()
                    .and_then(|t| t.block_tracker().in_flight())
                    .map(|f| f.command.to_string());
                match (cwd, cmd) {
                    (Some(cwd), Some(cmd)) => {
                        let cmd_short: String = cmd.chars().take(16).collect();
                        let suffix = if cmd.chars().count() > 16 { "…" } else { "" };
                        format!("{} · {}{}", cwd, cmd_short, suffix)
                    }
                    (Some(cwd), None) => cwd,
                    (None, Some(cmd)) => {
                        let cmd_short: String = cmd.chars().take(16).collect();
                        let suffix = if cmd.chars().count() > 16 { "…" } else { "" };
                        format!("Tab {} · {}{}", i + 1, cmd_short, suffix)
                    }
                    (None, None) => format!("Tab {}", i + 1),
                }
            })
            .collect();
        TabBarDrawState {
            tab_count: self.sessions.len(),
            active_tab: self.sessions.active_idx(),
            labels,
            hovered_tab: self.tab_bar.hovered_tab,
            scroll_offset: self.tab_bar.scroll_offset,
            plus_hovered: self.tab_bar.plus_hovered,
            arrow_left_hovered: self.tab_bar.arrow_left_hovered,
            arrow_right_hovered: self.tab_bar.arrow_right_hovered,
            chrome_left: self.tab_bar_chrome_left(),
        }
    }

    pub(super) fn toggle_theme(&mut self) {
        // v1.0: Cmd+Shift+T now cycles through ALL built-in themes
        // (not just dark↔light). Find the current theme in the list and
        // advance to the next one, wrapping around at the end.
        if self.config_state.config.theme.follow_system {
            self.config_state.config.theme.follow_system = false;
            info!("follow_system disabled by manual toggle");
        }
        let themes = self.settings_theme_views();
        let current = &self.config_state.config.theme.name;
        // Find current theme index; default to 0 if not found.
        let idx = themes.iter().position(|t| t.name == current).unwrap_or(0);
        let next = &themes[(idx + 1) % themes.len()];
        let name = next.name.to_string();
        // Determine if the new theme is dark (for theme_is_dark tracking).
        let is_light = name.contains("light");
        self.config_state.theme_is_dark = !is_light;
        if !is_light {
            self.config_state.preferred_dark_theme = name.clone();
        }
        // Update config.theme.name so the Settings panel reflects the
        // currently active theme after a toggle.
        self.config_state.config.theme.name = name.clone();
        // v1.0 fix: sync settings_draft so the Settings panel shows the
        // current theme when toggling while the panel is open.
        if self.settings.open {
            self.settings.draft.theme.name = name.clone();
        }
        let theme = weft_core::config::Theme::resolve_named(&name, &self.config_state.config.theme);
        if let Some(r) = &mut self.renderer {
            r.set_theme(theme.clone());
        }
        // Reseed ALL tabs' palettes, not just the active one — otherwise
        // switching tabs shows the old theme's ANSI colors.
        for tab in self.sessions.tabs_mut() {
            if let Some(t) = &mut tab.terminal {
                t.set_palette(theme.palette);
            }
        }
        info!(dark = self.config_state.theme_is_dark, name, "theme cycled");
        self.request_redraw();
    }

    /// v0.9 U-D1: Apply a theme by name (light or dark), respecting the
    /// `[theme]` overrides from the loaded config. Updates `theme_is_dark`
    /// and reseeds the terminal palette so existing cells recolor on the
    /// next draw.
    pub(super) fn apply_theme_by_name(&mut self, name: &str, dark: bool) {
        let theme = weft_core::config::Theme::resolve_named(name, &self.config_state.config.theme);
        if let Some(r) = &mut self.renderer {
            r.set_theme(theme.clone());
        }
        if let Some(t) = self.sessions.active_mut().terminal.as_mut() {
            t.set_palette(theme.palette);
        }
        self.config_state.theme_is_dark = dark;
        // v1.0: sync config.theme.name + preferred_dark_theme so Settings
        // panel and Cmd+Shift+T stay in sync with the palette picker.
        self.config_state.config.theme.name = name.to_string();
        if dark {
            self.config_state.preferred_dark_theme = name.to_string();
        }
        info!(dark, name, "theme applied");
        self.request_redraw();
    }

    /// v0.9 W2+: List all resolvable theme names for the picker.
    ///
    /// Returns built-in names (sorted by display order, not alphabetically)
    /// followed by custom theme files discovered in
    /// `~/.config/weft/themes/` (stem of `.toml`/`.yaml`/`.yml` files).
    /// Built-ins are returned in a curated order (defaults first, then
    /// classic themes, then community themes) so the picker shows the most
    /// useful themes at the top.
    pub(super) fn available_theme_names(&self) -> Vec<String> {
        let mut names: Vec<String> = [
            "weft-warm",
            "weft-light",
            "warp",
            "dracula",
            "solarized-dark",
            "gruvbox-dark",
            "nord",
            "tokyo-night",
            "catppuccin",
            "one-dark",
            "monokai-pro",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        // Append custom theme file stems from the themes dir (if any).
        if let Some(dir) = weft_core::config::Theme::themes_dir() {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                let mut customs: Vec<String> = Vec::new();
                for entry in entries.flatten() {
                    if let Some(stem) = entry.path().file_stem().and_then(|s| s.to_str()) {
                        // Skip names that collide with built-ins.
                        if !names.iter().any(|n| n == stem) {
                            customs.push(stem.to_string());
                        }
                    }
                }
                customs.sort();
                names.extend(customs);
            }
        }
        names
    }

    /// v0.9 W2+: Rebuild `palette_results` for the theme picker sub-mode.
    ///
    /// Filters `themes` by the buffer (case-insensitive substring) and
    /// pushes each as a `PaletteEntry::Builtin(BuiltinCmd::SelectTheme)` —
    /// but the renderer projection (in `draw()`) maps each to a
    /// `(theme_name, "", "Theme")` tuple via the sub-mode check, so the
    /// picker rows show theme names with a "Theme" suffix label.
    pub(super) fn refresh_theme_picker_results(&mut self) {
        let (buffer, themes) = match &self.palette.submode {
            PaletteSubMode::SelectTheme { buffer, themes } => (buffer.clone(), themes.clone()),
            _ => return,
        };
        let q = buffer.to_lowercase();
        let mut results: Vec<PaletteEntry> = Vec::new();
        for name in &themes {
            if q.is_empty() || name.to_lowercase().contains(&q) {
                // We reuse PaletteEntry::Builtin(SelectTheme) as a sentinel;
                // the renderer projection distinguishes themes by sub-mode.
                results.push(PaletteEntry::Builtin(BuiltinCmd::SelectTheme));
            }
        }
        self.palette.results = results;
        if self.palette.selection >= self.palette.results.len() {
            self.palette.selection = 0;
        }
    }

    /// v0.9 W2+: Handle keyboard input in the theme picker sub-mode.
    ///
    /// - Up/Down: navigate the filtered theme list.
    /// - Enter: apply the selected theme (dark heuristic: name ends with
    ///   `-dark`/`-night` or contains "dracula"/"nord"/"tokyo"/"monokai"
    ///   → dark; otherwise treat as light). The palette closes after apply.
    /// - Escape: return to Search sub-mode (does not close the palette).
    /// - Backspace: pop the filter buffer; if empty, return to Search.
    /// - Printable char: append to the filter buffer and refresh.
    pub(super) fn handle_palette_select_theme_key(
        &mut self,
        key: KeyCode,
        text: Option<&str>,
    ) -> bool {
        let (buffer, themes) = match &self.palette.submode {
            PaletteSubMode::SelectTheme { buffer, themes } => (buffer.clone(), themes.clone()),
            _ => return false,
        };

        // Helper: given the current filtered selection index, resolve back to
        // the actual theme name from the full `themes` list.
        let selected_name = |sel: usize, buf: &str| -> Option<String> {
            let q = buf.to_lowercase();
            themes
                .iter()
                .filter(|n| q.is_empty() || n.to_lowercase().contains(&q))
                .nth(sel)
                .cloned()
        };

        match key {
            KeyCode::Escape => {
                // Return to search mode (keep palette open).
                self.palette.submode = PaletteSubMode::Search;
                self.palette.query.clear();
                self.palette.selection = 0;
                self.refresh_palette_results();
                self.request_redraw();
                true
            }
            KeyCode::Up => {
                if self.palette.selection > 0 {
                    self.palette.selection -= 1;
                }
                self.request_redraw();
                true
            }
            KeyCode::Down => {
                if self.palette.selection + 1 < self.palette.results.len() {
                    self.palette.selection += 1;
                }
                self.request_redraw();
                true
            }
            KeyCode::Enter => {
                if let Some(name) = selected_name(self.palette.selection, &buffer) {
                    // Heuristic: classify as dark unless the name clearly
                    // indicates a light theme. Affects follow_system parity.
                    let dark = !matches!(
                        name.as_str(),
                        "weft-light" | "solarized-light" | "gruvbox-light"
                    );
                    self.apply_theme_by_name(&name, dark);
                    self.palette.open = false;
                    self.palette.submode = PaletteSubMode::Search;
                    self.palette.query.clear();
                    self.request_redraw();
                }
                true
            }
            KeyCode::Backspace => {
                if buffer.is_empty() {
                    // Exit sub-mode back to search.
                    self.palette.submode = PaletteSubMode::Search;
                    self.palette.query.clear();
                    self.palette.selection = 0;
                    self.refresh_palette_results();
                } else {
                    // Pop filter char.
                    if let PaletteSubMode::SelectTheme { buffer, .. } = &mut self.palette.submode {
                        buffer.pop();
                        let buf = buffer.clone();
                        self.palette.selection = 0;
                        self.refresh_theme_picker_results();
                        // refresh_theme_picker_results reads buffer from self.
                        let _ = buf; // silence unused
                    }
                }
                self.request_redraw();
                true
            }
            _ => {
                let c = resolve_text_char(text, '\0', false);
                if c == '\0' || c.is_control() {
                    return false;
                }
                if let PaletteSubMode::SelectTheme { buffer, .. } = &mut self.palette.submode {
                    buffer.push(c);
                    self.palette.selection = 0;
                    self.refresh_theme_picker_results();
                }
                self.request_redraw();
                true
            }
        }
    }

    /// v0.9 U-D1: Poll the macOS system appearance and switch theme if it
    /// has changed since the last poll. Throttled to one query per second
    /// to avoid per-frame `NSUserDefaults` overhead. No-op when
    /// `[theme] follow_system = false`.
    pub(super) fn poll_system_appearance(&mut self) {
        // Throttle: at most one query per second. The throttle is shared
        // between the appearance check (below) and the F3-2 reduce-motion
        // check, since both read NSUserDefaults / NSWorkspace and 1Hz is
        // sufficient for both.
        if self.window_runtime.last_appearance_check.elapsed() < std::time::Duration::from_secs(1) {
            return;
        }
        self.window_runtime.last_appearance_check = std::time::Instant::now();

        // F3-2: Poll macOS Reduce Motion setting (always, regardless of
        // follow_system). When true, the running-command spinner uses a
        // static ● indicator instead of animated braille glyphs.
        let reduce = unsafe { system_reduce_motion() };
        if reduce != self.window_runtime.reduce_motion {
            self.window_runtime.reduce_motion = reduce;
        }

        // F6: Poll macOS Increase Contrast setting (always, regardless of
        // follow_system). When true, the renderer strengthens borders,
        // selection highlights and focus rings.
        let contrast = unsafe { system_increase_contrast() };
        if contrast != self.window_runtime.increase_contrast {
            self.window_runtime.increase_contrast = contrast;
            self.request_redraw();
        }

        if !self.config_state.config.theme.follow_system {
            return;
        }
        let dark = unsafe { system_appearance_is_dark() };
        if Some(dark) != self.window_runtime.last_system_appearance_dark {
            self.window_runtime.last_system_appearance_dark = Some(dark);
            let name = if dark {
                self.config_state
                    .config
                    .theme
                    .dark_name
                    .clone()
                    .unwrap_or_else(|| "weft-warm".to_string())
            } else {
                self.config_state
                    .config
                    .theme
                    .light_name
                    .clone()
                    .unwrap_or_else(|| "weft-light".to_string())
            };
            self.apply_theme_by_name(&name, dark);
        }
    }

    /// Apply a (possibly new) config: theme, font, keybindings, scrollback.
    /// Theme/font/scrollback changes take effect immediately; window size/title
    /// apply on the next launch.
    pub(super) fn apply_config(&mut self, config: Config) {
        // Theme — renderer defaults + terminal palette reseed (recolors all
        // Palette-indexed cells on the next draw).
        //
        // v0.9 U-D1: when `[theme] follow_system = true`, the system
        // appearance picks the theme (via `light_name` / `dark_name`,
        // defaulting to `weft-light` / `weft-warm`). The `name` field is
        // ignored in this mode.
        let theme = if config.theme.follow_system {
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
        }
        if let Some(t) = self.sessions.active_mut().terminal.as_mut() {
            t.set_palette(theme.palette);
        }
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
        if self.config_state.config.font.family != config.font.family
            || self.config_state.config.font.size != config.font.size
            || self.config_state.config.font.line_height != config.font.line_height
            || self.config_state.font_scale != 1.0
        {
            if let Some(r) = &mut self.renderer {
                let mut scaled = config.font.clone();
                scaled.size *= self.config_state.font_scale;
                r.rebuild_atlas(scaled);
            }
            self.recompute_layout();
        }

        // Window background opacity (layer-level transparency; text stays
        // opaque). Recolors the next frame. Window-level transparency is
        // startup-only — see `resumed`.
        if (self.config_state.config.window.opacity - config.window.opacity).abs() > f32::EPSILON {
            if let Some(r) = &mut self.renderer {
                r.set_opacity(config.window.opacity);
            }
        }

        // Content padding (changes usable rows/cols → recompute layout).
        if self.config_state.config.window.padding_x != config.window.padding_x
            || self.config_state.config.window.padding_y != config.window.padding_y
        {
            if let Some(r) = &mut self.renderer {
                r.set_padding((config.window.padding_x, config.window.padding_y));
            }
            self.recompute_layout();
        }

        // Keybindings.
        self.config_state.keybindings = config.keybindings();

        // Scrollback capacity.
        if let Some(t) = self.sessions.active_mut().terminal.as_mut() {
            let cols = t.grid().num_cols;
            t.grid_mut()
                .scrollback
                .set_max_lines(config.scrollback.lines, cols);
        }

        // v1.0 Logo: sync Dock icon if variant changed.
        if config.logo.variant != self.window_runtime.current_logo_variant {
            self.window_runtime.current_logo_variant = config.logo.variant;
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

        self.config_state.config = config;
        self.request_redraw();
    }

    /// Adjust `font_scale` for a zoom action (Cmd+= / Cmd+- / Cmd+0) and
    /// rebuild the glyph atlas with the scaled size. Each ZoomIn/Out step
    /// multiplies/divides by 1.1; `font_scale` is clamped to [0.5, 3.0] so
    /// the cell dimensions stay sane. ZoomReset restores 1.0.
    pub(super) fn zoom_action(&mut self, action: Action) {
        let new_scale = match action {
            Action::ZoomIn => (self.config_state.font_scale * 1.1).min(3.0),
            Action::ZoomOut => (self.config_state.font_scale / 1.1).max(0.5),
            Action::ZoomReset => 1.0,
            _ => return,
        };
        if (new_scale - self.config_state.font_scale).abs() < f32::EPSILON
            && action != Action::ZoomReset
        {
            return;
        }
        self.config_state.font_scale = new_scale;
        if let Some(r) = &mut self.renderer {
            let mut scaled = self.config_state.config.font.clone();
            scaled.size *= self.config_state.font_scale;
            r.rebuild_atlas(scaled);
        }
        self.recompute_layout();
        self.request_redraw();
    }
}
