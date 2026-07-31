use serde::Deserialize;

/// A bindable action (the value side of a keybinding).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Deserialize)]
pub enum Action {
    #[serde(rename = "copy")]
    Copy,
    #[serde(rename = "paste")]
    Paste,
    #[serde(rename = "reload_config")]
    ReloadConfig,
    #[serde(rename = "scroll_page_up")]
    ScrollPageUp,
    #[serde(rename = "scroll_page_down")]
    ScrollPageDown,
    /// Scroll the scrollback buffer up by one line (Cmd+↑).
    #[serde(rename = "scroll_line_up")]
    ScrollLineUp,
    /// Scroll the scrollback buffer down by one line (Cmd+↓).
    #[serde(rename = "scroll_line_down")]
    ScrollLineDown,
    #[serde(rename = "scroll_to_top")]
    ScrollToTop,
    #[serde(rename = "scroll_to_bottom")]
    ScrollToBottom,
    #[serde(rename = "toggle_block_panel")]
    ToggleBlockPanel,
    #[serde(rename = "toggle_command_palette")]
    ToggleCommandPalette,
    /// Increase font size (Cmd+=). Multiplies the active font size by 1.1,
    /// clamped to 3× the configured base.
    #[serde(rename = "zoom_in")]
    ZoomIn,
    /// Decrease font size (Cmd+-). Divides the active font size by 1.1,
    /// clamped to 0.5× the configured base.
    #[serde(rename = "zoom_out")]
    ZoomOut,
    /// Reset font size to the configured base (Cmd+0).
    #[serde(rename = "zoom_reset")]
    ZoomReset,
    /// Open the in-grid search bar (Cmd+F). Typing debounces 150ms then
    /// scans visible content + recent scrollback for matches.
    #[serde(rename = "find_in_grid")]
    FindInGrid,
    /// Toggle between dark and light themes at runtime (Cmd+Shift+T).
    /// Independent of `ReloadConfig` (Cmd+Shift+,): reload re-reads the
    /// config file and resets the theme to whatever's named there, while
    /// ToggleTheme flips the in-memory `theme_is_dark` flag without
    /// touching disk.
    #[serde(rename = "toggle_theme")]
    ToggleTheme,
    /// Open a new tab (Cmd+T). Spawns a fresh shell session and switches
    /// to it.
    #[serde(rename = "new_tab")]
    NewTab,
    /// Close the current tab (Cmd+Ctrl+W). If this was the last tab, the app
    /// exits.
    #[serde(rename = "close_tab")]
    CloseTab,
    /// Switch to the next tab (Cmd+Shift+] or Cmd+Shift+Right).
    #[serde(rename = "next_tab")]
    NextTab,
    /// Switch to the previous tab (Cmd+Shift+[ or Cmd+Shift+Left).
    #[serde(rename = "prev_tab")]
    PrevTab,
    /// v1.0 S1: Open the Settings panel (Cmd+,). Modal overlay with tabs
    /// for Appearance / Font / Keybindings / Window.
    #[serde(rename = "toggle_settings")]
    ToggleSettings,
    /// v1.3: Split the active pane horizontally (Cmd+D). The active pane's
    /// content area is divided top/bottom, and a new shell session is
    /// spawned in the bottom half. The new pane becomes the focused pane.
    #[serde(rename = "split_horizontal")]
    SplitHorizontal,
    /// v1.3: Split the active pane vertically (Cmd+Shift+D). The active
    /// pane's content area is divided left/right, and a new shell session
    /// is spawned in the right half. The new pane becomes the focused pane.
    #[serde(rename = "split_vertical")]
    SplitVertical,
    /// v1.3: Move keyboard focus to the next pane in declaration order
    /// (Cmd+Option+] or Cmd+Option+Right). Wraps around to the first pane
    /// when invoked on the last pane.
    #[serde(rename = "focus_next_pane")]
    FocusNextPane,
    /// v1.3: Move keyboard focus to the previous pane in declaration order
    /// (Cmd+Option+[ or Cmd+Option+Left). Wraps around to the last pane
    /// when invoked on the first pane.
    #[serde(rename = "focus_prev_pane")]
    FocusPrevPane,
    /// v1.3: Close the focused pane (Cmd+W). When the tab has only
    /// one pane left, this is equivalent to `CloseTab` — the whole tab is
    /// closed and the surrounding tab is activated. Distinct from
    /// `CloseTab` (Cmd+Ctrl+W) which always closes the whole tab regardless
    /// of pane count.
    #[serde(rename = "close_pane")]
    ClosePane,
    /// v1.3.3: Zoom the active pane to fill the viewport (Cmd+Shift+Return).
    /// A second invocation restores the prior layout. While zoomed, focus
    /// cycling and direction focus are no-ops, and the underlying tree
    /// shape is preserved so un-zooming is a perfect inverse.
    #[serde(rename = "toggle_pane_zoom")]
    TogglePaneZoom,
    /// v1.3.3: Move focus to the nearest pane above the active pane
    /// (Cmd+Alt+Up). Spatial, not declaration-order — picks the candidate
    /// whose bottom edge is nearest the active pane's top edge, requiring
    /// real edge adjacency on the horizontal axis.
    #[serde(rename = "focus_pane_up")]
    FocusPaneUp,
    /// v1.3.3: Move focus to the nearest pane below the active pane
    /// (Cmd+Alt+Down). Mirror of `FocusPaneUp`.
    #[serde(rename = "focus_pane_down")]
    FocusPaneDown,
    /// v1.3.3: Move focus to the nearest pane to the left of the active
    /// pane (Cmd+Alt+Left). Replaces the v1.3.0 cyclic `FocusPrevPane`
    /// binding on Cmd+Alt+Left — cyclic focus is still reachable via
    /// Cmd+Alt+LeftBracket.
    #[serde(rename = "focus_pane_left")]
    FocusPaneLeft,
    /// v1.3.3: Move focus to the nearest pane to the right of the active
    /// pane (Cmd+Alt+Right). Replaces the v1.3.0 cyclic `FocusNextPane`
    /// binding on Cmd+Alt+Right — cyclic focus is still reachable via
    /// Cmd+Alt+Right Bracket.
    #[serde(rename = "focus_pane_right")]
    FocusPaneRight,
    /// v1.8.1: Open the command palette in AI mode (natural-language →
    /// shell command). Requires a configured local Ollama backend.
    #[serde(rename = "generate_command")]
    GenerateCommand,
    /// v1.8.1: Insert the currently-selected AI suggestion into the editor.
    /// Equivalent to pressing Enter on an AiSuggestion palette entry, but
    /// bindable as a dedicated key.
    #[serde(rename = "insert_ai_suggestion")]
    InsertAiSuggestion,
    /// v1.8.1: Cancel any in-flight AI request and clear the palette's AI
    /// suggestions. Does not close the palette.
    #[serde(rename = "cancel_ai_request")]
    CancelAiRequest,
}

/// v1.5.0: Canonical string form of an [`Action`], matching the serde
/// `rename` attributes. Used by the profile keybindings writer (and any
/// future code that needs the TOML string without going through serde).
/// Keep in sync with the `#[serde(rename = …)]` list above.
pub fn action_to_str(action: &Action) -> &'static str {
    match action {
        Action::Copy => "copy",
        Action::Paste => "paste",
        Action::ReloadConfig => "reload_config",
        Action::ScrollPageUp => "scroll_page_up",
        Action::ScrollPageDown => "scroll_page_down",
        Action::ScrollLineUp => "scroll_line_up",
        Action::ScrollLineDown => "scroll_line_down",
        Action::ScrollToTop => "scroll_to_top",
        Action::ScrollToBottom => "scroll_to_bottom",
        Action::ToggleBlockPanel => "toggle_block_panel",
        Action::ToggleCommandPalette => "toggle_command_palette",
        Action::ZoomIn => "zoom_in",
        Action::ZoomOut => "zoom_out",
        Action::ZoomReset => "zoom_reset",
        Action::FindInGrid => "find_in_grid",
        Action::ToggleTheme => "toggle_theme",
        Action::NewTab => "new_tab",
        Action::CloseTab => "close_tab",
        Action::NextTab => "next_tab",
        Action::PrevTab => "prev_tab",
        Action::ToggleSettings => "toggle_settings",
        Action::SplitHorizontal => "split_horizontal",
        Action::SplitVertical => "split_vertical",
        Action::FocusNextPane => "focus_next_pane",
        Action::FocusPrevPane => "focus_prev_pane",
        Action::ClosePane => "close_pane",
        Action::TogglePaneZoom => "toggle_pane_zoom",
        Action::FocusPaneUp => "focus_pane_up",
        Action::FocusPaneDown => "focus_pane_down",
        Action::FocusPaneLeft => "focus_pane_left",
        Action::FocusPaneRight => "focus_pane_right",
        Action::GenerateCommand => "generate_command",
        Action::InsertAiSuggestion => "insert_ai_suggestion",
        Action::CancelAiRequest => "cancel_ai_request",
    }
}
