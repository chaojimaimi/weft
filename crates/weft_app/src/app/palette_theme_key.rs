//! v0.9 W2+: the theme-picker sub-mode keyboard handler, moved out of
//! `lifecycle_controller.rs` (v1.14.0 PLAN_v1140 file-budget split; verbatim
//! code motion — the tab/theme lifecycle file crossed the 800-line gate).

use super::helpers::resolve_text_char;
use weft_core::input::{KeyCode, Modifiers};

use crate::palette_state::PaletteSubMode;

impl crate::App {
    /// v0.9 W2+: Handle keyboard input in the theme picker sub-mode.
    ///
    /// - Up/Down: navigate the filtered theme list.
    /// - Enter: apply the selected theme (dark heuristic: name ends with
    ///   `-dark`/`-night` or contains "dracula"/"nord"/"tokyo"/"monokai"
    ///   → dark; otherwise treat as light). The palette closes after apply.
    /// - Escape: return to Search sub-mode (does not close the palette).
    /// - Backspace: pop the filter buffer; if empty, return to Search.
    /// - Printable char: append to the filter buffer and refresh.
    pub(crate) fn handle_palette_select_theme_key(
        &mut self,
        key: KeyCode,
        mods: Modifiers,
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
        let filtered_len = themes
            .iter()
            .filter(|name| {
                buffer.is_empty() || name.to_lowercase().contains(&buffer.to_lowercase())
            })
            .count();

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
                if self.palette.selection + 1 < filtered_len {
                    self.palette.selection += 1;
                }
                self.request_redraw();
                true
            }
            KeyCode::PageUp | KeyCode::PageDown => {
                self.palette.selection = crate::paint::command_surface::apply_page_selection(
                    self.palette.selection,
                    filtered_len,
                    self.interaction.popup_max_rows,
                    key == KeyCode::PageDown,
                );
                self.request_redraw();
                true
            }
            KeyCode::Tab => {
                self.palette.selection = crate::input_router::cycle_list_selection(
                    self.palette.selection,
                    filtered_len,
                    !mods.contains(Modifiers::SHIFT),
                );
                self.request_redraw();
                true
            }
            KeyCode::Enter => {
                if let Some(name) = selected_name(self.palette.selection, &buffer) {
                    // v1.12: 按解析后背景的相对亮度判定，不再猜名字。
                    let dark =
                        crate::palette_state::theme_is_dark(&name, &self.config_state.config.theme);
                    self.apply_theme_by_name(&name, dark);
                    self.palette.submode = PaletteSubMode::Search;
                    self.palette.query.clear();
                    self.close_palette();
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
}
