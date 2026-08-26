//! Semantic exit-tail extraction, styling, and spacing.

use crate::blocks::{OutputCapture, StyledOutput};

pub(super) fn merge_primary_screen_interrupt_tail(
    frozen_text: String,
    mut frozen_styled: StyledOutput,
    tail: &OutputCapture,
) -> (String, StyledOutput) {
    let mut tail_snapshot = tail.clone();
    let (tail_text, tail_styled) = tail_snapshot.take_styled();
    let semantic = semantic_exit_tail(&tail_text);
    let leading_trimmed = semantic.trim_start_matches('\n');
    let semantic_start = tail_text.len().saturating_sub(semantic.len())
        + semantic.len().saturating_sub(leading_trimmed.len());
    let tail = leading_trimmed.trim_end_matches('\n');
    if tail.is_empty() {
        return (frozen_text, frozen_styled);
    }

    let frozen = frozen_text.trim_end_matches('\n');
    let frozen_line_count = frozen.lines().count();
    let separator_line_count = usize::from(!frozen.is_empty());
    let tail_start_line = tail_text[..semantic_start]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count();
    let tail_line_count = tail.split('\n').count();
    if let Some(styled) = tail_styled {
        frozen_styled.lines.extend(
            styled
                .lines
                .into_iter()
                .filter(|line| {
                    let line = line.line as usize;
                    tail_start_line <= line && line < tail_start_line + tail_line_count
                })
                .map(|mut line| {
                    line.line = line
                        .line
                        .saturating_sub(tail_start_line as u32)
                        .saturating_add(
                            frozen_line_count.saturating_add(separator_line_count) as u32
                        );
                    line
                }),
        );
    }
    let merged = if frozen.is_empty() {
        tail.to_owned()
    } else {
        format!("{frozen}\n\n{tail}")
    };
    space_primary_screen_exit_tail(merged, frozen_styled)
}

fn semantic_exit_tail(tail: &str) -> &str {
    let explicit = [
        "Press Ctrl-C again to exit",
        "Resume this session with:",
        "To resume this session:",
    ]
    .into_iter()
    .filter_map(|marker| line_marker_start(tail, marker))
    .min();
    let session_card = tail
        .match_indices("Session")
        .filter(|(start, _)| *start == 0 || tail.as_bytes().get(start - 1) == Some(&b'\n'))
        .find_map(|(start, _)| {
            tail[start..]
                .lines()
                .skip(1)
                .take(3)
                .any(|line| line.trim_start().starts_with("Continue"))
                .then_some(start)
        });
    explicit
        .into_iter()
        .chain(session_card)
        .min()
        .map_or(tail, |start| &tail[start..])
}

fn line_marker_start(text: &str, marker: &str) -> Option<usize> {
    text.match_indices(marker)
        .map(|(start, _)| start)
        .find(|&start| start == 0 || text.as_bytes().get(start - 1) == Some(&b'\n'))
}

pub(super) fn space_primary_screen_exit_tail(
    mut text: String,
    mut styled: StyledOutput,
) -> (String, StyledOutput) {
    if !text.contains("Press Ctrl-C again to exit")
        && !text.contains("Resume this session with:")
        && !text.contains("To resume this session:")
    {
        return (text, styled);
    }
    let compact_lines: Vec<&str> = text.split('\n').collect();
    if let Some(marker_index) = compact_lines.iter().position(|line| {
        let line = line.trim();
        line == "Press Ctrl-C again to exit"
            || line == "Resume this session with:"
            || line.starts_with("To resume this session:")
    }) {
        let mut blank_start = marker_index;
        while blank_start > 0 && compact_lines[blank_start - 1].trim().is_empty() {
            blank_start -= 1;
        }
        let blank_count = marker_index.saturating_sub(blank_start);
        if blank_count > 1 {
            let remove_end = marker_index - 1;
            let removed = remove_end - blank_start;
            let mut compacted = compact_lines;
            compacted.drain(blank_start..remove_end);
            text = compacted.join("\n");
            styled.lines.retain_mut(|line| {
                let original = line.line as usize;
                if (blank_start..remove_end).contains(&original) {
                    return false;
                }
                if original >= remove_end {
                    line.line = line.line.saturating_sub(removed as u32);
                }
                true
            });
        }
    }
    let lines: Vec<&str> = text.split('\n').collect();
    let insert_before: Vec<usize> = (1..lines.len())
        .filter(|&index| {
            let line = lines[index].trim();
            let semantic_tail = line == "Press Ctrl-C again to exit"
                || line == "Resume this session with:"
                || line.starts_with("To resume this session:");
            semantic_tail && !lines[index - 1].trim().is_empty()
        })
        .collect();
    if insert_before.is_empty() {
        drop(lines);
        return (text, styled);
    }

    let mut spaced = Vec::with_capacity(lines.len() + insert_before.len());
    for (index, line) in lines.into_iter().enumerate() {
        if insert_before.binary_search(&index).is_ok() {
            spaced.push("");
        }
        spaced.push(line);
    }
    for line in &mut styled.lines {
        let original = line.line as usize;
        let shift = insert_before.partition_point(|&index| index <= original);
        line.line = line.line.saturating_add(shift as u32);
    }
    (spaced.join("\n"), styled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::{CapturedStyle, StyledLine, MAX_OUTPUT_BYTES};
    use crate::grid::{CellColor, CellFlags, UnderlineStyle};

    fn empty_styled_line(line: u32) -> StyledLine {
        StyledLine {
            line,
            foregrounds: Vec::new(),
            backgrounds: Vec::new(),
            links: Vec::new(),
            attributes: Vec::new(),
            underline_colors: Vec::new(),
        }
    }

    #[test]
    fn exit_tail_spacing_shifts_parallel_style_line_indices() {
        let styled = StyledOutput {
            lines: (0..4).map(empty_styled_line).collect(),
        };
        let (text, styled) = space_primary_screen_exit_tail(
            "answer\nPress Ctrl-C again to exit\nResume this session with:\nclaude --resume id"
                .to_string(),
            styled,
        );

        assert_eq!(
            text,
            "answer\n\nPress Ctrl-C again to exit\n\nResume this session with:\nclaude --resume id"
        );
        assert_eq!(
            styled
                .lines
                .iter()
                .map(|line| line.line)
                .collect::<Vec<_>>(),
            [0, 2, 4, 5]
        );
    }

    #[test]
    fn exit_tail_spacing_compacts_coordinate_gap_before_generic_resume_hint() {
        let styled = StyledOutput {
            lines: [0, 4].into_iter().map(empty_styled_line).collect(),
        };
        let (text, styled) = space_primary_screen_exit_tail(
            "answer\n\n\n\nTo resume this session: agent --session id".into(),
            styled,
        );
        assert_eq!(text, "answer\n\nTo resume this session: agent --session id");
        assert_eq!(
            styled
                .lines
                .iter()
                .map(|line| line.line)
                .collect::<Vec<_>>(),
            [0, 2]
        );
    }

    #[test]
    fn semantic_tail_discards_repainted_banners_for_multiple_agents() {
        assert_eq!(
            semantic_exit_tail("repainted answer\nPress Ctrl-C again to exit\nResume this session with:\nclaude --resume id"),
            "Press Ctrl-C again to exit\nResume this session with:\nclaude --resume id"
        );
        assert_eq!(
            semantic_exit_tail("opencode banner\nSession   project\nContinue  opencode -s id"),
            "Session   project\nContinue  opencode -s id"
        );
        assert_eq!(
            semantic_exit_tail(
                "repainted footer\n\n\nTo resume this session: agent --session session-id"
            ),
            "To resume this session: agent --session session-id"
        );
    }

    #[test]
    fn primary_screen_resume_tail_compacts_gap_and_preserves_style_hierarchy() {
        let mut tail = OutputCapture::default();
        let plain = CapturedStyle::default();
        let dim = CapturedStyle::from_attrs(
            CellColor::Default,
            CellColor::Default,
            CellFlags::DIM,
            UnderlineStyle::Single,
            None,
        );
        tail.print_ascii(b"repainted footer", plain, MAX_OUTPUT_BYTES);
        for _ in 0..3 {
            tail.newline(MAX_OUTPUT_BYTES);
        }
        let label = "To resume this session:";
        tail.print_ascii(label.as_bytes(), dim, MAX_OUTPUT_BYTES);
        tail.print_ascii(b" agent --session session-id\n", plain, MAX_OUTPUT_BYTES);

        let (text, styled) =
            merge_primary_screen_interrupt_tail("answer".into(), StyledOutput::default(), &tail);
        assert_eq!(
            text,
            "answer\n\nTo resume this session: agent --session session-id"
        );
        let line = styled.line(2).expect("resume line keeps captured style");
        assert_eq!(line.attributes_at(0), CellFlags::DIM);
        assert_eq!(
            line.attributes_at(label.chars().count() + 1),
            CellFlags::empty(),
            "resume command remains brighter than its dim label"
        );
    }

    #[test]
    fn semantic_tail_without_frozen_document_has_no_leading_gap() {
        let mut tail = OutputCapture::default();
        let dim = CapturedStyle::from_attrs(
            CellColor::Default,
            CellColor::Default,
            CellFlags::DIM,
            UnderlineStyle::Single,
            None,
        );
        tail.print_ascii(b"To resume this session:", dim, MAX_OUTPUT_BYTES);
        tail.print_ascii(
            b" agent --session id",
            CapturedStyle::default(),
            MAX_OUTPUT_BYTES,
        );

        let (text, styled) =
            merge_primary_screen_interrupt_tail(String::new(), StyledOutput::default(), &tail);
        assert_eq!(text, "To resume this session: agent --session id");
        assert_eq!(styled.line(0).unwrap().attributes_at(0), CellFlags::DIM);
    }
}
