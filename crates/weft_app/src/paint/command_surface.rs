// arch-gate: allow-over-800
// F4 command_surface: shell + row + state + keyboard + focus restore.
// All pieces are pure functions with unit tests; splitting would scatter
// one cohesive API across files. ~30 tests account for ~460 lines.
//! F4: Shared shell, row, state and keyboard protocol for command surfaces.
//!
//! Completion, Command Palette, Find and ContextMenu all render a floating
//! popup with the same visual language: drop shadow + tinted background +
//! 1px border + optional resize handles, plus a list of rows that respond to
//! hover/selected/disabled states. This module collects those shared pieces
//! so every surface paints them the same way and responds to the keyboard
//! with the same Up/Down/PageUp/PageDown/Enter/Esc/Tab contract.
//!
//! The shell/row builders are **pure vertex emitters** — they take a
//! `&mut Vec<f32>` and the resolved colors, and push raw vertex data. They
//! do not depend on `MetalRenderer` so they can be unit-tested without a
//! GPU. Text rasterization stays on the renderer (it needs the glyph atlas).
//!
//! See `docs/FRONTEND_DESIGN_OPTIMIZATION_PLAN.md` §4.5/4.6/4.9.

use crate::layout::Rect;
use crate::paint::primitives::{push_quad, push_triangle};
use crate::scene::{FocusId, FocusScope};
use weft_core::input::{KeyCode, Modifiers};

// ── Shell ──────────────────────────────────────────────────────────────

/// Visual configuration for a command-surface shell. Built by each surface
/// from its own layout + the active theme, then handed to
/// [`build_command_surface_shell`] to emit the shared background/border/shadow.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CommandSurfaceShell {
    /// Outer popup rectangle `[x0, y0, x1, y1]` in physical pixels.
    pub popup_rect: Rect,
    /// Drop-shadow padding in physical pixels. `0.0` skips the shadow
    /// (used by surfaces like Find that sit flush with the viewport edge).
    pub shadow_pad: f32,
    /// Whether to paint Warp-style resize handles on the top + right
    /// borders. Completion + Palette opt in; Find does not.
    pub with_resize_handles: bool,
    /// Border RGBA (already normalized). Shared across surfaces for visual
    /// consistency — typically `[0.5, 0.5, 0.5, 0.20]`.
    pub border_color: [f32; 4],
    /// Background RGBA (already normalized). Typically the theme background
    /// lifted by 8% toward white.
    pub bg_color: [f32; 4],
    /// UV rect sampling the space glyph (`mask = 0` → solid bg color).
    pub bg_uv: [f32; 4],
}

impl CommandSurfaceShell {
    /// Construct a shell with the canonical Weft styling: 8% lifted
    /// background, `[0.5, 0.5, 0.5, 0.20]` border, and the given shadow
    /// padding + resize-handle flag. The caller still picks `popup_rect`
    /// and `bg_uv` because those derive from the surface's layout + the
    /// renderer's glyph atlas.
    pub(crate) fn canonical(
        popup_rect: Rect,
        shadow_pad: f32,
        with_resize_handles: bool,
        theme_bg: [f32; 4],
        bg_uv: [f32; 4],
    ) -> Self {
        let bg_color = [
            theme_bg[0] + (1.0 - theme_bg[0]) * 0.08,
            theme_bg[1] + (1.0 - theme_bg[1]) * 0.08,
            theme_bg[2] + (1.0 - theme_bg[2]) * 0.08,
            1.0,
        ];
        Self {
            popup_rect,
            shadow_pad,
            with_resize_handles,
            border_color: [0.5, 0.5, 0.5, 0.20],
            bg_color,
            bg_uv,
        }
    }
}

/// Emit the shared shell (shadow + background + 1px border + optional
/// resize handles) for a command surface. Pure vertex emitter — pushes raw
/// `f32` data into `verts`, no renderer dependency.
pub(crate) fn build_command_surface_shell(verts: &mut Vec<f32>, shell: CommandSurfaceShell) {
    let [x0, y0, x1, y1] = shell.popup_rect;

    // Drop shadow (offset pad on all sides, low opacity black).
    if shell.shadow_pad > 0.0 {
        push_quad(
            verts,
            [
                x0 - shell.shadow_pad,
                y0 - shell.shadow_pad,
                x1 + shell.shadow_pad,
                y1 + shell.shadow_pad,
            ],
            shell.bg_uv,
            [0.0; 4],
            [0.0, 0.0, 0.0, 0.15],
        );
    }

    // Background.
    push_quad(
        verts,
        shell.popup_rect,
        shell.bg_uv,
        [0.0; 4],
        shell.bg_color,
    );

    // 1px border on all four edges.
    for (bx0, by0, bx1, by1) in [
        (x0, y0, x1, y0 + 1.0),
        (x0, y1 - 1.0, x1, y1),
        (x0, y0, x0 + 1.0, y1),
        (x1 - 1.0, y0, x1, y1),
    ] {
        push_quad(
            verts,
            [bx0, by0, bx1, by1],
            shell.bg_uv,
            [0.0; 4],
            shell.border_color,
        );
    }

    if shell.with_resize_handles {
        build_command_surface_resize_handles(verts, shell.popup_rect, shell.bg_uv);
    }
}

/// Emit Warp-style resize handles on the top + right borders. Pure vertex
/// emitter extracted from `MetalRenderer::draw_resize_handles` so any
/// command surface can opt in without depending on the renderer.
pub(crate) fn build_command_surface_resize_handles(
    verts: &mut Vec<f32>,
    popup_rect: Rect,
    bg_uv: [f32; 4],
) {
    let [x0, y0, x1, y1] = popup_rect;
    let handle_color = [0.55, 0.55, 0.55, 0.85];
    let s = 4.0_f32; // triangle half-size
    let gap = 6.0_f32; // gap between the two triangles (line length)

    // Right border: two triangles pointing inward (◀ ▶) + connecting line.
    let mid_y = (y0 + y1) / 2.0;
    let rx = x1;
    push_triangle(
        verts,
        [rx - s, mid_y - gap - s],
        [rx, mid_y - gap],
        [rx - s, mid_y - gap],
        handle_color,
        bg_uv,
    );
    push_triangle(
        verts,
        [rx - s, mid_y + gap + s],
        [rx, mid_y + gap],
        [rx - s, mid_y + gap],
        handle_color,
        bg_uv,
    );
    push_quad(
        verts,
        [rx - 1.5, mid_y - gap, rx, mid_y + gap],
        bg_uv,
        [0.0; 4],
        handle_color,
    );

    // Top border: two triangles pointing inward + connecting line.
    let mid_x = (x0 + x1) / 2.0;
    let ty = y0;
    push_triangle(
        verts,
        [mid_x - gap - s, ty],
        [mid_x - gap - s, ty + s],
        [mid_x - gap, ty + s / 2.0],
        handle_color,
        bg_uv,
    );
    push_triangle(
        verts,
        [mid_x + gap + s, ty],
        [mid_x + gap + s, ty + s],
        [mid_x + gap, ty + s / 2.0],
        handle_color,
        bg_uv,
    );
    push_quad(
        verts,
        [mid_x - gap, ty, mid_x + gap, ty + 1.5],
        bg_uv,
        [0.0; 4],
        handle_color,
    );
}

// ── Row ────────────────────────────────────────────────────────────────

/// Unified row data for command surfaces. Each surface maps its domain
/// model (PaletteEntry, completion::Match, FindMatch, …) into this shape so
/// the row renderer can treat them uniformly.
#[allow(dead_code)] // F4: data model for future unified row rendering
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CommandSurfaceRow {
    pub label: String,
    pub description: String,
    pub kind: String,
    pub shortcut: String,
}

/// Per-row visual state. Drives the background treatment. Multiple states
/// can be combined (e.g. a disabled row that happens to be selected still
/// renders as disabled). `disabled` wins over `selected`/`hovered`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CommandSurfaceRowState {
    pub selected: bool,
    pub hovered: bool,
    pub disabled: bool,
}

impl CommandSurfaceRowState {
    /// Effective background color for the row, derived from the theme.
    /// Returns `None` when the row has no special background (plain).
    pub(crate) fn background(self, theme_bg: [f32; 4], accent: [f32; 4]) -> Option<[f32; 4]> {
        if self.disabled {
            // Dim: blend theme_bg toward mid-gray so disabled rows read as
            // inert without disappearing against the popup background.
            return Some([
                theme_bg[0] * 0.6 + 0.2,
                theme_bg[1] * 0.6 + 0.2,
                theme_bg[2] * 0.6 + 0.2,
                1.0,
            ]);
        }
        if self.selected {
            // Selection: accent * 0.35 + bg * 0.65 — matches Settings/Palette.
            return Some([
                accent[0] * 0.35 + theme_bg[0] * 0.65,
                accent[1] * 0.35 + theme_bg[1] * 0.65,
                accent[2] * 0.35 + theme_bg[2] * 0.65,
                1.0,
            ]);
        }
        if self.hovered {
            // Hover: bg lifted 4% toward white — subtle.
            return Some([
                theme_bg[0] + (1.0 - theme_bg[0]) * 0.04,
                theme_bg[1] + (1.0 - theme_bg[1]) * 0.04,
                theme_bg[2] + (1.0 - theme_bg[2]) * 0.04,
                1.0,
            ]);
        }
        None
    }
}

/// Emit the background quad for a single command-surface row, honoring
/// selected/hover/disabled states. Pure vertex emitter — text is left to
/// the caller (it needs the glyph atlas). The quad is inset by 1px on the
/// left/right so it doesn't paint over the shell's border.
pub(crate) fn build_command_surface_row_bg(
    verts: &mut Vec<f32>,
    row_rect: Rect,
    state: CommandSurfaceRowState,
    theme_bg: [f32; 4],
    accent: [f32; 4],
    bg_uv: [f32; 4],
) {
    let Some(color) = state.background(theme_bg, accent) else {
        return;
    };
    let [x0, y0, x1, y1] = row_rect;
    push_quad(verts, [x0 + 1.0, y0, x1 - 1.0, y1], bg_uv, [0.0; 4], color);
}

// ── State ──────────────────────────────────────────────────────────────

/// Formal state of a command surface's result set. Replaces implicit
/// "blank space" rendering with an explicit, accessible status. Each
/// surface derives its state from its own inputs (Find: query/matches/
/// regex_error; Palette: query/results/store; Completion: matches).
#[allow(dead_code)] // F4: complete state set; some variants await wiring
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum CommandSurfaceState {
    /// Results are ready to display (the normal case).
    #[default]
    Ready,
    /// Search is in flight (e.g. the async Find worker is still scanning).
    Loading,
    /// Search completed with no results.
    Empty,
    /// Search failed (e.g. invalid regex, store error). Carries the
    /// human-readable reason so the surface can show it in-place.
    Error(String),
    /// Surface is temporarily disabled (e.g. no terminal attached, or the
    /// command store couldn't be opened). Distinct from Empty because the
    /// user's query is not at fault.
    Disabled,
}

impl CommandSurfaceState {
    /// Human-readable status text shown in the surface's empty area or
    /// footer. Empty string for `Ready` (no status to show).
    pub(crate) fn status_text(&self) -> String {
        match self {
            Self::Ready => String::new(),
            Self::Loading => "Loading…".into(),
            Self::Empty => "No results".into(),
            Self::Error(msg) => {
                if msg.is_empty() {
                    "Error".into()
                } else {
                    format!("Error: {msg}")
                }
            }
            Self::Disabled => "Unavailable".into(),
        }
    }

    /// Whether the surface should paint its results list. `false` for
    /// Loading/Empty/Error/Disabled — those show status text instead so
    /// the user never sees an unexplained blank popup.
    pub(crate) fn shows_results(&self) -> bool {
        matches!(self, Self::Ready)
    }

    /// Whether the status should be rendered with the error color. Used
    /// by surfaces to pick between `error` and `text_secondary` tokens.
    pub(crate) fn is_error(&self) -> bool {
        matches!(self, Self::Error(_))
    }
}

/// Derive the surface state for the Find bar from its observable inputs.
/// Pure function — surfaces call this to convert their ad-hoc fields into
/// the unified [`CommandSurfaceState`].
pub(crate) fn find_surface_state(
    query: &str,
    worker_busy: bool,
    total_matches: usize,
    regex_error: Option<&str>,
) -> CommandSurfaceState {
    if let Some(err) = regex_error {
        return CommandSurfaceState::Error(err.to_string());
    }
    if query.is_empty() {
        // No query yet — Ready (the surface shows the placeholder).
        return CommandSurfaceState::Ready;
    }
    if worker_busy {
        return CommandSurfaceState::Loading;
    }
    if total_matches == 0 {
        return CommandSurfaceState::Empty;
    }
    CommandSurfaceState::Ready
}

/// Derive the surface state for the Command Palette from its observable
/// inputs. Pure function.
pub(crate) fn palette_surface_state(
    query: &str,
    results_len: usize,
    store_present: bool,
) -> CommandSurfaceState {
    if !store_present && query.is_empty() {
        // No store attached — the palette can still show builtins, so this
        // is Ready, not Disabled. Disabled is reserved for when the palette
        // truly can't function (e.g. no terminal at all).
        return CommandSurfaceState::Ready;
    }
    if !query.is_empty() && results_len == 0 {
        return CommandSurfaceState::Empty;
    }
    CommandSurfaceState::Ready
}

/// Derive the surface state for the Completion popup. Pure function.
/// Completion is synchronous (no loading state), but it can be Empty.
pub(crate) fn completion_surface_state(matches_len: usize) -> CommandSurfaceState {
    if matches_len == 0 {
        CommandSurfaceState::Empty
    } else {
        CommandSurfaceState::Ready
    }
}

// ── Keyboard protocol ─────────────────────────────────────────────────

/// Unified keyboard action for command surfaces. Each surface (Completion,
/// Palette, Find) maps its key events to this enum via
/// [`resolve_command_surface_key`], then dispatches via its own handler.
/// This guarantees Up/Down/PageUp/PageDown/Enter/Esc/Tab behave
/// consistently across surfaces — the surface-specific differences live in
/// the *effect* of each action, not in which keys produce which action.
#[allow(dead_code)] // F4: formal protocol; controllers currently dispatch directly
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommandSurfaceKeyAction {
    /// Move selection up by one row.
    MoveUp,
    /// Move selection down by one row.
    MoveDown,
    /// Move selection up by one page (≈ visible row count).
    PageUp,
    /// Move selection down by one page.
    PageDown,
    /// Accept the current selection / submit.
    Accept,
    /// Close the surface without accepting.
    Cancel,
    /// Move focus to the next field in the surface (e.g. Find query →
    /// buttons, Palette form field → next field).
    CycleFocus,
    /// Key is not part of the unified protocol. The surface may still
    /// handle it (printable chars, Backspace, etc.).
    Unhandled,
}

/// Resolve a winit key + modifiers into a unified command-surface action.
/// Pure function — no App state. Surfaces call this first, then fall
/// through to their own handling for [`CommandSurfaceKeyAction::Unhandled`].
///
/// Modifier chords (SUPER/CONTROL/ALT) always return `Unhandled` so
/// app-level shortcuts (Cmd+F, Cmd+P, Cmd+R, etc.) still fire while a
/// surface is open. Shift is allowed through (Shift+Enter = previous match
/// in Find, Shift+Tab = reverse cycle — surfaces decide).
pub(crate) fn resolve_command_surface_key(
    key: KeyCode,
    mods: Modifiers,
) -> CommandSurfaceKeyAction {
    if mods.intersects(Modifiers::SUPER | Modifiers::CONTROL | Modifiers::ALT) {
        return CommandSurfaceKeyAction::Unhandled;
    }
    match key {
        KeyCode::Escape => CommandSurfaceKeyAction::Cancel,
        KeyCode::Up => CommandSurfaceKeyAction::MoveUp,
        KeyCode::Down => CommandSurfaceKeyAction::MoveDown,
        KeyCode::PageUp => CommandSurfaceKeyAction::PageUp,
        KeyCode::PageDown => CommandSurfaceKeyAction::PageDown,
        KeyCode::Enter => CommandSurfaceKeyAction::Accept,
        KeyCode::Tab => CommandSurfaceKeyAction::CycleFocus,
        _ => CommandSurfaceKeyAction::Unhandled,
    }
}

/// Apply a page-up/page-down movement to a selection index. Pure function
/// — returns the new index. Clamps to `[0, len-1]`. `page_size` is the
/// visible row count; the actual step is `page_size.max(1)`.
pub(crate) fn apply_page_selection(
    selection: usize,
    len: usize,
    page_size: usize,
    forward: bool,
) -> usize {
    if len == 0 {
        return 0;
    }
    let step = page_size.max(1);
    let new = if forward {
        selection.saturating_add(step)
    } else {
        selection.saturating_sub(step)
    };
    new.min(len - 1)
}

// ── Focus restore ─────────────────────────────────────────────────────

/// Compute the current logical [`FocusId`] from the overlay state. Pure
/// function — used by `InteractionState` to save the focus before a modal
/// opens so it can be restored (visually / for accessibility) when the
/// modal closes.
///
/// Priority mirrors [`crate::input_router::OverlayInputOwner::resolve`]:
/// Palette > Settings > Find > ContextMenu > PanelSearch > Editor.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compute_current_focus(
    palette_open: bool,
    settings_open: bool,
    find_open: bool,
    context_menu_open: bool,
    panel_search_focused: bool,
    editor_active: bool,
    completion_active: bool,
    active_tab: usize,
) -> Option<FocusId> {
    if palette_open {
        Some(FocusId::PaletteQuery)
    } else if settings_open {
        Some(FocusId::Settings)
    } else if find_open {
        Some(FocusId::FindQuery)
    } else if context_menu_open {
        Some(FocusId::ContextMenu)
    } else if panel_search_focused {
        Some(FocusId::SidebarSearch)
    } else if completion_active {
        Some(FocusId::Completion)
    } else if editor_active {
        Some(FocusId::Tab(active_tab))
    } else {
        None
    }
}

/// Decide whether opening a modal surface should save the previous focus.
/// Returns `Some(prev)` when a focus is already active and the new modal
/// would supersede it, or `None` when there's nothing to save (no prior
/// focus) or the modal is already the active focus (re-entry into the same
/// surface shouldn't overwrite the saved focus).
///
/// Pure function — the caller threads the result into `prev_focus`.
pub(crate) fn save_focus_for_modal(
    current_focus: Option<FocusId>,
    prev_focus: Option<FocusId>,
    opening: FocusId,
) -> Option<FocusId> {
    // If we already saved a prev focus, keep it — don't overwrite the
    // *original* focus when a second modal opens on top of the first.
    if prev_focus.is_some() {
        return prev_focus;
    }
    match current_focus {
        Some(f) if f != opening => Some(f),
        _ => None,
    }
}

// ── F6: Focus scope stack ─────────────────────────────────────────────

/// F6: Determine the current focus scope from the stack. Returns
/// [`FocusScope::Terminal`] when the stack is empty (the default scope).
///
/// Pure function — the caller passes `&interaction.focus_stack`.
#[allow(dead_code)] // F6: scaffolding; wired into the renderer in a follow-up
pub(crate) fn current_focus_scope(stack: &[FocusScope]) -> FocusScope {
    stack.last().copied().unwrap_or(FocusScope::Terminal)
}

/// F6: Push a scope onto the focus stack when a modal opens. The previous
/// scope remains underneath so closing the modal restores it.
///
/// Pure function — returns the new stack so the caller can assign it.
/// In practice the caller uses `stack.push(scope)` directly; this function
/// exists so the push/pop semantics are unit-testable without an `App`.
#[allow(dead_code)] // F6: scaffolding; wired into the renderer in a follow-up
pub(crate) fn push_focus_scope(mut stack: Vec<FocusScope>, scope: FocusScope) -> Vec<FocusScope> {
    stack.push(scope);
    stack
}

/// F6: Pop a scope from the focus stack when a modal closes. Returns the
/// popped scope, or `None` when the stack was already empty (defensive —
/// the caller should only pop when a modal was pushed).
///
/// Pure function — returns `(popped, remaining_stack)`.
#[allow(dead_code)] // F6: scaffolding; wired into the renderer in a follow-up
pub(crate) fn pop_focus_scope(mut stack: Vec<FocusScope>) -> (Option<FocusScope>, Vec<FocusScope>) {
    let popped = stack.pop();
    (popped, stack)
}

/// F6: Cycle focus within a list of focusable elements in the current scope.
/// Returns the next (forward) or previous (backward) `FocusId`. Wraps around.
/// Returns `None` when the list is empty. When the current focus is not in
/// the list, returns the first element (forward) or the last (backward).
///
/// Pure function — the caller builds the candidates list from the current
/// scope and modal state, then passes it here. This keeps the cycling logic
/// testable without an `App` instance.
#[allow(dead_code)] // F6: scaffolding; wired into the renderer in a follow-up
pub(crate) fn cycle_focus(
    candidates: &[FocusId],
    current: Option<FocusId>,
    forward: bool,
) -> Option<FocusId> {
    if candidates.is_empty() {
        return None;
    }
    let idx = current
        .and_then(|c| candidates.iter().position(|&f| f == c))
        .map(|i| {
            if forward {
                (i + 1) % candidates.len()
            } else {
                (i + candidates.len() - 1) % candidates.len()
            }
        })
        .unwrap_or_else(|| {
            // Current focus not in the list — start from the first (forward)
            // or the last (backward) so Tab always lands on a valid element.
            if forward {
                0
            } else {
                candidates.len() - 1
            }
        });
    Some(candidates[idx])
}

#[cfg(test)]
mod tests {
    use super::*;
    use weft_core::input::{KeyCode, Modifiers};

    // ── Shell ──────────────────────────────────────────────────────────

    #[test]
    fn canonical_shell_lifts_background_and_uses_unified_border() {
        let shell = CommandSurfaceShell::canonical(
            [0.0, 0.0, 100.0, 50.0],
            4.0,
            true,
            [0.1, 0.1, 0.1, 1.0],
            [0.0, 0.0, 1.0, 1.0],
        );
        // bg = bg + (1 - bg) * 0.08 = 0.1 + 0.9*0.08 = 0.172
        assert!((shell.bg_color[0] - 0.172).abs() < 1e-4);
        assert!((shell.bg_color[3] - 1.0).abs() < 1e-4);
        assert_eq!(shell.border_color, [0.5, 0.5, 0.5, 0.20]);
        assert!(shell.with_resize_handles);
        assert_eq!(shell.shadow_pad, 4.0);
    }

    #[test]
    fn build_shell_emits_shadow_bg_border_and_handles() {
        let mut verts = Vec::new();
        let shell = CommandSurfaceShell::canonical(
            [10.0, 10.0, 110.0, 60.0],
            4.0,
            true,
            [0.2, 0.2, 0.2, 1.0],
            [0.0, 0.0, 1.0, 1.0],
        );
        build_command_surface_shell(&mut verts, shell);

        // Each push_quad emits 6 verts × 12 floats = 72 floats.
        // Expected quads: 1 shadow + 1 bg + 4 border edges = 6 quads.
        // Plus resize handles: 4 triangles (3 verts × 12 floats = 36) +
        // 2 connecting lines (quads, 72 each). Total handle floats:
        // 4*36 + 2*72 = 144 + 144 = 288.
        // Shell quads: 6 * 72 = 432. Total = 432 + 288 = 720.
        assert_eq!(verts.len(), 720);

        // Shadow pad expands the rect by 4 on each side.
        // Verify the first quad's first vertex is the shadow top-left.
        assert_eq!(verts[0], 10.0 - 4.0); // x0 - shadow_pad
        assert_eq!(verts[1], 10.0 - 4.0); // y0 - shadow_pad
    }

    #[test]
    fn build_shell_skips_shadow_when_pad_zero() {
        let mut verts_with = Vec::new();
        let mut verts_without = Vec::new();
        build_command_surface_shell(
            &mut verts_with,
            CommandSurfaceShell::canonical(
                [0.0, 0.0, 100.0, 50.0],
                4.0,
                false,
                [0.2, 0.2, 0.2, 1.0],
                [0.0, 0.0, 1.0, 1.0],
            ),
        );
        build_command_surface_shell(
            &mut verts_without,
            CommandSurfaceShell::canonical(
                [0.0, 0.0, 100.0, 50.0],
                0.0,
                false,
                [0.2, 0.2, 0.2, 1.0],
                [0.0, 0.0, 1.0, 1.0],
            ),
        );
        // The version with shadow has one extra quad (the shadow).
        assert_eq!(verts_with.len() - verts_without.len(), 72);
    }

    #[test]
    fn resize_handles_emit_expected_vertex_count() {
        let mut verts = Vec::new();
        build_command_surface_resize_handles(&mut verts, [0.0, 0.0, 100.0, 50.0], [0.0; 4]);
        // 4 triangles (3 verts × 12 floats = 36) + 2 quads (6 verts × 12 = 72)
        // = 4*36 + 2*72 = 144 + 144 = 288.
        assert_eq!(verts.len(), 288);
    }

    // ── Row state ──────────────────────────────────────────────────────

    #[test]
    fn row_state_plain_has_no_background() {
        let state = CommandSurfaceRowState::default();
        assert_eq!(state.background([0.2; 4], [0.8; 4]), None);
    }

    #[test]
    fn row_state_selected_blends_accent_with_bg() {
        let state = CommandSurfaceRowState {
            selected: true,
            ..Default::default()
        };
        let bg = [0.2, 0.2, 0.2, 1.0];
        let accent = [0.8, 0.8, 0.8, 1.0];
        let color = state.background(bg, accent).unwrap();
        // accent*0.35 + bg*0.65 = 0.8*0.35 + 0.2*0.65 = 0.28 + 0.13 = 0.41
        assert!((color[0] - 0.41).abs() < 1e-4);
        assert_eq!(color[3], 1.0);
    }

    #[test]
    fn row_state_hovered_lifts_bg_slightly() {
        let state = CommandSurfaceRowState {
            hovered: true,
            ..Default::default()
        };
        let bg = [0.2, 0.2, 0.2, 1.0];
        let color = state.background(bg, [0.8; 4]).unwrap();
        // bg + (1-bg)*0.04 = 0.2 + 0.8*0.04 = 0.232
        assert!((color[0] - 0.232).abs() < 1e-4);
    }

    #[test]
    fn row_state_disabled_wins_over_selected_and_hovered() {
        let state = CommandSurfaceRowState {
            selected: true,
            hovered: true,
            disabled: true,
        };
        let bg = [0.2, 0.2, 0.2, 1.0];
        let color = state.background(bg, [0.8; 4]).unwrap();
        // disabled formula: bg*0.6 + 0.2 = 0.12 + 0.2 = 0.32
        assert!((color[0] - 0.32).abs() < 1e-4);
        // Must NOT match the selected or hover formula.
        let selected_color = CommandSurfaceRowState {
            selected: true,
            ..Default::default()
        }
        .background(bg, [0.8; 4])
        .unwrap();
        assert_ne!(color, selected_color);
    }

    #[test]
    fn build_row_bg_emits_quad_only_when_state_has_background() {
        let bg_uv = [0.0, 0.0, 1.0, 1.0];
        let theme_bg = [0.2, 0.2, 0.2, 1.0];
        let accent = [0.8, 0.8, 0.8, 1.0];

        // Plain row → no quad.
        let mut verts = Vec::new();
        build_command_surface_row_bg(
            &mut verts,
            [0.0, 0.0, 100.0, 20.0],
            CommandSurfaceRowState::default(),
            theme_bg,
            accent,
            bg_uv,
        );
        assert!(verts.is_empty());

        // Selected row → one quad (72 floats).
        let mut verts = Vec::new();
        build_command_surface_row_bg(
            &mut verts,
            [0.0, 0.0, 100.0, 20.0],
            CommandSurfaceRowState {
                selected: true,
                ..Default::default()
            },
            theme_bg,
            accent,
            bg_uv,
        );
        assert_eq!(verts.len(), 72);
        // Inset by 1px on left/right.
        assert_eq!(verts[0], 1.0); // x0 + 1
    }

    // ── State ──────────────────────────────────────────────────────────

    #[test]
    fn state_status_text_matches_each_variant() {
        assert_eq!(CommandSurfaceState::Ready.status_text(), "");
        assert_eq!(CommandSurfaceState::Loading.status_text(), "Loading…");
        assert_eq!(CommandSurfaceState::Empty.status_text(), "No results");
        assert_eq!(
            CommandSurfaceState::Error("bad regex".into()).status_text(),
            "Error: bad regex"
        );
        assert_eq!(
            CommandSurfaceState::Error(String::new()).status_text(),
            "Error"
        );
        assert_eq!(CommandSurfaceState::Disabled.status_text(), "Unavailable");
    }

    #[test]
    fn state_shows_results_only_when_ready() {
        assert!(CommandSurfaceState::Ready.shows_results());
        assert!(!CommandSurfaceState::Loading.shows_results());
        assert!(!CommandSurfaceState::Empty.shows_results());
        assert!(!CommandSurfaceState::Error("x".into()).shows_results());
        assert!(!CommandSurfaceState::Disabled.shows_results());
    }

    #[test]
    fn state_is_error_only_for_error_variant() {
        assert!(!CommandSurfaceState::Ready.is_error());
        assert!(!CommandSurfaceState::Loading.is_error());
        assert!(!CommandSurfaceState::Empty.is_error());
        assert!(CommandSurfaceState::Error("x".into()).is_error());
        assert!(!CommandSurfaceState::Disabled.is_error());
    }

    #[test]
    fn find_surface_state_regex_error_takes_priority() {
        let state = find_surface_state("foo", false, 5, Some("unbalanced paren"));
        assert_eq!(state, CommandSurfaceState::Error("unbalanced paren".into()));
    }

    #[test]
    fn find_surface_state_empty_query_is_ready() {
        let state = find_surface_state("", false, 0, None);
        assert_eq!(state, CommandSurfaceState::Ready);
    }

    #[test]
    fn find_surface_state_busy_is_loading() {
        let state = find_surface_state("foo", true, 0, None);
        assert_eq!(state, CommandSurfaceState::Loading);
    }

    #[test]
    fn find_surface_state_no_matches_is_empty() {
        let state = find_surface_state("foo", false, 0, None);
        assert_eq!(state, CommandSurfaceState::Empty);
    }

    #[test]
    fn find_surface_state_matches_is_ready() {
        let state = find_surface_state("foo", false, 3, None);
        assert_eq!(state, CommandSurfaceState::Ready);
    }

    #[test]
    fn palette_surface_state_empty_query_with_results_is_ready() {
        assert_eq!(
            palette_surface_state("", 4, true),
            CommandSurfaceState::Ready
        );
    }

    #[test]
    fn palette_surface_state_query_no_results_is_empty() {
        assert_eq!(
            palette_surface_state("zzz", 0, true),
            CommandSurfaceState::Empty
        );
    }

    #[test]
    fn palette_surface_state_no_store_still_ready_for_builtins() {
        assert_eq!(
            palette_surface_state("", 0, false),
            CommandSurfaceState::Ready
        );
    }

    #[test]
    fn completion_surface_state_empty_when_no_matches() {
        assert_eq!(completion_surface_state(0), CommandSurfaceState::Empty);
        assert_eq!(completion_surface_state(5), CommandSurfaceState::Ready);
    }

    // ── Keyboard protocol ──────────────────────────────────────────────

    #[test]
    fn resolve_key_returns_unhandled_for_modifier_chords() {
        assert_eq!(
            resolve_command_surface_key(KeyCode::Enter, Modifiers::SUPER),
            CommandSurfaceKeyAction::Unhandled
        );
        assert_eq!(
            resolve_command_surface_key(KeyCode::Up, Modifiers::CONTROL),
            CommandSurfaceKeyAction::Unhandled
        );
        assert_eq!(
            resolve_command_surface_key(KeyCode::Down, Modifiers::ALT),
            CommandSurfaceKeyAction::Unhandled
        );
    }

    #[test]
    fn resolve_key_maps_navigation_keys() {
        assert_eq!(
            resolve_command_surface_key(KeyCode::Escape, Modifiers::empty()),
            CommandSurfaceKeyAction::Cancel
        );
        assert_eq!(
            resolve_command_surface_key(KeyCode::Up, Modifiers::empty()),
            CommandSurfaceKeyAction::MoveUp
        );
        assert_eq!(
            resolve_command_surface_key(KeyCode::Down, Modifiers::empty()),
            CommandSurfaceKeyAction::MoveDown
        );
        assert_eq!(
            resolve_command_surface_key(KeyCode::PageUp, Modifiers::empty()),
            CommandSurfaceKeyAction::PageUp
        );
        assert_eq!(
            resolve_command_surface_key(KeyCode::PageDown, Modifiers::empty()),
            CommandSurfaceKeyAction::PageDown
        );
        assert_eq!(
            resolve_command_surface_key(KeyCode::Enter, Modifiers::empty()),
            CommandSurfaceKeyAction::Accept
        );
        assert_eq!(
            resolve_command_surface_key(KeyCode::Tab, Modifiers::empty()),
            CommandSurfaceKeyAction::CycleFocus
        );
    }

    #[test]
    fn resolve_key_shift_does_not_block_navigation() {
        // Shift is allowed through — surfaces decide (Shift+Enter = prev).
        assert_eq!(
            resolve_command_surface_key(KeyCode::Enter, Modifiers::SHIFT),
            CommandSurfaceKeyAction::Accept
        );
        assert_eq!(
            resolve_command_surface_key(KeyCode::Tab, Modifiers::SHIFT),
            CommandSurfaceKeyAction::CycleFocus
        );
    }

    #[test]
    fn resolve_key_unhandled_for_printable_and_misc() {
        assert_eq!(
            resolve_command_surface_key(KeyCode::Char('a'), Modifiers::empty()),
            CommandSurfaceKeyAction::Unhandled
        );
        assert_eq!(
            resolve_command_surface_key(KeyCode::Backspace, Modifiers::empty()),
            CommandSurfaceKeyAction::Unhandled
        );
        assert_eq!(
            resolve_command_surface_key(KeyCode::Left, Modifiers::empty()),
            CommandSurfaceKeyAction::Unhandled
        );
    }

    #[test]
    fn apply_page_selection_forward_advances_by_page_size() {
        // selection=5, len=20, page_size=8 → 5+8=13
        assert_eq!(apply_page_selection(5, 20, 8, true), 13);
    }

    #[test]
    fn apply_page_selection_backward_decreases_by_page_size() {
        // selection=13, len=20, page_size=8 → 13-8=5
        assert_eq!(apply_page_selection(13, 20, 8, false), 5);
    }

    #[test]
    fn apply_page_selection_clamps_to_last() {
        // selection=18, len=20, page_size=8 → 18+8=26 → clamp to 19
        assert_eq!(apply_page_selection(18, 20, 8, true), 19);
    }

    #[test]
    fn apply_page_selection_clamps_to_first() {
        // selection=3, len=20, page_size=8 → 3-8 → 0
        assert_eq!(apply_page_selection(3, 20, 8, false), 0);
    }

    #[test]
    fn apply_page_selection_zero_len_returns_zero() {
        assert_eq!(apply_page_selection(5, 0, 8, true), 0);
        assert_eq!(apply_page_selection(5, 0, 8, false), 0);
    }

    #[test]
    fn apply_page_selection_step_is_at_least_one() {
        // page_size=0 → step clamps to 1.
        assert_eq!(apply_page_selection(5, 20, 0, true), 6);
        assert_eq!(apply_page_selection(5, 20, 0, false), 4);
    }

    // ── Focus restore ──────────────────────────────────────────────────

    #[test]
    fn compute_focus_returns_none_when_nothing_active() {
        assert_eq!(
            compute_current_focus(false, false, false, false, false, false, false, 0),
            None
        );
    }

    #[test]
    fn compute_focus_palette_wins_over_settings_and_find() {
        let focus = compute_current_focus(true, true, true, false, false, true, false, 2);
        assert_eq!(focus, Some(FocusId::PaletteQuery));
    }

    #[test]
    fn compute_focus_settings_when_palette_closed() {
        let focus = compute_current_focus(false, true, true, false, false, false, false, 1);
        assert_eq!(focus, Some(FocusId::Settings));
    }

    #[test]
    fn compute_focus_find_when_no_palette_or_settings() {
        let focus = compute_current_focus(false, false, true, false, false, false, false, 0);
        assert_eq!(focus, Some(FocusId::FindQuery));
    }

    #[test]
    fn compute_focus_context_menu_beats_panel_and_editor() {
        let focus = compute_current_focus(false, false, false, true, true, true, false, 3);
        assert_eq!(focus, Some(FocusId::ContextMenu));
    }

    #[test]
    fn compute_focus_panel_search_when_no_modal() {
        let focus = compute_current_focus(false, false, false, false, true, true, false, 1);
        assert_eq!(focus, Some(FocusId::SidebarSearch));
    }

    #[test]
    fn compute_focus_completion_when_editor_and_popup_active() {
        let focus = compute_current_focus(false, false, false, false, false, true, true, 0);
        assert_eq!(focus, Some(FocusId::Completion));
    }

    #[test]
    fn compute_focus_tab_index_when_only_editor_active() {
        let focus = compute_current_focus(false, false, false, false, false, true, false, 4);
        assert_eq!(focus, Some(FocusId::Tab(4)));
    }

    #[test]
    fn save_focus_keeps_existing_prev_when_already_saved() {
        // Re-entry into a second modal must not overwrite the original focus.
        let prev = Some(FocusId::Tab(0));
        let result = save_focus_for_modal(Some(FocusId::FindQuery), prev, FocusId::PaletteQuery);
        assert_eq!(result, Some(FocusId::Tab(0)));
    }

    #[test]
    fn save_focus_returns_prev_when_current_differs_from_opening() {
        let result = save_focus_for_modal(Some(FocusId::Tab(2)), None, FocusId::PaletteQuery);
        assert_eq!(result, Some(FocusId::Tab(2)));
    }

    #[test]
    fn save_focus_returns_none_when_current_equals_opening() {
        // Opening the same surface again (e.g. re-focusing palette while
        // already open) shouldn't save palette as its own prev.
        let result = save_focus_for_modal(Some(FocusId::PaletteQuery), None, FocusId::PaletteQuery);
        assert_eq!(result, None);
    }

    #[test]
    fn save_focus_returns_none_when_no_current_focus() {
        let result = save_focus_for_modal(None, None, FocusId::Settings);
        assert_eq!(result, None);
    }

    // ── F6: Focus scope stack ─────────────────────────────────────────

    #[test]
    fn current_focus_scope_defaults_to_terminal_when_empty() {
        assert_eq!(current_focus_scope(&[]), FocusScope::Terminal);
    }

    #[test]
    fn current_focus_scope_returns_last_pushed() {
        let stack = vec![FocusScope::Terminal, FocusScope::Modal];
        assert_eq!(current_focus_scope(&stack), FocusScope::Modal);
    }

    #[test]
    fn push_focus_scope_appends_scope() {
        let stack = push_focus_scope(vec![], FocusScope::Modal);
        assert_eq!(stack, vec![FocusScope::Modal]);
        let stack = push_focus_scope(stack, FocusScope::Sidebar);
        assert_eq!(stack, vec![FocusScope::Modal, FocusScope::Sidebar]);
    }

    #[test]
    fn pop_focus_scope_returns_last_and_remaining() {
        let stack = vec![FocusScope::Terminal, FocusScope::Modal];
        let (popped, remaining) = pop_focus_scope(stack);
        assert_eq!(popped, Some(FocusScope::Modal));
        assert_eq!(remaining, vec![FocusScope::Terminal]);
    }

    #[test]
    fn pop_focus_scope_on_empty_returns_none() {
        let (popped, remaining) = pop_focus_scope(vec![]);
        assert_eq!(popped, None);
        assert!(remaining.is_empty());
    }

    // ── F6: cycle_focus ───────────────────────────────────────────────

    #[test]
    fn cycle_focus_forward_wraps_around() {
        let cands = [FocusId::FindQuery, FocusId::PaletteQuery];
        assert_eq!(
            cycle_focus(&cands, Some(FocusId::FindQuery), true),
            Some(FocusId::PaletteQuery)
        );
        assert_eq!(
            cycle_focus(&cands, Some(FocusId::PaletteQuery), true),
            Some(FocusId::FindQuery) // wraps
        );
    }

    #[test]
    fn cycle_focus_backward_wraps_around() {
        let cands = [FocusId::FindQuery, FocusId::PaletteQuery];
        assert_eq!(
            cycle_focus(&cands, Some(FocusId::FindQuery), false),
            Some(FocusId::PaletteQuery) // wraps backward
        );
        assert_eq!(
            cycle_focus(&cands, Some(FocusId::PaletteQuery), false),
            Some(FocusId::FindQuery)
        );
    }

    #[test]
    fn cycle_focus_single_candidate_returns_it() {
        let cands = [FocusId::Settings];
        assert_eq!(
            cycle_focus(&cands, Some(FocusId::Settings), true),
            Some(FocusId::Settings)
        );
        assert_eq!(
            cycle_focus(&cands, Some(FocusId::Settings), false),
            Some(FocusId::Settings)
        );
    }

    #[test]
    fn cycle_focus_empty_returns_none() {
        let cands: [FocusId; 0] = [];
        assert_eq!(cycle_focus(&cands, Some(FocusId::FindQuery), true), None);
        assert_eq!(cycle_focus(&cands, None, false), None);
    }

    #[test]
    fn cycle_focus_current_not_in_list_starts_at_first_or_last() {
        let cands = [FocusId::FindQuery, FocusId::PaletteQuery];
        // Current not in list → forward starts at first (index 0).
        assert_eq!(
            cycle_focus(&cands, Some(FocusId::Settings), true),
            Some(FocusId::FindQuery)
        );
        // Current not in list → backward starts at last.
        assert_eq!(
            cycle_focus(&cands, Some(FocusId::Settings), false),
            Some(FocusId::PaletteQuery)
        );
    }

    #[test]
    fn cycle_focus_none_current_forward_starts_at_first() {
        let cands = [FocusId::FindQuery, FocusId::PaletteQuery];
        assert_eq!(cycle_focus(&cands, None, true), Some(FocusId::FindQuery));
    }

    #[test]
    fn cycle_focus_three_candidates_cycles_in_order() {
        let cands = [FocusId::Tab(0), FocusId::Completion, FocusId::SidebarSearch];
        // Forward: 0 → 1 → 2 → 0
        assert_eq!(
            cycle_focus(&cands, Some(FocusId::Tab(0)), true),
            Some(FocusId::Completion)
        );
        assert_eq!(
            cycle_focus(&cands, Some(FocusId::Completion), true),
            Some(FocusId::SidebarSearch)
        );
        assert_eq!(
            cycle_focus(&cands, Some(FocusId::SidebarSearch), true),
            Some(FocusId::Tab(0))
        );
    }
}
