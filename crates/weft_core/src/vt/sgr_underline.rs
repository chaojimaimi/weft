//! v1.11.3 (PLAN_v1113 §2.1): colon-form SGR group handlers (`4:x`,
//! `58:x`) plus the flat-walk attribute/color dispatcher. Extracted from
//! `vt/mod.rs` so the terminal facade stays within its architecture-gate
//! line budget — these are pure `Attrs` transforms.

use crate::grid::{CellColor, CellFlags, Color, UnderlineStyle};
use crate::vt::attrs::Attrs;

/// v1.11.3 (PLAN_v1113 §2.1): the flat SGR walk — the "after group-view"
/// dispatcher for the flattened semicolon params. Moved verbatim from
/// `handle_sgr` with the walker index so multi-value color sequences
/// (38;2;R;G;B) consume their payload bytes, and the underline family
/// arms (4/21/24) mirror-sync the dual-track carriers (PLAN_v1113 §1.2).
pub(super) fn apply_flat_sgr(attrs: &mut Attrs, vals: &[u16]) {
    let mut i = 0;
    while i < vals.len() {
        let v = vals[i];
        match v {
            0 => *attrs = Attrs::default(),
            1 => attrs.flags.insert(CellFlags::BOLD),
            2 => attrs.flags.insert(CellFlags::DIM),
            3 => attrs.flags.insert(CellFlags::ITALIC),
            4 => {
                attrs.flags.insert(CellFlags::UNDERLINE);
                // v1.11.3 plain-4 归位 (PLAN_v1113 §2.1): SGR 4
                // (semicolon/plain) resets style to Single — `4:3`
                // then `4` must not leave wavy residue (kitty/
                // wezterm/Ghostty consensus).
                attrs.underline_style = UnderlineStyle::Single;
            }
            7 => attrs.flags.insert(CellFlags::REVERSE),
            8 => attrs.flags.insert(CellFlags::HIDDEN),
            9 => attrs.flags.insert(CellFlags::STRIKETHROUGH),
            // ECMA-48 and the modern-terminal consensus (Ghostty,
            // WezTerm, Alacritty, current xterm): SGR 21 = doubly
            // underlined. The old "clear bold" reading is Linux-console
            // legacy — it also left DOUBLE_UNDER (which block_view/style.rs
            // fully renders) unreachable from the parser.
            21 => {
                attrs.flags.insert(CellFlags::DOUBLE_UNDER);
                // v1.11.3 dual-track (§1.2): both carriers set; the
                // renderer prefers the bit (bit-only legacy cells).
                attrs.underline_style = UnderlineStyle::Double;
            }
            22 => attrs.flags.remove(CellFlags::BOLD | CellFlags::DIM),
            23 => attrs.flags.remove(CellFlags::ITALIC),
            24 => {
                attrs
                    .flags
                    .remove(CellFlags::UNDERLINE | CellFlags::DOUBLE_UNDER);
                // v1.11.3 (§2.1): SGR 24 also 归位s style to Single.
                attrs.underline_style = UnderlineStyle::Single;
            }
            27 => attrs.flags.remove(CellFlags::REVERSE),
            28 => attrs.flags.remove(CellFlags::HIDDEN),
            29 => attrs.flags.remove(CellFlags::STRIKETHROUGH),
            30..=37 => attrs.fg = CellColor::Palette((v - 30) as u8),
            38 => {
                if let Some((color, skip)) = parse_sgr_color(vals, i + 1) {
                    attrs.fg = color;
                    i += skip;
                }
            }
            39 => attrs.fg = CellColor::Default,
            40..=47 => attrs.bg = CellColor::Palette((v - 40) as u8),
            48 => {
                if let Some((color, skip)) = parse_sgr_color(vals, i + 1) {
                    attrs.bg = color;
                    i += skip;
                }
            }
            49 => attrs.bg = CellColor::Default,
            // Underline color set/reset (AUDIT_v1.10.39 P0-1 + v1.11.3).
            // Consumed here so `2`/`5`/`0` payload bytes can't leak into
            // the flat walk as DIM/ITALIC/reset; the color is STORED now
            // (colon form `58:x` was dispatched in the group view).
            58 => {
                if let Some((color, skip)) = parse_sgr_color(vals, i + 1) {
                    attrs.underline_color = Some(color);
                    i += skip;
                }
            }
            59 => attrs.underline_color = None,
            90..=97 => attrs.fg = CellColor::Palette((v - 90 + 8) as u8),
            100..=107 => attrs.bg = CellColor::Palette((v - 100 + 8) as u8),
            _ => {
                tracing::trace!(v, "unhandled SGR param");
            }
        }
        i += 1;
    }
}

/// Parse SGR color starting after the 38/48/58 marker.
/// Returns `(CellColor, skip_count)` where skip_count is how many extra
/// values (beyond the 38/48/58) were consumed. Stores the *origin* (palette
/// index or explicit RGB) rather than resolving against the palette, so a
/// theme/palette change can recolor already-written cells.
fn parse_sgr_color(vals: &[u16], start: usize) -> Option<(CellColor, usize)> {
    let kind = vals.get(start).copied()?;
    match kind {
        // Indexed 256-color: 38;5;N
        5 => {
            let idx = vals.get(start + 1).copied()?.min(255) as u8;
            Some((CellColor::Palette(idx), 2))
        }
        // Truecolor: 38;2;R;G;B
        2 => {
            let r = vals.get(start + 1).copied()? as u8;
            let g = vals.get(start + 2).copied()? as u8;
            let b = vals.get(start + 3).copied()? as u8;
            Some((CellColor::Rgb(Color::rgb(r, g, b)), 4))
        }
        _ => None,
    }
}

/// v1.11.3 (PLAN_v1113 §2.1): whole-group handling for colon-form `4:x`.
///
/// Probe verdict (PLAN_v1113 step 1): vte 0.13.1 materializes every `:` as
/// `params.extend(accumulated)`, so an empty tail yields an explicit `0`
/// subparam — `4:` arrives as `[4,0]`, identical to `4:0`. There is NO
/// "missing subparam" shape; the `sub[1] == 0` branch IS the empty-tail
/// branch (clear underline, SGR 24 semantics).
pub(super) fn handle_underline_group(attrs: &mut Attrs, sub: &[u16]) {
    match sub[1] {
        // kitty-style "no underline": equivalent to SGR 24, style 归位.
        0 => {
            attrs
                .flags
                .remove(CellFlags::UNDERLINE | CellFlags::DOUBLE_UNDER);
            attrs.underline_style = UnderlineStyle::Single;
        }
        1 => {
            attrs.flags.insert(CellFlags::UNDERLINE);
            attrs.underline_style = UnderlineStyle::Single;
            // Mirror-sync (PLAN_v1113 §1.2): a single underline must clear
            // a stale DOUBLE_UNDER bit (SGR 21 then 4:1), or the renderer's
            // bit-first priority keeps drawing double.
            attrs.flags.remove(CellFlags::DOUBLE_UNDER);
        }
        2 => {
            attrs.flags.insert(CellFlags::UNDERLINE);
            attrs.underline_style = UnderlineStyle::Double;
            // Mirror-sync: both carriers agree (SGR 21 writes both).
            attrs.flags.insert(CellFlags::DOUBLE_UNDER);
        }
        3 => {
            attrs.flags.insert(CellFlags::UNDERLINE);
            attrs.underline_style = UnderlineStyle::Wavy;
            attrs.flags.remove(CellFlags::DOUBLE_UNDER);
        }
        4 => {
            attrs.flags.insert(CellFlags::UNDERLINE);
            attrs.underline_style = UnderlineStyle::Dotted;
            attrs.flags.remove(CellFlags::DOUBLE_UNDER);
        }
        5 => {
            attrs.flags.insert(CellFlags::UNDERLINE);
            attrs.underline_style = UnderlineStyle::Dashed;
            attrs.flags.remove(CellFlags::DOUBLE_UNDER);
        }
        other => {
            // Unknown style value (kitty beyond 4:5 incl. curly 4:9): the
            // kitty/WezTerm/Ghostty consensus — and PLAN_v1113 §2.1 verbatim
            // (reviewer Minor-1) — is to IGNORE the whole group: no style
            // write, no flag insertion. Inserting UNDERLINE here would make
            // a probe like `4:9` unexpectedly paint an underline.
            tracing::trace!(style = other, "SGR 4:x unknown underline style ignored");
        }
    }
}

/// v1.11.3 (PLAN_v1113 §2.1): whole-group handling for colon-form `58:x`.
/// kitty and nvim both colon-ize their underline-color sequences (the
/// semicolon form is still covered by the flat walk via
/// `parse_sgr_color`):
///   [58, 2, R, G, B]      → truecolor
///   [58, 2, 0, R, G, B]   → truecolor, empty colorspace slot (nvim sends
///                           `58:2::R:G:B`; per the vte probe that arrives
///                           as [58,2,0,R,G,B])
///   [58, 5, N]            → palette
/// Anything malformed → silently ignore the WHOLE group (no element may
/// leak into the flat walk — the AUDIT_v1.10.39 P0-1 leak).
pub(super) fn handle_underline_color_group(attrs: &mut Attrs, sub: &[u16]) {
    match (sub[1], sub.len()) {
        (2, 5) => {
            let r = sub[2] as u8;
            let g = sub[3] as u8;
            let b = sub[4] as u8;
            attrs.underline_color = Some(CellColor::Rgb(Color::rgb(r, g, b)));
        }
        (2, 6) if sub[2] == 0 => {
            // Colorspace slot is 0 (empty in `::`) — ignore it, keep RGB.
            let r = sub[3] as u8;
            let g = sub[4] as u8;
            let b = sub[5] as u8;
            attrs.underline_color = Some(CellColor::Rgb(Color::rgb(r, g, b)));
        }
        (5, 3) => {
            attrs.underline_color = Some(CellColor::Palette(sub[2].min(255) as u8));
        }
        _ => {
            tracing::trace!(sub = ?sub, "SGR 58:x malformed group ignored");
        }
    }
}
