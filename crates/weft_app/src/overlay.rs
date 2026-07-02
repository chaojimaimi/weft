//! Unified overlay stack — manages z-order, rendering, warm-up, and hit-testing
//! for all overlay UI layers (history panel, prompt input box, completion
//! dropdown, and future Command Palette / context menu).
//!
//! ## Design
//!
//! Each overlay is an [`OverlayLayer`] with a z-order ([`OverlayZ`]), an input
//! policy ([`OverlayInputPolicy`]), and content ([`OverlayContent`]). The
//! [`OverlayStack`] is a **stack-local temporary** built per frame in the
//! `RedrawRequested` handler via [`build_overlay_stack`] — it is NOT stored as
//! an `App` field, to avoid self-referential borrow conflicts with
//! `&mut self.renderer.draw()`.
//!
//! See `docs/superpowers/App:Renderer Overlay 架构重构设计.md` for the full
//! design rationale and `docs/superpowers/specs/2026-07-02-overlay-refactor.md`
//! (planned).

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
    // v0.7预留：CommandPalette,
    // v0.7预留：ContextMenu,
}

/// The renderable content of an overlay layer. Each variant wraps the
/// existing `*DrawParams` struct, so the refactor doesn't change what data
/// flows into the renderer — only how it's packaged.
pub enum OverlayContent<'a> {
    HistoryPanel(PanelDrawParams<'a>),
    Prompt(PromptDrawParams<'a>),
    Completion(CompletionDrawParams<'a>),
    // v0.7预留：
    // CommandPalette(PaletteDrawParams<'a>),
    // ContextMenu(ContextMenuDrawParams<'a>),
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
        }
    }
}

/// Build the overlay stack for the current frame. This is a **free function**

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
    viewport_width: f32,
    renderer_scale: f64,
    cell_height: u32,
    padding_y: f32,
    panel_open: bool,
    panel_query: &'a str,
    panel_selection: usize,
    panel_expanded: Option<BlockId>,
    ime_preedit: &'a str,
) -> OverlayStack<'a> {
    let mut layers = Vec::new();

    // History panel (Cmd+Shift+B).
    if panel_open {
        let width_px = (viewport_width * 0.38).min(460.0 * renderer_scale as f32);
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
            }),
        });
    }

    // Prompt input box (Editor mode only).
    if terminal.effective_input_mode() == weft_core::input::InputMode::Editor {
        let search = terminal.editor().search_view();
        let completions = terminal.editor().completion_view();

        // Compute prompt box geometry (shared between prompt and completion
        // anchoring). This replaces the duplicated box_h calculation that
        // previously existed in both draw() and build_prompt_vertices().
        // Commit 2 will use this to anchor the split-out completion layer.
        let _ = (cell_height, padding_y);

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
                // Commit 1: pass completions through to prompt for backward
                // compat. Commit 2 will split this into a separate layer.
                completions,
            }),
        });

        // Commit 2 will add: if completions.is_some() → push Completion layer.
    }

    // v0.7预留：
    // if palette_open { layers.push(OverlayLayer { ... CommandPalette ... }); }

    OverlayStack { layers }
}
