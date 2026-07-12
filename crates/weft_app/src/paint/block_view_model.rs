//! Immutable per-frame inputs for BlockView painting.

use weft_core::blocks::{Block, InFlightBlock};

pub(crate) struct BlockViewPaintModel<'a> {
    pub(crate) blocks: &'a [Block],
    pub(crate) region_bottom_y: f32,
    pub(crate) cwd: Option<&'a str>,
    pub(crate) git_branch: Option<&'a str>,
    pub(crate) live: Option<InFlightBlock<'a>>,
    pub(crate) block_scroll: usize,
}
