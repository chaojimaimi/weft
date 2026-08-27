//! v1.11.4 (PLAN_v1114 §4.5/§4.4): legacy-twin parameterization, the
//! decision-table row-coverage golden matrix and the L2 handler seam.
//! Nested via `#[path]` from `kitty_tests.rs` (module-size gate).

use super::*;

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

// ── legacy twin (PLAN_v1114 §4.5) ────────────────────────────────

/// Parameterized twin: every legacy encoder row, flags=0, must be
/// byte-identical through the handler's single point.
#[test]
fn legacy_twin_flags_zero_byte_identical() {
    let cases: &[(KeyCode, Modifiers, Option<&str>, &[u8])] = &[
        (KeyCode::Char('a'), Modifiers::empty(), None, b"a"),
        (KeyCode::Char('A'), Modifiers::empty(), None, b"A"),
        (KeyCode::Char('a'), Modifiers::SHIFT, None, b"A"),
        (KeyCode::Char('/'), Modifiers::SHIFT, None, b"?"),
        (KeyCode::Char('`'), Modifiers::SHIFT, None, b"~"),
        (KeyCode::Char('a'), Modifiers::CONTROL, None, b"\x01"),
        (KeyCode::Char('['), Modifiers::CONTROL, None, b"\x1b"),
        (KeyCode::Char('a'), Modifiers::ALT, None, b"\x1ba"),
        (KeyCode::Char(';'), Modifiers::CONTROL, None, b";"),
        (KeyCode::Enter, Modifiers::empty(), None, b"\r"),
        (KeyCode::Enter, Modifiers::ALT, None, b"\x1b\r"),
        (KeyCode::Tab, Modifiers::empty(), None, b"\t"),
        (KeyCode::Tab, Modifiers::SHIFT, None, b"\x1b[Z"),
        (
            KeyCode::Tab,
            Modifiers::SHIFT | Modifiers::ALT,
            None,
            b"\x1b[Z",
        ),
        (KeyCode::Backspace, Modifiers::empty(), None, b"\x7f"),
        (KeyCode::Backspace, Modifiers::ALT, None, b"\x1b\x7f"),
        (KeyCode::Escape, Modifiers::empty(), None, b"\x1b"),
        (KeyCode::Up, Modifiers::empty(), None, b"\x1b[A"),
        (KeyCode::Up, Modifiers::SHIFT, None, b"\x1b[1;2A"),
        (KeyCode::Right, Modifiers::CONTROL, None, b"\x1b[1;5C"),
        (KeyCode::Left, Modifiers::ALT, None, b"\x1b[1;3D"),
        (KeyCode::Home, Modifiers::empty(), None, b"\x1b[1~"),
        (KeyCode::End, Modifiers::empty(), None, b"\x1b[4~"),
        (KeyCode::Home, Modifiers::SHIFT, None, b"\x1b[1;2~"),
        (KeyCode::PageUp, Modifiers::empty(), None, b"\x1b[H"),
        (KeyCode::PageDown, Modifiers::empty(), None, b"\x1b[I"),
        (KeyCode::Delete, Modifiers::empty(), None, b"\x1b[3~"),
        (KeyCode::Insert, Modifiers::empty(), None, b"\x1b[2~"),
        (KeyCode::F(1), Modifiers::empty(), None, b"\x1bOP"),
        (KeyCode::F(4), Modifiers::empty(), None, b"\x1bOS"),
        (KeyCode::F(1), Modifiers::SHIFT, None, b"\x1b[1;2P"),
        (KeyCode::F(5), Modifiers::empty(), None, b"\x1b[15~"),
        (KeyCode::F(10), Modifiers::empty(), None, b"\x1b[21~"),
        (KeyCode::F(11), Modifiers::empty(), None, b"\x1b[23~"),
        (KeyCode::F(12), Modifiers::CONTROL, None, b"\x1b[24;5~"),
        (KeyCode::Numpad('5'), Modifiers::empty(), None, b"5"),
    ];
    for (key, mods, text, expected) in cases {
        let h = InputHandler::new();
        assert_eq!(
            h.encode_key(*key, *mods),
            *expected,
            "encode_key {key:?} {mods:?}"
        );
        assert_eq!(
            h.encode_key_text(*key, *mods, *text),
            *expected,
            "encode_key_text {key:?} {mods:?}"
        );
    }
    // with the event kind flipped the flags=0 stream must not change either
    for kind in [KittyEventKind::Repeat, KittyEventKind::Release] {
        let mut h = InputHandler::new();
        h.kitty_event_kind = kind;
        assert_eq!(h.encode_key(KeyCode::Up, Modifiers::CONTROL), b"\x1b[1;5A");
        assert_eq!(h.encode_key(KeyCode::Char('a'), Modifiers::empty()), b"a");
    }
}

// ── golden count eligibility: every decision-table row ≥2 vectors ─

#[test]
fn decision_table_row_coverage() {
    // 可打印无修饰 L1 原文 / L4 / L5
    assert_eq!(enc_h(0b1, KeyCode::Char('x'), Modifiers::empty()), b"x");
    assert_eq!(
        enc(0b1000, KeyCode::Char('x'), Modifiers::empty()),
        b"\x1b[120u"
    );
    assert_eq!(
        enc(0b11000, KeyCode::Char('x'), Modifiers::empty()),
        b"\x1b[120;;120u"
    );
    // Shift+可打印 三级
    assert_eq!(
        enc_text(0b1, KeyCode::Char('x'), Modifiers::SHIFT, Some("X")),
        b"X"
    );
    assert_eq!(
        enc(0b1000, KeyCode::Char('x'), Modifiers::SHIFT),
        b"\x1b[120;2u"
    );
    assert_eq!(
        enc(0b11000, KeyCode::Char('x'), Modifiers::SHIFT),
        b"\x1b[120;2;120u"
    );
    // Esc 三级
    assert_eq!(enc(0b1, KeyCode::Escape, Modifiers::empty()), b"\x1b[27u");
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
        enc(0b1000, KeyCode::Escape, Modifiers::empty()),
        b"\x1b[27u"
    );
    // Ctrl+无C0键
    assert_eq!(
        enc(0b1, KeyCode::Char(')'), Modifiers::CONTROL),
        b"\x1b[41;5u"
    );
    assert_eq!(
        enc(0b1000, KeyCode::F(9), Modifiers::CONTROL),
        b"\x1b[20;5~"
    );
    // Ctrl+Enter/Tab/BS
    assert_eq!(
        enc(0b1, KeyCode::Backspace, Modifiers::CONTROL),
        b"\x1b[127;5u"
    );
    // Alt+Enter / Alt+可打印
    assert_eq!(enc(0b1, KeyCode::Enter, Modifiers::ALT), b"\x1b[13;3u");
    assert_eq!(enc(0b1, KeyCode::Char('a'), Modifiers::ALT), b"\x1b[97;3u");
    // Enter/Tab/BS 无修饰 全级
    assert_eq!(enc_h(0b1, KeyCode::Enter, Modifiers::empty()), b"\r");
    assert_eq!(enc(0b1000, KeyCode::Enter, Modifiers::empty()), b"\x1b[13u");
    // Shift+Tab 三级
    assert_eq!(enc_h(0b1, KeyCode::Tab, Modifiers::SHIFT), b"\x1b[Z");
    assert_eq!(enc_h(0b11, KeyCode::Tab, Modifiers::SHIFT), b"\x1b[Z");
    assert_eq!(enc(0b1000, KeyCode::Tab, Modifiers::SHIFT), b"\x1b[9;2u");
    // 箭头无修饰 / 带修饰
    assert_eq!(enc_h(0b1000, KeyCode::Up, Modifiers::empty()), b"\x1b[A");
    assert_eq!(
        enc(0b1000, KeyCode::Right, Modifiers::CONTROL),
        b"\x1b[1;5C"
    );
    // 箭头/导航 L4 u 形
    assert_eq!(enc(0b1000, KeyCode::End, Modifiers::empty()), b"\x1b[8~");
    assert_eq!(enc(0b1000, KeyCode::End, Modifiers::ALT), b"\x1b[8;3~");
    // Super
    assert_eq!(
        enc(0b1, KeyCode::Char('v'), Modifiers::SUPER),
        b"\x1b[118;9u"
    );
    assert_eq!(enc(0b1, KeyCode::Down, Modifiers::SUPER), b"\x1b[1;9B");
    // 物化 ;1:N
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
}

/// L2 end-to-end through the handler (the pipe's encoder side).
#[test]
fn handler_l2_event_kind_reaches_encoder() {
    let mut h = InputHandler::new();
    h.kitty_flags = 0b11;
    h.kitty_event_kind = KittyEventKind::Repeat;
    assert_eq!(
        h.encode_key_text(KeyCode::Escape, Modifiers::SHIFT, None),
        b"\x1b[27;2:2u"
    );
    h.kitty_event_kind = KittyEventKind::Press;
    assert_eq!(
        h.encode_key_text(KeyCode::Escape, Modifiers::SHIFT, None),
        b"\x1b[27;2u"
    );
    h.kitty_flags = 0b1; // without 0b10 the kind is inert
    assert_eq!(
        h.encode_key_text(KeyCode::Escape, Modifiers::SHIFT, None),
        b"\x1b[27;2u"
    );
}

/// rust-reviewer v1.11.4 Blocker-1 end-to-end releases: a released Ctrl+C
/// must be escape-CODED (never a second 0x03), a released Enter must not
/// double-commit, and releases of C0-mappable letters follow the same rule.
#[test]
fn release_events_are_escape_coded_never_legacy() {
    assert_eq!(
        super::encode_kitty_key(
            0b11,
            KittyEventKind::Release,
            KeyCode::Char('c'),
            Modifiers::CONTROL,
            Some("c")
        ),
        Some(b"\x1b[99;5:3u".to_vec()),
        "released ctrl+c is a CSI report, NOT a second interrupt"
    );
    assert_eq!(
        super::encode_kitty_key(
            0b11,
            KittyEventKind::Release,
            KeyCode::Enter,
            Modifiers::empty(),
            None
        ),
        Some(b"\x1b[13;1:3u".to_vec()),
        "released Enter must not double-commit"
    );
}

/// AUDIT follow-up: SUPPORTED_FLAG_MASK cuts 0b1000 (Blocker-2 downgrade
/// clause) — a negotiated 0b1000 can never come back in the ack.
#[test]
fn mask_cuts_report_all_keys_bit() {
    let mut s = crate::vt::kitty_keyboard::KittyKeyboardState::default();
    s.push(false, 0b1_1011);
    assert_eq!(
        s.flags(false),
        0b1_0011,
        "0b1000 must be masked out of the negotiated set"
    );
}
