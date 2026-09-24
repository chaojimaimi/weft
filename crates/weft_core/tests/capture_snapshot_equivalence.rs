//! PLAN_B Phase 0 — capture/snapshot dual-truth differential equivalence
//! (docs/PLAN_B_phase0.md P0-2).
//!
//! Two sources of truth exist for a command's text:
//! 1. **capture** — the block tracker's `Block.output` (OSC 133;B → 133;D
//!    print stream, finalized with trim_end + truncation marker);
//! 2. **snapshot** — the Grid document walk behind
//!    `Terminal::primary_screen_document_snapshot` (here exercised through
//!    the pub `Grid::document_snapshot_from_position_with_resolver`, the
//!    façade's only non-masked Grid call).
//!
//! # Anchor rule (plain channel, review P1 revision)
//!
//! Every scenario feeds bytes in three segments: `\x1b]133;A\x07\x1b]133;B\x07`
//! → scenario output → `\x1b]133;D;0\x07`. The anchor is sampled IMMEDIATELY
//! after the 133;B bytes are processed: `grid().scrollback.position()`. The
//! snapshot side is `document_snapshot_from_position_with_resolver(anchor,…)`;
//! the capture side is the last finalized block's `output`. The FIRST scenario
//! pins the off-by-one: 133;B directly followed by 133;D must yield EMPTY text
//! on both sides.
//!
//! # Line-domain canonical form (D1 normalization, review P2 revision)
//!
//! The snapshot product is physical rows; the capture product is logical
//! lines. Both sides are reduced to a canonical logical-line sequence before
//! comparison:
//! 1. snapshot side: the document range is walked row-domain
//!    (`grid.scrollback.get` with `Row.wrapped` for retained rows,
//!    `displayed_row_text`/`displayed_row_wrapped` for the viewport) and
//!    soft wraps are reconnected into logical lines. This walk uses the
//!    same range computation and row-extraction semantics as the façade
//!    (`styled_row`/`row_text`), so the resulting logical sequence is the
//!    façade's text re-expressed in the line domain.
//! 2. capture side: `Block.output` split on `'\n'` (already logical).
//! 3. Both: trim_end every logical line, drop leading/trailing empty
//!    logical lines, compare sequences.
//!
//! # Divergence catalog (裁定记录, PLAN_B_phase0 分歧类目录)
//!
//! | id | condition | verdict | Phase 1 semantics pointer | dynamic result |
//! |----|-----------|---------|---------------------------|----------------|
//! | D1 | soft wrap: snapshot = physical rows, capture = logical lines | 设计内 | line-domain canonical normalization | EQUAL (s03, s13) |
//! | D2 | empty-line handling: snapshot drops edge empties, keeps internal 1:1 per physical row | 设计内 — with an internal-empty ACCOUNTING subclass: when a capture mirror folds a physical row (see D9), the canonical form cannot restore it | canonical form strips edge empties both sides only | EQUAL for pure-LF streams (s04); internal-empty divergences are catalog D9 (s14) |
//! | D3 | CSI 2J / 3J / ECH `X` have no capture mirror | 真分歧待裁 | Phase 1 decides: mirror or exemption | EXPECTED DIFFERENCE (s05, s06, s07) — capture keeps cleared content |
//! | D4 | wide-char cell layout: grid +2 cols, capture +1 char; grid wraps whole glyphs at line end, capture never wraps | 预期等价 after canonical form | none needed if equal | EQUAL (s08) |
//! | D5 | orphan combining scalar: grid drops (no base cell), capture keeps | 设计内 (directional) | capture keeps the mark the grid drops | EXPECTED DIFFERENCE (s09) |
//! | D6 | hyperlink / SGR styles | no text channel on capture | pure-text comparison rules them out | (no text scenario — styles never change text) |
//! | D7 | 1 MiB budget: capture appends `\n…(block excerpt truncated at 1 MiB — full output remains in scrollback)`, snapshot appends a single space + drops styles | 设计内 | budget overflow is catalog-specific | EXPECTED DIFFERENCE (s10) |
//! | D8 | absolute addressing / edit ops (CUP `H`/`f`, ICH/DCH/IL/DL/SU/SD `@ P L M S T`) have no capture mirror | 设计内 | screen-owned channel exists for exactly this | EXPECTED DIFFERENCE (s12) |
//! | D9 | vertical moves (CUU/CUD) mirror onto the capture WITHOUT writing the rows they cross, so the capture folds physical blank rows the snapshot keeps | 设计内/待裁 (Phase 1: either the capture materializes folded rows, or the snapshot walk folds empty rows for plain commands) | the D2 canonical normalization does NOT absorb internal empties | EXPECTED DIFFERENCE (s14) |
//! | D4b | wide-glyph overwrite: a glyph landing on a pair's lead blanks the orphaned spacer in the grid while the capture keeps the overwritten char | 设计内 (D4 cell-model subclass) | no capture channel for pair cleanup | fuzz-verified (fuzz_lite predicate ⑧, seed 11916113683178599450) |
//! | D10 | TAB/positional fills: the capture writes a space per advanced column (destructive) while the grid TAB/CUF only move | 设计内 | capture positional mirrors over-write; grid moves don't | fuzz-verified (fuzz_lite predicate ⑨) |
//! | —  | alt-screen excursion: prints during DEC 1049 go to neither the capture (`!alt_active` gate) nor the primary grid | 设计内边界 | both truths drop the window symmetrically | EQUAL + both sides lack the alt content (s11) |
//!
//! D4 (s08), D5 (s09) and the alt boundary (s11) are the plan's three
//! 〔动态验证〕items: their verdicts above were produced by RUNNING the
//! scenarios below, not by desk inspection.

use weft_core::grid::{CellFlags, Row};
use weft_core::vt::Terminal;

/// Capture-side truncation marker appended by `OutputCapture::take_styled`
/// (PLAN_v11217 §3.5 T4 copy: names the configured cap and clarifies the full
/// output remains in scrollback — this constant pins the DEFAULT-cap form).
const TRUNCATION_MARKER: &str =
    "\n…(block excerpt truncated at 1 MiB — full output remains in scrollback)";

/// 1 MiB capture budget — mirrors `blocks::DEFAULT_OUTPUT_CAP` (pub(crate),
/// hence the local copy). MUST stay equal to `CAPTURE_BUDGET` in
/// `tests/fuzz_lite.rs`; the s10 scenario below pins the truncated capture
/// to EXACTLY `CAPTURE_BUDGET + TRUNCATION_MARKER.len()` bytes, so drift
/// breaks that assertion loudly. Aliased to the production constant (T4
/// review P3: a hardcoded mirror could drift from a raised configured cap).
use weft_core::blocks::DEFAULT_OUTPUT_CAP as CAPTURE_BUDGET;

// ── Normalization (line-domain canonical form) ─────────────────────────

/// trim_end every line, then drop leading/trailing empty lines.
fn normalize(mut lines: Vec<String>) -> Vec<String> {
    for line in &mut lines {
        *line = line.trim_end().to_string();
    }
    while lines.first().is_some_and(|line| line.is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}

/// Capture truth: `Block.output` is already a logical-line sequence.
fn capture_lines(text: &str) -> Vec<String> {
    normalize(text.split('\n').map(str::to_string).collect())
}

/// Text of one physical row, mirroring `Grid::row_text` /
/// `snapshot_row_extent` semantics for an arbitrary row (skip WIDE_SPACER,
/// contribute EXTRA grapheme clusters, trim trailing blank/default cells).
fn physical_row_text(row: &Row, cols: usize) -> String {
    let last = row
        .cells
        .iter()
        .take(cols)
        .rposition(|cell| cell.character != ' ' && cell.character != '\0')
        .map_or(0, |index| index + 1);
    let mut out = String::new();
    for (col, cell) in row.cells.iter().take(last).enumerate() {
        if cell.flags.contains(CellFlags::WIDE_SPACER) {
            continue;
        }
        if cell.flags.contains(CellFlags::EXTRA) {
            if let Some(cluster) = row.extras.grapheme_at(col) {
                out.push_str(cluster);
                continue;
            }
        }
        out.push(if cell.character == '\0' {
            ' '
        } else {
            cell.character
        });
    }
    out
}

/// Snapshot truth in the line domain: same document range as
/// `document_snapshot_from_position_with_resolver`, walked per physical row
/// and reconnected through the wrapped flags into logical lines.
fn snapshot_logical_lines(terminal: &Terminal, anchor: u64) -> Vec<String> {
    let grid = terminal.grid();
    let viewport_origin = grid.scrollback.position();
    let (scrollback_start, viewport_start) = if anchor <= viewport_origin {
        (grid.scrollback.index_since(anchor), 0)
    } else {
        (
            grid.scrollback.len(),
            anchor.saturating_sub(viewport_origin) as usize,
        )
    };
    let mut physical: Vec<(String, bool)> = Vec::new();
    for index in scrollback_start..grid.scrollback.len() {
        if let Some(row) = grid.scrollback.get(index) {
            physical.push((physical_row_text(&row, grid.num_cols), row.wrapped));
        }
    }
    for index in viewport_start..grid.num_rows {
        physical.push((
            grid.displayed_row_text(index),
            grid.displayed_row_wrapped(index),
        ));
    }
    // `Row.wrapped` is HEAD-row semantics: the flagged row continues onto the
    // NEXT physical row (deferred_wrap_newline marks the row the cursor
    // exited). Accumulate until an unflagged row closes the logical line.
    let mut logical: Vec<String> = Vec::new();
    let mut pending = String::new();
    for (text, wrapped) in physical {
        pending.push_str(&text);
        if !wrapped {
            logical.push(std::mem::take(&mut pending));
        }
    }
    if !pending.is_empty() {
        logical.push(pending);
    }
    normalize(logical)
}

// ── Scenario table ──────────────────────────────────────────────────────

/// What the differential must observe for one scenario.
enum Verdict {
    /// Normalized logical-line sequences must be equal.
    Equal,
    /// Cataloged directional divergence: both sides fully specified.
    /// `retained` must appear in the capture text and be absent from the
    /// snapshot text — the catalog's direction pin.
    Diverged {
        capture: Vec<String>,
        snapshot: Vec<String>,
        retained: Option<&'static str>,
    },
    /// D7: capture hit the 1 MiB budget (marker present), snapshot shows the
    /// retained grid tail. Texts are NOT comparable by design.
    TruncationMarker,
}

struct Scenario {
    name: &'static str,
    catalog: &'static str,
    rows: usize,
    cols: usize,
    scrollback: usize,
    /// Bytes fed between the 133;B anchor sample and 133;D.
    output: String,
    /// Text that must appear on NEITHER side (alt-screen boundary pin).
    forbidden: Option<&'static str>,
    verdict: Verdict,
}

fn scenarios() -> Vec<Scenario> {
    let long_l = "L".repeat(199);
    let cjk45: String = "中".repeat(45);
    let w100 = "W".repeat(100);
    let ten_lines: String = (0..10).map(|i| format!("L{i:02}\n")).collect();
    vec![
        Scenario {
            name: "s01_empty_output_anchor_pinned",
            catalog: "anchor",
            rows: 6,
            cols: 40,
            scrollback: 50,
            // 133;B directly followed by 133;D: both truths must be EMPTY
            // (off-by-one pin — the anchor row is not off by one).
            output: String::new(),
            forbidden: None,
            verdict: Verdict::Equal,
        },
        Scenario {
            name: "s02_plain_baseline",
            catalog: "baseline",
            rows: 6,
            cols: 40,
            scrollback: 50,
            output: "alpha\nbeta\ngamma\n".to_string(),
            forbidden: None,
            verdict: Verdict::Equal,
        },
        Scenario {
            name: "s03_d1_soft_wrap_long_line",
            catalog: "D1",
            rows: 8,
            cols: 40,
            scrollback: 50,
            // 199 = 4×40 + 39: four wrapped physical rows + a 39-char tail
            // that shares its row with "END" (deliberately not an exact
            // column multiple, so "END" must NOT reconnect into the line).
            output: format!("{long_l}END\n"),
            forbidden: None,
            verdict: Verdict::Equal,
        },
        Scenario {
            name: "s04_d2_internal_blank_lines",
            catalog: "D2",
            rows: 8,
            cols: 40,
            scrollback: 50,
            output: "one\n\n\ntwo\n".to_string(),
            forbidden: None,
            verdict: Verdict::Equal,
        },
        Scenario {
            name: "s05_d3_csi_2j_no_capture_mirror",
            catalog: "D3(2J)",
            rows: 6,
            cols: 40,
            scrollback: 50,
            output: "before-clear\n\x1b[2J".to_string(),
            forbidden: None,
            verdict: Verdict::Diverged {
                capture: vec!["before-clear".to_string()],
                snapshot: vec![],
                retained: Some("before-clear"),
            },
        },
        Scenario {
            name: "s06_d3_csi_3j_no_capture_mirror",
            catalog: "D3(3J)",
            rows: 4,
            cols: 40,
            scrollback: 50,
            // 10 lines in a 4-row viewport: L00..L06 scroll into the
            // scrollback, L07..L09 stay visible; 3J clears the scrollback.
            output: format!("{ten_lines}\x1b[3J"),
            forbidden: None,
            verdict: Verdict::Diverged {
                capture: (0..10).map(|i| format!("L{i:02}")).collect(),
                snapshot: (7..10).map(|i| format!("L{i:02}")).collect(),
                retained: Some("L00"),
            },
        },
        Scenario {
            name: "s07_d3_ech_x_no_capture_mirror",
            catalog: "D3(X)",
            rows: 6,
            cols: 40,
            scrollback: 50,
            // BS×3 moves the cursor to column 2 (BS *is* mirrored in the
            // capture, so the capture cursor rewinds too); ECH then erases
            // "CDE" from the grid — and the capture keeps it.
            output: "ABCDE\x08\x08\x08\x1b[3X".to_string(),
            forbidden: None,
            verdict: Verdict::Diverged {
                capture: vec!["ABCDE".to_string()],
                snapshot: vec!["AB".to_string()],
                retained: Some("ABCDE"),
            },
        },
        Scenario {
            name: "s08_d4_cjk_wide_wrap",
            catalog: "D4",
            rows: 8,
            cols: 40,
            scrollback: 50,
            // 45 wide glyphs = 90 columns: rows of 20/20/5 with whole-glyph
            // wrap at the row end; the capture stores 45 chars, the snapshot
            // reconnect must reproduce them (〔动态验证〕item).
            output: format!("{cjk45}\n"),
            forbidden: None,
            verdict: Verdict::Equal,
        },
        Scenario {
            name: "s09_d5_orphan_combining_scalar",
            catalog: "D5",
            rows: 6,
            cols: 40,
            scrollback: 50,
            // U+0301 at column 0 has no base cell: the grid drops it
            // (perform.rs "No previous cell to extend") AFTER the capture
            // sink received it — directional divergence (〔动态验证〕item).
            output: "\u{0301}abc\n".to_string(),
            forbidden: None,
            verdict: Verdict::Diverged {
                capture: vec!["\u{0301}abc".to_string()],
                snapshot: vec!["abc".to_string()],
                retained: Some("\u{0301}"),
            },
        },
        Scenario {
            name: "s10_d7_capture_budget_truncation",
            catalog: "D7",
            rows: 24,
            cols: 80,
            scrollback: 200,
            // > 1 MiB of printable bytes: the capture stops at DEFAULT_OUTPUT_CAP
            // and appends the marker; the snapshot walks the retained grid
            // tail. Only the marker's presence is pinned (plan: 缩小验证点).
            output: "x".repeat(1_100_000),
            forbidden: None,
            verdict: Verdict::TruncationMarker,
        },
        Scenario {
            name: "s11_alt_screen_output_dropped_both_sides",
            catalog: "alt-boundary",
            rows: 6,
            cols: 40,
            scrollback: 50,
            // Prints inside the DEC 1049 excursion land in the alt grid and
            // are skipped by the capture's !alt_active gate — the boundary
            // property is that BOTH truths drop them (〔动态验证〕item).
            output: "before\n\x1b[?1049halt-only line\n\x1b[?1049lafter\n".to_string(),
            forbidden: Some("alt-only line"),
            verdict: Verdict::Equal,
        },
        Scenario {
            name: "s12_d8_edit_sequence_catalog_skip_demo",
            catalog: "D8",
            rows: 6,
            cols: 40,
            scrollback: 50,
            // BS×2 rewinds to column 2, ICH inserts two blanks there. ICH
            // ('@' ∈ the D8 opcode set) has no capture mirror, so the
            // snapshot shows the gap and the capture does not. This window's
            // byte shape is what the fuzz skip predicate ① excludes.
            output: "ABCD\x08\x08\x1b[2@".to_string(),
            forbidden: None,
            verdict: Verdict::Diverged {
                capture: vec!["ABCD".to_string()],
                snapshot: vec!["AB  CD".to_string()],
                retained: None,
            },
        },
        Scenario {
            name: "s13_d1_wrap_between_plain_lines",
            catalog: "D1",
            rows: 8,
            cols: 40,
            scrollback: 50,
            // Wrapped rows sandwiched between plain lines: reconnection must
            // stop exactly at the last wrapped row.
            output: format!("head line\n{w100}\ntail\n"),
            forbidden: None,
            verdict: Verdict::Equal,
        },
        Scenario {
            name: "s14_d9_vertical_move_folds_blank_row",
            catalog: "D9",
            rows: 8,
            cols: 40,
            scrollback: 50,
            // CUD moves the capture cursor down WITHOUT writing the crossed
            // row, so the capture folds the physical blank row while the
            // snapshot keeps it — the D2 canonical form (edge-empty strip
            // only) cannot absorb an INTERNAL empty line.
            output: "one\n\x1b[1Bx\n".to_string(),
            forbidden: None,
            verdict: Verdict::Diverged {
                capture: vec!["one".to_string(), "x".to_string()],
                snapshot: vec!["one".to_string(), String::new(), "x".to_string()],
                retained: None,
            },
        },
    ]
}

fn scenario(name: &str) -> Scenario {
    scenarios()
        .into_iter()
        .find(|scenario| scenario.name == name)
        .unwrap_or_else(|| panic!("scenario {name} missing from the table"))
}

// ── Runner ──────────────────────────────────────────────────────────────

fn run_scenario(scenario: &Scenario) {
    let label = format!("{} [{}]", scenario.name, scenario.catalog);
    let mut terminal = Terminal::with_scrollback(scenario.rows, scenario.cols, scenario.scrollback);
    terminal.process(b"\x1b]133;A\x07\x1b]133;B\x07");
    // ANCHOR RULE: sampled immediately after the 133;B bytes are processed.
    let anchor = terminal.grid().scrollback.position();
    terminal.process(scenario.output.as_bytes());
    terminal.process(b"\x1b]133;D;0\x07");

    let capture_text = terminal
        .block_tracker()
        .blocks()
        .last()
        .unwrap_or_else(|| panic!("{label}: no finalized block"))
        .output
        .to_string();
    let capture = capture_lines(&capture_text);
    let snapshot = snapshot_logical_lines(&terminal, anchor);
    let snapshot_text = snapshot.join("\n");

    if let Some(forbidden) = scenario.forbidden {
        assert!(
            !capture_text.contains(forbidden),
            "{label}: capture must not contain {forbidden:?}: {capture_text:?}"
        );
        assert!(
            !snapshot_text.contains(forbidden),
            "{label}: snapshot must not contain {forbidden:?}: {snapshot_text:?}"
        );
    }

    match &scenario.verdict {
        Verdict::Equal => {
            assert_eq!(
                capture, snapshot,
                "{label}: capture and snapshot truths diverged"
            );
        }
        Verdict::Diverged {
            capture: want_capture,
            snapshot: want_snapshot,
            retained,
        } => {
            assert_eq!(capture, *want_capture, "{label}: capture side mismatch");
            assert_eq!(snapshot, *want_snapshot, "{label}: snapshot side mismatch");
            if let Some(retained) = retained {
                assert!(
                    capture_text.contains(retained),
                    "{label}: capture must retain {retained:?} (direction pin): {capture_text:?}"
                );
                assert!(
                    !snapshot_text.contains(retained),
                    "{label}: snapshot must not contain {retained:?} (direction pin): {snapshot_text:?}"
                );
            }
        }
        Verdict::TruncationMarker => {
            assert!(
                capture_text.contains(TRUNCATION_MARKER),
                "{label}: capture must carry the truncation marker"
            );
            // P2-2 anti-drift pin: the capture stops at exactly
            // DEFAULT_OUTPUT_CAP of content and appends exactly the marker.
            assert_eq!(
                capture_text.len(),
                CAPTURE_BUDGET + TRUNCATION_MARKER.len(),
                "{label}: truncated capture must be exactly budget + marker"
            );
            assert!(
                !snapshot.is_empty()
                    && snapshot
                        .iter()
                        .all(|line| !line.is_empty() && line.chars().all(|c| c == 'x')),
                "{label}: snapshot must be the retained all-x grid tail: {snapshot:?}"
            );
            assert_ne!(
                capture, snapshot,
                "{label}: budget overflow must be a cataloged divergence"
            );
        }
    }
}

// One #[test] per scenario keeps failure isolation; the table above stays the
// single source of truth (表驱动).

#[test]
fn s01_empty_output_anchor_pinned() {
    run_scenario(&scenario("s01_empty_output_anchor_pinned"));
}

#[test]
fn s02_plain_baseline() {
    run_scenario(&scenario("s02_plain_baseline"));
}

#[test]
fn s03_d1_soft_wrap_long_line() {
    run_scenario(&scenario("s03_d1_soft_wrap_long_line"));
}

#[test]
fn s04_d2_internal_blank_lines() {
    run_scenario(&scenario("s04_d2_internal_blank_lines"));
}

#[test]
fn s05_d3_csi_2j_no_capture_mirror() {
    run_scenario(&scenario("s05_d3_csi_2j_no_capture_mirror"));
}

#[test]
fn s06_d3_csi_3j_no_capture_mirror() {
    run_scenario(&scenario("s06_d3_csi_3j_no_capture_mirror"));
}

#[test]
fn s07_d3_ech_x_no_capture_mirror() {
    run_scenario(&scenario("s07_d3_ech_x_no_capture_mirror"));
}

#[test]
fn s08_d4_cjk_wide_wrap() {
    run_scenario(&scenario("s08_d4_cjk_wide_wrap"));
}

#[test]
fn s09_d5_orphan_combining_scalar() {
    run_scenario(&scenario("s09_d5_orphan_combining_scalar"));
}

#[test]
fn s10_d7_capture_budget_truncation() {
    run_scenario(&scenario("s10_d7_capture_budget_truncation"));
}

#[test]
fn s11_alt_screen_output_dropped_both_sides() {
    run_scenario(&scenario("s11_alt_screen_output_dropped_both_sides"));
}

#[test]
fn s12_d8_edit_sequence_catalog_skip_demo() {
    run_scenario(&scenario("s12_d8_edit_sequence_catalog_skip_demo"));
}

#[test]
fn s13_d1_wrap_between_plain_lines() {
    run_scenario(&scenario("s13_d1_wrap_between_plain_lines"));
}

#[test]
fn s14_d9_vertical_move_folds_blank_row() {
    run_scenario(&scenario("s14_d9_vertical_move_folds_blank_row"));
}

/// The façade contract: the resolver variant the differential uses produces
/// PHYSICAL rows (that is exactly catalog D1), so the line-domain walk can
/// only be string-equal to it after soft-wrap reconnection. Pin the precise
/// relationship: same characters, strictly finer segmentation.
#[test]
fn line_domain_walk_matches_facade_text_on_a_wrapped_document() {
    let mut terminal = Terminal::with_scrollback(8, 40, 50);
    terminal.process(b"\x1b]133;A\x07\x1b]133;B\x07");
    let anchor = terminal.grid().scrollback.position();
    let long_line = format!("{}END\nplain\n", "L".repeat(199));
    terminal.process(long_line.as_bytes());
    terminal.process(b"\x1b]133;D;0\x07");

    let (facade_text, _, _) = terminal
        .grid()
        .document_snapshot_from_position_with_resolver(anchor, |_| None, CAPTURE_BUDGET);
    let facade_physical = normalize(facade_text.split('\n').map(str::to_string).collect());
    let walked = snapshot_logical_lines(&terminal, anchor);
    assert!(
        facade_physical.len() > walked.len(),
        "the façade must be split finer than the logical walk: {facade_physical:?} vs {walked:?}"
    );
    assert_eq!(
        facade_physical.concat(),
        walked.concat(),
        "reconnection must preserve the façade's characters exactly"
    );
    assert_eq!(
        walked,
        vec![format!("{}END", "L".repeat(199)), "plain".to_string()],
        "soft wraps must reconnect into the capture's logical lines"
    );
}
