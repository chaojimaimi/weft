//! Terminal capability-query reply builders (FIX_TERMINAL_CAPABILITY_HARDENING).
//!
//! XTVERSION (`CSI > 0 q`) and XTGETTCAP (`DCS + q ...`) reply construction
//! lives here so the `vte::Perform` impl in `perform.rs` stays within its
//! audited line budget (scripts/architecture_allowlist.txt). These are pure
//! byte builders — no terminal state, no I/O.

/// XTVERSION reply (`CSI > 0 q` / `CSI > q`): `DCS > | weft <version> ST`,
/// the form xterm answers with. Applications (opencode, etc.) parse the
/// `weft` name + version banner at startup to detect terminal identity.
#[must_use]
pub fn xtversion_reply() -> Vec<u8> {
    format!("\x1bP>|weft {}\x1b\\", env!("CARGO_PKG_VERSION")).into_bytes()
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

/// Bounded collector for an in-flight XTGETTCAP request (`DCS + q ... ST`).
///
/// vte delivers the introducer to `Perform::hook`, every payload byte to
/// `put()`, and the terminator to `unhook()`. The payload is capped at
/// [`MAX_XTGETTCAP_PAYLOAD`] so a `DCS + q` introducer followed by a long
/// run of non-exit bytes cannot grow memory without bound (vte Puts every
/// passthrough byte in 0x00..=0x7e). An overflowed request is dropped
/// whole in `finish()` — silently, never partially answered.
pub const MAX_XTGETTCAP_PAYLOAD: usize = 1024;

#[derive(Default)]
pub struct XtgettcapCollector {
    payload: Option<Vec<u8>>,
    truncated: bool,
}

impl XtgettcapCollector {
    /// Start (or reset) collection. Only a `+q` introducer is collected;
    /// any other DCS is ignored end-to-end.
    pub fn begin(&mut self, action: char, intermediates: &[u8]) {
        self.truncated = false;
        self.payload = (action == 'q' && intermediates == [b'+']).then(Vec::new);
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

    /// End of the request: returns the collected payload to answer, or
    /// `None` when there was no `+q` request or it overflowed the cap.
    pub fn finish(&mut self) -> Option<Vec<u8>> {
        let payload = self.payload.take()?;
        if self.truncated {
            None
        } else {
            Some(payload)
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

    #[test]
    fn xtgettcap_collector_answers_within_cap() {
        let mut c = XtgettcapCollector::default();
        c.begin('q', b"+");
        for &b in b"4d73" {
            c.push(b);
        }
        let payload = c.finish().expect("in-cap request must answer");
        assert_eq!(payload, b"4d73");
    }

    #[test]
    fn xtgettcap_collector_caps_payload_and_drops_overflow() {
        let mut c = XtgettcapCollector::default();
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
        let mut c = XtgettcapCollector::default();
        c.begin('r', b"!"); // not a `+q` introducer
        c.push(b'4');
        assert!(c.finish().is_none());

        // a fresh request after any state resets cleanly
        let mut c = XtgettcapCollector::default();
        c.begin('q', b"+");
        c.push(b'4');
        c.push(b'd');
        c.push(b'7');
        c.push(b'3');
        assert_eq!(c.finish().expect("re-armed request must answer"), b"4d73");
    }
}
