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

fn clear_block(id: u64) -> Block {
    Block {
        id: BlockId(id),
        command: "clear".to_owned(),
        cwd: None,
        output: "".into(),
        styled_output: None,
        exit_code: Some(0),
        started_at: std::time::SystemTime::UNIX_EPOCH,
        finished_at: Some(std::time::SystemTime::UNIX_EPOCH),
        collapsed: false,
    }
}

/// R2-2 fix regression test: a bare `clear` block emits a viewport-sized
/// spacer (viewport_rows * pitch) that the prefix sum does NOT include
/// (viewport_rows is per-frame). Before the fix, the binary search + fast
/// forward underestimated every block above the clear, so after scrolling
/// past it the older block's rows were culled as invisible and its output
/// vanished.
#[test]
fn scrolling_past_clear_keeps_older_output_visible() {
    // 布局(新→旧):new_blk(有输出), clear, old_blk(有输出)
    let old_blk = finished_block(1, "old output line\n", false);
    let clear_blk = clear_block(2);
    let new_blk = finished_block(3, "new output line\n", false);
    let blocks = [old_blk, clear_blk, new_blk]; // blocks[0]=最旧

    let mut cache = BlockLayoutCache::default();
    for b in &blocks {
        cache.ensure_cached(b, 80);
    }
    cache.build_prefix_sum(&blocks);

    // 滚动越过 clear(spacer = viewport_rows=24 行=480px),让 old_blk 进入视口。
    // block_scroll=30 → 600px;old_blk 位于 cursor_dist [648,752] → 屏幕 y [752,648]。
    let empty_map = std::collections::HashMap::new();
    let layout = compute_block_layout_pass(
        LayoutPassInput {
            blocks: &blocks,
            live: None,
            pane_session_id: 1,
            cwd: None,
            git_branch: None,
            block_scroll: 30.0, // 滚动量 > clear spacer,触发快进
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
        &cache,
        &mut LiveLayoutCache::default(),
    );

    // 断言:old_blk 的 Output 行存在(bug 时会被误判不可见而缺失)
    let has_old_output = layout
        .row_data
        .iter()
        .any(|r| matches!(r, LaidRow::Output { text, .. } if *text == "old output line"));
    assert!(
        has_old_output,
        "old block output vanished after scrolling past clear"
    );
}

fn completed_layout<'a>(blocks: &'a [Block], cache: &BlockLayoutCache) -> LayoutPassOutput<'a> {
    completed_layout_cols(blocks, cache, 80)
}

fn completed_layout_cols<'a>(
    blocks: &'a [Block],
    cache: &BlockLayoutCache,
    cols: usize,
) -> LayoutPassOutput<'a> {
    let empty_map = std::collections::HashMap::new();
    compute_block_layout_pass(
        LayoutPassInput {
            blocks,
            live: None,
            pane_session_id: 1,
            cwd: None,
            git_branch: None,
            block_scroll: 0.0,
            viewport_rows: 24,
            cols,
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
        &mut LiveLayoutCache::default(),
    )
}

/// R2-2 (stage 2): long commands wrap into multiple rows. The wrapped row
/// count must flow into `command_wrap_rows` (cache), `base_row_count`
/// (prefix sum) AND the layout pass rows — all from the same
/// `command_line_chunks` call with the same args, so scroll/height
/// accounting can't drift between the two paths.
#[test]
fn long_command_wraps_into_multiple_layout_rows() {
    let mut block = finished_block(1, "result\n", false);
    block.command = "cargo build --release --workspace --verbose".to_owned();
    let mut cache = BlockLayoutCache::default();
    cache.ensure_cached(&block, 20); // cols=20 → first line 17 cols (foldable)
    cache.build_prefix_sum(std::slice::from_ref(&block));

    let cached = cache.get(block.id.0);
    let wrap_rows = cached.command_wrap_rows;
    assert!(wrap_rows > 1, "43-char command at cols=20 must wrap");

    // base_row_count = content(1) + gap(1) + wrapped command + separator(1)
    assert_eq!(cached.base_row_count, 3 + wrap_rows);
    assert_eq!(cache.prefix_sum(), &[0, 3 + wrap_rows]);

    // Layout pass must agree: command band height = wrap_rows * pitch.
    let layout = completed_layout_cols(std::slice::from_ref(&block), &cache, 20);
    let cmd_idx = layout
        .row_data
        .iter()
        .position(|r| matches!(r, LaidRow::Command { .. }))
        .expect("command row present");
    assert_eq!(
        layout.rows[cmd_idx] - layout.rows[cmd_idx - 1],
        wrap_rows as f32 * 20.0
    );
    let cmd_chunks = match &layout.row_data[cmd_idx] {
        LaidRow::Command { chunks, .. } => chunks.clone(),
        _ => unreachable!(),
    };
    assert_eq!(cmd_chunks.len(), wrap_rows);
    // Chunks rejoin to the prompt-stripped command (no text loss).
    assert_eq!(cmd_chunks.concat(), strip_prompt_prefix(&block.command));
}

/// Collapsed blocks keep the command on a single row regardless of length.
#[test]
fn collapsed_long_command_stays_single_line() {
    let mut block = finished_block(1, "result\n", true);
    block.command = "cargo build --release --workspace --verbose".to_owned();
    let mut cache = BlockLayoutCache::default();
    cache.ensure_cached(&block, 20);
    cache.build_prefix_sum(std::slice::from_ref(&block));
    assert_eq!(cache.get(block.id.0).command_wrap_rows, 1);
    // collapsed: content 0 + gap 0 + command 1 + separator 1
    assert_eq!(cache.get(block.id.0).base_row_count, 2);
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
                version: 1,
            }),
            pane_session_id: 1,
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
        &mut LiveLayoutCache::default(),
    )
}

/// v1.10.23 (FIX_LIVE_BLOCK_SCROLL_PERF): the live branch materializes
/// only the visible logical-line window (± overscan) instead of all
/// `MAX_LAYOUT_LINES_LIVE` lines, while the fixed structural rows keep
/// their full-layout y-positions.
#[test]
fn live_output_culls_offscreen_lines_but_keeps_positions() {
    let output = (0..20)
        .map(|line| format!("line-{line}\n"))
        .collect::<String>();
    let mk = |scroll: f32| {
        let live = InFlightBlock {
            command: "long-running-command",
            cwd: Some("/tmp"),
            output: &output,
            styled_output: None,
            version: 1,
        };
        compute_block_layout_pass(
            LayoutPassInput {
                blocks: &[],
                live: Some(live),
                pane_session_id: 1,
                cwd: None,
                git_branch: None,
                block_scroll: scroll,
                viewport_rows: 6,
                cols: 80,
                pitch: 20.0,
                header_height: 24.0,
                content_bottom_y: 120.0,
                clip_top: 0.0,
                clip_bottom: 120.0,
                resolve_styles: false,
                styled_lookup_counter: None,
                block_diagnose_state: &std::collections::HashMap::new(),
            },
            &BlockLayoutCache::default(),
            &mut LiveLayoutCache::default(),
        )
    };

    // Following at the bottom: visible band [-64, 184]px → lines 6..19
    // (14 output rows, overscan = ceil(64/20)+1 = 5) + Blank +
    // LiveCommand + LiveHeader + Separator = 18 rows.
    let out = mk(0.0);
    assert_eq!(out.row_data.len(), 18, "culled live rows + 4 structural");
    assert!(matches!(&out.row_data[0], LaidRow::Output { text, .. } if *text == "line-19"));
    assert!(matches!(out.row_data[14], LaidRow::Blank));
    assert!(matches!(out.row_data[17], LaidRow::Separator));
    // Blank row keeps its FULL-layout position: total(400px) + pitch.
    assert_eq!(out.rows[14], 420.0);
    // Culled lines 0..5 are absent; emitted line indices carry base_idx.
    assert!(!out
        .row_data
        .iter()
        .any(|r| matches!(r, LaidRow::Output { line, .. } if *line < 6)));

    // Scrolled 5 rows up: band [36, 284]px → lines 1..19 (19 rows).
    let out = mk(5.0);
    assert_eq!(out.row_data.len(), 23, "culled live rows + 4 structural");
    let oldest = out
        .row_data
        .iter()
        .position(|r| matches!(r, LaidRow::Output { text, .. } if *text == "line-1"))
        .expect("oldest culled-in line");
    // Its top keeps the FULL-layout position: dist = (20-1)*20 = 380.
    assert_eq!(out.rows[oldest], 380.0);
    assert!(!out
        .row_data
        .iter()
        .any(|r| matches!(r, LaidRow::Output { line, .. } if *line < 1)));
}
