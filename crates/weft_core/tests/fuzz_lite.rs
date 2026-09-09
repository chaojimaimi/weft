//! fuzz-lite: seeded property tests + directed extreme-input regression
//! tests for the VT parsing layer.
//!
//! Motivation (AUDIT_v1.10.39 stability audit): the parser's robustness was
//! asserted by manual testing and a handful of hand-picked fixtures. This
//! file turns the audit's weak spots into repeatable coverage:
//!
//! 1. Property tests — a deterministic, dependency-free (xorshift64) byte
//!    generator biased toward printable ASCII / CJK / controls / ESC
//!    sequences feeds `Terminal::process` in 1..=4096-byte chunks. After
//!    every chunk: no panic, grid dimensions unchanged, wide/spacer pairs
//!    intact. 48 seeds × 200 steps, same seed => same bytes => same result.
//! 2. Directed tests — one test per audit weakness (OSC raw accumulation,
//!    SGR param floods, max-u16 CSI counts, unbounded grapheme clusters,
//!    the historical wide-char splat, 1-column resize storms).
//!
//! All tests are pure `Terminal` logic — no PTY, no wall clock. The whole
//! file is budgeted to run well under 30 s in the debug profile.

use weft_core::grid::{CellFlags, CellWidth};
use weft_core::vt::Terminal;

// ── Deterministic PRNG ────────────────────────────────────────────────

/// xorshift64 (Marsaglia 2003) — the only PRNG allowed here: no new
/// dependencies, no std hash randomization, identical stream on every
/// platform. All fuzz bytes derive from it, so a failing seed reproduces
/// with a single `cargo test <seed-test-name>`.
struct XorShift64(u64);

impl XorShift64 {
    fn next_u64(&mut self) -> u64 {
        let mut state = self.0;
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        self.0 = state;
        state
    }

    /// Uniform integer in `0..limit`.
    fn below(&mut self, limit: u64) -> u64 {
        self.next_u64() % limit
    }

    /// Uniform pick from a small pool.
    fn pick<T: Copy>(&mut self, pool: &[T]) -> T {
        pool[self.below(pool.len() as u64) as usize]
    }

    /// CSI parameter with the audit's extreme values sampled: 1/16 chance
    /// of `0`, 1/16 chance of `65535` (u16 max — the largest count a
    /// terminal sequence can express), otherwise uniform u16. Extremes are
    /// the ones that historically found clamp bugs.
    fn param_u16(&mut self) -> u16 {
        match self.below(16) {
            0 => 0,
            1 => 65535,
            _ => self.next_u64() as u16,
        }
    }
}

/// 48 distinct seeds (audit floor: >= 48). Derived by a bijective scramble
/// of the index (odd multiplier + xor are permutations mod 2^64), so every
/// index yields a unique seed while staying readable in failure output.
fn seed_for(index: u64) -> u64 {
    0x9E37_79B9_7F4A_7C15 ^ index.wrapping_mul(0xBF58_476D_1CE4_E5B9)
}

// ── Byte generator ────────────────────────────────────────────────────

/// Printable ASCII (55% weight).
const PRINTABLE: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789 !\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~";

/// Common CJK characters (10% weight) — emitted as complete 3-byte UTF-8
/// so the parser sees real multibyte input, not mojibake.
const CJK_COMMON: &[char] = &[
    '中', '文', '测', '试', '数', '据', '全', '角', '字', '符', '你', '好', '世', '界', '行', '列',
];

/// Control characters (10% weight). The 0x07 is also the OSC terminator,
/// so it doubles as a way to close runaway OSC payloads within a chunk.
const CONTROLS: &[u8] = &[b'\r', b'\n', 0x08, 0x09, 0x07];

/// Character-set designators accepted after `ESC (` / `ESC )`.
const CHARSET_SETS: &[u8] = b"0BA";

/// DEC private modes the fuzzer toggles with `?N h/l`.
const DEC_PRIVATE_MODES: &[u16] = &[1, 12, 25, 47, 1000, 1002, 1047, 1049, 2004, 2026, 2027];

/// Attribute codes for small random SGR runs (bold/italic/underline/colors/
/// resets only — never the 38/48/58 param-bearing codes).
const SGR_ATTRS: &[u16] = &[0, 1, 2, 3, 4, 5, 7, 8, 9, 22, 24, 25, 27, 28, 39, 49];

fn push_number(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(value.to_string().as_bytes());
}

/// Emit one ESC-sequence atom into `out`. Well-formed by construction
/// (the fuzzer's job is mixing sequences, not malforming them — malformed
/// bytes come naturally from chunk truncation below).
fn emit_esc_atom(rng: &mut XorShift64, out: &mut Vec<u8>) {
    match rng.below(19) {
        // CUP — absolute cursor addressing.
        0 => {
            out.extend_from_slice(b"\x1b[");
            push_number(out, rng.param_u16());
            out.push(b';');
            push_number(out, rng.param_u16());
            out.push(b'H');
        }
        // CUU / CUD / CUF / CUB — directional cursor moves.
        1..=4 => {
            out.extend_from_slice(b"\x1b[");
            push_number(out, rng.param_u16());
            out.push(b"ABCD"[(rng.below(4)) as usize]);
        }
        // ED / EL — erase display / line.
        5 | 6 => {
            out.extend_from_slice(b"\x1b[");
            push_number(out, rng.param_u16());
            out.push(if rng.below(2) == 0 { b'J' } else { b'K' });
        }
        // ICH / DCH / IL / DL / SU / SD — insert/delete/scroll storms.
        // `param_u16` supplies the audit's 0 and 65535 extremes.
        7..=12 => {
            out.extend_from_slice(b"\x1b[");
            push_number(out, rng.param_u16());
            out.push(b"@PLMST"[(rng.below(6)) as usize]);
        }
        // SGR — five shapes: reset, indexed, truecolor, underline-color,
        // and a >32-parameter flood.
        13 => match rng.below(5) {
            0 => out.extend_from_slice(b"\x1b[m"),
            1 => {
                out.extend_from_slice(b"\x1b[38;5;");
                push_number(out, rng.param_u16() % 256);
                out.push(b'm');
            }
            2 => {
                out.extend_from_slice(b"\x1b[38;2;");
                for _ in 0..3 {
                    push_number(out, rng.param_u16() % 256);
                    out.push(b';');
                }
                out.pop();
                out.push(b'm');
            }
            3 => {
                out.extend_from_slice(b"\x1b[58;2;");
                for _ in 0..3 {
                    push_number(out, rng.param_u16() % 256);
                    out.push(b';');
                }
                out.pop();
                out.push(b'm');
            }
            // Audit shape: "random long parameter string > 32".
            4 => {
                let count = 33 + rng.below(96) as usize; // 33..=128 params
                out.extend_from_slice(b"\x1b[");
                for i in 0..count {
                    if i > 0 {
                        out.push(b';');
                    }
                    push_number(out, rng.param_u16() % 400);
                }
                out.push(b'm');
            }
            5 => {
                let count = 1 + rng.below(6) as usize;
                out.extend_from_slice(b"\x1b[");
                for i in 0..count {
                    if i > 0 {
                        out.push(b';');
                    }
                    push_number(out, rng.pick(SGR_ATTRS));
                }
                out.push(b'm');
            }
            _ => unreachable!("rng.below(5) < 5"),
        },
        // OSC fragment — half terminate with BEL, half with ST. Codes 0
        // (title, exercised by state) and 52 (clipboard; v1.11.5 the payload
        // is parsed — decodes to a ClipboardWrite UiEvent or, for garbage
        // base64, is ignored with a trace — never a panic).
        14 => {
            out.extend_from_slice(if rng.below(2) == 0 {
                b"\x1b]0;"
            } else {
                b"\x1b]52;c;"
            });
            let payload_len = 1 + rng.below(64) as usize;
            for _ in 0..payload_len {
                out.push(rng.pick(PRINTABLE));
            }
            if rng.below(2) == 0 {
                out.push(0x07);
            } else {
                out.extend_from_slice(b"\x1b\\");
            }
        }
        // DCS fragment (＋q-style payload) — half terminated, half not.
        15 => {
            out.extend_from_slice(b"\x1bP+q");
            let payload_len = 1 + rng.below(32) as usize;
            for _ in 0..payload_len {
                out.push(rng.pick(PRINTABLE));
            }
            if rng.below(2) == 0 {
                out.extend_from_slice(b"\x1b\\");
            }
        }
        // APC fragment (used by kitty/iTerm protocols) — half terminated.
        16 => {
            out.extend_from_slice(b"\x1b_");
            let payload_len = 1 + rng.below(16) as usize;
            for _ in 0..payload_len {
                out.push(rng.pick(PRINTABLE));
            }
            if rng.below(2) == 0 {
                out.extend_from_slice(b"\x1b\\");
            }
        }
        // Character-set shift (G0/G1 designation).
        17 => {
            out.push(0x1b);
            out.push(rng.pick(b"()"));
            out.push(rng.pick(CHARSET_SETS));
        }
        // DEC private mode set/reset.
        18 => {
            out.extend_from_slice(b"\x1b[?");
            push_number(out, rng.pick(DEC_PRIVATE_MODES));
            out.push(rng.pick(b"hl"));
        }
        _ => unreachable!("rng.below(19) < 19"),
    }
}

/// Generate one `target`-byte chunk: weighted atoms until `target` is
/// reached, then truncate. Truncation deliberately splits sequences /
/// UTF-8 mid-code-unit — exactly what a real PTY byte stream does when a
/// chunk boundary falls inside a sequence.
fn gen_chunk(rng: &mut XorShift64, target: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(target + 16);
    while out.len() < target {
        match rng.below(100) as u8 {
            0..=54 => out.push(rng.pick(PRINTABLE)), // printable ASCII ~55%
            55..=64 => {
                // CJK ~10% — always a complete 3-byte UTF-8 sequence.
                let c = rng.pick(CJK_COMMON);
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            65..=74 => out.push(rng.pick(CONTROLS)), // controls ~10%
            _ => emit_esc_atom(rng, &mut out),       // ESC pool ~25%
        }
    }
    out.truncate(target);
    out
}

// ── Wide/spacer invariant ─────────────────────────────────────────────

/// Assert no orphaned wide cells in the visible grid: every `WIDE_SPACER`
/// must follow a `Full` cell, and every `Full` cell must be followed by a
/// `WIDE_SPACER`. Mirrors `replay_fixtures::assert_no_orphaned_wide_cells`
/// (integration-test binaries cannot share items, so this is a local copy).
/// `context` names the failing fuzz step / test round in the message.
fn assert_no_orphaned_wide_cells(terminal: &Terminal, context: &str) {
    let grid = terminal.grid();
    for row in 0..grid.num_rows {
        for col in 0..grid.num_cols {
            let cell = grid.cell(row, col);
            if cell.flags.contains(CellFlags::WIDE_SPACER) {
                assert!(col > 0, "wide spacer at left edge ({context}): {row}:{col}");
                assert_eq!(
                    grid.cell(row, col - 1).width,
                    CellWidth::Full,
                    "orphaned wide spacer ({context}): {row}:{col}"
                );
            }
            if cell.width == CellWidth::Full {
                assert!(
                    col + 1 < grid.num_cols,
                    "wide lead at right edge ({context}): {row}:{col}"
                );
                assert!(
                    grid.cell(row, col + 1)
                        .flags
                        .contains(CellFlags::WIDE_SPACER),
                    "orphaned wide lead ({context}): {row}:{col}"
                );
            }
        }
    }
}

// ── Property tests: 48 seeds x 200 steps ──────────────────────────────

/// One full run for a seed: 200 chunks of 1..=4096 bytes, invariant
/// checks after every chunk (dimensions fixed, wide pairs intact, no
/// panic — a panic is the failure).
fn run_property_seed(seed: u64) {
    const ROWS: usize = 24;
    const COLS: usize = 80;
    const STEPS: usize = 200;

    let mut rng = XorShift64(seed);
    let mut terminal = Terminal::with_scrollback(ROWS, COLS, 1_000);

    for step in 0..STEPS {
        let chunk_len = 1 + rng.below(4096) as usize;
        let chunk = gen_chunk(&mut rng, chunk_len);
        terminal.process(&chunk);

        // Invariant (b): the grid is never resized by the byte stream.
        assert_eq!(
            terminal.grid().num_rows,
            ROWS,
            "row count drifted, seed {seed} step {step}"
        );
        assert_eq!(
            terminal.grid().num_cols,
            COLS,
            "column count drifted, seed {seed} step {step}"
        );
        // Invariant (c): wide/spacer pairing survives every chunk.
        assert_no_orphaned_wide_cells(&terminal, &format!("seed {seed} step {step}"));
    }
}

macro_rules! seed_property_tests {
    ($($index:literal => $name:ident),+ $(,)?) => {
        $(
            /// AUDIT_v1.10.39 fuzz-lite property test for one fixed seed:
            /// 200 random chunks against a 24x80 terminal with scrollback.
            #[test]
            fn $name() {
                run_property_seed(seed_for($index));
            }
        )+
    };
}

seed_property_tests! {
    0  => property_seed_00, 1  => property_seed_01, 2  => property_seed_02,
    3  => property_seed_03, 4  => property_seed_04, 5  => property_seed_05,
    6  => property_seed_06, 7  => property_seed_07, 8  => property_seed_08,
    9  => property_seed_09, 10 => property_seed_10, 11 => property_seed_11,
    12 => property_seed_12, 13 => property_seed_13, 14 => property_seed_14,
    15 => property_seed_15, 16 => property_seed_16, 17 => property_seed_17,
    18 => property_seed_18, 19 => property_seed_19, 20 => property_seed_20,
    21 => property_seed_21, 22 => property_seed_22, 23 => property_seed_23,
    24 => property_seed_24, 25 => property_seed_25, 26 => property_seed_26,
    27 => property_seed_27, 28 => property_seed_28, 29 => property_seed_29,
    30 => property_seed_30, 31 => property_seed_31, 32 => property_seed_32,
    33 => property_seed_33, 34 => property_seed_34, 35 => property_seed_35,
    36 => property_seed_36, 37 => property_seed_37, 38 => property_seed_38,
    39 => property_seed_39, 40 => property_seed_40, 41 => property_seed_41,
    42 => property_seed_42, 43 => property_seed_43, 44 => property_seed_44,
    45 => property_seed_45, 46 => property_seed_46, 47 => property_seed_47,
}

// ── OSC guard differential harness (v1.11.2 X1 / PLAN_v1112 §3.2) ─────

/// Compare the observable text/geometry state of two terminals fed the same
/// byte stream. Colors are deliberately NOT compared: over-cap OSC 4
/// truncation legally changes palette-derived colors in the guarded instance
/// while text, geometry and scroll volume must match bit-for-bit (the
/// whitelist is limited to palette-origin colors — PLAN_v1112 §8).
fn assert_same_observable_state(guarded: &Terminal, unguarded: &Terminal, context: &str) {
    let (g, u) = (guarded.grid(), unguarded.grid());
    assert_eq!(g.num_rows, u.num_rows, "rows diverged ({context})");
    for row in 0..g.num_rows {
        assert_eq!(
            g.row_text(row),
            u.row_text(row),
            "row {row} text diverged ({context})"
        );
    }
    let (gc, uc) = (&g.cursor, &u.cursor);
    assert_eq!(gc.row, uc.row, "cursor row diverged ({context})");
    assert_eq!(gc.col, uc.col, "cursor col diverged ({context})");
    assert_eq!(
        gc.wrap_pending, uc.wrap_pending,
        "wrap_pending diverged ({context})"
    );
    assert_eq!(
        g.scrollback_len(),
        u.scrollback_len(),
        "scrollback length diverged ({context})"
    );
}

/// One differential run for a seed: 200 chunks of 1..=4096 bytes fed to a
/// guard-ON instance (default constructor) and a guard-OFF instance (the
/// pre-guard behavior anchor). After every chunk the two must agree on all
/// row texts, cursor position, scrollback depth; wide pairs must hold in
/// each independently. Any divergence means the FSM lost sync with vte.
fn run_guard_differential_seed(seed: u64) {
    const ROWS: usize = 24;
    const COLS: usize = 80;
    const STEPS: usize = 200;

    let mut rng = XorShift64(seed);
    let mut guarded = Terminal::with_scrollback(ROWS, COLS, 1_000);
    let mut unguarded = Terminal::with_osc_guard(ROWS, COLS, 1_000, false);

    for step in 0..STEPS {
        let chunk_len = 1 + rng.below(4096) as usize;
        let chunk = gen_chunk(&mut rng, chunk_len);
        guarded.process(&chunk);
        unguarded.process(&chunk);

        let ctx = format!("seed {seed} step {step}");
        assert_same_observable_state(&guarded, &unguarded, &ctx);
        assert_no_orphaned_wide_cells(&guarded, &ctx);
        assert_no_orphaned_wide_cells(&unguarded, &ctx);
    }
}

macro_rules! guard_differential_tests {
    ($($index:literal => $name:ident),+ $(,)?) => {
        $(
            /// v1.11.2 X1 differential harness for one fixed seed: the OSC
            /// guard must be observationally invisible except for over-cap
            /// payload truncation (which cannot change text/geometry here).
            #[test]
            fn $name() {
                run_guard_differential_seed(seed_for($index));
            }
        )+
    };
}

guard_differential_tests! {
    0  => guard_diff_seed_00, 1  => guard_diff_seed_01, 2  => guard_diff_seed_02,
    3  => guard_diff_seed_03, 4  => guard_diff_seed_04, 5  => guard_diff_seed_05,
    6  => guard_diff_seed_06, 7  => guard_diff_seed_07,
    8  => guard_diff_seed_08, 9  => guard_diff_seed_09, 10 => guard_diff_seed_10,
    11 => guard_diff_seed_11, 12 => guard_diff_seed_12, 13 => guard_diff_seed_13,
    14 => guard_diff_seed_14, 15 => guard_diff_seed_15,
}

/// Directed differential companion to the seeded harness: the seeded chunks
/// never exceed the 1 MiB OSC cap (OSC atoms are ≤64 payload bytes), so this
/// test drives the Swallowing state through the real process() path and
/// asserts the guard stays observationally invisible on text/geometry even
/// while truncating a 3 MiB payload.
#[test]
fn guard_differential_over_cap_osc_no_text_divergence() {
    const PAYLOAD_SIZE: usize = 3 * 1024 * 1024;
    let mut chunk = Vec::with_capacity(PAYLOAD_SIZE + 32);
    chunk.extend_from_slice(b"\x1b]52;c;");
    chunk.resize(chunk.len() + PAYLOAD_SIZE, b'A');
    chunk.extend_from_slice(b"\x07visible-after");

    let mut guarded = Terminal::with_scrollback(24, 80, 1_000);
    let mut unguarded = Terminal::with_osc_guard(24, 80, 1_000, false);
    guarded.process(&chunk);
    unguarded.process(&chunk);

    assert_same_observable_state(&guarded, &unguarded, "over-cap OSC");
    assert!(
        guarded.grid().row_text(0).contains("visible-after"),
        "guarded parser must resync at the BEL after swallowing"
    );
}

// ── Directed extreme-input tests (AUDIT_v1.10.39) ──────────────────────

/// AUDIT_v1.10.39 (OSC raw accumulation): vte 0.13 with `no_std` disabled
/// accumulates `osc_raw` in an unbounded heap `Vec<u8>` until the BEL/ST
/// terminator arrives (the workspace Cargo.toml documents the 1 KiB
/// ArrayVec → Vec change for OSC 8 URLs; nothing caps the Vec afterwards).
/// `ESC]52;c;` + 4 MiB of payload is the audit's worst-case single OSC:
/// memory grows ~4 MiB, but parsing must survive and resume immediately
/// after the BEL — the trailing "OK" must land on the grid. v1.11.5: OSC 52
/// is now parsed — the guard-capped payload (≤ 1 MiB raw) decodes into a
/// single in-bounds ClipboardWrite UiEvent (asserted below).
#[test]
fn unterminated_osc_four_mib_then_bel_recovers() {
    const PAYLOAD_SIZE: usize = 4 * 1024 * 1024;

    let mut bytes = Vec::with_capacity(PAYLOAD_SIZE + 16);
    bytes.extend_from_slice(b"\x1b]52;c;"); // OSC 52 — vte accumulates the raw payload
    bytes.resize(PAYLOAD_SIZE, b'A');
    bytes.extend_from_slice(b"\x07OK"); // BEL terminates the OSC; ground state resumes with "OK"

    let mut terminal = Terminal::new(24, 80);
    terminal.process(&bytes);

    assert_eq!(
        terminal.grid().row_text(0),
        "OK",
        "parser must recover from a 4 MiB unterminated OSC"
    );

    // v1.11.5: the guard-capped payload parses as one in-bounds
    // ClipboardWrite UiEvent (never panics, never exceeds OSC52_MAX_BYTES).
    let events = terminal.take_ui_events();
    assert_eq!(events.len(), 1, "exactly one event from the 52;c; payload");
    match &events[0] {
        weft_core::vt::UiEvent::ClipboardWrite { data, truncated } => {
            assert!(
                data.len() <= weft_core::vt::OSC52_MAX_BYTES,
                "decoded payload must stay within the business cap"
            );
            assert!(!truncated, "guard-capped raw ≤ 1MiB decodes below the cap");
        }
        other => panic!("expected ClipboardWrite, got {other:?}"),
    }

    // v1.11.2 X1 (PLAN_v1112 §3.2): with the guard ON (default), the OSC is
    // truncated at 1 MiB so vte's raw buffer stops growing — the semantic
    // anchor for "memory cannot explode". The parser must still be fully
    // usable after the BEL resync.
    terminal.process(b"after-truncation");
    let text = terminal.grid().row_text(0);
    assert!(
        text.contains("after-truncation"),
        "parser must keep printing normally after a truncated OSC: {text}"
    );
    // And the truncated OSC 52 payload must not leak into downstream state:
    // the hyperlink registry stays empty and no cell is tagged HYPERLINK.
    assert!(
        !terminal.grid().viewport.iter().any(|row| row
            .cells
            .iter()
            .any(|c| c.flags.contains(CellFlags::HYPERLINK))),
        "truncated OSC payload must not tag cells as hyperlinks"
    );
}

/// AUDIT_v1.10.39 (OSC title churn): 2000 back-to-back OSC 0 title sets of
/// 4 KiB each. Every dispatch allocates a fresh `String` and drops the old
/// title, so the flood hammers allocation without freeing between sets;
/// it must neither panic nor wedge the terminal.
#[test]
fn osc_title_flood_no_panic() {
    let mut set_title = Vec::with_capacity(4096 + 3);
    set_title.extend_from_slice(b"\x1b]0;");
    set_title.resize(4096, b'x');
    set_title.push(0x07);

    let mut terminal = Terminal::new(24, 80);
    for _ in 0..2000 {
        terminal.process(&set_title);
    }

    terminal.process(b"post-flood");
    assert!(
        terminal.grid().row_text(0).starts_with("post-flood"),
        "terminal must still print after 2000 title sets"
    );
}

/// AUDIT_v1.10.39 (SGR parameter flood): a single SGR with 200 parameters
/// (alternating bold/italic) beyond any fixed parser buffer must parse
/// completely — the following text still prints, and the LAST attribute
/// wins without any param leaking into a separate code.
#[test]
fn sgr_param_flood_beyond_stack_buffer() {
    let mut sgr = b"\x1b[".to_vec();
    for i in 0..200 {
        if i > 0 {
            sgr.push(b';');
        }
        sgr.extend_from_slice(if i % 2 == 0 { b"1" } else { b"3" }); // bold / italic
    }
    sgr.push(b'm');
    sgr.extend_from_slice(b"TEXT");

    let mut terminal = Terminal::new(24, 80);
    terminal.process(&sgr);

    let text = terminal.grid().row_text(0);
    assert!(text.contains("TEXT"), "SGR flood ate the text: {text}");
    let cell = terminal.grid().cell(0, 0);
    assert!(
        cell.flags.contains(CellFlags::BOLD) && cell.flags.contains(CellFlags::ITALIC),
        "attributes must survive a 200-param SGR (flags {:?})",
        cell.flags
    );
}

/// AUDIT_v1.10.39 (extreme CSI counts): SU/SD/IL/DL/DCH/ICH at the
/// maximum expressible count (65535) must clamp, not corrupt: grid size
/// unchanged, cursor in bounds, printing still works afterwards. Every
/// grid op clamps via `.min()`, so this pins that clamping on real input.
#[test]
fn su_sd_il_dl_huge_counts_grid_intact() {
    const ROWS: usize = 8;
    const COLS: usize = 40;

    let mut terminal = Terminal::new(ROWS, COLS);
    for seq in [
        b"\x1b[65535S".as_slice(), // SU — scroll up
        b"\x1b[65535T",            // SD — scroll down
        b"\x1b[65535L",            // IL — insert lines
        b"\x1b[65535M",            // DL — delete lines
        b"\x1b[65535P",            // DCH — delete chars
        b"\x1b[65535@",            // ICH — insert chars
    ] {
        terminal.process(seq);
    }

    assert_eq!(
        terminal.grid().num_rows,
        ROWS,
        "rows changed under huge counts"
    );
    assert_eq!(
        terminal.grid().num_cols,
        COLS,
        "cols changed under huge counts"
    );
    let cursor = terminal.grid().cursor.clone();
    assert!(
        cursor.row < ROWS && cursor.col < COLS,
        "cursor out of bounds after max-count storms: {cursor:?}"
    );

    terminal.process(b"ALIVE");
    let row = terminal.grid().cursor.row;
    assert!(
        terminal.grid().row_text(row).contains("ALIVE"),
        "terminal must still print after max-count storms"
    );
}

/// AUDIT_v1.10.39 (grapheme cluster growth): `RowExtras::append_scalar`
/// has no cluster-length cap and rebuilds the cluster string on every
/// appended mark — for `n` marks that is O(n²) copying on one cell.
/// 50_000 marks (the audit case) must complete without panic, keep the
/// base char on the cell, and flag the cluster with `EXTRA`.
#[test]
fn combining_mark_cluster_flood() {
    // 'a' followed by 50_000 U+0301 combining acute accents (2 UTF-8 bytes each).
    let mut bytes = Vec::with_capacity(2 + 100_000);
    bytes.push(b'a');
    for _ in 0..50_000 {
        bytes.extend_from_slice("\u{0301}".as_bytes());
    }

    let mut terminal = Terminal::new(24, 80);
    terminal.process(&bytes);

    let cell = terminal.grid().cell(0, 0);
    assert_eq!(
        cell.character, 'a',
        "base char must survive the cluster flood"
    );
    assert!(
        cell.flags.contains(CellFlags::EXTRA),
        "multi-scalar cluster must be tagged with EXTRA"
    );
}

/// AUDIT_v1.10.39 (wide-char splat regression): overwriting full-width
/// pairs at line edges is the historical CJK splat (2026-07 vim scroll
/// bug — orphaned wide cells left in the grid). Storm three shapes per
/// row: wide char at the very last column (forces the wrap-first path),
/// wide char straddling the last pair, and half-width splats that clobber
/// the spacer half. No orphaned wide cell may survive a round.
#[test]
fn wide_char_splat_at_line_end_storm() {
    const ROWS: usize = 12;
    const COLS: usize = 40;

    let mut terminal = Terminal::new(ROWS, COLS);
    for round in 0..25 {
        for row in 0..ROWS {
            // Wide char at the last column — must wrap to the next line
            // rather than leaving a Full cell with nowhere for its spacer.
            terminal.process(format!("\x1b[{};{}H中", row + 1, COLS).as_bytes());
            // Straddle at cols-2/cols-1: lead + spacer at the right edge.
            terminal.process(format!("\x1b[{};{}H中", row + 1, COLS - 1).as_bytes());
            // Splat half-width chars over both halves of the pair.
            terminal.process(
                format!("\x1b[{};{}Hx\x1b[{};{}Hy", row + 1, COLS - 1, row + 1, COLS).as_bytes(),
            );
        }
        // The exact invariant the splat bug broke: no orphaned pair cells.
        assert_no_orphaned_wide_cells(&terminal, &format!("round {round}"));
    }

    terminal.process(b"END");
    assert!(
        (0..ROWS).any(|row| terminal.grid().row_text(row).contains("END")),
        "terminal must still print after the wide-char storm"
    );
}

/// AUDIT_v1.10.39 (resize extremes): with mixed ASCII+CJK content
/// resident, collapse the terminal to 1x1 and restore the full size
/// repeatedly. Every step must keep dimensions exact, wide pairs intact
/// (skipped at 1 column: a Full cell at col 0 with no room for a spacer
/// is the legitimate, documented clamp when the window is narrower than
/// the pair), the cursor in bounds, and printing must work after the last
/// restore.
#[test]
fn resize_to_minimum_and_back_with_content() {
    const ROWS: usize = 24;
    const COLS: usize = 80;

    let mut terminal = Terminal::new(ROWS, COLS);
    for i in 0..12 {
        terminal.process(format!("line {i}: The quick 棕色 fox jumps over 懒 dog\n").as_bytes());
    }

    // Collapse/restore cycle — the audit's minimum (1x1) plus one odd
    // intermediate size, exercised repeatedly.
    let rounds: [(usize, usize); 6] = [
        (1, 1),
        (ROWS, COLS),
        (3, 7),
        (ROWS, COLS),
        (1, 1),
        (ROWS, COLS),
    ];
    for (round, (rows, cols)) in rounds.iter().enumerate() {
        terminal.resize(*rows, *cols);

        assert_eq!(
            terminal.grid().num_rows,
            *rows,
            "round {round}: row count after resize to {rows}x{cols}"
        );
        assert_eq!(
            terminal.grid().num_cols,
            *cols,
            "round {round}: column count after resize to {rows}x{cols}"
        );
        let cursor = terminal.grid().cursor.clone();
        assert!(
            cursor.row < *rows && cursor.col < *cols,
            "round {round}: cursor out of bounds after resize to {rows}x{cols}: {cursor:?}"
        );
        if *cols >= 2 {
            // At 1 column a wide pair cannot fit (see doc comment), but at
            // any wider size every pair must be whole.
            assert_no_orphaned_wide_cells(&terminal, &format!("round {round} {rows}x{cols}"));
        }

        // Keep printing during narrow phases: wrapped chars legitimately
        // scroll into history, but processing must never stall or panic.
        terminal.process(b"R");
    }

    // After the last restore the terminal must print normally again.
    terminal.process(b"FINAL");
    assert!(
        (0..ROWS).any(|row| terminal.grid().row_text(row).contains("FINAL")),
        "terminal must still print after the resize storm"
    );
}
