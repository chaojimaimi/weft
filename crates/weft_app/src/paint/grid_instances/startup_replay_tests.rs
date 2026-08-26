//! FIX_OPENCODE_STARTUP_FLASH — Phase 0 forensic replay harness.
//!
//! Replays the captured opencode startup byte stream
//! (`fixtures/opencode_startup.bin`, vendored via `include_bytes!` so CI and
//! fresh checkouts exercise the same regression assertions — the original
//! capture lived under `.zcode/`, which is gitignored) through
//! `Terminal::process` in PTY-sized batches and, for every batch where the
//! real event-loop gating would present a frame
//! (`!terminal.synchronized_output()`), pushes the grid through the real
//! `build_row_instances` pipeline and reports:
//!
//!   1. REVERSE-flagged cells (branch A precondition),
//!   2. minimum-contrast boosts applied to default-color cells (branch C),
//!   3. background runs wide+bright enough to read as "white bars".
//!
//! The three `forensic_*` tests are pure diagnostics and `#[ignore]`d (CI
//! only runs the `no_bright_bar_*` regression). Run the dumps explicitly
//! with:
//!   cargo test --bin weft startup_replay -- --ignored --nocapture

use super::{build_row_instances, BgInstance};
use weft_core::grid::{CellColor, CellFlags, Color, CursorStyle};
use weft_core::selection::SelectionHandler;
use weft_core::vt::Terminal;

/// Vendored opencode startup capture (7987 bytes, SHA-256
/// 9953c21d…c073a matching `.zcode/replay/opencode_startup.bin` at capture
/// time). Compile-time loading removes the fs error path — the regression
/// assertions below must run identically on CI, where `.zcode/` does not
/// exist.
const STARTUP_BYTES: &[u8] = include_bytes!("fixtures/opencode_startup.bin");

/// Cell width (px) used by this harness's `build_row_instances` calls; the
/// wide-run probes derive cell counts from quad widths via this constant.
const CELL_W: f32 = 10.0;

/// User config ships `[theme] name = "warp"` — Warp Dark palette.
const THEME_FG: [f32; 4] = [
    0xd9 as f32 / 255.0,
    0xd9 as f32 / 255.0,
    0xe3 as f32 / 255.0,
    1.0,
];
const THEME_BG: [f32; 4] = [
    0x1b as f32 / 255.0,
    0x1b as f32 / 255.0,
    0x28 as f32 / 255.0,
    1.0,
];

/// sRGB relative luminance of an RGBA quad color (alpha ignored).
fn luminance(c: [f32; 4]) -> f32 {
    let linear = |ch: f32| {
        let ch = ch.clamp(0.0, 1.0);
        if ch <= 0.04045 {
            ch / 12.92
        } else {
            ((ch + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(c[0]) + 0.7152 * linear(c[1]) + 0.0722 * linear(c[2])
}

struct ReplayConfig {
    rows: usize,
    cols: usize,
    /// PTY read size in bytes (weft's pty.rs reads up to 256 KiB; small
    /// values stress-test intermediate frames).
    batch: usize,
    minimum_contrast: f32,
}

impl ReplayConfig {
    fn capture_geometry(batch: usize) -> Self {
        // The capture PTY drew rows 1..=24 × 80 cols (window 0 of the stream).
        Self {
            rows: 24,
            cols: 80,
            batch,
            minimum_contrast: 7.0,
        }
    }

    fn dogfood_geometry(batch: usize) -> Self {
        // Real weft.log geometry: alt-screen toggled rows=62 cols=200.
        Self {
            rows: 62,
            cols: 200,
            batch,
            minimum_contrast: 7.0,
        }
    }
}

/// Per-batch forensic record.
struct FrameReport {
    batch: usize,
    byte_range: std::ops::Range<usize>,
    /// `Terminal::synchronized_output()` after processing this batch — when
    /// true the app-layer gate suppresses the redraw entirely.
    sync_active: bool,
    alt_active: bool,
    reverse_cells: usize,
    contrast_boosted_cells: usize,
    /// Wide bright bg runs (candidate white bars), formatted.
    bright_bars: Vec<String>,
    /// Cells carrying text attributes that NO sequence in the stream ever
    /// requested (SGR 4/21/24 are absent; UNDERLINE only arrives via the
    /// `>4;1m` private-SGR misparse — see `csi_gt_private_sgr_is_not_text_attributes`).
    underline_flagged_cells: usize,
    /// Glyph-stream decoration quads (underline/strike rails) colored
    /// near-white — the actual "white horizontal bar" artifact, emitted
    /// for the space-cleared rows when the misparse poisons their attrs.
    bright_decoration_rails: usize,
}

/// Full per-row instance stream (bg runs + glyphs) for the white-bar probe.
fn painted_row(t: &Terminal, cfg: &ReplayConfig, row: usize) -> super::GridRowInstances {
    let grid = t.grid();
    build_row_instances(
        grid,
        &Color::standard_palette(),
        row,
        THEME_FG,
        THEME_BG,
        [
            0xf0 as f32 / 255.0,
            0xd4 as f32 / 255.0,
            0xa8 as f32 / 255.0,
            1.0,
        ],
        [0.3, 0.5, 0.7, 0.6],
        [0.22, 0.34, 0.50, 1.0],
        &grid.cursor,
        CursorStyle::Block,
        false,
        &SelectionHandler::new(),
        1.0,
        cfg.minimum_contrast,
        CELL_W,
        20.0,
        0.0,
        0.0,
        false,
    )
}

fn painted_bgs(t: &Terminal, cfg: &ReplayConfig, row: usize) -> Vec<BgInstance> {
    painted_row(t, cfg, row).bg_instances
}

/// Scan one presented frame. Returns
/// `(reverse, boosted, bright_bg_bars, underline_cells, white_rails)`.
///
/// `boosted_cells` counts cells whose emitted glyph foreground differs from
/// the raw resolved foreground — i.e. `ensure_minimum_text_contrast` fired
/// (branch C evidence). Only non-explicit, non-graphic cells can boost.
///
/// `white_rails` counts near-white Decoration quads in the glyph stream —
/// the actual white-bar artifact (underline rails on space-cleared rows,
/// FIX_OPENCODE_STARTUP_FLASH), which bg-run scans cannot see.
fn scan_frame(t: &Terminal, cfg: &ReplayConfig) -> (usize, usize, Vec<String>, usize, usize) {
    let grid = t.grid();
    let mut reverse_cells = 0usize;
    let mut boosted_cells = 0usize;
    let mut bars = Vec::new();
    let mut underline_cells = 0usize;
    let mut white_rails = 0usize;

    for row in 0..grid.num_rows {
        for col in 0..grid.num_cols {
            let cell = grid.cell(row, col);
            if cell.flags.contains(CellFlags::WIDE_SPACER) {
                continue;
            }
            if cell.flags.contains(CellFlags::REVERSE) {
                reverse_cells += 1;
            }
            if cell.flags.contains(CellFlags::UNDERLINE) {
                underline_cells += 1;
            }
            // Branch C probe: would the booster change this cell's fg?
            let explicit = cell.fg != CellColor::Default || cell.flags.contains(CellFlags::REVERSE);
            let graphic = matches!(cell.character, '\u{2500}'..='\u{259f}');
            if !explicit && !graphic {
                let raw_fg =
                    super::resolve_cell_color(cell.fg, THEME_FG, &Color::standard_palette());
                let boosted = crate::paint::primitives::ensure_minimum_text_contrast(
                    raw_fg,
                    THEME_BG,
                    cfg.minimum_contrast,
                );
                if boosted != raw_fg {
                    boosted_cells += 1;
                }
            }
        }
        // White-bar probe: any bg quad >= 4 cells wide above the bright line.
        for bi in painted_bgs(t, cfg, row) {
            let cells_wide = bi.w / CELL_W;
            if cells_wide >= 4.0 && luminance(bi.bg) > 0.5 {
                bars.push(format!(
                    "row {row}: BRIGHT BAR {:.1} cells wide bg={:?} lum={:.3}",
                    cells_wide,
                    bi.bg,
                    luminance(bi.bg)
                ));
            }
        }
        // White-rail probe (the real flash): near-white decoration quads
        // spanning cells (underline/strike rails painted on cleared rows).
        for gi in &painted_row(t, cfg, row).glyph_instances {
            if let super::GlyphInstance::Decoration { dst, color } = gi {
                if luminance(*color) > 0.5 {
                    white_rails += 1;
                    let cells_wide = (dst[2] - dst[0]) / CELL_W;
                    if cells_wide >= 4.0 {
                        bars.push(format!(
                            "row {row}: WHITE RAIL {cells_wide:.1} cells wide color={color:?} lum={:.3}",
                            luminance(*color)
                        ));
                    }
                }
            }
        }
    }
    (
        reverse_cells,
        boosted_cells,
        bars,
        underline_cells,
        white_rails,
    )
}

/// Byte offsets where DEC 2026 windows open (`2026h`) / close (`2026l`),
/// derived from the fixture itself. Used instead of
/// `Terminal::synchronized_output()` for the presentation decision because
/// that method reads the WALL CLOCK (`Instant::now()` vs a 200 ms timeout):
/// under a debug-profile harness the per-batch pixel scan can legitimately
/// burn >200 ms of real time inside one window and would misreport the gate.
/// The parser's own state machine was verified to match these tokens exactly
/// (see `forensic_trace_sync_state_transitions`).
fn sync_windows(data: &[u8]) -> Vec<std::ops::Range<usize>> {
    const OPEN: &[u8] = b"\x1b[?2026h";
    const CLOSE: &[u8] = b"\x1b[?2026l";
    let mut events = Vec::new();
    for (pat, opens) in [(OPEN, true), (CLOSE, false)] {
        let mut start = 0;
        while let Some(idx) = find_at(data, pat, start) {
            events.push((idx, opens));
            start = idx + pat.len();
        }
    }
    events.sort_by_key(|(idx, _)| *idx);
    let mut windows = Vec::new();
    let mut open_at: Option<usize> = None;
    for (idx, opens) in events {
        if opens {
            open_at.get_or_insert(idx);
        } else if let Some(start) = open_at.take() {
            windows.push(start..idx + CLOSE.len());
        }
    }
    if let Some(start) = open_at {
        windows.push(start..data.len());
    }
    windows
}

fn find_at(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// Deterministic stand-in for the app-layer gate: would a frame be presented
/// after processing bytes `..end`? True iff no 2026 window spans that offset.
fn gate_would_present(windows: &[std::ops::Range<usize>], end: usize) -> bool {
    !windows.iter().any(|w| w.contains(&end))
}

/// Core forensic loop shared by the dump and the assertions. The gate
/// decision is derived from the fixture's own 2026 token layout (see
/// `sync_windows`) — deterministic, independent of harness wall-clock time.
fn replay_in_batches(cfg: &ReplayConfig) -> Vec<FrameReport> {
    let data = STARTUP_BYTES;
    let windows = sync_windows(data);
    let mut t = Terminal::new(cfg.rows, cfg.cols);
    let mut out = Vec::new();
    for (i, chunk) in data.chunks(cfg.batch).enumerate() {
        t.process(chunk);
        let end = i * cfg.batch + chunk.len();
        let sync_active = !gate_would_present(&windows, end);
        let alt = t.is_alt_screen_active();
        // Scan every intermediate grid state (not just presentable ones): the
        // assertions below must hold even for frames a correct gate suppresses,
        // so a future gate regression can never silently expose bright bars.
        let (reverse, boosted, bars, underline_cells, white_rails) = scan_frame(&t, cfg);
        out.push(FrameReport {
            batch: i,
            byte_range: i * cfg.batch..end,
            sync_active,
            alt_active: alt,
            reverse_cells: reverse,
            contrast_boosted_cells: boosted,
            bright_bars: bars,
            underline_flagged_cells: underline_cells,
            bright_decoration_rails: white_rails,
        });
    }
    out
}

fn summarize(label: &str, cfg: &ReplayConfig) {
    let frames = replay_in_batches(cfg);
    let presented: Vec<&FrameReport> = frames.iter().filter(|f| !f.sync_active).collect();
    let total_reverse: usize = frames.iter().map(|f| f.reverse_cells).sum();
    let max_boosted = frames
        .iter()
        .map(|f| f.contrast_boosted_cells)
        .max()
        .unwrap_or(0);
    let max_underline = frames
        .iter()
        .map(|f| f.underline_flagged_cells)
        .max()
        .unwrap_or(0);
    let max_rails = frames
        .iter()
        .map(|f| f.bright_decoration_rails)
        .max()
        .unwrap_or(0);
    println!(
        "{label}: {} batches, {} presentable frames, cumulative REVERSE cells={}, \
         max contrast-boosted cells in one frame={}, max underline cells={}, max white rails={}",
        frames.len(),
        presented.len(),
        total_reverse,
        max_boosted,
        max_underline,
        max_rails
    );
    for f in &frames {
        if f.sync_active
            && f.bright_bars.is_empty()
            && f.reverse_cells == 0
            && f.underline_flagged_cells == 0
            && f.bright_decoration_rails == 0
        {
            continue;
        }
        println!(
            "  batch {:2} bytes {:5}..{:5} sync={:5} alt={:5} reverse={:3} boosted={:3} \
             underline={:4} rails={:4}",
            f.batch,
            f.byte_range.start,
            f.byte_range.end,
            f.sync_active,
            f.alt_active,
            f.reverse_cells,
            f.contrast_boosted_cells,
            f.underline_flagged_cells,
            f.bright_decoration_rails
        );
        for bar in &f.bright_bars {
            println!("    {bar}");
        }
    }
}

/// Dump grid row content (char + resolved fg/bg + REVERSE/DIM flags) for the
/// frames a correct gate would present, plus the final stable grid. Prints
/// only rows with content or non-default colors. Pure diagnostics — ignored
/// by default; run with `cargo test --bin weft startup_replay -- --ignored
/// --nocapture`.
#[test]
#[ignore = "pure diagnostics; run explicitly with --ignored"]
fn forensic_dump_grid_content() {
    use std::fmt::Write as _;
    let data = STARTUP_BYTES;
    let windows = sync_windows(data);
    for (rows, cols, label) in [(24usize, 80usize, "capture"), (62, 200, "dogfood")] {
        let mut t = Terminal::new(rows, cols);
        println!("--- grid content {label} {rows}x{cols} ---");
        let mut presented_any = false;
        for (i, chunk) in data.chunks(256).enumerate() {
            t.process(chunk);
            let end = i * 256 + chunk.len();
            if gate_would_present(&windows, end) {
                presented_any = true;
                let mut text = String::new();
                for r in 0..grid_rows_dump(&t) {
                    let line = dump_row(&t, r);
                    if !line.is_empty() {
                        let _ = writeln!(text, "    row {r:2}: {line}");
                    }
                }
                println!(
                    "  frame @ bytes 0..{end}: alt={} sync=off\n{text}",
                    t.is_alt_screen_active()
                );
            }
        }
        let mut text = String::new();
        for r in 0..grid_rows_dump(&t) {
            let line = dump_row(&t, r);
            if !line.is_empty() {
                let _ = writeln!(text, "    row {r:2}: {line}");
            }
        }
        println!(
            "  FINAL grid after full stream (alt={}):\n{text}",
            t.is_alt_screen_active()
        );
        println!("  presentable frames: {presented_any}");
    }
}

fn grid_rows_dump(t: &Terminal) -> usize {
    t.grid().num_rows
}

/// One row rendered as visible content; empty rows with default colors
/// produce "". Non-default colors/flags annotate the char compactly:
/// `[│ fg=109,109,109 bg=15,15,15 D]`.
fn dump_row(t: &Terminal, row: usize) -> String {
    use std::fmt::Write;
    use weft_core::grid::CellFlags;
    let grid = t.grid();
    let palette = Color::standard_palette();
    let mut out = String::new();
    for col in 0..grid.num_cols {
        let cell = grid.cell(row, col);
        if cell.flags.contains(CellFlags::WIDE_SPACER) {
            continue;
        }
        let explicit = |c: CellColor| -> String {
            let resolved = super::resolve_cell_color(c, [0.0; 4], &palette);
            match c {
                CellColor::Default => "def".into(),
                _ => format!(
                    "{:.0},{:.0},{:.0}",
                    resolved[0] * 255.0,
                    resolved[1] * 255.0,
                    resolved[2] * 255.0
                ),
            }
        };
        let is_default =
            cell.character == ' ' && cell.fg == CellColor::Default && cell.bg == CellColor::Default;
        if is_default && !cell.flags.intersects(CellFlags::REVERSE | CellFlags::DIM) {
            continue;
        }
        let mut tag = String::new();
        if cell.character != ' ' {
            tag.push(cell.character);
        } else {
            tag.push('·');
        }
        let mut flags = String::new();
        if cell.flags.contains(CellFlags::REVERSE) {
            flags.push('R');
        }
        if cell.flags.contains(CellFlags::DIM) {
            flags.push('D');
        }
        if cell.flags.contains(CellFlags::BOLD) {
            flags.push('B');
        }
        if cell.flags.contains(CellFlags::UNDERLINE) {
            flags.push('U');
        }
        let _ = write!(
            out,
            "[{tag} fg={} bg={} {flags}]",
            explicit(cell.fg),
            explicit(cell.bg)
        );
    }
    out
}

/// Byte-level trace of DEC 2026 state transitions: prints every offset where
/// `synchronized_output()` flips. Exposes any code path that silently clears
/// an open sync window mid-stream. Pure diagnostics — ignored by default;
/// run with `cargo test --bin weft startup_replay -- --ignored --nocapture`.
#[test]
#[ignore = "pure diagnostics; run explicitly with --ignored"]
fn forensic_trace_sync_state_transitions() {
    for (rows, cols, label) in [(24usize, 80usize, "capture"), (62, 200, "dogfood")] {
        let data = STARTUP_BYTES;
        let mut t = Terminal::new(rows, cols);
        let mut prev = false;
        println!("--- sync transitions {label} {rows}x{cols} ---");
        for (i, b) in data.iter().enumerate() {
            t.process(std::slice::from_ref(b));
            let now = t.synchronized_output();
            if now != prev {
                println!("  byte {i}: sync {prev} -> {now}");
                prev = now;
            }
        }
    }
}

/// Forensic dump (Phase 0 deliverable). Prints per-batch evidence for three
/// chunkings x two geometries. Assertions live in the tests below. Pure
/// diagnostics — ignored by default; run with `cargo test --bin weft
/// startup_replay -- --ignored --nocapture`.
#[test]
#[ignore = "pure diagnostics; run explicitly with --ignored"]
fn forensic_dump_opencode_startup_frames() {
    let configs = [
        (
            "capture 24x80 whole-stream",
            ReplayConfig::capture_geometry(8192),
        ),
        (
            "capture 24x80 256B chunks",
            ReplayConfig::capture_geometry(256),
        ),
        (
            "capture 24x80 64B chunks",
            ReplayConfig::capture_geometry(64),
        ),
        (
            "dogfood 62x200 256B chunks",
            ReplayConfig::dogfood_geometry(256),
        ),
    ];
    for (label, cfg) in configs {
        summarize(label, &cfg);
    }
}

/// Regression invariant: replaying the real opencode startup stream must
/// never surface a near-white background bar or white underline rail through
/// the grid pipeline under ANY chunking, regardless of whether the DEC 2026
/// gate holds (worst case: every intermediate frame is presented).
///
/// The underline/rail invariants bind FIX_OPENCODE_STARTUP_FLASH: the stream
/// NEVER emits SGR 4/21/24, so every UNDERLINE-flagged cell and every white
/// decoration quad is the private-SGR (`CSI >4;1m`) misparse leaking through
/// the parser → the white-fg space-cleared rows painted full-width white
/// rails. See `crates/weft_core/src/vt/tests.rs`:
/// `csi_gt_private_sgr_is_not_text_attributes`.
#[test]
fn no_bright_bar_in_any_presented_or_intermediate_grid_frame() {
    for (rows, cols, label) in [(24usize, 80usize, "capture"), (62, 200, "dogfood")] {
        for batch in [8192usize, 256, 64] {
            let cfg = ReplayConfig {
                rows,
                cols,
                batch,
                minimum_contrast: 7.0,
            };
            let frames = replay_in_batches(&cfg);
            for f in &frames {
                assert!(
                    f.bright_bars.is_empty(),
                    "{label} {rows}x{cols} batch={batch}: frame {} (bytes {:?}) paints \
                     bright bars: {:?}",
                    f.batch,
                    f.byte_range,
                    f.bright_bars
                );
                assert_eq!(
                    f.underline_flagged_cells, 0,
                    "{label} {rows}x{cols} batch={batch}: frame {} (bytes {:?}) has \
                     UNDERLINE cells the stream never requested (private-SGR \
                     misparse leak): {} cells",
                    f.batch, f.byte_range, f.underline_flagged_cells
                );
                assert_eq!(
                    f.bright_decoration_rails, 0,
                    "{label} {rows}x{cols} batch={batch}: frame {} (bytes {:?}) emits \
                     {} near-white decoration rails (the white-bar flash)",
                    f.batch, f.byte_range, f.bright_decoration_rails
                );
            }
        }
    }
}
