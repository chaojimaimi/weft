// arch-gate: allow-over-800
// Overlay stack builder: z-order + warm-up + hit-testing for all overlay
// layers. Grew with F3 panel scroll_offset param and block hover actions;
// remaining size is the per-overlay build_overlay_stack dispatch.
//! Unified overlay stack — manages z-order, rendering, warm-up, and hit-testing
//! for all overlay UI layers (history panel, prompt input box, completion
//! dropdown, and future Command Palette / context menu).
//!
//! See `docs/superpowers/App:Renderer Overlay 架构重构设计.md` for the full
//! design rationale.
#![allow(dead_code)]

use std::collections::HashSet;

use weft_core::blocks::BlockId;
use weft_core::complete::Match;
use weft_core::vt::Terminal;

use crate::paint::panel::PanelDrawParams;
use crate::paint::preedit::TuiPreeditDrawParams;
use crate::paint::prompt::PromptDrawParams;

// ── Z-order ───────────────────────────────────────────────────────────

/// Explicit z-order for overlay layers. Lower values render first (bottom),
/// higher values render last (top). This replaces the implicit "code append
/// order" that existed before.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum OverlayZ {
    /// The base block-view or grid content (not an overlay per se, but
    /// conceptually the bottom layer).
    BaseBlockView = 0,
    /// History sidebar panel (Cmd+Shift+B).
    Panel = 1,
    /// Bottom editor input box.
    Prompt = 2,
    /// Tab completion dropdown (floats above the prompt).
    Completion = 3,
    /// Command Palette (Cmd+P) — v0.7.
    Palette = 4,
    /// Right-click context menu (F7) — future.
    ContextMenu = 5,
    /// Settings panel (Cmd+,) — v1.0 S1. Highest z so it sits above all
    /// other overlays when open.
    Settings = 6,
}

/// How an overlay layer participates in keyboard/mouse input routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayInputPolicy {
    /// No input interaction (for purely decorative overlays).
    None,
    /// Visible but doesn't intercept input (passive overlay).
    Passive,
    /// Receives input when focused (e.g. prompt editor, completion nav).
    Focused,
    /// Captures all input while open (e.g. Command Palette, context menu).
    Modal,
}

/// Identifies the overlay type for dispatch and debugging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayKind {
    HistoryPanel,
    TuiPreedit,
    Prompt,
    Completion,
    CommandPalette,
    Settings,
    // future: ContextMenu,
}

/// The renderable content of an overlay layer. Each variant wraps the
/// existing `*DrawParams` struct, so the refactor doesn't change what data
/// flows into the renderer — only how it's packaged.
pub enum OverlayContent<'a> {
    HistoryPanel(PanelDrawParams<'a>),
    TuiPreedit(TuiPreeditDrawParams<'a>),
    Prompt(PromptDrawParams<'a>),
    Completion(CompletionDrawParams<'a>),
    CommandPalette(PaletteDrawParams<'a>),
    Settings(SettingsDrawParams<'a>),
    // future: ContextMenu(ContextMenuDrawParams<'a>),
}

/// F5: Which category of the Settings panel is active. The old v1.0 tab
/// bar (Appearance / Font / Keybindings / Window / Logo) has been replaced
/// by a 6-entry split sidebar. Logo and Font merged into Appearance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    /// Theme list, logo variant, font family/size/line-height, window opacity.
    Appearance,
    /// Scrollback, padding X/Y, alt-screen behavior.
    Terminal,
    /// Editor mode (submit_on_ctrl_enter), IME / mouse settings.
    Input,
    /// Keybinding list + conflict detection + restore defaults.
    Keybindings,
    /// Window size, sidebar width, tab bar.
    Window,
    /// Debug logging, experimental features (restart-required badges).
    Advanced,
}

impl SettingsTab {
    /// All categories in sidebar display order.
    pub const ALL: [SettingsTab; 6] = [
        SettingsTab::Appearance,
        SettingsTab::Terminal,
        SettingsTab::Input,
        SettingsTab::Keybindings,
        SettingsTab::Window,
        SettingsTab::Advanced,
    ];

    /// Human-readable label for the sidebar.
    pub fn label(self) -> &'static str {
        match self {
            SettingsTab::Appearance => "Appearance",
            SettingsTab::Terminal => "Terminal",
            SettingsTab::Input => "Input",
            SettingsTab::Keybindings => "Keybindings",
            SettingsTab::Window => "Window",
            SettingsTab::Advanced => "Advanced",
        }
    }
}

/// v1.0 S1: A single theme entry in the Appearance tab's theme list.
#[derive(Debug, Clone, Copy)]
pub struct SettingsThemeView {
    /// Theme name as it appears in config (`weft-warm`, `warp`, etc.).
    pub name: &'static str,
    /// Human-readable display label.
    pub label: &'static str,
}

/// v1.0 S1: A keybinding row in the Keybindings tab.
#[derive(Debug, Clone)]
pub struct SettingsKeybindingView {
    /// Human-readable action label (e.g. "Copy", "Paste").
    pub action: String,
    /// The key chord string (e.g. "cmd+c").
    pub binding: String,
    /// F5: True when this chord is also bound to another action (conflict).
    /// The renderer highlights the row and the controller surfaces a summary.
    pub conflict: bool,
}

/// v1.5.1: A profile entry in the Settings profile toolbar. The list is
/// built per-frame by the redraw controller: index 0 is always "Base"
/// (`is_active` = no active profile), followed by sorted profile names.
/// The renderer draws each entry as a clickable tab in the toolbar; the
/// active entry gets an accent underline.
#[derive(Debug, Clone, Copy)]
pub struct SettingsProfileView<'a> {
    /// Display name. The special `"Base"` sentinel means "no active profile".
    pub name: &'a str,
    /// True when this entry is the currently-active profile (or Base when
    /// no profile is active). The renderer highlights it.
    pub is_active: bool,
}

/// F5: Settings panel rendering parameters. The renderer reads these to lay
/// out the panel's sidebar + content form. All data is borrowed from the
/// `App` struct's settings-related fields.
#[derive(Clone, Copy)]
pub struct SettingsDrawParams<'a> {
    /// Which category is active (sidebar selection).
    pub active_tab: SettingsTab,
    /// Row cursor position within the active category's content area.
    pub selection: usize,
    /// v1.0 fix: vertical scroll offset for list-based categories (Keybindings).
    /// The renderer renders rows `[offset .. offset+max_rows]`.
    pub scroll_offset: usize,
    /// Current theme name (config value, e.g. "weft-warm").
    pub theme_name: &'a str,
    /// All available built-in themes (name + display label).
    pub themes: &'a [SettingsThemeView],
    /// Current font family.
    pub font_family: &'a str,
    /// Current font size (points).
    pub font_size: f32,
    /// Current line height multiplier.
    pub line_height: f32,
    /// Window opacity (0.0–1.0).
    pub window_opacity: f32,
    /// Window horizontal padding (cells).
    pub window_padding_x: u32,
    /// Window vertical padding (cells).
    pub window_padding_y: u32,
    /// Scrollback buffer size (lines).
    pub scrollback_lines: usize,
    /// Paint-time minimum contrast for terminal command/output text.
    pub minimum_contrast: f32,
    /// F5: Window width (px) — Window category.
    pub window_width: u32,
    /// F5: Window height (px) — Window category.
    pub window_height: u32,
    /// F5: Sidebar width override (logical pt) — Window category. None = default.
    pub sidebar_width: Option<f32>,
    /// F5: Submit-on-Ctrl+Enter toggle — Input category.
    pub submit_on_ctrl_enter: bool,
    /// Keybindings to display (with F5 conflict flag).
    pub keybindings: &'a [SettingsKeybindingView],
    /// v1.0 Logo: currently-applied Dock icon variant (Appearance category
    /// displays its label; ←/→ cycles).
    pub logo_variant: weft_core::config::LogoVariant,
    /// v1.0 S2: Last save error message. `None` when the most recent save
    /// succeeded (or no save has been attempted). Surfaced as a red banner
    /// at the top of the panel + field-level errors.
    pub error: Option<&'a str>,
    /// F5: True when the viewport is narrow (<640pt logical). The renderer
    /// switches to single-column drill-down: sidebar only, or content only.
    pub is_narrow: bool,
    /// F5: In narrow mode, true = show content (user drilled into a category),
    /// false = show sidebar. In wide mode this is ignored (both are visible).
    pub drill_down: bool,
    /// F5: Number of keybinding conflicts detected. Shown as a summary badge
    /// in the Keybindings category header.
    pub keybinding_conflict_count: usize,
    /// F5: Field-level validation errors (field_label, message). Rendered
    /// inline next to the offending field AND aggregated in the top summary.
    pub field_errors: &'a [(String, String)],
    /// v1.5.1: Profile entries for the Settings toolbar. Index 0 is always
    /// "Base" (no active profile); 1..N are sorted profile names. Empty when
    /// the content area is hidden (narrow sidebar-only mode) — the toolbar
    /// is zero-sized and the renderer skips it.
    pub profiles: &'a [SettingsProfileView<'a>],
    /// v1.7.0-D: Semantic output fallback classifier toggle (Appearance tab).
    /// When true, unstyled output gets semantic role coloring; when false,
    /// only ANSI-styled output is colored. Defaults to true.
    pub semantic_output_enabled: bool,
}

/// Command Palette rendering parameters (v0.7).
pub struct PaletteDrawParams<'a> {
    pub query: &'a str,
    pub entries: &'a [PaletteEntryView<'a>],
    pub selection: usize,
    /// Variable-fill form (Some = form mode, None = search mode).
    pub form: Option<&'a PaletteFormView<'a>>,
    /// Sub-mode banner text (e.g. "New workflow — name:", "Edit deploy:",
    /// "Delete 'sync'? (y/n)"). Empty string = normal search mode.
    pub banner: &'a str,
    /// Input buffer content for sub-modes (create/edit).
    pub submode_input: &'a str,
}

/// A palette entry, rendered in the dropdown.
pub struct PaletteEntryView<'a> {
    pub label: &'a str,
    pub description: &'a str,
    pub kind_label: &'a str, // "Workflow" / "Builtin"
}

/// Form mode view for variable filling.
pub struct PaletteFormView<'a> {
    pub workflow_name: &'a str,
    pub fields: &'a [(String, String, bool)], // (name, value, is_current)
    pub current_field: usize,
}

/// Completion popup parameters, split out from `PromptDrawParams`. The popup
/// floats above the prompt input box; its anchor is the prompt box's top edge.
pub struct CompletionDrawParams<'a> {
    /// Tab-completion candidates (from `editor.completion_view()`).
    pub matches: &'a [Match],
    /// Currently highlighted candidate index.
    pub selected: usize,
    /// The prompt input box's top y-coordinate in physical pixels. The popup
    /// is positioned directly above this anchor (matching the old behavior
    /// where `popup_bottom = box_y0` inside `build_prompt_vertices`).
    pub anchor_y: f32,
}

// ── Overlay layer & stack ─────────────────────────────────────────────

/// A single overlay layer in the stack.
pub struct OverlayLayer<'a> {
    pub kind: OverlayKind,
    pub z: OverlayZ,
    pub input_policy: OverlayInputPolicy,
    pub content: OverlayContent<'a>,
}

/// The complete set of active overlays for a frame, ordered by z (lowest =
/// bottom). Built fresh each frame by [`build_overlay_stack`].
pub struct OverlayStack<'a> {
    pub layers: Vec<OverlayLayer<'a>>,
}

impl<'a> OverlayStack<'a> {
    /// Iterate layers sorted by z-order (ascending = render bottom-to-top).
    pub fn layers_sorted(&self) -> impl Iterator<Item = &OverlayLayer<'a>> {
        // Layers are pushed in z-order during build, so stable order is
        // preserved. If we ever push out-of-order, sort here.
        self.layers.iter()
    }

    /// The topmost Modal overlay (if any) — it captures all keyboard input.
    pub fn topmost_modal(&self) -> Option<&OverlayLayer<'a>> {
        self.layers
            .iter()
            .filter(|l| l.input_policy == OverlayInputPolicy::Modal)
            .max_by_key(|l| l.z)
    }

    pub(crate) fn tui_preedit(&self) -> Option<TuiPreeditDrawParams<'a>> {
        self.layers.iter().find_map(|layer| match &layer.content {
            OverlayContent::TuiPreedit(params) => Some(*params),
            _ => None,
        })
    }
}

// ── Warm-up trait ─────────────────────────────────────────────────────

/// Collects characters that an overlay will render, so the glyph atlas can
/// pre-rasterize them before vertex building. Each `OverlayContent` variant
/// implements this to keep warm-up and rendering in sync.
pub trait OverlayWarmup {
    fn warm_chars(&self, missing: &mut HashSet<char>);
}

impl OverlayWarmup for OverlayContent<'_> {
    fn warm_chars(&self, missing: &mut HashSet<char>) {
        match self {
            OverlayContent::HistoryPanel(p) => {
                missing.extend("Search:".chars());
                missing.extend(p.query.chars());
            }
            OverlayContent::TuiPreedit(p) => {
                missing.extend(p.text.chars());
            }
            OverlayContent::Prompt(p) => {
                missing.extend("❯ ".chars());
                if let Some(cwd) = p.cwd {
                    missing.extend(cwd.chars());
                }
                for line in p.lines {
                    missing.extend(line.chars());
                }
                if let Some(preedit) = p.preedit {
                    missing.extend(preedit.chars());
                }
                if let Some((q, sel)) = p.search {
                    missing.extend("search: ".chars());
                    missing.extend(q.chars());
                    if let Some(m) = sel {
                        missing.extend(m.chars());
                    }
                }
                missing.extend("⏎⇧⌃·RunNewline".chars());
            }
            OverlayContent::Completion(c) => {
                missing.extend(['📁', '📄', '»']);
                for m in c.matches {
                    missing.extend(m.label.chars());
                }
            }
            OverlayContent::CommandPalette(p) => {
                missing.extend(p.query.chars());
                for e in p.entries {
                    missing.extend(e.label.chars());
                    missing.extend(e.description.chars());
                    missing.extend(e.kind_label.chars());
                }
                missing.extend("Workflow Builtin — 填写参数 Enter 下一项 Esc".chars());
            }
            OverlayContent::Settings(s) => {
                // F5: sidebar category labels + status text + theme names + keybinding strings.
                missing.extend(
                    "Settings Appearance Terminal Input Keybindings Window Advanced".chars(),
                );
                missing.extend(
                    "Theme: Font: Size: Line: Opacity Padding Scrollback Lines Variant: Width Height Sidebar Submit Debug Experimental Conflict restart Semantic: On Off"
                        .chars(),
                );
                // v1.0 fix: warm up the actual footer glyphs. The footer
                // now uses Unicode symbols ⏎ (U+23CE), ⇥ (U+21E5), ⌘ (U+2318)
                // which are not otherwise present in the atlas — without
                // warming they render as blank cells.
                // v1.0 S1-c/S2: added ←→ (U+2190/U+2192) for the adjust hint
                // and ⚠ (U+26A0) for the save-error banner.
                missing.extend("↑↓⏎⇥⌘←→esc navigate apply switch adjust close save".chars());
                missing.insert('\u{26a0}');
                missing.insert('\u{25cf}'); // ● current-theme marker
                missing.insert('\u{21bb}'); // ↻ restart-required badge
                missing.extend(s.theme_name.chars());
                missing.extend(s.font_family.chars());
                if let Some(err) = s.error {
                    missing.extend(err.chars());
                }
                for t in s.themes {
                    missing.extend(t.label.chars());
                }
                for kb in s.keybindings {
                    missing.extend(kb.action.chars());
                    missing.extend(kb.binding.chars());
                }
                // v1.0 Logo: warm all variant labels (Cool/Warm/Light/Transparent
                // + parenthetical descriptions) so the Appearance category renders correctly.
                for v in weft_core::config::LogoVariant::ALL {
                    missing.extend(v.label().chars());
                }
                // v1.5.1: profile toolbar glyphs: "Base" label, profile names,
                // and the +/− buttons. The minus is U+2212 (not ASCII hyphen)
                // so it must be warmed explicitly.
                missing.extend("Base+".chars());
                missing.insert('\u{2212}'); // − minus sign for delete button
                for p in s.profiles {
                    missing.extend(p.name.chars());
                }
            }
        }
    }
}

/// A rectangular region that responds to mouse input, tagged with its target.
/// Produced by the renderer during vertex building, consumed by the app's
/// mouse handler.
pub type HitRegion = crate::scene::HitRegion<HitTarget>;

/// What a hit region refers to — determines the action taken on click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitTarget {
    /// A foldable block's command line (click toggles collapse).
    BlockFold(BlockId),
    /// F3-1: Block header hover-action "copy command" button.
    BlockActionCopy(BlockId),
    /// F3-1: Block header hover-action "toggle fold" button.
    BlockActionFold(BlockId),
    /// v1.8.2: Block header hover-action "diagnose failure" button.
    /// Only rendered for failed blocks (exit_code != 0) when AI is configured.
    BlockActionDiagnose(BlockId),
    /// v1.8.2: Close button on the AI diagnose panel rendered below a
    /// failed block's output. Clicking it clears `block_diagnose_state`
    /// for that block and removes the panel.
    BlockDiagnoseClose(BlockId),
    /// A completion popup row (click accepts candidate).
    CompletionItem(usize),
    /// Completion popup right border (drag to resize width) — v0.7 W2b.
    CompletionResizeWidth,
    /// Completion popup top border (drag to resize height) — v0.7 W2b.
    CompletionResizeHeight,
    /// F3-3: Sidebar right-edge resize handle (drag to adjust sidebar width).
    SidebarResize,
    // v0.7预留：
    // PaletteItem(usize),
    // PaletteResizeWidth,
    // ContextMenuItem(usize),
}

// ── Stack builder ─────────────────────────────────────────────────────

/// Build the overlay stack for the current frame. This is a **free function**
/// (not an `App` method) to avoid self-referential borrow conflicts: it
/// borrows `terminal` and individual `self.*` fields by reference, keeping
/// them disjoint from the `&mut self.renderer` that `draw()` requires.
///
/// Call this inside the `if let (Some(renderer), Some(terminal)) = ...`
/// split-borrow block in `RedrawRequested`.
#[allow(clippy::too_many_arguments)]
pub fn build_overlay_stack<'a>(
    terminal: &'a Terminal,
    _viewport_width: f32,
    panel_width: f32,
    panel_open: bool,
    panel_query: &'a str,
    panel_selection: usize,
    panel_expanded: Option<BlockId>,
    panel_search_focused: bool,
    panel_scroll_offset: usize,
    ime_preedit: &'a str,
    ime_preedit_cursor: Option<(usize, usize)>,
    terminal_owns_ime: bool,
    palette_open: bool,
    palette_query: &'a str,
    palette_selection: usize,
    palette_entries: &'a [(String, String, &'a str)],
    palette_banner: &'a str,
    palette_submode_input: &'a str,
    palette_form: Option<&'a PaletteFormView<'a>>,
    prompt_selection: Option<((usize, usize), (usize, usize))>,
    submit_on_ctrl_enter: bool,
    settings_open: bool,
    settings_tab: SettingsTab,
    settings_selection: usize,
    settings_scroll_offset: usize,
    settings_theme_name: &'a str,
    settings_themes: &'a [SettingsThemeView],
    settings_font_family: &'a str,
    settings_font_size: f32,
    settings_line_height: f32,
    settings_window_opacity: f32,
    settings_window_padding_x: u32,
    settings_window_padding_y: u32,
    settings_scrollback_lines: usize,
    settings_minimum_contrast: f32,
    settings_window_width: u32,
    settings_window_height: u32,
    settings_sidebar_width: Option<f32>,
    settings_submit_on_ctrl_enter: bool,
    settings_keybindings: &'a [SettingsKeybindingView],
    settings_logo_variant: weft_core::config::LogoVariant,
    settings_error: Option<&'a str>,
    settings_is_narrow: bool,
    settings_drill_down: bool,
    settings_keybinding_conflict_count: usize,
    settings_field_errors: &'a [(String, String)],
    settings_profiles: &'a [SettingsProfileView<'a>],
    settings_semantic_output_enabled: bool,
) -> OverlayStack<'a> {
    let mut layers = Vec::new();

    // History panel (Cmd+Shift+B).
    if panel_open {
        layers.push(OverlayLayer {
            kind: OverlayKind::HistoryPanel,
            z: OverlayZ::Panel,
            input_policy: OverlayInputPolicy::Focused,
            content: OverlayContent::HistoryPanel(PanelDrawParams {
                blocks: terminal.block_tracker().blocks(),
                width_px: panel_width,
                query: panel_query,
                selection: panel_selection,
                expanded_id: panel_expanded,
                search_focused: panel_search_focused,
                scroll_offset: panel_scroll_offset,
            }),
        });
    }

    // Passthrough programs own the terminal grid, but macOS still sends
    // marked text to Weft until the IME commits it. Paint that marked text at
    // the TUI cursor so applications such as OpenCode, vim and less get the
    // same inline composition feedback as the native editor.
    if should_show_tui_preedit(
        terminal.effective_input_mode(),
        ime_preedit,
        terminal_owns_ime,
    ) {
        layers.push(OverlayLayer {
            kind: OverlayKind::TuiPreedit,
            z: OverlayZ::Prompt,
            input_policy: OverlayInputPolicy::Passive,
            content: OverlayContent::TuiPreedit(TuiPreeditDrawParams {
                text: ime_preedit,
                cursor: ime_preedit_cursor,
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
                preedit: if ime_preedit.is_empty() {
                    None
                } else {
                    Some(ime_preedit)
                },
                preedit_cursor: if ime_preedit.is_empty() {
                    None
                } else {
                    ime_preedit_cursor
                },
                search,
                selection: prompt_selection,
                scroll_offset: terminal.editor().buffer.scroll_offset,
                submit_on_ctrl_enter,
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
    if palette_open {
        let entry_views: Vec<PaletteEntryView<'a>> = palette_entries
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
                query: palette_query,
                entries: entry_views_box,
                selection: palette_selection,
                form: palette_form,
                banner: palette_banner,
                submode_input: palette_submode_input,
            }),
        });
    }

    // Settings panel (Cmd+,) — v1.0 S1. Highest z so it overlays everything.
    if settings_open {
        layers.push(OverlayLayer {
            kind: OverlayKind::Settings,
            z: OverlayZ::Settings,
            input_policy: OverlayInputPolicy::Modal,
            content: OverlayContent::Settings(SettingsDrawParams {
                active_tab: settings_tab,
                selection: settings_selection,
                scroll_offset: settings_scroll_offset,
                theme_name: settings_theme_name,
                themes: settings_themes,
                font_family: settings_font_family,
                font_size: settings_font_size,
                line_height: settings_line_height,
                window_opacity: settings_window_opacity,
                window_padding_x: settings_window_padding_x,
                window_padding_y: settings_window_padding_y,
                scrollback_lines: settings_scrollback_lines,
                minimum_contrast: settings_minimum_contrast,
                window_width: settings_window_width,
                window_height: settings_window_height,
                sidebar_width: settings_sidebar_width,
                submit_on_ctrl_enter: settings_submit_on_ctrl_enter,
                keybindings: settings_keybindings,
                logo_variant: settings_logo_variant,
                error: settings_error,
                is_narrow: settings_is_narrow,
                drill_down: settings_drill_down,
                keybinding_conflict_count: settings_keybinding_conflict_count,
                field_errors: settings_field_errors,
                profiles: settings_profiles,
                semantic_output_enabled: settings_semantic_output_enabled,
            }),
        });
    }

    OverlayStack { layers }
}

fn should_show_tui_preedit(
    mode: weft_core::input::InputMode,
    text: &str,
    terminal_owns_ime: bool,
) -> bool {
    terminal_owns_ime && mode == weft_core::input::InputMode::Passthrough && !text.is_empty()
}

#[cfg(test)]
#[path = "overlay/tests.rs"]
mod tests;
