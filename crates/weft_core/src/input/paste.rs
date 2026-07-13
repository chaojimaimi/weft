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

/// Wrap text for bracketed paste mode.
pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        let mut bytes = bracketed_paste_start();
        bytes.extend_from_slice(text.as_bytes());
        bytes.extend(bracketed_paste_end());
        bytes
    } else {
        text.as_bytes().to_vec()
    }
}
