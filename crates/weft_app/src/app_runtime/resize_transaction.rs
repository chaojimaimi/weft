use weft_core::vt::Terminal;

pub(super) fn commit_pty_resize_result(
    terminal: &mut Terminal,
    pending: &mut Option<(usize, usize)>,
    requested: (usize, usize),
    resize_succeeded: bool,
) -> bool {
    if !resize_succeeded {
        return false;
    }
    terminal.resize(requested.0, requested.1);
    if *pending == Some(requested) {
        *pending = None;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pty_resize_result_commits_grid_only_after_success() {
        let mut terminal = Terminal::new(4, 20);
        let mut pending = Some((8, 40));

        assert!(!commit_pty_resize_result(
            &mut terminal,
            &mut pending,
            (8, 40),
            false,
        ));
        assert_eq!(
            (terminal.grid().num_rows, terminal.grid().num_cols),
            (4, 20)
        );
        assert_eq!(pending, Some((8, 40)));

        assert!(commit_pty_resize_result(
            &mut terminal,
            &mut pending,
            (8, 40),
            true,
        ));
        assert_eq!(
            (terminal.grid().num_rows, terminal.grid().num_cols),
            (8, 40)
        );
        assert_eq!(pending, None);
    }

    #[test]
    fn successful_stale_resize_preserves_newer_pending_request() {
        let mut terminal = Terminal::new(4, 20);
        let mut pending = Some((10, 50));

        assert!(commit_pty_resize_result(
            &mut terminal,
            &mut pending,
            (8, 40),
            true,
        ));
        assert_eq!(
            (terminal.grid().num_rows, terminal.grid().num_cols),
            (8, 40)
        );
        assert_eq!(pending, Some((10, 50)));
    }
}
