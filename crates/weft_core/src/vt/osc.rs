use base64::engine::general_purpose::GeneralPurpose;
use base64::engine::{DecodePaddingMode, GeneralPurposeConfig};
use base64::Engine;

use crate::grid::Color;

use super::ui_events::{
    DockProgress, NOTIFY_BODY_MAX, NOTIFY_TITLE_MAX, OSC52_MAX_BYTES, OSC52_READ_REPLY_MAX,
};

/// Hard cap for OSC 0/2 window titles and OSC 7 cwd payloads. The osc_guard
/// 1MiB cap bounds the raw sequence; this bounds what we retain — a title
/// longer than any legible window label is noise (and an NSWindow-title
/// rendering hazard). Truncation is char-boundary safe (CJK/emoji titles).
pub const OSC_TITLE_MAX_BYTES: usize = 4096;

/// Lossy-decode an OSC payload and cap it at [`OSC_TITLE_MAX_BYTES`] bytes
/// without splitting a UTF-8 char (follows `parse_osc_notify`'s char-level
/// truncation precedent).
pub fn cap_osc_payload(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    if s.len() <= OSC_TITLE_MAX_BYTES {
        return s.into_owned();
    }
    // Walk back to the nearest char boundary at or before the cap
    // (`is_char_boundary(0)` is always true, so this terminates).
    let mut end = OSC_TITLE_MAX_BYTES;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Parse an OSC 7 payload `file://[host]/abs/path` → `/abs/path`.
pub fn parse_osc7_cwd(payload: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(payload).ok()?;
    let s = s.strip_prefix("file://")?;
    let path_start = s.find('/')?;
    Some(s[path_start..].to_string())
}

/// Parse an X11 color string (#RRGGBB or rgb:RR/GG/BB) into a Color.
pub fn parse_x11_color(bytes: &[u8]) -> Option<Color> {
    let s = std::str::from_utf8(bytes).ok()?;

    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() == 6 {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some(Color::rgb(r, g, b));
        }
    }

    if let Some(rest) = s.strip_prefix("rgb:") {
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() == 3 {
            let r = u8::from_str_radix(parts[0], 16).ok()?;
            let g = u8::from_str_radix(parts[1], 16).ok()?;
            let b = u8::from_str_radix(parts[2], 16).ok()?;
            return Some(Color::rgb(r, g, b));
        }
    }

    None
}

// ── v1.11.5 OSC 52 / 9 / 777 (PLAN_v1115 §M1) ────────────────────────────

/// RFC 4648 standard-alphabet decoder that accepts missing padding — OSC 52
/// interop reality: wezterm/ghostty emit paddingless payloads, xterm pads.
const B64_INDIFFERENT: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// What an OSC 52 sequence should do.
#[derive(Debug, PartialEq, Eq)]
pub enum Osc52Result {
    /// `;?` payload — the program asks to read the clipboard. The app
    /// decides (permission prompt); answering is app-side.
    Read,
    /// A decodeable write payload for the system clipboard. `truncated` is
    /// set when the payload exceeded [`OSC52_MAX_BYTES`] after decoding.
    Write { data: Vec<u8>, truncated: bool },
    /// Not addressed to the system clipboard (selection mask mismatch),
    /// malformed, or empty — dropped with a trace, no event.
    Ignore,
}

/// Parse `OSC 52 ; <sel> ; <data>` (params after splitting on `;`).
///
/// Selection semantics (xterm): `0`/`c` = clipboard, `s` = primary,
/// `p` = secondary. We map the system clipboard only; per PLAN_v1115 §D-b,
/// an EMPTY sel segment (`52;;data`) also means the clipboard (wezterm/
/// ghostty convention), and a sel containing ANY of `c`/`s`/`p`/`0` is
/// treated as clipboard — weft has a single pasteboard anyway ("primary
/// selection 独立粘贴板语义" is a declared non-goal).
///
/// `data == "?"` is a read request. Anything else is base64 (`Indifferent`
/// padding: padded and paddingless both decode). A payload that decodes to
/// more than [`OSC52_MAX_BYTES`] is truncated with `truncated: true` — the
/// app shows a toast. Invalid base64 or a missing data segment yields
/// [`Osc52Result::Ignore`] (no event, defensive trace at the call site).
///
/// Layering note (reviewer P2-6): with the OSC watch guard enabled (the
/// default) the byte-level 1 MiB raw cap fires FIRST, so any payload that
/// survives the guard decodes to ≲ 786 KiB < [`OSC52_MAX_BYTES`] and
/// `truncated` stays false in practice — the toast path is a defensive
/// layer that only lives when the guard is disabled.
///
/// Known accepted quirk: a truncated decode may cut a UTF-8 sequence in the
/// middle, whose tail renders as U+FFFD — documented, not worth repairing.
pub fn parse_osc52(params: &[&[u8]]) -> Osc52Result {
    let Some(sel) = params.get(1).copied() else {
        return Osc52Result::Ignore; // bare `OSC 52` — nothing addressed
    };
    if !sel.is_empty() && !sel.iter().any(|b| matches!(b, b'c' | b's' | b'p' | b'0')) {
        return Osc52Result::Ignore; // e.g. `52;q;...` — not the clipboard
    }
    let Some(data) = params.get(2).copied() else {
        return Osc52Result::Ignore; // `OSC 52;c` — missing data segment
    };
    if data == b"?" {
        return Osc52Result::Read;
    }
    let Ok(mut decoded) = B64_INDIFFERENT.decode(data) else {
        return Osc52Result::Ignore; // invalid base64 — never a partial event
    };
    let truncated = decoded.len() > OSC52_MAX_BYTES;
    if truncated {
        decoded.truncate(OSC52_MAX_BYTES);
    }
    Osc52Result::Write {
        data: decoded,
        truncated,
    }
}

/// Remote notification text (title + body) reconstructed from an OSC 9 or
/// OSC 777 sequence.
#[derive(Debug, PartialEq, Eq)]
pub struct OscNotifyText {
    pub title: String,
    pub body: String,
}

/// Parse `OSC 9;message` / `OSC 777;notify;title;body` into a notification.
///
/// - `9`: params[1..] re-joined with `;` (vte splits on `;`, so a message
///   containing `;` must be reassembled); title stays empty.
/// - `777`: first param after the code must be exactly `notify`; title is
///   params[2], body is params[3..] re-joined with `;` (rxvt/wezterm
///   convention). Any other 777 subcommand → `None`.
///
/// Non-UTF-8 input is lossy-converted; title/body are truncated at their
/// char caps. Returns `None` for an empty message or a non-notify 777.
pub fn parse_osc_notify(params: &[&[u8]]) -> Option<OscNotifyText> {
    match params.first().copied() {
        Some(b"9") => {
            if params.len() < 2 {
                return None;
            }
            let body = join_params(params, 1);
            let body = String::from_utf8_lossy(&body).into_owned();
            if body.is_empty() {
                return None;
            }
            Some(OscNotifyText {
                title: String::new(),
                body: truncate_chars(body, NOTIFY_BODY_MAX),
            })
        }
        Some(b"777") => {
            if params.get(1).copied() != Some(b"notify".as_slice()) {
                return None; // non-notify 777 subcommand
            }
            let title = params
                .get(2)
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .unwrap_or_default();
            let body = join_params(params, 3);
            let body = String::from_utf8_lossy(&body).into_owned();
            Some(OscNotifyText {
                title: truncate_chars(title, NOTIFY_TITLE_MAX),
                body: truncate_chars(body, NOTIFY_BODY_MAX),
            })
        }
        _ => None,
    }
}

/// Parse `OSC 9;4;state[;progress]` into a [`DockProgress`].
///
/// State codes take **iTerm2-family semantics** (0→Clear, 1→Indeterminate,
/// 2→Failed, 3→Percent), NOT ConEmu-family (which swaps 1/3 — used by
/// Ghostty/Windows Terminal). Known deviation, recorded: the badges are
/// decorative text, and iTerm2 is the macOS reference implementation; if a
/// ConEmu-family program is ever observed to mis-display, add a compat mode.
pub fn parse_osc_progress(state: &[u8], progress: &[u8]) -> Option<DockProgress> {
    match state {
        b"0" => Some(DockProgress::Clear),
        b"1" => Some(DockProgress::Indeterminate),
        b"2" => Some(DockProgress::Failed),
        b"3" => {
            let percent = std::str::from_utf8(progress).ok()?.parse::<u16>().ok()?;
            Some(DockProgress::Percent(percent.clamp(0, 100) as u8))
        }
        _ => None, // unknown state or missing param → caller ignores
    }
}

/// Re-join `params[start..]` with `;` (vte splits OSC params on `;`).
fn join_params(params: &[&[u8]], start: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, p) in params.iter().enumerate().skip(start) {
        if i > start {
            out.push(b';');
        }
        out.extend_from_slice(p);
    }
    out
}

/// Truncate at a UTF-8 char boundary (never splits a sequence).
fn truncate_chars(s: String, max: usize) -> String {
    if s.chars().count() <= max {
        s
    } else {
        s.chars().take(max).collect()
    }
}

/// Build the OSC 52 read-answer bytes: `ESC ] 52 ; c ; <b64> BEL`, STANDARD
/// (padded) encoding — xterm-form replies arrive padded.
///
/// Returns `None` when the clipboard payload exceeds
/// [`OSC52_READ_REPLY_MAX`]: the caller must then answer as DENY (no
/// answer at all). Echoing half a base64 blob into a 50 ms `write_sync`
/// budget would time the peer out silently, which is worse than a clean
/// no-answer (PLAN_v1115 D-j). Empty data is legal (`52;c;` + BEL).
pub fn osc52_read_reply(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() > OSC52_READ_REPLY_MAX {
        return None;
    }
    let b64 = base64::engine::general_purpose::STANDARD.encode(data);
    let mut out = Vec::with_capacity(b64.len() + 9);
    out.extend_from_slice(b"\x1b]52;c;");
    out.extend_from_slice(b64.as_bytes());
    out.push(0x07);
    Some(out)
}

// ── tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vt::ui_events::DockProgress;

    /// Helper: build params the way vte hands `osc_dispatch` (split on `;`).
    fn p(input: &str) -> Vec<&[u8]> {
        input.split(';').map(|s| s.as_bytes()).collect()
    }

    // ── OSC 9;message ─────────────────────────────────────────────────

    #[test]
    fn osc9_message_joins_multi_segment_body() {
        // vte splits `OSC 9;hello world` on `;` → params[1..] must be
        // re-joined so a body containing `;` survives intact.
        let text = parse_osc_notify(&p("9;a;b;c")).expect("notify text");
        assert_eq!(text.title, "");
        assert_eq!(text.body, "a;b;c");
    }

    #[test]
    fn osc9_empty_message_is_none() {
        assert!(parse_osc_notify(&p("9")).is_none(), "no message");
        assert!(parse_osc_notify(&[b"9", b""]).is_none(), "empty message");
    }

    #[test]
    fn osc9_body_is_lossy_and_truncated() {
        let long = "x".repeat(NOTIFY_BODY_MAX + 50);
        let text = parse_osc_notify(&[b"9", long.as_bytes()]).expect("notify text");
        assert_eq!(text.body.chars().count(), NOTIFY_BODY_MAX);
        // non-UTF-8 payload → U+FFFD, no panic, still truncated
        let text = parse_osc_notify(&[b"9", &[0xFF, 0xFE, b'a', 0x80][..]]).expect("lossy text");
        assert!(text.body.contains('\u{FFFD}'));
    }

    // ── OSC 777;notify;title;body ──────────────────────────────────────

    #[test]
    fn osc777_notify_parses_title_and_body() {
        let text =
            parse_osc_notify(&p("777;notify;Deploy;build done;on;prod")).expect("notify text");
        assert_eq!(text.title, "Deploy");
        assert_eq!(text.body, "build done;on;prod");
    }

    #[test]
    fn osc777_missing_body_fields_default_empty() {
        let text = parse_osc_notify(&p("777;notify;Title")).expect("notify text");
        assert_eq!(text.title, "Title");
        assert_eq!(text.body, "");
        let text = parse_osc_notify(&p("777;notify")).expect("notify text");
        assert_eq!(text.title, "");
        assert_eq!(text.body, "");
    }

    #[test]
    fn osc777_non_notify_subcommand_is_none() {
        // e.g. rxvt's `777;scrollback:...` — must not become a notification.
        assert!(parse_osc_notify(&p("777;scrollback;+1")).is_none());
        assert!(parse_osc_notify(&p("777;noti")).is_none()); // prefix ≠ word
    }

    #[test]
    fn osc777_title_and_body_truncated_at_caps() {
        let title = "t".repeat(NOTIFY_TITLE_MAX + 10);
        let body = "b".repeat(NOTIFY_BODY_MAX + 10);
        let text = parse_osc_notify(&[b"777", b"notify", title.as_bytes(), body.as_bytes()])
            .expect("notify text");
        assert_eq!(text.title.chars().count(), NOTIFY_TITLE_MAX);
        assert_eq!(text.body.chars().count(), NOTIFY_BODY_MAX);
        // truncation never splits a UTF-8 sequence: CJK title stays valid
        let title = "汉".repeat(NOTIFY_TITLE_MAX + 5);
        let text =
            parse_osc_notify(&[b"777", b"notify", title.as_bytes(), b"b"]).expect("notify text");
        assert_eq!(text.title.chars().count(), NOTIFY_TITLE_MAX);
        assert!(text.title.is_char_boundary(text.title.len()));
    }

    #[test]
    fn osc777_lossy_converts_invalid_utf8() {
        let text =
            parse_osc_notify(&[b"777", b"notify", &[0xC3, 0x28][..], b"ok"]).expect("lossy text");
        assert!(text.title.contains('\u{FFFD}'));
    }

    #[test]
    fn osc_notify_unknown_code_is_none() {
        assert!(parse_osc_notify(&p("100;x")).is_none());
    }

    // ── OSC 9;4 progress (iTerm2 semantics) ────────────────────────────

    #[test]
    fn progress_state_codes_follow_iterm2_semantics() {
        // 0→Clear, 1→Indeterminate, 2→Failed, 3→Percent — NOT ConEmu's
        // 1/3 swap (see parse_osc_progress doc for the known deviation).
        assert_eq!(parse_osc_progress(b"0", b""), Some(DockProgress::Clear));
        assert_eq!(
            parse_osc_progress(b"1", b""),
            Some(DockProgress::Indeterminate)
        );
        assert_eq!(parse_osc_progress(b"2", b""), Some(DockProgress::Failed));
        assert_eq!(
            parse_osc_progress(b"3", b"47"),
            Some(DockProgress::Percent(47))
        );
    }

    #[test]
    fn progress_percent_clamps_and_rejects_garbage() {
        assert_eq!(
            parse_osc_progress(b"3", b"0"),
            Some(DockProgress::Percent(0))
        );
        assert_eq!(
            parse_osc_progress(b"3", b"100"),
            Some(DockProgress::Percent(100))
        );
        assert_eq!(
            parse_osc_progress(b"3", b"150"),
            Some(DockProgress::Percent(100)),
            "clamp above 100"
        );
        assert!(
            parse_osc_progress(b"3", b"abc").is_none(),
            "non-numeric progress"
        );
        assert!(parse_osc_progress(b"3", b"").is_none(), "missing progress");
    }

    #[test]
    fn progress_missing_or_unknown_state_ignored() {
        assert!(parse_osc_progress(b"", b"").is_none(), "missing state");
        assert!(parse_osc_progress(b"9", b"42").is_none(), "unknown state");
    }

    // ── OSC 52 ─────────────────────────────────────────────────────────

    #[test]
    fn osc52_read_request() {
        assert_eq!(parse_osc52(&p("52;c;?")), Osc52Result::Read);
        assert_eq!(
            parse_osc52(&p("52;;?")),
            Osc52Result::Read,
            "empty sel = clipboard"
        );
    }

    #[test]
    fn osc52_write_decodes_plain_and_padded() {
        match parse_osc52(&p("52;c;Zm9vYmFy")) {
            Osc52Result::Write { data, truncated } => {
                assert_eq!(data, b"foobar");
                assert!(!truncated);
            }
            other => panic!("expected write, got {other:?}"),
        }
        // xterm-style padded payload must decode identically (`foob` needs
        // `==` padding in the standard alphabet)
        match parse_osc52(&p("52;c;Zm9vYg==")) {
            Osc52Result::Write { data, .. } => assert_eq!(data, b"foob"),
            other => panic!("expected padded write, got {other:?}"),
        }
        // empty sel (`52;;data`) writes the clipboard (wezterm/ghostty)
        match parse_osc52(&p("52;;Zm9v")) {
            Osc52Result::Write { data, .. } => assert_eq!(data, b"foo"),
            other => panic!("expected empty-sel write, got {other:?}"),
        }
    }

    #[test]
    fn osc52_write_cjk_survives_roundtrip() {
        let raw = "你好，weft".as_bytes();
        let b64 = B64_INDIFFERENT.encode(raw);
        match parse_osc52(&[b"52", b"c", b64.as_bytes()]) {
            Osc52Result::Write { data, truncated } => {
                assert_eq!(data, raw);
                assert!(!truncated);
            }
            other => panic!("expected write, got {other:?}"),
        }
    }

    #[test]
    fn osc52_sel_mask_variants_map_to_clipboard() {
        // a combined mask containing c/s/p any position → clipboard; `0` is
        // xterm's legacy clipboard selector and maps the same way
        for sel in [b"cs".as_slice(), b"scq", b"p", b"s", b"0", b"c"] {
            match parse_osc52(&[b"52", sel, b"Zm8="]) {
                Osc52Result::Write { data, .. } => assert_eq!(data, b"fo"),
                other => panic!("sel {sel:?}: expected write, got {other:?}"),
            }
        }
        // mask without c/s/p (e.g. `q`) → not our clipboard, ignored
        assert_eq!(parse_osc52(&p("52;q;Zm9v")), Osc52Result::Ignore);
    }

    #[test]
    fn osc52_invalid_or_missing_segments_ignored() {
        assert_eq!(parse_osc52(&p("52")), Osc52Result::Ignore, "no sel");
        assert_eq!(parse_osc52(&p("52;c")), Osc52Result::Ignore, "no data");
        // invalid base64 (illegal alphabet char + wrong length) → Ignore
        assert_eq!(parse_osc52(&p("52;c;!!!!")), Osc52Result::Ignore);
        assert_eq!(parse_osc52(&p("52;c;#")), Osc52Result::Ignore);
        // non-UTF8 params are fine for base64 decode rejection paths
        assert_eq!(
            parse_osc52(&[b"52", b"c", &[0xFF][..]]),
            Osc52Result::Ignore
        );
    }

    #[test]
    fn osc52_oversized_payload_truncated_with_flag() {
        // ~1.2 MiB of 'a' → base64 of the 1MiB cap is well under the guard's
        // 1MiB raw cap at decode time... build decoded→encoded programmatically
        let decoded = vec![b'a'; OSC52_MAX_BYTES + 1000];
        let b64 = B64_INDIFFERENT.encode(&decoded);
        match parse_osc52(&[b"52", b"c", b64.as_bytes()]) {
            Osc52Result::Write { data, truncated } => {
                assert!(truncated, "must flag truncation");
                assert_eq!(data.len(), OSC52_MAX_BYTES);
            }
            other => panic!("expected truncated write, got {other:?}"),
        }
        // at-or-below cap → not truncated
        let b64 = B64_INDIFFERENT.encode(vec![b'b'; OSC52_MAX_BYTES]);
        match parse_osc52(&[b"52", b"c", b64.as_bytes()]) {
            Osc52Result::Write { truncated, .. } => assert!(!truncated),
            other => panic!("expected write, got {other:?}"),
        }
    }

    // ── osc52_read_reply (M3 answer builder) ───────────────────────────

    #[test]
    fn read_reply_encodes_padded_standard() {
        // xterm-form replies are STANDARD padded: `foobar` stays
        // `Zm9vYmFy` (2 full groups), `foob` gains `==`.
        let reply = osc52_read_reply(b"foobar").expect("in-cap");
        assert_eq!(reply, b"\x1b]52;c;Zm9vYmFy\x07");
        let reply = osc52_read_reply(b"foob").expect("in-cap");
        assert_eq!(reply, b"\x1b]52;c;Zm9vYg==\x07");
    }

    #[test]
    fn read_reply_roundtrips_cjk_and_empty() {
        let raw = "你好，weft".as_bytes();
        let reply = osc52_read_reply(raw).expect("in-cap");
        let payload = &reply[7..reply.len() - 1]; // strip ESC]52;c; + BEL
        let decoded = B64_INDIFFERENT.decode(payload).unwrap();
        assert_eq!(decoded, raw);
        // empty clipboard → empty payload, still a valid answer shape
        let reply = osc52_read_reply(b"").expect("empty is legal");
        assert_eq!(reply, b"\x1b]52;c;\x07");
    }

    #[test]
    fn read_reply_over_cap_is_none() {
        let big = vec![b'x'; OSC52_READ_REPLY_MAX + 1];
        assert!(osc52_read_reply(&big).is_none(), "over cap → deny shape");
        let at_cap = vec![b'x'; OSC52_READ_REPLY_MAX];
        assert!(
            osc52_read_reply(&at_cap).is_some(),
            "exactly at cap still answers"
        );
    }

    // ── VULN-009: title/cwd payload cap ──────────────────────────────────

    #[test]
    fn cap_osc_payload_ascii_boundaries() {
        assert_eq!(cap_osc_payload(b"short"), "short");
        let b4095 = vec![b'x'; 4095];
        assert_eq!(
            cap_osc_payload(&b4095),
            String::from_utf8(b4095.clone()).unwrap()
        );
        let b4096 = vec![b'x'; 4096];
        assert_eq!(
            cap_osc_payload(&b4096),
            String::from_utf8(b4096.clone()).unwrap()
        );
        let b4097 = vec![b'x'; 4097];
        assert_eq!(cap_osc_payload(&b4097).len(), 4096);
        assert_eq!(cap_osc_payload(b""), "");
    }

    #[test]
    fn cap_osc_payload_truncates_on_char_boundary() {
        // 2000 CJK chars = 6000 bytes, all valid — the cut at byte 4096
        // would land mid-char (4095 is the boundary), so the result is the
        // first 1365 chars.
        let title = "表".repeat(2000);
        let capped = cap_osc_payload(title.as_bytes());
        assert_eq!(capped.chars().count(), 1365);
        assert_eq!(capped.len(), 1365 * 3);
        assert!(capped.len() <= OSC_TITLE_MAX_BYTES);
        assert!(
            !capped.contains('\u{FFFD}'),
            "valid UTF-8 must not be lossy-mangled"
        );
        assert_eq!(capped, "表".repeat(1365));
    }

    #[test]
    fn cap_osc_payload_dangling_continuation_byte_is_lossy_replaced() {
        // 4095 ASCII bytes + one dangling continuation byte: the lossy pass
        // turns it into U+FFFD (3 bytes ⇒ 4098), then the cap walks back to
        // the boundary at 4095 — no split char in the output.
        let mut bytes = vec![b'a'; 4095];
        bytes.push(0x80);
        let capped = cap_osc_payload(&bytes);
        assert_eq!(capped.len(), 4095);
        assert_eq!(capped.chars().count(), 4095);
        assert!(capped.chars().all(|c| c == 'a'));
    }
}
