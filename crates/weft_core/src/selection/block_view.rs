use super::BlockViewSelection;

/// Extract a selection whose endpoints belong to wrapped chunks of one
/// source line. Renderer snapshots store chunks in source order, while the
/// legacy block selection walker orders independent rows bottom-to-top. The
/// source offsets are therefore the only correct ordering key here.
pub(super) fn wrapped_source_text(selection: &BlockViewSelection) -> Option<String> {
    if selection.start.row_index == selection.end.row_index {
        return None;
    }
    let start_row = selection.rows.get(selection.start.row_index)?;
    let end_row = selection.rows.get(selection.end.row_index)?;
    let block_id = start_row.block_id?;
    let line = start_row.line?;
    if end_row.block_id != Some(block_id) || end_row.line != Some(line) {
        return None;
    }

    let start = start_row.chunk_char_offset
        + selection
            .start
            .char_index
            .min(start_row.text.chars().count());
    let end =
        end_row.chunk_char_offset + selection.end.char_index.min(end_row.text.chars().count());
    let (lo, hi) = (start.min(end), start.max(end));

    let mut chunks: Vec<_> = selection
        .rows
        .iter()
        .filter(|row| row.block_id == Some(block_id) && row.line == Some(line))
        .collect();
    chunks.sort_unstable_by_key(|row| row.chunk_char_offset);
    let source: String = chunks.iter().map(|row| row.text.as_str()).collect();
    Some(
        source
            .chars()
            .skip(lo)
            .take(hi.saturating_sub(lo))
            .collect(),
    )
}
