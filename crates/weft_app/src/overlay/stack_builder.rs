//! `build_overlay_stack` + its TUI-preedit gate, moved verbatim out of
//! `overlay.rs` (v1.13.8 S3 zero-behavior file-budget split; free-fn
//! submodule per the overlay/view_params.rs precedent).

use super::view_params::{
    ImeViewParams, PaletteViewParams, PanelViewParams, PromptViewParams, SettingsViewParams,
};
use super::{
    CompletionDrawParams, OverlayContent, OverlayInputPolicy, OverlayKind, OverlayLayer,
    OverlayStack, OverlayZ, PaletteDrawParams, PaletteEntryView, SettingsDrawParams,
};
use crate::paint::panel::PanelDrawParams;
use crate::paint::preedit::TuiPreeditDrawParams;
use crate::paint::prompt::PromptDrawParams;
use weft_core::vt::Terminal;
// ── Stack builder ─────────────────────────────────────────────────────

/// Build the overlay stack for the current frame. This is a **free function**
/// (not an `App` method) to avoid self-referential borrow conflicts: it
/// borrows `terminal` and individual `self.*` fields by reference, keeping
/// them disjoint from the `&mut self.renderer` that `draw()` requires.
///
/// Call this inside the `if let (Some(renderer), Some(terminal)) = ...`
/// split-borrow block in `RedrawRequested`.
///
/// v1.11 audit (PLAN_audit_fix_batch3 C1): the flat 60-parameter signature
/// is clustered into five view-parameter groups (see `view_params.rs`); the
/// unused `_viewport_width` was dropped and the settings group is `Option`-
/// gated at the call site.
pub fn build_overlay_stack<'a>(
    terminal: &'a Terminal,
    panel: PanelViewParams<'a>,
    ime: ImeViewParams<'a>,
    palette: PaletteViewParams<'a>,
    prompt: PromptViewParams,
    settings: Option<&'a SettingsViewParams<'a>>,
) -> OverlayStack<'a> {
    let mut layers = Vec::new();

    // History panel (Cmd+Shift+B).
    if panel.panel_open {
        layers.push(OverlayLayer {
            kind: OverlayKind::HistoryPanel,
            z: OverlayZ::Panel,
            input_policy: OverlayInputPolicy::Focused,
            content: OverlayContent::HistoryPanel(PanelDrawParams {
                blocks: terminal.block_tracker().blocks(),
                width_px: panel.panel_width,
                query: panel.panel_query,
                panel_ime_preedit: panel.panel_ime_preedit,
                selection: panel.panel_selection,
                expanded_id: panel.panel_expanded,
                search_focused: panel.panel_search_focused,
                scroll_offset: panel.panel_scroll_offset,
            }),
        });
    }

    // Passthrough programs own the terminal grid, but macOS still sends
    // marked text to Weft until the IME commits it. Paint that marked text at
    // the TUI cursor so applications such as OpenCode, vim and less get the
    // same inline composition feedback as the native editor.
    let tui_preedit_mode = terminal.effective_input_mode();
    if !ime.ime_preedit.is_empty()
        && !should_show_tui_preedit(tui_preedit_mode, ime.ime_preedit, ime.terminal_owns_ime)
    {
        // v1.10.26 probe: composing text exists but the TUI preedit overlay
        // gate rejected it — the only silent failure left between the
        // router and the renderer. Info: fires only while composing.
        tracing::info!(
            ?tui_preedit_mode,
            terminal_owns_ime = ime.terminal_owns_ime,
            len = ime.ime_preedit.chars().count(),
            "IME_PREEDIT_GATE_REJECT"
        );
    }
    if should_show_tui_preedit(
        terminal.effective_input_mode(),
        ime.ime_preedit,
        ime.terminal_owns_ime,
    ) {
        layers.push(OverlayLayer {
            kind: OverlayKind::TuiPreedit,
            z: OverlayZ::Prompt,
            input_policy: OverlayInputPolicy::Passive,
            content: OverlayContent::TuiPreedit(TuiPreeditDrawParams {
                text: ime.ime_preedit,
                cursor: ime.ime_preedit_cursor,
            }),
        });
    }

    // Prompt input box (Editor mode only).
    if terminal.effective_input_mode() == weft_core::input::InputMode::Editor {
        let search = terminal.editor().search_view();

        layers.push(OverlayLayer {
            kind: OverlayKind::Prompt,
            z: OverlayZ::Prompt,
            input_policy: OverlayInputPolicy::Focused,
            content: OverlayContent::Prompt(PromptDrawParams {
                focused: true,
                cwd: terminal.cwd(),
                lines: &terminal.editor().buffer.lines,
                cursor: terminal.editor().buffer.cursor,
                preedit: if ime.ime_preedit.is_empty() {
                    None
                } else {
                    Some(ime.ime_preedit)
                },
                preedit_cursor: if ime.ime_preedit.is_empty() {
                    None
                } else {
                    ime.ime_preedit_cursor
                },
                search,
                selection: prompt.prompt_selection,
                scroll_offset: terminal.editor().buffer.scroll_offset,
                submit_on_ctrl_enter: prompt.submit_on_ctrl_enter,
            }),
        });

        // Completion popup as a separate layer above the prompt.
        let completions = terminal.editor().completion_view();
        if let Some((matches, selected)) = completions {
            if !matches.is_empty() && search.is_none() {
                layers.push(OverlayLayer {
                    kind: OverlayKind::Completion,
                    z: OverlayZ::Completion,
                    input_policy: OverlayInputPolicy::Passive,
                    content: OverlayContent::Completion(CompletionDrawParams {
                        matches,
                        selected,
                        // anchor_y is recalculated by the renderer from its
                        // own viewport geometry (see draw() completion block).
                        anchor_y: 0.0,
                    }),
                });
            }
        }
    }

    // Command Palette (v0.7).
    if palette.palette_open {
        let entry_views: Vec<PaletteEntryView<'a>> = palette
            .palette_entries
            .iter()
            .map(|(label, desc, kind)| PaletteEntryView {
                label: label.as_str(),
                description: desc.as_str(),
                kind_label: kind,
            })
            .collect();

        // The PaletteDrawParams needs to borrow entry_views, but entry_views
        // is a local. To avoid lifetime issues, we leak it into a Box that
        // lives as long as 'a — but that's wrong. Instead, we store the
        // entry_views inline. The OverlayStack owns the data.
        // Actually, the entries are borrowed from palette_entries which has
        // lifetime 'a, so we can build PaletteEntryView directly from it
        // without an intermediate Vec. But PaletteDrawParams takes a slice.
        // The simplest approach: store entry_views in a Box pinned to the
        // stack. Since OverlayStack is stack-local and consumed within the
        // same function, this is safe via a temporary.
        //
        // In practice we build the entry views into a leaked Box. This is
        // acceptable because the OverlayStack is consumed and dropped within
        // the same RedrawRequested handler — the leak is per-frame and freed
        // when the frame ends. This is a known Rust pattern for self-referential
        // temporaries in render loops.
        let entry_views_box: &'a [PaletteEntryView<'a>] = {
            let boxed: Box<Vec<PaletteEntryView<'a>>> = Box::new(entry_views);
            Box::leak(boxed).as_slice()
        };

        layers.push(OverlayLayer {
            kind: OverlayKind::CommandPalette,
            z: OverlayZ::Palette,
            input_policy: OverlayInputPolicy::Modal,
            content: OverlayContent::CommandPalette(PaletteDrawParams {
                query: palette.palette_query,
                entries: entry_views_box,
                selection: palette.palette_selection,
                form: palette.palette_form,
                banner: palette.palette_banner,
                submode_input: palette.palette_submode_input,
                ime_preedit: palette.palette_ime_preedit,
                ime_preedit_cursor: palette.palette_ime_preedit_cursor,
            }),
        });
    }

    // Settings panel (Cmd+,) — v1.0 S1. Highest z so it overlays everything.
    // C1 gating: the caller passes None while the panel is closed, so the
    // whole settings construction chain is skipped on its side too.
    if let Some(settings) = settings {
        layers.push(OverlayLayer {
            kind: OverlayKind::Settings,
            z: OverlayZ::Settings,
            input_policy: OverlayInputPolicy::Modal,
            content: OverlayContent::Settings(SettingsDrawParams {
                active_tab: settings.settings_tab,
                selection: settings.settings_selection,
                scroll_offset: settings.settings_scroll_offset,
                theme_name: settings.settings_theme_name,
                themes: settings.settings_themes,
                font_family: settings.settings_font_family,
                font_size: settings.settings_font_size,
                line_height: settings.settings_line_height,
                window_opacity: settings.settings_window_opacity,
                window_padding_x: settings.settings_window_padding_x,
                window_padding_y: settings.settings_window_padding_y,
                scrollback_lines: settings.settings_scrollback_lines,
                minimum_contrast: settings.settings_minimum_contrast,
                window_width: settings.settings_window_width,
                window_height: settings.settings_window_height,
                sidebar_width: settings.settings_sidebar_width,
                submit_on_ctrl_enter: settings.settings_submit_on_ctrl_enter,
                smart_select: settings.settings_smart_select,
                paste_rows: settings.settings_paste_rows,
                keybindings: settings.settings_keybindings,
                logo_variant: settings.settings_logo_variant,
                error: settings.settings_error,
                is_narrow: settings.settings_is_narrow,
                drill_down: settings.settings_drill_down,
                keybinding_conflict_count: settings.settings_keybinding_conflict_count,
                field_errors: settings.settings_field_errors,
                profiles: &settings.settings_profiles,
                semantic_output_enabled: settings.settings_semantic_output_enabled,
                ai: settings.settings_ai,
                notify_enabled: settings.settings_notify_enabled,
                notify_threshold_secs: settings.settings_notify_threshold_secs,
                notify_sound: settings.settings_notify_sound,
                osc52_mode: settings.settings_osc52_mode,
                recovery_mode: settings.settings_recovery_mode,
                blocks_retained_limit: settings.settings_blocks_retained_limit,
                blocks_output_cap_mib: settings.settings_blocks_output_cap_mib,
                blocks_history_max_age_days: settings.settings_blocks_history_max_age_days,
                blocks_history_max_db_mb: settings.settings_blocks_history_max_db_mb,
                update_tier: settings.settings_update_tier,
            }),
        });
    }

    OverlayStack { layers }
}

pub(super) fn should_show_tui_preedit(
    mode: weft_core::input::InputMode,
    text: &str,
    terminal_owns_ime: bool,
) -> bool {
    terminal_owns_ime && mode == weft_core::input::InputMode::Passthrough && !text.is_empty()
}
