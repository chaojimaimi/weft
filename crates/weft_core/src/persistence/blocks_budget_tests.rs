//! v1.13.12 FIX-G (D3): styled-output persistence budget tests, split out
//! of the inline `mod tests` in `blocks.rs` to keep that file under the
//! architecture gate's 800-line budget (same `#[path]` pattern as
//! `paint/block_view/b_path_tests.rs`). Helpers come from `mod tests`.

use super::tests::{block, temp_store};
use crate::blocks::BlockId;
use crate::persistence::blocks::MAX_STYLED_OUTPUT_JSON_BYTES;

/// A styled payload of roughly `lines * ~6 KiB` of JSON — the per-line
/// 100 Rgb spans each serialize to ~60 bytes.
fn big_styled_output(lines: usize) -> crate::blocks::StyledOutput {
    crate::blocks::StyledOutput {
        lines: (0..lines)
            .map(|i| crate::blocks::StyledLine {
                line: i as u32,
                foregrounds: (0..100)
                    .map(|j| crate::blocks::ForegroundSpan {
                        start: j * 10,
                        end: j * 10 + 5,
                        color: crate::grid::CellColor::Rgb(crate::grid::Color::rgb(
                            0xAB, 0xCD, 0xEF,
                        )),
                    })
                    .collect(),
                backgrounds: Vec::new(),
                links: Vec::new(),
                attributes: Vec::new(),
                underline_colors: Vec::new(),
            })
            .collect(),
    }
}

/// FIX-G acceptance: a block whose styled JSON lands in the old-capped
/// band (256KiB, 1MiB] keeps its styling across a store re-open (the
/// pre-FIX behavior silently degraded it to plain text).
#[test]
fn styled_output_between_256kib_and_1mib_survives_restart() {
    let store = temp_store();
    let styled = big_styled_output(70);
    let json_len = serde_json::to_string(&styled).unwrap().len();
    assert!(
        json_len > 256 * 1024,
        "fixture must exceed the OLD 256KiB cap: {json_len}"
    );
    assert!(
        json_len <= MAX_STYLED_OUTPUT_JSON_BYTES,
        "fixture must fit the NEW cap: {json_len}"
    );
    let mut b = block(31, "big", "out", Some(0));
    b.styled_output = Some(std::sync::Arc::new(styled.clone()));
    store.insert(&b).unwrap();

    // Re-open (simulating restart) and read back.
    let path = std::path::Path::new(store.conn.path().expect("temp store is file-backed"));
    let reopened = super::super::BlockStore::open(path).expect("reopen store");
    let loaded = reopened.get(BlockId(31)).unwrap().expect("row present");
    assert_eq!(
        loaded.styled_output,
        Some(std::sync::Arc::new(styled)),
        "styled survives a restart in the (256KiB, 1MiB] band"
    );
}

/// FIX-G upper bound: over 1 MiB still degrades to plain text (the
/// runaway-buffer guard keeps working).
#[test]
fn styled_output_over_1mib_still_degrades_to_plain_text() {
    let store = temp_store();
    let styled = big_styled_output(400);
    let json_len = serde_json::to_string(&styled).unwrap().len();
    assert!(
        json_len > MAX_STYLED_OUTPUT_JSON_BYTES,
        "fixture must exceed the new cap: {json_len}"
    );
    let mut b = block(32, "huge", "out", Some(0));
    b.styled_output = Some(std::sync::Arc::new(styled));
    store.insert(&b).unwrap();
    let loaded = store.get(BlockId(32)).unwrap().expect("row present");
    assert_eq!(loaded.output.as_ref(), "out", "plain text survives");
    assert_eq!(
        loaded.styled_output, None,
        "over-budget styled output still degrades"
    );
}
