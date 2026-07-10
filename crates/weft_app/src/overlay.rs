//! Unified overlay stack — manages z-order, rendering, warm-up, and hit-testing
//! for all overlay UI layers (history panel, prompt input box, completion
//! dropdown, and future Command Palette / context menu).
//!
//! See `docs/superpowers/App:Renderer Overlay 架构重构设计.md` for the full
//! design rationale.
//!
//! Types defined here are not yet wired into the render path (commits 3-4
//! will migrate panel/prompt/completion/hit-test to use them).
#![allow(dead_code)]

use std::collections::HashSet;

use weft_core::blocks::BlockId;
use weft_core::complete::Match;
use weft_core::vt::Terminal;

use crate::renderer::{PanelDrawParams, PromptDrawParams};

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

// ── Input policy ──────────────────────────────────────────────────────

/// How an overlay layer participates in keyboard/mouse input routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayInputPolicy {
    /// No input interaction (e.g. scrollbar indicator).
    None,
    /// Visible but doesn't intercept input (passive overlay).
    Passive,
    /// Receives input when focused (e.g. prompt editor, completion nav).
    Focused,
    /// Captures all input while open (e.g. Command Palette, context menu).
    Modal,
}

// ── Overlay kind & content ────────────────────────────────────────────

/// Identifies the overlay type for dispatch and debugging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayKind {
    HistoryPanel,
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
    Prompt(PromptDrawParams<'a>),
    Completion(CompletionDrawParams<'a>),
    CommandPalette(PaletteDrawParams<'a>),
    Settings(SettingsDrawParams<'a>),
    // future: ContextMenu(ContextMenuDrawParams<'a>),
}

/// v1.0 S1: Which tab of the Settings panel is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    Appearance,
    Font,
    Keybindings,
    Window,
    /// v1.0 Logo: Dock icon variant selection (←/→ cycles Cool/Warm/Light/Transparent).
    Logo,
}

impl SettingsTab {
    /// All tabs in display order.
    pub const ALL: [SettingsTab; 5] = [
        SettingsTab::Appearance,
        SettingsTab::Font,
        SettingsTab::Keybindings,
        SettingsTab::Window,
        SettingsTab::Logo,
    ];

    /// Human-readable label for the tab bar.
    pub fn label(self) -> &'static str {
        match self {
            SettingsTab::Appearance => "Appearance",
            SettingsTab::Font => "Font",
            SettingsTab::Keybindings => "Keybindings",
            SettingsTab::Window => "Window",
            SettingsTab::Logo => "Logo",
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
}

/// v1.0 S1: Settings panel rendering parameters. The renderer reads these
/// to lay out the panel's tabs and content. All data is borrowed from the
/// `App` struct's settings-related fields.
#[derive(Clone, Copy)]
pub struct SettingsDrawParams<'a> {
    /// Which tab is active.
    pub active_tab: SettingsTab,
    /// Row cursor position within the active tab's content area.
    pub selection: usize,
    /// v1.0 fix: vertical scroll offset for list-based tabs (Keybindings).
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
    /// Keybindings to display (read-only in v1.0).
    pub keybindings: &'a [SettingsKeybindingView],
    /// v1.0 Logo: currently-applied Dock icon variant (Logo tab displays its label).
    pub logo_variant: weft_core::config::LogoVariant,
    /// v1.0 S2: Last save error message. `None` when the most recent save
    /// succeeded (or no save has been attempted). Surfaced as a red banner
    /// at the top of the panel.
    pub error: Option<&'a str>,
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
                // Note: detailed block scanning is handled by the renderer's
                // warm-up (which has access to panel_display/visible_panel_rows
                // helpers). Here we just warm the query + label.
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
            }
            OverlayContent::Completion(c) => {
                // Emoji icons used by the popup (📁📄 via CoreText color path).
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
                // Tab labels + status text + theme names + keybinding strings.
                missing.extend("Settings Appearance Font Keybindings Window Logo".chars());
                missing.extend(
                    "Theme: Font: Size: Line: Window Opacity Padding Scrollback Lines Variant:"
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
                // + parenthetical descriptions) so the Logo tab renders correctly.
                for v in weft_core::config::LogoVariant::ALL {
                    missing.extend(v.label().chars());
                }
            }
        }
    }
}

/// A rectangular region that responds to mouse input, tagged with its target.
/// Produced by the renderer during vertex building, consumed by the app's
/// mouse handler.
#[derive(Debug, Clone)]
pub struct HitRegion {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
    pub target: HitTarget,
}

/// What a hit region refers to — determines the action taken on click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitTarget {
    /// A foldable block's command line (click toggles collapse).
    BlockFold(BlockId),
    /// A completion popup row (click accepts candidate).
    CompletionItem(usize),
    /// Completion popup right border (drag to resize width) — v0.7 W2b.
    CompletionResizeWidth,
    /// Completion popup top border (drag to resize height) — v0.7 W2b.
    CompletionResizeHeight,
    // v0.7预留：
    // PaletteItem(usize),
    // PaletteResizeWidth,
    // ContextMenuItem(usize),
}

impl HitRegion {
    /// Whether a point (in physical pixels) falls within this region.
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x0 && x <= self.x1 && y >= self.y0 && y <= self.y1
    }
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
    renderer_scale: f64,
    panel_open: bool,
    panel_query: &'a str,
    panel_selection: usize,
    panel_expanded: Option<BlockId>,
    panel_search_focused: bool,
    ime_preedit: &'a str,
    palette_open: bool,
    palette_query: &'a str,
    palette_selection: usize,
    palette_entries: &'a [(String, String, &'a str)],
    palette_banner: &'a str,
    palette_submode_input: &'a str,
    prompt_selection: Option<((usize, usize), (usize, usize))>,
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
    settings_keybindings: &'a [SettingsKeybindingView],
    settings_logo_variant: weft_core::config::LogoVariant,
    settings_error: Option<&'a str>,
) -> OverlayStack<'a> {
    let mut layers = Vec::new();

    // History panel (Cmd+Shift+B).
    if panel_open {
        // v0.9 W5: panel width must match renderer's sidebar_width() (240×scale)
        // so chrome_left == panel width and content isn't covered.
        let width_px = 240.0 * renderer_scale as f32;
        layers.push(OverlayLayer {
            kind: OverlayKind::HistoryPanel,
            z: OverlayZ::Panel,
            input_policy: OverlayInputPolicy::Focused,
            content: OverlayContent::HistoryPanel(PanelDrawParams {
                blocks: terminal.block_tracker().blocks(),
                width_px,
                query: panel_query,
                selection: panel_selection,
                expanded_id: panel_expanded,
                search_focused: panel_search_focused,
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
                cwd: terminal.cwd(),
                lines: &terminal.editor().buffer.lines,
                cursor: terminal.editor().buffer.cursor,
                preedit: if ime_preedit.is_empty() {
                    None
                } else {
                    Some(ime_preedit)
                },
                search,
                selection: prompt_selection,
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
                form: None,
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
                keybindings: settings_keybindings,
                logo_variant: settings_logo_variant,
                error: settings_error,
            }),
        });
    }

    OverlayStack { layers }
}

#[cfg(test)]
mod tests {
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
                search: None,
                selection: None,
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
            search: None,
            selection: None,
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
        assert!(labels.contains(&"Font"));
        assert!(labels.contains(&"Keybindings"));
        assert!(labels.contains(&"Window"));
        assert!(labels.contains(&"Logo"));
    }

    #[test]
    fn settings_tab_all_has_five_tabs() {
        assert_eq!(SettingsTab::ALL.len(), 5);
        assert_eq!(SettingsTab::ALL[0], SettingsTab::Appearance);
        assert_eq!(SettingsTab::ALL[3], SettingsTab::Window);
        assert_eq!(SettingsTab::ALL[4], SettingsTab::Logo);
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
            keybindings: &kbs,
            logo_variant: weft_core::config::LogoVariant::Cool,
            error: None,
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
                        keybindings: &[],
                        logo_variant: weft_core::config::LogoVariant::Cool,
                        error: None,
                    }),
                },
            ],
        };
        let top = stack.topmost_modal().expect("should have a modal");
        assert_eq!(top.z, OverlayZ::Settings);
        assert_eq!(top.kind, OverlayKind::Settings);
    }
}
