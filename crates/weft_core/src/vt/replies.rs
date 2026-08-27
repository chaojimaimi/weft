//! Terminal capability-query reply builders (FIX_TERMINAL_CAPABILITY_HARDENING).
//!
//! XTVERSION (`CSI > 0 q`) and XTGETTCAP (`DCS + q ...`) reply construction
//! lives here so the `vte::Perform` impl in `perform.rs` stays within its
//! audited line budget (scripts/architecture_allowlist.txt). These are pure
//! byte builders — no terminal state, no I/O. v1.11.3 adds DECRQSS (`DCS $ q
//! ...`), the nvim extended-underline probe path (PLAN_v1113 §2.2), and the
//! XTGETTCAP `Su` positive answer (PLAN_v1113 §2.3).

use super::attrs::Attrs;
use crate::grid::{CellColor, CellFlags, UnderlineStyle};
use base64::Engine;

/// XTVERSION reply (`CSI > 0 q` / `CSI > q`): `DCS > | weft <version> ST`,
/// the form xterm answers with. Applications (opencode, etc.) parse the
/// `weft` name + version banner at startup to detect terminal identity.
#[must_use]
pub fn xtversion_reply() -> Vec<u8> {
    format!("\x1bP>|weft {}\x1b\\", env!("CARGO_PKG_VERSION")).into_bytes()
}

/// v1.11.4 (PLAN_v1114 §1.2): kitty keyboard-protocol query answer —
/// `CSI ? <flags> u` (no space). Always answered with the CURRENT flags
/// (stack top); a request parameter (`CSI ? 1 u`) is ignored — the
/// protocol has no conditional-answer concept and 0 is a legitimate state,
/// not a "no support" signal.
#[must_use]
pub fn kitty_flags_reply(flags: u8) -> Vec<u8> {
    format!("\x1b[?{flags}u").into_bytes()
}

/// Split an XTGETTCAP request payload into its `;`-separated hex names.
///
/// A name is well-formed iff it is non-empty, even-length, and all ASCII
/// hex digits. Malformed segments (odd length, non-hex chars, empty) are
/// skipped — the request is answered per valid name and junk never echoes
/// back, so a hostile/corrupt payload cannot panic or produce garbage.
fn valid_hex_names(payload: &[u8]) -> Vec<&[u8]> {
    payload
        .split(|&b| b == b';')
        .filter(|seg| {
            !seg.is_empty() && seg.len() % 2 == 0 && seg.iter().all(|b| b.is_ascii_hexdigit())
        })
        .collect()
}

/// v1.11.3 (PLAN_v1113 §2.3): the one string capability we answer. `Su`
/// (terminfo `set_underline_color`, xterm/mintty) is mintty-protocol — it
/// is NOT the nvim COLORS path (nvim's probe uses DECRQSS, see
/// `decrqss_sgr_reply`); this answers terminals that speak the old protocol.
pub const KNOWN_STRING_CAPS: &[(&str, &str)] = &[("5375", "\x1b[4:%dm")]; // Su

/// Public view of the well-formed hex names in an XTGETTCAP payload, so
/// `perform.rs` can answer per name (positive hit → `xtgettcap_reply`,
/// else negative) without re-parsing.
pub fn xtgettcap_requested_names(payload: &[u8]) -> Vec<&[u8]> {
    valid_hex_names(payload)
}

/// v1.11.3 (PLAN_v1113 §2.3): positive XTGETTCAP answer for a KNOWN_STRING_CAPS
/// hit: `DCS 1 + r <hex>=<base64(value)> ST`. Unknown names → `None` and the
/// caller falls back to the negative `0+r` answer.
#[must_use]
pub fn xtgettcap_reply(hex_name: &[u8]) -> Option<Vec<u8>> {
    let name = std::str::from_utf8(hex_name).ok()?;
    let (_, value) = KNOWN_STRING_CAPS.iter().find(|(n, _)| *n == name)?;
    let mut reply = Vec::with_capacity(name.len() + value.len() * 2 + 16);
    reply.extend_from_slice(b"\x1bP1+r");
    reply.extend_from_slice(hex_name);
    reply.push(b'=');
    reply.extend_from_slice(base64_encode(value.as_bytes()).as_bytes());
    reply.extend_from_slice(b"\x1b\\");
    Some(reply)
}

/// RFC 4648 standard alphabet, no padding — xterm's XTGETTCAP answers omit
/// padding. v1.11.5 (PLAN_v1115 D-b): replaced the handwritten encoder with
/// the promoted `base64` crate (locked at 0.22.1); `Pad::None` keeps the
/// `Su` golden answer byte-identical (`G1s0OiVkbQ`, asserted in tests).
fn base64_encode(input: &[u8]) -> String {
    base64::engine::general_purpose::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new().with_encode_padding(false),
    )
    .encode(input)
}

/// Negative XTGETTCAP answers: `DCS 0 + r <hex-name> ST` per requested name.
///
/// This terminal supports none of the queried capabilities, so every
/// well-formed requested name is answered `0` (not supported). The reply
/// echoes the request's hex name verbatim, exactly as xterm does.
#[must_use]
pub fn xtgettcap_negative_replies(payload: &[u8]) -> Vec<Vec<u8>> {
    valid_hex_names(payload)
        .into_iter()
        .map(|name| {
            let mut reply = Vec::with_capacity(name.len() + 6);
            reply.extend_from_slice(b"\x1bP0+r");
            reply.extend_from_slice(name);
            reply.push(0x1b); // ST — string terminator
            reply.push(b'\\');
            reply
        })
        .collect()
}

/// v1.11.3 DECRQSS (PLAN_v1113 §2.2): refused answer for an unanswerable
/// query — `DCS 0 $ r <pt> ST`, echoing the query string per DECRQSS.
#[must_use]
pub fn decrqss_refused_reply(payload: &[u8]) -> Vec<u8> {
    let mut reply = Vec::with_capacity(payload.len() + 6);
    reply.extend_from_slice(b"\x1bP0$r");
    reply.extend_from_slice(payload);
    reply.extend_from_slice(b"\x1b\\");
    reply
}

/// v1.11.3 DECRQSS `$q m` SGR answer (PLAN_v1113 §2.2) — the nvim
/// extended-underline unlock path.
///
/// nvim probes with `ESC[0m ESC[4:3m ESC P $ q m ST` and requires the
/// response body to equal `1$r4:3m` (or the xterm-style `1$r0;4:3m`)
/// exactly — `tui.c tui_query_extended_underline` + `input.c
/// handle_term_response` byte-match the whole body between DCS/ST. The
/// serializer therefore emits ONLY non-default attributes, canonical order
/// (underline family first so the probe's wavy state serializes as `4:3`
/// with no other params), and `0` alone for the all-default state.
#[must_use]
pub fn decrqss_sgr_reply(attrs: &Attrs) -> Vec<u8> {
    let mut parts: Vec<String> = Vec::new();

    // Dual-track (PLAN_v1113 §1.2): the DOUBLE_UNDER bit wins when both
    // carriers disagree (legacy cells set only the bit; SGR 4:2 sets both).
    let style = if attrs.flags.contains(CellFlags::DOUBLE_UNDER) {
        Some(UnderlineStyle::Double)
    } else if attrs.flags.contains(CellFlags::UNDERLINE) {
        Some(attrs.underline_style)
    } else {
        None
    };
    if let Some(s) = style {
        // Single serializes as plain `4`; the rest as `4:<n>` (n∈1..=5).
        parts.push(match s {
            UnderlineStyle::Single => "4".to_string(),
            UnderlineStyle::Double => "4:2".to_string(),
            UnderlineStyle::Wavy => "4:3".to_string(),
            UnderlineStyle::Dotted => "4:4".to_string(),
            UnderlineStyle::Dashed => "4:5".to_string(),
        });
    }
    if attrs.flags.contains(CellFlags::BOLD) {
        parts.push("1".into());
    }
    if attrs.flags.contains(CellFlags::DIM) {
        parts.push("2".into());
    }
    if attrs.flags.contains(CellFlags::ITALIC) {
        parts.push("3".into());
    }
    if attrs.flags.contains(CellFlags::REVERSE) {
        parts.push("7".into());
    }
    if attrs.flags.contains(CellFlags::HIDDEN) {
        parts.push("8".into());
    }
    if attrs.flags.contains(CellFlags::STRIKETHROUGH) {
        parts.push("9".into());
    }
    match attrs.fg {
        CellColor::Default => {}
        CellColor::Palette(n) => parts.push(format!("38;5;{n}")),
        CellColor::Rgb(c) => parts.push(format!("38;2;{};{};{}", c.r, c.g, c.b)),
    }
    match attrs.bg {
        CellColor::Default => {}
        CellColor::Palette(n) => parts.push(format!("48;5;{n}")),
        CellColor::Rgb(c) => parts.push(format!("48;2;{};{};{}", c.r, c.g, c.b)),
    }
    match attrs.underline_color {
        None => {}
        Some(CellColor::Palette(n)) => parts.push(format!("58;5;{n}")),
        Some(CellColor::Rgb(c)) => parts.push(format!("58;2;{};{};{}", c.r, c.g, c.b)),
        Some(CellColor::Default) => {}
    }

    let body = if parts.is_empty() {
        "0".to_string() // all-default state: explicit reset, xterm-style
    } else {
        parts.join(";")
    };
    format!("\x1bP1$r{body}m\x1b\\").into_bytes()
}

/// v1.11.3 (PLAN_v1113 §2.2): build ALL reply bytes for a finished DCS
/// query, so `perform.rs`'s unhook stays a thin loop. XTGETTCAP: known
/// string caps get a positive `1+r <hex>=<base64>` answer, everything else
/// the negative `0+r <hex>`; malformed hex segments are skipped. DECRQSS:
/// payload `m` answers with the current SGR (nvim's path), anything else
/// gets the refused `0$r <pt>` echo.
#[must_use]
pub fn dcs_query_replies(kind: DcsQueryKind, payload: &[u8], attrs: &Attrs) -> Vec<Vec<u8>> {
    match kind {
        DcsQueryKind::Xtgettcap => xtgettcap_requested_names(payload)
            .into_iter()
            .map(|name| {
                xtgettcap_reply(name).unwrap_or_else(|| {
                    // `name` came from valid_hex_names → exactly one negative
                    // reply by construction.
                    xtgettcap_negative_replies(name).remove(0)
                })
            })
            .collect(),
        DcsQueryKind::Decrqss => {
            if payload == b"m" {
                vec![decrqss_sgr_reply(attrs)]
            } else {
                vec![decrqss_refused_reply(payload)]
            }
        }
    }
}

/// Which DCS query a collector instance is accumulating (v1.11.3,
/// PLAN_v1113 §2.2): XTGETTCAP (`DCS + q ...`) or DECRQSS (`DCS $ q ...`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DcsQueryKind {
    Xtgettcap,
    Decrqss,
}

/// Bounded collector for an in-flight DCS query (`DCS (+|$) q ... ST`).
///
/// vte delivers the introducer to `Perform::hook`, every payload byte to
/// `put()`, and the terminator to `unhook()`. The payload is capped at
/// [`MAX_XTGETTCAP_PAYLOAD`] so a `DCS (+|$) q` introducer followed by a
/// long run of non-exit bytes cannot grow memory without bound (vte Puts
/// every passthrough byte in 0x00..=0x7e). An overflowed request is dropped
/// whole in `finish()` — silently, never partially answered.
pub const MAX_XTGETTCAP_PAYLOAD: usize = 1024;

#[derive(Default)]
pub struct DcsQueryCollector {
    payload: Option<Vec<u8>>,
    truncated: bool,
    kind: Option<DcsQueryKind>,
}

impl DcsQueryCollector {
    /// Start (or reset) collection. Only a `+q` (XTGETTCAP) or `$q`
    /// (DECRQSS) introducer is collected; any other DCS is ignored
    /// end-to-end.
    pub fn begin(&mut self, action: char, intermediates: &[u8]) {
        self.truncated = false;
        match (action, intermediates) {
            ('q', [b'+']) => {
                self.kind = Some(DcsQueryKind::Xtgettcap);
                self.payload = Some(Vec::new());
            }
            ('q', [b'$']) => {
                self.kind = Some(DcsQueryKind::Decrqss);
                self.payload = Some(Vec::new());
            }
            _ => {
                self.kind = None;
                self.payload = None;
            }
        }
    }

    /// Feed one payload byte; appends stop at the cap and the request is
    /// marked truncated.
    pub fn push(&mut self, byte: u8) {
        if let Some(payload) = self.payload.as_mut() {
            if payload.len() < MAX_XTGETTCAP_PAYLOAD {
                payload.push(byte);
            } else {
                self.truncated = true;
            }
        }
    }

    /// End of the request: returns the collected payload + query kind to
    /// answer, or `None` when there was no `+q`/`$q` request or it
    /// overflowed the cap.
    pub fn finish(&mut self) -> Option<(DcsQueryKind, Vec<u8>)> {
        let payload = self.payload.take()?;
        let kind = self.kind.take()?;
        if self.truncated {
            None
        } else {
            Some((kind, payload))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xtversion_reply_has_banner_shape() {
        let reply = xtversion_reply();
        let s = String::from_utf8_lossy(&reply);
        assert!(s.starts_with("\x1bP>|weft "), "banner prefix: {s:?}");
        assert!(s.ends_with("\x1b\\"), "ST terminator: {s:?}");
        assert!(s.contains(env!("CARGO_PKG_VERSION")), "version: {s:?}");
    }

    /// v1.11.4 (PLAN_v1114 §4.2): exact answer bytes — `?0u` is a
    /// legitimate answer (empty stack), never the "silent" signal.
    #[test]
    fn kitty_flags_reply_exact_bytes() {
        assert_eq!(kitty_flags_reply(0), b"\x1b[?0u");
        assert_eq!(kitty_flags_reply(1), b"\x1b[?1u");
        assert_eq!(kitty_flags_reply(0b1_1011), b"\x1b[?27u");
        assert!(!kitty_flags_reply(1).contains(&b' '), "no space in answer");
    }

    #[test]
    fn xtgettcap_answers_each_valid_name() {
        let replies = xtgettcap_negative_replies(b"4d73;636f6c6f72");
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0], b"\x1bP0+r4d73\x1b\\");
        assert_eq!(replies[1], b"\x1bP0+r636f6c6f72\x1b\\");
    }

    #[test]
    fn xtgettcap_skips_malformed_segments() {
        // odd-length hex, non-hex, empty segment, uppercase hex (valid).
        let replies = xtgettcap_negative_replies(b"4d7;zz;636f6c6f72;x;;4D73");
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0], b"\x1bP0+r636f6c6f72\x1b\\");
        assert_eq!(replies[1], b"\x1bP0+r4D73\x1b\\");
    }

    #[test]
    fn xtgettcap_empty_payload_answers_nothing() {
        assert!(xtgettcap_negative_replies(b"").is_empty());
        assert!(xtgettcap_negative_replies(b";;;").is_empty());
    }

    /// v1.11.3 (PLAN_v1113 §4.4): known cap `Su` → positive `1+r` answer
    /// with base64 value; unknown caps stay negative.
    #[test]
    fn xtgettcap_su_gets_positive_answered_reply() {
        // Su = "5375"; value "\x1b[4:%dm" → base64 "G1s0OiVkbQ==" without
        // padding: "G1s0OiVkbQ". xterm omits padding in XTGETTCAP values.
        let reply = xtgettcap_reply(b"5375").expect("Su must be known");
        assert_eq!(
            std::str::from_utf8(&reply).unwrap(),
            "\x1bP1+r5375=G1s0OiVkbQ\x1b\\"
        );
    }

    #[test]
    fn xtgettcap_unknown_name_returns_none() {
        assert!(xtgettcap_reply(b"636f6c6f72").is_none(), "color: unknown");
        assert!(xtgettcap_reply(b"zz").is_none(), "malformed hex: no panic");
    }

    #[test]
    fn base64_encoder_matches_rfc_vectors_without_padding() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg");
        assert_eq!(base64_encode(b"fo"), "Zm8");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    /// v1.11.3 (PLAN_v1113 §4.3): DECRQSS `$qm` after `ESC[0m ESC[4:3m`
    /// answers EXACTLY `1$r4:3m` — nvim byte-matches this body.
    #[test]
    fn decrqss_sgr_reply_wavy_is_exactly_1_dollar_r_4_colon_3() {
        let attrs = Attrs {
            flags: CellFlags::UNDERLINE,
            underline_style: UnderlineStyle::Wavy,
            ..Attrs::default()
        };
        assert_eq!(
            std::str::from_utf8(&decrqss_sgr_reply(&attrs)).unwrap(),
            "\x1bP1$r4:3m\x1b\\"
        );
    }

    /// v1.11.3: double underline (both carriers, SGR 21 state) → `4:2`.
    #[test]
    fn decrqss_sgr_reply_double_serializes_4_colon_2() {
        let attrs = Attrs {
            flags: CellFlags::DOUBLE_UNDER,
            underline_style: UnderlineStyle::Double,
            ..Attrs::default()
        };
        assert_eq!(
            std::str::from_utf8(&decrqss_sgr_reply(&attrs)).unwrap(),
            "\x1bP1$r4:2m\x1b\\"
        );
    }

    /// v1.11.3: the DOUBLE_UNDER bit alone (legacy path, contrast_tests
    /// builds cells like this) also serializes as `4:2`.
    #[test]
    fn decrqss_sgr_reply_double_bit_without_style_still_4_colon_2() {
        let attrs = Attrs {
            flags: CellFlags::DOUBLE_UNDER,
            ..Attrs::default()
        };
        assert_eq!(
            std::str::from_utf8(&decrqss_sgr_reply(&attrs)).unwrap(),
            "\x1bP1$r4:2m\x1b\\"
        );
    }

    /// v1.11.3: fully default attrs → the answer contains no underline
    /// segment (explicit `0` reset body).
    #[test]
    fn decrqss_sgr_reply_default_attrs_have_no_underline() {
        let reply = String::from_utf8(decrqss_sgr_reply(&Attrs::default())).unwrap();
        assert_eq!(reply, "\x1bP1$r0m\x1b\\");
        assert!(!reply.contains("4:"), "no underline segment: {reply}");
    }

    /// v1.11.3: non-underline attributes join in canonical order with
    /// colors (38/48/58 per SGR convention).
    #[test]
    fn decrqss_sgr_reply_full_attrs_join_with_semicolons() {
        let attrs = Attrs {
            flags: CellFlags::BOLD | CellFlags::UNDERLINE,
            underline_style: UnderlineStyle::Dashed,
            fg: CellColor::Palette(2),
            bg: CellColor::Rgb(crate::grid::Color::rgb(1, 2, 3)),
            underline_color: Some(CellColor::Palette(196)),
        };
        let reply = String::from_utf8(decrqss_sgr_reply(&attrs)).unwrap();
        assert_eq!(reply, "\x1bP1$r4:5;1;38;5;2;48;2;1;2;3;58;5;196m\x1b\\");
    }

    #[test]
    fn xtgettcap_collector_answers_within_cap() {
        let mut c = DcsQueryCollector::default();
        c.begin('q', b"+");
        for &b in b"4d73" {
            c.push(b);
        }
        let (kind, payload) = c.finish().expect("in-cap request must answer");
        assert_eq!(kind, DcsQueryKind::Xtgettcap);
        assert_eq!(payload, b"4d73");
    }

    /// v1.11.3: DECRQSS `$q` introducer collects the same way, tagged with
    /// its kind so unhook can dispatch between `1$r` and `1+r` prefixes.
    #[test]
    fn decrqss_collector_answers_with_kind() {
        let mut c = DcsQueryCollector::default();
        c.begin('q', b"$");
        for &b in b"m" {
            c.push(b);
        }
        let (kind, payload) = c.finish().expect("DECRQSS request must answer");
        assert_eq!(kind, DcsQueryKind::Decrqss);
        assert_eq!(payload, b"m");
    }

    #[test]
    fn xtgettcap_collector_caps_payload_and_drops_overflow() {
        let mut c = DcsQueryCollector::default();
        c.begin('q', b"+");
        for _ in 0..2048 {
            c.push(b'4');
        }
        assert!(
            c.finish().is_none(),
            "overflowed request must be dropped whole, silently"
        );
    }

    #[test]
    fn xtgettcap_collector_ignores_non_requests() {
        let mut c = DcsQueryCollector::default();
        c.begin('r', b"!"); // not a `+q`/`$q` introducer
        c.push(b'4');
        assert!(c.finish().is_none());

        // a fresh request after any state resets cleanly
        let mut c = DcsQueryCollector::default();
        c.begin('q', b"+");
        c.push(b'4');
        c.push(b'd');
        c.push(b'7');
        c.push(b'3');
        let (_, payload) = c.finish().expect("re-armed request must answer");
        assert_eq!(payload, b"4d73");
    }
}
