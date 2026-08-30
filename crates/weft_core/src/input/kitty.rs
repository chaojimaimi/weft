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
//! - **0b100 ReportAlternateKeys is cut** — no `:sk` sub-segment anywhere
//!   (M5), even though the planner's L2 example bytes (`CSI 97:65…`) show
//!   one; the v2 table's `Shift+可打印` row (no `:sk`) wins.
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
    FLAG_DISAMBIGUATE, FLAG_REPORT_ALL_KEYS, FLAG_REPORT_ASSOCIATED_TEXT, FLAG_REPORT_EVENT_TYPES,
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
/// delegation) byte-identical to pre-v1.11.4.
pub fn encode_kitty_key(
    flags: u8,
    kind: KittyEventKind,
    key: KeyCode,
    mods: Modifiers,
    text: Option<&str>,
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
        let seg = mods_segment(mod_value(mods), ":3");
        return Some(match key {
            KeyCode::Escape => csi_u(&format!("27{seg}"), b'u'),
            KeyCode::Enter => csi_u(&format!("13{seg}"), b'u'),
            KeyCode::Backspace => csi_u(&format!("127{seg}"), b'u'),
            KeyCode::Tab => csi_u(&format!("9{seg}"), b'u'),
            KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right => {
                csi_u(&format!("1{seg}"), arrow_tail(key))
            }
            KeyCode::Char(c) | KeyCode::Numpad(c) => csi_u(&format!("{}{seg}", key_code(c)), b'u'),
            other => {
                let (param, final_byte) = legacy_fn_shape(other);
                csi_u(&format!("{param}{seg}"), final_byte)
            }
        });
    }
    let sub = event_sub(flags, kind);

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
        KeyCode::Char(c) | KeyCode::Numpad(c) => printable_row(level, c, mods, text, sub),
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
    // Spec terminators: F1-F4 = `CSI 1;<m>P..S` (or 11~..14~), the rest =
    // `CSI <7/8/5/6/2/3>;<m>~`. The `u` terminator is reserved for the four
    // dedicated codes (Esc/Enter/Tab/BS) — rust-reviewer v1.11.4 Blocker-2
    // (these L4 rows are unreachable while 0b1000 is masked, kept correct
    // for future unmasking).
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
/// L2 `+;:N` rows (F1-4: `CSI 1;m<PQRS>`; F5-10/11-12: `CSI <code>;m~`;
/// Home/End/PgUp/PgDn/Ins/Del: `CSI <code>;m~`).
fn legacy_fn_shape(key: KeyCode) -> (String, u8) {
    match key {
        KeyCode::F(1) => ("1".to_string(), b'P'),
        KeyCode::F(2) => ("1".to_string(), b'Q'),
        KeyCode::F(3) => ("1".to_string(), b'R'),
        KeyCode::F(4) => ("1".to_string(), b'S'),
        _ => (
            function_code(key).expect("nav key has a code").to_string(),
            b'~',
        ),
    }
}

/// Printable-key decision rows (§2.3): L1/L2 raw UTF-8 (layout text wins),
/// L4 `CSI kc[;m]u`, L5 `CSI kc[;m];cps u` (no `:sk` — M5). Ctrl+letter →
/// None (C0 stays at every level).
fn printable_row(
    level: u8,
    c: char,
    mods: Modifiers,
    text: Option<&str>,
    sub: &str,
) -> Option<Vec<u8>> {
    if mods.contains(Modifiers::CONTROL) {
        // Ctrl+letter (C0-mappable) → legacy C0 byte at every level —
        // Weft explicit deviation (module doc). Ctrl+; / Ctrl+数字 / etc.
        // (no C0 map) → `CSI kc;5u` from L1.
        if InputHandler::ctrl_char_for(c).is_some() {
            return None;
        }
        let body = format!("{}{}", key_code(c), mods_segment(mod_value(mods), sub));
        return Some(csi_u(&body, b'u'));
    }
    if mods.contains(Modifiers::SUPER) {
        // Super row: `CSI kc;9u` (+event) — unbound Cmd chords.
        let body = format!("{}{}", key_code(c), mods_segment(mod_value(mods), sub));
        return Some(csi_u(&body, b'u'));
    }
    if mods.contains(Modifiers::ALT) {
        // Alt+printable: `CSI kc;3u` (NO ESC prefix) from L1; L5 add cps.
        let cps = if level >= 5 {
            format!(";{}", text_cps(c, text))
        } else {
            String::new()
        };
        let body = format!("{}{}{cps}", key_code(c), mods_segment(mod_value(mods), sub));
        return Some(csi_u(&body, b'u'));
    }
    // No ctrl/alt/super.
    if level < 4 {
        // 原文: with layout text, hand it through unchanged; without text
        // (synthetic paths), fall to legacy so shift mapping stays in one
        // place.
        if let Some(t) = text.filter(|t| !t.is_empty()) {
            return Some(t.as_bytes().to_vec());
        }
        return None;
    }
    let kc = key_code(c);
    if level >= 5 {
        let cps = text_cps(c, text);
        // `CSI kc;;cps u` for unmodified (mods segment empty), event via
        // the materialized `;1:N` slot.
        if mods.is_empty() && sub.is_empty() {
            return Some(csi_u(&format!("{kc};;{cps}"), b'u'));
        }
        if mods.is_empty() {
            return Some(csi_u(&format!("{kc}{};{cps}", mods_segment(1, sub)), b'u'));
        }
        let body = format!("{kc}{};{cps}", mods_segment(mod_value(mods), sub));
        return Some(csi_u(&body, b'u'));
    }
    if mods.is_empty() && sub.is_empty() {
        return Some(csi_u(&kc.to_string(), b'u'));
    }
    if mods.is_empty() {
        return Some(csi_u(&format!("{kc}{}", mods_segment(1, sub)), b'u'));
    }
    let body = format!("{kc}{}", mods_segment(mod_value(mods), sub));
    Some(csi_u(&body, b'u'))
}

#[cfg(test)]
#[path = "kitty_tests.rs"]
mod tests;
