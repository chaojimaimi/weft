use super::*;

fn finished_block(id: u64, output: &str, collapsed: bool) -> Block {
    Block {
        id: BlockId(id),
        command: "printf result".to_owned(),
        cwd: None,
        output: output.into(),
        styled_output: None,
        exit_code: Some(0),
        started_at: std::time::SystemTime::UNIX_EPOCH,
        finished_at: Some(std::time::SystemTime::UNIX_EPOCH),
        collapsed,
    }
}

fn completed_layout<'a>(blocks: &'a [Block], cache: &BlockLayoutCache) -> LayoutPassOutput<'a> {
    let empty_map = std::collections::HashMap::new();
    compute_block_layout_pass(
        LayoutPassInput {
            blocks,
            live: None,
            cwd: None,
            git_branch: None,
            block_scroll: 0.0,
            viewport_rows: 24,
            cols: 80,
            pitch: 20.0,
            header_height: 24.0,
            content_bottom_y: 800.0,
            clip_top: 0.0,
            clip_bottom: 800.0,
            resolve_styles: false,
            styled_lookup_counter: None,
            block_diagnose_state: &empty_map,
        },
        cache,
    )
}

#[test]
fn command_output_gap_is_a_shared_structural_row_for_live_and_completed_blocks() {
    let with_output = finished_block(1, "result\n", false);
    let mut cache = BlockLayoutCache::default();
    cache.ensure_cached(&with_output, 80);
    cache.build_prefix_sum(std::slice::from_ref(&with_output));
    let completed = completed_layout(std::slice::from_ref(&with_output), &cache);
    assert!(matches!(completed.row_data[0], LaidRow::Output { .. }));
    assert!(matches!(completed.row_data[1], LaidRow::Blank));
    assert!(matches!(completed.row_data[2], LaidRow::Command { .. }));
    assert_eq!(completed.rows[1] - completed.rows[0], 20.0);
    assert_eq!(completed.rows[2] - completed.rows[1], 20.0);

    for block in [
        finished_block(2, "", false),
        finished_block(3, "result\n", true),
    ] {
        let mut cache = BlockLayoutCache::default();
        cache.ensure_cached(&block, 80);
        cache.build_prefix_sum(std::slice::from_ref(&block));
        let layout = completed_layout(std::slice::from_ref(&block), &cache);
        assert!(!layout
            .row_data
            .iter()
            .any(|row| matches!(row, LaidRow::Blank)));
    }

    let live = live_layout("printf result", "result\n");
    assert!(matches!(live.row_data[0], LaidRow::Output { .. }));
    assert!(matches!(live.row_data[1], LaidRow::Blank));
    assert!(matches!(live.row_data[2], LaidRow::LiveCommand { .. }));

    let empty_live = live_layout("true", "");
    assert!(!empty_live
        .row_data
        .iter()
        .any(|row| matches!(row, LaidRow::Blank)));
}

fn live_layout<'a>(command: &'a str, output: &'a str) -> LayoutPassOutput<'a> {
    let empty_map = std::collections::HashMap::new();
    compute_block_layout_pass(
        LayoutPassInput {
            blocks: &[],
            live: Some(InFlightBlock {
                command,
                cwd: None,
                output,
                styled_output: None,
            }),
            cwd: None,
            git_branch: None,
            block_scroll: 0.0,
            viewport_rows: 24,
            cols: 80,
            pitch: 20.0,
            header_height: 24.0,
            content_bottom_y: 800.0,
            clip_top: 0.0,
            clip_bottom: 800.0,
            resolve_styles: false,
            styled_lookup_counter: None,
            block_diagnose_state: &empty_map,
        },
        &BlockLayoutCache::default(),
    )
}
