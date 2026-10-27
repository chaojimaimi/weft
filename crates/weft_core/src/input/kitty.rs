//! v1.11.4 (PLAN_v1114 §2): kitty keyboard-protocol key encoder.
//!
//! Decision source: docs/PLAN_v1114_KITTY_KEYBOARD.md v2 decision table
//! (§2.3) — the ONLY contract; anything this file emits is pinned there.
//! Deliberate divergences from the kitty spec (module-level) and their
//! row choices:
//!
//! - **Level model**: L1 = 0b1 Disambiguate, L2 = +0b10 event types,
//!   L4 = +0b1000 all keys, L5 = +0b10000 associated text. The effective
//!   level is the TOP set bit; each row's highest applicable column wins
//!   (e.g. L5 printables carry the `;cps` text segment, L4 not).
//! - **Ctrl+letter stays C0 at every level** — the kitty branch returns
//!   None and the legacy C0 byte wins (Weft explicit deviation: the
//!   InterruptPty `[0x03]` chain). `ctrl_char_for` (handler.rs) is the
//!   shared C0 table.
//! - **v1.16.0 (PLAN_v1160)**: Flag 8 ReportAllKeysAsEscapeCodes is
//!   UNMASKED (the v1.11.4 Blocker-2 downgrade clause is discharged by
//!   spec-table verification + golden pins — `kitty_v1160_tests.rs`), and
//!   Flag 4 ReportAlternateKeys emits the SHIFTED sub-segment `key:shifted`
//!   only (no base-layout slot — registry, PLAN_v1160 §6). Keypad digits
//!   ride the dedicated codes 57399-57408, gated on the FLAG_DISAMBIGUATE
//!   bit OR FLAG_REPORT_ALL_KEYS (the spec: Flag 8 "implies all keys are
//!   automatically disambiguated as well"; 0b10/0b10000 alone claim nothing):
//!   without either bit the key is not claimed and the legacy plain digit
//!   byte stands.
//! - **Enter/Tab/Backspace carry NO :2/:3 events without Flag 8** (:3 is
//!   the spec's own sentence — "do not have release events unless
//!   ReportAllKeysAsEscapeCodes"; the :2 extension is a conservative Weft
//!   divergence, pinned in the kitty_keyboard.rs doc). With Flag 8 the
//!   release materializes; the app-side gate drops the event without it.
//! - **Primary key code layout source (PLAN_v1160 §4)**: the unmodified
//!   L4+ printable primary is the layout text's first char (caps-lock
//!   guarded) per spec ("must not be CSI 65"); MODIFIED arms keep the
//!   physical base char — `Ctrl+;` must answer kc 59 regardless of layout —
//!   and the Shift path keeps the physical primary + the Flag 4 shifted
//!   segment (`primary_key_code`).
//! - **Arrows keep the compatibility tail form** `CSI 1;<m>[:N]ABCD`
//!   (plan row parenthetical + kitty spec: arrows are `CSI 1 letter`
//!   family; the `1` and modifier segment are omitted when there is
//!   nothing to report, so a plain arrow press stays `CSI A` even at L4 —
//!   identical to the legacy byte and to kitty itself).
//! - **Plain Enter/Tab/Backspace never carry event sub-segments** — the
//!   table's `保持（无 release，保 reset\n 通道）` note applies at every
//!   level (kitty would report release/Enter differently; Weft keeps the
//!   shell's blank-Enter channel).
//! - **L2 events only materialize on keys whose form already has modifiers
//!   under the table** (Esc, Ctrl/Alt/Super-modified keys, L4 family/keys):
//!   plain arrows and plain printables do not repeat-report at L2 (their
//!   L2 columns are 原文 / 不变).
//! - **L4 pure-modifier key presses are dropped** (explicit deviation) —
//!   handled upstream (map_winit_key returns None for modifier keys), so
//!   the encoder never sees them.
//! - **kc** comes from the BASE key char (`map_winit_key`'s abstract key),
//!   NOT the layout text — `Ctrl+;` must answer kc 59 regardless of
//!   layout; the text is only the L5 `;cps` segment (and the L1 raw bytes).
//!
//! Byte shapes (kitty spec / plan §2.3):
//! - `CSI <kc> u` — no modifiers, no event, no text.
//! - `CSI <kc>;<m> u` — modifiers (`m` = bit-or of shift1/alt2/ctrl4/
//!   super8, plus 1; empty only when no modifier at all).
//! - `CSI <kc>;<m>:<N> u` — event sub-segment (`:2` repeat, `:3` release),
//!   `;1:N` materialized when modifiers are empty.
//! - `CSI <kc>;<m>;<cps> u` — L5 associated text (colon-joined Unicode
//!   code points), `;;` for empty modifiers.

use super::handler::InputHandler;
use super::keys::{KeyCode, Modifiers};
pub use crate::vt::kitty_keyboard::{
    FLAG_DISAMBIGUATE, FLAG_REPORT_ALL_KEYS, FLAG_REPORT_ALTERNATE, FLAG_REPORT_ASSOCIATED_TEXT,
    FLAG_REPORT_EVENT_TYPES,
};

/// The class of the winit key event that reached the encoder (v1.11.4 L2
/// pipeline). Only materialized as `:N` when the app negotiated 0b10.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum KittyEventKind {
    #[default]
    Press,
    Repeat,
    Release,
}

/// Dedicated keypad key codes (kitty spec §7, effective from the
/// Disambiguate flag): KP_0-9 = 57399-57408. Domain: `'0'..='9'` (the
/// `KeyCode::Numpad` contract, keys.rs) — KP operators/, /Enter are not in
/// the enum surface and are never mapped (PLAN_v1160 §6 registry).
fn numpad_code(c: char) -> u32 {
    57399 + (c as u32 - '0' as u32)
}

/// Convert a base key char to its kitty key code (Unicode code point).
fn key_code(c: char) -> u32 {
    if c.is_control() {
        // Defensive: a control char in the base slot has no key code —
        // kitty uses 0 for keys with no code, and the text segment (L5)
        // carries the real payload.
        0
    } else {
        c as u32
    }
}

/// L5 associated-text segment: colon-joined code points of the layout
/// text; falls back to the base char when no text arrived.
fn text_cps(c: char, text: Option<&str>) -> String {
    let chars = text
        .filter(|t| !t.is_empty())
        .map(|t| t.chars().collect::<Vec<_>>())
        .unwrap_or_else(|| vec![c]);
    let mut out = String::with_capacity(chars.len() * 2);
    for (i, ch) in chars.iter().enumerate() {
        if i > 0 {
            out.push(':');
        }
        out.push_str(&(*ch as u32).to_string());
    }
    out
}

/// Modifier value for the kitty segment: (shift?1 | alt?2 | ctrl?4 |
/// super?8) + 1. Unlike the legacy `modifier_code`, SUPER is represented.
fn mod_value(mods: Modifiers) -> u8 {
    1 + mods.contains(Modifiers::SHIFT) as u8
        + mods.contains(Modifiers::ALT) as u8 * 2
        + mods.contains(Modifiers::CONTROL) as u8 * 4
        + mods.contains(Modifiers::SUPER) as u8 * 8
}

/// `:N` event sub-segment — only when the app negotiated event reporting.
fn event_sub(flags: u8, kind: KittyEventKind) -> &'static str {
    if flags & FLAG_REPORT_EVENT_TYPES == 0 {
        return "";
    }
    match kind {
        KittyEventKind::Press => "",
        KittyEventKind::Repeat => ":2",
        KittyEventKind::Release => ":3",
    }
}

/// Build `CSI <body> u` (or `CSI <body> <tail>` for the arrow compat form).
fn csi_u(body: &str, tail: u8) -> Vec<u8> {
    let mut buf = Vec::with_capacity(body.len() + 5);
    buf.extend_from_slice(b"\x1b[");
    buf.extend_from_slice(body.as_bytes());
    buf.push(tail);
    buf
}

/// Assemble the modifiers segment: `;<m>` — or `;<m>:<N>` with an event.
fn mods_segment(m: u8, sub: &str) -> String {
    format!(";{m}{sub}")
}

/// Functional key numbers (plan §2.3): Esc27 Enter13 Tab9 BS127,
/// 方向1-4 (Up1 Down2 Right3 Left4 — the compat-form arrow params),
/// Ins2 Del3 PgUp5 PgDn6 Home7 End8, F:11,12,13,14,15,17-21,23,24.
fn function_code(key: KeyCode) -> Option<u32> {
    Some(match key {
        KeyCode::Enter => 13,
        KeyCode::Backspace => 127,
        KeyCode::Tab => 9,
        KeyCode::Escape => 27,
        KeyCode::Up => 1,
        KeyCode::Down => 2,
        KeyCode::Right => 3,
        KeyCode::Left => 4,
        KeyCode::Insert => 2,
        KeyCode::Delete => 3,
        KeyCode::PageUp => 5,
        KeyCode::PageDown => 6,
        KeyCode::Home => 7,
        KeyCode::End => 8,
        KeyCode::F(1) => 11,
        KeyCode::F(2) => 12,
        KeyCode::F(3) => 13,
        KeyCode::F(4) => 14,
        KeyCode::F(5) => 15,
        KeyCode::F(6) => 17,
        KeyCode::F(7) => 18,
        KeyCode::F(8) => 19,
        KeyCode::F(9) => 20,
        KeyCode::F(10) => 21,
        KeyCode::F(11) => 23,
        KeyCode::F(12) => 24,
        _ => return None,
    })
}

/// Tail letters for the arrow compat forms (VT order: A=up B=down
/// C=right D=left).
fn arrow_tail(key: KeyCode) -> u8 {
    match key {
        KeyCode::Up => b'A',
        KeyCode::Down => b'B',
        KeyCode::Right => b'C',
        KeyCode::Left => b'D',
        _ => unreachable!("arrow_tail called for a non-arrow"),
    }
}

/// Encode one key event under the negotiated kitty flags.
///
/// Returns `None` when the key must fall through to the legacy encoders —
/// the invariant that keeps `flags == 0` (and every row's L1/L2 legacy
/// delegation) byte-identical to pre-v1.11.4. Convenience form with no
/// Flag 4 shifted knowledge (`shifted = None`).
pub fn encode_kitty_key(
    flags: u8,
    kind: KittyEventKind,
    key: KeyCode,
    mods: Modifiers,
    text: Option<&str>,
) -> Option<Vec<u8>> {
    encode_kitty_key_shifted(flags, kind, key, mods, text, None)
}

/// v1.16.0 (PLAN_v1160 §5): full-form entry — `shifted` is the Flag 4
/// (ReportAlternateKeys) source, `InputHandler.kitty_shifted` (the layout
/// text's first char while Shift is down; `None` on synthetic paths).
/// Omitting the alternates sub-segment is always spec-legal (逐键可省略),
/// so `None` merely means "no shifted sub-segment".
pub fn encode_kitty_key_shifted(
    flags: u8,
    kind: KittyEventKind,
    key: KeyCode,
    mods: Modifiers,
    text: Option<&str>,
    shifted: Option<char>,
) -> Option<Vec<u8>> {
    // v1.11.13 (PLAN_v11113 §M3): defense in depth — a Release is only
    // reportable when 0b10 (ReportEventTypes) was negotiated. Orthogonal to
    // the escape-coding branch below (which owns the 0b10 release shape):
    // without 0b10 a Release must fall through (None ⇒ legacy never emits
    // it; the app-side gate drops the event, this guards the encoder seam).
    if kind == KittyEventKind::Release && flags & FLAG_REPORT_EVENT_TYPES == 0 {
        return None;
    }
    let level = if flags & FLAG_REPORT_ALL_KEYS != 0 {
        // 0b1000 gates the all-keys tier: plain printables flip to escape
        // codes ONLY under this flag (spec: ReportAssociatedText attaches
        // code points to keys that are already escape-coded, it does not
        // promote text keys on its own — flags are orthogonal bits, the
        // old ordered-ladder mis-encoded 0b10000-alone as L4/L5).
        if flags & FLAG_REPORT_ASSOCIATED_TEXT != 0 {
            5
        } else {
            4
        }
    } else if flags & FLAG_REPORT_EVENT_TYPES != 0 {
        2
    } else if flags & FLAG_DISAMBIGUATE != 0 {
        1
    } else {
        return None;
    };
    // kitty spec, ReportEventTypes: with 0b10 negotiated a key RELEASE is
    // ALWAYS reported as an escape code — raw text and legacy C0 bytes are
    // forbidden on this path. A released Ctrl+C must not re-fire
    // InterruptPty and a released Enter must not double-commit
    // (rust-reviewer v1.11.4 Blocker-1). Note the C0 deviation applies to
    // Press/Repeat only; ctrl+letter releases CSI-ify like everything else.
    // Without 0b10 releases are unreportable → fall through (None ⇒ the
    // app-side gate drops them before any legacy emission).
    if kind == KittyEventKind::Release && flags & FLAG_REPORT_EVENT_TYPES != 0 {
        // kitty spec, ReportEventTypes: "Enter, Tab and Backspace keys do
        // not have release events" unless ReportAllKeysAsEscapeCodes (Flag
        // 8) is set — the gate is on the Flag 8 bit even though 0b10 is
        // negotiated (PLAN_v1160 §4, :3 is the spec's own sentence). None
        // ⇒ the app-side gate drops the event (never legacy `\r`/`\t`/
        // `\x7f` bytes); this None only guards the encoder seam.
        if matches!(key, KeyCode::Enter | KeyCode::Tab | KeyCode::Backspace)
            && flags & FLAG_REPORT_ALL_KEYS == 0
        {
            return None;
        }
        let seg = mods_segment(mod_value(mods), ":3");
        return Some(match key {
            KeyCode::Escape => csi_u(&format!("27{seg}"), b'u'),
            KeyCode::Enter => csi_u(&format!("13{seg}"), b'u'),
            KeyCode::Backspace => csi_u(&format!("127{seg}"), b'u'),
            KeyCode::Tab => csi_u(&format!("9{seg}"), b'u'),
            KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right => {
                csi_u(&format!("1{seg}"), arrow_tail(key))
            }
            KeyCode::Char(c) => csi_u(&format!("{}{seg}", key_code(c)), b'u'),
            // v1.16.0 (PLAN_v1160 §3): the release arm rides the KP
            // dedicated code when Disambiguate OR Flag 8 (implies it) is set;
            // without either bit the dedicated code must NOT materialize and the
            // digit's code point stands in — releases stay escape-coded
            // either way (never legacy bytes, Blocker-1).
            KeyCode::Numpad(c) => {
                let kc = if flags & (FLAG_DISAMBIGUATE | FLAG_REPORT_ALL_KEYS) != 0 {
                    numpad_code(c)
                } else {
                    key_code(c)
                };
                csi_u(&format!("{kc}{seg}"), b'u')
            }
            other => {
                let (param, final_byte) = legacy_fn_shape(other);
                csi_u(&format!("{param}{seg}"), final_byte)
            }
        });
    }
    let sub = event_sub(flags, kind);
    // v1.16.0 (PLAN_v1160 §4): the same Flag 8 gate extends conservatively
    // to :2 REPEATS of the three keys (explicit Weft divergence — the spec
    // sentence only names releases; reviewer P3 ruling): below L4 the press
    // form stands in (`CSI 13;5u`), under it the event materializes.
    let sub = if flags & FLAG_REPORT_ALL_KEYS == 0
        && matches!(key, KeyCode::Enter | KeyCode::Tab | KeyCode::Backspace)
    {
        ""
    } else {
        sub
    };

    match key {
        // ── Enter / Tab / Backspace family ───────────────────────────
        KeyCode::Enter | KeyCode::Backspace | KeyCode::Tab => {
            enter_tab_bs_row(level, key, mods, sub)
        }
        // ── Escape (M2: CSI 27u from L1 on) ───────────────────────────
        KeyCode::Escape => {
            let m = mod_value(mods);
            let body = if m == 1 && sub.is_empty() {
                "27".to_string()
            } else {
                format!("27{}", mods_segment(m, sub))
            };
            Some(csi_u(&body, b'u'))
        }
        // ── Arrows / Home / End / PgUp / PgDn / Ins / Del / F ────────
        KeyCode::Up
        | KeyCode::Down
        | KeyCode::Left
        | KeyCode::Right
        | KeyCode::Home
        | KeyCode::End
        | KeyCode::PageUp
        | KeyCode::PageDown
        | KeyCode::Delete
        | KeyCode::Insert
        | KeyCode::F(_) => navigation_row(level, key, mods, sub),
        // ── Printable keys ────────────────────────────────────────────
        KeyCode::Char(c) => printable_row(flags, level, c, mods, text, sub, shifted),
        // ── Keypad digits (kitty spec §7 dedicated codes) ────────────
        KeyCode::Numpad(c) => numpad_row(flags, level, c, mods, text, sub),
    }
}

/// Enter/Tab/Backspace decision rows (§2.3): Ctrl and Alt rows report from
/// L1 (`CSI 13;5u` / `CSI 13;3u`), plain keys stay legacy until L4
/// (`\r`/`\t`/`\x7f`), and plain Enter/Tab/BS never carry event
/// sub-segments (the `reset\n` channel row note).
fn enter_tab_bs_row(level: u8, key: KeyCode, mods: Modifiers, sub: &str) -> Option<Vec<u8>> {
    let code = function_code(key).expect("enter/tab/bs has a code");
    let c = match key {
        KeyCode::Enter => '\r',
        KeyCode::Backspace => '\u{7f}',
        _ => '\t',
    };
    if mods.contains(Modifiers::CONTROL) {
        // Ctrl+Enter/Tab/BS ⇒ `CSI 13;5u` / `CSI 9;5u` / `CSI 127;5u` —
        // same bytes at every level (ctrl row's 同左 columns).
        let body = format!("{code}{}", mods_segment(mod_value(mods), sub));
        return Some(csi_u(&body, b'u'));
    }
    if mods.contains(Modifiers::ALT) {
        // Alt+Enter/Tab/BS: `CSI 13;3u` (+event); L5 adds `;13` cps only
        // for Enter (`CSI 13;3;\r u` row).
        let cps = if level >= 5 && key == KeyCode::Enter {
            format!(";{}", text_cps(c, Some("\r")))
        } else {
            String::new()
        };
        let body = format!("{code}{}{cps}", mods_segment(mod_value(mods), sub));
        return Some(csi_u(&body, b'u'));
    }
    // Shift+Tab is its own row: legacy `CSI Z` through L2, `CSI 9;2u`
    // from L4.
    if key == KeyCode::Tab && mods == Modifiers::SHIFT {
        if level < 4 {
            return None;
        }
        let body = format!("9{}", mods_segment(2, sub));
        return Some(csi_u(&body, b'u'));
    }
    if mods.is_empty() {
        // 无修饰: L1/L2 stay exactly `\r`/`\t`/`\x7f` (no event reporting
        // ever — the row note), L4+ → `CSI 13/9/127 u`, Enter adds its
        // cps at L5.
        if level < 4 {
            return None;
        }
        let cps = if level >= 5 && key == KeyCode::Enter {
            format!(";;{}", text_cps(c, Some("\r")))
        } else {
            String::new()
        };
        let body = format!("{code}{cps}");
        return Some(csi_u(&body, b'u'));
    }
    // Other modified combinations (e.g. Shift+Enter, Shift+Backspace):
    // generalized modified form from L1.
    let body = format!("{code}{}", mods_segment(mod_value(mods), sub));
    Some(csi_u(&body, b'u'))
}

/// Arrows / Home / End / PgUp / PgDn / Ins / Del / F decision rows:
/// unmodified keys stay legacy through L2; modified keys keep the legacy
/// `CSI 1;5C` / `CSI 15;5~` shapes at L1 (plus `:N` at L2) and at L4 the
/// family keeps its spec terminator (`~` / `H` / `F` / `P..S` — reviewer
/// Blocker-2). Arrows ALWAYS use the compat `CSI 1;<m>[ABCD]` tail (kitty
/// spec: arrows are the CSI-1 letter family, params omitted when bare).
fn navigation_row(level: u8, key: KeyCode, mods: Modifiers, sub: &str) -> Option<Vec<u8>> {
    let is_arrow = matches!(
        key,
        KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right
    );
    // Super row (§2.3): unbound Cmd chords report escape-coded from L1 —
    // legacy has no super representation (super+Up was a plain `CSI A`).
    if mods.contains(Modifiers::SUPER) {
        if is_arrow {
            // Compat tail form: `CSI 1;9A` — kitty's own shape for Cmd+arrow.
            let body = format!("1{}", mods_segment(mod_value(mods), sub));
            return Some(csi_u(&body, arrow_tail(key)));
        }
        let (param, final_byte) = legacy_fn_shape(key);
        let body = format!("{param}{}", mods_segment(mod_value(mods), sub));
        return Some(csi_u(&body, final_byte));
    }
    let has_legacy_mods = mods.intersects(Modifiers::SHIFT | Modifiers::ALT | Modifiers::CONTROL);
    if is_arrow {
        if has_legacy_mods {
            if level < 4 {
                // L1: the legacy `CSI 1;5C` IS the kitty L1 shape; L2 adds
                // `:N` onto it; only at L4 does the form matter again (it
                // does not change — compat tail with the same bytes).
                if level < 2 || sub.is_empty() {
                    return None;
                }
                let body = format!("1{}", mods_segment(mod_value(mods), sub));
                return Some(csi_u(&body, arrow_tail(key)));
            }
            let body = format!("1{}", mods_segment(mod_value(mods), sub));
            return Some(csi_u(&body, arrow_tail(key)));
        }
        // Unmodified arrow: legacy `CSI A` until an event must be
        // reported; from L4 a repeat/release materializes `CSI 1;1:NA`.
        if level < 4 || sub.is_empty() {
            return None;
        }
        let body = format!("1{}", mods_segment(1, sub));
        return Some(csi_u(&body, arrow_tail(key)));
    }
    // ── Non-arrow functional family (Home/End/PgUp/PgDn/Ins/Del/F) ──
    // Spec terminators: F1-F4 = `CSI 1;<m>P..S` (or 11~..14~; F3 has NO R
    // form — spec deleted it, `CSI 13~` only), the rest =
    // `CSI <7/8/5/6/2/3>;<m>~`. The `u` terminator is reserved for the four
    // dedicated codes (Esc/Enter/Tab/BS) — rust-reviewer v1.11.4 Blocker-2
    // (v1.16.0: the L4 rows are live — unmasked and spec-table verified).
    if has_legacy_mods {
        if mods.contains(Modifiers::CONTROL) && !is_arrow {
            // The Ctrl row (§2.3) explicitly lists F keys — spec shape keeps
            // the family terminator (`CSI 11;5~` / `CSI 1;5H`-style).
            let (param, final_byte) = legacy_fn_shape(key);
            let body = format!("{param}{}", mods_segment(mod_value(mods), sub));
            return Some(csi_u(&body, final_byte));
        }
        if level < 4 {
            if level < 2 || sub.is_empty() {
                return None;
            }
            // L2: legacy shape + `:N` — `CSI 17;5:2~` / `CSI 1;2:2P`.
            let (param, final_byte) = legacy_fn_shape(key);
            let body = format!("{param}{}", mods_segment(mod_value(mods), sub));
            return Some(csi_u(&body, final_byte));
        }
        let (param, final_byte) = legacy_fn_shape(key);
        let body = format!("{param}{}", mods_segment(mod_value(mods), sub));
        return Some(csi_u(&body, final_byte));
    }
    // Unmodified family keys.
    if level < 4 {
        return None;
    }
    if sub.is_empty() {
        let (param, final_byte) = legacy_fn_shape(key);
        return Some(csi_u(&param, final_byte));
    }
    let (param, final_byte) = legacy_fn_shape(key);
    let body = format!("{param}{}", mods_segment(1, sub));
    Some(csi_u(&body, final_byte))
}

/// Legacy `CSI <param><final>` shape of a function/navigation key for the
/// L2 `+;:N` rows (F1-2/4: `CSI 1;m<PQS>`; F5-10/11-12: `CSI <code>;m~`;
/// Home/End/PgUp/PgDn/Ins/Del: `CSI <code>;m~`). F3 has NO `CSI 1R` form —
/// the kitty spec deleted it (conflicts with the CPR cursor-position
/// reply), so F3 rides the tilde form `CSI 13~` at every kitty level
/// (PLAN_v1160 §4; the pure-legacy stream keeps `ESC O R`/`CSI 1;5R`).
fn legacy_fn_shape(key: KeyCode) -> (String, u8) {
    match key {
        KeyCode::F(1) => ("1".to_string(), b'P'),
        KeyCode::F(2) => ("1".to_string(), b'Q'),
        KeyCode::F(4) => ("1".to_string(), b'S'),
        _ => (
            function_code(key).expect("nav key has a code").to_string(),
            b'~',
        ),
    }
}

/// Shared `CSI <kc><alts>[;<m>[:N]][;<cps>] u` body for keys whose form is
/// the text-API shape: printables from L4 (plus the alt arm from L1), KP
/// digits under Disambiguate. L5 (all-keys ∧ associated-text) attaches the
/// `;<cps>` segment; `alts` carries the Flag 4 `:<shifted>` sub-segment
/// (empty = omitted — always spec-legal).
fn u_form_body(
    kc: u32,
    alts: &str,
    level: u8,
    c: char,
    mods: Modifiers,
    text: Option<&str>,
    sub: &str,
) -> Vec<u8> {
    if level >= 5 {
        let cps = text_cps(c, text);
        if mods.is_empty() && sub.is_empty() {
            return csi_u(&format!("{kc}{alts};;{cps}"), b'u');
        }
        if mods.is_empty() {
            return csi_u(&format!("{kc}{alts}{};{cps}", mods_segment(1, sub)), b'u');
        }
        return csi_u(
            &format!("{kc}{alts}{};{cps}", mods_segment(mod_value(mods), sub)),
            b'u',
        );
    }
    if mods.is_empty() && sub.is_empty() {
        return csi_u(&format!("{kc}{alts}"), b'u');
    }
    if mods.is_empty() {
        return csi_u(&format!("{kc}{alts}{}", mods_segment(1, sub)), b'u');
    }
    csi_u(
        &format!("{kc}{alts}{}", mods_segment(mod_value(mods), sub)),
        b'u',
    )
}

/// v1.16.0 (PLAN_v1160 §4 step 2): the primary key code for the unmodified
/// L4+ rows — kitty spec: the key code is the CURRENT LAYOUT's unshifted
/// code point ("must not be CSI 65" for shift+a). winit exposes the layout
/// only as `text`; use its first char when Shift is up, falling back to the
/// US-physical table. CapsLock guard: text being exactly the uppercased
/// physical char is the caps artifact ('A' for KeyA), not the layout's
/// unshifted char — the physical table wins there too (spec: must not be
/// CSI 65). ASCII layouts are zero-change (text == physical table);
/// non-ASCII layouts go from wrong to right.
fn primary_key_code(c: char, mods: Modifiers, text: Option<&str>) -> u32 {
    let layout_char = if mods.contains(Modifiers::SHIFT) {
        // Shift is down: text is the SHIFTED char — never the unshifted
        // primary; the physical table + the Flag 4 shifted segment own it.
        None
    } else {
        text.and_then(|t| t.chars().next())
            .filter(|t| !t.is_control() && Some(*t) != c.to_uppercase().next())
    };
    key_code(layout_char.unwrap_or(c))
}

/// Flag 4 sub-segment builder: `:<codepoint>` of the shifted char (kitty
/// spec: alternates are colon-joined sub-segments after the primary key;
/// the base-layout slot `key::base` is not implemented — PLAN_v1160 §6
/// registry). Empty when there is no shifted knowledge.
fn alternates_segment(shifted: Option<char>) -> String {
    match shifted {
        Some(s) => {
            let cp = s as u32;
            format!(":{cp}")
        }
        None => String::new(),
    }
}

/// v1.16.0 (PLAN_v1160 §5 step 1, verbatim chain): the Flag 4 shifted
/// sub-segment source — sent only when FLAG_REPORT_ALTERNATE is negotiated
/// ∧ Shift is in the modifiers ∧ the event is already CSI u via other
/// flags (structural: every caller is an escape-coded arm — 0b100 alone
/// never reaches here, the level ladder leaves plain printables on the raw
/// text path, pinned since v1.11.4) ∧ the shifted char differs from the
/// primary ∧ the shifted char is ASCII (outside ASCII the primary itself
/// is untrustworthy — omitting is strictly no worse and spec-legal).
fn shifted_segment(flags: u8, mods: Modifiers, primary: char, shifted: Option<char>) -> String {
    if flags & FLAG_REPORT_ALTERNATE == 0
        || !mods.contains(Modifiers::SHIFT)
        || !shifted.is_some_and(|s| s != primary && s.is_ascii())
    {
        return String::new();
    }
    alternates_segment(shifted)
}

/// KP_0-9 decision row (v1.16.0 PLAN_v1160 §3, kitty spec §7): the
/// dedicated codes 57399-57408 take effect from the DISAMBIGUATE FLAG —
/// gated on the bit, not the negotiated level (0b10 / 0b10000 alone must
/// not promote the keypad). Without the bit the key is NOT claimed ⇒ the
/// handler's legacy plain digit byte (the revived dead key's L1 form).
/// Shape: `CSI 57399+n[;<m>[:N]][;<cps>] u`.
fn numpad_row(
    flags: u8,
    level: u8,
    c: char,
    mods: Modifiers,
    text: Option<&str>,
    sub: &str,
) -> Option<Vec<u8>> {
    // rust-reviewer v1.16.0 P1: Flag 8 implies disambiguation (kitty spec:
    // "this enhancement implies all keys are automatically disambiguated as
    // well"), so 0b1000 alone must also claim the dedicated keypad codes —
    // 0b10/0b10000 alone still do not.
    if flags & (FLAG_DISAMBIGUATE | FLAG_REPORT_ALL_KEYS) == 0 {
        return None;
    }
    Some(u_form_body(numpad_code(c), "", level, c, mods, text, sub))
}

/// Printable-key decision rows (§2.3): L1/L2 raw UTF-8 (layout text wins),
/// L4+ `CSI kc[;m[:e]][;cps] u`. Ctrl+letter → None (C0 stays at every
/// level). v1.16.0 (PLAN_v1160 §4/§5): the unmodified L4+ primary comes
/// from the LAYOUT (`primary_key_code`), and the Flag 4 shifted
/// sub-segment rides every escape-coded printable arm whose condition
/// chain holds. kc on the modified arms stays the physical base char
/// (module doc: `Ctrl+;` must answer 59 regardless of layout).
fn printable_row(
    flags: u8,
    level: u8,
    c: char,
    mods: Modifiers,
    text: Option<&str>,
    sub: &str,
    shifted: Option<char>,
) -> Option<Vec<u8>> {
    let alts = shifted_segment(flags, mods, c, shifted);
    if mods.contains(Modifiers::CONTROL) {
        // Ctrl+letter (C0-mappable) → legacy C0 byte at every level —
        // Weft explicit deviation (module doc). Ctrl+; / Ctrl+数字 / etc.
        // (no C0 map) → `CSI kc;5u` from L1.
        if InputHandler::ctrl_char_for(c).is_some() {
            return None;
        }
        let body = format!(
            "{}{alts}{}",
            key_code(c),
            mods_segment(mod_value(mods), sub)
        );
        return Some(csi_u(&body, b'u'));
    }
    if mods.contains(Modifiers::SUPER) {
        // Super row: `CSI kc;9u` (+event) — unbound Cmd chords.
        let body = format!(
            "{}{alts}{}",
            key_code(c),
            mods_segment(mod_value(mods), sub)
        );
        return Some(csi_u(&body, b'u'));
    }
    if mods.contains(Modifiers::ALT) {
        // Alt+printable: `CSI kc;3u` (NO ESC prefix) from L1; L5 add cps.
        return Some(u_form_body(key_code(c), &alts, level, c, mods, text, sub));
    }
    // No ctrl/alt/super (mods ⊆ {SHIFT} here).
    if level < 4 {
        // 原文: with layout text, hand it through unchanged; without text
        // (synthetic paths), fall to legacy so shift mapping stays in one
        // place.
        if let Some(t) = text.filter(|t| !t.is_empty()) {
            return Some(t.as_bytes().to_vec());
        }
        return None;
    }
    Some(u_form_body(
        primary_key_code(c, mods, text),
        &alts,
        level,
        c,
        mods,
        text,
        sub,
    ))
}

#[cfg(test)]
#[path = "kitty_tests.rs"]
mod tests;
