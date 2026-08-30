//! v1.11.6 (PLAN_v1116 M3 / D-a / D-b): vertex-level golden baseline for
//! `build_block_view_vertices`.
//!
//! These are the FIRST tests that lock the function's `Vec<f32>` output
//! (F19: no existing test touches it). M4's structural split and M5/M6's
//! value-preserving color migrations must keep these bytes byte-equal, so a
//! pure structure move can never silently change pixels.
//!
//! Design rules (from the plan):
//! - Every scenario input is a fixed literal (command text, duration, cwd,
//!   spinner phase, bookmark set); no URL content, no selection.
//! - The renderer comes from the headless constructor
//!   (`MetalRenderer::new_headless_paint`), which shares the production
//!   `build_paint_core` and fills `layout_ctx` through the production
//!   draw()-entry chain — no second geometry derivation in the test.
//! - Vertices are serialized as f32 little-endian bytes (no hash, failures
//!   can be diffed). Hits are pinned by a sorted discriminant digest.
//! - Every scenario is built twice per run and asserted byte-identical
//!   (hidden-nondeterminism self-proof; also required for baseline capture).

use std::collections::HashMap;
use std::path::PathBuf;

use weft_core::blocks::{Block, BlockId, ForegroundSpan, InFlightBlock, StyledLine, StyledOutput};
use weft_core::config::Theme;
use weft_core::grid::{CellColor, Color};
use weft_core::selection::{BlockSelAnchor, BlockViewSelection, SelectionHandler};

use crate::app_state::BlockDiagnoseState;
use crate::paint::block_view_model::BlockViewPaintModel;
use crate::renderer::MetalRenderer;

const UPDATE_ENV: &str = "WEFT_UPDATE_BLOCK_VIEW_GOLDENS";
const REQUIRE_ENV: &str = "WEFT_REQUIRE_BLOCK_VIEW_GOLDENS";

fn flag_is_enabled(value: Option<&std::ffi::OsStr>) -> bool {
    matches!(value.and_then(|v| v.to_str()), Some("1") | Some("true"))
}

fn snapshot_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots")
        .join(format!("block_view_{name}.bin"))
}

/// A finished block with fully literal metadata: fixed timestamps → fixed
/// duration text ("1.2s"), fixed cwd/command/output, no styled output.
fn block_literal(
    id: u64,
    command: &str,
    cwd: Option<&str>,
    output: &str,
    exit_code: Option<i32>,
) -> Block {
    let started = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    Block {
        id: BlockId(id),
        command: command.to_string(),
        cwd: cwd.map(str::to_string),
        output: std::sync::Arc::from(output),
        styled_output: None,
        exit_code,
        started_at: started,
        finished_at: Some(started + std::time::Duration::from_millis(1_200)),
        collapsed: false,
        screen_origin: false,
    }
}

/// `block_literal` plus an explicit `styled_output` (S4 needs a line whose
/// cells carry an explicit RGB background to exercise the C1 flip).
fn block_styled_literal(
    id: u64,
    command: &str,
    cwd: Option<&str>,
    output: &str,
    exit_code: Option<i32>,
    styled: StyledOutput,
) -> Block {
    let mut block = block_literal(id, command, cwd, output, exit_code);
    block.styled_output = Some(std::sync::Arc::new(styled));
    block
}

/// Fixed placeholder palette. Blocks carry no styled_output, so palette
/// values only feed the styled-cache fingerprint (must simply be stable).
fn palette_literal() -> [Color; 256] {
    let mut palette = [Color::rgb(0, 0, 0); 256];
    palette[0] = Color::rgb(204, 204, 204);
    palette[1] = Color::rgb(255, 0, 0);
    palette[11] = Color::rgb(0, 255, 255);
    palette
}

/// S1 `all_row_types` — covers all 8 `LaidRow` variants through the real
/// layout chain (`compute_block_layout_pass`): finished-block Output
/// (single-chunk), multi-chunk Output (long wrapped line), Command (foldable
/// chevron + header actions), Header (bookmark star + exit-coded statuses),
/// live Output, LiveCommand (spinner), LiveHeader, Blank (output gap),
/// Separator, DiagnosePanel.
fn s1_model<'a>(
    blocks: &'a [Block],
    diagnose: &'a HashMap<BlockId, BlockDiagnoseState>,
    palette: &'a [Color; 256],
) -> BlockViewPaintModel<'a> {
    BlockViewPaintModel {
        blocks,
        live_head_lines: 0,
        region_bottom_y: 600.0,
        cwd: None,
        git_branch: Some("main"),
        live: Some(InFlightBlock {
            command: "npm test",
            cwd: Some("/tmp/weft"),
            output: "> weft@1.11.5 test\n> jest\nPASS tests/unit/sample.test.js (12 ms)\n",
            styled_output: None,
            version: 1,
            screen_origin: false,
        }),
        block_scroll: 0.0,
        viewport_rows: 24,
        block_hovered: None,
        block_selected: None,
        block_action_hovered: None,
        spinner_phase: 0.25,
        find_block_highlight: None,
        palette,
        cache_namespace: 1,
        block_diagnose_state: diagnose,
        ai_configured: false,
        tui_cursor: None,
        tui_preedit: None,
        cursor_blink_on: false,
        is_alt: false,
    }
}

/// S2 `wrapped_output` — multi-line multi-chunk output with soft wrap plus a
/// find highlight on the wrapped line (multi-chunk highlight branch + find
/// canvas). No in-flight block → the fixed-CWD header path activates.
fn s2_model<'a>(
    blocks: &'a [Block],
    palette: &'a [Color; 256],
    diagnose_state: &'a HashMap<BlockId, BlockDiagnoseState>,
) -> BlockViewPaintModel<'a> {
    BlockViewPaintModel {
        blocks,
        live_head_lines: 0,
        region_bottom_y: 600.0,
        cwd: Some("/tmp/weft"),
        git_branch: None,
        live: None,
        block_scroll: 0.0,
        viewport_rows: 24,
        block_hovered: None,
        block_selected: None,
        block_action_hovered: None,
        spinner_phase: -1.0,
        find_block_highlight: Some((5, 1, false, 10, 5)),
        palette,
        cache_namespace: 2,
        block_diagnose_state: diagnose_state,
        ai_configured: false,
        tui_cursor: None,
        tui_preedit: None,
        cursor_blink_on: false,
        is_alt: false,
    }
}

/// S3 `tui_preedit` — LiveCommand + in-flight output with a TUI caret and an
/// active IME preedit (preedit paints on the matched live row).
fn s3_model<'a>(
    palette: &'a [Color; 256],
    diagnose_state: &'a HashMap<BlockId, BlockDiagnoseState>,
) -> BlockViewPaintModel<'a> {
    BlockViewPaintModel {
        blocks: &[],
        live_head_lines: 0,
        region_bottom_y: 600.0,
        cwd: None,
        git_branch: None,
        live: Some(InFlightBlock {
            command: "openclaw",
            cwd: None,
            output: "frame-top\nframe-two\nframe-three\nframe-four\nframe-five\n",
            styled_output: None,
            version: 7,
            screen_origin: false,
        }),
        block_scroll: 0.0,
        viewport_rows: 24,
        block_hovered: None,
        block_selected: None,
        block_action_hovered: None,
        spinner_phase: 0.25,
        find_block_highlight: None,
        palette,
        cache_namespace: 3,
        block_diagnose_state: diagnose_state,
        ai_configured: false,
        tui_cursor: Some((3, 2)),
        tui_preedit: Some(("ni'hao", Some((0, 2)))),
        cursor_blink_on: false,
        is_alt: false,
    }
}

/// S4 `selection_over_explicit_bg` — one finished block whose output line
/// carries an explicit RGB background over its full width, with a block-view
/// selection covering chars 2..6 of that line. The C1 flip (M7/D-e) must
/// make the selection band win on exactly the selected span while explicit-
/// bg cells survive on both sides.
fn s4_model<'a>(
    blocks: &'a [Block],
    palette: &'a [Color; 256],
    diagnose_state: &'a HashMap<BlockId, BlockDiagnoseState>,
) -> BlockViewPaintModel<'a> {
    BlockViewPaintModel {
        blocks,
        live_head_lines: 0,
        region_bottom_y: 600.0,
        cwd: None,
        git_branch: None,
        live: None,
        block_scroll: 0.0,
        viewport_rows: 24,
        block_hovered: None,
        block_selected: None,
        block_action_hovered: None,
        spinner_phase: -1.0,
        find_block_highlight: None,
        palette,
        cache_namespace: 4,
        block_diagnose_state: diagnose_state,
        ai_configured: false,
        tui_cursor: None,
        tui_preedit: None,
        cursor_blink_on: false,
        is_alt: false,
    }
}

/// Sorted hit-target digest: each target's Debug string, sorted, joined.
/// Pins hits independently of the vertex bytes (weak orthogonal lock).
fn hits_digest(hits: &[crate::overlay::HitRegion]) -> String {
    let mut kinds: Vec<String> = hits.iter().map(|h| format!("{:?}", h.target)).collect();
    kinds.sort();
    format!("{}|{}", kinds.len(), kinds.join(","))
}

/// Build the scene twice (determinism self-proof), serialize verts to
/// little-endian f32 bytes, then update or assert against the .bin.
/// `model_a`/`model_b` are two identical model instances (the function takes
/// the model by value, so the double build passes two copies of the same
/// literal input). Returns the first build's vertex stream so callers can
/// run byte-level behavioral checks (S4's C1 lock) on the captured bytes.
fn run_golden(
    name: &str,
    renderer: &MetalRenderer,
    model_a: BlockViewPaintModel<'_>,
    model_b: BlockViewPaintModel<'_>,
    expected_hits_digest: &str,
) -> Vec<f32> {
    run_golden_with_selection(
        name,
        renderer,
        model_a,
        model_b,
        expected_hits_digest,
        SelectionHandler::new(),
    )
}

/// `run_golden` with a caller-provisioned selection (S4). The handler must
/// carry a fingerprint matching `block_selection_fingerprint(blocks, live_
/// head_lines)`, or `build_block_view_vertices` clears it before painting.
fn run_golden_with_selection(
    name: &str,
    renderer: &MetalRenderer,
    model_a: BlockViewPaintModel<'_>,
    model_b: BlockViewPaintModel<'_>,
    expected_hits_digest: &str,
    mut selection: SelectionHandler,
) -> Vec<f32> {
    let (verts_a, hits_a, bv_rows_a) = renderer.build_block_view_vertices(model_a, &mut selection);
    // Determinism self-proof: a second identical build must produce
    // byte-identical vertices (plus identical hits/rows counts).
    let (verts_b, hits_b, bv_rows_b) = renderer.build_block_view_vertices(model_b, &mut selection);
    if verts_a != verts_b {
        let diff = verts_a
            .iter()
            .zip(&verts_b)
            .position(|(a, b)| a != b)
            .unwrap_or(0);
        eprintln!(
            "{name}: DETERMINISM FAIL at f32[{diff}]: a={:?} b={:?} (len a={} b={})",
            verts_a.get(diff),
            verts_b.get(diff),
            verts_a.len(),
            verts_b.len()
        );
        assert_eq!(
            verts_a, verts_b,
            "{name}: double-build vertex variance — hidden nondeterminism"
        );
    }
    assert_eq!(hits_a, hits_b, "{name}: double-build hits variance");
    // BlockViewRow carries no PartialEq (defined in weft_core); length + the
    // digest below are the observable contract.
    assert_eq!(bv_rows_a.len(), bv_rows_b.len());

    let bytes: Vec<u8> = verts_a.iter().flat_map(|f| f32::to_le_bytes(*f)).collect();
    let path = snapshot_path(name);
    let update = std::env::var_os(UPDATE_ENV).is_some_and(|v| flag_is_enabled(Some(&v)));
    let digest = hits_digest(&hits_a);
    if update {
        std::fs::write(&path, &bytes)
            .unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
        eprintln!("{name}: updated {} ({} bytes)", path.display(), bytes.len());
        eprintln!("{name}: hits digest = {digest}");
    } else {
        assert_eq!(
            digest, expected_hits_digest,
            "{name}: hits digest drift (len + sorted target discriminants)"
        );
        let expected = std::fs::read(&path).unwrap_or_else(|error| {
            panic!(
                "read {}: {error} (run with {UPDATE_ENV}=1 to capture)",
                path.display()
            )
        });
        assert_eq!(
            expected.len(),
            bytes.len(),
            "{name}: byte length drift (expected {} from {}, got {})",
            expected.len(),
            path.display(),
            bytes.len()
        );
        let first_diff = expected
            .iter()
            .zip(&bytes)
            .position(|(e, a)| e != a)
            .unwrap_or(expected.len().min(bytes.len()));
        assert_eq!(
            expected,
            bytes,
            "{name}: vertex bytes differ from {} at byte {first_diff} \
             (run with {UPDATE_ENV}=1 only after an intentional visual change)",
            path.display()
        );
    }
    eprintln!(
        "{name}: {} verts ({} bytes), {} hits, {} bv_rows",
        verts_a.len(),
        bytes.len(),
        hits_a.len(),
        bv_rows_a.len()
    );
    verts_a
}

fn renderer_headless() -> MetalRenderer {
    MetalRenderer::new_headless_paint(Theme::weft_dark())
}

/// Mirrors the offscreen_snapshots skip precedent: no Metal device (CI
/// without GPU) skips the goldens unless REQUIRE is explicitly set.
fn require_metal_or_skip() {
    let Some(_device) = metal::Device::system_default() else {
        let require = std::env::var_os(REQUIRE_ENV).is_some_and(|v| flag_is_enabled(Some(&v)));
        assert!(
            !require,
            "{REQUIRE_ENV}=1, but no Metal device is available"
        );
        eprintln!("skipping block-view goldens: no Metal device is available");
        return;
    };
}

#[test]
fn golden_all_row_types() {
    require_metal_or_skip();
    let blocks = vec![
        // Block 1: single-chunk output, success tone, bookmarked header
        // (star glyph).
        block_literal(
            1,
            "cargo build",
            Some("/tmp/weft"),
            "Compiling weft v1.11.5\nFinished release profile target(s) in 1.20s\n",
            Some(0),
        ),
        // Block 2: long wrapped line → multi-chunk Output rows; exit 2 →
        // error-tone header with "exit 2" status.
        block_literal(
            2,
            "ls -la",
            Some("/tmp/weft"),
            &format!(
                "total 128\ndrwxr-xr-x  21 andylee  staff  672 {}\n",
                "x".repeat(180)
            ),
            Some(2),
        ),
        // Block 3: empty output (non-foldable) + interrupted (warning tone,
        // "interrupted" status).
        block_literal(3, "which git", Some("/tmp/weft"), "", None),
        // Block 4: diagnose panel below output (Ok result, success tone).
        block_literal(4, "weft diagnose", None, "scan complete\n", Some(0)),
    ];
    let mut diagnose = HashMap::new();
    diagnose.insert(
        BlockId(4),
        BlockDiagnoseState {
            pending_id: None,
            result: Some(Ok("scan passed, 0 issues".to_string())),
        },
    );
    let palette = palette_literal();

    // Bookmarked star on block 1's header.
    let mut renderer = renderer_headless();
    renderer.bookmarked_blocks.insert(BlockId(1));

    let model_a = s1_model(&blocks, &diagnose, &palette);
    let model_b = s1_model(&blocks, &diagnose, &palette);
    // Digest expectations are filled after the first capture run prints them.
    run_golden(
        "all_row_types",
        &renderer,
        model_a,
        model_b,
        "12|BlockActionCopy(BlockId(1)),BlockActionCopy(BlockId(2)),BlockActionCopy(BlockId(3)),BlockActionCopy(BlockId(4)),BlockActionFold(BlockId(1)),BlockActionFold(BlockId(2)),BlockActionFold(BlockId(3)),BlockActionFold(BlockId(4)),BlockDiagnoseClose(BlockId(4)),BlockFold(BlockId(1)),BlockFold(BlockId(2)),BlockFold(BlockId(4))",
    );
}

#[test]
fn golden_wrapped_output() {
    require_metal_or_skip();
    // P2-5 (PLAN_v11110): the mixed-word tail line is the word-boundary
    // anchor for the M-A change. Word+space widths (8 and 6 cols) do NOT
    // divide the 88-col headless layout evenly, so under pure character
    // wrapping the second "b.txt" would split mid-word ("…b.|txt…"); the
    // word-aware rule keeps every word whole. The anchor assertion below
    // locks that shape so a future regression to character wrapping fails
    // even after a blind golden re-capture.
    let words = format!("{}{}", "USER.md ".repeat(10), "b.txt ".repeat(5));
    let blocks = vec![block_literal(
        5,
        "grep -r weft src",
        Some("/tmp/weft"),
        &format!(
            "src/weft/main.rs:1: use weft_core::grid;\n{}\nlast line\n{}\n",
            "x".repeat(260),
            words,
        ),
        Some(0),
    )];
    let palette = palette_literal();
    let renderer = renderer_headless();
    let empty = HashMap::new();
    // P2-5 word-boundary anchor: recompute the same chunking the golden path
    // uses (layout cols from the headless layout ctx, S2 has the fixed-CWD
    // header active). Every break must land at a word start and the chunk
    // before each break must end in whitespace.
    let ctx = renderer.layout_ctx.expect("headless layout ctx");
    let layout = crate::layout::layout_block_view(&ctx, 600.0, true);
    let ranges = crate::paint::grid_cache::block_line_chunk_ranges(&words, layout.cols);
    let chunks: Vec<&str> = ranges.iter().map(|r| &words[r.clone()]).collect();
    eprintln!("S2 word-anchor: cols={} chunks={chunks:?}", layout.cols);
    assert!(
        chunks.len() > 1,
        "the mixed-word line must wrap at cols {}",
        layout.cols
    );
    assert!(
        chunks[0].ends_with("b.txt "),
        "the chunk before the first break keeps the trailing padding whitespace"
    );
    assert!(
        chunks.iter().skip(1).all(|c| c.starts_with("b.txt ")),
        "every continuation chunk must start at a word boundary (got {chunks:?})"
    );
    let model_a = s2_model(&blocks, &palette, &empty);
    let model_b = s2_model(&blocks, &palette, &empty);
    run_golden(
        "wrapped_output",
        &renderer,
        model_a,
        model_b,
        "3|BlockActionCopy(BlockId(5)),BlockActionFold(BlockId(5)),BlockFold(BlockId(5))",
    );
}

#[test]
fn golden_tui_preedit() {
    require_metal_or_skip();
    let palette = palette_literal();
    let renderer = renderer_headless();
    let empty = HashMap::new();
    let model_a = s3_model(&palette, &empty);
    let model_b = s3_model(&palette, &empty);
    run_golden("tui_preedit", &renderer, model_a, model_b, "0|");
}

/// v1.11.6 (PLAN_v1116 M7.3): byte-level C1 lock companion to the S4 golden.
/// Scans the emitted vertex stream for (a) the selection-band quad (exact
/// `selection_colors().quad` bits — the color the scene paints selected
/// cells with) and (b) every per-cell explicit-bg quad (exact
/// `resolve_cell_color(Rgb)` bits), then proves:
/// - the band is a single wide quad covering 4 cells (chars 2..6 of the
///   scene's 10-char line, all single-width);
/// - every explicit-bg quad is a 1-cell quad lying fully OUTSIDE the band
///   (cells 0..1 and 6..9 keep their explicit background);
/// - explicit-bg cells exist on BOTH sides (the flip is span-scoped, not a
///   scene-wide wipe).
///
/// This must stay permanent: a blind `WEFT_UPDATE_BLOCK_VIEW_GOLDENS=1`
/// re-capture after a C1 regression would otherwise keep the .bin "green"
/// while the painted bytes silently reverted explicit-bg-wins.
fn assert_selection_wins_over_explicit_bg(
    verts: &[f32],
    theme: &Theme,
    palette: &[Color; 256],
    cw: f32,
) {
    const VERTEX_FLOATS: usize = 12; // position_tex + fg_color + bg_color
    const QUAD_FLOATS: usize = 6 * VERTEX_FLOATS;
    assert_eq!(
        verts.len() % QUAD_FLOATS,
        0,
        "vertex stream must be a whole number of quads"
    );
    let explicit = crate::paint::primitives::resolve_cell_color(
        CellColor::Rgb(Color::rgb(120, 40, 30)),
        [0.0; 4],
        palette,
    );
    let band = crate::paint::selection_color::selection_colors(theme).quad;

    let x_interval = |quad: &[f32]| {
        let mut x0 = f32::INFINITY;
        let mut x1 = f32::NEG_INFINITY;
        for v in 0..6 {
            let x = quad[v * VERTEX_FLOATS];
            x0 = x0.min(x);
            x1 = x1.max(x);
        }
        (x0, x1)
    };
    let mut band_intervals: Vec<(f32, f32)> = Vec::new();
    let mut explicit_intervals: Vec<(f32, f32)> = Vec::new();
    for quad in verts.chunks_exact(QUAD_FLOATS) {
        let bg = [quad[8], quad[9], quad[10], quad[11]];
        if bg == band {
            band_intervals.push(x_interval(quad));
        } else if bg == explicit {
            explicit_intervals.push(x_interval(quad));
        }
    }

    // The selection band: exactly one quad, exactly 4 cells wide (chars
    // 2..6 of the 10-char literal line).
    assert_eq!(
        band_intervals.len(),
        1,
        "the selection band must be exactly one quad (found {})",
        band_intervals.len()
    );
    let (band_x0, band_x1) = band_intervals[0];
    let cw_tol = cw * 1e-3;
    assert!(
        (band_x1 - band_x0 - 4.0 * cw).abs() < cw_tol,
        "band must span exactly 4 cells, got {} (cw={cw})",
        band_x1 - band_x0
    );

    // Every explicit-bg quad: one cell wide, and fully outside the band —
    // the C1 flip's byte-level signature (no explicit bg quad may overlap
    // the selected span; the band shows through instead).
    assert!(
        !explicit_intervals.is_empty(),
        "explicit-bg quads must exist outside the selection"
    );
    let mut left_count = 0usize;
    let mut right_count = 0usize;
    for &(x0, x1) in &explicit_intervals {
        assert!(
            (x1 - x0 - cw).abs() < cw_tol,
            "explicit-bg quad must be one cell wide, got {} (cw={cw})",
            x1 - x0
        );
        if x1 <= band_x0 + cw_tol {
            left_count += 1;
        } else if x0 >= band_x1 - cw_tol {
            right_count += 1;
        } else {
            panic!(
                "explicit-bg quad [{x0}, {x1}) overlaps the selection band \
                 [{band_x0}, {band_x1}) — C1 flip regressed (selection must \
                 win over explicit bg)"
            );
        }
    }
    assert!(
        left_count > 0 && right_count > 0,
        "explicit-bg cells must survive on both sides of the selection \
         (left={left_count}, right={right_count})"
    );
}

#[test]
fn golden_selection_over_explicit_bg() {
    require_metal_or_skip();
    // One finished block: a single 10-char output line carrying an explicit
    // RGB background (CellColor::Rgb → the `explicit_background` cell path
    // at style.rs:289-292) over its FULL width, so the C1 flip has both a
    // selected span and unselected flanking cells in the same line.
    let blocks = vec![block_styled_literal(
        1,
        "cargo build",
        Some("/tmp/weft"),
        "bg_stripe!\n",
        Some(1),
        StyledOutput {
            lines: vec![StyledLine {
                line: 0,
                foregrounds: Vec::new(),
                backgrounds: vec![ForegroundSpan {
                    start: 0,
                    end: 10,
                    color: CellColor::Rgb(Color::rgb(120, 40, 30)),
                }],
                links: Vec::new(),
                attributes: Vec::new(),
                underline_colors: Vec::new(),
            }],
        },
    )];
    let palette = palette_literal();
    let renderer = renderer_headless();
    let empty = HashMap::new();
    let model_a = s4_model(&blocks, &palette, &empty);
    let model_b = s4_model(&blocks, &palette, &empty);
    let mut sel = SelectionHandler::new();
    // Content-anchored selection over chars 2..6 of block 1's line 0. The
    // document fingerprint must match what build_block_view_vertices
    // derives, else the renderer clears the selection (block_view.rs:211-
    // 226).
    sel.block_view_selection = Some(BlockViewSelection::new(
        BlockSelAnchor::new(Some(1), 0, 2),
        BlockSelAnchor::new(Some(1), 0, 6),
    ));
    sel.block_doc_fingerprint = Some(crate::selection::block_selection_fingerprint(&blocks, 0));
    let verts = run_golden_with_selection(
        "selection_over_explicit_bg",
        &renderer,
        model_a,
        model_b,
        "3|BlockActionCopy(BlockId(1)),BlockActionFold(BlockId(1)),BlockFold(BlockId(1))",
        sel,
    );
    // Byte-level C1 lock (permanent — see assert_selection_wins_over_explicit_bg).
    assert_selection_wins_over_explicit_bg(
        &verts,
        &Theme::weft_dark(),
        &palette,
        renderer.cell_width() as f32,
    );
}

#[test]
fn block_view_golden_env_flags_exactly_one() {
    assert!(flag_is_enabled(Some(std::ffi::OsStr::new("1"))));
    assert!(flag_is_enabled(Some(std::ffi::OsStr::new("true"))));
    for value in [
        None,
        Some(std::ffi::OsStr::new("")),
        Some(std::ffi::OsStr::new("0")),
        Some(std::ffi::OsStr::new("2")),
    ] {
        assert!(!flag_is_enabled(value));
    }
}
