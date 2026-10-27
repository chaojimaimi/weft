//! PLAN_v1153 §2.2 防线 2 — `Row.occ` property tests.
//!
//! Randomized mixed operation streams driven through the real VT parser,
//! asserting after EVERY operation that `occ - 1` equals the pre-occ
//! reverse-scan oracle (`Row::last_written_cell`, kept verbatim under
//! `#[cfg(test)]`). The generator deliberately covers the op classes the
//! seq-style streams cannot reach (the P1-1 lesson): ICH/DCH/IL/DL/ED/EL,
//! ECH, DECSTBM region scrolls, wide chars at the wrap boundary, CJK
//! splats, and combining marks — the sites where whole-row dirty marks,
//! per-cell shifts, and erase paths could pin or drift `occ`.
//!
//! Deterministic: fixed xorshift64 seeds, no external RNG dependency.

use super::Terminal;
use crate::grid::{Grid, Row};

/// The oracle: `occ` must equal `last_written_cell() + 1` exactly.
fn audit_row(row: &Row, ctx: &str) {
    assert_eq!(
        row.occ as isize,
        row.last_written_cell() + 1,
        "{ctx}: occ drifted from the reverse-scan oracle"
    );
}

fn audit_grid(grid: &Grid, label: &str) {
    for (i, row) in grid.viewport.iter().enumerate() {
        audit_row(row, &format!("{label}: viewport row {i}"));
    }
}

/// Sparse round-trip audit: materialize one history row through the flat
/// path and check its reconstructed `occ` too.
fn audit_one_history_row(grid: &Grid, label: &str) {
    let len = grid.scrollback.len();
    if len == 0 {
        return;
    }
    let index = len - 1; // newest row: the one just pushed by a scroll
    if let Some(row) = grid.scrollback.get(index) {
        audit_row(&row, &format!("{label}: materialized history row {index}"));
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// One random VT operation as raw bytes.
fn random_op(t: &Terminal, rng: &mut Rng) -> String {
    let cols = t.grid().num_cols as u64;
    let rows = t.grid().num_rows as u64;
    match rng.below(20) {
        // ASCII run (print path + ascii-run fast path)
        0 => {
            let n = 1 + rng.below(6);
            (0..n)
                .map(|_| (b'a' + rng.below(26) as u8) as char)
                .collect()
        }
        // CJK / wide chars (standard print + wide splat handling)
        1 => ["中", "日", "😀", "📘"][rng.below(4) as usize].to_string(),
        // Wide char at the LAST column → the wrap-first lead+spacer path
        2 => format!("\x1b[{};{}H中", 1 + rng.below(rows), cols),
        // Combining mark on the previous cell (grapheme append path)
        3 => "e\u{0301}".to_string(),
        // SGR attribute flips (write funnels re-run with style bits set)
        4 => ["\x1b[1m", "\x1b[0m", "\x1b[3m", "\x1b[38;5;196m", "\x1b[4m"][rng.below(5) as usize]
            .to_string(),
        // ICH
        5 => format!("\x1b[{}@", 1 + rng.below(4)),
        // DCH
        6 => format!("\x1b[{}P", 1 + rng.below(4)),
        // IL
        7 => format!("\x1b[{}L", 1 + rng.below(3)),
        // DL
        8 => format!("\x1b[{}M", 1 + rng.below(3)),
        // ED
        9 => format!("\x1b[{}J", rng.below(3)),
        // EL
        10 => format!("\x1b[{}K", rng.below(3)),
        // ECH
        11 => format!("\x1b[{}X", 1 + rng.below(4)),
        // DECSTBM region + LF scroll inside it
        12 => {
            let bottom = 2 + rng.below(rows.saturating_sub(1)).max(2);
            format!("\x1b[2;{bottom}r\n\n\n")
        }
        // Reset scroll region
        13 => "\x1b[r".to_string(),
        // Cursor motion
        14 => format!("\x1b[{};{}H", 1 + rng.below(rows), 1 + rng.below(cols)),
        // Relative moves
        15 => ["\x1b[2A", "\x1b[2B", "\x1b[3C", "\x1b[3D"][rng.below(4) as usize].to_string(),
        // LF / CR / BS
        16 => ["\n", "\r", "\r\n", "\x08"][rng.below(4) as usize].to_string(),
        // Wrap pressure: park the cursor on the last column, print ASCII
        17 => format!("\x1b[{};{}Hx", 1 + rng.below(rows), cols),
        // Alt screen flip (moves writes to the alt grid)
        18 => ["\x1b[?1049h", "\x1b[?1049l"][rng.below(2) as usize].to_string(),
        // Tab / runs of spaces
        _ => "\t   ".to_string(),
    }
}

fn run_stream(seed: u64, ops: usize, rows: usize, cols: usize) {
    let mut t = Terminal::new(rows, cols);
    let mut rng = Rng(seed);
    for i in 0..ops {
        let bytes = random_op(&t, &mut rng);
        t.process(bytes.as_bytes());
        audit_grid(t.grid(), &format!("seed {seed} op {i} {bytes:?}"));
        // The alt grid receives writes while an alt-screen app is active.
        if t.capabilities.alt_active {
            audit_grid(&t.alt_grid, &format!("seed {seed} op {i} (alt)"));
        }
        if i % 29 == 0 {
            audit_one_history_row(t.grid(), &format!("seed {seed} op {i}"));
        }
    }
}

#[test]
fn occ_matches_reverse_scan_under_mixed_stream_default_size() {
    for seed in [1u64, 42, 1337, 2026_1027] {
        run_stream(seed, 4_000, 12, 30);
    }
}

#[test]
fn occ_matches_reverse_scan_under_mixed_stream_small_grid() {
    // Tiny grid: wrap / region edges are hit within a handful of ops.
    for seed in [7u64, 99] {
        run_stream(seed, 4_000, 5, 8);
    }
}

#[test]
fn occ_matches_reverse_scan_under_wide_and_erase_barrage() {
    // Directed mix: wide chars + erase/shift ops back to back (the P1-1
    // regression shape — whole-row dirty marks must NOT pin occ).
    let mut t = Terminal::new(6, 10);
    let mut rng = Rng(2026);
    for i in 0..2_000 {
        let op = match rng.below(6) {
            0 => "\x1b[1;10H中日语😀".to_string(),
            1 => format!("\x1b[{}K", rng.below(3)),
            2 => format!("\x1b[{}J", rng.below(3)),
            3 => format!("\x1b[{}@", 1 + rng.below(5)),
            4 => format!("\x1b[{}P", 1 + rng.below(5)),
            _ => ["\x1b[2L", "\x1b[2M", "\x1b[3X", "\r\n"][rng.below(4) as usize].to_string(),
        };
        t.process(op.as_bytes());
        audit_grid(t.grid(), &format!("barrage op {i} {op:?}"));
    }
}
