//! v1.20.0 (PLAN_v1200 §4 D11 / §7) — overlay-chrome S1 suite.
//! Mounted off `overlay.rs` via `#[path]` (atlas_warmup_tests precedent).
//!
//! - P1 pixel parity: under `complete_block_overlay = true` the laid
//!   row-stream drops the gap/Blank, Header, and Separator entries while
//!   every remaining row keeps its exact legacy distance, and the overlay
//!   chrome rows carry those same distances (same-y emission).
//! - Sister domain (D2a): flag ON row counts = hint + output + diagnose +
//!   command-wrap + clear; the deficit vs OFF is exactly gap + header +
//!   separator per block.
//! - Cache keys (D2e): `BlockLayoutCache` prefix sums are flag-invariant
//!   (B2c dual-field + uniform-px argument), `BlockScrollMetricsKey` and
//!   `BgBlockFingerprint` gain a flag bit.
//! - Find/panel chokepoints (D2f), settle compensation (D2d, inline in
//!   tab/scroll.rs), overscan widening (D6), resume-hint guard (D7),
//!   surfaces bounds parity (D5), atlas warm-window identity (D2c/N1).

use super::{OverlayChromeKind, OverlayChromeRow};
use crate::app_state::BlockDiagnoseState;
use crate::block_component::{
    clear_block_spacer_rows, command_wrap_rows_for, completed_block_layout_rows,
    completed_block_match_row_from_bottom, completed_block_output_rows, diagnose_panel_chunks,
};
use crate::paint::block_view::layout_pass::bounds::{lower_bound_height, upper_bound_height};
use crate::paint::block_view::layout_pass::{
    compute_block_layout_pass, LaidRow, LayoutPassInput, LayoutPassOutput,
};
use crate::paint::grid_cache::BlockLayoutCache;
use crate::paint::live_cache::{BlockScrollMetricsKey, LiveLayoutCache};
use crate::renderer::bg_block_cache::{
    bg_block_cache_action, BgBlockCacheAction, BgBlockFingerprint,
};
use crate::renderer::MetalRenderer;
use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime};
use weft_core::blocks::{Block, BlockId};
use weft_core::config::Theme;
use weft_core::find::BlockMatch;

const COLS: usize = 80;
const VIEWPORT: usize = 24;
const PITCH: f32 = 20.0;
const HEADER: f32 = 24.0;
const CONTENT_BOTTOM: f32 = 800.0;
const NOW: SystemTime = SystemTime::UNIX_EPOCH;

fn plain_block(id: u64, output: &str, collapsed: bool) -> Block {
    Block {
        id: BlockId(id),
        command: "printf result".to_owned(),
        cwd: None,
        output: output.into(),
        styled_output: None,
        exit_code: Some(0),
        started_at: NOW,
        finished_at: Some(NOW),
        collapsed,
        screen_origin: false,
    }
}

fn clear_block(id: u64) -> Block {
    Block {
        command: "clear".to_owned(),
        ..plain_block(id, "", false)
    }
}

/// opencode + non-clean exit → the two resume hints render (D7 fixture).
fn opencode_block(id: u64) -> Block {
    Block {
        command: "opencode --resume".to_owned(),
        exit_code: Some(130),
        ..plain_block(id, "buffered\n", false)
    }
}

fn diagnose_map(entries: Vec<(u64, BlockDiagnoseState)>) -> HashMap<BlockId, BlockDiagnoseState> {
    entries
        .into_iter()
        .map(|(id, st)| (BlockId(id), st))
        .collect()
}

fn empty_map() -> HashMap<BlockId, BlockDiagnoseState> {
    HashMap::new()
}

fn cache_for(blocks: &[Block]) -> BlockLayoutCache {
    let mut cache = BlockLayoutCache::default();
    for b in blocks {
        cache.ensure_cached(b, COLS);
    }
    cache.build_prefix_sum(blocks);
    cache
}

fn layout<'a>(
    blocks: &'a [Block],
    cache: &'a BlockLayoutCache,
    flag: bool,
    diagnose: &HashMap<BlockId, BlockDiagnoseState>,
    clip_top: f32,
    clip_bottom: f32,
    scroll_px: f32,
) -> LayoutPassOutput<'a> {
    compute_block_layout_pass(
        LayoutPassInput {
            blocks,
            live: None,
            pane_session_id: 1,
            cwd: None,
            git_branch: None,
            block_scroll: scroll_px / PITCH,
            viewport_rows: VIEWPORT,
            cols: COLS,
            pitch: PITCH,
            header_height: HEADER,
            content_bottom_y: CONTENT_BOTTOM,
            clip_top,
            clip_bottom,
            resolve_styles: false,
            styled_lookup_counter: None,
            complete_block_overlay: flag,
            block_diagnose_state: diagnose,
            now: NOW + Duration::from_secs(1_700_000_000),
            region: None,
            region_state: None,
        },
        cache,
        &mut LiveLayoutCache::default(),
    )
}

/// `Output` rows carrying resume hints are marked `line == usize::MAX`.
fn kind(row: &LaidRow<'_>) -> &'static str {
    match row {
        LaidRow::Output { line, .. } if *line == usize::MAX => "hint",
        LaidRow::Output { .. } => "out",
        LaidRow::Command { .. } => "cmd",
        LaidRow::Header { .. } => "hdr",
        LaidRow::Separator => "sep",
        LaidRow::Blank => "blank",
        LaidRow::DiagnosePanel { .. } => "ai",
        LaidRow::LiveCommand { .. } => "livecmd",
        LaidRow::LiveHeader { .. } => "livehdr",
    }
}

fn kinds(out: &LayoutPassOutput<'_>) -> Vec<&'static str> {
    out.row_data.iter().map(kind).collect()
}

fn chrome_px(out: &LayoutPassOutput<'_>) -> Vec<(OverlayChromeKind, f32)> {
    out.overlay_rows
        .iter()
        .map(|c: &OverlayChromeRow| (c.kind, c.dist_px))
        .collect()
}

// ── D1/D3: assembly re-rail ─────────────────────────────────────────────

/// Overlay ON drops the gap/Header/Separator stream entries; every kept row
/// keeps its byte-identical legacy distance, and the chrome overlay rows
/// carry exactly the distances the dropped entries had (same-y emission).
#[test]
fn overlay_on_drops_chrome_rows_and_preserves_distances() {
    let blocks = vec![plain_block(1, "one\ntwo\n", false)];
    let cache = cache_for(&blocks);
    let off = layout(
        &blocks,
        &cache,
        false,
        &empty_map(),
        0.0,
        CONTENT_BOTTOM,
        0.0,
    );
    let on = layout(
        &blocks,
        &cache,
        true,
        &empty_map(),
        0.0,
        CONTENT_BOTTOM,
        0.0,
    );

    assert_eq!(
        kinds(&off),
        ["out", "out", "blank", "cmd", "hdr", "sep"],
        "legacy shape"
    );
    assert_eq!(kinds(&on), ["out", "out", "cmd"], "chrome entries gone");

    // Kept rows (out/cmd/hint/ai): aligned kind + dist(f32 bits) parity.
    // The streams differ in length by the dropped chrome entries, so a raw
    // zip misaligns — filter each stream to kept kinds first, then pair.
    let keep = ["out", "cmd", "hint", "ai"];
    let kept_idx = |rows: &[LaidRow<'_>]| -> Vec<usize> {
        rows.iter()
            .enumerate()
            .filter(|&(_, r)| keep.contains(&kind(r)))
            .map(|(i, _)| i)
            .collect()
    };
    let off_kept = kept_idx(&off.row_data);
    let on_kept = kept_idx(&on.row_data);
    assert_eq!(off_kept.len(), on_kept.len(), "kept-row count parity");
    for (n, (i, j)) in off_kept.iter().zip(&on_kept).enumerate() {
        let (ra, da) = (&off.row_data[*i], &off.rows[*i]);
        let (rb, db) = (&on.row_data[*j], &on.rows[*j]);
        assert_eq!(kind(ra), kind(rb), "kept row {n} kind diverged");
        assert_eq!(da.to_bits(), db.to_bits(), "kept row {n} dist drifted");
    }

    // Chrome overlay rows: same-y parity with the dropped entries.
    let chrome = chrome_px(&on);
    assert_eq!(chrome.len(), 3);
    assert_eq!(chrome[0], (OverlayChromeKind::Gap, off.rows[2]));
    assert_eq!(chrome[1], (OverlayChromeKind::Header, off.rows[4]));
    assert_eq!(chrome[2], (OverlayChromeKind::Separator, off.rows[5]));
    assert_eq!(on.overlay_rows[2].block_id, Some(BlockId(1)));
    assert_eq!(on.expanded_block_count, off.expanded_block_count);
    assert_eq!(on.expanded_block_count, 1);
}

/// Sister-domain deficit (D2a): OFF − ON stream entries = Σ (gap rows) +
/// (header + separator) per completed block; clear spacers (D4) and resume
/// hints (D7) stay in the stream under both flags. Cursor end parity: the
/// last ON dist plus the dropped chrome px equals the last OFF dist.
#[test]
fn sister_domain_deficit_equals_gap_plus_header_plus_separator() {
    // Fixture A (small enough to fit one window): three blocks with output
    // → 3 gap rows + 3 × (header + separator) dropped.
    let blocks = vec![
        plain_block(1, "old\n", false),
        plain_block(3, "b\nc\n", false),
        opencode_block(4),
    ];
    let cache = cache_for(&blocks);
    let off = layout(
        &blocks,
        &cache,
        false,
        &empty_map(),
        0.0,
        CONTENT_BOTTOM,
        0.0,
    );
    let on = layout(
        &blocks,
        &cache,
        true,
        &empty_map(),
        0.0,
        CONTENT_BOTTOM,
        0.0,
    );

    let deficit = off.row_data.len() - on.row_data.len();
    assert_eq!(deficit, 3 + 6);

    // Cursor-end parity: ON's last entry is the oldest block's Command; OFF
    // continues with header + separator px past it.
    assert_eq!(
        off.rows.last().copied().unwrap(),
        on.rows.last().copied().unwrap() + HEADER + PITCH
    );
    // Hints survive the overlay rail (D7).
    assert_eq!(on.row_data.iter().filter(|r| kind(r) == "hint").count(), 2);

    // Fixture B: the clear-command spacer keeps its count AND its exact
    // distances in both streams (D4).
    let blocks = vec![plain_block(1, "old\n", false), clear_block(2)];
    let cache = cache_for(&blocks);
    let off = layout(
        &blocks,
        &cache,
        false,
        &empty_map(),
        0.0,
        CONTENT_BOTTOM,
        0.0,
    );
    let on = layout(
        &blocks,
        &cache,
        true,
        &empty_map(),
        0.0,
        CONTENT_BOTTOM,
        0.0,
    );
    let clear_off: Vec<u32> = off
        .row_data
        .iter()
        .zip(&off.rows)
        .filter(|(r, _)| kind(r) == "blank")
        .map(|(_, d)| d.to_bits())
        .collect();
    let clear_on: Vec<u32> = on
        .row_data
        .iter()
        .zip(&on.rows)
        .filter(|(r, _)| kind(r) == "blank")
        .map(|(_, d)| d.to_bits())
        .collect();
    // The clear spacer Blank survives at its exact legacy distance
    // (flag-invariant pixels, D2c/D4); the extra OFF blank is the plain
    // block's gap Blank, which ON drops. Tail-compare isolates the spacer.
    assert_eq!(
        clear_on.len() + 1,
        clear_off.len(),
        "dropped entries = gap blank"
    );
    assert_eq!(
        clear_off[..clear_off.len() - 1],
        clear_on[..],
        "spacer distances parity"
    );
}

// ── D2a: five-member truth table ────────────────────────────────────────

#[test]
fn five_member_truth_table_under_overlay() {
    // (a) plain output: output + wrap only.
    let blocks = vec![plain_block(1, "one\ntwo\n", false)];
    let cache = cache_for(&blocks);
    let on = layout(
        &blocks,
        &cache,
        true,
        &empty_map(),
        0.0,
        CONTENT_BOTTOM,
        0.0,
    );
    assert_eq!(kinds(&on), ["out", "out", "cmd"]);
    let wrap = command_wrap_rows_for(&blocks[0], COLS, true);
    assert_eq!(wrap, 1);

    // (b) diagnose thinking → one placeholder row, counted by the shared
    // chunks helper (single source with the layout pass).
    let thinking = diagnose_map(vec![(
        1,
        BlockDiagnoseState {
            pending_id: Some(9),
            result: None,
        },
    )]);
    let on = layout(&blocks, &cache, true, &thinking, 0.0, CONTENT_BOTTOM, 0.0);
    assert_eq!(kinds(&on), ["out", "out", "ai", "cmd"]);
    assert_eq!(diagnose_panel_chunks(&thinking[&BlockId(1)], COLS).len(), 1);

    // (c) diagnose Ok multi-line → one chunk per logical line at cols 80.
    let ok = diagnose_map(vec![(
        1,
        BlockDiagnoseState {
            pending_id: None,
            result: Some(Ok("alpha\nbeta".to_owned())),
        },
    )]);
    let on = layout(&blocks, &cache, true, &ok, 0.0, CONTENT_BOTTOM, 0.0);
    assert_eq!(kinds(&on), ["out", "out", "ai", "ai", "cmd"]);

    // (d) diagnose Err → "AI error: …" single row, is_error flag.
    let err = diagnose_map(vec![(
        1,
        BlockDiagnoseState {
            pending_id: None,
            result: Some(Err("boom".to_owned())),
        },
    )]);
    let on = layout(&blocks, &cache, true, &err, 0.0, CONTENT_BOTTOM, 0.0);
    assert_eq!(kinds(&on), ["out", "out", "ai", "cmd"]);
    assert!(matches!(
        on.row_data.iter().find(|r| kind(r) == "ai"),
        Some(LaidRow::DiagnosePanel { is_error: true, .. })
    ));

    // (e) collapsed: no output/hint/diagnose rows even with a pending panel.
    let collapsed = vec![plain_block(1, "x\n", true)];
    let cache_c = cache_for(&collapsed);
    let on = layout(
        &collapsed,
        &cache_c,
        true,
        &thinking,
        0.0,
        CONTENT_BOTTOM,
        0.0,
    );
    let off = layout(
        &collapsed,
        &cache_c,
        false,
        &thinking,
        0.0,
        CONTENT_BOTTOM,
        0.0,
    );
    assert_eq!(kinds(&on), ["cmd"]);
    assert_eq!(kinds(&off), ["cmd", "hdr", "sep"]);

    // (f) clear block: viewport-sized spacer stays in the ON stream (D4);
    // gap/header/separator still drop.
    let clear = vec![clear_block(1)];
    let cache_cl = cache_for(&clear);
    let on = layout(
        &clear,
        &cache_cl,
        true,
        &empty_map(),
        0.0,
        CONTENT_BOTTOM,
        0.0,
    );
    let off = layout(
        &clear,
        &cache_cl,
        false,
        &empty_map(),
        0.0,
        CONTENT_BOTTOM,
        0.0,
    );
    assert_eq!(kinds(&on), ["cmd", "blank"]);
    assert_eq!(kinds(&off), ["cmd", "hdr", "sep", "blank"]);
    assert_eq!(
        clear_block_spacer_rows("clear", VIEWPORT),
        VIEWPORT,
        "spacer member = viewport rows"
    );
    let spacer_advance = on.rows[1] - on.rows[0];
    // Flag-invariant pixels (D2c/D4): the spacer Blank keeps its legacy
    // distance, so the cmd→spacer advance still spans the dropped header
    // band + separator pitches — the overlay chrome re-paints those bands.
    assert_eq!(spacer_advance, HEADER + PITCH + VIEWPORT as f32 * PITCH);

    // (g) empty output: no gap member at all.
    let empty = vec![plain_block(1, "", false)];
    let cache_e = cache_for(&empty);
    let on = layout(
        &empty,
        &cache_e,
        true,
        &empty_map(),
        0.0,
        CONTENT_BOTTOM,
        0.0,
    );
    let off = layout(
        &empty,
        &cache_e,
        false,
        &empty_map(),
        0.0,
        CONTENT_BOTTOM,
        0.0,
    );
    assert_eq!(kinds(&on), ["cmd"]);
    assert_eq!(kinds(&off), ["cmd", "hdr", "sep"]);
}

/// D7: resume-hint pseudo-rows (`line == usize::MAX`) must survive the
/// overlay rail at their exact legacy distances.
#[test]
fn resume_hints_are_guarded_under_overlay() {
    let blocks = vec![opencode_block(1)];
    let cache = cache_for(&blocks);
    let off = layout(
        &blocks,
        &cache,
        false,
        &empty_map(),
        0.0,
        CONTENT_BOTTOM,
        0.0,
    );
    let on = layout(
        &blocks,
        &cache,
        true,
        &empty_map(),
        0.0,
        CONTENT_BOTTOM,
        0.0,
    );
    // hint,hint + content out + gap blank + cmd + hdr + sep; ON drops the
    // gap/header/separator entries, hints and content stay.
    assert_eq!(
        kinds(&off),
        ["hint", "hint", "out", "blank", "cmd", "hdr", "sep"]
    );
    assert_eq!(kinds(&on), ["hint", "hint", "out", "cmd"]);
    for i in 0..2 {
        assert!(matches!(
            on.row_data[i],
            LaidRow::Output {
                line: usize::MAX,
                ..
            }
        ));
        assert_eq!(
            off.rows[i].to_bits(),
            on.rows[i].to_bits(),
            "hint {i} y drift"
        );
    }
}

// ── D4 rollback: flag OFF byte-shape ────────────────────────────────────

/// Flag OFF keeps the exact legacy kind sequence and px arithmetic — the
/// rollback path P1 promises.
#[test]
fn flag_off_stream_matches_legacy_shape_byte_for_byte() {
    let blocks = vec![
        plain_block(1, "old\n", false),
        plain_block(2, "n1\nn2\n", false),
    ];
    let cache = cache_for(&blocks);
    let off = layout(
        &blocks,
        &cache,
        false,
        &empty_map(),
        0.0,
        CONTENT_BOTTOM,
        0.0,
    );
    // Newest block (id 2) first, then the older one (id 1).
    assert_eq!(
        kinds(&off),
        ["out", "out", "blank", "cmd", "hdr", "sep", "out", "blank", "cmd", "hdr", "sep"]
    );
    let d = &off.rows;
    assert_eq!(d[0], 20.0);
    assert_eq!(d[1], 40.0);
    assert_eq!(d[2], 60.0, "gap = +1 pitch");
    assert_eq!(d[3], 80.0, "command = +wrap pitch");
    assert_eq!(d[4], 104.0, "header = +header_height px");
    assert_eq!(d[5], 124.0, "separator = +1 pitch");
    assert_eq!(d[6], 144.0);
    assert_eq!(d[7], 164.0);
    assert_eq!(d[8], 184.0);
    assert_eq!(d[9], 208.0);
    assert_eq!(d[10], 228.0);
}

/// Header overlay payload mirrors the legacy Header row payload exactly
/// (cwd/duration/status/tone), so the emission paints identical pixels.
#[test]
fn overlay_header_payload_matches_legacy_header_row() {
    let mut b = plain_block(1, "out\n", false);
    b.cwd = Some("/tmp/weft".to_owned());
    let blocks = vec![b];
    let cache = cache_for(&blocks);
    let off = layout(
        &blocks,
        &cache,
        false,
        &empty_map(),
        0.0,
        CONTENT_BOTTOM,
        0.0,
    );
    let on = layout(
        &blocks,
        &cache,
        true,
        &empty_map(),
        0.0,
        CONTENT_BOTTOM,
        0.0,
    );
    let hdr = off
        .row_data
        .iter()
        .find_map(|r| match r {
            LaidRow::Header {
                cwd,
                duration,
                status,
                tone,
                ..
            } => Some((cwd.clone(), duration.clone(), status.clone(), *tone)),
            _ => None,
        })
        .expect("legacy header row");
    let chrome = on
        .overlay_rows
        .iter()
        .find(|c| c.kind == OverlayChromeKind::Header)
        .expect("overlay header");
    let params = chrome.header.as_ref().expect("header payload");
    assert_eq!(params.cwd, hdr.0);
    assert_eq!(params.duration, hdr.1);
    assert_eq!(params.status, hdr.2);
    assert_eq!(params.tone, hdr.3);
}

// ── D2e: cache keys ─────────────────────────────────────────────────────

/// B2c dual-field invariant + flag-invariant prefix sums: toggling the
/// cache's flag domain rebuilds the derived arrays O(n) (dirty flip) but
/// their VALUES are byte-identical because base_row_count ==
/// content_row_count + gap + wrap + separator and the overlay chrome px
/// equals (gap + 1) * pitch + header band.
#[test]
fn cache_toggle_rebuilds_but_prefix_sums_are_flag_invariant() {
    let blocks = vec![
        plain_block(1, "a\nb\n", false),
        plain_block(2, "", false),
        plain_block(3, "x\n", true),
        clear_block(4),
    ];
    let mut cache = cache_for(&blocks);
    cache.set_complete_block_overlay(true);
    cache.sync_blocks(
        &blocks,
        COLS,
        crate::paint::grid_cache::BandSync {
            low_rows: 0,
            high_rows: 0,
        },
    );
    let on_prefix = cache.prefix_sum().to_vec();
    let on_clear = cache.clear_prefix_sum().to_vec();

    // Dual-field scalars per entry.
    let plain = cache.get(1);
    assert_eq!(plain.content_row_count, 2);
    assert_eq!(plain.base_row_count, plain.content_row_count + 1 + 1 + 1);
    let empty = cache.get(2);
    assert_eq!(empty.content_row_count, 0);
    assert_eq!(empty.base_row_count, 2);
    let collapsed = cache.get(3);
    assert_eq!(collapsed.content_row_count, 0, "collapsed shows no content");
    assert_eq!(collapsed.stale_output_rows, 1, "legacy scalar preserved");
    assert_eq!(collapsed.base_row_count, 2);

    // Toggle OFF → rebuild, identical arrays.
    cache.set_complete_block_overlay(false);
    cache.sync_blocks(
        &blocks,
        COLS,
        crate::paint::grid_cache::BandSync {
            low_rows: 0,
            high_rows: 0,
        },
    );
    assert_eq!(
        cache.prefix_sum(),
        &on_prefix[..],
        "prefix sums flag-invariant"
    );
    assert_eq!(cache.clear_prefix_sum(), &on_clear[..]);
    assert_eq!(
        cache.get(1).content_row_count,
        2,
        "dual fields survive toggle"
    );

    // Setter is idempotent.
    cache.set_complete_block_overlay(false);
    cache.sync_blocks(
        &blocks,
        COLS,
        crate::paint::grid_cache::BandSync {
            low_rows: 0,
            high_rows: 0,
        },
    );
    assert_eq!(cache.prefix_sum(), &on_prefix[..]);
}

/// The scrollbar metrics memo key gains a flag bit: same pane state with a
/// different flag must miss the memo.
#[test]
fn block_scroll_metrics_key_flag_bit_fails_equality() {
    let base = BlockScrollMetricsKey {
        pane_session_id: 1,
        cols: 80,
        num_rows: 24,
        live_version: None,
        blocks_len: 3,
        first_block_id: Some(1),
        last_block_id: Some(3),
        cache_rebuilds: 0,
        header_rows: 2,
        visible: 24,
        cwd_present: true,
        complete_block_overlay: true,
    };
    let mut toggled = base;
    toggled.complete_block_overlay = false;
    assert_ne!(base, toggled);
    assert_eq!(base, base);
}

/// The background-block fingerprint gains a flag bit: a toggle is a content
/// delta — it rides the throttle window (not the immediate path, unlike a
/// rect/scroll delta).
#[test]
fn bg_block_fingerprint_flag_bit_breaks_same_content() {
    let fp = |flag: bool| BgBlockFingerprint {
        live_output_version: 7,
        screen_head_lines: 0,
        cwd: Some("/tmp".to_owned()),
        git_branch: None,
        theme_generation: 1,
        cell_dims: (8, 16),
        rect: [0.0, 0.0, 400.0, 300.0],
        block_scroll: 0.0,
        editor_mode: false,
        complete_block_overlay: flag,
    };
    let on = fp(true);
    let off = fp(false);
    assert_ne!(on, off);
    assert!(!on.same_content(&off));

    let last = Instant::now();
    let now = last + Duration::from_millis(30);
    let throttle = Duration::from_millis(60);
    assert_eq!(
        bg_block_cache_action(Some((&on, last)), &off, now, throttle),
        BgBlockCacheAction::Throttled,
        "flag delta is content, not immediate geometry"
    );
    let later = last + throttle + Duration::from_millis(1);
    assert_eq!(
        bg_block_cache_action(Some((&on, last)), &off, later, throttle),
        BgBlockCacheAction::RebuildNow,
    );
    // Rect-only delta (same flag): immediate rebuild.
    let mut moved = fp(true);
    moved.rect = [4.0, 0.0, 404.0, 300.0];
    assert_eq!(
        bg_block_cache_action(Some((&on, last)), &moved, now, throttle),
        BgBlockCacheAction::RebuildNow,
    );
}

// ── D2f: find/panel chokepoints ─────────────────────────────────────────

#[test]
fn find_jump_chokepoint_drops_chrome_from_distance() {
    let b = plain_block(1, "alpha line\nbeta line\n", false);
    let cmd_hit = BlockMatch {
        block_id: b.id,
        is_command: true,
        line: 0,
        col: 0,
        len: 3,
    };
    let out_hit = BlockMatch {
        block_id: b.id,
        is_command: false,
        line: 0,
        col: 0,
        len: 3,
    };
    // 2 output chunks, no hints: command distance = output rows under ON,
    // output + gap + separator under OFF.
    assert_eq!(completed_block_output_rows(&b, COLS), 2);
    assert_eq!(
        completed_block_match_row_from_bottom(&b, &cmd_hit, COLS, true),
        2
    );
    assert_eq!(
        completed_block_match_row_from_bottom(&b, &cmd_hit, COLS, false),
        4
    );
    // Output hit: chunks after the hit chunk; OFF adds the separator (+1).
    assert_eq!(
        completed_block_match_row_from_bottom(&b, &out_hit, COLS, true),
        1
    );
    assert_eq!(
        completed_block_match_row_from_bottom(&b, &out_hit, COLS, false),
        2
    );

    // Whole-block row counts (panel/find scroll targets): plain / clear.
    assert_eq!(completed_block_layout_rows(&b, COLS, 2, VIEWPORT, true), 3);
    // OFF = out(2) + gap(1) + header band(2, the header_rows param) + wrap(1)
    // + separator(1) — the legacy formula, byte-identical chokepoint.
    assert_eq!(completed_block_layout_rows(&b, COLS, 2, VIEWPORT, false), 7);
    let clear = clear_block(2);
    assert_eq!(
        completed_block_layout_rows(&clear, COLS, 2, VIEWPORT, true),
        25
    );
    assert_eq!(
        completed_block_layout_rows(&clear, COLS, 2, VIEWPORT, false),
        28
    );
}

// ── D6: overscan widening ───────────────────────────────────────────────

/// Four 1-line blocks, 104 px each under both flags (total 416). With the
/// clip top raised so the OLDEST block's bottom edge (y = 488) lands in the
/// widened-overscan band [clip−128, clip−64) = [425, 489) but OUTSIDE the
/// legacy band, OFF excludes it and ON includes it. Interior windows stay
/// identical (no drift).
#[test]
fn overscan_widens_by_uniform_chrome_under_flag() {
    let blocks = vec![
        plain_block(1, "o\n", false),
        plain_block(2, "m\n", false),
        plain_block(3, "n\n", false),
        plain_block(4, "p\n", false),
    ];
    let cache = cache_for(&blocks);
    let diag = empty_map();
    // Oldest block spans dist 312..416 → y 384..488. Legacy overscan band
    // starts at clip−64 = 489 (misses by 1 px); widened starts at clip−128
    // = 425 (covers it).
    let clip_top = 553.0;
    let off = layout(&blocks, &cache, false, &diag, clip_top, CONTENT_BOTTOM, 0.0);
    let on = layout(&blocks, &cache, true, &diag, clip_top, CONTENT_BOTTOM, 0.0);
    assert_eq!(off.expanded_block_count, 3, "legacy overscan misses it");
    assert_eq!(on.expanded_block_count, 4, "widened overscan reaches it");

    // Interior window: identical expansion both flags.
    let off = layout(&blocks, &cache, false, &diag, 0.0, CONTENT_BOTTOM, 0.0);
    let on = layout(&blocks, &cache, true, &diag, 0.0, CONTENT_BOTTOM, 0.0);
    assert_eq!(off.expanded_block_count, 4);
    assert_eq!(on.expanded_block_count, 4);
    let on_outputs: Vec<u32> = on
        .row_data
        .iter()
        .zip(&on.rows)
        .filter(|(r, _)| kind(r) == "out")
        .map(|(_, d)| d.to_bits())
        .collect();
    let off_outputs: Vec<u32> = off
        .row_data
        .iter()
        .zip(&off.rows)
        .filter(|(r, _)| kind(r) == "out")
        .map(|(_, d)| d.to_bits())
        .collect();
    assert_eq!(on_outputs, off_outputs, "output rows y-identical");
}

/// D2c/N1: the warm-window inputs are flag-independent — prefix sums are
/// byte-identical across the toggle and the bounds binary search returns
/// the same window for the same thresholds.
#[test]
fn atlas_warmup_window_flag_identity() {
    let blocks = vec![
        plain_block(1, "a\n", false),
        plain_block(2, "b\nc\n", false),
    ];
    let mut cache_on = cache_for(&blocks);
    cache_on.set_complete_block_overlay(true);
    cache_on.sync_blocks(
        &blocks,
        COLS,
        crate::paint::grid_cache::BandSync {
            low_rows: 0,
            high_rows: 0,
        },
    );
    let mut cache_off = cache_for(&blocks);
    cache_off.set_complete_block_overlay(false);
    cache_off.sync_blocks(
        &blocks,
        COLS,
        crate::paint::grid_cache::BandSync {
            low_rows: 0,
            high_rows: 0,
        },
    );

    assert_eq!(cache_on.prefix_sum(), cache_off.prefix_sum());
    assert_eq!(cache_on.clear_prefix_sum(), cache_off.clear_prefix_sum());

    let clear_pitch = VIEWPORT as f32 * PITCH;
    let threshold = 500.0;
    assert_eq!(
        lower_bound_height(
            cache_on.prefix_sum(),
            cache_on.clear_prefix_sum(),
            PITCH,
            HEADER,
            clear_pitch,
            threshold
        ),
        lower_bound_height(
            cache_off.prefix_sum(),
            cache_off.clear_prefix_sum(),
            PITCH,
            HEADER,
            clear_pitch,
            threshold
        )
    );
    assert_eq!(
        upper_bound_height(
            cache_on.prefix_sum(),
            cache_on.clear_prefix_sum(),
            PITCH,
            HEADER,
            clear_pitch,
            threshold
        ),
        upper_bound_height(
            cache_off.prefix_sum(),
            cache_off.clear_prefix_sum(),
            PITCH,
            HEADER,
            clear_pitch,
            threshold
        )
    );
}

// ── D5: surfaces bounds parity ──────────────────────────────────────────

/// Metal-gated (golden precedent): block surface bounds under overlay ON
/// equal the flag-OFF bounds byte-for-byte — the constant-arithmetic fold
/// only restores the header band for blocks with no laid output rows.
#[test]
fn surfaces_bounds_align_with_emission_geometry() {
    let Some(device) = metal::Device::system_default() else {
        eprintln!("skipping surfaces parity test: no Metal device available");
        return;
    };
    let _ = device;
    let blocks = vec![plain_block(1, "out\n", false), plain_block(2, "x\n", true)];
    let cache = cache_for(&blocks);

    let mut renderer = MetalRenderer::new_headless_paint(Theme::weft_dark());
    renderer.sync_pane_frame_state(0.0, true);
    let on = layout(
        &blocks,
        &cache,
        true,
        &empty_map(),
        0.0,
        CONTENT_BOTTOM,
        0.0,
    );
    let mut verts_on = Vec::new();
    let surfaces_on = crate::paint::block_view::surfaces::push_block_surfaces(
        &renderer,
        &mut verts_on,
        &on.rows,
        &on.row_data,
        &on.overlay_rows,
        &crate::layout::BlockViewLayout {
            frame_left: 0.0,
            frame_right: 600.0,
            pitch: PITCH,
            left: 8.0,
            right: 592.0,
            cols: COLS,
            clip_top: 0.0,
            clip_bottom: CONTENT_BOTTOM,
            fixed_cwd_y: 0.0,
        },
        crate::paint::block_view::surfaces::BlockSurfaceState {
            scroll_px: 0.0,
            header_height: HEADER,
            hovered: None,
            selected: None,
        },
    );

    renderer.sync_pane_frame_state(0.0, false);
    let off = layout(
        &blocks,
        &cache,
        false,
        &empty_map(),
        0.0,
        CONTENT_BOTTOM,
        0.0,
    );
    let mut verts_off = Vec::new();
    let surfaces_off = crate::paint::block_view::surfaces::push_block_surfaces(
        &renderer,
        &mut verts_off,
        &off.rows,
        &off.row_data,
        &[],
        &crate::layout::BlockViewLayout {
            frame_left: 0.0,
            frame_right: 600.0,
            pitch: PITCH,
            left: 8.0,
            right: 592.0,
            cols: COLS,
            clip_top: 0.0,
            clip_bottom: CONTENT_BOTTOM,
            fixed_cwd_y: 0.0,
        },
        crate::paint::block_view::surfaces::BlockSurfaceState {
            scroll_px: 0.0,
            header_height: HEADER,
            hovered: None,
            selected: None,
        },
    );

    assert_eq!(surfaces_on, surfaces_off, "D5 bounds parity per block");
    // Both blocks present: the plain one spans output∪command, the
    // collapsed one spans header+command (the ON no-output fold restores
    // the OFF band).
    assert_eq!(surfaces_on.len(), 2);
    // Byte-level quad parity: the map values are per-block surface COLORS
    // (bounds are internal), so the geometry promise is asserted on the
    // pushed vertices — identical band quads under both flags.
    assert_eq!(verts_on, verts_off, "D5 vertex parity per block");
}
