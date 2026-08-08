use super::Tab;
use crate::effect::PendingPaneResize;
use weft_core::vt::Terminal;

impl Tab {
    /// Read every pane's pending PTY resize and synchronized-frame state.
    pub(crate) fn pending_pane_resizes(&self) -> Vec<PendingPaneResize> {
        let mut out = Vec::new();
        for (id, pane) in &self.panes {
            if let Some(dim) = pane.pending_pty_resize {
                let synchronized = pane
                    .terminal
                    .as_ref()
                    .is_some_and(Terminal::synchronized_output);
                out.push(PendingPaneResize::new(*id, dim, synchronized));
            }
        }
        out
    }

    pub(crate) fn any_synchronized_output(&self) -> bool {
        self.panes.values().any(|pane| {
            pane.terminal
                .as_ref()
                .is_some_and(Terminal::synchronized_output)
        })
    }
}
