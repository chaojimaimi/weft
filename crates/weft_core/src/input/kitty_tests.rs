use super::*;
use crate::input::{InputHandler, Modifiers};

fn enc(flags: u8, key: KeyCode, mods: Modifiers) -> Vec<u8> {
    encode_kitty_key(flags, KittyEventKind::Press, key, mods, None)
        .expect("kitty encode must claim this key")
}

fn enc_text(flags: u8, key: KeyCode, mods: Modifiers, text: Option<&str>) -> Vec<u8> {
    encode_kitty_key(flags, KittyEventKind::Press, key, mods, text)
        .expect("kitty encode must claim this key")
}

/// Through-the-handler form: the kitty branch first, legacy fallback
/// after — what the PTY actually receives (legacy-delegated rows).
fn enc_h(flags: u8, key: KeyCode, mods: Modifiers) -> Vec<u8> {
    let mut h = InputHandler::new();
    h.kitty_flags = flags;
    h.encode_key(key, mods)
}

fn enc_h_text(flags: u8, key: KeyCode, mods: Modifiers, text: Option<&str>) -> Vec<u8> {
    let mut h = InputHandler::new();
    h.kitty_flags = flags;
    h.encode_key_text(key, mods, text)
}

// ── raw-text rows (L1/L2 printables) ─────────────────────────────

#[test]
fn l1_plain_printable_is_raw_utf8() {
    assert_eq!(
        enc_text(0b1, KeyCode::Char('a'), Modifiers::empty(), Some("a")),
        b"a"
    );
    // non-US layout text passes through untouched
    assert_eq!(
        enc_text(0b1, KeyCode::Char('z'), Modifiers::empty(), Some("y")),
        b"y"
    );
    // CJK layout text
    assert_eq!(
        enc_text(0b1, KeyCode::Char('z'), Modifiers::empty(), Some("中")),
        "中".as_bytes()
    );
    assert_eq!(
        enc_h_text(0b1, KeyCode::Char('a'), Modifiers::empty(), None),
        b"a",
        "no text ⇒ legacy raw char"
    );
}

#[test]
fn l2_plain_printable_press_repeat_raw_release_escape_coded() {
    // L2 column = 原文 for Press/Repeat — repeats do NOT materialize for
    // plain printables. Release is ALWAYS escape-coded once 0b10 is
    // negotiated (kitty spec, ReportEventTypes) — raw text may never ride
    // the release path (rust-reviewer v1.11.4 Blocker-1).
    for kind in [KittyEventKind::Press, KittyEventKind::Repeat] {
        let bytes = encode_kitty_key(
            0b11,
            kind,
            KeyCode::Char('a'),
            Modifiers::empty(),
            Some("a"),
        );
        assert_eq!(bytes, Some(b"a".to_vec()), "{kind:?} must stay raw");
    }
    assert_eq!(
        encode_kitty_key(
            0b11,
            KittyEventKind::Release,
            KeyCode::Char('a'),
            Modifiers::empty(),
            Some("a")
        ),
        Some(b"\x1b[97;1:3u".to_vec()),
        "release is escape-coded, never raw text"
    );
}

#[test]
fn l1_shift_printable_is_raw_utf8() {
    assert_eq!(
        enc_text(0b1, KeyCode::Char('a'), Modifiers::SHIFT, Some("A")),
        b"A"
    );
    assert_eq!(
        enc_text(0b1, KeyCode::Char('/'), Modifiers::SHIFT, Some("?")),
        b"?"
    );
    // no text: falls to legacy shift mapping ('a' → 'A')
    assert_eq!(enc_h(0b1, KeyCode::Char('a'), Modifiers::SHIFT), b"A");
}

// ── L4/L5 printable rows ─────────────────────────────────────────

#[test]
fn l4_plain_printable_uses_key_code() {
    assert_eq!(
        enc(0b1000, KeyCode::Char('a'), Modifiers::empty()),
        b"\x1b[97u"
    );
    assert_eq!(
        enc(0b1000, KeyCode::Char('A'), Modifiers::empty()),
        b"\x1b[65u"
    );
    // kc comes from the LAYOUT text (v1.16.0 PLAN §4 primary source — the
    // unmodified L4 primary is the layout's unshifted code point; the
    // pre-v1.16 physical-table answer 122 was the spec gap this fixes)
    assert_eq!(
        enc_text(0b1000, KeyCode::Char('z'), Modifiers::empty(), Some("y")),
        b"\x1b[121u"
    );
    // CJK base char → its code point
    assert_eq!(
        enc(0b1000, KeyCode::Char('中'), Modifiers::empty()),
        b"\x1b[20013u"
    );
}

#[test]
fn l4_shift_printable_is_key_code_with_shift_modifier() {
    assert_eq!(
        enc(0b1000, KeyCode::Char('a'), Modifiers::SHIFT),
        b"\x1b[97;2u"
    );
    assert_eq!(
        enc_text(0b1000, KeyCode::Char('a'), Modifiers::SHIFT, Some("A")),
        b"\x1b[97;2u"
    );
}

#[test]
fn l5_printable_carries_text_segment() {
    assert_eq!(
        enc_text(0b11000, KeyCode::Char('a'), Modifiers::empty(), Some("a")),
        b"\x1b[97;;97u"
    );
    // shift: `CSI kc;2;cps u`, no :sk (M5)
    assert_eq!(
        enc_text(0b11000, KeyCode::Char('a'), Modifiers::SHIFT, Some("A")),
        b"\x1b[97;2;65u"
    );
    // multi-codepoint text joins with ':'
    assert_eq!(
        enc_text(
            0b11000,
            KeyCode::Char('e'),
            Modifiers::empty(),
            Some("e\u{301}")
        ),
        b"\x1b[101;;101:769u"
    );
    // no text → base char code point
    assert_eq!(
        enc(0b11000, KeyCode::Char('a'), Modifiers::empty()),
        b"\x1b[97;;97u"
    );
}

#[test]
fn l5_alt_printable_carries_cps() {
    assert_eq!(
        enc_text(0b11011, KeyCode::Char('a'), Modifiers::ALT, Some("a")),
        b"\x1b[97;3;97u"
    );
}

#[test]
fn l4_pure_text_control_char_materializes_kc_zero() {
    // defensive: a control char in the base slot has kc=0, text carries
    // the payload
    assert_eq!(
        enc_text(0b11000, KeyCode::Char('\r'), Modifiers::empty(), Some("\r")),
        b"\x1b[0;;13u"
    );
}

// ── Esc row (M2) ─────────────────────────────────────────────────

#[test]
fn esc_row_all_levels() {
    // L1: CSI 27u — M2 (never the bare ESC byte once negotiated)
    assert_eq!(enc(0b1, KeyCode::Escape, Modifiers::empty()), b"\x1b[27u");
    // L2 press: same; repeat/release materialize
    assert_eq!(enc(0b11, KeyCode::Escape, Modifiers::empty()), b"\x1b[27u");
    assert_eq!(
        encode_kitty_key(
            0b11,
            KittyEventKind::Repeat,
            KeyCode::Escape,
            Modifiers::empty(),
            None
        )
        .unwrap(),
        b"\x1b[27;1:2u"
    );
    assert_eq!(
        encode_kitty_key(
            0b11,
            KittyEventKind::Release,
            KeyCode::Escape,
            Modifiers::empty(),
            None
        )
        .unwrap(),
        b"\x1b[27;1:3u"
    );
    // L4: CSI 27u
    assert_eq!(
        enc(0b1000, KeyCode::Escape, Modifiers::empty()),
        b"\x1b[27u"
    );
    // with modifiers
    assert_eq!(enc(0b1, KeyCode::Escape, Modifiers::ALT), b"\x1b[27;3u");
    assert_eq!(
        encode_kitty_key(
            0b11,
            KittyEventKind::Repeat,
            KeyCode::Escape,
            Modifiers::SHIFT,
            None
        )
        .unwrap(),
        b"\x1b[27;2:2u"
    );
}

// ── C0 / InterruptPty invariant ──────────────────────────────────

/// The hard gate (PLAN_v1114 §5): Ctrl+letter stays C0 at EVERY level —
/// `Effect::InterruptPty`'s `[0x03]` atomic interrupt chain must never
/// be CSI-ified.
#[test]
fn interrupt_pty_ctrl_c_is_0x03_at_every_level() {
    for flags in [0b1, 0b11, 0b1011, 0b11011, 0b11111] {
        let bytes = encode_kitty_key(
            flags,
            KittyEventKind::Press,
            KeyCode::Char('c'),
            Modifiers::CONTROL,
            Some("c"),
        );
        let via_handler = {
            let mut h = InputHandler::new();
            h.kitty_flags = flags;
            h.encode_key_text(KeyCode::Char('c'), Modifiers::CONTROL, Some("c"))
        };
        assert_eq!(bytes, None, "kitty branch defers ctrl+c; flags=0b{flags:b}");
        assert_eq!(via_handler, b"\x03", "handler path flags=0b{flags:b}");
    }
    // repeats must not matter either — the encoder side of the L2 pipe
    let mut ih = InputHandler::new();
    ih.kitty_flags = 0b11011;
    ih.kitty_event_kind = KittyEventKind::Repeat;
    assert_eq!(
        ih.encode_key_text(KeyCode::Char('c'), Modifiers::CONTROL, Some("c")),
        b"\x03"
    );
}

#[test]
fn ctrl_letters_stay_c0_all_levels() {
    for flags in [0b1, 0b11, 0b1011, 0b11011] {
        for c in ['a', 'z', 'A', '[', '\\', '@'] {
            let bytes = encode_kitty_key(
                flags,
                KittyEventKind::Press,
                KeyCode::Char(c),
                Modifiers::CONTROL,
                None,
            );
            assert_eq!(bytes, None, "ctrl+{c:?} defers to legacy at 0b{flags:b}");
        }
    }
}

// ── Ctrl on non-C0 keys ──────────────────────────────────────────

#[test]
fn ctrl_semicolon_is_kc_59_mod_5() {
    // the plan's golden: Ctrl+; = 59
    assert_eq!(
        enc(0b1, KeyCode::Char(';'), Modifiers::CONTROL),
        b"\x1b[59;5u"
    );
    assert_eq!(
        enc(0b1000, KeyCode::Char(';'), Modifiers::CONTROL),
        b"\x1b[59;5u"
    );
    assert_eq!(
        enc(0b1, KeyCode::Char('1'), Modifiers::CONTROL),
        b"\x1b[49;5u"
    );
    assert_eq!(
        enc(0b1, KeyCode::Char(' '), Modifiers::CONTROL),
        b"\x1b[32;5u"
    );
    assert_eq!(
        enc(0b1, KeyCode::F(5), Modifiers::CONTROL),
        b"\x1b[15;5~",
        "F keys keep the family terminator (reviewer Blocker-2)"
    );
    assert_eq!(
        enc_h(0b1, KeyCode::Up, Modifiers::CONTROL),
        b"\x1b[1;5A",
        "arrows are NOT in the ctrl row"
    );
    assert_eq!(
        encode_kitty_key(
            0b11,
            KittyEventKind::Repeat,
            KeyCode::Char(';'),
            Modifiers::CONTROL,
            None
        )
        .unwrap(),
        b"\x1b[59;5:2u"
    );
    // ctrl+shift+; → 6
    assert_eq!(
        enc(
            0b1,
            KeyCode::Char(';'),
            Modifiers::CONTROL | Modifiers::SHIFT
        ),
        b"\x1b[59;6u"
    );
}

#[test]
fn ctrl_enter_tab_bs_rows() {
    assert_eq!(enc(0b1, KeyCode::Enter, Modifiers::CONTROL), b"\x1b[13;5u");
    assert_eq!(enc(0b1, KeyCode::Tab, Modifiers::CONTROL), b"\x1b[9;5u");
    assert_eq!(
        enc(0b1, KeyCode::Backspace, Modifiers::CONTROL),
        b"\x1b[127;5u"
    );
    // same at L4 (同左)
    assert_eq!(
        enc(0b1000, KeyCode::Enter, Modifiers::CONTROL),
        b"\x1b[13;5u"
    );
    // L2 repeat: the :2 sub-segment requires Flag 8 for the three keys
    // (v1.16.0 conservative ruling, PLAN §4) — the press form stands in.
    assert_eq!(
        encode_kitty_key(
            0b11,
            KittyEventKind::Repeat,
            KeyCode::Enter,
            Modifiers::CONTROL,
            None
        )
        .unwrap(),
        b"\x1b[13;5u"
    );
    assert_eq!(
        encode_kitty_key(
            0b1_1011,
            KittyEventKind::Repeat,
            KeyCode::Enter,
            Modifiers::CONTROL,
            None
        )
        .unwrap(),
        b"\x1b[13;5:2u",
        "with Flag 8 the repeat materializes"
    );
}

// The Alt rows test lives in kitty_golden_tests.rs (module-size gate —
// this file stays under the 800-line budget; golden owns row coverage).

// ── Enter/Tab/BS plain rows ──────────────────────────────────────

#[test]
fn plain_enter_tab_bs_stay_legacy_through_l2_no_release() {
    for flags in [0b1, 0b11] {
        assert_eq!(
            enc_h(flags, KeyCode::Enter, Modifiers::empty()),
            b"\r",
            "0b{flags:b}"
        );
        assert_eq!(enc_h(flags, KeyCode::Tab, Modifiers::empty()), b"\t");
        assert_eq!(
            enc_h(flags, KeyCode::Backspace, Modifiers::empty()),
            b"\x7f"
        );
        // No release sub-segments WITHOUT 0b10 — and since v1.16.0 (PLAN
        // §4) NONE without 0b1000 either for the Enter/Tab/BS family (spec:
        // no release events unless ReportAllKeysAsEscapeCodes): the app-side
        // gate drops the event, the encoder's None guards the seam. Repeats
        // of the plain keys stay legacy `\r` at every sub-L4 level.
        for kind in [KittyEventKind::Repeat, KittyEventKind::Release] {
            let bytes = encode_kitty_key(flags, kind, KeyCode::Enter, Modifiers::empty(), None);
            assert_eq!(
                bytes, None,
                "no Flag 8 ⇒ the three keys report no :2/:3; 0b{flags:b} {kind:?}"
            );
        }
    }
}

#[test]
fn plain_enter_tab_bs_kitty_forms_from_l4() {
    assert_eq!(enc(0b1000, KeyCode::Enter, Modifiers::empty()), b"\x1b[13u");
    assert_eq!(enc(0b1000, KeyCode::Tab, Modifiers::empty()), b"\x1b[9u");
    assert_eq!(
        enc(0b1000, KeyCode::Backspace, Modifiers::empty()),
        b"\x1b[127u"
    );
    // L5: Enter adds its cps, Tab/BS don't (Enter-only row)
    assert_eq!(
        enc(0b11000, KeyCode::Enter, Modifiers::empty()),
        b"\x1b[13;;13u"
    );
    assert_eq!(enc(0b11000, KeyCode::Tab, Modifiers::empty()), b"\x1b[9u");
    assert_eq!(
        enc(0b11000, KeyCode::Backspace, Modifiers::empty()),
        b"\x1b[127u"
    );
}

// ── Shift+Tab three levels ───────────────────────────────────────

#[test]
fn shift_tab_stays_legacy_until_l4() {
    assert_eq!(enc_h(0b1, KeyCode::Tab, Modifiers::SHIFT), b"\x1b[Z");
    assert_eq!(enc_h(0b11, KeyCode::Tab, Modifiers::SHIFT), b"\x1b[Z");
    assert_eq!(
        encode_kitty_key(
            0b11,
            KittyEventKind::Repeat,
            KeyCode::Tab,
            Modifiers::SHIFT,
            None
        ),
        None,
        "L2 still legacy for Shift+Tab"
    );
    assert_eq!(
        enc_h(0b11, KeyCode::Tab, Modifiers::SHIFT),
        b"\x1b[Z",
        "handler still emits legacy CSI Z at L2"
    );
    assert_eq!(enc(0b1000, KeyCode::Tab, Modifiers::SHIFT), b"\x1b[9;2u");
    assert_eq!(
        encode_kitty_key(
            0b1010,
            KittyEventKind::Repeat,
            KeyCode::Tab,
            Modifiers::SHIFT,
            None
        )
        .unwrap(),
        b"\x1b[9;2:2u"
    );
}

#[cfg(test)]
#[path = "kitty_v1160_tests.rs"]
mod v1160;

#[cfg(test)]
#[path = "kitty_golden_tests.rs"]
mod golden;

#[cfg(test)]
#[path = "kitty_key_tables_tests.rs"]
mod key_tables;
