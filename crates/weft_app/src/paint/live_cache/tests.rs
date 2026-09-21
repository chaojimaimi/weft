//! LiveLayoutCache tests — migrated from the inline `mod tests` (M6-a moved
//! them here per the `*/tests.rs` commit-gate exemption) plus the M6-a
//! incremental-sync suite (PLAN_M6 §A-3): append equivalence, partial-line
//! growth, window identity, guard truth table, dual-consumer timing and the
//! rewrite-stream fuzz against a forced-full oracle.

use super::*;
use crate::paint::grid_cache::block_line_chunks;
use weft_core::blocks::{BlockTracker, CapturedStyle, StyledOutput};

/// Fixed pane_session_id for the shared-key tests; the session-scoping
/// tests below pass ids explicitly. Defaults to the screen-origin (TUI)
/// slice so the layout-math tests keep one-clipped-row-per-line geometry;
/// the ordinary soft-wrap split is exercised by the dedicated tests under
/// `sync(..., screen_origin: false)`.
fn sync(cache: &mut LiveLayoutCache, output: &str, version: u64, cols: usize) {
    cache.sync(output, 0, version, cols, true, usize::MAX);
}

/// Rows-per-line by the live layout split (v1.10.26 Batch B review
/// blocker BL-1): screen-origin lines clip to exactly one row regardless
/// of width; ordinary shell-output lines count their soft-wrap chunks.
fn wrap_counts(output: &str, cols: usize, screen_origin: bool) -> Vec<usize> {
    output
        .lines()
        .map(|line| {
            if screen_origin {
                screen_origin_line_chunks(line, cols).count()
            } else {
                block_line_chunks(line, cols).count()
            }
        })
        .collect()
}

#[test]
fn cumulative_screen_origin_counts_one_per_line() {
    let output = "0123456789abcdefghij\none\ntwo lines\n";
    let mut cache = LiveLayoutCache::default();
    sync(&mut cache, output, 7, 8);
    // B-1: a 20-char line at cols 8 (screen-origin) is CLIPPED to one
    // display row (no soft-wrap) — screen-origin counts are 1 per line.
    let counts = wrap_counts(output, 8, true);
    assert_eq!(counts, vec![1, 1, 1]);
    let mut acc = 0;
    for (i, c) in counts.iter().enumerate() {
        acc += c;
        assert_eq!(cache.cumulative()[i + 1] as usize, acc, "prefix at {i}");
    }
    assert_eq!(cache.total_display_rows(), 3);
    assert_eq!(cache.total_lines(), 3);
    assert_eq!(cache.base_idx(), 0);
    // line_ranges must slice back the exact lines() texts (incl. \r strip).
    assert_eq!(
        &output[cache.line_range(0).0..cache.line_range(0).1],
        "0123456789abcdefghij"
    );
    assert_eq!(
        &output[cache.line_range(2).0..cache.line_range(2).1],
        "two lines"
    );
}

#[test]
fn cr_stripped_ranges_match_lines() {
    let output = "a\r\nb\r\nc";
    let mut cache = LiveLayoutCache::default();
    sync(&mut cache, output, 1, 80);
    let lines: Vec<&str> = output.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let (s, e) = cache.line_range(i);
        assert_eq!(&output[s..e], *line, "range {i}");
    }
}

#[test]
fn cache_hit_skips_rebuild_version_and_cols_keys() {
    // B-1: the live layout is screen-origin (clip-not-wrap), so wrapping
    // is cols-INDEPENDENT — the cache still re-keys on cols/version, but
    // a rebuild produces identical cumulative.
    let output = format!("{}\nbbb\n", "x".repeat(60));
    let mut cache = LiveLayoutCache::default();
    sync(&mut cache, &output, 3, 80);
    assert_eq!(cache.rebuilds(), 1);
    let cumulative = cache.cumulative().to_vec();

    // Same version + cols → HIT: no rebuild, cumulative identical.
    sync(&mut cache, &output, 3, 80);
    assert_eq!(cache.rebuilds(), 1, "same version+cols must be a hit");
    assert_eq!(cache.cumulative(), cumulative.as_slice());

    // cols change → MISS (re-key) but cumulative IDENTICAL: the
    // screen-origin clip layout yields one row per line regardless of
    // width.
    sync(&mut cache, &output, 3, 40);
    assert_eq!(cache.rebuilds(), 2, "cols change must be a miss");
    assert_eq!(
        cache.cumulative().last().copied().unwrap_or(0),
        2,
        "60-char line clips to 1 row + 'bbb' = 2 rows total"
    );

    // version change → MISS even with same cols (and identical bytes:
    // the reset must re-key, not reuse).
    sync(&mut cache, &output, 4, 40);
    let cumulative2 = cache.cumulative().to_vec();
    assert_eq!(cache.rebuilds(), 3, "version change must be a miss");
    sync(&mut cache, &output, 5, 40);
    assert_eq!(cache.rebuilds(), 4, "version change must be a miss");
    // Same bytes + same cols → identical layout after rebuild (no drift).
    assert_eq!(cache.cumulative(), cumulative2.as_slice());
}

/// B1 (review blocker): the cache is shared across panes but version
/// counters are per-Tab (start at 0) — two panes coincidentally at the
/// same version+cols must NOT hit each other's entries (slicing one
/// pane's bytes with the other's ranges).
#[test]
fn different_pane_session_same_version_cols_misses() {
    let output = "aaa\nbbb\n";
    let mut cache = LiveLayoutCache::default();
    cache.sync(output, 10, 7, 8, true, usize::MAX); // pane A, version 7
    assert_eq!(cache.rebuilds(), 1);
    let pane_a_cumulative = cache.cumulative().to_vec();
    // Pane B: same version+cols, different session → MISS.
    cache.sync(output, 11, 7, 8, true, usize::MAX);
    assert_eq!(
        cache.rebuilds(),
        2,
        "same version+cols across sessions must miss"
    );
    // Alternating back to pane A rebuilds again — no cross-session reuse.
    cache.sync(output, 10, 7, 8, true, usize::MAX);
    assert_eq!(cache.rebuilds(), 3, "session switch must not reuse");
    assert_eq!(cache.cumulative(), pane_a_cumulative.as_slice());
    cache.sync(output, 11, 7, 8, true, usize::MAX);
    assert_eq!(cache.rebuilds(), 4);
}

#[test]
fn same_pane_session_same_version_cols_hits() {
    let output = "aaa\nbbb\n";
    let mut cache = LiveLayoutCache::default();
    cache.sync(output, 10, 7, 8, true, usize::MAX);
    assert_eq!(cache.rebuilds(), 1);
    cache.sync(output, 10, 7, 8, true, usize::MAX);
    assert_eq!(cache.rebuilds(), 1, "same session+version+cols is a hit");
}

// ── BL-1 (Batch B review blocker): split the live layout by screen_origin
// ────────────────────────────────────────────────────────────────────────

/// A plain shell command (no `screen_document_start`) emits long streaming
/// lines. While the command is in flight they must SOFT-WRAP like finished
/// shell-output blocks — the initial Batch B "live is always screen-origin"
/// clip was a functional regression (a `make` diagnostic line was chopped
/// at the window edge mid-stream).
#[test]
fn ordinary_live_long_line_soft_wraps_with_complete_content() {
    let output = format!("{}\nshort line\n", "x".repeat(40));
    let mut cache = LiveLayoutCache::default();
    // screen_origin = false: ordinary streaming output.
    cache.sync(&output, 0, 3, 8, false, usize::MAX);
    // 40-char line at cols 8 → 5 wrapped rows; "short line" (10 cols) → 2.
    let counts = wrap_counts(&output, 8, false);
    assert_eq!(counts, vec![5, 2], "ordinary long line soft-wraps");
    assert_eq!(cache.total_display_rows(), 7);
    // Content is preserved across the wrapped rows (nothing clipped).
    let mut reconstructed = String::new();
    for i in 0..cache.total_lines() {
        let (s, e) = cache.line_range(i);
        reconstructed.push_str(&output[s..e]);
        reconstructed.push('\n');
    }
    assert_eq!(reconstructed, output, "soft-wrap must not lose text");
}

/// The screen-owned TUI case: a long frame row is a hard terminal row that
/// CLIPS to one display row — the `|]` border must never fold.
#[test]
fn screen_owned_live_long_line_clips_to_one_row() {
    let line = format!("[|{}|]", "x".repeat(40));
    let output = format!("{line}\n");
    let mut cache = LiveLayoutCache::default();
    cache.sync(&output, 0, 3, 8, true, usize::MAX);
    assert_eq!(
        cache.total_display_rows(),
        1,
        "screen-origin long line is clipped, not wrapped ({line})"
    );
    assert_eq!(cache.total_lines(), 1);
    let counts = wrap_counts(&output, 8, true);
    assert_eq!(counts, vec![1]);
}

/// The two live modes flip mid-command (`begin_screen_owned_output` after
/// plain streaming): the version bump must invalidate the cache and the
/// rebuilt layout must switch to clip — same bytes, different row counts.
#[test]
fn screen_origin_switch_invalidates_cache_and_clips() {
    let output = format!("{}\n", "y".repeat(40));
    let mut cache = LiveLayoutCache::default();
    // Ordinary phase: long line soft-wraps → 40-col line at cols 10 = 4 rows.
    cache.sync(&output, 0, 7, 10, false, usize::MAX);
    assert_eq!(cache.rebuilds(), 1);
    assert_eq!(cache.total_display_rows(), 4, "ordinary: wrapped rows");
    // Same bytes+cols, but the tracker bump from screen handoff changed
    // state → the flag join forces a MISS, and the rebuilt layout clips.
    cache.sync(&output, 0, 8, 10, true, usize::MAX);
    assert_eq!(cache.rebuilds(), 2, "screen_origin flip must invalidate");
    assert_eq!(
        cache.total_display_rows(),
        1,
        "same bytes now clip to one row — the switch took effect"
    );
    // Flipping back (new command, plain output) rebuilds again.
    cache.sync(&output, 0, 9, 10, false, usize::MAX);
    assert_eq!(cache.rebuilds(), 3);
    assert_eq!(cache.total_display_rows(), 4);
}

#[test]
fn tail_window_caps_at_max_layout_lines() {
    let output = (0..3000).map(|i| format!("line {i}\n")).collect::<String>();
    let mut cache = LiveLayoutCache::default();
    sync(&mut cache, &output, 1, 80);
    assert_eq!(cache.total_lines(), MAX_LAYOUT_LINES_LIVE);
    assert_eq!(cache.base_idx(), 1000);
    // The window starts at raw line 1000 ("line 999\n" is the last
    // excluded line); ranges slice the exact line bytes (no newline).
    let (s, e) = cache.line_range(0);
    assert_eq!(&output[s..e], "line 1000");
    let (_, e) = cache.line_range(MAX_LAYOUT_LINES_LIVE - 1);
    assert_eq!(&output[e - "line 2999".len()..e], "line 2999");
}

// ── visible_window: cumulative 二分窗口边界 ─────────────────────────

/// Viewport sits at the bottom (following live): the window hugs the
/// NEWEST lines — line 19 (newest) stays inside, the older lines are
/// culled. 20 lines × 1 display row @ pitch 20 → total 400px; viewport
/// 6 rows (120px), overscan 3 lines.
#[test]
fn visible_window_following_bottom_hugs_newest() {
    let output = (0..20).map(|i| format!("l{i}\n")).collect::<String>();
    let mut cache = LiveLayoutCache::default();
    sync(&mut cache, &output, 1, 80);
    let pitch = 20.0;
    // scroll_px = 0 → bottom edge at -overscan(64px), top edge at
    // 160px; overscan 64px → 5 lines.
    let (start, end) = cache.visible_window(pitch, -40.0, 160.0, 64.0);
    assert_eq!((start, end), (7, 20));
    assert!(start <= 19 && end > 19, "newest line must stay in window");
    // Only 13 of 20 lines materialized.
    assert!(end - start < 20);
    // The visible band (120px = 6 rows) is fully covered: lines 12..19
    // are inside [start, end).
    assert!(start <= 12 && end > 19);
}

#[test]
fn visible_window_tail_less_than_one_screen() {
    // 3 lines, viewport 6 rows → whole document visible, full window.
    let output = "a\nb\nc\n";
    let mut cache = LiveLayoutCache::default();
    sync(&mut cache, output, 1, 80);
    let (start, end) = cache.visible_window(20.0, -40.0, 160.0, 64.0);
    assert_eq!((start, end), (0, 3));
}

#[test]
fn visible_window_scrolled_to_oldest_edge() {
    // 100 lines → total 2000px. Scroll far past the block: the window
    // falls back to the top (oldest) lines nearest the band.
    let output = (0..100).map(|i| format!("l{i}\n")).collect::<String>();
    let mut cache = LiveLayoutCache::default();
    sync(&mut cache, &output, 1, 80);
    let (start, end) = cache.visible_window(20.0, 10_000.0, 10_400.0, 64.0);
    assert_eq!((start, end), (0, 6));

    // Band [1900, 1950]px cuts lines 2..4 (tops 1960/1940/1920,
    // bottoms 1940/1920/1900); window = [l-5, f+1+5).
    let (start2, end2) = cache.visible_window(20.0, 1900.0, 1950.0, 64.0);
    assert_eq!((start2, end2), (0, 10));
    assert!(start2 <= 2 && end2 > 4, "band lines 2..4 must be inside");
    // Window covers the band in cumulative-row space: start row <= top
    // bound (2.5), end row >= bottom bound (5).
    let cum = cache.cumulative();
    assert!(cum[start2] as f32 <= 2.5);
    assert!(cum[end2] as f32 >= 5.0);
}

#[test]
fn visible_window_empty_output() {
    let mut cache = LiveLayoutCache::default();
    sync(&mut cache, "", 1, 80);
    assert_eq!(cache.visible_window(20.0, 0.0, 100.0, 64.0), (0, 0));
    assert_eq!(cache.total_display_rows(), 0);
}

/// The `BlockTracker` live-output version — the cache key — bumps on
/// every mutation (print/newline/ascii/snapshot-replace/clear) and stays
/// put while the output is unchanged (scroll ticks don't invalidate).
#[test]
fn tracker_live_output_version_tracks_mutations() {
    use weft_core::blocks::{BlockTracker, CapturedStyle, StyledOutput};
    let mut t = BlockTracker::new();
    t.on_prompt_start();
    let v_before = t.in_flight().map(|l| l.version);
    // Not capturing: prints must NOT bump (output unchanged).
    t.on_print('x', CapturedStyle::default());
    assert_eq!(t.in_flight().map(|l| l.version), v_before);

    t.on_command_start("echo".to_string());
    let v0 = t.in_flight().expect("command executing").version;
    t.on_print('a', CapturedStyle::default());
    assert_eq!(t.in_flight().unwrap().version, v0 + 1, "print bumps");
    t.on_newline();
    t.on_print_ascii_run(b"bcd", CapturedStyle::default());
    assert_eq!(t.in_flight().unwrap().version, v0 + 3, "newline+ascii bump");

    // Screen-owned path: handoff clears output, snapshot replace bumps.
    t.on_command_end(0);
    t.on_prompt_start();
    t.on_command_start("tui".to_string());
    let v_owned = t.in_flight().unwrap().version;
    t.begin_screen_owned_output(0);
    assert_eq!(
        t.in_flight().unwrap().version,
        v_owned + 1,
        "screen handoff clears output"
    );
    let styled = StyledOutput { lines: Vec::new() };
    t.replace_screen_snapshot("frame one\n", styled.clone());
    let v1 = t.in_flight().unwrap().version;
    t.replace_screen_snapshot("frame two\n", styled);
    assert_eq!(
        t.in_flight().unwrap().version,
        v1 + 1,
        "snapshot replace bumps"
    );
}

/// The window never skips a line the old full materialization showed:
/// every line whose band intersects the viewport must be inside the
/// emitted window, and the emitted count stays bounded.
#[test]
fn visible_window_covers_band_exactly() {
    let output = (0..50).map(|i| format!("l{i}\n")).collect::<String>();
    let mut cache = LiveLayoutCache::default();
    sync(&mut cache, &output, 1, 80);
    let pitch = 20.0;
    let total = 50.0_f32 * pitch; // 1000px
    for band_top in [0.0_f32, 200.0, 500.0, 900.0, 980.0] {
        let band_bottom = band_top + 200.0;
        let (start, end) = cache.visible_window(pitch, band_top, band_bottom, 40.0);
        assert!(start < end, "band {band_top}");
        // Every visible line (top >= band_top, bottom <= band_bottom)
        // is inside the window.
        for i in 0..50usize {
            let top = total - cache.cumulative()[i] as f32 * pitch;
            let bottom = total - cache.cumulative()[i + 1] as f32 * pitch;
            if top >= band_top && bottom <= band_bottom {
                assert!(
                    start <= i && i < end,
                    "band {band_top}: visible line {i} culled"
                );
            }
        }
        // Materialization bounded: band rows + 2×overscan + slack.
        let band_rows = (band_bottom - band_top) / pitch;
        assert!(
            end - start <= band_rows as usize + 2 * 2 + 4,
            "band {band_top}"
        );
    }
}

// ── M6-a: incremental append vs full rebuild (PLAN_M6 §A-3) ─────────

/// Whole-layout snapshot of every public accessor, for the equivalence
/// comparisons. `windows` exercises the absolute-space threshold
/// translation across three viewport placements (bottom / mid / oldest).
struct LayoutSnapshot {
    label: String,
    cumulative: Vec<u32>,
    ranges: Vec<(usize, usize)>,
    windows: Vec<(usize, usize)>,
}

fn snapshot(cache: &LiveLayoutCache, output: &str, label: &str) -> LayoutSnapshot {
    let mut ranges = Vec::with_capacity(cache.total_lines());
    for i in 0..cache.total_lines() {
        let (s, e) = cache.line_range(i);
        // Range sanity: in-bounds and on char boundaries (slicing below
        // would panic otherwise — the panic IS the failure mode we guard).
        assert!(e <= output.len() && output.is_char_boundary(s), "{label}");
        assert!(
            s <= e && (e == output.len() || output.is_char_boundary(e)),
            "{label}"
        );
        ranges.push((s, e));
    }
    let windows = vec![
        cache.visible_window(20.0, -40.0, 160.0, 64.0),
        cache.visible_window(20.0, 300.0, 500.0, 40.0),
        cache.visible_window(20.0, 10_000.0, 10_400.0, 64.0),
    ];
    let label = format!(
        "{label}: lines={} base={} rows={} win={windows:?}",
        cache.total_lines(),
        cache.base_idx(),
        cache.total_display_rows(),
    );
    LayoutSnapshot {
        label,
        cumulative: cache.cumulative().to_vec(),
        ranges,
        windows,
    }
}

/// K incremental appends vs one one-shot rebuild: every accessor must be
/// value-identical. Covers plain `\n`, CRLF (ranges strip `\r` like
/// `lines()`), multibyte content around boundary offsets, an empty-output
/// start, and a single unterminated line that grows.
#[test]
fn incremental_appends_equal_one_shot_rebuild() {
    const COLS: usize = 12;
    let cases: &[(&str, &[&str])] = &[
        ("plain", &["alpha\n", "beta\n", "gamma\n"]),
        ("crlf", &["win1\r", "\nwin2\r", "\nend\r\n"]),
        ("multibyte", &["你好\n", "世界 mixed x\n", "终\n"]),
        ("empty start", &["", "first\n", "second\n"]),
        (
            "partial start",
            &["just one", " partial", " line\n", "tail\n"],
        ),
    ];
    for (label, chunks) in cases {
        let mut cache = LiveLayoutCache::default();
        let mut doc = String::new();
        for (i, chunk) in chunks.iter().enumerate() {
            doc.push_str(chunk);
            let watermark = doc.len() - chunk.len();
            cache.sync(&doc, 1, i as u64 + 1, COLS, false, watermark);
        }
        let mut oracle = LiveLayoutCache::default();
        oracle.sync(&doc, 999, 1, COLS, false, usize::MAX);
        assert_eq!(cache.total_lines(), oracle.total_lines(), "{label}");
        assert_eq!(cache.base_idx(), oracle.base_idx(), "{label}");
        assert_eq!(
            cache.total_display_rows(),
            oracle.total_display_rows(),
            "{label}"
        );
        let have = snapshot(&cache, &doc, label);
        let want = snapshot(&oracle, &doc, label);
        assert_eq!(have.label, want.label, "{label}");
        assert_eq!(have.cumulative, want.cumulative, "{label}");
        assert_eq!(have.ranges, want.ranges, "{label}");
        assert_eq!(have.windows, want.windows, "{label}");
        // The fast path must actually have been taken (first sync on a
        // fresh cache is a full rebuild by pane mismatch, the rest append).
        assert_eq!(cache.appends(), chunks.len() - 1, "{label}");
    }
}

/// `"abc"` → `"abc\n"` → `"abc\nde"` → `"abc\nde\n"`: each step must be
/// correct in isolation (the growing partial line is re-read, the boundary
/// never advances past it).
#[test]
fn partial_line_growth_sequence() {
    let mut c = LiveLayoutCache::default();
    c.sync("abc", 1, 1, 10, false, usize::MAX);
    assert_eq!(c.total_lines(), 1);
    assert_eq!(c.total_display_rows(), 1);
    assert_eq!(c.line_range(0), (0, 3));
    assert_eq!(c.appends(), 0, "fresh cache rebuilds");

    c.sync("abc\n", 1, 2, 10, false, 3);
    assert_eq!(c.appends(), 1, "partial completed in place");
    assert_eq!(c.total_lines(), 1);
    assert_eq!(c.line_range(0), (0, 3));
    assert_eq!(c.total_display_rows(), 1);

    c.sync("abc\nde", 1, 3, 10, false, 4);
    assert_eq!(c.appends(), 2);
    assert_eq!(c.total_lines(), 2);
    assert_eq!(c.line_range(1), (4, 6));
    assert_eq!(c.total_display_rows(), 2);

    c.sync("abc\nde\n", 1, 4, 10, false, 4);
    assert_eq!(c.appends(), 3);
    assert_eq!(c.total_lines(), 2);
    assert_eq!(c.line_range(1), (4, 6));
    assert_eq!(c.total_display_rows(), 2);

    let mut oracle = LiveLayoutCache::default();
    oracle.sync("abc\nde\n", 9, 1, 10, false, usize::MAX);
    assert_eq!(c.cumulative(), oracle.cumulative());
    assert_eq!(c.total_display_rows(), oracle.total_display_rows());
}

/// 3000+ lines streamed in batches: at EVERY step the window identity
/// holds — `total_lines() == min(doc_lines, MAX_LAYOUT_LINES_LIVE)` and
/// `base_idx()` equals the full-rebuild oracle — with all accessors
/// value-identical to a fresh full rebuild (the continuous-cap contract;
/// `tail_window_caps_at_max_layout_lines` pins the at-rest end state).
#[test]
fn streaming_window_identity_matches_full_rebuild_at_every_step() {
    const COLS: usize = 16;
    let mut cache = LiveLayoutCache::default();
    let mut doc = String::new();
    let mut doc_lines = 0usize;
    for batch in 0..82u64 {
        for i in 0..37 {
            doc_lines += 1;
            doc.push_str(&format!("stream {batch}-{i} payload row\n"));
        }
        cache.sync(&doc, 1, batch + 1, COLS, false, usize::MAX);
        let mut oracle = LiveLayoutCache::default();
        oracle.sync(&doc, 500_000 + batch, batch + 1, COLS, false, usize::MAX);
        assert_eq!(cache.total_lines(), oracle.total_lines(), "batch {batch}");
        assert_eq!(
            cache.total_lines(),
            doc_lines.min(MAX_LAYOUT_LINES_LIVE),
            "batch {batch}: continuous cap"
        );
        assert_eq!(cache.base_idx(), oracle.base_idx(), "batch {batch}");
        assert_eq!(
            cache.base_idx(),
            doc_lines.saturating_sub(MAX_LAYOUT_LINES_LIVE),
            "batch {batch}"
        );
        assert_eq!(cache.cumulative(), oracle.cumulative(), "batch {batch}");
        assert_eq!(
            cache.total_display_rows(),
            oracle.total_display_rows(),
            "batch {batch}"
        );
        for i in 0..cache.total_lines() {
            assert_eq!(
                cache.line_range(i),
                oracle.line_range(i),
                "batch {batch} line {i}"
            );
        }
    }
    assert!(doc_lines > 3000, "must cross the window cap");
}

/// Guard truth table (PLAN_M6 §A-2): each failing guard alone must force
/// the full-rebuild fallback (and produce correct tables), while a healthy
/// append between failures still takes the fast path.
#[test]
fn guard_chain_fallback_truth_table() {
    let mut c = LiveLayoutCache::default();
    // Baseline: fresh cache full-rebuilds, then appends incrementally.
    c.sync("ab\ncd\n", 1, 1, 10, false, usize::MAX);
    assert_eq!(c.rebuilds(), 1);
    c.sync("ab\ncd\nef\n", 1, 2, 10, false, 6);
    assert_eq!((c.rebuilds(), c.appends()), (1, 1));

    // cols change → full rebuild.
    c.sync("ab\ncd\nef\ngh\n", 1, 3, 20, false, usize::MAX);
    assert_eq!(c.rebuilds(), 2);
    // screen_origin flip (ordinary → screen) → full rebuild.
    c.sync("ab\ncd\nef\ngh\n", 1, 4, 20, true, usize::MAX);
    assert_eq!(c.rebuilds(), 3);
    // pane change → full rebuild.
    c.sync("ab\ncd\nef\ngh\n", 2, 5, 20, true, usize::MAX);
    assert_eq!(c.rebuilds(), 4);
    // screen flip back (cached screen_origin=true) → full rebuild.
    c.sync("ab\ncd\nef\ngh\n", 2, 6, 20, false, usize::MAX);
    assert_eq!(c.rebuilds(), 5);
    // pane back → full rebuild.
    c.sync("ab\ncd\nef\ngh\n", 1, 7, 20, false, usize::MAX);
    assert_eq!(c.rebuilds(), 6);
    assert_eq!(c.total_lines(), 4);
    // watermark below the boundary → full rebuild (and the fallback's
    // tables are correct: the fifth line appears).
    // synced is 12 after the rebuild ("ab\ncd\nef\ngh\n"); wm 0 < 12.
    c.sync("ab\ncd\nef\ngh\nij\n", 1, 8, 20, false, 0);
    assert_eq!(c.rebuilds(), 7);
    assert_eq!(c.total_lines(), 5);
    // Healthy watermark → incremental again (2nd append overall).
    c.sync("ab\ncd\nef\ngh\nij\nkl\n", 1, 9, 20, false, 15);
    assert_eq!((c.rebuilds(), c.appends()), (7, 2));
    assert_eq!(c.total_lines(), 6);
    // output shrink (len < synced) → full rebuild.
    c.sync("ab\n", 1, 10, 20, false, usize::MAX);
    assert_eq!(c.rebuilds(), 8);
    assert_eq!(c.total_lines(), 1);
    // Growing the unterminated tail after that rebuild is a legitimate
    // append (boundary at the partial's start, canary byte intact).
    c.sync("ab\nYZX", 1, 11, 20, false, usize::MAX);
    assert_eq!((c.rebuilds(), c.appends()), (8, 3));
    assert_eq!(c.total_lines(), 2);
    assert_eq!(c.line_range(1), (3, 6));
    // Canary failure: the doc changed IN PLACE below the stale boundary
    // (bytes[2] is no longer the '\n' the boundary sits after) — the
    // depth-on-defense check must reject the fast path and full-rebuild.
    c.sync("abXYZ\n", 1, 12, 20, false, usize::MAX);
    assert_eq!(c.rebuilds(), 9, "canary failure must force full rebuild");
    assert_eq!(c.total_lines(), 1);
    let (s, e) = c.line_range(0);
    assert_eq!(&"abXYZ\n"[s..e], "abXYZ");
}

/// Dual-consumer timing (plan review 2 P1): the first consumer takes the
/// REAL watermark and consumes (append or full), the second re-syncing the
/// same version early-exits on the key — in BOTH orders, with identical
/// outcomes.
#[test]
fn dual_consumer_watermark_timing_is_order_independent() {
    let mut c = LiveLayoutCache::default();
    c.sync("ab\ncd\n", 1, 1, 10, false, usize::MAX);
    assert_eq!(c.rebuilds(), 1);

    // Frame A — metrics path first: real watermark consumed by the append.
    c.sync("ab\ncd\nef\n", 1, 2, 10, false, 6);
    assert_eq!((c.rebuilds(), c.appends()), (1, 1));
    // Layout pass re-syncs the SAME version (its take already reset the
    // capture watermark to MAX): key hit, guard untouched, counters idle.
    c.sync("ab\ncd\nef\n", 1, 2, 10, false, usize::MAX);
    assert_eq!((c.rebuilds(), c.appends()), (1, 1));

    // Frame B — layout pass first: the append happens on the first call.
    c.sync("ab\ncd\nef\ngh\n", 1, 3, 10, false, 9);
    assert_eq!((c.rebuilds(), c.appends()), (1, 2));
    // Metrics path then hits the same-version key.
    c.sync("ab\ncd\nef\ngh\n", 1, 3, 10, false, usize::MAX);
    assert_eq!((c.rebuilds(), c.appends()), (1, 2));
}

// ── M6-a: rewrite-stream fuzz vs forced-full oracle ─────────────────

/// Deterministic PRNG (repo XorShift convention, grid_cache/visual_rows_tests.rs).
struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Random op sequences through the REAL `BlockTracker` (print/newline/CR/
/// BS/EL, CSI A/B via `on_move_cursor_rows` ±, CHA/CUB/CUF via
/// `on_set_cursor_column`/`on_move_cursor_columns`, `on_print_ascii_run`
/// batches, screen-origin flip via `begin_screen_owned_output` +
/// `replace_screen_snapshot`, command boundaries via
/// `on_command_start`/`on_command_end`), including the multi-line
/// progress-bar repaint shape (mid-stream shorten + tail append) and
/// multibyte content. After every op the incremental sync must be
/// value-identical to a forced-full oracle.
///
/// (`OutputCapture::goto` has no production path into the live block's
/// capture — CSI H/f is only mirrored into the separate screen-exit tail
/// capture — so its watermark accounting is pinned by the dedicated
/// weft_core unit tests instead.)
#[test]
fn rewrite_stream_fuzz_matches_full_rebuild_oracle() {
    const COLS: usize = 24;
    const PANE: u64 = 7;
    const ROUNDS: u64 = 600;
    let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
    let chars = ['a', 'b', 'x', ' ', '你', '好', '中'];
    let mut tracker = BlockTracker::new();
    tracker.on_prompt_start();
    tracker.on_command_start("fuzz".to_string());
    let mut cache = LiveLayoutCache::default();
    let mut took_append = false;
    let mut took_rebuild = false;

    for round in 0..ROUNDS {
        match rng.below(20) {
            0..=6 => {
                let n = rng.below(3) + 1;
                for _ in 0..n {
                    let ch = chars[rng.below(chars.len() as u64) as usize];
                    tracker.on_print(ch, CapturedStyle::default());
                }
            }
            7 => tracker.on_newline(),
            8 => tracker.on_carriage_return(),
            9 => tracker.on_backspace(),
            10 => tracker.on_erase_line(rng.below(3) as u16),
            11 => {
                let d = rng.below(4) as isize + 1;
                if rng.below(2) == 0 {
                    tracker.on_move_cursor_rows(-d);
                } else {
                    tracker.on_move_cursor_rows(d);
                }
            }
            12 => tracker.on_set_cursor_column(rng.below(20) as usize),
            13 => tracker.on_move_cursor_columns(rng.below(10) as isize - 5),
            14 => tracker.on_print_ascii_run(b"progress 42%", CapturedStyle::default()),
            // Multi-line progress-bar repaint: up k rows, CR, shorter
            // reprint, EL — the `ollama pull` shape that must fall back to
            // a full rebuild via the watermark and stay correct.
            15 => {
                let k = rng.below(3) + 1;
                tracker.on_move_cursor_rows(-(k as isize));
                tracker.on_carriage_return();
                for ch in "50% done".chars() {
                    tracker.on_print(ch, CapturedStyle::default());
                }
                tracker.on_erase_line(0);
            }
            // Mid-stream shorten + tail append (progress redraw + new
            // output in one burst).
            16 => {
                tracker.on_move_cursor_rows(-1);
                tracker.on_carriage_return();
                tracker.on_print_ascii_run(b"up", CapturedStyle::default());
                tracker.on_erase_line(0);
                tracker.on_move_cursor_rows(1);
                tracker.on_carriage_return();
                tracker.on_print_ascii_run(b"tail-appended", CapturedStyle::default());
            }
            // Screen-origin flip + snapshot replace + command boundary.
            17 => {
                tracker.begin_screen_owned_output(0);
                let styled = StyledOutput { lines: Vec::new() };
                tracker.replace_screen_snapshot("frame A\nframe B\n", styled.clone());
                tracker.replace_screen_snapshot("frame C\n", styled);
                tracker.on_command_end(0);
                tracker.on_prompt_start();
                tracker.on_command_start(format!("tui {round}"));
            }
            // Command boundary.
            _ => {
                tracker.on_command_end(0);
                tracker.on_prompt_start();
                tracker.on_command_start(format!("cmd {round}"));
            }
        }

        let live = tracker.in_flight().expect("capturing after every branch");
        let output = live.output;
        let version = live.version;
        let screen_origin = live.screen_origin;
        let watermark = live.take_min_write_offset();
        cache.sync(output, PANE, version, COLS, screen_origin, watermark);
        // Oracle: the pane key changes every step, so its guard chain can
        // never take the append path — a forced full rebuild.
        let mut oracle = LiveLayoutCache::default();
        oracle.sync(
            output,
            1_000_000 + round,
            version,
            COLS,
            screen_origin,
            usize::MAX,
        );

        let label = format!("round {round}");
        assert_eq!(cache.total_lines(), oracle.total_lines(), "{label}");
        assert_eq!(cache.base_idx(), oracle.base_idx(), "{label}");
        assert_eq!(
            cache.total_display_rows(),
            oracle.total_display_rows(),
            "{label}"
        );
        assert_eq!(cache.cumulative(), oracle.cumulative(), "{label}");
        assert_eq!(
            cache.visible_window(20.0, -40.0, 160.0, 64.0),
            oracle.visible_window(20.0, -40.0, 160.0, 64.0),
            "{label}"
        );
        for i in 0..cache.total_lines() {
            let (s, e) = cache.line_range(i);
            let (os, oe) = oracle.line_range(i);
            assert_eq!((s, e), (os, oe), "{label} range {i}");
            assert_eq!(&output[s..e], &output[os..oe], "{label} text {i}");
        }
        // The first sync is a full rebuild (fresh pane key); afterwards the
        // mix of rewrites and appends must exercise both paths.
        if round > 0 {
            took_append |= cache.appends() > 0;
            took_rebuild |= cache.rebuilds() > 1;
        }
    }
    assert!(took_append, "fuzz never exercised the append fast path");
    assert!(took_rebuild, "fuzz never exercised the rebuild fallback");
}
