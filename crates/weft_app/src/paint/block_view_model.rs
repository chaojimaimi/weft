//! Immutable per-frame inputs for BlockView painting.

use weft_core::blocks::{Block, BlockId, InFlightBlock};

pub(crate) struct BlockViewPaintModel<'a> {
    pub(crate) blocks: &'a [Block],
    pub(crate) region_bottom_y: f32,
    pub(crate) cwd: Option<&'a str>,
    pub(crate) git_branch: Option<&'a str>,
    pub(crate) live: Option<InFlightBlock<'a>>,
    pub(crate) block_scroll: usize,
    /// F3-1: Block currently hovered by the mouse. The header row for this
    /// block renders inline copy/fold action buttons.
    pub(crate) block_hovered: Option<BlockId>,
    /// F3-2: Normalized phase (0..1) for the running-command spinner.
    /// Advances ~once per 80ms; the renderer maps it to a braille spinner
    /// glyph. `-1.0` disables the spinner (reduce-motion or not running).
    pub(crate) spinner_phase: f32,
}
