/// Build the bytes written to the PTY on submit. Warp model (DECISION §7.2):
/// `Ctrl-U` (clear any half-line defensively) + command (wrapped in
/// bracketed-paste if enabled, else internal `\n`→`\r`) + `\n`.
pub fn build_submit_bytes(command: &str, bracketed_paste_on: bool) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(command.len() + 16);
    bytes.push(0x15); // Ctrl-U
    if bracketed_paste_on && !command.is_empty() {
        bytes.extend(b"\x1b[200~");
        bytes.extend(command.as_bytes());
        bytes.extend(b"\x1b[201~");
    } else {
        for &b in command.as_bytes() {
            match b {
                b'\n' => bytes.push(b'\r'),
                // Strip C0 control chars and DEL (esp. ESC 0x1b) so a pasted
                // command can't inject terminal sequences when the shell lacks
                // bracketed-paste mode. \t and UTF-8 bytes pass through.
                0x00..=0x08 | 0x0b..=0x1f | 0x7f => {}
                _ => bytes.push(b),
            }
        }
    }
    bytes.push(b'\n');
    bytes
}

/// Encode bracketed paste start/end sequences.
pub fn bracketed_paste_start() -> Vec<u8> {
    b"\x1b[200~".to_vec()
}

pub fn bracketed_paste_end() -> Vec<u8> {
    b"\x1b[201~".to_vec()
}

/// C0 controls (except \t \n \r) and DEL stripped from bracketed paste
/// payloads. ESC is the injection vector: a pasted `\x1b[201~` forges the
/// terminator and everything after it executes as fresh keystrokes. Same
/// strip class as `build_submit_bytes`'s non-bracketed branch, minus \r
/// (paste preserves line structure; kitty strips the same class).
fn strip_bracketed_paste_controls(text: &str) -> String {
    text.chars()
        .filter(|&c| {
            // Inverse of `contains_dangerous_control_chars`'s predicate in
            // the same term order: the \t\n\r exemptions of the two functions
            // must stay identical (pinned by the consistency test below).
            !(c == '\x7f'
                || (c.is_ascii() && (c as u8) < 0x20 && c != '\t' && c != '\n' && c != '\r'))
        })
        .collect()
}

/// Wrap text for bracketed paste mode.
pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        let mut bytes = bracketed_paste_start();
        bytes.extend_from_slice(strip_bracketed_paste_controls(text).as_bytes());
        bytes.extend(bracketed_paste_end());
        bytes
    } else {
        // Explicit-risk path released by the confirmation gate (PLAN_v1111
        // §7 rollback contract): byte-verbatim passthrough, no stripping.
        text.as_bytes().to_vec()
    }
}

// ── v1.11.1 large-paste protection (PLAN_v1111 §4.1) ────────────────────
//
// Pure decision helpers for the paste-confirmation gate in
// `weft_app::app::effect_dispatch::apply_paste`. Zero platform dependencies
// so the whole matrix below is headless-testable.

/// Default size threshold (KiB) above which a paste asks for confirmation.
/// PLAN_v1111 §1: "超大（默认 >16KiB）".
pub const DEFAULT_PASTE_SIZE_THRESHOLD_KIB: u32 = 16;

/// Why a paste was flagged. `LargeAndControlChars` lets the alert name both
/// risks in one prompt instead of stacking two dialogs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasteRisk {
    /// Byte length exceeds the configured threshold.
    Large,
    /// Contains dangerous control characters (ESC / NUL / DEL …).
    ControlChars,
    /// Both of the above.
    LargeAndControlChars,
}

impl PasteRisk {
    /// Human-readable risk label used in the confirmation headline.
    pub fn label(self) -> &'static str {
        match self {
            PasteRisk::Large => "超大文本",
            PasteRisk::ControlChars => "含控制字符",
            PasteRisk::LargeAndControlChars => "超大文本且含控制字符",
        }
    }
}

/// The `[paste]` config subset the classifier needs. Kept separate from
/// `weft_core::config::PasteConfig` so this module stays a pure decision
/// layer (the config type owns serde/TOML concerns).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PasteGuardCfg {
    pub confirm_large: bool,
    pub confirm_control_chars: bool,
    pub size_threshold_kib: u32,
}

/// Classify a paste. Returns `Some(risk)` when user confirmation is needed,
/// `None` when the paste may proceed directly.
///
/// Size comparison is STRICTLY GREATER than the threshold (PLAN_v1111 §1:
/// ">16KiB" — exactly 16384 bytes passes without a dialog, 16385 confirms).
/// A disabled switch disables its risk class entirely; that is the plan's
/// documented one-switch rollback path back to pre-v1.11.1 behavior.
pub fn classify_paste(
    bytes_len: usize,
    has_control_chars: bool,
    cfg: &PasteGuardCfg,
) -> Option<PasteRisk> {
    let threshold_bytes = cfg.size_threshold_kib as usize * 1024;
    let large = cfg.confirm_large && bytes_len > threshold_bytes;
    let control = cfg.confirm_control_chars && has_control_chars;
    match (large, control) {
        (true, true) => Some(PasteRisk::LargeAndControlChars),
        (true, false) => Some(PasteRisk::Large),
        (false, true) => Some(PasteRisk::ControlChars),
        (false, false) => None,
    }
}

/// Dangerous control characters: C0 controls other than `\t` `\n` `\r`
/// (byte < 0x20), plus DEL (0x7F). Judged per `char`, so multi-byte UTF-8
/// sequences can never produce a false positive from their continuation
/// bytes. ESC (0x1b) is the main threat — pasted terminal sequences can
/// inject commands into a shell without bracketed-paste mode.
pub fn contains_dangerous_control_chars(text: &str) -> bool {
    text.chars().any(|c| {
        c == '\x7f' || (c.is_ascii() && (c as u8) < 0x20 && c != '\t' && c != '\n' && c != '\r')
    })
}

/// Safe preview of the first `max_chars` characters, truncated on a char
/// boundary (chars-based take is inherently boundary-safe) with a trailing
/// `…` only when truncation actually happened.
pub fn paste_preview(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut preview: String = text.chars().take(max_chars).collect();
    preview.push('…');
    preview
}

/// Human-readable byte count, 1024-based, at most one decimal place;
/// whole numbers render without a decimal point ("512 B", "15.6 KiB",
/// "1.2 MiB"). PLAN_v1111 §4.1 examples.
pub fn format_byte_count(bytes: usize) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if b < KIB {
        return format!("{bytes} B");
    }
    let (value, unit) = if b < MIB {
        (b / KIB, "KiB")
    } else if b < GIB {
        (b / MIB, "MiB")
    } else {
        (b / GIB, "GiB")
    };
    // Round half-to-even via f64 round(); strip ".0" for whole numbers.
    let rounded = (value * 10.0).round() / 10.0;
    if rounded.fract() == 0.0 {
        format!("{} {}", rounded as u64, unit)
    } else {
        format!("{rounded:.1} {unit}")
    }
}

#[cfg(test)]
mod paste_guard_tests {
    use super::{
        bracketed_paste_end, bracketed_paste_start, classify_paste,
        contains_dangerous_control_chars, encode_paste, format_byte_count, paste_preview,
        strip_bracketed_paste_controls, PasteGuardCfg, PasteRisk, DEFAULT_PASTE_SIZE_THRESHOLD_KIB,
    };

    fn cfg(size_threshold_kib: u32) -> PasteGuardCfg {
        PasteGuardCfg {
            confirm_large: true,
            confirm_control_chars: true,
            size_threshold_kib,
        }
    }

    fn default_cfg() -> PasteGuardCfg {
        cfg(DEFAULT_PASTE_SIZE_THRESHOLD_KIB)
    }

    // ── classify_paste: default-threshold boundary ─────────────────────

    #[test]
    fn exactly_16kib_passes_but_one_more_byte_confirms() {
        // PLAN_v1111 §1/§5.1: strictly ">16KiB" — 16384 free, 16385 confirms.
        assert_eq!(classify_paste(16384, false, &default_cfg()), None);
        assert_eq!(
            classify_paste(16385, false, &default_cfg()),
            Some(PasteRisk::Large)
        );
    }

    #[test]
    fn every_configured_threshold_tier_has_the_same_strict_boundary() {
        // Six legal tiers {8,16,32,64,128,256}: each must confirm at
        // kib*1024+1 and pass at exactly kib*1024 and below.
        for kib in [8u32, 16, 32, 64, 128, 256] {
            let guard = cfg(kib);
            let boundary = kib as usize * 1024;
            assert_eq!(
                classify_paste(boundary, false, &guard),
                None,
                "{kib} KiB exact"
            );
            assert_eq!(
                classify_paste(boundary + 1, false, &guard),
                Some(PasteRisk::Large),
                "{kib} KiB + 1"
            );
            assert_eq!(
                classify_paste(boundary - 1, false, &guard),
                None,
                "{kib} KiB - 1"
            );
        }
    }

    #[test]
    fn dangerous_control_chars_confirm_even_when_tiny() {
        assert_eq!(
            classify_paste(3, true, &default_cfg()),
            Some(PasteRisk::ControlChars)
        );
    }

    #[test]
    fn large_text_with_control_chars_reports_both_risks() {
        assert_eq!(
            classify_paste(20000, true, &default_cfg()),
            Some(PasteRisk::LargeAndControlChars)
        );
    }

    #[test]
    fn both_switches_off_is_the_full_rollback_path() {
        // PLAN_v1111 §7 回滚：两开关关闭 = 旧行为（直通）。
        let off = PasteGuardCfg {
            confirm_large: false,
            confirm_control_chars: false,
            size_threshold_kib: DEFAULT_PASTE_SIZE_THRESHOLD_KIB,
        };
        assert_eq!(classify_paste(usize::MAX, false, &off), None);
        assert_eq!(classify_paste(1 << 20, true, &off), None);
    }

    #[test]
    fn switch_combinations_select_independent_risk_classes() {
        // Four-combination matrix from PLAN_v1111 §5.1 (large payload,
        // no control chars).
        let large_only = PasteGuardCfg {
            confirm_large: true,
            confirm_control_chars: false,
            size_threshold_kib: DEFAULT_PASTE_SIZE_THRESHOLD_KIB,
        };
        assert_eq!(
            classify_paste(20000, false, &large_only),
            Some(PasteRisk::Large)
        );
        // Control chars present but their switch is off → the large switch
        // alone decides; a small paste sails through.
        assert_eq!(classify_paste(4, true, &large_only), None);

        let control_only = PasteGuardCfg {
            confirm_large: false,
            confirm_control_chars: true,
            size_threshold_kib: DEFAULT_PASTE_SIZE_THRESHOLD_KIB,
        };
        assert_eq!(
            classify_paste(20000, false, &control_only),
            None,
            "large switch off → size never confirms"
        );
        assert_eq!(
            classify_paste(4, true, &control_only),
            Some(PasteRisk::ControlChars)
        );
    }

    // ── contains_dangerous_control_chars ───────────────────────────────

    #[test]
    fn tab_newline_cr_are_not_dangerous() {
        assert!(!contains_dangerous_control_chars("a\tb\nc\r"));
        assert!(!contains_dangerous_control_chars(""));
    }

    #[test]
    fn nul_esc_and_del_are_dangerous() {
        // PLAN_v1111 §5.1 matrix: \x00 / \x1b / \x7f each flagged.
        assert!(contains_dangerous_control_chars("a\u{0}b"));
        assert!(contains_dangerous_control_chars("echo\u{1b}[2J"));
        assert!(contains_dangerous_control_chars("x\u{7f}y"));
        // Other C0 controls (bell) are also covered by "byte < 0x20".
        assert!(contains_dangerous_control_chars("\u{7}"));
    }

    #[test]
    fn multibyte_utf8_never_false_positives_via_continuation_bytes() {
        // CJK continuation bytes live in 0x80..=0xBF — all ≥ 0x20, but this
        // guards against any future byte-wise rewrite of the check.
        let cjk = "中文漢字テスト🎉";
        assert!(!contains_dangerous_control_chars(cjk));
    }

    // ── paste_preview: char-boundary safety ────────────────────────────

    #[test]
    fn preview_short_text_is_verbatim_without_ellipsis() {
        assert_eq!(paste_preview("hello", 80), "hello");
        assert_eq!(paste_preview("", 80), "");
        // Exactly max_chars → fits, no ellipsis.
        assert_eq!(paste_preview("abcde", 5), "abcde");
    }

    #[test]
    fn preview_truncates_on_char_boundary_for_cjk_and_emoji() {
        let cjk = "中".repeat(100);
        assert_eq!(paste_preview(&cjk, 80), format!("{}…", "中".repeat(80)));
        // Emoji are multi-byte single chars — a byte-wise cut would panic.
        let emoji = "🎉".repeat(90);
        let preview = paste_preview(&emoji, 80);
        assert_eq!(preview.chars().count(), 81);
        assert!(preview.ends_with('…'));
    }

    // ── format_byte_count: every magnitude band ────────────────────────

    #[test]
    fn byte_band_renders_whole_numbers() {
        assert_eq!(format_byte_count(0), "0 B");
        assert_eq!(format_byte_count(512), "512 B");
        assert_eq!(format_byte_count(1023), "1023 B");
    }

    #[test]
    fn kib_band_rounds_to_one_decimal_and_strips_trailing_zero() {
        // PLAN_v1111 §4.1 examples.
        assert_eq!(
            format_byte_count(16 * 1024),
            "16 KiB",
            "whole numbers lose '.0'"
        );
        assert_eq!(format_byte_count(16000), "15.6 KiB");
    }

    #[test]
    fn mib_band_and_beyond() {
        assert_eq!(format_byte_count(1258291), "1.2 MiB");
        assert_eq!(format_byte_count(2 * 1024 * 1024), "2 MiB");
        assert_eq!(format_byte_count(3 * 1024 * 1024 * 1024), "3 GiB");
    }

    // ── encode_paste: bracketed-paste injection stripping (audit VULN-002)

    /// Payload between the bracketed wrapper markers.
    fn bracketed_payload(out: &[u8]) -> String {
        let start = bracketed_paste_start().len();
        let end = out.len() - bracketed_paste_end().len();
        String::from_utf8_lossy(&out[start..end]).into_owned()
    }

    #[test]
    fn bracketed_paste_strips_forged_terminator_esc() {
        // The only ESC bytes in the output belong to the 200~/201~ wrapper;
        // the payload's forged `\x1b[201~` loses its ESC, so the leftover
        // "[201~" text stays inert inside bracketed mode.
        let out = encode_paste("\x1b[201~echo pwned", true);
        let mut expected = bracketed_paste_start();
        expected.extend_from_slice(b"[201~echo pwned");
        expected.extend(bracketed_paste_end());
        assert_eq!(out, expected);
    }

    #[test]
    fn bracketed_paste_preserves_tab_newline_cr_and_multibyte() {
        let text = "a\tb\nc\rd 中文🎉";
        let out = encode_paste(text, true);
        assert_eq!(
            bracketed_payload(&out),
            text,
            "\\t\\n\\r and CJK/emoji are exempt"
        );
    }

    #[test]
    fn bracketed_paste_strips_del_and_other_c0() {
        let out = encode_paste("x\u{7f}y\u{7}z", true);
        assert_eq!(
            bracketed_payload(&out),
            "xyz",
            "DEL (0x7f) and bell must go"
        );
    }

    #[test]
    fn all_control_payload_reduces_to_bare_wrapper() {
        let text: String = (0u8..0x20)
            .filter(|&b| b != b'\t' && b != b'\n' && b != b'\r')
            .chain(std::iter::once(0x7f))
            .map(|b| b as char)
            .collect();
        let out = encode_paste(&text, true);
        let mut expected = bracketed_paste_start();
        expected.extend(bracketed_paste_end());
        assert_eq!(out, expected, "empty payload → only 200~/201~ wrapper");
    }

    #[test]
    fn bracketed_empty_payload_is_bare_wrapper() {
        let mut expected = bracketed_paste_start();
        expected.extend(bracketed_paste_end());
        assert_eq!(encode_paste("", true), expected);
    }

    #[test]
    fn strip_result_never_still_classifies_dangerous() {
        // Consistency contract: the strip fn is the "peeled" twin of
        // contains_dangerous_control_chars — their \t\n\r exemption sets
        // must be identical, so a stripped payload can never reclassify.
        let samples = [
            "\x1b[201~echo pwned",
            "a\tb\nc\rd",
            "\u{7}\u{0}\u{7f}",
            "中文🎉\x1b",
        ];
        for t in samples {
            let stripped = strip_bracketed_paste_controls(t);
            assert!(
                !contains_dangerous_control_chars(&stripped),
                "stripped {t:?} still contains dangerous controls: {stripped:?}"
            );
        }
    }

    #[test]
    fn strip_is_identity_on_plain_text_with_exempt_controls() {
        assert_eq!(
            strip_bracketed_paste_controls("hello 中文\tworld\nline2\r"),
            "hello 中文\tworld\nline2\r"
        );
    }

    #[test]
    fn non_bracketed_paste_is_byte_verbatim() {
        // Rollback anchor (PLAN_v1111 §7): the explicit-risk non-bracketed
        // path stays untouched — bytes in, bytes out.
        let text = "\x1b[201~echo pwned\r\x7f中文";
        assert_eq!(encode_paste(text, false), text.as_bytes().to_vec());
    }
}
