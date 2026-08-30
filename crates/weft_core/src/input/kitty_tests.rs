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
    // kc comes from the BASE key, not layout text
    assert_eq!(
        enc_text(0b1000, KeyCode::Char('z'), Modifiers::empty(), Some("y")),
        b"\x1b[122u"
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
    assert_eq!(
        encode_kitty_key(
            0b11,
            KittyEventKind::Repeat,
            KeyCode::Enter,
            Modifiers::CONTROL,
            None
        )
        .unwrap(),
        b"\x1b[13;5:2u"
    );
}

// ── Alt rows ─────────────────────────────────────────────────────

#[test]
fn alt_printable_and_enter_rows() {
    // Alt+a: CSI 97;3u — NO ESC prefix (deviation from legacy \x1ba)
    assert_eq!(enc(0b1, KeyCode::Char('a'), Modifiers::ALT), b"\x1b[97;3u");
    // Alt+Enter (new row): CSI 13;3u
    assert_eq!(enc(0b1, KeyCode::Enter, Modifiers::ALT), b"\x1b[13;3u");
    assert_eq!(
        encode_kitty_key(
            0b11,
            KittyEventKind::Repeat,
            KeyCode::Enter,
            Modifiers::ALT,
            None
        )
        .unwrap(),
        b"\x1b[13;3:2u"
    );
    // L5: Alt+Enter carries \r cps
    assert_eq!(
        enc(0b11011, KeyCode::Enter, Modifiers::ALT),
        b"\x1b[13;3;13u"
    );
    // Alt+Backspace / Alt+Tab
    assert_eq!(enc(0b1, KeyCode::Backspace, Modifiers::ALT), b"\x1b[127;3u");
    assert_eq!(enc(0b1, KeyCode::Tab, Modifiers::ALT), b"\x1b[9;3u");
}

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
        // No release sub-segments WITHOUT 0b10 — the reset\n channel stays
        // usable. With 0b10 the release IS escape-coded (reviewer
        // Blocker-1 fix): raw text / legacy bytes may never ride release.
        for kind in [KittyEventKind::Repeat, KittyEventKind::Release] {
            let bytes = encode_kitty_key(flags, kind, KeyCode::Enter, Modifiers::empty(), None);
            let unreported = kind == KittyEventKind::Repeat
                || flags & crate::input::kitty::FLAG_REPORT_EVENT_TYPES == 0;
            if unreported {
                assert_eq!(
                    bytes, None,
                    "no event-types ⇒ no release report; 0b{flags:b} {kind:?}"
                );
            } else {
                assert_eq!(
                    bytes,
                    Some(b"\x1b[13;1:3u".to_vec()),
                    "0b10 release is escape-coded; 0b{flags:b} {kind:?}"
                );
            }
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

// ── arrows ───────────────────────────────────────────────────────

#[test]
fn plain_arrows_stay_legacy_at_l1_l2_and_l4() {
    for flags in [0b1, 0b11, 0b1000] {
        assert_eq!(
            enc_h(flags, KeyCode::Up, Modifiers::empty()),
            b"\x1b[A",
            "0b{flags:b}"
        );
        assert_eq!(enc_h(flags, KeyCode::Down, Modifiers::empty()), b"\x1b[B");
        assert_eq!(enc_h(flags, KeyCode::Right, Modifiers::empty()), b"\x1b[C");
        assert_eq!(enc_h(flags, KeyCode::Left, Modifiers::empty()), b"\x1b[D");
    }
}

#[test]
fn modified_arrows_keep_legacy_shape_at_l1_flip_at_l4() {
    // L1 = legacy CSI 1;5C (which IS kitty's L1 shape)
    assert_eq!(enc_h(0b1, KeyCode::Right, Modifiers::CONTROL), b"\x1b[1;5C");
    assert_eq!(enc_h(0b1, KeyCode::Up, Modifiers::SHIFT), b"\x1b[1;2A");
    assert_eq!(enc_h(0b1, KeyCode::Left, Modifiers::ALT), b"\x1b[1;3D");
    // L2 repeat: `CSI 1;5:2C`
    assert_eq!(
        encode_kitty_key(
            0b11,
            KittyEventKind::Repeat,
            KeyCode::Right,
            Modifiers::CONTROL,
            None
        )
        .unwrap(),
        b"\x1b[1;5:2C"
    );
    // L4: same compat-tail bytes (with mods = legacy shape)
    assert_eq!(
        enc(0b1000, KeyCode::Right, Modifiers::CONTROL),
        b"\x1b[1;5C"
    );
}

#[test]
fn arrow_repeat_at_l4_materializes_1_1_event() {
    // the materialized `;1:N` on an unmodified arrow (compat tail)
    assert_eq!(
        encode_kitty_key(
            0b1010,
            KittyEventKind::Repeat,
            KeyCode::Up,
            Modifiers::empty(),
            None
        )
        .unwrap(),
        b"\x1b[1;1:2A"
    );
    assert_eq!(
        encode_kitty_key(
            0b1010,
            KittyEventKind::Release,
            KeyCode::Down,
            Modifiers::empty(),
            None
        )
        .unwrap(),
        b"\x1b[1;1:3B"
    );
}

#[test]
fn super_arrows_use_compat_tail() {
    assert_eq!(enc(0b1, KeyCode::Up, Modifiers::SUPER), b"\x1b[1;9A");
    assert_eq!(
        encode_kitty_key(
            0b11,
            KittyEventKind::Repeat,
            KeyCode::Left,
            Modifiers::SUPER,
            None
        )
        .unwrap(),
        b"\x1b[1;9:2D"
    );
}

#[test]
fn super_printable_and_fn_rows() {
    assert_eq!(
        enc(0b1, KeyCode::Char('a'), Modifiers::SUPER),
        b"\x1b[97;9u"
    );
    assert_eq!(enc(0b1, KeyCode::F(1), Modifiers::SUPER), b"\x1b[1;9P");
    assert_eq!(enc(0b1, KeyCode::Home, Modifiers::SUPER), b"\x1b[7;9~");
    assert_eq!(
        encode_kitty_key(
            0b11,
            KittyEventKind::Repeat,
            KeyCode::Char('a'),
            Modifiers::SUPER,
            None
        )
        .unwrap(),
        b"\x1b[97;9:2u"
    );
}

// ── navigation/family rows ───────────────────────────────────────

#[test]
fn home_end_pgup_pgdn_ins_del_plain() {
    for flags in [0b1, 0b11] {
        assert_eq!(
            enc_h(flags, KeyCode::Home, Modifiers::empty()),
            b"\x1b[1~",
            "0b{flags:b}"
        );
        assert_eq!(enc_h(flags, KeyCode::End, Modifiers::empty()), b"\x1b[4~");
        // v1.11.13 (PLAN_v11113 §M3): the handler fallback chain now emits
        // the standard xterm tilde form (PageUp quirk fix — was `CSI H`/`I`).
        assert_eq!(
            enc_h(flags, KeyCode::PageUp, Modifiers::empty()),
            b"\x1b[5~"
        );
        assert_eq!(
            enc_h(flags, KeyCode::PageDown, Modifiers::empty()),
            b"\x1b[6~"
        );
        assert_eq!(
            enc_h(flags, KeyCode::Insert, Modifiers::empty()),
            b"\x1b[2~"
        );
        assert_eq!(
            enc_h(flags, KeyCode::Delete, Modifiers::empty()),
            b"\x1b[3~"
        );
    }
    // L4 forms keep the family terminators (spec): Home7~ End8~ PgUp5~
    // PgDn6~ Ins2~ Del3~ — reviewer Blocker-2. Unreachable while 0b1000 is
    // masked; pinned for future unmasking.
    assert_eq!(enc(0b1000, KeyCode::Home, Modifiers::empty()), b"\x1b[7~");
    assert_eq!(enc(0b1000, KeyCode::End, Modifiers::empty()), b"\x1b[8~");
    assert_eq!(enc(0b1000, KeyCode::PageUp, Modifiers::empty()), b"\x1b[5~");
    assert_eq!(
        enc(0b1000, KeyCode::PageDown, Modifiers::empty()),
        b"\x1b[6~"
    );
    assert_eq!(enc(0b1000, KeyCode::Insert, Modifiers::empty()), b"\x1b[2~");
    assert_eq!(enc(0b1000, KeyCode::Delete, Modifiers::empty()), b"\x1b[3~");
}

#[test]
fn modified_navigation_keys_l1_legacy_l4_kitty() {
    // L1: legacy shapes for shift/alt; ctrl points at the ctrl row
    assert_eq!(enc_h(0b1, KeyCode::Home, Modifiers::SHIFT), b"\x1b[1;2~");
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
    assert_eq!(enc_h(0b1, KeyCode::F(1), Modifiers::SHIFT), b"\x1b[1;2P");
    // L2 repeat: ctrl row shape + :N (u-tail from L1 on)
    assert_eq!(
        encode_kitty_key(
            0b11,
            KittyEventKind::Repeat,
            KeyCode::F(5),
            Modifiers::CONTROL,
            None
        )
        .unwrap(),
        b"\x1b[15;5:2~"
    );
    assert_eq!(
        encode_kitty_key(
            0b11,
            KittyEventKind::Release,
            KeyCode::F(1),
            Modifiers::SHIFT,
            None
        )
        .unwrap(),
        b"\x1b[1;2:3P"
    );
    // L4: family terminators preserved
    assert_eq!(
        enc(0b1000, KeyCode::F(5), Modifiers::CONTROL),
        b"\x1b[15;5~"
    );
    assert_eq!(enc(0b1000, KeyCode::F(2), Modifiers::SHIFT), b"\x1b[1;2Q");
    assert_eq!(enc(0b1000, KeyCode::Home, Modifiers::ALT), b"\x1b[7;3~");
    // L4 + event on a family key
    assert_eq!(
        encode_kitty_key(
            0b1010,
            KittyEventKind::Repeat,
            KeyCode::F(6),
            Modifiers::SHIFT,
            None
        )
        .unwrap(),
        b"\x1b[17;2:2~"
    );
    assert_eq!(
        encode_kitty_key(
            0b1010,
            KittyEventKind::Repeat,
            KeyCode::Home,
            Modifiers::empty(),
            None
        )
        .unwrap(),
        b"\x1b[7;1:2~"
    );
}

#[test]
fn f_key_codes_l4() {
    // Spec terminators: F1-F4 = 1P..1S, F5+ = 15~/17~/.../24~ (reviewer
    // Blocker-2 — `u` is reserved for Esc/Enter/Tab/BS).
    assert_eq!(enc(0b1000, KeyCode::F(1), Modifiers::empty()), b"\x1b[1P");
    assert_eq!(enc(0b1000, KeyCode::F(2), Modifiers::empty()), b"\x1b[1Q");
    assert_eq!(enc(0b1000, KeyCode::F(3), Modifiers::empty()), b"\x1b[1R");
    assert_eq!(enc(0b1000, KeyCode::F(4), Modifiers::empty()), b"\x1b[1S");
    assert_eq!(enc(0b1000, KeyCode::F(5), Modifiers::empty()), b"\x1b[15~");
    assert_eq!(enc(0b1000, KeyCode::F(6), Modifiers::empty()), b"\x1b[17~");
    assert_eq!(enc(0b1000, KeyCode::F(7), Modifiers::empty()), b"\x1b[18~");
    assert_eq!(enc(0b1000, KeyCode::F(8), Modifiers::empty()), b"\x1b[19~");
    assert_eq!(enc(0b1000, KeyCode::F(9), Modifiers::empty()), b"\x1b[20~");
    assert_eq!(enc(0b1000, KeyCode::F(10), Modifiers::empty()), b"\x1b[21~");
    assert_eq!(enc(0b1000, KeyCode::F(11), Modifiers::empty()), b"\x1b[23~");
    assert_eq!(enc(0b1000, KeyCode::F(12), Modifiers::empty()), b"\x1b[24~");
}

#[test]
fn numpad_digit_goes_through_printable_rows() {
    assert_eq!(enc_h(0b1, KeyCode::Numpad('5'), Modifiers::empty()), b"5");
    assert_eq!(
        enc(0b1000, KeyCode::Numpad('5'), Modifiers::empty()),
        b"\x1b[53u"
    );
    assert_eq!(
        enc(0b1, KeyCode::Numpad('5'), Modifiers::CONTROL),
        b"\x1b[53;5u"
    );
}

// ── level masking / thresholds ───────────────────────────────────

#[test]
fn unsupported_flag_combinations_delegate_legacy() {
    // 0b100 (ReportAlternateKeys alone) is cut — no kitty behavior
    let bytes = encode_kitty_key(
        0b100,
        KittyEventKind::Press,
        KeyCode::Char('a'),
        Modifiers::empty(),
        Some("a"),
    );
    assert_eq!(bytes, None, "cut flag ⇒ legacy");
    // 0 (no flags) ⇒ legacy
    assert_eq!(
        encode_kitty_key(
            0,
            KittyEventKind::Press,
            KeyCode::Char('a'),
            Modifiers::empty(),
            None
        ),
        None
    );
}

#[test]
fn l5_implies_l4_forms_for_textless_keys() {
    // F key at L5 (0b11000 = all-keys + text): no text on a textless key —
    // same shape as L4, family terminator preserved (reviewer Blocker-2).
    assert_eq!(enc(0b11000, KeyCode::F(5), Modifiers::empty()), b"\x1b[15~");
    assert_eq!(enc(0b11000, KeyCode::Home, Modifiers::empty()), b"\x1b[7~");
    // Flags are orthogonal bits: associated text WITHOUT all-keys never
    // promotes text keys into CSI u (spec — text only attaches to keys
    // already escape-coded).
    assert_eq!(
        enc_text(0b1_0001, KeyCode::Char('a'), Modifiers::empty(), Some("a")),
        b"a"
    );
}

#[cfg(test)]
#[path = "kitty_golden_tests.rs"]
mod golden;
