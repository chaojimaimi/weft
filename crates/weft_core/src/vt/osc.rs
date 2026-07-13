use crate::grid::Color;

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
