use super::*;

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
    // PgDn6~ Ins2~ Del3~ — reviewer Blocker-2. Live since v1.16.0 (Flag 8
    // unmasked, spec-table verified — PLAN_v1160 §4).
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
    // Spec terminators: F1/F2/F4 = 1P/1Q/1S, F3 = 13~ (the spec DELETED the
    // `CSI 1R` form — CPR conflict; PLAN_v1160 §4), F5+ = 15~/17~/.../24~
    // (reviewer Blocker-2 — `u` is reserved for Esc/Enter/Tab/BS).
    assert_eq!(enc(0b1000, KeyCode::F(1), Modifiers::empty()), b"\x1b[1P");
    assert_eq!(enc(0b1000, KeyCode::F(2), Modifiers::empty()), b"\x1b[1Q");
    assert_eq!(enc(0b1000, KeyCode::F(3), Modifiers::empty()), b"\x1b[13~");
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
fn numpad_dedicated_codes_gated_on_disambiguate() {
    // v1.16.0 (PLAN_v1160 §3, kitty spec §7): KP_0-9 = 57399-57408, gated
    // on the DISAMBIGUATE FLAG BIT — not the level (0b10 / 0b10000 alone
    // must not promote the keypad). Flag 8 ALSO claims the code (spec:
    // "this enhancement implies all keys are automatically disambiguated
    // as well" — rust-reviewer v1.16.0 P1). Without either bit the encoder
    // does not claim the key ⇒ the handler's legacy plain digit byte.
    assert_eq!(
        enc(0b1, KeyCode::Numpad('5'), Modifiers::empty()),
        b"\x1b[57404u"
    );
    assert_eq!(enc_h(0b0, KeyCode::Numpad('5'), Modifiers::empty()), b"5");
    assert_eq!(
        enc_h(0b1000, KeyCode::Numpad('5'), Modifiers::empty()),
        b"\x1b[57404u"
    );
    assert_eq!(
        enc_h(0b1_0000, KeyCode::Numpad('5'), Modifiers::empty()),
        b"5"
    );
    // legacy twin: flags=0 (and every delegation) stays the plain byte
    assert_eq!(
        enc_h(0b1, KeyCode::Numpad('5'), Modifiers::empty()),
        b"\x1b[57404u"
    );
}

// ── level masking / thresholds ───────────────────────────────────

#[test]
fn unsupported_flag_combinations_delegate_legacy() {
    // 0b100 (ReportAlternateKeys) has NO level of its own — alone it never
    // claims a text key (v1.16.0: alternates only ride events already CSI
    // via other flags, PLAN_v1160 §5)
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
