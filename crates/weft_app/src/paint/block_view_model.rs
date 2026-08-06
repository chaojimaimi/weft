//! Immutable per-frame inputs for BlockView painting.

use std::collections::HashMap;

use weft_core::blocks::{Block, BlockId, InFlightBlock};
use weft_core::grid::Color;

use crate::app_state::BlockDiagnoseState;

pub(crate) struct BlockViewPaintModel<'a> {
    pub(crate) blocks: &'a [Block],
    pub(crate) region_bottom_y: f32,
    pub(crate) cwd: Option<&'a str>,
    pub(crate) git_branch: Option<&'a str>,
    pub(crate) live: Option<InFlightBlock<'a>>,
    pub(crate) block_scroll: f32,
    /// Terminal rows represented by a completed `clear` command's blank band.
    pub(crate) viewport_rows: usize,
    /// F3-1: Block currently hovered by the mouse. The header row for this
    /// block renders inline copy/fold action buttons.
    pub(crate) block_hovered: Option<BlockId>,
    pub(crate) block_selected: Option<BlockId>,
    pub(crate) block_action_hovered: Option<crate::block_component::BlockHeaderAction>,
    /// F3-2: Normalized phase (0..1) for the running-command spinner.
    /// Advances ~once per 80ms; the renderer maps it to a braille spinner
    /// glyph. `-1.0` disables the spinner (reduce-motion or not running).
    pub(crate) spinner_phase: f32,
    /// Active-pane find highlight. Background panes always pass `None`.
    pub(crate) find_block_highlight: Option<(u64, usize, bool, usize, usize)>,
    pub(crate) palette: &'a [Color; 256],
    /// v1.4.1: pane-scoped namespace for the styled-line vertex cache.
    /// Uses `Pane::pane_session_id` (global, monotonic) so entries from a
    /// closed pane naturally miss without explicit invalidation.
    pub(crate) cache_namespace: u64,
    /// v1.8.2: Per-block AI diagnose state. An entry exists when a diagnose
    /// request is in flight or a result is being displayed. The renderer
    /// injects extra rows below the block's output to show the panel.
    pub(crate) block_diagnose_state: &'a HashMap<BlockId, BlockDiagnoseState>,
    /// v1.8.2: Whether the local Ollama backend is configured. When false,
    /// the diagnose button is not rendered on block headers and the
    /// `Action::DiagnoseBlock` keybinding is a no-op.
    pub(crate) ai_configured: bool,
    /// v1.10.5: BlockView-mode TUI caret — the grid cursor mapped into the
    /// live block's snapshot text `(line, col)`. Primary-screen TUIs kept
    /// in the BlockView (openclaw/pi) input at their own bottom row; the
    /// grid cursor is invisible while the document renders, so the caret
    /// (blinking bar) and IME preedit are drawn directly on the live block
    /// row. `None` for Editor-mode / non-TUI block views.
    pub(crate) tui_cursor: Option<(usize, usize)>,
    /// v1.10.5: active IME preedit for a BlockView-mode TUI, drawn at the
    /// mapped caret. `(text, caret_byte_range)`.
    pub(crate) tui_preedit: Option<(&'a str, Option<(usize, usize)>)>,
    /// v1.10.5: reserved for future blink-driven TUI caret animation.
    /// Currently unused — the BlockView TUI caret is steady-on (the blink
    /// timer only wakes for AtPrompt, so in CommandExecuting this flag is
    /// stale and would hide the caret permanently).
    #[allow(dead_code)]
    pub(crate) cursor_blink_on: bool,
}
