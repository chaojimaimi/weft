//! Reader-thread mouse-disable pre-scan (v1.11.15 FIX A,
//! PLAN_v11115_EXIT_RACE_MOUSE_LEAK §1).
//!
//! When a TUI (e.g. opencode, DECSET 1003 AnyEvent) exits, it emits its
//! mouse-disable sequences (`CSI ?1003l …`) to the PTY. Those bytes used to
//! sit in the reader→Wake→pump pipeline for tens to hundreds of milliseconds
//! before the main thread parsed them — meanwhile weft kept writing hover
//! events into a PTY whose shell had already restored cooked mode + echo, so
//! the kernel echoed the reports back in caret form (`^[[<48;56;29M`) and the
//! shell's line editor swallowed them as input. The fix closes the window at
//! the source: the reader thread itself scans every chunk it reads and flips
//! a shared flag the moment a mouse DECRST is seen; the app's mouse senders
//! check that flag and stop writing.
//!
//! Undo is authoritative and single-source: the main-thread vte parser clears
//! the flag whenever it parses a DEC private mode in the mouse family — set
//! (h) AND reset (l) alike (see `Terminal::handle_dec_private_mode`). Because
//! the scanner's grammar is a strict subset of what vte dispatches as DEC
//! private mode, a scanner hit is always followed by a parser clear within
//! one parse latency; the flag cannot stick.
//!
//! Known accepted miss (under-suppress = safe): colon sub-parameter forms
//! such as `CSI ?1003:7l` — vte does not dispatch multi-element sub-params
//! through the single-param `if let &[mode] = sub` pattern either, so the
//! scanner (which collects only digits and `;`) staying quiet keeps the two
//! machines consistent. Real applications do not emit this form.

use std::sync::atomic::{AtomicBool, Ordering};

/// Per-pane suppression flag shared between the PTY reader thread (writer)
/// and the UI thread (vte parser clears, mouse senders read).
pub type MouseSuppressFlag = std::sync::Arc<AtomicBool>;

/// Create a fresh, cleared suppression flag (one per Pane).
pub fn new_flag() -> MouseSuppressFlag {
    std::sync::Arc::new(AtomicBool::new(false))
}

/// Set the flag (reader thread: mouse-disable bytes observed, or the PTY hit
/// EOF/EIO/EBADF — the session is going away either way). Release pairs with
/// the UI thread's Acquire loads.
pub fn set_suppressed(flag: &MouseSuppressFlag) {
    flag.store(true, Ordering::Release);
}

/// Read the flag (UI thread: mouse senders gate on this).
pub fn is_suppressed(flag: &MouseSuppressFlag) -> bool {
    flag.load(Ordering::Acquire)
}

/// Mouse DEC private modes whose set/reset must clear the suppression flag
/// (shared with the vte parser's pre-clear arm).
pub(crate) const MOUSE_SUPPRESS_CLEAR_MODES: [u16; 5] = [9, 1000, 1002, 1003, 1006];

/// Incremental scanner for mouse-disable sequences: a persistent FSM whose
/// state survives across `feed` calls, so a sequence split across reader
/// chunks is recognized without any carry array — the partial bytes simply
/// stay inside the state machine.
///
/// Grammar (deliberately conservative — see the module doc for why a subset
/// of vte's DEC-private dispatch is the consistency anchor):
/// `ESC [ ? <digits (; digits)*> l` with any parsed param in
/// {9, 1000, 1002, 1003, 1006} → hit. `h` (enable) never hits; non-mouse
/// resets (`?1049l` …) never hit; sequences carrying intermediates
/// (0x20–0x2F) or non-`?` CSI parameter bytes before the `?` never hit.
pub struct MouseDisableScanner {
    state: FsmState,
    /// Collected raw parameter characters (`0-9`, `;`) of the current
    /// private sequence, capped at [`MAX_PRIV_PARAMS`] bytes.
    params: ([u8; MAX_PRIV_PARAMS], usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FsmState {
    /// Outside any escape sequence.
    Ground,
    /// Saw ESC; waiting to learn the sequence family.
    Esc,
    /// Saw `ESC [` — still eligible to become a private (`?`) sequence.
    Csi,
    /// Saw `ESC [` followed by non-`?` parameter bytes (digits, `;`,
    /// `<`, `=`, `>`): the sequence can no longer dispatch as `CSI ? …` in
    /// vte (its intermediates would not be exactly `[b'?']`), so it can
    /// never hit. Kept distinct from `Csi` so a later `?` cannot re-arm it.
    CsiParam,
    /// Saw `ESC [ ?` — collecting digits/`;` until the final byte.
    PrivParams,
}

/// Parameter-buffer cap. A private sequence longer than this is abandoned
/// (reset to Ground) so a hostile stream cannot grow the buffer.
const MAX_PRIV_PARAMS: usize = 32;

impl MouseDisableScanner {
    pub fn new() -> Self {
        Self {
            state: FsmState::Ground,
            params: ([0u8; MAX_PRIV_PARAMS], 0),
        }
    }

    /// Feed one read chunk; returns true if a mouse-disable sequence
    /// completed inside it (the whole chunk is always consumed so the FSM
    /// stays in sync with the byte stream).
    pub fn feed(&mut self, chunk: &[u8]) -> bool {
        let mut hit = false;
        for &byte in chunk {
            hit |= self.step(byte);
        }
        hit
    }

    fn push_param_char(&mut self, byte: u8) {
        let (buf, len) = &mut self.params;
        if *len >= MAX_PRIV_PARAMS {
            // Over-long parameter list without a final byte: abandon the
            // sequence (anti-bloat) — the remaining bytes degrade to Ground.
            self.state = FsmState::Ground;
            return;
        }
        buf[*len] = byte;
        *len += 1;
    }

    fn params_reset(&mut self) {
        self.params.1 = 0;
    }

    /// True when the collected parameters name any mouse mode.
    fn params_name_mouse_mode(&self) -> bool {
        let (buf, len) = &self.params;
        let text = std::str::from_utf8(&buf[..*len]).unwrap_or("");
        text.split(';').any(|p| {
            p.parse::<u16>()
                .is_ok_and(|mode| MOUSE_SUPPRESS_CLEAR_MODES.contains(&mode))
        })
    }

    /// One byte through the FSM; returns true when this byte completes a
    /// mouse-disable sequence.
    fn step(&mut self, byte: u8) -> bool {
        match self.state {
            FsmState::Ground => {
                if byte == 0x1B {
                    self.state = FsmState::Esc;
                }
                false
            }
            FsmState::Esc => {
                if byte == b'[' {
                    self.params_reset();
                    self.state = FsmState::Csi;
                } else if byte != 0x1B {
                    // Any other byte cancels the escape (vte esc_dispatch
                    // semantics); ESC chains into a fresh escape.
                    self.state = FsmState::Ground;
                }
                false
            }
            FsmState::Csi => {
                match byte {
                    0x1B => self.state = FsmState::Esc,
                    // vte: C0 executes and the sequence continues.
                    0x00..=0x1F => {}
                    b'?' => {
                        self.params_reset();
                        self.state = FsmState::PrivParams;
                    }
                    // Other parameter bytes: sequence goes non-private.
                    0x30..=0x3F => self.state = FsmState::CsiParam,
                    // Intermediates (0x20-0x2F): conservative reset — the
                    // sequence can no longer be a bare `CSI ? … l`.
                    0x20..=0x2F => self.state = FsmState::Ground,
                    // Final byte of a non-private sequence.
                    0x40..=0x7E => self.state = FsmState::Ground,
                    _ => self.state = FsmState::Ground,
                }
                false
            }
            FsmState::CsiParam => {
                match byte {
                    0x1B => self.state = FsmState::Esc,
                    // C0 executes and the sequence continues (vte parity).
                    0x00..=0x1F => {}
                    // Parameters (including a `?` here — it cannot re-arm a
                    // private sequence: vte would report intermediates
                    // ≠ [?]) and intermediates keep the sequence non-private.
                    0x20..=0x3F => {}
                    // Final byte of a non-private sequence: never a hit.
                    0x40..=0x7E => self.state = FsmState::Ground,
                    _ => self.state = FsmState::Ground,
                }
                false
            }
            FsmState::PrivParams => {
                match byte {
                    0x1B => {
                        // Mid-sequence ESC re-enters escape (vte parity) —
                        // the abandoned params are cleared on the next `[`.
                        self.state = FsmState::Esc;
                        false
                    }
                    // C0 executes and the sequence continues (vte parity).
                    0x00..=0x1F => false,
                    b'0'..=b'9' | b';' => {
                        self.push_param_char(byte);
                        false
                    }
                    // Intermediates and any non-grammar parameter byte
                    // (`:` sub-params, `<`,`=`,`>`) invalidate the strict
                    // `ESC [ ? <digits/;> l` shape → conservative miss.
                    0x20..=0x3F => {
                        self.state = FsmState::Ground;
                        false
                    }
                    // Final byte: judge.
                    0x40..=0x7E => {
                        let hit = byte == b'l' && self.params_name_mouse_mode();
                        self.params_reset();
                        self.state = FsmState::Ground;
                        hit
                    }
                    _ => {
                        self.state = FsmState::Ground;
                        false
                    }
                }
            }
        }
    }
}

impl Default for MouseDisableScanner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(bytes: &[u8]) -> bool {
        MouseDisableScanner::new().feed(bytes)
    }

    // ── §1.4 test fixtures ①–⑪ ────────────────────────────────────────

    /// ① single `?1006l`.
    #[test]
    fn single_sgr_mouse_disable_hits() {
        assert!(hit(b"\x1b[?1006l"));
    }

    /// ② merged disable `?1000;1002;1003;1006l`.
    #[test]
    fn merged_multi_mode_disable_hits() {
        assert!(hit(b"\x1b[?1000;1002;1003;1006l"));
    }

    /// ③ consecutive `?1003l` + `?1002l`.
    #[test]
    fn consecutive_disables_hit() {
        assert!(hit(b"\x1b[?1003l\x1b[?1002l"));
    }

    /// ④ cross-feed split: the break lands mid-sequence AND exactly on a
    /// state boundary; both must hit across two feeds.
    #[test]
    fn sequence_split_across_feeds_still_hits() {
        // Break inside the parameter digits: `ESC [ ? 100` | `3 l`.
        let mut s = MouseDisableScanner::new();
        assert!(!s.feed(b"\x1b[?100"));
        assert!(s.feed(b"3l"));
        // Break exactly on the Esc→Csi state boundary: `ESC` | `[?1003l`.
        let mut s = MouseDisableScanner::new();
        assert!(!s.feed(b"\x1b"));
        assert!(s.feed(b"[?1003l"));
        // Break exactly on the Esc byte itself.
        let mut s = MouseDisableScanner::new();
        assert!(!s.feed(b"\x1b[?1003"));
        assert!(s.feed(b"l"));
    }

    /// ⑤ `?1003h` (enable) never hits.
    #[test]
    fn mouse_enable_never_hits() {
        assert!(!hit(b"\x1b[?1003h"));
    }

    /// ⑥ `?1049l` (alt screen, not a mouse mode) never hits.
    #[test]
    fn non_mouse_reset_never_hits() {
        assert!(!hit(b"\x1b[?1049l"));
    }

    /// ⑦ ordinary cursor addressing `CSI 2;3H` never hits.
    #[test]
    fn cursor_addressing_never_hits() {
        assert!(!hit(b"\x1b[2;3H"));
    }

    /// ⑧ sequences carrying intermediates (0x20–0x2F) never hit — and the
    /// scanner recovers to Ground afterwards.
    #[test]
    fn intermediate_byte_sequence_never_hits() {
        // DECSCUSR `CSI 0 SP q`.
        assert!(!hit(b"\x1b[0 q"));
        // A disable AFTER an intermediate sequence is still recognized.
        let mut s = MouseDisableScanner::new();
        assert!(!s.feed(b"\x1b[0 q"));
        assert!(s.feed(b"\x1b[?1003l"));
    }

    /// ⑨ an over-long parameter list is abandoned and the FSM resets.
    #[test]
    fn overlong_params_reset_the_scanner() {
        let mut s = MouseDisableScanner::new();
        let mut bytes = b"\x1b[?1003".to_vec();
        bytes.extend(std::iter::repeat(b'1').take(MAX_PRIV_PARAMS + 8));
        bytes.push(b'l');
        assert!(
            !s.feed(&bytes),
            "over-long private sequence must be abandoned"
        );
        // The FSM recovered: a well-formed disable right after still hits.
        assert!(s.feed(b"\x1b[?1003l"));
    }

    /// ⑩ an ESC nested inside a half-collected sequence re-enters Esc — the
    /// abandoned `?100` params must not merge into the next sequence.
    #[test]
    fn embedded_esc_reentry_does_not_misfire() {
        assert!(!hit(b"\x1b[?100\x1b[3l"));
        // The trailing `ESC [ 3 l` itself must stay a miss (non-private).
    }

    /// A private sequence whose params name no mouse mode never hits
    /// (`?2004l` bracketed paste, `?2031l` …).
    #[test]
    fn other_private_resets_never_hit() {
        assert!(!hit(b"\x1b[?2004l"));
        assert!(!hit(b"\x1b[?2031l"));
        assert!(!hit(b"\x1b[?25h"));
    }

    /// SGR mouse REPORT bytes (what the leaked echo carried) never hit.
    #[test]
    fn sgr_mouse_reports_never_hit() {
        assert!(!hit(b"\x1b[<48;56;29M"));
    }

    /// ⑪ End-to-end: the real opencode 1.18.27 teardown tail, vendored as a
    /// byte literal (repo convention for replay fixtures). Source:
    /// 2026-09-08 capture `/tmp/weft_forensics/run_b.bin` bytes [25727..],
    /// i.e. from the first mouse disable to EOF. The scanner must hit —
    /// that is exactly the leak window this fix closes — and it must hit
    /// on the REAL shapes (per-sequence disables, a non-`?` private SGR,
    /// OSC strings, DECSCUSR intermediate, DECTCEM enable) without any
    /// false positive from the trailing caret-echo residue.
    #[test]
    fn opencode_teardown_tail_from_run_b_hits_end_to_end() {
        // The caret-echo residue alone (literal `^[[<48;56;29M` ×4 after a
        // DECTCEM enable — the bytes the shell actually swallowed) must
        // NOT false-hit: the hit comes only from the disable block.
        const ECHO_RESIDUE: &[u8] =
            b"\x1b[?25h^[[<48;56;29M\x1b[?25h^[[<48;56;29M^[[<48;56;29M^[[<48;56;29M";
        assert!(!hit(ECHO_RESIDUE));

        const TEARDOWN_TAIL: &[u8] = b"\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?1006l\
\x1b[?2004l\x1b[?1049l\x1b[?2031l\x1b]0;\x07\x1b]12;default\x07\x1b]112\x07\
\x1b[0 q\x1b[?25h^[[<48;56;29M\x1b[?25h^[[<48;56;29M^[[<48;56;29M^[[<48;56;29M";
        assert!(hit(TEARDOWN_TAIL));

        // The same tail fed in 64-byte reader chunks still hits.
        let mut s = MouseDisableScanner::new();
        let chunked_hit = TEARDOWN_TAIL
            .chunks(64)
            .fold(false, |acc, chunk| acc | s.feed(chunk));
        assert!(chunked_hit, "chunked feeding must preserve the hit");
    }

    // ── Flag helpers ──────────────────────────────────────────────────

    #[test]
    fn flag_round_trip() {
        let flag = new_flag();
        assert!(!is_suppressed(&flag));
        set_suppressed(&flag);
        assert!(is_suppressed(&flag));
    }
}
