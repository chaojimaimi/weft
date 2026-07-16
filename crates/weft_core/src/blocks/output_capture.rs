//! Terminal-like single-line rewrite handling for detached block output.

#[derive(Debug, Default)]
pub(super) struct OutputCapture {
    text: String,
    cursor: usize,
    truncated: bool,
}

impl OutputCapture {
    pub(super) fn as_str(&self) -> &str {
        &self.text
    }

    pub(super) fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.truncated = false;
    }

    pub(super) fn print(&mut self, c: char, max_bytes: usize) {
        if self.truncated {
            return;
        }
        let replaced_len = self
            .char_end_at_cursor()
            .filter(|_| self.text.as_bytes().get(self.cursor) != Some(&b'\n'))
            .map(|end| end - self.cursor)
            .unwrap_or(0);
        let next_len = self
            .text
            .len()
            .saturating_sub(replaced_len)
            .saturating_add(c.len_utf8());
        if next_len > max_bytes {
            self.truncated = true;
            return;
        }

        if replaced_len > 0 {
            let end = self.cursor + replaced_len;
            self.text.replace_range(self.cursor..end, &c.to_string());
        } else {
            self.text.insert(self.cursor, c);
        }
        self.cursor += c.len_utf8();
    }

    pub(super) fn print_ascii(&mut self, bytes: &[u8], max_bytes: usize) {
        if bytes.is_empty() || self.truncated {
            return;
        }
        if self.cursor == self.text.len() {
            let remaining = max_bytes.saturating_sub(self.text.len());
            let accepted = remaining.min(bytes.len());
            if accepted > 0 {
                // Callers guarantee printable ASCII.
                self.text
                    .push_str(std::str::from_utf8(&bytes[..accepted]).unwrap_or(""));
                self.cursor = self.text.len();
            }
            if accepted < bytes.len() {
                self.truncated = true;
            }
            return;
        }
        for byte in bytes {
            self.print(char::from(*byte), max_bytes);
            if self.truncated {
                break;
            }
        }
    }

    pub(super) fn newline(&mut self, max_bytes: usize) {
        if self.truncated {
            return;
        }
        self.cursor = self.line_end();
        if self.text.as_bytes().get(self.cursor) == Some(&b'\n') {
            self.cursor += 1;
            return;
        }
        if self.text.len() < max_bytes {
            self.text.push('\n');
            self.cursor = self.text.len();
        } else {
            self.truncated = true;
        }
    }

    pub(super) fn carriage_return(&mut self) {
        self.cursor = self.line_start();
    }

    pub(super) fn backspace(&mut self) {
        let start = self.line_start();
        if self.cursor > start {
            self.cursor -= 1;
            while !self.text.is_char_boundary(self.cursor) {
                self.cursor -= 1;
            }
        }
    }

    pub(super) fn erase_line(&mut self, mode: u16) {
        let start = self.line_start();
        let end = self.line_end();
        match mode {
            0 => self.text.replace_range(self.cursor..end, ""),
            1 => {
                let replacement = " ".repeat(self.text[start..self.cursor].chars().count());
                self.text.replace_range(start..self.cursor, &replacement);
                self.cursor = start + replacement.len();
            }
            2 => {
                self.text.replace_range(start..end, "");
                self.cursor = start;
            }
            _ => {}
        }
    }

    pub(super) fn take(&mut self) -> String {
        let mut text = std::mem::take(&mut self.text);
        if self.truncated {
            text.push_str("\n…(output truncated, >1 MiB)");
        }
        self.cursor = 0;
        self.truncated = false;
        text
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |offset| self.cursor + offset)
    }

    fn char_end_at_cursor(&self) -> Option<usize> {
        let c = self.text.get(self.cursor..)?.chars().next()?;
        Some(self.cursor + c.len_utf8())
    }
}

#[cfg(test)]
mod tests {
    use super::OutputCapture;

    #[test]
    fn carriage_return_overwrites_spinner_frame_in_place() {
        let mut output = OutputCapture::default();
        output.print_ascii(b"Upgrading.", 1024);
        output.carriage_return();
        output.print_ascii(b"Upgrading..", 1024);
        output.carriage_return();
        output.print_ascii(b"Upgrading...", 1024);
        assert_eq!(output.as_str(), "Upgrading...");
    }

    #[test]
    fn erase_right_removes_tail_from_a_shorter_progress_frame() {
        let mut output = OutputCapture::default();
        output.print_ascii(b"Downloading 100%", 1024);
        output.carriage_return();
        output.print_ascii(b"Done", 1024);
        output.erase_line(0);
        assert_eq!(output.as_str(), "Done");
    }

    #[test]
    fn utf8_replacement_keeps_cursor_on_a_character_boundary() {
        let mut output = OutputCapture::default();
        output.print_ascii(b"abc", 1024);
        output.carriage_return();
        output.print('中', 1024);
        output.print('文', 1024);
        assert_eq!(output.as_str(), "中文c");
    }
}
