//! v1.16.0 (PLAN_v1160): the kitty keyboard-protocol completion car's test
//! matrix — T1 keypad dedicated codes, T2 Flag 8 spec-table verification +
//! primary layout source, T3 Flag 4 shifted sub-segment. Nested via
//! `#[path]` from `kitty_tests.rs` (module-size gate, golden precedent).
//! Every T2 row cites its kitty-spec source (2026-10-27 pull): this file is
//! the discharge record of the v1.11.4 Blocker-2 "kitten show-key"
//! downgrade clause.

use super::*;

fn enc(flags: u8, key: KeyCode, mods: Modifiers) -> Vec<u8> {
    encode_kitty_key(flags, KittyEventKind::Press, key, mods, None)
        .expect("kitty encode must claim this key")
}

/// Through-the-handler form with the Flag 4 field set (the
/// `forward_key_to_pty` seam).
fn enc_hs_text(
    flags: u8,
    key: KeyCode,
    mods: Modifiers,
    text: Option<&str>,
    shifted: Option<char>,
) -> Vec<u8> {
    let mut h = InputHandler::new();
    h.kitty_flags = flags;
    h.kitty_shifted = shifted;
    h.encode_key_text(key, mods, text)
}

// ── T1 (PLAN_v1160 §3): keypad dedicated codes ───────────────────

/// KP_0-9 sweep: dedicated codes 57399-57408 from the Disambiguate flag
/// (kitty spec §7).
#[test]
fn t1_kp_digit_sweep_dedicated_codes() {
    for (c, code) in ('0'..='9').zip(57399..=57408) {
        assert_eq!(
            encode_kitty_key(
                0b1,
                KittyEventKind::Press,
                KeyCode::Numpad(c),
                Modifiers::empty(),
                None
            ),
            Some(format!("\x1b[{code}u").into_bytes()),
            "KP_{c} must answer the dedicated code {code}"
        );
    }
}

/// KP_5 三态 × (0b1, 0b0): unmodified / ctrl / event-type (PLAN §3.4).
#[test]
fn t1_kp5_three_states_by_gate() {
    // 无修饰
    assert_eq!(
        enc(0b1, KeyCode::Numpad('5'), Modifiers::empty()),
        b"\x1b[57404u"
    );
    assert_eq!(
        {
            let h = InputHandler::new();
            h.encode_key(KeyCode::Numpad('5'), Modifiers::empty())
        },
        b"5",
        "flags=0 ⇒ legacy plain digit (handler.rs arm unchanged)"
    );
    // 带 ctrl
    assert_eq!(
        enc(0b1, KeyCode::Numpad('5'), Modifiers::CONTROL),
        b"\x1b[57404;5u"
    );
    assert_eq!(
        {
            let h = InputHandler::new();
            h.encode_key(KeyCode::Numpad('5'), Modifiers::CONTROL)
        },
        b"5",
        "legacy numpad ignores ctrl (dead-key revival form)"
    );
    // 带事件类型 (0b11): repeat/release dedicated codes
    assert_eq!(
        encode_kitty_key(
            0b11,
            KittyEventKind::Repeat,
            KeyCode::Numpad('5'),
            Modifiers::empty(),
            None
        ),
        Some(b"\x1b[57404;1:2u".to_vec())
    );
    assert_eq!(
        encode_kitty_key(
            0b11,
            KittyEventKind::Release,
            KeyCode::Numpad('5'),
            Modifiers::empty(),
            None
        ),
        Some(b"\x1b[57404;1:3u".to_vec())
    );
    // 带事件类型 × 0b1 (no event-types flag): a repeat reads as a fresh
    // press, a release is unreportable (entry guard)
    assert_eq!(
        encode_kitty_key(
            0b1,
            KittyEventKind::Repeat,
            KeyCode::Numpad('5'),
            Modifiers::empty(),
            None
        ),
        Some(b"\x1b[57404u".to_vec())
    );
    assert_eq!(
        encode_kitty_key(
            0b1,
            KittyEventKind::Release,
            KeyCode::Numpad('5'),
            Modifiers::empty(),
            None
        ),
        None
    );
}

/// The KP gate is the FLAG BIT, not the negotiated level. Disambiguate OR
/// Flag 8 (the spec's "this enhancement implies all keys are automatically
/// disambiguated as well" — rust-reviewer v1.16.0 P1; PLAN §3.2 missed the
/// implication) claim the dedicated code; 0b10 / 0b10000 alone must not
/// promote, but a 0b10 release still stays escape-coded (code-point
/// stand-in, never legacy bytes).
#[test]
fn t1_kp_gate_is_flag_bit_not_level() {
    assert_eq!(
        encode_kitty_key(
            0b1000,
            KittyEventKind::Press,
            KeyCode::Numpad('5'),
            Modifiers::empty(),
            None
        ),
        Some(b"\x1b[57404u".to_vec()),
        "Flag 8 alone implies disambiguation ⇒ dedicated code (P1 fix)"
    );
    assert_eq!(
        encode_kitty_key(
            0b1010,
            KittyEventKind::Release,
            KeyCode::Numpad('5'),
            Modifiers::empty(),
            None
        ),
        Some(b"\x1b[57404;1:3u".to_vec()),
        "Flag 8 + EventTypes release: dedicated code, not the code-point stand-in"
    );
    assert_eq!(
        encode_kitty_key(
            0b1_0000,
            KittyEventKind::Press,
            KeyCode::Numpad('5'),
            Modifiers::empty(),
            None
        ),
        None,
        "0b10000 alone must not promote"
    );
    assert_eq!(
        encode_kitty_key(
            0b10,
            KittyEventKind::Release,
            KeyCode::Numpad('5'),
            Modifiers::empty(),
            None
        ),
        Some(b"\x1b[53;1:3u".to_vec()),
        "no dedicated code without the bit, but release stays escape-coded"
    );
}

/// L5 carries the `;cps` text slot on the dedicated code.
#[test]
fn t1_kp5_l5_cps_slot() {
    assert_eq!(
        enc(0b1_1001, KeyCode::Numpad('5'), Modifiers::empty()),
        b"\x1b[57404;;53u"
    );
}

/// KP row across the negotiation ladder — dedicated codes from 0b1 on,
/// the L5 cps form once the text bit joins, legacy byte at 0b0 (阶梯不回归).
#[test]
fn t1_kp_ladder() {
    for flags in [0b1_u8, 0b11, 0b1011] {
        assert_eq!(
            encode_kitty_key(
                flags,
                KittyEventKind::Press,
                KeyCode::Numpad('7'),
                Modifiers::empty(),
                None
            ),
            Some(b"\x1b[57406u".to_vec()),
            "0b{flags:b}"
        );
    }
    // 0b11011 / 0b11111 carry the text bit ⇒ level 5 ⇒ cps slot
    for flags in [0b11011_u8, 0b1_1111] {
        assert_eq!(
            encode_kitty_key(
                flags,
                KittyEventKind::Press,
                KeyCode::Numpad('7'),
                Modifiers::empty(),
                None
            ),
            Some(b"\x1b[57406;;55u".to_vec()),
            "0b{flags:b}: L5 adds the cps slot"
        );
    }
}

// ── T2 (PLAN_v1160 §4): Flag 8 spec-table verification ───────────

/// The L4 functional-key table, verified row-by-row against the kitty
/// spec (2026-10-27 pull) — the discharge record of the v1.11.4 Blocker-2
/// "kitten show-key" downgrade clause (PLAN_v1160 §4 step 1).
#[test]
fn t2_l4_functional_key_spec_table() {
    // spec "Report all keys as escape codes" CSI table: the four dedicated
    // codes — Enter 13 / Tab 9 / Backspace 127 / Esc 27, `u`-terminated
    // (flags 0b1000 = pure L4: all-keys, no text bit)
    assert_eq!(enc(0b1000, KeyCode::Enter, Modifiers::empty()), b"\x1b[13u");
    assert_eq!(enc(0b1000, KeyCode::Tab, Modifiers::empty()), b"\x1b[9u");
    assert_eq!(
        enc(0b1000, KeyCode::Backspace, Modifiers::empty()),
        b"\x1b[127u"
    );
    assert_eq!(
        enc(0b1000, KeyCode::Escape, Modifiers::empty()),
        b"\x1b[27u"
    );
    // spec functional table: Insert 2~, Delete 3~, PageUp 5~, PageDown 6~,
    // Home 7~ (alt form 1H), End 8~ (alt form 1F) — Weft pins the tilde forms
    assert_eq!(enc(0b1000, KeyCode::Insert, Modifiers::empty()), b"\x1b[2~");
    assert_eq!(enc(0b1000, KeyCode::Delete, Modifiers::empty()), b"\x1b[3~");
    assert_eq!(enc(0b1000, KeyCode::PageUp, Modifiers::empty()), b"\x1b[5~");
    assert_eq!(
        enc(0b1000, KeyCode::PageDown, Modifiers::empty()),
        b"\x1b[6~"
    );
    assert_eq!(enc(0b1000, KeyCode::Home, Modifiers::empty()), b"\x1b[7~");
    assert_eq!(enc(0b1000, KeyCode::End, Modifiers::empty()), b"\x1b[8~");
    // F1/F2/F4 = CSI 1P/1Q/1S; F3 = 13~ (the spec DELETED the CSI 1R form —
    // CPR conflict); F5-F12 = 15/17/18/19/20/21/23/24~
    assert_eq!(enc(0b1000, KeyCode::F(1), Modifiers::empty()), b"\x1b[1P");
    assert_eq!(enc(0b1000, KeyCode::F(2), Modifiers::empty()), b"\x1b[1Q");
    assert_eq!(enc(0b1000, KeyCode::F(3), Modifiers::empty()), b"\x1b[13~");
    assert_eq!(enc(0b1000, KeyCode::F(4), Modifiers::empty()), b"\x1b[1S");
    for (n, code) in [
        (5u8, "15"),
        (6, "17"),
        (7, "18"),
        (8, "19"),
        (9, "20"),
        (10, "21"),
        (11, "23"),
        (12, "24"),
    ] {
        assert_eq!(
            enc(0b1000, KeyCode::F(n), Modifiers::empty()),
            format!("\x1b[{code}~").into_bytes(),
            "F{n}"
        );
    }
    // printables = CSI kc u (L4)
    assert_eq!(
        enc(0b1000, KeyCode::Char('a'), Modifiers::empty()),
        b"\x1b[97u"
    );
    assert_eq!(
        enc(0b1000, KeyCode::Char('a'), Modifiers::SHIFT),
        b"\x1b[97;2u"
    );
    // L5 (0b1_1000 = all-keys + text): Enter/printable add the cps slot,
    // Tab/BS keep the bare form (Enter-only cps row)
    assert_eq!(
        enc(0b1_1000, KeyCode::Enter, Modifiers::empty()),
        b"\x1b[13;;13u"
    );
    assert_eq!(enc(0b1_1000, KeyCode::Tab, Modifiers::empty()), b"\x1b[9u");
    assert_eq!(
        enc_text(0b1_1000, KeyCode::Char('a'), Modifiers::empty(), Some("a")),
        b"\x1b[97;;97u"
    );
}

/// Enter/Tab/BS :2/:3 require Flag 8 (spec, ReportEventTypes: "do not
/// have release events unless ReportAllKeysAsEscapeCodes"); L2×L4
/// combination materializes them.
#[test]
fn t2_enter_tab_bs_events_require_flag8() {
    // with 0b10 but without 0b1000: nothing — release dropped, plain
    // repeat stays legacy
    for key in [KeyCode::Enter, KeyCode::Tab, KeyCode::Backspace] {
        assert_eq!(
            encode_kitty_key(0b11, KittyEventKind::Release, key, Modifiers::empty(), None),
            None,
            "{key:?} release without Flag 8 is dropped"
        );
        assert_eq!(
            encode_kitty_key(0b11, KittyEventKind::Repeat, key, Modifiers::empty(), None),
            None,
            "{key:?} plain repeat stays legacy below L4"
        );
    }
    // modified repeat at 0b11: press form stands in (conservative :2
    // ruling, explicit divergence)
    assert_eq!(
        encode_kitty_key(
            0b11,
            KittyEventKind::Repeat,
            KeyCode::Enter,
            Modifiers::CONTROL,
            None
        ),
        Some(b"\x1b[13;5u".to_vec())
    );
    // L2×L4 (0b1010 = event-types + all-keys, no text bit): the three
    // keys' :2/:3 materialize
    assert_eq!(
        encode_kitty_key(
            0b1010,
            KittyEventKind::Release,
            KeyCode::Tab,
            Modifiers::empty(),
            None
        ),
        Some(b"\x1b[9;1:3u".to_vec())
    );
    assert_eq!(
        encode_kitty_key(
            0b1010,
            KittyEventKind::Release,
            KeyCode::Backspace,
            Modifiers::SHIFT,
            None
        ),
        Some(b"\x1b[127;2:3u".to_vec())
    );
    assert_eq!(
        encode_kitty_key(
            0b1010,
            KittyEventKind::Repeat,
            KeyCode::Enter,
            Modifiers::ALT,
            None
        ),
        Some(b"\x1b[13;3:2u".to_vec())
    );
}

/// F3 has no CSI R form at ANY kitty level (spec deleted it) — the
/// modified F3 rides the tilde family. The flags=0 legacy stream is
/// untouched (ESC O R / CSI 1;5R).
#[test]
fn t2_f3_has_no_csi_r_form() {
    assert_eq!(enc(0b1, KeyCode::F(3), Modifiers::CONTROL), b"\x1b[13;5~");
    assert_eq!(
        enc(0b1000, KeyCode::F(3), Modifiers::SHIFT),
        b"\x1b[13;2~",
        "from L4 the modified F3 keeps the family terminator under Shift"
    );
    assert_eq!(
        encode_kitty_key(
            0b11,
            KittyEventKind::Repeat,
            KeyCode::F(3),
            Modifiers::CONTROL,
            None
        ),
        Some(b"\x1b[13;5:2~".to_vec())
    );
    assert_eq!(enc(0b1000, KeyCode::F(3), Modifiers::empty()), b"\x1b[13~");
    assert_eq!(
        {
            let h = InputHandler::new();
            h.encode_key(KeyCode::F(3), Modifiers::empty())
        },
        b"\x1bOR",
        "legacy F3 untouched"
    );
    assert_eq!(
        {
            let h = InputHandler::new();
            h.encode_key(KeyCode::F(3), Modifiers::CONTROL)
        },
        b"\x1b[1;5R",
        "legacy modified F3 untouched"
    );
    assert_eq!(
        {
            let h = InputHandler::new();
            h.encode_key(KeyCode::F(3), Modifiers::SHIFT)
        },
        b"\x1b[1;2R",
        "legacy L1 Shift+F3 delegation untouched (kitty branch defers below L4)"
    );
}

/// Primary layout source (PLAN §4 step 2): the unmodified L4/L5 primary
/// is the layout text's first char; Shift keeps the physical table; the
/// capslock artifact is guarded out (spec: must not be CSI 65).
#[test]
fn t2_primary_layout_source_rules() {
    // non-US layout: text wins on the no-mods path
    assert_eq!(
        enc_text(0b1000, KeyCode::Char('z'), Modifiers::empty(), Some("y")),
        b"\x1b[121u"
    );
    // ASCII layout: text == physical ⇒ zero change
    assert_eq!(
        enc_text(0b1000, KeyCode::Char('a'), Modifiers::empty(), Some("a")),
        b"\x1b[97u"
    );
    // Shift down: text is the SHIFTED char — physical stays primary
    assert_eq!(
        enc_text(0b1000, KeyCode::Char('a'), Modifiers::SHIFT, Some("A")),
        b"\x1b[97;2u"
    );
    // capslock guard: text 'A' == physical 'a'.to_uppercase() ⇒ physical 97
    assert_eq!(
        enc_text(0b1000, KeyCode::Char('a'), Modifiers::empty(), Some("A")),
        b"\x1b[97u",
        "capslock must not answer CSI 65 (spec)"
    );
    // L5 with capslock: primary 97, cps carries the real text 'A'
    assert_eq!(
        enc_text(0b1_1001, KeyCode::Char('a'), Modifiers::empty(), Some("A")),
        b"\x1b[97;;65u"
    );
}

// ── T3 (PLAN_v1160 §5): Flag 4 shifted sub-segment ───────────────

/// Positive vectors (reviewer P1-1 corrected matrix): shift+a / shift+1
/// at 0b1101 and 0b1111; the full 0b1_1111 ladder adds the cps slot.
#[test]
fn t3_shifted_subsegment_positive() {
    for flags in [0b1101_u8, 0b1111] {
        assert_eq!(
            encode_kitty_key_shifted(
                flags,
                KittyEventKind::Press,
                KeyCode::Char('a'),
                Modifiers::SHIFT,
                Some("A"),
                Some('A')
            ),
            Some(b"\x1b[97:65;2u".to_vec()),
            "0b{flags:b}"
        );
    }
    assert_eq!(
        encode_kitty_key_shifted(
            0b1101,
            KittyEventKind::Press,
            KeyCode::Char('1'),
            Modifiers::SHIFT,
            Some("!"),
            Some('!')
        ),
        Some(b"\x1b[49:33;2u".to_vec())
    );
    assert_eq!(
        encode_kitty_key_shifted(
            0b1_1111,
            KittyEventKind::Press,
            KeyCode::Char('a'),
            Modifiers::SHIFT,
            Some("A"),
            Some('A')
        ),
        Some(b"\x1b[97:65;2;65u".to_vec()),
        "L5: shifted sub-segment + cps text slot"
    );
}

/// Negative matrix: no Flag 4 bit / no Shift / no shifted knowledge /
/// non-ASCII text / shifted == primary — the segment never fires.
#[test]
fn t3_shifted_subsegment_negative() {
    // no 0b100 in flags ⇒ no segment
    assert_eq!(
        encode_kitty_key_shifted(
            0b1000,
            KittyEventKind::Press,
            KeyCode::Char('a'),
            Modifiers::SHIFT,
            Some("A"),
            Some('A')
        ),
        Some(b"\x1b[97;2u".to_vec())
    );
    // no Shift in mods ⇒ no segment (even with shifted knowledge)
    assert_eq!(
        encode_kitty_key_shifted(
            0b1101,
            KittyEventKind::Press,
            KeyCode::Char('a'),
            Modifiers::empty(),
            Some("a"),
            Some('A')
        ),
        Some(b"\x1b[97u".to_vec())
    );
    // no shifted knowledge (synthetic path) ⇒ no segment
    assert_eq!(
        encode_kitty_key_shifted(
            0b1101,
            KittyEventKind::Press,
            KeyCode::Char('a'),
            Modifiers::SHIFT,
            None,
            None
        ),
        Some(b"\x1b[97;2u".to_vec())
    );
    // non-ASCII shifted char ⇒ omitted (宁缺勿错: the primary is
    // untrustworthy outside ASCII, alternates would amplify the error)
    assert_eq!(
        encode_kitty_key_shifted(
            0b1101,
            KittyEventKind::Press,
            KeyCode::Char('a'),
            Modifiers::SHIFT,
            Some("Ä"),
            Some('Ä')
        ),
        Some(b"\x1b[97;2u".to_vec())
    );
    // shifted == primary ⇒ omitted
    assert_eq!(
        encode_kitty_key_shifted(
            0b1101,
            KittyEventKind::Press,
            KeyCode::Char('a'),
            Modifiers::SHIFT,
            Some("a"),
            Some('a')
        ),
        Some(b"\x1b[97;2u".to_vec())
    );
}

/// 0b100 alone never promotes a text key (the v1.11.4 pinned semantics,
/// kitty_tests `l5_implies_l4_forms_for_textless_keys` family): 0b100 and
/// 0b101 with shift+a still deliver raw text.
#[test]
fn t3_alternates_alone_never_promote() {
    assert_eq!(
        encode_kitty_key_shifted(
            0b100,
            KittyEventKind::Press,
            KeyCode::Char('a'),
            Modifiers::SHIFT,
            Some("A"),
            Some('A')
        ),
        None,
        "0b100 alone: no level ⇒ not claimed"
    );
    assert_eq!(
        enc_hs_text(
            0b100,
            KeyCode::Char('a'),
            Modifiers::SHIFT,
            Some("A"),
            Some('A')
        ),
        b"A",
        "handler: raw text 'A'"
    );
    assert_eq!(
        enc_hs_text(
            0b101,
            KeyCode::Char('a'),
            Modifiers::SHIFT,
            Some("A"),
            Some('A')
        ),
        b"A",
        "0b101: L1 raw-text row, no promotion"
    );
    assert_eq!(
        enc_hs_text(
            0b101,
            KeyCode::Char('1'),
            Modifiers::SHIFT,
            Some("!"),
            Some('!')
        ),
        b"!"
    );
}

/// Handler e2e: the kitty_shifted field flows (app fill point →
/// encode_key_text → `:sk` segment), kitty_event_kind precedent.
#[test]
fn t3_handler_field_e2e() {
    assert_eq!(
        enc_hs_text(
            0b1_1111,
            KeyCode::Char('a'),
            Modifiers::SHIFT,
            Some("A"),
            Some('A')
        ),
        b"\x1b[97:65;2;65u"
    );
    // field None ⇒ no segment, everything else unchanged
    assert_eq!(
        enc_hs_text(
            0b1_1111,
            KeyCode::Char('a'),
            Modifiers::SHIFT,
            Some("A"),
            None
        ),
        b"\x1b[97;2;65u"
    );
    // the interrupted invariant holds under the shifted segment too
    let mut h = InputHandler::new();
    h.kitty_flags = 0b1_1111;
    h.kitty_shifted = Some('C');
    assert_eq!(
        h.encode_key_text(KeyCode::Char('c'), Modifiers::CONTROL, Some("c")),
        b"\x03",
        "ctrl+c stays C0 (Weft deviation) even with Flag 4 knowledge"
    );
}
