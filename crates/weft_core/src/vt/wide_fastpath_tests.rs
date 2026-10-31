//! WP1 (v1.17.0 PLAN_v1.17.0_WP1_CJK_FASTPATH §3) — UTF-8 wide-char fast
//! lane semantics钉死 tests.
//!
//! - Differential fuzz: fixed-seed mixed streams (ASCII / CJK / ESC / OSC /
//!   SGR / C0 / ZWJ / VS16 / RI / modifiers / malformed bytes) in RANDOM
//!   chunk boundaries — including cuts inside multi-byte sequences (the
//!   lane/slow divergence point is exactly a cross-batch partial sequence) —
//!   against the cfg(test) `force_slow_path_for_test` baseline (no
//!   production flag). Comparisons cover the R2 scope: every cell
//!   (char/width/fg/bg/flags/underline), grapheme extras, cursor,
//!   scroll_offset, scrollback depth, wrapped flags.
//! - Capture equivalence: block text + style RLE + truncation marker
//!   (in-flight, finalized, over-cap, preexec-staging orphan).
//! - D3 admission pinning + malformed-UTF-8 directed cases; D4
//!   full-scalar-domain width sweep against unicode_width.
//!
//! Deterministic: fixed xorshift64 seeds, no external RNG dependency
//! (occ_property_tests / fuzz_lite precedent).

use super::Terminal;
use crate::blocks::{CapturedStyle, OutputCapture};
use crate::grid::{terminal_char_width, CellColor};

// ── Deterministic PRNG (xorshift64, fuzz_lite precedent) ────────────────

struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut state = self.0;
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        self.0 = state;
        state
    }

    fn below(&mut self, limit: u64) -> u64 {
        self.next_u64() % limit.max(1)
    }
}

fn seed_for(index: u64) -> u64 {
    0x9E37_79B9_7F4A_7C15 ^ index.wrapping_mul(0xBF58_476D_1CE4_E5B9)
}

// ── Differential harness ────────────────────────────────────────────────

/// A lane-ON terminal and its lane-OFF twin (pure vte per-byte baseline).
fn paired(rows: usize, cols: usize) -> (Terminal, Terminal) {
    let mut slow = Terminal::new(rows, cols);
    slow.force_slow_path_for_test();
    (Terminal::new(rows, cols), slow)
}

/// R2 comparison scope: every cell, grapheme, cursor, scroll_offset,
/// scrollback depth, wrapped flags.
fn assert_lane_equivalent(fast: &Terminal, slow: &Terminal, context: &str) {
    let (g, s) = (fast.grid(), slow.grid());
    assert_eq!(g.num_rows, s.num_rows, "rows diverged ({context})");
    assert_eq!(g.num_cols, s.num_cols, "cols diverged ({context})");
    for r in 0..g.num_rows {
        for c in 0..g.num_cols {
            let (a, b) = (g.cell(r, c), s.cell(r, c));
            assert_eq!(
                a.character, b.character,
                "cell char diverged r{r} c{c} ({context})"
            );
            assert_eq!(
                a.width, b.width,
                "cell width diverged r{r} c{c} ({context})"
            );
            assert_eq!(a.fg, b.fg, "cell fg diverged r{r} c{c} ({context})");
            assert_eq!(a.bg, b.bg, "cell bg diverged r{r} c{c} ({context})");
            assert_eq!(
                a.flags, b.flags,
                "cell flags diverged r{r} c{c} ({context})"
            );
            assert_eq!(
                a.underline_style, b.underline_style,
                "underline style diverged r{r} c{c} ({context})"
            );
            assert_eq!(
                a.underline_color, b.underline_color,
                "underline color diverged r{r} c{c} ({context})"
            );
            assert_eq!(
                g.grapheme_at(r, c),
                s.grapheme_at(r, c),
                "grapheme diverged r{r} c{c} ({context})"
            );
        }
        assert_eq!(
            g.viewport[r].wrapped, s.viewport[r].wrapped,
            "wrapped flag diverged row {r} ({context})"
        );
    }
    assert_eq!(
        g.cursor.row, s.cursor.row,
        "cursor row diverged ({context})"
    );
    assert_eq!(
        g.cursor.col, s.cursor.col,
        "cursor col diverged ({context})"
    );
    assert_eq!(
        g.cursor.wrap_pending, s.cursor.wrap_pending,
        "wrap_pending diverged ({context})"
    );
    assert_eq!(
        g.cursor.visible, s.cursor.visible,
        "cursor visibility diverged ({context})"
    );
    assert_eq!(
        g.scroll_offset(),
        s.scroll_offset(),
        "scroll_offset diverged ({context})"
    );
    assert_eq!(
        g.scrollback_len(),
        s.scrollback_len(),
        "scrollback depth diverged ({context})"
    );
}

// ── Byte generator ──────────────────────────────────────────────────────

const CJK: &[char] = &['中', '文', '测', '试', '数', '据', '全', '角', '字', '符'];

/// Other admitted width-2 families: Hangul / Hiragana / fullwidth Latin /
/// emoji (no modifier) / CJK punctuation.
const WIDE_OTHER: &[char] = &['가', 'な', 'Ａ', '１', '\u{1f44d}', '、', '…'];

/// Characters the D3 exemption table keeps in the slow path.
const NARROW_SPECIAL: &[char] = &[
    '\u{200d}',  // ZWJ
    '\u{fe0f}',  // VS16
    '\u{1f3fb}', // emoji modifier (skin tone)
    '\u{1f1fa}', // Regional Indicator (first half)
    '\u{1f1f8}', // Regional Indicator (second half)
    '\u{0301}',  // combining acute
    '\u{ff71}',  // halfwidth katakana (width 1)
    '\u{303f}',  // IDEOGRAPHIC HALF FILL SPACE (width 1)
    'é',         // 2-byte Latin (width 1)
];

/// Malformed / hostile bytes the lane must never consume.
const INVALID_BYTES: &[u8] = &[0x80, 0xC0, 0xC1, 0xF5, 0xFF, 0x7F, 0x01, 0x1A];

fn push_utf8(v: &mut Vec<u8>, c: char) {
    let mut buf = [0u8; 4];
    v.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
}

fn gen_chunk(rng: &mut Rng, budget: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(budget * 4);
    while v.len() < budget {
        match rng.below(24) {
            // Printable ASCII run.
            0..=7 => {
                let n = 1 + rng.below(6);
                for _ in 0..n {
                    v.push(0x21 + rng.below(0x5E) as u8);
                }
            }
            // CJK run.
            8..=11 => {
                let n = 1 + rng.below(4);
                for _ in 0..n {
                    push_utf8(&mut v, CJK[rng.below(CJK.len() as u64) as usize]);
                }
            }
            // Other admitted wide chars.
            12 => push_utf8(
                &mut v,
                WIDE_OTHER[rng.below(WIDE_OTHER.len() as u64) as usize],
            ),
            // SGR / cursor CSI.
            13 => {
                v.extend_from_slice(b"\x1b[");
                match rng.below(4) {
                    0 => v.extend_from_slice(b"31m"),
                    1 => v.extend_from_slice(b"0m"),
                    2 => v.extend_from_slice(b"1;4m"),
                    _ => v.extend_from_slice(b"K"),
                }
            }
            // Bare ESC (lane terminator).
            14 => v.push(0x1B),
            // OSC 8 hyperlink / OSC 133 marker / OSC 0 title.
            15 => match rng.below(3) {
                0 => v.extend_from_slice(b"\x1b]8;;https://example.test\x1b\\"),
                1 => v.extend_from_slice(b"\x1b]8;;\x1b\\"),
                _ => v.extend_from_slice(b"\x1b]133;C\x07"),
            },
            16 => v.push(b'\r'),
            17 => v.push(b'\n'),
            18 => v.push(b'\t'),
            19 => push_utf8(
                &mut v,
                NARROW_SPECIAL[rng.below(NARROW_SPECIAL.len() as u64) as usize],
            ),
            // Malformed bytes.
            20 => v.push(INVALID_BYTES[rng.below(INVALID_BYTES.len() as u64) as usize]),
            // Truncated multi-byte lead pair (cross-batch hang fodder).
            21 => v.extend_from_slice(&[0xE4, 0xB8]),
            22 => v.extend_from_slice(&[0xF0, 0x9F]),
            // Backspace + erase line (capture rewrite paths).
            _ => match rng.below(2) {
                0 => v.push(0x08),
                _ => v.extend_from_slice(b"\x1b[2K\r"),
            },
        }
    }
    v
}

/// Feed `bytes` to both terminals in random chunk boundaries, asserting
/// equivalence after EVERY chunk (so a divergence reports its exact step).
fn feed_differential(
    fast: &mut Terminal,
    slow: &mut Terminal,
    bytes: &[u8],
    rng: &mut Rng,
    seed: u64,
    step: usize,
) {
    let mut off = 0usize;
    let mut cut = 0usize;
    while off < bytes.len() {
        let n = match rng.below(8) {
            0 => 1,                         // byte-at-a-time
            1 => 1 + rng.below(2) as usize, // tiny (splits multibyte often)
            _ => 1 + rng.below(24) as usize,
        };
        let end = (off + n).min(bytes.len());
        fast.process(&bytes[off..end]);
        slow.process(&bytes[off..end]);
        let ctx = format!("seed {seed} step {step} cut {cut} bytes[..{})", end);
        assert_lane_equivalent(fast, slow, &ctx);
        off = end;
        cut += 1;
    }
}

/// One differential seed run: mixed stream, random chunk splits.
fn run_lane_differential_seed(seed: u64) {
    const ROWS: usize = 8;
    const COLS: usize = 20;
    const STEPS: usize = 24;

    let mut rng = Rng(seed);
    let (mut fast, mut slow) = paired(ROWS, COLS);
    for step in 0..STEPS {
        let budget = 48 + rng.below(80) as usize;
        let bytes = gen_chunk(&mut rng, budget);
        feed_differential(&mut fast, &mut slow, &bytes, &mut rng, seed, step);
    }
}

macro_rules! lane_differential_tests {
    ($($index:literal => $name:ident),+ $(,)?) => {
        $(
            #[test]
            fn $name() {
                run_lane_differential_seed(seed_for($index));
            }
        )+
    };
}

lane_differential_tests! {
    0 => lane_diff_seed_00, 1 => lane_diff_seed_01, 2 => lane_diff_seed_02,
    3 => lane_diff_seed_03, 4 => lane_diff_seed_04, 5 => lane_diff_seed_05,
    6 => lane_diff_seed_06, 7 => lane_diff_seed_07, 8 => lane_diff_seed_08,
    9 => lane_diff_seed_09, 10 => lane_diff_seed_10, 11 => lane_diff_seed_11,
}

/// Directed extreme split: the stream fed one byte at a time — every
/// multi-byte sequence hangs across a batch boundary, and the lane must
/// leave the hanging bytes for the slow path on every batch.
#[test]
fn byte_at_a_time_feed_matches_slow_path() {
    const ROWS: usize = 6;
    const COLS: usize = 16;
    let (mut fast, mut slow) = paired(ROWS, COLS);
    // CJK + emoji + ZWJ sequence + RI flag + SGR + malformed, all together.
    let stream: Vec<u8> = "中文测试emoji序列"
        .chars()
        .chain(['\u{1f44d}', '\u{fe0f}', '\u{1f1fa}', '\u{1f1f8}'])
        .flat_map(|c| c.to_string().into_bytes())
        .chain(b"\x1b[31mred\xe4\xb8\xad\x80end".to_vec())
        .chain(b"\r\n".to_vec())
        .chain([0xF0u8, 0x9F, 0x98])
        .chain([0x80u8])
        .chain("完成".chars().flat_map(|c| c.to_string().into_bytes()))
        .collect();
    for (i, b) in stream.iter().enumerate() {
        fast.process(&[*b]);
        slow.process(&[*b]);
        let ctx = format!("byte {i} ({b:#04x})");
        assert_lane_equivalent(&fast, &slow, &ctx);
    }
}

// ── D3 admission set (scan_wide_run) ────────────────────────────────────

#[test]
fn scan_admits_consecutive_width_two_runs() {
    assert_eq!(
        Terminal::scan_wide_run("中文测试".as_bytes()).map(str::len),
        Some(12)
    );
    assert_eq!(
        Terminal::scan_wide_run("가나다".as_bytes()).map(str::len),
        Some(9)
    ); // Hangul
    assert_eq!(
        Terminal::scan_wide_run("あいう".as_bytes()).map(str::len),
        Some(9)
    ); // Hiragana
    assert_eq!(
        Terminal::scan_wide_run("ＡＢ".as_bytes()).map(str::len),
        Some(6)
    ); // Fullwidth
    assert_eq!(
        Terminal::scan_wide_run("、。".as_bytes()).map(str::len),
        Some(6)
    ); // CJK punct
    assert_eq!(
        Terminal::scan_wide_run("\u{1f44d}".as_bytes()).map(str::len),
        Some(4)
    ); // emoji
    assert_eq!(
        Terminal::scan_wide_run("中文测试".as_bytes()).unwrap(),
        "中文测试",
        "the borrow carries the validated evidence (P2-1)"
    );
}

#[test]
fn scan_stops_at_ascii_and_lane_terminators() {
    assert_eq!(Terminal::scan_wide_run("中文a".as_bytes()), Some("中文"));
    assert_eq!(Terminal::scan_wide_run("中文\r\n".as_bytes()), Some("中文"));
    assert_eq!(
        Terminal::scan_wide_run("中文\u{1b}[31m".as_bytes()),
        Some("中文")
    );
    assert_eq!(
        Terminal::scan_wide_run("中文\u{7f}".as_bytes()),
        Some("中文")
    );
}

/// D3 exemption table: every family the plan keeps in the slow path.
#[test]
fn scan_rejects_d3_exempt_families() {
    assert_eq!(Terminal::scan_wide_run("🇺🇸".as_bytes()), None, "RI pairing");
    assert_eq!(
        Terminal::scan_wide_run("\u{1f1fa}".as_bytes()),
        None,
        "single RI"
    );
    assert_eq!(Terminal::scan_wide_run("\u{200d}".as_bytes()), None, "ZWJ");
    assert_eq!(Terminal::scan_wide_run("\u{fe0f}".as_bytes()), None, "VS16");
    assert_eq!(
        Terminal::scan_wide_run("\u{1f3fb}".as_bytes()),
        None,
        "modifier"
    );
    assert_eq!(
        Terminal::scan_wide_run("\u{0301}".as_bytes()),
        None,
        "combining"
    );
    assert_eq!(
        Terminal::scan_wide_run("ｱ".as_bytes()),
        None,
        "halfwidth kana"
    );
    assert_eq!(
        Terminal::scan_wide_run("\u{303f}".as_bytes()),
        None,
        "half fill space (width 1)"
    );
    assert_eq!(
        Terminal::scan_wide_run("é".as_bytes()),
        None,
        "2-byte Latin"
    );
}

#[test]
fn scan_never_consumes_malformed_or_trailing_partial_sequences() {
    // Malformed leads.
    assert_eq!(Terminal::scan_wide_run(&[0x80]), None);
    assert_eq!(Terminal::scan_wide_run(&[0xC0, 0x80]), None); // overlong
    assert_eq!(Terminal::scan_wide_run(&[0xC1]), None);
    assert_eq!(Terminal::scan_wide_run(&[0xF5]), None);
    assert_eq!(Terminal::scan_wide_run(&[0xFF]), None);
    // Batch-truncated sequences: nothing consumed.
    assert_eq!(Terminal::scan_wide_run(&[0xE4, 0xB8]), None);
    assert_eq!(Terminal::scan_wide_run(&[0xF0, 0x9F, 0x98]), None);
    // Admitted run ends at a malformed byte — prefix kept, rest untouched.
    assert_eq!(
        Terminal::scan_wide_run(b"\xe4\xb8\xad\xe6\x96\x87\x80"),
        Some("中文")
    );
    assert_eq!(
        Terminal::scan_wide_run(b"\xe4\xb8\xad\xff\xe6\x96\x87"),
        Some("中")
    );
    // Stray continuation byte inside an otherwise valid candidate.
    assert_eq!(
        Terminal::scan_wide_run(b"\xe4\xb8\xad\x80\xe6\x96\x87"),
        Some("中")
    );
}

// ── Malformed UTF-8 directed differential ───────────────────────────────

#[test]
fn malformed_utf8_streams_match_slow_path() {
    const COLS: usize = 12;
    let cases: &[&[u8]] = &[
        &[0x80],
        &[0xC0, 0x80],
        &[0xC1, 0xBF],
        &[0xFF, 0xFF],
        &[0xE4, 0xB8, 0xAD, 0x80, 0xE6, 0x96, 0x87], // 中 0x80 文
        &[0xF0, 0x9F, 0x98, 0x80, 0x80],             // 👀 then stray 0x80
        &[0xE0, 0x80, 0x80],                         // overlong 3-byte
        b"\xe4\xb8",                                 // hanging lead pair
        "中\u{e4}文".as_bytes(),                     // lead between wide chars
        b"\x1b[31m\xe4\xb8\xad\x80\x1b[0m",          // malformed inside SGR
    ];
    for (i, case) in cases.iter().enumerate() {
        let (mut fast, mut slow) = paired(4, COLS);
        // Split once mid-sequence where possible to exercise cross-batch hangs.
        let mid = case.len() / 2;
        fast.process(&case[..mid]);
        slow.process(&case[..mid]);
        fast.process(&case[mid..]);
        slow.process(&case[mid..]);
        let ctx = format!("malformed case {i}");
        assert_lane_equivalent(&fast, &slow, &ctx);
    }
}

#[test]
fn hanging_multibyte_across_batches_completes_identically() {
    const COLS: usize = 12;
    let (mut fast, mut slow) = paired(4, COLS);
    let full = "中文测试数据".as_bytes();
    // Feed lead, then continuations one at a time, then the rest as one run.
    for chunk in full.chunks(1).take(4) {
        fast.process(chunk);
        slow.process(chunk);
    }
    fast.process(&full[4..]);
    slow.process(&full[4..]);
    assert_lane_equivalent(&fast, &slow, "hanging sequence completion");
    assert_eq!(fast.grid().row_text(0), slow.grid().row_text(0));
}

// ── Resize race (§3) ────────────────────────────────────────────────────

/// Narrowing resize with the cursor past the new right edge (the
/// perform.rs resize-race branch, now shared by the lane): both lanes must
/// wrap identically and lose nothing.
#[test]
fn wide_print_after_narrowing_resize_matches_slow_path() {
    let (mut fast, mut slow) = paired(4, 12);
    // Fill rows so the cursor is mid-grid, then narrow the grid the way the
    // dimension-only path does (rows stay wide internally; num_cols clips).
    fast.process("中文中文中文中文中文中文".as_bytes());
    slow.process("中文中文中文中文中文中文".as_bytes());
    fast.resize(4, 6);
    slow.resize(4, 6);
    fast.process("测试数据继续输出".as_bytes());
    slow.process("测试数据继续输出".as_bytes());
    assert_lane_equivalent(&fast, &slow, "post-resize wide print");
}

/// Directed: cursor parked past num_cols (the in-flight SIGWINCH window)
/// hits the print core's col >= num_cols branch with wide chars.
#[test]
fn wide_print_with_cursor_past_num_cols_matches_slow_path() {
    let (mut fast, mut slow) = paired(4, 6);
    for t in [&mut fast, &mut slow] {
        // Simulate the race window: grid narrowed, PTY cursor still at the
        // old column.
        t.grid_mut().resize_dims(4, 6);
        t.grid_mut().cursor.col = 10;
        t.grid_mut().cursor.row = 1;
    }
    fast.process("中文测试".as_bytes());
    slow.process("中文测试".as_bytes());
    assert_lane_equivalent(&fast, &slow, "cursor past num_cols wide print");
}

// ── Scroll interaction (R2-P1: run-level snap) ──────────────────────────

/// User is scrolled back while output streams: the run-level snap must land
/// in the same observable state as the per-char snap.
#[test]
fn run_level_scroll_snap_matches_per_char_snap() {
    let (mut fast, mut slow) = paired(6, 12);
    // Produce scrollback, then scroll both users back the same amount.
    let filler = "行内容数据测试".as_bytes();
    for t in [&mut fast, &mut slow] {
        t.process(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
        for _ in 0..12 {
            t.process(filler);
            t.process(b"\r\n");
        }
        t.grid_mut().set_scroll_offset(5);
        assert!(t.grid().scroll_offset() > 0);
    }
    assert_lane_equivalent(&fast, &slow, "scrolled baseline");
    // Streaming output in CommandExecuting snaps the offset to 0 in both.
    fast.process("中文输出".as_bytes());
    slow.process("中文输出".as_bytes());
    assert_lane_equivalent(&fast, &slow, "after snap print");
    assert_eq!(
        fast.grid().scroll_offset(),
        0,
        "run-level snap resets offset"
    );
    // History-browsing flag suppresses the snap in both lanes alike.
    for t in [&mut fast, &mut slow] {
        // set_primary_history_view(true) snaps the offset to 0 — scroll back
        // AFTER entering the history view.
        t.set_primary_history_view(true);
        t.grid_mut().set_scroll_offset(4);
    }
    fast.process("中文浏览".as_bytes());
    slow.process("中文浏览".as_bytes());
    assert_lane_equivalent(&fast, &slow, "history-view print");
    assert_eq!(
        fast.grid().scroll_offset(),
        4,
        "primary_history_view must suppress the snap"
    );
}

// ── Capture equivalence (text + style RLE + truncated) ──────────────────

fn marker_stream() -> Vec<u8> {
    let mut v = Vec::new();
    for i in 0..40 {
        v.extend_from_slice(format!("line{i:02} ").into_bytes().as_slice());
        v.extend_from_slice("中文数据测试样例行".as_bytes());
        if i % 3 == 0 {
            v.extend_from_slice("\u{1b}[32mGREEN中文\u{1b}[0m".as_bytes());
        }
        if i % 5 == 0 {
            v.extend_from_slice(
                "\u{1b}]8;;https://example.test/\u{1b}\\链接\u{1b}]8;;\u{1b}\\".as_bytes(),
            );
        }
        v.extend_from_slice(b"\r\n");
    }
    v
}

type CapturedBlock = (String, Option<std::sync::Arc<crate::blocks::StyledOutput>>);

/// Stream through an in-flight capture; return the LIVE text buffer (the
/// capture's own text, both lanes see the same bytes) and the FINALIZED
/// block (text + style RLE + truncation state via take_styled). The live
/// styled SNAPSHOT is deliberately not compared: it is a 100ms-throttled
/// rendering cache published per run (fast) vs per char (slow) — mid-batch
/// content is timing-dependent by design and converges at finalize.
fn capture_live_and_final(t: &mut Terminal, stream: &[u8]) -> (String, CapturedBlock) {
    t.process(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
    t.process(stream);
    let live = t
        .block_tracker()
        .in_flight()
        .map(|f| f.output.to_string())
        .unwrap_or_default();
    t.process(b"\x1b]133;D;0\x07");
    let block = t.block_tracker().session_blocks().last().unwrap();
    let fin = (block.output.to_string(), block.styled_output.clone());
    (live, fin)
}
#[test]
fn capture_text_style_and_truncation_match_slow_path() {
    let stream = marker_stream();
    let (mut fast, mut slow) = paired(8, 40);
    let (f_live, f_fin) = capture_live_and_final(&mut fast, &stream);
    let (s_live, s_fin) = capture_live_and_final(&mut slow, &stream);
    assert_eq!(f_live, s_live, "live capture text diverged between lanes");
    assert_eq!(f_fin.0, s_fin.0, "captured text diverged between lanes");
    assert_eq!(
        f_fin.1, s_fin.1,
        "captured style RLE diverged between lanes"
    );
    assert!(f_fin.0.contains("中文数据测试样例行"));
    assert!(f_fin.1.is_some(), "SGR runs must be captured");
}

/// Over-cap CJK output: truncation marker (proving `truncated` flipped in
/// both) and the retained text must be lane-identical.
#[test]
fn capture_over_cap_truncation_marker_matches_slow_path() {
    let (mut fast, mut slow) = paired(8, 40);
    for t in [&mut fast, &mut slow] {
        // Tracker clamps to >= 1 MiB; 1 MiB cap with a ~1.2 MiB stream.
        t.set_block_output_cap(1);
    }
    let mut stream = Vec::new();
    for i in 0..24_000 {
        stream.extend_from_slice(format!("{i:06} ").into_bytes().as_slice());
        stream.extend_from_slice("中文测试数据行内容样本评估".as_bytes());
        stream.extend_from_slice(b"\r\n");
    }
    let (f_live, f_fin) = capture_live_and_final(&mut fast, &stream);
    let (s_live, s_fin) = capture_live_and_final(&mut slow, &stream);
    assert_eq!(f_live, s_live, "over-cap live text diverged");
    assert_eq!(f_fin.0, s_fin.0, "over-cap captured text diverged");
    assert!(
        f_fin.0.contains("…(block excerpt truncated at 1 MiB"),
        "truncation marker missing"
    );
    assert_eq!(f_fin.1, s_fin.1);
}

/// Preexec staging (editor submit → orphan `133;D`) routes wide runs through
/// `OutputCapture::print_str` — the synthesized block must match the slow
/// path byte for byte.
#[test]
fn preexec_staging_orphan_block_matches_slow_path() {
    let (mut fast, mut slow) = paired(6, 40);
    for t in [&mut fast, &mut slow] {
        t.process(b"\x1b]133;A\x07");
        let _ = t.run_command("echo nonexistent-file");
    }
    // Editor echo noise (lands in staging: AtPrompt + submitted command).
    let echo: Vec<u8> = "echo 中文测试文件名\r\n"
        .bytes()
        .chain("更多中文输出\r\n".bytes())
        .collect();
    fast.process(&echo);
    slow.process(&echo);
    // Orphan command end (no 133;B) synthesizes the block from staging.
    fast.process(b"\x1b]133;D;2\x07");
    slow.process(b"\x1b]133;D;2\x07");
    let bf = fast.block_tracker().session_blocks().last().unwrap();
    let bs = slow.block_tracker().session_blocks().last().unwrap();
    assert_eq!(bf.output, bs.output, "staged orphan block text diverged");
    assert_eq!(bf.styled_output, bs.styled_output);
    assert!(bf.output.contains("中文测试文件名"));
}

// ── OutputCapture::print_str cap boundary (§3 cap 边界专项) ─────────────

fn style() -> CapturedStyle {
    CapturedStyle::default()
}

/// Drain the capture; split off the finalize-only truncation marker so
/// assertions address content and the `truncated` flag separately.
fn take_text(cap: &mut OutputCapture) -> (String, bool) {
    let (text, _) = cap.take_styled();
    match text.find("\n…(block excerpt truncated") {
        Some(i) => (text[..i].to_string(), true),
        None => (text, false),
    }
}

#[test]
fn print_str_truncates_on_char_boundary_never_mid_char() {
    let mut cap = OutputCapture::default();
    // 12-byte run against a 10-byte cap: accepted backs up from byte 10 to
    // the char boundary at 9 — three whole chars, nothing partial stored.
    cap.print_str("中文中文", style(), 10);
    let (text, truncated) = take_text(&mut cap);
    assert_eq!(text, "中文中", "truncation must keep whole chars only");
    assert!(truncated);

    let mut cap = OutputCapture::default();
    cap.print_str("中文", style(), 6);
    let (text, truncated) = take_text(&mut cap);
    assert_eq!(text, "中文", "exact-fit boundary must not truncate");
    assert!(!truncated);

    let mut cap = OutputCapture::default();
    cap.print_str("中文中文", style(), 8);
    let (text, truncated) = take_text(&mut cap);
    assert_eq!(text, "中文", "8 leaves 2 bytes — second pair cannot fit");
    assert!(truncated);

    let mut cap = OutputCapture::default();
    cap.print_str("中文", style(), 0);
    let (text, truncated) = take_text(&mut cap);
    assert!(text.is_empty(), "zero cap stores nothing");
    assert!(truncated);
}

#[test]
fn print_str_after_truncation_is_inert_and_ascii_keeps_working() {
    let mut cap = OutputCapture::default();
    cap.print_str("中文", style(), 4); // truncated (needs 6)
    cap.print_str("更多数据", style(), 64); // must be dropped wholesale
    let (text, truncated) = take_text(&mut cap);
    assert!(truncated);
    assert_eq!(text, "中", "post-truncation prints are dropped");
}

#[test]
fn print_str_maintains_t16a_ledgers() {
    let mut cap = OutputCapture::default();
    cap.print_str("中文\n数据\n", style(), 1024);
    assert_eq!(cap.text_char_count(), 6, "chars().count() ledger");
    assert_eq!(cap.text_line_count(), 2, "newline ledger");
    let (text, truncated) = take_text(&mut cap);
    assert!(!truncated);
    assert_eq!(text, "中文\n数据");
}

#[test]
fn print_str_mid_document_falls_back_to_per_char_print() {
    let mut cap = OutputCapture::default();
    cap.print_str("abc", style(), 64);
    cap.carriage_return();
    cap.print_str("中文", style(), 64);
    let (text, truncated) = take_text(&mut cap);
    assert!(!truncated);
    assert_eq!(
        text, "中文c",
        "mid-document overwrite uses print() semantics"
    );
}

#[test]
fn print_str_style_splice_is_char_aligned() {
    let mut cap = OutputCapture::default();
    cap.print_str("默认", style(), 64);
    let mut red = style();
    red.fg = CellColor::Palette(1);
    red.ansi_owned = true;
    cap.print_str("红色", red, 64);
    let (text, styled) = cap.take_styled();
    assert_eq!(text, "默认红色");
    let styled = styled.expect("non-default runs must survive");
    let line = styled.line(0).expect("line 0 styled");
    // The red run must cover exactly chars 2..4 (char-space, not byte-space).
    assert!(
        line.foregrounds.iter().any(|s| s.start == 2 && s.end == 4),
        "style run must be char-indexed over the CJK suffix: {:?}",
        line.foregrounds
    );
}

/// Negative control pinning WHY `print_str` exists: the ASCII twin
/// conflates bytes with chars and silently swallows a split CJK char
/// (plan P0 evidence, unchanged — ASCII-lane callers guarantee ASCII).
#[test]
fn print_ascii_documented_byte_char_conflation_is_unchanged() {
    let mut cap = OutputCapture::default();
    cap.print_ascii(&[0xE4, 0xB8], style(), 64); // half a 中
    let (text, truncated) = take_text(&mut cap);
    assert!(!truncated);
    assert!(text.is_empty(), "print_ascii cannot store an invalid chunk");
}

// ── D4: full-scalar-domain width sweep ──────────────────────────────────

/// `terminal_char_width` must equal
/// `unicode_width(...).unwrap_or(0)` over the ENTIRE scalar domain except
/// the two documented terminal conventions (RI = 2, emoji modifier = 0).
/// This pins the D4 range-first fast path value-for-value (排除法).
#[test]
fn terminal_char_width_matches_unicode_width_everywhere() {
    let mut mismatches: Vec<(u32, usize, usize)> = Vec::new();
    for cp in 0u32..=0x10FFFF {
        let Some(ch) = char::from_u32(cp) else {
            continue; // surrogates are not Rust chars
        };
        let expected = if matches!(ch, '\u{1f3fb}'..='\u{1f3ff}') {
            0 // emoji modifier: cluster-joining, never a cell
        } else if matches!(ch, '\u{1f1e6}'..='\u{1f1ff}') {
            2 // RI: terminal double-width convention
        } else {
            unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0)
        };
        let actual = terminal_char_width(ch);
        if actual != expected {
            mismatches.push((cp, actual, expected));
        }
    }
    assert!(
        mismatches.is_empty(),
        "terminal_char_width diverged from unicode_width at {} code point(s), first 16: {:?}",
        mismatches.len(),
        &mismatches[..mismatches.len().min(16)]
    );
}
