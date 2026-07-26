//! Immutable per-frame inputs for BlockView painting.

use weft_core::blocks::{Block, BlockId, InFlightBlock};
use weft_core::grid::Color;

pub(crate) struct BlockViewPaintModel<'a> {
    pub(crate) blocks: &'a [Block],
    pub(crate) region_bottom_y: f32,
    pub(crate) cwd: Option<&'a str>,
    pub(crate) git_branch: Option<&'a str>,
    pub(crate) live: Option<InFlightBlock<'a>>,
    pub(crate) block_scroll: usize,
    /// Terminal rows represented by a completed `clear` command's blank band.
    pub(crate) viewport_rows: usize,
    /// F3-1: Block currently hovered by the mouse. The header row for this
    /// block renders inline copy/fold action buttons.
    pub(crate) block_hovered: Option<BlockId>,
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
}
