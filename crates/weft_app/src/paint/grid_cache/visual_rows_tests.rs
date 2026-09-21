//! Tests for `visual_rows` (L1/L2 tables + shared wrap machine).
//! G4 property equivalence: the table-fed machine must match the legacy
//! text-fed path byte-for-byte (PLAN_M5 §五 M5-a 验收门).

use super::super::wrapping::{block_line_chunk_ranges, screen_origin_line_chunk_ranges};
use super::*;
use crate::block_component::{command_resume_hints, completed_block_output_rows};
use crate::paint::grid_cache::{block_line_chunks, BlockLayoutCache};
use weft_core::blocks::{Block, BlockId};

/// INDEPENDENT text-path oracle for the row-count equivalence (M5-b: the
/// production `completed_block_output_rows` itself migrated onto the L2
/// tables, so the oracle here sums the legacy text-fed chunk ranges
/// directly — hints via `block_line_chunks`, lines via
/// `block_line_chunk_ranges`).
fn legacy_output_rows(block: &Block, cols: usize) -> usize {
    if block.collapsed {
        return 0;
    }
    let hints: usize = command_resume_hints(block)
        .iter()
        .map(|hint| block_line_chunks(hint, cols).count())
        .sum();
    let mut lines: Vec<&str> = block.output.lines().collect();
    let len = trimmed_output_line_count(&lines);
    lines.truncate(len);
    hints
        + lines
            .iter()
            .map(|line| block_line_chunk_ranges(line, cols).len())
            .sum::<usize>()
}

fn mk_block(command: &str, output: &str, screen_origin: bool, exit_code: Option<i32>) -> Block {
    Block {
        id: BlockId(1),
        command: command.to_string(),
        cwd: None,
        output: output.into(),
        styled_output: None,
        exit_code,
        started_at: std::time::SystemTime::UNIX_EPOCH,
        finished_at: None,
        collapsed: false,
        screen_origin,
    }
}

fn row_ranges(l1: &ContentTable, rows: &[VisualRow]) -> Vec<Range<usize>> {
    rows.iter()
        .map(|r| {
            let start = l1.graphemes[r.g_start as usize].byte_offset as usize;
            start..start + r.byte_len as usize
        })
        .collect()
}

// ── G4: property equivalence vs the legacy (text-fed) path ─────────

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

/// Explicitly enumerated hard cases: CJK, emoji modifier, flag, ZWJ
/// cluster, tab, whitespace runs, ZWSP (zero-width non-ws), multi-byte
/// narrow cluster, box-drawing fragments (below classification
/// thresholds), plain ASCII.
const TOKENS: &[&str] = &[
    "中",
    "文",
    "数据",
    "ab",
    "word",
    "x",
    " ",
    "  ",
    "\t",
    "👍🏽",
    "🇨🇳",
    "👩‍🔬",
    "é",
    "\u{200b}",
    "▕",
    "█",
    "│",
    "─",
];

fn random_line(rng: &mut XorShift) -> String {
    let count = 1 + rng.below(14) as usize;
    let mut line = String::new();
    for _ in 0..count {
        line.push_str(TOKENS[rng.below(TOKENS.len() as u64) as usize]);
    }
    // 尾随空白吸收:explicit trailing whitespace tail.
    match rng.below(4) {
        0 => line.push_str("   "),
        1 => line.push('\t'),
        _ => {}
    }
    line
}

/// Structure-line injection: PureBox / ProgressGauge / TableRow.
fn structure_line(kind: u64, rng: &mut XorShift) -> String {
    match kind % 3 {
        0 => "─".repeat(8 + rng.below(30) as usize),
        1 => format!(
            "▕{}▏ pulling d4b8b4f4c350  {}%",
            "█".repeat(6 + rng.below(12) as usize),
            rng.below(100)
        ),
        _ => format!("│ {} │ Data {}G │ 充裕 │", rng.below(100), rng.below(900)),
    }
}

/// Cols sweep includes the degenerate 0 (whole-line branch) and 1
/// (single-cluster-wider-than-cols guard) tiers.
const COLS_CASES: [usize; 7] = [0, 1, 2, 3, 7, 80, 180];

#[test]
fn property_l2_matches_legacy_wrap_across_cols_structures_and_origin() {
    let mut rng = XorShift(0x243F_6A88_85A3_08D3);
    for trial in 0..64u64 {
        let line_count = 3 + rng.below(4) as usize;
        let structure_at = rng.below(line_count as u64) as usize;
        let mut lines: Vec<String> = (0..line_count)
            .map(|i| {
                if i == structure_at {
                    structure_line(trial, &mut rng)
                } else {
                    random_line(&mut rng)
                }
            })
            .collect();
        // Trailing junk exercises the shared trim.
        lines.push(String::new());
        lines.push("%".to_string());
        let output = lines.join("\n");
        let raw: Vec<&str> = output.lines().collect();
        let trimmed = trimmed_output_line_count(&raw);
        let surviving: Vec<&str> = raw[..trimmed].to_vec();

        for screen_origin in [false, true] {
            let block = mk_block("echo", &output, screen_origin, Some(0));
            let l1 = build_content_table(&output, screen_origin);
            assert_eq!(l1.line_meta.len(), trimmed, "trial {trial}");
            assert_eq!(l1.trailing_trim_lines as usize, raw.len() - trimmed);
            assert_eq!(
                l1.line_meta
                    .iter()
                    .map(|m| m.grapheme_len as usize)
                    .sum::<usize>(),
                l1.graphemes.len(),
                "cluster windows must tile the table"
            );
            for (i, line) in surviving.iter().enumerate() {
                let meta = &l1.line_meta[i];
                assert_eq!(meta.char_count as usize, line.chars().count());
                assert_eq!(meta.grapheme_len as usize, line.graphemes(true).count());
                assert_eq!(meta.byte_len as usize, line.len());
            }
            for cols in COLS_CASES {
                let l2 = build_width_table(&l1, &[], cols);
                assert_eq!(
                    l2.line_row_base.len(),
                    l1.line_meta.len() + 1,
                    "trial {trial} cols {cols}"
                );
                assert_eq!(l2.line_row_base[0], 0);
                assert_eq!(
                    l2.line_row_base.last().copied().map(|v| v as usize),
                    Some(l2.rows.len())
                );
                // Row-count equivalence: L2 == legacy oracle. For
                // screen-origin blocks the cache (and L2) count rows
                // with the screen-origin policy, so the oracle is the
                // per-line screen-origin chunk sum.
                let legacy_total: usize = surviving
                    .iter()
                    .map(|line| {
                        if screen_origin {
                            screen_origin_line_chunk_ranges(line, cols).len()
                        } else {
                            block_line_chunk_ranges(line, cols).len()
                        }
                    })
                    .sum();
                assert_eq!(
                    l2.rows.len(),
                    legacy_total,
                    "trial {trial} cols {cols} screen_origin {screen_origin}"
                );
                if !screen_origin {
                    assert_eq!(
                        l2.hint_rows as usize + l2.rows.len(),
                        legacy_output_rows(&block, cols),
                        "trial {trial} cols {cols}: hint_rows + rows.len() must equal the
                             independent text-path oracle"
                    );
                }
                // Per-visual-row byte intervals == legacy chunk bounds.
                for (i, line) in surviving.iter().enumerate() {
                    let legacy = if screen_origin {
                        screen_origin_line_chunk_ranges(line, cols)
                    } else {
                        block_line_chunk_ranges(line, cols)
                    };
                    let base = l2.line_row_base[i] as usize;
                    let end = l2.line_row_base[i + 1] as usize;
                    let rows = &l2.rows[base..end];
                    let ranges = row_ranges(&l1, rows);
                    assert_eq!(
                        ranges, legacy,
                        "trial {trial} line {i} cols {cols}: row byte intervals must equal
                             the legacy chunk boundaries"
                    );
                    let meta = &l1.line_meta[i];
                    let prose = !screen_origin && meta.structure == StructureKind::None;
                    if prose {
                        // Prose rows concatenate back to the source line.
                        let joined: String = ranges
                            .iter()
                            .map(|r| line[r.start..r.end].to_string())
                            .collect();
                        assert_eq!(joined, *line, "trial {trial} line {i} cols {cols}");
                        // Continuation rows never start with whitespace.
                        for r in rows.iter().skip(1) {
                            assert!(
                                l1.graphemes[r.g_start as usize].flags & IS_WHITESPACE == 0,
                                "trial {trial} line {i} cols {cols}: continuation row must
                                     not start with a whitespace cluster"
                            );
                        }
                    }
                    if screen_origin
                        || matches!(
                            meta.structure,
                            StructureKind::PureBox | StructureKind::TableRow
                        )
                    {
                        // Clip lines: exactly one row, width ≤ cols
                        // (cols == 0 keeps the whole line by design).
                        assert_eq!(rows.len(), 1, "trial {trial} line {i} cols {cols}");
                        if cols > 0 {
                            let r = &ranges[0];
                            let width = weft_core::grid::terminal_text_width(&line[r.start..r.end]);
                            assert!(
                                width <= cols,
                                "trial {trial} line {i} cols {cols}: clipped row width
                                     {width} must fit"
                            );
                        }
                    }
                }
            }
        }
    }
}

/// Hand-picked regressions from wrapping.rs' word-aware suite, pushed
/// through the L2 path at the sweep cols — deterministic anchor next to
/// the random property above.
#[test]
fn l2_rows_match_legacy_on_word_aware_regressions() {
    for line in [
        "zz aaaaaaa  XY",
        "abcdefghi   ",
        "中文命令测试数据",
        "A👩‍🔬B",
        "ab\u{200b}cdef",
        "A  B  C  skills  USER.md",
    ] {
        let l1 = build_content_table(line, false);
        assert_eq!(l1.line_meta.len(), 1);
        for cols in COLS_CASES {
            let l2 = build_width_table(&l1, &[], cols);
            let legacy = block_line_chunk_ranges(line, cols);
            assert_eq!(l2.rows.len(), legacy.len(), "line {line:?} cols {cols}");
            let ranges = row_ranges(&l1, &l2.rows);
            assert_eq!(ranges, legacy, "line {line:?} cols {cols}");
            let joined: String = ranges
                .iter()
                .map(|r| line[r.start..r.end].to_string())
                .collect();
            assert_eq!(joined, line);
        }
    }
}

// ── L1 unit behaviour ───────────────────────────────────────────────

#[test]
fn l1_grapheme_entries_record_width_and_flags() {
    let line = "中\t ab🇨🇳👍🏽👩‍🔬";
    let l1 = build_content_table(line, false);
    let entries = &l1.graphemes;
    let meta = &l1.line_meta[0];
    assert_eq!(
        meta.grapheme_len as usize, 8,
        "中 tab sp a b flag thumbs scientist"
    );
    assert_eq!(meta.char_count as usize, 12);
    let widths: Vec<u8> = entries.iter().map(|e| e.width).collect();
    assert_eq!(widths, [2, 1, 1, 1, 1, 2, 2, 2]);
    assert_eq!(entries[0].flags, 0);
    assert_eq!(entries[1].flags, IS_WHITESPACE | IS_TAB_ZERO_W);
    assert_eq!(entries[2].flags, IS_WHITESPACE);
    assert_eq!(entries[5].flags, 0, "🇨🇳 is one non-ws cluster (flag emoji)");
    assert_eq!(entries[6].flags, 0, "👍🏽 is one cluster (emoji + modifier)");
    assert_eq!(entries[7].flags, 0, "👩‍🔬 is one cluster (ZWJ sequence)");
    // Cluster byte offsets tile the line at grapheme boundaries.
    let mut offset = 0usize;
    for (entry, grapheme) in entries.iter().zip(line.graphemes(true)) {
        assert_eq!(entry.byte_offset as usize, offset);
        offset += grapheme.len();
    }
    assert_eq!(meta.byte_len as usize, offset);
}

#[test]
fn l1_applies_the_shared_trailing_trim() {
    let l1 = build_content_table("first\n\nsecond\n%\n$\n#\n", false);
    assert_eq!(l1.line_meta.len(), 3, "only surviving lines get metadata");
    assert_eq!(l1.trailing_trim_lines, 3);
    let l1_empty = build_content_table("\n\n\n", false);
    assert!(l1_empty.line_meta.is_empty());
    assert_eq!(l1_empty.trailing_trim_lines, 3);
}

// ── L2 / wiring equivalence ─────────────────────────────────────────

#[test]
fn resume_hints_are_counted_by_the_same_machine() {
    let long_line = "a much longer output line that definitely wraps when the window is narrow";
    let block = mk_block(
        "opencode",
        &format!("short line\n{long_line}\n"),
        false,
        Some(1),
    );
    assert!(
        !command_resume_hints(&block).is_empty(),
        "test setup: a failed opencode command must yield resume hints"
    );
    for cols in COLS_CASES {
        let l1 = build_content_table(&block.output, false);
        let l2 = build_width_table(&l1, command_resume_hints(&block), cols);
        let legacy_hints: usize = command_resume_hints(&block)
            .iter()
            .map(|hint| block_line_chunks(hint, cols).count())
            .sum();
        assert_eq!(l2.hint_rows as usize, legacy_hints, "cols {cols}");
        assert_eq!(
            l2.hint_rows as usize + l2.rows.len(),
            legacy_output_rows(&block, cols),
            "cols {cols}: hint_rows must be included in the output-row read"
        );
        assert_eq!(
            completed_output_rows(&block, cols, None),
            legacy_output_rows(&block, cols),
            "cols {cols}: the metrics fallback must equal the legacy oracle"
        );
    }
}

#[test]
fn completed_output_rows_matches_legacy_and_collapses_to_zero() {
    let output = "ordinary prose line\n中文中文中文中文中文\n██████████ gauge-ish\n";
    for screen_origin in [false, true] {
        let block = mk_block("echo", output, screen_origin, Some(0));
        let rows = completed_output_rows(&block, 80, None);
        if screen_origin {
            // Screen-origin rows follow the cache semantics: one row per line.
            assert_eq!(rows, output.lines().count());
        } else {
            assert_eq!(rows, legacy_output_rows(&block, 80));
        }
        let mut collapsed = block.clone();
        collapsed.collapsed = true;
        assert_eq!(completed_output_rows(&collapsed, 80, None), 0);
    }
}

#[test]
fn screen_origin_block_rows_are_one_per_line() {
    let output = "[| wide tui frame row over any layout width |]\nshort\n";
    let l1 = build_content_table(output, true);
    assert!(l1.screen_origin);
    for cols in COLS_CASES {
        let l2 = build_width_table(&l1, &[], cols);
        assert_eq!(l2.rows.len(), 2, "cols {cols}");
        for (i, line) in output.lines().enumerate() {
            let base = l2.line_row_base[i] as usize;
            let end = l2.line_row_base[i + 1] as usize;
            assert_eq!(end - base, 1, "cols {cols} line {i}");
            let ranges = row_ranges(&l1, &l2.rows[base..end]);
            assert_eq!(
                ranges,
                screen_origin_line_chunk_ranges(line, cols),
                "cols {cols} line {i}"
            );
        }
    }
}

#[test]
fn blank_interior_line_takes_the_empty_row_branch_across_cols() {
    // rust-reviewer M5-a P2-2: the `entries.is_empty()` payload branch in
    // build_width_table guards the empty-slice indexing for interior blank
    // lines ("a\n\nb" — trim never removes those). Deleting the branch must
    // turn this red (entries[cursor] would panic on the empty slice).
    let output = "a\n\nb\n";
    let l1 = build_content_table(output, false);
    assert_eq!(
        l1.line_meta.len(),
        3,
        "interior blank line is a surviving line"
    );
    assert_eq!(
        l1.line_meta[1].grapheme_len, 0,
        "blank line carries no graphemes"
    );
    for cols in COLS_CASES {
        let l2 = build_width_table(&l1, &[], cols);
        let legacy: Vec<Range<usize>> = output
            .lines()
            .flat_map(|l| block_line_chunk_ranges(l, cols))
            .collect();
        let ranges = row_ranges(&l1, &l2.rows);
        assert_eq!(
            ranges, legacy,
            "cols {cols}: empty-row branch must match legacy"
        );
        // The blank line owns exactly one empty visual row at legacy position.
        let base = l2.line_row_base[1] as usize;
        assert_eq!(l2.rows[base].byte_len, 0, "blank line owns one empty row");
    }
}

#[test]
fn metrics_fallback_reads_stored_tables_without_drift() {
    use crate::paint::grid_cache::BlockLayoutCache;
    let output = "prose line that eventually wraps when cols shrink enough\n\
                      ────────────────────────────\n\
                      中文中文中文中文中文中文中文中文\n";
    let block = mk_block("echo", output, false, Some(0));
    let mut cache = BlockLayoutCache::default();
    cache.ensure_cached(&block, 80);

    // Cols-fresh, block still expanded: stored-L2 read == the entry's own count.
    assert_eq!(
        completed_output_rows(&block, 80, Some(cache.get(block.id.0))),
        cache.get(block.id.0).width.hint_rows as usize + cache.get(block.id.0).width.rows.len(),
        "fresh entry: width tables must agree with output rows"
    );
    // Cols-fresh + collapsed mismatch: O(1) stored-L2 read, gated to 0.
    let mut collapsed = block.clone();
    collapsed.collapsed = true;
    assert_eq!(
        completed_output_rows(&collapsed, 80, Some(cache.get(block.id.0))),
        0
    );
    // Cols-miss: L2-only rebuild from the stored L1.
    assert_eq!(
        completed_output_rows(&block, 20, Some(cache.get(block.id.0))),
        legacy_output_rows(&block, 20),
        "cols-miss: stored-L1 rebuild must equal the legacy oracle"
    );
    // Stale identity (output grew): full rebuild, still exact.
    let grown = mk_block("echo", &format!("{output}more\n"), false, Some(0));
    assert_eq!(
        completed_output_rows(&grown, 20, Some(cache.get(block.id.0))),
        legacy_output_rows(&grown, 20),
        "stale-identity entry must fall through to a full table build"
    );
}

#[test]
fn migrated_completed_block_output_rows_matches_text_oracle() {
    // M5-b migration equivalence: the production row-count (now L2-fed)
    // must equal the independent text-path oracle across the cols sweep,
    // including a resume-hint block and a screen-origin block.
    for (command, output, exit_code, screen_origin) in [
        (
            "echo",
            "prose line one\nwrapped prose line that keeps going and going on\n",
            Some(0),
            false,
        ),
        (
            "echo",
            "─── pure box ───\n中文中文中文中文中文\n",
            Some(0),
            false,
        ),
        ("echo", "[| tui frame |]\nsecond row\n", Some(0), true),
        ("opencode", "failed session output line\n", Some(1), false),
    ] {
        let block = mk_block(command, output, screen_origin, exit_code);
        // Screen-origin rows follow the screen-origin clip policy (the M5-a
        // recorded deviation: the v1.12.2 fallback soft-wrapped there and
        // disagreed with the cache; the L2-based count matches the cache).
        let raw: Vec<&str> = block.output.lines().collect();
        let trimmed = trimmed_output_line_count(&raw);
        for cols in COLS_CASES {
            let hints: usize = command_resume_hints(&block)
                .iter()
                .map(|hint| block_line_chunks(hint, cols).count())
                .sum();
            let oracle = if screen_origin {
                hints
                    + raw[..trimmed]
                        .iter()
                        .map(|line| screen_origin_line_chunk_ranges(line, cols).len())
                        .sum::<usize>()
            } else {
                hints
                    + raw[..trimmed]
                        .iter()
                        .map(|line| block_line_chunk_ranges(line, cols).len())
                        .sum::<usize>()
            };
            assert_eq!(
                completed_block_output_rows(&block, cols),
                oracle,
                "command {command} cols {cols} origin {screen_origin}"
            );
        }
    }
}

#[test]
fn collapse_toggle_reuses_l1_and_rebuilds_width_only() {
    // M5-b P2-2: a collapse/expand toggle must take the WidthOnly rebuild
    // path — the stored L1 table (per-cluster, the expensive enumeration)
    // is preserved untouched, and only the cols/collapsed-derived fields
    // are recomputed. Guards the three toggle call sites' no-invalidate
    // contract (mouse_controller / mouse_press_controller / find_controller).
    let mut cache = BlockLayoutCache::default();
    let block = mk_block("echo", "hello wrapped world line\nsecond\n", false, Some(0));
    cache.ensure_cached(&block, 80);
    let content_len = cache.get(block.id.0).content.graphemes.len();
    let rows_before = cache.get(block.id.0).width.rows.len();

    // Collapse: WidthOnly — no L1 re-enumeration, L2 stays populated
    // (collapse gates READS, not the table), row count reads 0.
    //
    // `content_builds()` counts THIS THREAD's builds only (see the
    // `CONTENT_BUILDS` thread-local in visual_rows.rs): the counter used to
    // be a process global, and any concurrently-running test's cold build
    // landing inside the read→toggle→read window false-failed this pin.
    let mut folded = block.clone();
    folded.collapsed = true;
    let builds_before = content_builds();
    cache.ensure_cached(&folded, 80);
    assert_eq!(
        content_builds(),
        builds_before,
        "collapse toggle must not rebuild L1"
    );
    assert_eq!(
        cache.get(block.id.0).content.graphemes.len(),
        content_len,
        "the L1 cluster table must be preserved by the move"
    );
    assert_eq!(cache.get(block.id.0).width.rows.len(), rows_before);
    assert_eq!(
        completed_output_rows(&folded, 80, Some(cache.get(block.id.0))),
        0
    );
    assert_eq!(
        cache.get(block.id.0).base_row_count,
        2,
        "collapsed base = cmd + sep"
    );

    // Expand: still WidthOnly, rows readable again.
    let builds_before = content_builds();
    cache.ensure_cached(&block, 80);
    assert_eq!(
        content_builds(),
        builds_before,
        "expand toggle must not rebuild L1"
    );
    assert_eq!(
        completed_output_rows(&block, 80, Some(cache.get(block.id.0))),
        2,
        "two fitting lines → two output rows"
    );
}
