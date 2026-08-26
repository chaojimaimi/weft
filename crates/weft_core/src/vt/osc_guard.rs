//! v1.11.2 X1 (PLAN_v1112 §3): OSC accumulation guard — a shadow observer
//! FSM that mirrors vte's OscString state and stops forwarding payload bytes
//! once an unterminated OSC exceeds [`OSC_GUARD_PAYLOAD_CAP`].
//!
//! # Why this exists
//!
//! vte 0.13 accumulates `osc_raw` in an unbounded heap Vec until BEL/ST
//! arrives. A hostile stream (`ESC ] 52 ; c ; <4 MiB>` with no terminator)
//! grows memory without bound. The guard feeds vte at most CAP payload bytes
//! per OSC; past that it withholds bytes from vte (so vte's buffer stops
//! growing) while watching for the terminator byte itself, which is ALWAYS
//! forwarded so both state machines resynchronize.
//!
//! # Invariants (from the v1.11.2 investigation §10 — do not "optimize")
//!
//! Derived line-by-line from vte 0.13.1 `table.rs`:
//! - Inside OscString, every C0 other than 0x07/0x18/0x1A/0x1B is
//!   `Ignore` — payload context, NOT a terminator.
//! - 0x9C (C1 ST) inside OscString falls in the `0x20..=0xff => OscPut`
//!   range — payload. Only DCS/APC states treat 0x9C as terminators.
//! - ESC (0x1B) hits the `Anywhere => Escape` rule: the OSC ends AT the ESC
//!   byte (vte dispatches immediately, without waiting for `'\'`). The next
//!   byte is processed from Escape — where `']'` re-enters OscString — which
//!   our Ground + prev_esc rule reproduces exactly.
//! - UTF-8 continuation bytes are `0x80..=0xBF`: they can never embed a
//!   `< 0x80` terminator value, so blind per-byte counting is safe.
//!
//! Over-CAP truncation is a visible but bounded behavior change: dispatch
//! sees a truncated payload. Downstream consumers already cap (title in
//! perform.rs, OSC 8 URLs in hyperlink.rs); OSC 133 markers are tiny.

/// Maximum OSC payload bytes forwarded to vte per OSC sequence before the
/// guard switches to Swallowing (1 MiB).
pub const OSC_GUARD_PAYLOAD_CAP: u64 = 1_048_576;

/// Observer state for the OSC guard. Persists across `Terminal::process()`
/// batches — OSC sequences can span chunk boundaries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OscWatch {
    /// Not inside an OSC sequence (or the parser has never seen one).
    #[default]
    Ground,
    /// Inside an OSC string that has accumulated `count` payload bytes so far.
    InOsc { count: u64 },
    /// Payload exceeded the cap: everything except the four terminator bytes
    /// is withheld from vte until resynchronization.
    Swallowing,
}

/// One byte through the observer FSM.
///
/// Updates `state` / `prev_esc` in place and returns whether the byte must be
/// forwarded to the vte parser (`false` = drop on the floor). `prev_esc` is
/// the "last byte in Ground was ESC" bit shared with the caller.
///
/// This is a pure function of `(state, prev_esc, b)` — the §3.1 transition
/// table below is implemented verbatim; any change must go through the plan.
pub fn observe(state: &mut OscWatch, prev_esc: &mut bool, b: u8) -> bool {
    match *state {
        OscWatch::Ground => {
            if b == 0x1B {
                // §3.1 row 1: remember the ESC so a following ']' opens OSC.
                *prev_esc = true;
            } else if b == b']' && *prev_esc {
                // §3.1 row 2: OSC introducer completed (ESC ]).
                *state = OscWatch::InOsc { count: 0 };
                *prev_esc = false;
            } else {
                // §3.1 row 3: any other byte; only ESC sets the bit.
                *prev_esc = b == 0x1B;
            }
            true
        }
        OscWatch::InOsc { mut count } => match b {
            // §3.1 rows 4-6: real terminators are always forwarded.
            0x07 | 0x18 | 0x1A => {
                // BEL / CAN / SUB — vte leaves OscString this same byte.
                *state = OscWatch::Ground;
                true
            }
            0x1B => {
                // ESC terminates the OSC immediately (Anywhere => Escape);
                // prev_esc=true reproduces vte's Escape-state behavior where
                // a following ']' re-enters OscString.
                *state = OscWatch::Ground;
                *prev_esc = true;
                true
            }
            _ => {
                // §3.1 row 7: everything else is payload (other C0, DEL,
                // 0x80..=0xff including 0x9C). Count per byte; over-cap
                // bytes flip to Swallowing and are NOT forwarded.
                count += 1;
                if count > OSC_GUARD_PAYLOAD_CAP {
                    *state = OscWatch::Swallowing;
                    false
                } else {
                    *state = OscWatch::InOsc { count };
                    true
                }
            }
        },
        OscWatch::Swallowing => match b {
            // §3.1 row 8: forwarding the terminator is the ONLY way to
            // resynchronize with vte.
            0x07 | 0x18 | 0x1A | 0x1B => {
                *state = OscWatch::Ground;
                *prev_esc = b == 0x1B;
                true
            }
            // §3.1 row 9: dropped; we stay swallowing regardless of volume.
            _ => false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive one byte through a fresh-ish FSM carried in `fsm`.
    fn step(state: &mut OscWatch, prev_esc: &mut bool, b: u8) -> bool {
        observe(state, prev_esc, b)
    }

    // §3.1 row 1: Ground + ESC → forward, prev_esc set.
    #[test]
    fn ground_esc_sets_prev_esc_and_forwards() {
        let mut s = OscWatch::Ground;
        let mut pe = false;
        assert!(step(&mut s, &mut pe, 0x1B));
        assert_eq!(s, OscWatch::Ground);
        assert!(pe);
    }

    // §3.1 row 2: Ground + ']' with prev_esc → InOsc{0}, forward.
    #[test]
    fn ground_bracket_after_esc_enters_osc() {
        let mut s = OscWatch::Ground;
        let mut pe = false;
        step(&mut s, &mut pe, 0x1B);
        assert!(step(&mut s, &mut pe, b']'));
        assert_eq!(s, OscWatch::InOsc { count: 0 });
        assert!(!pe);
    }

    // §3.1 row 3a: Ground + ordinary byte → forward, clears prev_esc.
    #[test]
    fn ground_plain_byte_clears_prev_esc() {
        let mut s = OscWatch::Ground;
        let mut pe = true;
        assert!(step(&mut s, &mut pe, b'a'));
        assert_eq!(s, OscWatch::Ground);
        assert!(!pe, "non-ESC byte must clear prev_esc");
    }

    // §3.1 row 3b: Ground + ']' WITHOUT prev_esc stays Ground (e.g. a bare
    // bracket printed at the prompt is not an OSC introducer).
    #[test]
    fn ground_bracket_without_prev_esc_stays_ground() {
        let mut s = OscWatch::Ground;
        let mut pe = false;
        assert!(step(&mut s, &mut pe, b']'));
        assert_eq!(s, OscWatch::Ground);
    }

    // §3.1 row 4: InOsc + BEL → forward, back to Ground.
    #[test]
    fn in_osc_bel_terminates_and_forwards() {
        let mut s = OscWatch::InOsc { count: 12 };
        let mut pe = false;
        assert!(step(&mut s, &mut pe, 0x07));
        assert_eq!(s, OscWatch::Ground);
    }

    // §3.1 row 5: InOsc + CAN/SUB → forward, back to Ground.
    #[test]
    fn in_osc_can_and_sub_terminate_and_forward() {
        for b in [0x18u8, 0x1A] {
            let mut s = OscWatch::InOsc { count: 3 };
            let mut pe = false;
            assert!(step(&mut s, &mut pe, b));
            assert_eq!(s, OscWatch::Ground, "byte {b:#04x} must terminate");
        }
    }

    // §3.1 row 6: InOsc + ESC → forward AND immediate return to Ground with
    // prev_esc set. A following ']' re-enters OSC ("两侧一致" with vte's
    // Escape → OscString transition); a following non-'\' does not.
    #[test]
    fn in_osc_esc_ends_osc_immediately_then_bracket_reenters() {
        let mut s = OscWatch::InOsc { count: 7 };
        let mut pe = false;
        assert!(step(&mut s, &mut pe, 0x1B));
        assert_eq!(s, OscWatch::Ground, "OSC ends at the ESC byte");
        assert!(pe);
        // Next byte '\' (ST tail): forwarded, stays Ground (vte EscDispatch).
        assert!(step(&mut s, &mut pe, b'\\'));
        assert_eq!(s, OscWatch::Ground);
        assert!(!pe);
        // Alternative: ESC then ']' re-opens OSC exactly like vte.
        let mut s = OscWatch::InOsc { count: 7 };
        let mut pe = false;
        step(&mut s, &mut pe, 0x1B);
        assert!(step(&mut s, &mut pe, b']'));
        assert_eq!(s, OscWatch::InOsc { count: 0 });
    }

    #[test]
    fn in_osc_esc_followed_by_letter_stays_ground() {
        // ESC ] 0 ; t ESC x — after the bare ESC, 'x' is an EscDispatch final
        // byte for vte and a plain Ground byte for us. Both stay Ground.
        let mut s = OscWatch::Ground;
        let mut pe = false;
        step(&mut s, &mut pe, 0x1B);
        step(&mut s, &mut pe, b']');
        step(&mut s, &mut pe, 0x1B);
        assert!(step(&mut s, &mut pe, b'x'));
        assert_eq!(s, OscWatch::Ground);
    }

    // §3.1 row 7: InOsc payload — C0 like \n \r \t stay INSIDE the OSC and
    // are forwarded; 0x9C and high bytes are payload too.
    #[test]
    fn in_osc_c0_payload_does_not_terminate() {
        for b in [b'\n', b'\r', b'\t', 0x00, 0x06, 0x08, 0x17, 0x19, 0x1C] {
            let mut s = OscWatch::InOsc { count: 1 };
            let mut pe = false;
            assert!(step(&mut s, &mut pe, b), "payload {b:#04x} must forward");
            assert_eq!(s, OscWatch::InOsc { count: 2 }, "byte {b:#04x} is payload");
        }
    }

    #[test]
    fn in_osc_0x9c_and_high_bytes_are_payload_not_terminators() {
        for b in [0x9Cu8, 0x80, 0xFF, 0x7F] {
            let mut s = OscWatch::InOsc { count: 0 };
            let mut pe = false;
            assert!(step(&mut s, &mut pe, b));
            assert_eq!(s, OscWatch::InOsc { count: 1 }, "{b:#04x} must be payload");
        }
    }

    // §3.1 row 7 boundary: exactly CAP payload bytes still forward; byte
    // CAP+1 flips to Swallowing and is dropped.
    #[test]
    fn in_osc_cap_boundary_exact_count_forwards_next_drops() {
        let mut s = OscWatch::InOsc {
            count: OSC_GUARD_PAYLOAD_CAP - 1,
        };
        let mut pe = false;
        assert!(step(&mut s, &mut pe, b'A'));
        assert_eq!(
            s,
            OscWatch::InOsc {
                count: OSC_GUARD_PAYLOAD_CAP
            },
            "byte filling the cap exactly is still forwarded"
        );
        // One more would exceed: dropped + Swallowing.
        assert!(!step(&mut s, &mut pe, b'B'));
        assert_eq!(s, OscWatch::Swallowing);
    }

    // §3.1 row 8: Swallowing forwards ONLY the four terminator bytes.
    #[test]
    fn swallowing_terminators_resynchronize() {
        for b in [0x07u8, 0x18, 0x1A, 0x1B] {
            let mut s = OscWatch::Swallowing;
            let mut pe = false;
            assert!(step(&mut s, &mut pe, b), "terminator {b:#04x} must forward");
            assert_eq!(s, OscWatch::Ground, "terminator {b:#04x} resyncs");
        }
    }

    // §3.1 row 9: Swallowing drops everything else and stays put.
    #[test]
    fn swallowing_drops_other_bytes_including_c0_and_0x9c() {
        for b in [b'a', b'\n', b'\r', 0x9Cu8, 0x80] {
            let mut s = OscWatch::Swallowing;
            let mut pe = false;
            assert!(!step(&mut s, &mut pe, b), "{b:#04x} must be swallowed");
            assert_eq!(s, OscWatch::Swallowing);
        }
    }

    #[test]
    fn swallowing_esc_reenters_osc_on_bracket() {
        // Swallowing ended by ESC: prev_esc carries, ']' re-opens OSC —
        // matching vte's Escape → OscString path.
        let mut s = OscWatch::Swallowing;
        let mut pe = false;
        assert!(step(&mut s, &mut pe, 0x1B));
        assert!(pe);
        assert!(step(&mut s, &mut pe, b']'));
        assert_eq!(s, OscWatch::InOsc { count: 0 });
    }

    // End-to-end sanity through the pure FSM: a full OSC 0 title sequence
    // passes untouched and lands back in Ground.
    #[test]
    fn full_title_sequence_round_trips() {
        let mut s = OscWatch::Ground;
        let mut pe = false;
        for b in b"\x1b]0;hello\x07tail" {
            assert!(step(&mut s, &mut pe, *b), "{b:#04x} forwarded");
        }
        assert_eq!(s, OscWatch::Ground);
    }
}
