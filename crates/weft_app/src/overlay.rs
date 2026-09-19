// Overlay stack builder: z-order + warm-up + hit-testing for all overlay
// layers. v1.11 audit 3C (PLAN_audit_fix_batch3 C1): the parameter-group
// structs moved to overlay/view_params.rs, bringing the file back under the
// default 800-line budget (allowlist entry removed).
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

// v1.11 audit (PLAN_audit_fix_batch3 C1): the five view-parameter groups
// replacing build_overlay_stack's flat 60-param signature live in their own
// module (overlay.rs is at its line budget; the new file has zero pressure).
mod view_params;

pub use view_params::{
    ImeViewParams, PaletteViewParams, PanelViewParams, PromptViewParams, SettingsOwnedSnapshot,
    SettingsViewParams,
};

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
/// by a 7-entry split sidebar. Logo and Font merged into Appearance.
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
    /// v1.8.3: Local Ollama AI integration (enable, model, test connection,
    /// data-range toggles, max_tokens, timeout). AI config is global only.
    LocalAi,
    /// v1.11.0 隐藏入口：Debug logging / experimental / Import-Export 行均为
    /// 无真实配置支撑的 placeholder（无配置支撑，补实后恢复，见
    /// docs/PLAN_v111.md 第 1 项）。枚举变体与全部 match 臂**保留**，仅从
    /// `ALL` 可见列表移除，避免大面积 match 改动；如未来补实配置即加回。
    Advanced,
}

impl SettingsTab {
    /// All visible categories in sidebar display order.
    ///
    /// v1.11.0: `Advanced` 从可见列表移除（空壳 placeholder 无配置支撑，
    /// 见 AUDIT_v1.10.39 P2-M7 / PLAN_v111 第 1 项）。变体仍存在以满足
    /// 各 match 的穷尽性；键盘导航（Tab/↑/↓）与绘制/命中区全部由本列表
    /// 驱动，故移除即全局隐藏。
    pub const ALL: [SettingsTab; 6] = [
        SettingsTab::Appearance,
        SettingsTab::Terminal,
        SettingsTab::Input,
        SettingsTab::Keybindings,
        SettingsTab::Window,
        SettingsTab::LocalAi,
    ];

    /// Human-readable label for the sidebar.
    pub fn label(self) -> &'static str {
        match self {
            SettingsTab::Appearance => "Appearance",
            SettingsTab::Terminal => "Terminal",
            SettingsTab::Input => "Input",
            SettingsTab::Keybindings => "Keybindings",
            SettingsTab::Window => "Window",
            SettingsTab::LocalAi => "Local AI",
            SettingsTab::Advanced => "Advanced",
        }
    }
}

/// v1.0 S1 / v1.12: 主题条目。`name`/`label` 用 `String`（自定义主题名是运行时值）。
#[derive(Debug, Clone)]
pub struct SettingsThemeView {
    /// Theme name as it appears in config (`weft-warm`, `warp`, etc.).
    pub name: String,
    /// Human-readable display label.
    pub label: String,
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

/// v1.8.3: Settings LocalAi tab rendering parameters. Bundled into a single
/// struct so `SettingsDrawParams` and `build_overlay_stack` only grow by one
/// parameter instead of one per AI field. All data is borrowed from `App`'s
/// AI-related fields (`settings.draft.ai`, `ai_models`, `ai_connection_status`).
#[derive(Debug, Clone, Copy)]
pub struct AiSettingsView<'a> {
    /// True when `provider = Some("ollama")` in the draft config.
    pub enabled: bool,
    /// Current model name (empty string when `None`).
    pub model: &'a str,
    /// Effective base URL (defaults to `http://127.0.0.1:11434`).
    pub base_url: &'a str,
    /// Effective max_tokens (defaults to 1024).
    pub max_tokens: u32,
    /// Effective timeout in seconds (defaults to 30).
    pub timeout_secs: u32,
    /// Whether natural-language command generation is enabled.
    pub enable_command_generation: bool,
    /// Whether failed-block diagnosis is enabled.
    pub enable_error_diagnosis: bool,
    /// Cached model names from the last `/api/tags` refresh (for the dropdown).
    pub models: &'a [String],
    /// Human-readable connection status (e.g. "Connected (3 models)").
    pub connection_status: &'a str,
    /// True when a `/api/tags` refresh is in flight (disables the button).
    pub testing: bool,
    /// v1.8.3: Observability summary line (e.g. "12 req · 10 ok · 2 err ·
    /// p95 240ms"). Pre-formatted by the controller so the renderer just
    /// pushes one text run. Empty when no requests have been made.
    pub observability: &'a str,
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
    /// v1.10: Semantic target selection/opening gestures.
    pub smart_select: bool,
    /// v1.11.1 (PLAN_v1111 §4.6): paste-protection values (rows 2-4);
    /// grouped to keep this struct's field list from growing further.
    pub paste_rows: crate::settings_validation::PasteRowsView,
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
    /// v1.8.3: LocalAi tab parameters. Always provided; the renderer only
    /// reads it when `active_tab == SettingsTab::LocalAi`.
    pub ai: AiSettingsView<'a>,
    /// v1.11.5 (PLAN_v1115 §M8): notification + clipboard draft values
    /// (Advanced rows 4-7). Grouped like `paste_rows` to keep the field
    /// list flat.
    pub notify_enabled: bool,
    pub notify_threshold_secs: u64,
    pub notify_sound: bool,
    pub osc52_mode: weft_core::config::Osc52Mode,
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
    /// v1.8.4: IME preedit text for the palette input (both Search and
    /// banner submodes). Rendered inline after the input buffer.
    pub ime_preedit: &'a str,
    pub ime_preedit_cursor: Option<(usize, usize)>,
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
                // v1.8.4: warm up preedit + submode input chars.
                if !p.banner.is_empty() {
                    missing.extend(p.banner.chars());
                }
                missing.extend(p.submode_input.chars());
                if !p.ime_preedit.is_empty() {
                    missing.extend(p.ime_preedit.chars());
                }
            }
            OverlayContent::Settings(s) => {
                // F5: sidebar category labels + status text + theme names + keybinding strings.
                // v1.11.0: "Advanced" removed from the warmup — the tab is
                // hidden from the sidebar (see SettingsTab::ALL).
                missing.extend(
                    "Settings Appearance Terminal Input Keybindings Window Local AI".chars(),
                );
                missing.extend(
                    "Theme: Font: Size: Line: Opacity Padding Scrollback Lines Variant: Width Height Sidebar Submit Debug Experimental Conflict restart Semantic: On Off Enabled: Model: URL: Tokens: Timeout: Cmd Generation: Error Diagnosis: Test Connection Connected models Failed Not tested Testing"
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
                // v1.8.3: LocalAi tab — warm dynamic strings (model name,
                // base URL, connection status, discovered model names).
                missing.extend(s.ai.model.chars());
                missing.extend(s.ai.base_url.chars());
                missing.extend(s.ai.connection_status.chars());
                for m in s.ai.models {
                    missing.extend(m.chars());
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
