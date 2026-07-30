//! v1.7.4 Phase A: Kitty Graphics protocol parser POC.
//!
//! V17_IMPLEMENTATION_PLAN §6 Phase A decision gate. This module is a
//! **pure-logic POC** that parses Kitty Graphics APC payloads
//! (`ESC _ G <key=value>;<payload> ESC \`) with hard upper bounds. It does
//! NOT touch the filesystem, does NOT decode images, and does NOT allocate
//! Metal textures — it only validates that the payload can be safely parsed
//! within the resource envelope defined by the Phase A gate.
//!
//! ## Hard limits (V17 §6 Phase A gate conditions)
//!
//! | Limit | Value | Rationale |
//! |-------|-------|-----------|
//! | `MAX_APC_PAYLOAD_BYTES` | 256 KiB | Single APC chunk cap; Kitty chunks are typically 4-64 KiB |
//! | `MAX_IMAGE_WIDTH` | 4096 px | Single-image width cap |
//! | `MAX_IMAGE_HEIGHT` | 4096 px | Single-image height cap |
//! | `MAX_TOTAL_DECODED_BYTES` | 64 MiB | Total decoded RGBA memory across all images |
//!
//! ## Phase A findings (see `docs/V17_IMPLEMENTATION_PLAN.md` §6)
//!
//! - vte 0.13.1 routes `ESC _` into `SosPmApcString` state, which uses
//!   `Ignore` action for ALL payload bytes — they are silently dropped and
//!   never dispatched to any `Perform` method.
//! - There is no `apc_dispatch` in vte 0.13.1's `Perform` trait.
//! - APC does NOT break OSC/DCS/UTF-8/print — confirmed by replay fixtures
//!   in `tests/replay_fixtures.rs`.
//! - To intercept APC, a **bounded pre-parser** is required (see
//!   `ApcPreParser` in this module).

use std::collections::HashMap;

/// v1.7.4 Phase A: Hard limit on a single APC payload size (256 KiB).
/// Kitty Graphics chunks are typically 4-64 KiB; 256 KiB provides headroom
/// for base64-encoded 192 KiB raw payloads.
pub const MAX_APC_PAYLOAD_BYTES: usize = 256 * 1024;

/// v1.7.4 Phase A: Maximum single-image width in pixels.
pub const MAX_IMAGE_WIDTH: u32 = 4096;

/// v1.7.4 Phase A: Maximum single-image height in pixels.
pub const MAX_IMAGE_HEIGHT: u32 = 4096;

/// v1.7.4 Phase A: Maximum total decoded RGBA memory across all images
/// (64 MiB). At 4 bytes/px this allows ~16K full-screen 1024x1024 images
/// or a smaller number of larger images.
pub const MAX_TOTAL_DECODED_BYTES: usize = 64 * 1024 * 1024;

/// v1.7.4 Phase A: Kitty Graphics APC command verb. Kitty uses `G` as the
/// identifier byte after `ESC _`.
pub const KITTY_GRAPHICS_VERB: u8 = b'G';

/// v1.7.4 Phase A: Parsed Kitty Graphics command.
///
/// This is the output of parsing a single APC payload. It does NOT include
/// the decoded image data — only metadata and the raw (still-encoded)
/// payload bytes. Image decoding happens in Phase B.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyGraphicsCommand {
    /// The `a` (action) key value: `T` (transmit), `q` (query), `t` (transmit
    /// and display), `d` (display), `D` (delete), `f` (frame), `p` (place),
    /// `A` (animate), `c` (configure).
    pub action: Option<u8>,
    /// The `f` (format) key value: `24` (RGB), `32` (RGBA), `100` (PNG).
    pub format: Option<u32>,
    /// The `s` (width) key value in pixels.
    pub width: Option<u32>,
    /// The `v` (height) key value in pixels.
    pub height: Option<u32>,
    /// The `i` (image id) key value.
    pub image_id: Option<u32>,
    /// The `d` (delete) key value for delete commands.
    pub delete_action: Option<u8>,
    /// Raw payload bytes after the `;` separator (still encoded — base64 for
    /// PNG/RGBA, raw for compressed). Capped at `MAX_APC_PAYLOAD_BYTES`.
    pub payload: Vec<u8>,
}

/// v1.7.4 Phase A: Errors that can occur during APC payload parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KittyGraphicsError {
    /// Payload exceeds `MAX_APC_PAYLOAD_BYTES`.
    PayloadTooLarge { size: usize, max: usize },
    /// A key's value exceeded the allowed maximum (e.g., width > 4096).
    DimensionExceeded {
        key: &'static str,
        value: u64,
        max: u64,
    },
    /// The APC payload doesn't start with the `G` verb.
    InvalidVerb { got: u8 },
    /// A key's value wasn't a valid integer.
    InvalidInteger { key: &'static str },
    /// Malformed key=value syntax.
    MalformedKey,
    /// Total decoded memory would exceed `MAX_TOTAL_DECODED_BYTES`.
    TotalMemoryExceeded { would_be: usize, max: usize },
}

/// v1.7.4 Phase A: Parse a single Kitty Graphics APC payload.
///
/// Input: the bytes between `ESC _` and `ST` (not including the introducer
/// or terminator). The first byte must be `G` (the Kitty verb).
///
/// This is a **pure function** — no I/O, no side effects, no allocations
/// beyond the returned `Vec<u8>` for the payload.
///
/// Returns `Ok(command)` if the payload is well-formed and within limits,
/// `Err(error)` otherwise. On error, the caller should discard the payload
/// and continue parsing (the VT state machine remains valid).
pub fn parse_kitty_graphics_apc(
    apc_bytes: &[u8],
) -> Result<KittyGraphicsCommand, KittyGraphicsError> {
    // --- Step 1: Verify the verb byte ---
    if apc_bytes.is_empty() {
        return Err(KittyGraphicsError::InvalidVerb { got: 0 });
    }
    if apc_bytes[0] != KITTY_GRAPHICS_VERB {
        return Err(KittyGraphicsError::InvalidVerb { got: apc_bytes[0] });
    }

    // --- Step 2: Split key=value pairs and payload at ';' ---
    // Format: G key1=val1,key2=val2,...;payload
    // (Note: there's a space between G and the first key in Kitty's spec.)
    // The payload is everything after the first ';'.
    let after_verb = &apc_bytes[1..];
    // Skip the mandatory space after 'G' (Kitty spec: "G " prefix).
    let after_verb_trimmed = after_verb
        .iter()
        .copied()
        .skip_while(|b| b.is_ascii_whitespace())
        .collect::<Vec<u8>>();
    let (kv_section, payload) = split_at_first_semicolon(&after_verb_trimmed);

    // --- Step 3: Enforce payload size limit ---
    if payload.len() > MAX_APC_PAYLOAD_BYTES {
        return Err(KittyGraphicsError::PayloadTooLarge {
            size: payload.len(),
            max: MAX_APC_PAYLOAD_BYTES,
        });
    }

    // --- Step 4: Parse key=value pairs ---
    let mut keys: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    if !kv_section.is_empty() {
        for pair in kv_section.split(|&b| b == b',') {
            if pair.is_empty() {
                continue;
            }
            let (k, v) = split_at_first_equals(pair);
            if k.is_empty() {
                return Err(KittyGraphicsError::MalformedKey);
            }
            keys.insert(k.to_vec(), v.to_vec());
        }
    }

    // --- Step 5: Extract and validate known keys ---
    let action = keys.get(b"a".as_slice()).and_then(|v| v.first().copied());
    let format = parse_u32_key(&keys, "f")?;
    let width = parse_u32_key(&keys, "s")?;
    let height = parse_u32_key(&keys, "v")?;
    let image_id = parse_u32_key(&keys, "i")?;
    let delete_action = keys.get(b"d".as_slice()).and_then(|v| v.first().copied());

    // Validate dimensions against hard limits.
    if let Some(w) = width {
        if w > MAX_IMAGE_WIDTH {
            return Err(KittyGraphicsError::DimensionExceeded {
                key: "s (width)",
                value: w as u64,
                max: MAX_IMAGE_WIDTH as u64,
            });
        }
    }
    if let Some(h) = height {
        if h > MAX_IMAGE_HEIGHT {
            return Err(KittyGraphicsError::DimensionExceeded {
                key: "v (height)",
                value: h as u64,
                max: MAX_IMAGE_HEIGHT as u64,
            });
        }
    }

    // --- Step 6: Validate total decoded memory (if width/height/format known) ---
    if let (Some(w), Some(h), Some(fmt)) = (width, height, format) {
        let bytes_per_pixel = match fmt {
            24 => 3, // RGB
            32 => 4, // RGBA
            _ => 0,  // PNG — size only known after decode; skip pre-check
        };
        if bytes_per_pixel > 0 {
            let decoded_size = (w as usize)
                .checked_mul(h as usize)
                .and_then(|px| px.checked_mul(bytes_per_pixel))
                .unwrap_or(usize::MAX);
            if decoded_size > MAX_TOTAL_DECODED_BYTES {
                return Err(KittyGraphicsError::TotalMemoryExceeded {
                    would_be: decoded_size,
                    max: MAX_TOTAL_DECODED_BYTES,
                });
            }
        }
    }

    Ok(KittyGraphicsCommand {
        action,
        format,
        width,
        height,
        image_id,
        delete_action,
        payload: payload.to_vec(),
    })
}

/// Split a byte slice at the first `;`, returning (before, after).
/// If no `;` is found, returns (input, empty).
fn split_at_first_semicolon(bytes: &[u8]) -> (&[u8], &[u8]) {
    match bytes.iter().position(|&b| b == b';') {
        Some(idx) => (&bytes[..idx], &bytes[idx + 1..]),
        None => (bytes, &[]),
    }
}

/// Split a byte slice at the first `=`, returning (key, value).
/// If no `=` is found, returns (input, empty).
fn split_at_first_equals(bytes: &[u8]) -> (&[u8], &[u8]) {
    match bytes.iter().position(|&b| b == b'=') {
        Some(idx) => (&bytes[..idx], &bytes[idx + 1..]),
        None => (bytes, &[]),
    }
}

/// Parse a u32 value for a known key, with dimension validation.
fn parse_u32_key(
    keys: &HashMap<Vec<u8>, Vec<u8>>,
    key_name: &'static str,
) -> Result<Option<u32>, KittyGraphicsError> {
    let key = key_name.as_bytes();
    let Some(val) = keys.get(key) else {
        return Ok(None);
    };
    let s = std::str::from_utf8(val)
        .map_err(|_| KittyGraphicsError::InvalidInteger { key: key_name })?;
    let n: u32 = s
        .parse()
        .map_err(|_| KittyGraphicsError::InvalidInteger { key: key_name })?;
    Ok(Some(n))
}

// ---------------------------------------------------------------------------
// v1.7.4 Phase A: Bounded APC pre-parser
// ---------------------------------------------------------------------------

/// v1.7.4 Phase A: State for a bounded APC pre-parser.
///
/// Since vte 0.13.1 silently drops APC payload bytes (the `SosPmApcString`
/// state uses `Ignore` action), a pre-parser is required to intercept APC
/// before feeding bytes to vte. This struct implements a minimal state
/// machine that:
///
/// 1. Scans for `ESC _` (0x1b 0x5f) in the byte stream.
/// 2. Collects bytes until ST (`ESC \`) is seen.
/// 3. Enforces `MAX_APC_PAYLOAD_BYTES` — oversized payloads are discarded.
/// 4. Passes through all non-APC bytes unchanged.
///
/// The pre-parser is **conservative**: it only intercepts `ESC _` and leaves
/// all other escape sequences (OSC, DCS, CSI, ESC dispatch) to vte. It
/// correctly handles `ESC` inside UTF-8 sequences because it only enters
/// APC-collection mode on the exact 2-byte sequence `ESC _`.
///
/// ## State machine
///
/// ```text
/// PassThrough ──ESC _──> CollectApc
/// PassThrough ──ESC(other)──> PassThrough (ESC passed through)
/// CollectApc ──ESC \──> PassThrough (APC complete, dispatch)
/// CollectApc ──ESC(other)──> CollectApc (ESC stored, continue collecting)
/// CollectApc ──byte──> CollectApc (byte stored)
/// CollectApc ──overflow──> CollectApc (excess bytes dropped until ST)
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApcPreParserState {
    /// Normal pass-through mode: bytes go to vte unchanged.
    PassThrough,
    /// Just saw `ESC` (0x1b) in pass-through — need next byte to decide.
    SawEsc,
    /// Inside an APC payload, collecting bytes until ST.
    /// The accumulated payload is in the `Vec<u8>`.
    CollectApc {
        payload: Vec<u8>,
        saw_esc_inside: bool,
    },
}

/// v1.7.4 Phase A: Result of processing a chunk through the pre-parser.
#[derive(Debug, Default)]
pub struct ApcPreParserOutput {
    /// Bytes to feed to vte (non-APC bytes, passed through unchanged).
    pub passthrough: Vec<u8>,
    /// Completed APC payloads (one per ST-terminated APC sequence).
    /// Each entry is the raw bytes between `ESC _` and `ST` (excluding
    /// the introducer and terminator).
    pub apc_payloads: Vec<Vec<u8>>,
}

/// v1.7.4 Phase A: Process a chunk of PTY bytes through the APC pre-parser.
///
/// This is the main entry point. It takes a chunk of bytes and the current
/// parser state, and returns the pass-through bytes (for vte) plus any
/// completed APC payloads (for the Kitty Graphics handler).
///
/// The state is returned so the caller can continue across chunk boundaries
/// (PTY reads can split an APC sequence mid-payload).
pub fn process_apc_pre_parser(
    state: ApcPreParserState,
    input: &[u8],
) -> (ApcPreParserState, ApcPreParserOutput) {
    let mut output = ApcPreParserOutput::default();
    let mut current = state;

    for &byte in input {
        current = process_one_byte(current, byte, &mut output);
    }

    (current, output)
}

fn process_one_byte(
    state: ApcPreParserState,
    byte: u8,
    output: &mut ApcPreParserOutput,
) -> ApcPreParserState {
    match state {
        ApcPreParserState::PassThrough => {
            if byte == 0x1b {
                ApcPreParserState::SawEsc
            } else {
                output.passthrough.push(byte);
                ApcPreParserState::PassThrough
            }
        }
        ApcPreParserState::SawEsc => {
            if byte == 0x5f {
                // ESC _ — enter APC collection mode.
                ApcPreParserState::CollectApc {
                    payload: Vec::new(),
                    saw_esc_inside: false,
                }
            } else {
                // ESC followed by non-_ — pass both bytes through to vte.
                output.passthrough.push(0x1b);
                output.passthrough.push(byte);
                ApcPreParserState::PassThrough
            }
        }
        ApcPreParserState::CollectApc {
            mut payload,
            saw_esc_inside,
        } => {
            if saw_esc_inside {
                // Previous byte was ESC inside APC.
                if byte == 0x5c {
                    // ESC \ — ST terminator. APC is complete.
                    output.apc_payloads.push(std::mem::take(&mut payload));
                    ApcPreParserState::PassThrough
                } else {
                    // ESC followed by non-\ — the ESC was part of the
                    // payload (unusual but valid). Store both bytes.
                    if payload.len() + 2 <= MAX_APC_PAYLOAD_BYTES {
                        payload.push(0x1b);
                        payload.push(byte);
                    }
                    // If overflow, silently drop (payload stays capped).
                    ApcPreParserState::CollectApc {
                        payload,
                        saw_esc_inside: false,
                    }
                }
            } else if byte == 0x1b {
                // ESC inside APC — might be start of ST.
                ApcPreParserState::CollectApc {
                    payload,
                    saw_esc_inside: true,
                }
            } else {
                // Regular byte inside APC.
                if payload.len() < MAX_APC_PAYLOAD_BYTES {
                    payload.push(byte);
                }
                // If overflow, silently drop excess bytes (payload stays
                // capped; the APC is still consumed until ST).
                ApcPreParserState::CollectApc {
                    payload,
                    saw_esc_inside: false,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- KittyGraphicsCommand parsing tests ----

    #[test]
    fn parse_simple_transmit_command() {
        let apc = b"G a=T,f=24,s=2,v=2;iVBORw0KG";
        let cmd = parse_kitty_graphics_apc(apc).unwrap();
        assert_eq!(cmd.action, Some(b'T'));
        assert_eq!(cmd.format, Some(24));
        assert_eq!(cmd.width, Some(2));
        assert_eq!(cmd.height, Some(2));
        assert_eq!(cmd.payload, b"iVBORw0KG");
    }

    #[test]
    fn parse_delete_command() {
        let apc = b"G a=d,d=A,i=42";
        let cmd = parse_kitty_graphics_apc(apc).unwrap();
        assert_eq!(cmd.action, Some(b'd'));
        assert_eq!(cmd.delete_action, Some(b'A'));
        assert_eq!(cmd.image_id, Some(42));
        assert!(cmd.payload.is_empty());
    }

    #[test]
    fn parse_query_command_no_payload() {
        let apc = b"G a=q";
        let cmd = parse_kitty_graphics_apc(apc).unwrap();
        assert_eq!(cmd.action, Some(b'q'));
    }

    #[test]
    fn parse_invalid_verb_rejected() {
        let result = parse_kitty_graphics_apc(b"X a=T");
        assert_eq!(result, Err(KittyGraphicsError::InvalidVerb { got: b'X' }));
    }

    #[test]
    fn parse_empty_apc_rejected() {
        let result = parse_kitty_graphics_apc(b"");
        assert_eq!(result, Err(KittyGraphicsError::InvalidVerb { got: 0 }));
    }

    #[test]
    fn parse_width_exceeds_limit_rejected() {
        let apc = format!("G a=T,f=24,s={},v=1", MAX_IMAGE_WIDTH + 1).into_bytes();
        let result = parse_kitty_graphics_apc(&apc);
        assert!(matches!(
            result,
            Err(KittyGraphicsError::DimensionExceeded {
                key: "s (width)",
                ..
            })
        ));
    }

    #[test]
    fn parse_height_exceeds_limit_rejected() {
        let apc = format!("G a=T,f=24,s=1,v={}", MAX_IMAGE_HEIGHT + 1).into_bytes();
        let result = parse_kitty_graphics_apc(&apc);
        assert!(matches!(
            result,
            Err(KittyGraphicsError::DimensionExceeded {
                key: "v (height)",
                ..
            })
        ));
    }

    #[test]
    fn parse_total_memory_at_limit_accepted() {
        // 4096x4096 RGBA = 64 MiB exactly — at the limit, should pass.
        let apc = b"G a=T,f=32,s=4096,v=4096";
        let result = parse_kitty_graphics_apc(apc);
        assert!(result.is_ok(), "4096x4096 RGBA = 64MiB should be at limit");

        // 4096x4096 RGB (24-bit) = 48 MiB — well under limit.
        let apc = b"G a=T,f=24,s=4096,v=4096";
        let result = parse_kitty_graphics_apc(apc);
        assert!(result.is_ok());
    }

    #[test]
    fn parse_png_format_skips_memory_check() {
        // PNG format (100) — size unknown until decode, skip pre-check.
        let apc = b"G a=T,f=100,s=4096,v=4096";
        let result = parse_kitty_graphics_apc(apc);
        assert!(result.is_ok());
    }

    #[test]
    fn parse_invalid_integer_rejected() {
        let apc = b"G a=T,f=abc";
        let result = parse_kitty_graphics_apc(apc);
        assert!(matches!(
            result,
            Err(KittyGraphicsError::InvalidInteger { key: "f" })
        ));
    }

    #[test]
    fn parse_payload_at_limit_accepted() {
        let payload = "X".repeat(MAX_APC_PAYLOAD_BYTES);
        let apc = format!("G a=T;{}", payload).into_bytes();
        let result = parse_kitty_graphics_apc(&apc);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().payload.len(), MAX_APC_PAYLOAD_BYTES);
    }

    #[test]
    fn parse_payload_over_limit_rejected() {
        let payload = "X".repeat(MAX_APC_PAYLOAD_BYTES + 1);
        let apc = format!("G a=T;{}", payload).into_bytes();
        let result = parse_kitty_graphics_apc(&apc);
        assert!(matches!(
            result,
            Err(KittyGraphicsError::PayloadTooLarge { .. })
        ));
    }

    #[test]
    fn parse_malformed_key_rejected() {
        let apc = b"G =val";
        let result = parse_kitty_graphics_apc(apc);
        assert_eq!(result, Err(KittyGraphicsError::MalformedKey));
    }

    #[test]
    fn parse_extra_keys_ignored() {
        let apc = b"G a=T,f=24,s=1,v=1,x=unknown,y=999";
        let cmd = parse_kitty_graphics_apc(apc).unwrap();
        assert_eq!(cmd.action, Some(b'T'));
        assert_eq!(cmd.format, Some(24));
    }

    // ---- ApcPreParser tests ----

    #[test]
    fn pre_parser_passthrough_normal_text() {
        let (state, output) =
            process_apc_pre_parser(ApcPreParserState::PassThrough, b"hello world");
        assert_eq!(state, ApcPreParserState::PassThrough);
        assert_eq!(output.passthrough, b"hello world");
        assert!(output.apc_payloads.is_empty());
    }

    #[test]
    fn pre_parser_intercepts_apc() {
        let input = b"before\x1b_G a=T\x1b\\after";
        let (state, output) = process_apc_pre_parser(ApcPreParserState::PassThrough, input);
        assert_eq!(state, ApcPreParserState::PassThrough);
        assert_eq!(output.passthrough, b"beforeafter");
        assert_eq!(output.apc_payloads.len(), 1);
        assert_eq!(output.apc_payloads[0], b"G a=T");
    }

    #[test]
    fn pre_parser_passes_osc_through() {
        let input = b"\x1b]133;A\x07hello";
        let (state, output) = process_apc_pre_parser(ApcPreParserState::PassThrough, input);
        assert_eq!(state, ApcPreParserState::PassThrough);
        assert_eq!(output.passthrough, input);
        assert!(output.apc_payloads.is_empty());
    }

    #[test]
    fn pre_parser_passes_dcs_through() {
        let input = b"\x1bPdcs\x1b\\after";
        let (state, output) = process_apc_pre_parser(ApcPreParserState::PassThrough, input);
        assert_eq!(state, ApcPreParserState::PassThrough);
        assert_eq!(output.passthrough, input);
        assert!(output.apc_payloads.is_empty());
    }

    #[test]
    fn pre_parser_passes_csi_through() {
        let input = b"\x1b[31mred\x1b[0m";
        let (state, output) = process_apc_pre_parser(ApcPreParserState::PassThrough, input);
        assert_eq!(state, ApcPreParserState::PassThrough);
        assert_eq!(output.passthrough, input);
        assert!(output.apc_payloads.is_empty());
    }

    #[test]
    fn pre_parser_esc_alone_passes_through() {
        // ESC not followed by _ should pass through.
        let (state, output) = process_apc_pre_parser(ApcPreParserState::PassThrough, b"\x1b[31m");
        assert_eq!(state, ApcPreParserState::PassThrough);
        assert_eq!(output.passthrough, b"\x1b[31m");
    }

    #[test]
    fn pre_parser_multiple_apcs() {
        let input = b"\x1b_G a=T\x1b\\\x1b_G a=d\x1b\\text";
        let (state, output) = process_apc_pre_parser(ApcPreParserState::PassThrough, input);
        assert_eq!(state, ApcPreParserState::PassThrough);
        assert_eq!(output.passthrough, b"text");
        assert_eq!(output.apc_payloads.len(), 2);
        assert_eq!(output.apc_payloads[0], b"G a=T");
        assert_eq!(output.apc_payloads[1], b"G a=d");
    }

    #[test]
    fn pre_parser_apc_split_across_chunks() {
        // First chunk: ESC _ G a=T (no ST yet)
        let (state1, output1) =
            process_apc_pre_parser(ApcPreParserState::PassThrough, b"\x1b_G a=T");
        assert!(matches!(state1, ApcPreParserState::CollectApc { .. }));
        assert!(output1.passthrough.is_empty());
        assert!(output1.apc_payloads.is_empty());

        // Second chunk: ST + after
        let (state2, output2) = process_apc_pre_parser(state1, b"\x1b\\after");
        assert_eq!(state2, ApcPreParserState::PassThrough);
        assert_eq!(output2.passthrough, b"after");
        assert_eq!(output2.apc_payloads.len(), 1);
        assert_eq!(output2.apc_payloads[0], b"G a=T");
    }

    #[test]
    fn pre_parser_cjk_utf8_passthrough() {
        let input = "测试\x1b_G a=T\x1b\\后".as_bytes();
        let (state, output) = process_apc_pre_parser(ApcPreParserState::PassThrough, input);
        assert_eq!(state, ApcPreParserState::PassThrough);
        // CJK bytes should pass through unchanged (before and after APC).
        let expected = "测试后".as_bytes();
        assert_eq!(output.passthrough, expected);
        assert_eq!(output.apc_payloads.len(), 1);
    }

    #[test]
    fn pre_parser_oversized_payload_capped() {
        // Payload exceeding MAX_APC_PAYLOAD_BYTES should be capped (excess
        // bytes dropped) but the APC is still consumed until ST.
        let mut input = Vec::new();
        input.extend_from_slice(b"\x1b_G a=T;");
        input.extend(std::iter::repeat(b'X').take(MAX_APC_PAYLOAD_BYTES + 100));
        input.extend_from_slice(b"\x1b\\after");
        let (state, output) = process_apc_pre_parser(ApcPreParserState::PassThrough, &input);
        assert_eq!(state, ApcPreParserState::PassThrough);
        assert_eq!(output.passthrough, b"after");
        assert_eq!(output.apc_payloads.len(), 1);
        assert_eq!(
            output.apc_payloads[0].len(),
            MAX_APC_PAYLOAD_BYTES,
            "payload should be capped at MAX_APC_PAYLOAD_BYTES"
        );
    }

    #[test]
    fn pre_parser_esc_inside_apc_not_st() {
        // ESC inside APC followed by non-\ should be treated as payload.
        let input = b"\x1b_G payload\x1bXmore\x1b\\";
        let (state, output) = process_apc_pre_parser(ApcPreParserState::PassThrough, input);
        assert_eq!(state, ApcPreParserState::PassThrough);
        assert_eq!(output.apc_payloads.len(), 1);
        // The ESC X should be in the payload (unusual but valid).
        let payload = &output.apc_payloads[0];
        assert!(payload.starts_with(b"G payload"));
        assert!(payload.ends_with(b"more"));
    }

    #[test]
    fn pre_parser_no_apc_in_input() {
        let input = b"just plain text with \x1b[31m colors\x1b[0m";
        let (state, output) = process_apc_pre_parser(ApcPreParserState::PassThrough, input);
        assert_eq!(state, ApcPreParserState::PassThrough);
        assert_eq!(output.passthrough, input);
        assert!(output.apc_payloads.is_empty());
    }

    // ---- Integration: pre-parser + KittyGraphicsCommand parser ----

    #[test]
    fn integration_pre_parser_to_kitty_parser() {
        let pty_bytes = b"text\x1b_G a=T,f=24,s=2,v=2;iVBORw0KG=\x1b\\more";
        let (state, output) = process_apc_pre_parser(ApcPreParserState::PassThrough, pty_bytes);
        assert_eq!(state, ApcPreParserState::PassThrough);
        assert_eq!(output.passthrough, b"textmore");
        assert_eq!(output.apc_payloads.len(), 1);

        let cmd = parse_kitty_graphics_apc(&output.apc_payloads[0]).unwrap();
        assert_eq!(cmd.action, Some(b'T'));
        assert_eq!(cmd.format, Some(24));
        assert_eq!(cmd.width, Some(2));
        assert_eq!(cmd.height, Some(2));
        assert_eq!(cmd.payload, b"iVBORw0KG=");
    }

    #[test]
    fn integration_oversized_apc_discarded_safely() {
        // An APC with an oversized payload should be capped by the
        // pre-parser, then rejected by the Kitty parser. The passthrough
        // bytes should be unaffected.
        let mut pty_bytes = Vec::new();
        pty_bytes.extend_from_slice(b"before\x1b_G a=T;");
        pty_bytes.extend(std::iter::repeat(b'X').take(MAX_APC_PAYLOAD_BYTES + 1));
        pty_bytes.extend_from_slice(b"\x1b\\after");

        let (state, output) = process_apc_pre_parser(ApcPreParserState::PassThrough, &pty_bytes);
        assert_eq!(state, ApcPreParserState::PassThrough);
        assert_eq!(output.passthrough, b"beforeafter");
        assert_eq!(output.apc_payloads.len(), 1);

        // The payload is capped at MAX_APC_PAYLOAD_BYTES, so the Kitty
        // parser should accept it (it's exactly at the limit).
        let result = parse_kitty_graphics_apc(&output.apc_payloads[0]);
        assert!(result.is_ok(), "capped payload should parse: {:?}", result);
    }
}
