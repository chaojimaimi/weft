use crate::grid::Color;
use crate::input::{KeyCode, Modifiers};

/// Parse a hex color: `#rgb`, `#rrggbb`, or `#rrggbbaa` (case-insensitive,
/// leading `#` optional).
pub fn parse_hex(s: &str) -> Option<Color> {
    let s = s.trim().trim_start_matches('#');
    let (r, g, b, a) = match s.len() {
        3 => {
            let bytes = s.as_bytes();
            (
                hex_val(bytes[0])? * 17,
                hex_val(bytes[1])? * 17,
                hex_val(bytes[2])? * 17,
                255,
            )
        }
        6 | 8 => {
            let bytes = s.as_bytes();
            let r = hex_val(bytes[0])? * 16 + hex_val(bytes[1])?;
            let g = hex_val(bytes[2])? * 16 + hex_val(bytes[3])?;
            let b = hex_val(bytes[4])? * 16 + hex_val(bytes[5])?;
            let a = if s.len() == 8 {
                hex_val(bytes[6])? * 16 + hex_val(bytes[7])?
            } else {
                255
            };
            (r, g, b, a)
        }
        _ => return None,
    };
    Some(Color { r, g, b, a })
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Parse a keybinding spec like `"cmd+shift+page_up"` or `"cmd+,"` into a
/// `(KeyCode, Modifiers)` pair. Tokens are split on `+`; the final token is
/// the key, the rest are modifiers. Unknown tokens → `None`.
pub fn parse_binding(spec: &str) -> Option<(KeyCode, Modifiers)> {
    let tokens: Vec<&str> = spec.split('+').map(str::trim).collect();
    if tokens.is_empty() {
        return None;
    }
    let mut mods = Modifiers::empty();
    for tok in &tokens[..tokens.len() - 1] {
        match tok.to_ascii_lowercase().as_str() {
            "cmd" | "super" | "win" | "meta" => mods |= Modifiers::SUPER,
            "ctrl" | "control" => mods |= Modifiers::CONTROL,
            "alt" | "option" | "opt" => mods |= Modifiers::ALT,
            "shift" => mods |= Modifiers::SHIFT,
            _ => return None,
        }
    }
    let key = parse_key_token(tokens.last().unwrap())?;
    Some((key, mods))
}

fn parse_key_token(tok: &str) -> Option<KeyCode> {
    let lower = tok.to_ascii_lowercase();
    match lower.as_str() {
        "enter" | "return" => Some(KeyCode::Enter),
        "tab" => Some(KeyCode::Tab),
        "escape" | "esc" => Some(KeyCode::Escape),
        "backspace" => Some(KeyCode::Backspace),
        "up" => Some(KeyCode::Up),
        "down" => Some(KeyCode::Down),
        "left" => Some(KeyCode::Left),
        "right" => Some(KeyCode::Right),
        "home" => Some(KeyCode::Home),
        "end" => Some(KeyCode::End),
        "page_up" | "pageup" => Some(KeyCode::PageUp),
        "page_down" | "pagedown" => Some(KeyCode::PageDown),
        "delete" | "del" => Some(KeyCode::Delete),
        "insert" | "ins" => Some(KeyCode::Insert),
        "space" => Some(KeyCode::Char(' ')),
        "comma" => Some(KeyCode::Char(',')),
        "period" => Some(KeyCode::Char('.')),
        "minus" | "hyphen" => Some(KeyCode::Char('-')),
        "plus" => Some(KeyCode::Char('+')),
        "equals" => Some(KeyCode::Char('=')),
        "left_bracket" | "lbracket" => Some(KeyCode::Char('[')),
        "right_bracket" | "rbracket" => Some(KeyCode::Char(']')),
        _ => {
            // f1..=f12
            if let Some(n) = lower.strip_prefix('f') {
                if let Ok(n) = n.parse::<u8>() {
                    if (1..=12).contains(&n) {
                        return Some(KeyCode::F(n));
                    }
                }
            }
            // Single printable character.
            let mut chars = tok.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) if !c.is_whitespace() => Some(KeyCode::Char(c)),
                _ => None,
            }
        }
    }
}
