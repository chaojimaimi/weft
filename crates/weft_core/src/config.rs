//! Configuration & theming.
//!
//! TOML config at `$XDG_CONFIG_HOME/weft/config.toml` (or
//! `~/.config/weft/config.toml`), fully optional — sensible defaults apply.
//! Parsed here into a resolved [`Config`] / [`Theme`] / [`KeyBindings`] that the
//! app layer consumes. This module is pure logic (no rendering / fs effects
//! beyond reading the file), so it is unit-testable.
//!
//! ```toml
//! [font]
//! family = "Menlo"
//! size = 14.0
//! cjk_family = "PingFang SC"
//! emoji_family = "Apple Color Emoji"
//!
//! [theme]
//! name = "weft-warm"            # weft-warm | weft-light (weft-dark = legacy alias for weft-warm)
//! foreground = "#e0d4c4"        # optional inline overrides
//! accent = "#d4a574"            # v0.8: signature accent (amber)
//! palette = ["#2a2420", "#c86858", ...]   # optional, overrides ANSI 0-15
//!
//! [window]
//! width = 800
//! height = 600
//! title = "Weft"
//! opacity = 1.0
//!
//! [scrollback]
//! lines = 10000
//!
//! [keybindings]
//! "cmd+c" = "copy"
//! "cmd+v" = "paste"
//! ```

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

use crate::grid::Color;
use crate::input::{KeyCode, Modifiers};

// ── Theme (resolved colors the renderer needs) ─────────────────────────

/// Syntax-highlight color palette (9 colors). Theme-driven so every theme
/// can define its own command/flag/path/string colors; replaces the hardcoded
/// `syntax_color()` from renderer.rs v0.5. Conventions match the "Warm
/// Terminal" direction (v0.8 §0.3) but each theme fills its own values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyntaxColors {
    /// Command name (first word, or after `|` / `&&` / `;`).
    pub command: Color,
    /// A flag: `-x` / `--flag`.
    pub flag: Color,
    /// A filesystem path (any word containing `/`).
    pub path: Color,
    /// A quoted string (`"…"` / `'…'`), including the quotes.
    pub string: Color,
    /// A numeric literal (`^[+-]?\d+(\.\d+)?$`).
    pub number: Color,
    /// A variable reference: `$VAR` / `${VAR}`.
    pub variable: Color,
    /// A shell operator: `|` `>` `<` `>>` `&&` `||` `;` `&`.
    pub operator: Color,
    /// A shell comment: `#` to end of line.
    pub comment: Color,
    /// Anything else (arguments, values) — usually == theme.foreground.
    pub default: Color,
}

/// A fully-resolved theme: the colors the renderer paints with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Theme {
    pub foreground: Color,
    pub background: Color,
    pub cursor: Color,
    pub selection: Color,
    /// 256-color palette; slots 0-15 are the ANSI colors.
    pub palette: [Color; 256],
    /// Signature accent color — prompt `❯`, scrollbar thumb, cursor glow.
    /// Warm themes use amber (#d4a574); cool themes use blue/cyan.
    pub accent: Color,
    /// Dimmed accent — chevrons `▸▾`, completion hover, secondary chrome.
    /// Quieter than `accent`; the "Quiet" direction uses this for UI skeleton
    /// so the bones recede and text becomes the protagonist.
    pub accent_dim: Color,
    /// Block separator color. Subtle (low contrast) in the Quiet direction
    /// — blocks separate by whitespace first, the line is barely visible.
    pub separator: Color,
    /// Syntax-highlight palette (replaces hardcoded syntax_color in renderer).
    pub syntax: SyntaxColors,
}

impl Theme {
    /// Built-in dark theme — **weft-warm** (v0.8 default).
    ///
    /// Warm-toned (deep brown bg `#221c18` + amber accent `#d4a574`), the
    /// "Warm Terminal" direction (v0.8 §0.3). `weft_dark` is kept as a name
    /// alias for backward compatibility (old configs that say `weft-dark`).
    pub fn weft_dark() -> Self {
        Self::weft_warm()
    }

    /// v0.8 default theme: warm-toned dark. The weft visual identity.
    pub fn weft_warm() -> Self {
        let mut palette = Color::standard_palette();
        // Refined ANSI 0-15 — warm-tuned (softer than pure primaries, with a
        // slight amber bias to match the accent).
        let ansi = [
            (0x2a, 0x24, 0x20), // 0 black   (warm-tinted)
            (0xc8, 0x68, 0x58), // 1 red     (brick, not pure red)
            (0xb8, 0xc8, 0x78), // 2 green   (olive, not pure green)
            (0xd4, 0xa5, 0x43), // 3 yellow  (warm honey)
            (0xc4, 0xa0, 0xc8), // 4 blue    (dusty purple-blue for warmth)
            (0xd4, 0x88, 0x70), // 5 magenta (warm coral)
            (0xd4, 0xa5, 0x74), // 6 cyan    (amber — matches accent)
            (0xe0, 0xd4, 0xc4), // 7 white   (warm cream)
            (0x4a, 0x3f, 0x35), // 8 bright black (warm dark brown)
            (0xe0, 0x88, 0x78), // 9 bright red
            (0xd0, 0xe0, 0x90), // 10 bright green
            (0xe8, 0xc8, 0x70), // 11 bright yellow
            (0xd8, 0xc0, 0xe0), // 12 bright blue
            (0xe8, 0xa8, 0x90), // 13 bright magenta
            (0xe8, 0xc8, 0x9c), // 14 bright cyan
            (0xf0, 0xe8, 0xdc), // 15 bright white
        ];
        for (i, (r, g, b)) in ansi.iter().enumerate() {
            palette[i] = Color::rgb(*r, *g, *b);
        }
        Self {
            foreground: Color::rgb(0xe0, 0xd4, 0xc4), // warm cream
            background: Color::rgb(0x22, 0x1c, 0x18), // deep warm brown
            cursor: Color::rgb(0xf0, 0xd4, 0xa8),     // amber-tinted white (glow anchor)
            // Selection: warm amber tint at higher saturation than the
            // previous #4a3825 (which was nearly indistinguishable from
            // the #221c18 background — selection highlight was effectively
            // invisible). v0.8 user testing flagged this.
            selection: Color::rgb(0x8a, 0x5c, 0x28),
            palette,
            accent: Color::rgb(0xd4, 0xa5, 0x74), // amber — signature
            accent_dim: Color::rgb(0x7a, 0x6a, 0x58), // warm gray (chevrons, dim text)
            separator: Color::rgb(0x4a, 0x3f, 0x35), // barely-visible warm dark
            syntax: SyntaxColors {
                command: Color::rgb(0xb8, 0xc8, 0x78),  // olive
                flag: Color::rgb(0xd4, 0xa5, 0x74),     // amber (== accent)
                path: Color::rgb(0xc8, 0x98, 0x58),     // terracotta
                string: Color::rgb(0xd4, 0x88, 0x70),   // warm coral
                number: Color::rgb(0xd4, 0xa5, 0x43),   // warm yellow
                variable: Color::rgb(0xc4, 0xa0, 0xc8), // dusty purple
                operator: Color::rgb(0xc8, 0x68, 0x58), // brick red
                comment: Color::rgb(0x7a, 0x6a, 0x58),  // warm gray (== accent_dim)
                default: Color::rgb(0xe0, 0xd4, 0xc4),  // == foreground
            },
        }
    }

    /// Built-in light theme — warm-toned light variant of weft-warm.
    pub fn weft_light() -> Self {
        let mut palette = Color::standard_palette();
        let ansi = [
            (0x40, 0x38, 0x30), // 0 black
            (0xa8, 0x48, 0x38), // 1 red
            (0x6a, 0x80, 0x40), // 2 green
            (0xa8, 0x78, 0x20), // 3 yellow
            (0x80, 0x60, 0x90), // 4 blue
            (0xa8, 0x60, 0x50), // 5 magenta
            (0xa8, 0x78, 0x40), // 6 cyan (amber-ish)
            (0x3a, 0x32, 0x28), // 7 white (text)
            (0x80, 0x70, 0x60), // 8 bright black
            (0xc0, 0x58, 0x48), // 9 bright red
            (0x80, 0x98, 0x50), // 10 bright green
            (0xc0, 0x88, 0x30), // 11 bright yellow
            (0x98, 0x78, 0xa8), // 12 bright blue
            (0xc0, 0x78, 0x68), // 13 bright magenta
            (0xc0, 0x98, 0x50), // 14 bright cyan
            (0x28, 0x20, 0x18), // 15 bright white
        ];
        for (i, (r, g, b)) in ansi.iter().enumerate() {
            palette[i] = Color::rgb(*r, *g, *b);
        }
        Self {
            foreground: Color::rgb(0x3a, 0x32, 0x28), // warm dark brown text
            background: Color::rgb(0xf5, 0xf0, 0xe8), // warm cream-white
            cursor: Color::rgb(0x8a, 0x60, 0x30),     // warm amber-brown
            selection: Color::rgb(0xe0, 0xd0, 0xb8),  // warm tan
            palette,
            accent: Color::rgb(0xa8, 0x70, 0x30), // amber (darker for light bg)
            accent_dim: Color::rgb(0x8a, 0x78, 0x68), // warm gray
            separator: Color::rgb(0xd0, 0xc4, 0xb0), // warm light gray
            syntax: SyntaxColors {
                command: Color::rgb(0x5a, 0x78, 0x30),  // olive green
                flag: Color::rgb(0xa8, 0x70, 0x30),     // amber (== accent)
                path: Color::rgb(0x9a, 0x68, 0x20),     // terracotta
                string: Color::rgb(0xa8, 0x50, 0x40),   // warm coral
                number: Color::rgb(0x9a, 0x70, 0x20),   // warm yellow
                variable: Color::rgb(0x70, 0x50, 0x90), // dusty purple
                operator: Color::rgb(0xa8, 0x48, 0x38), // brick red
                comment: Color::rgb(0x8a, 0x78, 0x68),  // warm gray (== accent_dim)
                default: Color::rgb(0x3a, 0x32, 0x28),  // == foreground
            },
        }
    }

    /// v0.9 W2+: Warp-style dark theme.
    ///
    /// Inspired by Warp's default "Warp Dark" palette: cool dark background,
    /// coral-red accent (`#ff5d38`), and a saturated syntax palette tuned
    /// for readability on a dark canvas. Sits alongside `weft_warm` as a
    /// first-class built-in — set `name = "warp"` (or `"warp-dark"`) in
    /// `[theme]` to use it.
    pub fn warp_dark() -> Self {
        let mut palette = Color::standard_palette();
        // ANSI 0-15 — Warp-tuned cool palette with a coral accent bias.
        let ansi = [
            (0x1b, 0x1b, 0x28), // 0 black   (cool dark, == background)
            (0xff, 0x5d, 0x38), // 1 red     (Warp coral — accent)
            (0x3e, 0xd9, 0xa4), // 2 green   (mint)
            (0xff, 0xc7, 0x4a), // 3 yellow  (warm yellow)
            (0x5a, 0xb9, 0xf8), // 4 blue    (sky)
            (0xb3, 0x87, 0xff), // 5 magenta (lavender)
            (0x5a, 0xb9, 0xf8), // 6 cyan    (== blue for harmony)
            (0xd9, 0xd9, 0xe3), // 7 white   (cool off-white, == foreground)
            (0x3a, 0x3a, 0x52), // 8 bright black (muted purple-gray)
            (0xff, 0x8a, 0x6a), // 9 bright red
            (0x6a, 0xe8, 0xb8), // 10 bright green
            (0xff, 0xe0, 0x8a), // 11 bright yellow
            (0x8a, 0xc8, 0xff), // 12 bright blue
            (0xc8, 0xa8, 0xff), // 13 bright magenta
            (0x8a, 0xc8, 0xff), // 14 bright cyan
            (0xf0, 0xf0, 0xf8), // 15 bright white
        ];
        for (i, (r, g, b)) in ansi.iter().enumerate() {
            palette[i] = Color::rgb(*r, *g, *b);
        }
        Self {
            foreground: Color::rgb(0xd9, 0xd9, 0xe3), // cool off-white
            background: Color::rgb(0x1b, 0x1b, 0x28), // deep cool dark
            cursor: Color::rgb(0xff, 0x5d, 0x38),     // coral (glow anchor)
            selection: Color::rgb(0x3a, 0x3a, 0x5a),  // translucent purple
            palette,
            accent: Color::rgb(0xff, 0x5d, 0x38), // Warp coral — signature
            accent_dim: Color::rgb(0x7a, 0x4a, 0x3a), // dim coral
            separator: Color::rgb(0x2a, 0x2a, 0x40), // cool dark purple
            syntax: SyntaxColors {
                command: Color::rgb(0xd9, 0xd9, 0xe3),  // == foreground
                flag: Color::rgb(0xff, 0x5d, 0x38),     // coral (== accent)
                path: Color::rgb(0x5a, 0xb9, 0xf8),     // sky blue
                string: Color::rgb(0xc7, 0xa5, 0x5c),   // warm yellow
                number: Color::rgb(0xff, 0xc7, 0x4a),   // bright yellow
                variable: Color::rgb(0xb3, 0x87, 0xff), // lavender
                operator: Color::rgb(0x7a, 0x7a, 0x90), // cool gray
                comment: Color::rgb(0x5a, 0x5a, 0x72),  // muted purple-gray
                default: Color::rgb(0xd9, 0xd9, 0xe3),  // == foreground
            },
        }
    }

    /// v0.9 W2+: Dracula theme.
    ///
    /// The most famous dark theme ever created (Zeno Rocha). Color values
    /// taken from the official palette (https://draculatheme.com/palette).
    /// Set `name = "dracula"` in `[theme]` to use it.
    pub fn dracula() -> Self {
        let mut palette = Color::standard_palette();
        // ANSI 0-15 — Dracula official palette.
        let ansi = [
            (0x28, 0x2a, 0x36), // 0 black   (== background)
            (0xff, 0x55, 0x55), // 1 red
            (0x50, 0xfa, 0x7b), // 2 green
            (0xf1, 0xfa, 0x8c), // 3 yellow
            (0xbd, 0x93, 0xf9), // 4 blue    (Dracula "purple")
            (0xff, 0x79, 0xc6), // 5 magenta (Dracula "pink")
            (0x8b, 0xe9, 0xfd), // 6 cyan
            (0xf8, 0xf8, 0xf2), // 7 white   (== foreground)
            (0x62, 0x72, 0xa4), // 8 bright black (Dracula "comment")
            (0xff, 0x55, 0x55), // 9 bright red
            (0x50, 0xfa, 0x7b), // 10 bright green
            (0xf1, 0xfa, 0x8c), // 11 bright yellow
            (0xbd, 0x93, 0xf9), // 12 bright blue
            (0xff, 0x79, 0xc6), // 13 bright magenta
            (0x8b, 0xe9, 0xfd), // 14 bright cyan
            (0xff, 0xff, 0xff), // 15 bright white
        ];
        for (i, (r, g, b)) in ansi.iter().enumerate() {
            palette[i] = Color::rgb(*r, *g, *b);
        }
        Self {
            foreground: Color::rgb(0xf8, 0xf8, 0xf2),
            background: Color::rgb(0x28, 0x2a, 0x36),
            cursor: Color::rgb(0xbd, 0x93, 0xf9),    // purple
            selection: Color::rgb(0x44, 0x47, 0x5a), // current line
            palette,
            accent: Color::rgb(0xbd, 0x93, 0xf9), // Dracula purple
            accent_dim: Color::rgb(0x62, 0x72, 0xa4), // comment color
            separator: Color::rgb(0x44, 0x47, 0x5a), // current line
            syntax: SyntaxColors {
                command: Color::rgb(0xf8, 0xf8, 0xf2),  // == foreground
                flag: Color::rgb(0xff, 0x79, 0xc6),     // pink
                path: Color::rgb(0x8b, 0xe9, 0xfd),     // cyan
                string: Color::rgb(0xf1, 0xfa, 0x8c),   // yellow
                number: Color::rgb(0xbd, 0x93, 0xf9),   // purple
                variable: Color::rgb(0xff, 0xb8, 0x6c), // orange
                operator: Color::rgb(0xff, 0x55, 0x55), // red
                comment: Color::rgb(0x62, 0x72, 0xa4),  // comment
                default: Color::rgb(0xf8, 0xf8, 0xf2),  // == foreground
            },
        }
    }

    /// v0.9 W2+: Solarized Dark theme.
    ///
    /// Ethan Schoonover's precision-engineered palette. Color values taken
    /// from the official spec (https://ethanschoonover.com/solarized).
    /// Set `name = "solarized-dark"` in `[theme]` to use it.
    pub fn solarized_dark() -> Self {
        let mut palette = Color::standard_palette();
        // ANSI 0-15 — Solarized accent colors mapped to standard ANSI slots.
        let ansi = [
            (0x07, 0x36, 0x42), // 0 black   (base02)
            (0xdc, 0x32, 0x2f), // 1 red
            (0x85, 0x99, 0x00), // 2 green
            (0xb5, 0x89, 0x00), // 3 yellow
            (0x26, 0x8b, 0xd2), // 4 blue
            (0xd3, 0x36, 0x82), // 5 magenta
            (0x2a, 0xa1, 0x98), // 6 cyan
            (0xee, 0xe8, 0xd5), // 7 white   (base2)
            (0x00, 0x2b, 0x36), // 8 bright black (base03)
            (0xcb, 0x4b, 0x16), // 9 bright red (orange)
            (0x58, 0x6e, 0x75), // 10 bright green (base01)
            (0x83, 0x94, 0x96), // 11 bright yellow (base0)
            (0x93, 0xa1, 0xa1), // 12 bright blue (base1)
            (0x6c, 0x71, 0xc4), // 13 bright magenta (violet)
            (0x07, 0x36, 0x42), // 14 bright cyan (== base02 for harmony)
            (0xfd, 0xf6, 0xe3), // 15 bright white (base3)
        ];
        for (i, (r, g, b)) in ansi.iter().enumerate() {
            palette[i] = Color::rgb(*r, *g, *b);
        }
        Self {
            foreground: Color::rgb(0x93, 0xa1, 0xa1), // base1 (preferred text)
            background: Color::rgb(0x00, 0x2b, 0x36), // base03
            cursor: Color::rgb(0x93, 0xa1, 0xa1),     // base1
            selection: Color::rgb(0x07, 0x36, 0x42),  // base02
            palette,
            accent: Color::rgb(0x26, 0x8b, 0xd2), // blue (Solarized accent)
            accent_dim: Color::rgb(0x58, 0x6e, 0x75), // base01
            separator: Color::rgb(0x07, 0x36, 0x42), // base02
            syntax: SyntaxColors {
                command: Color::rgb(0x93, 0xa1, 0xa1),  // base1
                flag: Color::rgb(0x26, 0x8b, 0xd2),     // blue
                path: Color::rgb(0x2a, 0xa1, 0x98),     // cyan
                string: Color::rgb(0x85, 0x99, 0x00),   // green
                number: Color::rgb(0xb5, 0x89, 0x00),   // yellow (magenta)
                variable: Color::rgb(0x6c, 0x71, 0xc4), // violet
                operator: Color::rgb(0xdc, 0x32, 0x2f), // red
                comment: Color::rgb(0x58, 0x6e, 0x75),  // base01
                default: Color::rgb(0x93, 0xa1, 0xa1),  // base1
            },
        }
    }

    /// v0.9 W2+: Gruvbox Dark theme.
    ///
    /// morhetz's "retro groove" pastel palette. Color values taken from the
    /// official 256palette script (https://github.com/morhetz/gruvbox).
    /// Set `name = "gruvbox-dark"` in `[theme]` to use it.
    pub fn gruvbox_dark() -> Self {
        let mut palette = Color::standard_palette();
        // ANSI 0-15 — Gruvbox dark palette.
        let ansi = [
            (0x28, 0x28, 0x28), // 0 black   (bg)
            (0xcc, 0x24, 0x1d), // 1 red
            (0x98, 0x97, 0x1a), // 2 green
            (0xd7, 0x99, 0x21), // 3 yellow
            (0x45, 0x85, 0x88), // 4 blue
            (0xb1, 0x62, 0x86), // 5 magenta
            (0x68, 0x9d, 0x6a), // 6 cyan
            (0xa8, 0x99, 0x84), // 7 white   (fg4)
            (0x92, 0x83, 0x74), // 8 bright black (gray)
            (0xfb, 0x49, 0x34), // 9 bright red
            (0xb8, 0xbb, 0x26), // 10 bright green
            (0xfa, 0xbd, 0x2f), // 11 bright yellow
            (0x83, 0xa5, 0x98), // 12 bright blue
            (0xd3, 0x86, 0x9b), // 13 bright magenta
            (0x8e, 0xc0, 0x7c), // 14 bright cyan
            (0xeb, 0xdb, 0xb2), // 15 bright white (fg)
        ];
        for (i, (r, g, b)) in ansi.iter().enumerate() {
            palette[i] = Color::rgb(*r, *g, *b);
        }
        Self {
            foreground: Color::rgb(0xeb, 0xdb, 0xb2), // fg
            background: Color::rgb(0x28, 0x28, 0x28), // bg
            cursor: Color::rgb(0xeb, 0xdb, 0xb2),     // fg
            selection: Color::rgb(0x3c, 0x38, 0x36),  // bg2
            palette,
            accent: Color::rgb(0xfe, 0x80, 0x19), // orange (Gruvbox accent)
            accent_dim: Color::rgb(0x92, 0x83, 0x74), // gray
            separator: Color::rgb(0x3c, 0x38, 0x36), // bg2
            syntax: SyntaxColors {
                command: Color::rgb(0xeb, 0xdb, 0xb2),  // fg
                flag: Color::rgb(0xfe, 0x80, 0x19),     // orange
                path: Color::rgb(0x83, 0xa5, 0x98),     // blue
                string: Color::rgb(0xb8, 0xbb, 0x26),   // green
                number: Color::rgb(0xd3, 0x86, 0x9b),   // purple
                variable: Color::rgb(0xfa, 0xbd, 0x2f), // yellow
                operator: Color::rgb(0xfb, 0x49, 0x34), // red
                comment: Color::rgb(0x92, 0x83, 0x74),  // gray
                default: Color::rgb(0xeb, 0xdb, 0xb2),  // fg
            },
        }
    }

    /// v0.9 W2+: Nord theme.
    ///
    /// Arctic, north-bluish color palette (arcticicestudio, MIT). Official
    /// palette: https://www.nordtheme.com/docs/colors-and-palettes.
    /// Set `name = "nord"` in `[theme]` to use it.
    pub fn nord() -> Self {
        let mut palette = Color::standard_palette();
        // ANSI 0-15 — Nord official palette.
        let ansi = [
            (0x2e, 0x34, 0x40), // 0 black   (nord0 polar night)
            (0xbf, 0x61, 0x6a), // 1 red     (nord11 aurora)
            (0xa3, 0xbe, 0x8c), // 2 green   (nord14)
            (0xeb, 0xcb, 0x8b), // 3 yellow  (nord13)
            (0x81, 0xa1, 0xc1), // 4 blue    (nord9 frost)
            (0xb4, 0x8e, 0xad), // 5 magenta (nord15)
            (0x88, 0xc0, 0xd0), // 6 cyan    (nord8 frost light)
            (0xe5, 0xe9, 0xf0), // 7 white   (nord5 snow storm)
            (0x4c, 0x56, 0x6a), // 8 bright black (nord3)
            (0xbf, 0x61, 0x6a), // 9 bright red
            (0xa3, 0xbe, 0x8c), // 10 bright green
            (0xeb, 0xcb, 0x8b), // 11 bright yellow
            (0x81, 0xa1, 0xc1), // 12 bright blue
            (0xb4, 0x8e, 0xad), // 13 bright magenta
            (0x8f, 0xbc, 0xbb), // 14 bright cyan (nord7)
            (0xec, 0xef, 0xf4), // 15 bright white (nord6)
        ];
        for (i, (r, g, b)) in ansi.iter().enumerate() {
            palette[i] = Color::rgb(*r, *g, *b);
        }
        Self {
            foreground: Color::rgb(0xd8, 0xde, 0xe9), // nord4
            background: Color::rgb(0x2e, 0x34, 0x40), // nord0
            cursor: Color::rgb(0xd8, 0xde, 0xe9),     // nord4
            selection: Color::rgb(0x43, 0x4c, 0x5e),  // nord2
            palette,
            accent: Color::rgb(0x88, 0xc0, 0xd0), // nord8 (frost light)
            accent_dim: Color::rgb(0x4c, 0x56, 0x6a), // nord3
            separator: Color::rgb(0x3b, 0x42, 0x52), // nord1
            syntax: SyntaxColors {
                command: Color::rgb(0xd8, 0xde, 0xe9),  // nord4
                flag: Color::rgb(0x88, 0xc0, 0xd0),     // nord8 cyan-blue
                path: Color::rgb(0x81, 0xa1, 0xc1),     // nord9 frost
                string: Color::rgb(0xa3, 0xbe, 0x8c),   // nord14 green
                number: Color::rgb(0xeb, 0xcb, 0x8b),   // nord13 yellow
                variable: Color::rgb(0xb4, 0x8e, 0xad), // nord15 purple
                operator: Color::rgb(0xbf, 0x61, 0x6a), // nord11 red
                comment: Color::rgb(0x61, 0x69, 0x80),  // nord3 dimmed
                default: Color::rgb(0xd8, 0xde, 0xe9),  // nord4
            },
        }
    }

    /// v0.9 W2+: Tokyo Night theme.
    ///
    /// A clean, dark color scheme inspired by Tokyo city lights (enkia, MIT).
    /// Official: https://github.com/tokyo-night/tokyo-night-vscode-theme.
    /// Set `name = "tokyo-night"` in `[theme]` to use it.
    pub fn tokyo_night() -> Self {
        let mut palette = Color::standard_palette();
        let ansi = [
            (0x15, 0x16, 0x23), // 0 black   (bg darker)
            (0xf7, 0x76, 0x8e), // 1 red
            (0x9e, 0xce, 0x6a), // 2 green
            (0xe0, 0xaf, 0x68), // 3 yellow
            (0x7a, 0xa2, 0xf7), // 4 blue
            (0xbb, 0x9a, 0xf7), // 5 magenta
            (0x7d, 0xcf, 0xff), // 6 cyan
            (0xa9, 0xb1, 0xd6), // 7 white   (fg)
            (0x41, 0x42, 0x5a), // 8 bright black (comment)
            (0xf7, 0x76, 0x8e), // 9 bright red
            (0x9e, 0xce, 0x6a), // 10 bright green
            (0xe0, 0xaf, 0x68), // 11 bright yellow
            (0x7a, 0xa2, 0xf7), // 12 bright blue
            (0xbb, 0x9a, 0xf7), // 13 bright magenta
            (0x7d, 0xcf, 0xff), // 14 bright cyan
            (0xc0, 0xca, 0xf5), // 15 bright white
        ];
        for (i, (r, g, b)) in ansi.iter().enumerate() {
            palette[i] = Color::rgb(*r, *g, *b);
        }
        Self {
            foreground: Color::rgb(0xa9, 0xb1, 0xd6),
            background: Color::rgb(0x1a, 0x1b, 0x26),
            cursor: Color::rgb(0xc0, 0xca, 0xf5),
            selection: Color::rgb(0x28, 0x34, 0x57),
            palette,
            accent: Color::rgb(0x7a, 0xa2, 0xf7),     // blue
            accent_dim: Color::rgb(0x56, 0x5f, 0x89), // comment
            separator: Color::rgb(0x16, 0x18, 0x2a),
            syntax: SyntaxColors {
                command: Color::rgb(0xa9, 0xb1, 0xd6),  // fg
                flag: Color::rgb(0x7a, 0xa2, 0xf7),     // blue
                path: Color::rgb(0x7d, 0xcf, 0xff),     // cyan
                string: Color::rgb(0x9e, 0xce, 0x6a),   // green
                number: Color::rgb(0xff, 0x9e, 0x64),   // orange
                variable: Color::rgb(0xbb, 0x9a, 0xf7), // magenta
                operator: Color::rgb(0xf7, 0x76, 0x8e), // red
                comment: Color::rgb(0x56, 0x5f, 0x89),  // comment
                default: Color::rgb(0xa9, 0xb1, 0xd6),  // fg
            },
        }
    }

    /// v0.9 W2+: Catppuccin Mocha theme.
    ///
    /// Soothing pastel theme — the dark Mocha flavor (Catppuccin org, MIT).
    /// Official: https://catppuccin.com/palette. Set `name = "catppuccin"` in
    /// `[theme]` to use it.
    pub fn catppuccin_mocha() -> Self {
        let mut palette = Color::standard_palette();
        let ansi = [
            (0x1e, 0x1e, 0x2e), // 0 black   (base)
            (0xf3, 0x8b, 0xa8), // 1 red
            (0xa6, 0xe3, 0xa1), // 2 green
            (0xf9, 0xe2, 0xaf), // 3 yellow
            (0x89, 0xb4, 0xfa), // 4 blue
            (0xcb, 0xa6, 0xf7), // 5 magenta (mauve)
            (0x94, 0xe2, 0xd5), // 6 cyan    (teal)
            (0xcd, 0xd6, 0xf4), // 7 white   (text)
            (0x6c, 0x70, 0x86), // 8 bright black (overlay0)
            (0xf3, 0x8b, 0xa8), // 9 bright red
            (0xa6, 0xe3, 0xa1), // 10 bright green
            (0xf9, 0xe2, 0xaf), // 11 bright yellow
            (0x89, 0xb4, 0xfa), // 12 bright blue
            (0xcb, 0xa6, 0xf7), // 13 bright magenta
            (0x94, 0xe2, 0xd5), // 14 bright cyan
            (0xff, 0xff, 0xff), // 15 bright white
        ];
        for (i, (r, g, b)) in ansi.iter().enumerate() {
            palette[i] = Color::rgb(*r, *g, *b);
        }
        Self {
            foreground: Color::rgb(0xcd, 0xd6, 0xf4), // text
            background: Color::rgb(0x1e, 0x1e, 0x2e), // base
            cursor: Color::rgb(0xf5, 0xe0, 0xdc),     // rosewater
            selection: Color::rgb(0x45, 0x47, 0x5a),  // surface1
            palette,
            accent: Color::rgb(0xcb, 0xa6, 0xf7),     // mauve
            accent_dim: Color::rgb(0x6c, 0x70, 0x86), // overlay0
            separator: Color::rgb(0x31, 0x32, 0x44),  // surface0
            syntax: SyntaxColors {
                command: Color::rgb(0xcd, 0xd6, 0xf4),  // text
                flag: Color::rgb(0xcb, 0xa6, 0xf7),     // mauve
                path: Color::rgb(0x89, 0xb4, 0xfa),     // blue
                string: Color::rgb(0xa6, 0xe3, 0xa1),   // green
                number: Color::rgb(0xfa, 0xb3, 0x87),   // peach
                variable: Color::rgb(0xf9, 0xe2, 0xaf), // yellow
                operator: Color::rgb(0xf3, 0x8b, 0xa8), // red
                comment: Color::rgb(0x6c, 0x70, 0x86),  // overlay0
                default: Color::rgb(0xcd, 0xd6, 0xf4),  // text
            },
        }
    }

    /// v0.9 W2+: One Dark theme.
    ///
    /// Atom's iconic dark color scheme (MIT). Set `name = "one-dark"` in
    /// `[theme]` to use it.
    pub fn one_dark() -> Self {
        let mut palette = Color::standard_palette();
        let ansi = [
            (0x28, 0x2c, 0x34), // 0 black   (bg)
            (0xe0, 0x6c, 0x75), // 1 red
            (0x98, 0xc3, 0x79), // 2 green
            (0xe5, 0xc0, 0x7b), // 3 yellow
            (0x61, 0xaf, 0xef), // 4 blue
            (0xc6, 0x78, 0xdd), // 5 magenta (purple)
            (0x56, 0xb6, 0xc2), // 6 cyan
            (0xab, 0xb2, 0xbf), // 7 white   (fg)
            (0x5c, 0x63, 0x70), // 8 bright black (comment)
            (0xe0, 0x6c, 0x75), // 9 bright red
            (0x98, 0xc3, 0x79), // 10 bright green
            (0xe5, 0xc0, 0x7b), // 11 bright yellow
            (0x61, 0xaf, 0xef), // 12 bright blue
            (0xc6, 0x78, 0xdd), // 13 bright magenta
            (0x56, 0xb6, 0xc2), // 14 bright cyan
            (0xff, 0xff, 0xff), // 15 bright white
        ];
        for (i, (r, g, b)) in ansi.iter().enumerate() {
            palette[i] = Color::rgb(*r, *g, *b);
        }
        Self {
            foreground: Color::rgb(0xab, 0xb2, 0xbf),
            background: Color::rgb(0x28, 0x2c, 0x34),
            cursor: Color::rgb(0xab, 0xb2, 0xbf),    // fg
            selection: Color::rgb(0x3e, 0x44, 0x51), // current line
            palette,
            accent: Color::rgb(0x61, 0xaf, 0xef),     // blue
            accent_dim: Color::rgb(0x5c, 0x63, 0x70), // comment
            separator: Color::rgb(0x3e, 0x44, 0x51),
            syntax: SyntaxColors {
                command: Color::rgb(0xab, 0xb2, 0xbf),  // fg
                flag: Color::rgb(0xc6, 0x78, 0xdd),     // purple
                path: Color::rgb(0x56, 0xb6, 0xc2),     // cyan
                string: Color::rgb(0x98, 0xc3, 0x79),   // green
                number: Color::rgb(0xd1, 0x9a, 0x66),   // orange
                variable: Color::rgb(0xe5, 0xc0, 0x7b), // yellow
                operator: Color::rgb(0xe0, 0x6c, 0x75), // red
                comment: Color::rgb(0x5c, 0x63, 0x70),  // comment
                default: Color::rgb(0xab, 0xb2, 0xbf),  // fg
            },
        }
    }

    /// v0.9 W2+: Monokai Pro theme.
    ///
    /// Sublime Text's premium dark theme — classic vibrant palette (MIT).
    /// Set `name = "monokai-pro"` in `[theme]` to use it.
    pub fn monokai_pro() -> Self {
        let mut palette = Color::standard_palette();
        let ansi = [
            (0x2d, 0x2a, 0x2e), // 0 black   (bg)
            (0xff, 0x61, 0x88), // 1 red
            (0xa9, 0xdc, 0x76), // 2 green
            (0xff, 0xd8, 0x66), // 3 yellow
            (0xfc, 0x98, 0x67), // 4 orange (Monokai uses orange in slot 4)
            (0xab, 0x9d, 0xf2), // 5 magenta (purple)
            (0x78, 0xdc, 0xe8), // 6 cyan
            (0xfc, 0xfc, 0xfa), // 7 white   (fg)
            (0x72, 0x70, 0x72), // 8 bright black (comment)
            (0xff, 0x61, 0x88), // 9 bright red
            (0xa9, 0xdc, 0x76), // 10 bright green
            (0xff, 0xd8, 0x66), // 11 bright yellow
            (0xfc, 0x98, 0x67), // 12 bright orange
            (0xab, 0x9d, 0xf2), // 13 bright magenta
            (0x78, 0xdc, 0xe8), // 14 bright cyan
            (0xff, 0xff, 0xff), // 15 bright white
        ];
        for (i, (r, g, b)) in ansi.iter().enumerate() {
            palette[i] = Color::rgb(*r, *g, *b);
        }
        Self {
            foreground: Color::rgb(0xfc, 0xfc, 0xfa),
            background: Color::rgb(0x2d, 0x2a, 0x2e),
            cursor: Color::rgb(0xfc, 0xfc, 0xfa),
            selection: Color::rgb(0x40, 0x3e, 0x41),
            palette,
            accent: Color::rgb(0xff, 0xd8, 0x66), // yellow (Monokai signature)
            accent_dim: Color::rgb(0x72, 0x70, 0x72), // comment
            separator: Color::rgb(0x40, 0x3e, 0x41),
            syntax: SyntaxColors {
                command: Color::rgb(0xfc, 0xfc, 0xfa),  // fg
                flag: Color::rgb(0xff, 0x61, 0x88),     // red
                path: Color::rgb(0x78, 0xdc, 0xe8),     // cyan
                string: Color::rgb(0xa9, 0xdc, 0x76),   // green
                number: Color::rgb(0xab, 0x9d, 0xf2),   // purple
                variable: Color::rgb(0xab, 0x9d, 0xf2), // purple
                operator: Color::rgb(0xff, 0x61, 0x88), // red
                comment: Color::rgb(0x72, 0x70, 0x72),  // comment
                default: Color::rgb(0xfc, 0xfc, 0xfa),  // fg
            },
        }
    }

    /// Resolve a theme from config: pick the built-in base by `cfg.name`,
    /// then apply any inline hex overrides.
    ///
    /// Recognized names: `weft-warm` / `weft-dark` (alias) / `weft-light` /
    /// `warp` / `warp-dark` / `dracula` / `solarized-dark` / `gruvbox-dark`.
    /// Unknown names fall back to `weft-warm`.
    pub fn resolve(cfg: &ThemeConfig) -> Self {
        Self::resolve_named(&cfg.name, cfg)
    }

    /// Resolve a theme by explicit name (v0.9 U-D1 — used by system-theme
    /// follow to pick light/dark by appearance, ignoring `cfg.name`).
    /// Applies the same inline overrides as [`resolve`].
    ///
    /// v0.9 W2+: when `name` doesn't match any built-in, attempts to load a
    /// theme file from `~/.config/weft/themes/<name>.{toml,yaml,yml}`. File
    /// schema mirrors the `[theme]` section of `config.toml`. If no file is
    /// found, falls back to `weft_warm` (the v0.8 default).
    pub fn resolve_named(name: &str, cfg: &ThemeConfig) -> Self {
        let base = match name {
            "weft-light" => Self::weft_light(),
            // Both the v0.8 name and the legacy v0.7 name map to the warm
            // default — old configs that say `weft-dark` keep working but
            // now get the warm palette (the new visual identity).
            "weft-warm" | "weft-dark" | "weft_dark" => Self::weft_warm(),
            // v0.9 W2+: Warp-style dark theme.
            "warp" | "warp-dark" | "warp_dark" => Self::warp_dark(),
            // v0.9 W2+: Classic community themes.
            "dracula" => Self::dracula(),
            "solarized-dark" | "solarized_dark" | "solarized" => Self::solarized_dark(),
            "gruvbox-dark" | "gruvbox_dark" | "gruvbox" => Self::gruvbox_dark(),
            // v0.9 W2+: Community classic themes.
            "nord" => Self::nord(),
            "tokyo-night" | "tokyo_night" => Self::tokyo_night(),
            "catppuccin" | "catppuccin-mocha" | "catppuccin_mocha" => Self::catppuccin_mocha(),
            "one-dark" | "one_dark" | "onedark" => Self::one_dark(),
            "monokai-pro" | "monokai_pro" | "monokai" => Self::monokai_pro(),
            other => match Self::load_from_file(other) {
                Some(t) => t,
                None => Self::weft_warm(),
            },
        };
        let mut theme = base;
        if let Some(c) = cfg.foreground.as_deref().and_then(parse_hex) {
            theme.foreground = c;
        }
        if let Some(c) = cfg.background.as_deref().and_then(parse_hex) {
            theme.background = c;
        }
        if let Some(c) = cfg.cursor.as_deref().and_then(parse_hex) {
            theme.cursor = c;
        }
        if let Some(c) = cfg.selection.as_deref().and_then(parse_hex) {
            theme.selection = c;
        }
        if let Some(c) = cfg.accent.as_deref().and_then(parse_hex) {
            theme.accent = c;
        }
        if let Some(c) = cfg.accent_dim.as_deref().and_then(parse_hex) {
            theme.accent_dim = c;
        }
        if let Some(c) = cfg.separator.as_deref().and_then(parse_hex) {
            theme.separator = c;
        }
        for (i, hex) in cfg.palette.iter().enumerate() {
            if i >= 256 {
                break;
            }
            if let Some(c) = parse_hex(hex) {
                theme.palette[i] = c;
            }
        }
        // v1.0 S5: apply per-syntax-token color overrides on top of the base
        // theme's SyntaxColors. Each field is an optional hex string; absent
        // fields retain the base theme's value.
        if let Some(syn) = cfg.syntax.as_ref() {
            if let Some(c) = syn.command.as_deref().and_then(parse_hex) {
                theme.syntax.command = c;
            }
            if let Some(c) = syn.flag.as_deref().and_then(parse_hex) {
                theme.syntax.flag = c;
            }
            if let Some(c) = syn.path.as_deref().and_then(parse_hex) {
                theme.syntax.path = c;
            }
            if let Some(c) = syn.string.as_deref().and_then(parse_hex) {
                theme.syntax.string = c;
            }
            if let Some(c) = syn.number.as_deref().and_then(parse_hex) {
                theme.syntax.number = c;
            }
            if let Some(c) = syn.variable.as_deref().and_then(parse_hex) {
                theme.syntax.variable = c;
            }
            if let Some(c) = syn.operator.as_deref().and_then(parse_hex) {
                theme.syntax.operator = c;
            }
            if let Some(c) = syn.comment.as_deref().and_then(parse_hex) {
                theme.syntax.comment = c;
            }
            if let Some(c) = syn.default.as_deref().and_then(parse_hex) {
                theme.syntax.default = c;
            }
        }
        theme
    }

    /// v0.9 W2+: Load a custom theme from a file.
    ///
    /// Searches `~/.config/weft/themes/` (or `$XDG_CONFIG_HOME/weft/themes/`)
    /// for `<name>.toml`, `<name>.yaml`, or `<name>.yml`. The file's schema
    /// mirrors the `[theme]` section of `config.toml` — the same fields
    /// (`foreground`, `background`, `accent`, `palette`, etc.) are read and
    /// applied on top of the `weft_warm` base. This lets users drop in a
    /// community theme file (e.g. iTerm2 color schemes converted to TOML)
    /// without modifying weft's source.
    ///
    /// Returns `None` (with a `warn!` log) when:
    ///   - the themes directory doesn't exist or can't be read
    ///   - no file matching `<name>.*` is found
    ///   - the file fails to parse
    ///
    /// # File format
    /// TOML example (`~/.config/weft/themes/my-theme.toml`):
    /// ```toml
    /// background = "#1e1e2e"
    /// foreground = "#cdd6f4"
    /// accent = "#cba6f7"
    /// palette = ["#1e1e2e", "#f38ba8", "#a6e3a1", ...]
    /// ```
    ///
    /// YAML example (`my-theme.yaml`):
    /// ```yaml
    /// background: "#1e1e2e"
    /// foreground: "#cdd6f4"
    /// accent: "#cba6f7"
    /// palette:
    ///   - "#1e1e2e"
    ///   - "#f38ba8"
    /// ```
    pub fn load_from_file(name: &str) -> Option<Self> {
        let dir = Self::themes_dir()?;
        Self::load_from_dir(&dir, name)
    }

    /// v0.9 W2+: Internal loader that reads from an explicit `dir`. Used by
    /// [`load_from_file`] (which resolves the dir from env) and by unit
    /// tests (which pass a tempdir). See [`load_from_file`] for the file
    /// format and resolution semantics.
    fn load_from_dir(dir: &std::path::Path, name: &str) -> Option<Self> {
        // Try each supported extension in order: toml, yaml, yml.
        for ext in ["toml", "yaml", "yml"] {
            let path = dir.join(format!("{name}.{ext}"));
            if !path.exists() {
                continue;
            }
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "failed to read theme file",
                    );
                    return None;
                }
            };
            // Parse according to extension. TOML reuses ThemeConfig serde
            // (which derives Deserialize). YAML uses serde_yaml.
            let file_cfg: ThemeConfig = match ext {
                "toml" => match toml::from_str(&text) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %e,
                            "failed to parse theme file as TOML",
                        );
                        return None;
                    }
                },
                "yaml" | "yml" => match serde_yaml::from_str(&text) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %e,
                            "failed to parse theme file as YAML",
                        );
                        return None;
                    }
                },
                _ => unreachable!(),
            };
            tracing::info!(
                path = %path.display(),
                "loaded custom theme from file",
            );
            // The file's own inline overrides are applied by reusing the
            // resolve pipeline with the file's ThemeConfig. Base on weft_warm
            // so unspecified fields get sensible defaults.
            return Some(Self::resolve_named("weft-warm", &file_cfg));
        }
        // No file matched — silently fall back (user may have just typed a
        // built-in name we don't recognize yet, so don't warn here).
        None
    }

    /// The themes directory: `$XDG_CONFIG_HOME/weft/themes/` or
    /// `~/.config/weft/themes/`. `None` when neither env var is set.
    pub fn themes_dir() -> Option<PathBuf> {
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
            return Some(PathBuf::from(xdg).join("weft").join("themes"));
        }
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(".config").join("weft").join("themes"))
    }
}

// ── Actions & keybindings ──────────────────────────────────────────────

/// A bindable action (the value side of a keybinding).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Deserialize)]
pub enum Action {
    #[serde(rename = "copy")]
    Copy,
    #[serde(rename = "paste")]
    Paste,
    #[serde(rename = "reload_config")]
    ReloadConfig,
    #[serde(rename = "scroll_page_up")]
    ScrollPageUp,
    #[serde(rename = "scroll_page_down")]
    ScrollPageDown,
    /// Scroll the scrollback buffer up by one line (Cmd+↑).
    #[serde(rename = "scroll_line_up")]
    ScrollLineUp,
    /// Scroll the scrollback buffer down by one line (Cmd+↓).
    #[serde(rename = "scroll_line_down")]
    ScrollLineDown,
    #[serde(rename = "scroll_to_top")]
    ScrollToTop,
    #[serde(rename = "scroll_to_bottom")]
    ScrollToBottom,
    #[serde(rename = "toggle_block_panel")]
    ToggleBlockPanel,
    #[serde(rename = "toggle_command_palette")]
    ToggleCommandPalette,
    /// Increase font size (Cmd+=). Multiplies the active font size by 1.1,
    /// clamped to 3× the configured base.
    #[serde(rename = "zoom_in")]
    ZoomIn,
    /// Decrease font size (Cmd+-). Divides the active font size by 1.1,
    /// clamped to 0.5× the configured base.
    #[serde(rename = "zoom_out")]
    ZoomOut,
    /// Reset font size to the configured base (Cmd+0).
    #[serde(rename = "zoom_reset")]
    ZoomReset,
    /// Open the in-grid search bar (Cmd+F). Typing debounces 150ms then
    /// scans visible content + recent scrollback for matches.
    #[serde(rename = "find_in_grid")]
    FindInGrid,
    /// Toggle between dark and light themes at runtime (Cmd+Shift+T).
    /// Independent of `ReloadConfig` (Cmd+Shift+,): reload re-reads the
    /// config file and resets the theme to whatever's named there, while
    /// ToggleTheme flips the in-memory `theme_is_dark` flag without
    /// touching disk.
    #[serde(rename = "toggle_theme")]
    ToggleTheme,
    /// Open a new tab (Cmd+T). Spawns a fresh shell session and switches
    /// to it.
    #[serde(rename = "new_tab")]
    NewTab,
    /// Close the current tab (Cmd+W). If this was the last tab, the app
    /// exits.
    #[serde(rename = "close_tab")]
    CloseTab,
    /// Switch to the next tab (Cmd+Shift+] or Cmd+Shift+Right).
    #[serde(rename = "next_tab")]
    NextTab,
    /// Switch to the previous tab (Cmd+Shift+[ or Cmd+Shift+Left).
    #[serde(rename = "prev_tab")]
    PrevTab,
    /// v1.0 S1: Open the Settings panel (Cmd+,). Modal overlay with tabs
    /// for Appearance / Font / Keybindings / Window.
    #[serde(rename = "toggle_settings")]
    ToggleSettings,
}

/// Resolved keybinding table: physical key + modifiers → action.
#[derive(Clone, Debug)]
pub struct KeyBindings {
    pub map: HashMap<(KeyCode, Modifiers), Action>,
}

impl Default for KeyBindings {
    fn default() -> Self {
        // Sensible defaults; user config merges onto (overrides) these.
        let pairs: &[(&str, Action)] = &[
            ("cmd+c", Action::Copy),
            ("cmd+v", Action::Paste),
            // v0.9 fix: Cmd+Shift+V also pastes (common terminal convention;
            // matches macOS "Paste and Match Style" habit).
            ("cmd+shift+v", Action::Paste),
            ("cmd+shift+comma", Action::ReloadConfig),
            ("shift+page_up", Action::ScrollPageUp),
            ("shift+page_down", Action::ScrollPageDown),
            ("cmd+up", Action::ScrollLineUp),
            ("cmd+down", Action::ScrollLineDown),
            ("cmd+home", Action::ScrollToTop),
            ("cmd+end", Action::ScrollToBottom),
            ("cmd+shift+b", Action::ToggleBlockPanel),
            ("cmd+p", Action::ToggleCommandPalette),
            ("cmd+equals", Action::ZoomIn),
            ("cmd+minus", Action::ZoomOut),
            ("cmd+0", Action::ZoomReset),
            ("cmd+f", Action::FindInGrid),
            ("cmd+shift+t", Action::ToggleTheme),
            // v0.9 H1: tab management shortcuts.
            ("cmd+t", Action::NewTab),
            ("cmd+w", Action::CloseTab),
            ("cmd+shift+right_bracket", Action::NextTab),
            ("cmd+shift+left_bracket", Action::PrevTab),
            // v1.0 S1: Settings panel (macOS-standard Cmd+,).
            ("cmd+comma", Action::ToggleSettings),
        ];
        let mut map = HashMap::new();
        for (binding, action) in pairs {
            if let Some((k, m)) = parse_binding(binding) {
                map.insert((k, m), *action);
            }
        }
        Self { map }
    }
}

impl KeyBindings {
    /// Build from user overrides merged onto the defaults. Unparseable
    /// bindings are skipped (with a `warn!`).
    pub fn from_overrides(overrides: &HashMap<String, Action>) -> Self {
        let mut kb = Self::default();
        for (binding, action) in overrides {
            match parse_binding(binding) {
                Some((k, m)) => {
                    kb.map.insert((k, m), *action);
                }
                None => {
                    tracing::warn!(binding, "skipping unparseable keybinding");
                }
            }
        }
        kb
    }

    /// Look up the action for a key + modifier combo.
    pub fn lookup(&self, key: KeyCode, mods: Modifiers) -> Option<Action> {
        self.map.get(&(key, mods)).copied()
    }
}

// ── Config (deserialized from TOML) ────────────────────────────────────

/// Top-level config. Every section is optional (`#[serde(default)]`).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub font: FontConfig,
    pub theme: ThemeConfig,
    pub window: WindowConfig,
    pub scrollback: ScrollbackConfig,
    pub editor: EditorConfig,
    pub logo: LogoConfig,
    /// Raw user keybinding overrides: `"cmd+x" = "copy"`. Resolved later via
    /// [`Config::keybindings`] (merged onto defaults).
    pub keybindings: HashMap<String, Action>,
}

impl Config {
    /// Load config from the well-known path. Missing file or parse error
    /// falls back to defaults (parse errors are logged).
    pub fn load() -> Self {
        let path = Self::config_path();
        let Some(path) = path else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
                tracing::warn!(path = %path.display(), error = %e, "failed to parse config; using defaults");
                Self::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "failed to read config; using defaults");
                Self::default()
            }
        }
    }

    /// v1.0 S2: Save config to disk, preserving comments and unknown fields.
    ///
    /// Uses `toml_edit` so existing comments and layout survive the write.
    /// Only writes fields that differ from their defaults — fields the user
    /// never customized are left untouched (or omitted if the file is new).
    /// Writes atomically: the new content is written to `<path>.tmp` first,
    /// then renamed over `<path>`.
    ///
    /// Returns an error when:
    ///   - the config path can't be resolved (`HOME`/`XDG_CONFIG_HOME` unset)
    ///   - the parent directory can't be created
    ///   - the temp-file write or rename fails
    pub fn save(&self) -> Result<(), ConfigSaveError> {
        let path = Self::config_path().ok_or(ConfigSaveError::NoConfigPath)?;
        self.save_to_path(&path)
    }

    /// v1.0 S2: Save config to an explicit path. Used by [`save`] and by
    /// tests (which pass a tempdir path).
    pub fn save_to_path(&self, path: &std::path::Path) -> Result<(), ConfigSaveError> {
        // Read the existing file as a toml_edit document (preserves comments
        // and unknown fields). If the file doesn't exist or fails to parse,
        // start from an empty document.
        let existing = std::fs::read_to_string(path).ok();
        let mut doc: toml_edit::DocumentMut = existing
            .as_deref()
            .and_then(|s| s.parse::<toml_edit::DocumentMut>().ok())
            .unwrap_or_default();

        // [font] section.
        let default_font = FontConfig::default();
        let font_entry = doc.entry("font").or_insert_with(toml_edit::table);
        if font_entry.is_none() {
            *font_entry = toml_edit::table();
        }
        let font = font_entry.as_table_mut().expect("font is a table");
        set_string_if_diff(font, "family", &self.font.family, &default_font.family);
        set_f32_if_diff(font, "size", self.font.size, default_font.size);
        set_string_if_diff(
            font,
            "cjk_family",
            &self.font.cjk_family,
            &default_font.cjk_family,
        );
        set_string_if_diff(
            font,
            "emoji_family",
            &self.font.emoji_family,
            &default_font.emoji_family,
        );
        set_f32_if_diff(
            font,
            "line_height",
            self.font.line_height,
            default_font.line_height,
        );

        // [theme] section.
        let default_theme = ThemeConfig::default();
        let theme_entry = doc.entry("theme").or_insert_with(toml_edit::table);
        if theme_entry.is_none() {
            *theme_entry = toml_edit::table();
        }
        let theme = theme_entry.as_table_mut().expect("theme is a table");
        set_string_if_diff(theme, "name", &self.theme.name, &default_theme.name);
        set_opt_string(theme, "foreground", &self.theme.foreground);
        set_opt_string(theme, "background", &self.theme.background);
        set_opt_string(theme, "cursor", &self.theme.cursor);
        set_opt_string(theme, "selection", &self.theme.selection);
        set_opt_string(theme, "accent", &self.theme.accent);
        set_opt_string(theme, "accent_dim", &self.theme.accent_dim);
        set_opt_string(theme, "separator", &self.theme.separator);
        // palette: only write if non-empty (non-default).
        if !self.theme.palette.is_empty() {
            let mut arr = toml_edit::Array::new();
            for hex in &self.theme.palette {
                arr.push(hex.as_str());
            }
            theme["palette"] = toml_edit::Item::Value(toml_edit::Value::Array(arr));
        }
        // follow_system: only write if non-default (true).
        if self.theme.follow_system {
            theme["follow_system"] = toml_edit::value(true);
        } else if theme.contains_key("follow_system") {
            theme["follow_system"] = toml_edit::value(false);
        }
        // light_name / dark_name: write if set.
        if let Some(ln) = &self.theme.light_name {
            theme["light_name"] = toml_edit::value(ln.as_str());
        }
        if let Some(dn) = &self.theme.dark_name {
            theme["dark_name"] = toml_edit::value(dn.as_str());
        }
        // [theme.syntax] subsection.
        if let Some(syn) = &self.theme.syntax {
            let mut syntax_table = toml_edit::table();
            let st = syntax_table.as_table_mut().unwrap();
            set_opt_string(st, "command", &syn.command);
            set_opt_string(st, "flag", &syn.flag);
            set_opt_string(st, "path", &syn.path);
            set_opt_string(st, "string", &syn.string);
            set_opt_string(st, "number", &syn.number);
            set_opt_string(st, "variable", &syn.variable);
            set_opt_string(st, "operator", &syn.operator);
            set_opt_string(st, "comment", &syn.comment);
            set_opt_string(st, "default", &syn.default);
            // Only write the [theme.syntax] table if at least one field is
            // set (avoid emitting an empty `[theme.syntax]` section).
            if st.iter().count() > 0 {
                theme["syntax"] = syntax_table;
            }
        }

        // [window] section.
        let default_window = WindowConfig::default();
        let window_entry = doc.entry("window").or_insert_with(toml_edit::table);
        if window_entry.is_none() {
            *window_entry = toml_edit::table();
        }
        let window = window_entry.as_table_mut().expect("window is a table");
        set_u32_if_diff(window, "width", self.window.width, default_window.width);
        set_u32_if_diff(window, "height", self.window.height, default_window.height);
        set_string_if_diff(window, "title", &self.window.title, &default_window.title);
        set_f32_if_diff(
            window,
            "opacity",
            self.window.opacity,
            default_window.opacity,
        );
        set_u32_if_diff(
            window,
            "padding_x",
            self.window.padding_x,
            default_window.padding_x,
        );
        set_u32_if_diff(
            window,
            "padding_y",
            self.window.padding_y,
            default_window.padding_y,
        );

        // [scrollback] section.
        let default_scrollback = ScrollbackConfig::default();
        let scrollback_entry = doc.entry("scrollback").or_insert_with(toml_edit::table);
        if scrollback_entry.is_none() {
            *scrollback_entry = toml_edit::table();
        }
        let scrollback = scrollback_entry
            .as_table_mut()
            .expect("scrollback is a table");
        set_usize_if_diff(
            scrollback,
            "lines",
            self.scrollback.lines,
            default_scrollback.lines,
        );

        // [editor] section.
        if self.editor.submit_on_ctrl_enter {
            let editor_entry = doc.entry("editor").or_insert_with(toml_edit::table);
            if editor_entry.is_none() {
                *editor_entry = toml_edit::table();
            }
            let editor = editor_entry.as_table_mut().expect("editor is a table");
            editor["submit_on_ctrl_enter"] = toml_edit::value(true);
        }

        // [logo] section — write variant when non-default; clear it when
        // default so a later switch back to Cool doesn't get overridden by
        // a stale `variant = "warm"` left in the file.
        let default_logo = LogoConfig::default();
        if self.logo.variant != default_logo.variant {
            let logo_entry = doc.entry("logo").or_insert_with(toml_edit::table);
            if logo_entry.is_none() {
                *logo_entry = toml_edit::table();
            }
            let logo = logo_entry.as_table_mut().expect("logo is a table");
            logo["variant"] = toml_edit::value(self.logo.variant.as_str());
        } else if let Some(logo_entry) = doc.get_mut("logo") {
            // Default variant: remove any stale `variant` key so a saved
            // non-default value doesn't override the default on next load.
            if let Some(logo) = logo_entry.as_table_mut() {
                logo.remove("variant");
                // If the [logo] table is now empty, remove it entirely to
                // keep the config file clean.
                if logo.iter().count() == 0 {
                    doc.remove("logo");
                }
            }
        }

        // [keybindings] section.
        if !self.keybindings.is_empty() {
            let mut kb_table = toml_edit::table();
            let kt = kb_table.as_table_mut().unwrap();
            for (binding, action) in &self.keybindings {
                let action_str = match action {
                    Action::Copy => "copy",
                    Action::Paste => "paste",
                    Action::ReloadConfig => "reload_config",
                    Action::ScrollPageUp => "scroll_page_up",
                    Action::ScrollPageDown => "scroll_page_down",
                    Action::ScrollLineUp => "scroll_line_up",
                    Action::ScrollLineDown => "scroll_line_down",
                    Action::ScrollToTop => "scroll_to_top",
                    Action::ScrollToBottom => "scroll_to_bottom",
                    Action::ToggleBlockPanel => "toggle_block_panel",
                    Action::ToggleCommandPalette => "toggle_command_palette",
                    Action::ZoomIn => "zoom_in",
                    Action::ZoomOut => "zoom_out",
                    Action::ZoomReset => "zoom_reset",
                    Action::FindInGrid => "find_in_grid",
                    Action::ToggleTheme => "toggle_theme",
                    Action::NewTab => "new_tab",
                    Action::CloseTab => "close_tab",
                    Action::NextTab => "next_tab",
                    Action::PrevTab => "prev_tab",
                    Action::ToggleSettings => "toggle_settings",
                };
                kt.insert(binding, toml_edit::value(action_str));
            }
            doc["keybindings"] = kb_table;
        }

        // Atomic write: <path>.tmp → rename → <path>.
        let parent = path.parent().ok_or(ConfigSaveError::NoParentDir)?;
        std::fs::create_dir_all(parent).map_err(ConfigSaveError::Io)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, doc.to_string()).map_err(ConfigSaveError::Io)?;
        std::fs::rename(&tmp, path).map_err(ConfigSaveError::Io)?;
        Ok(())
    }

    /// The config file path: `$XDG_CONFIG_HOME/weft/config.toml`, else
    /// `~/.config/weft/config.toml`. `None` when neither env var is set.
    pub fn config_path() -> Option<PathBuf> {
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
            return Some(PathBuf::from(xdg).join("weft").join("config.toml"));
        }
        std::env::var_os("HOME").map(|h| {
            PathBuf::from(h)
                .join(".config")
                .join("weft")
                .join("config.toml")
        })
    }

    /// Resolve the active theme.
    pub fn theme(&self) -> Theme {
        Theme::resolve(&self.theme)
    }

    /// Resolve keybindings (defaults + user overrides).
    pub fn keybindings(&self) -> KeyBindings {
        KeyBindings::from_overrides(&self.keybindings)
    }
}

/// v1.0 S2: Error returned by [`Config::save`].
#[derive(Debug)]
pub enum ConfigSaveError {
    /// Neither `HOME` nor `XDG_CONFIG_HOME` is set, so the config path
    /// can't be resolved.
    NoConfigPath,
    /// The config path has no parent directory (shouldn't happen in
    /// practice, but handle it gracefully).
    NoParentDir,
    /// An I/O error occurred while creating the parent dir, writing the
    /// temp file, or renaming it over the target.
    Io(std::io::Error),
}

impl std::fmt::Display for ConfigSaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoConfigPath => write!(
                f,
                "cannot save config: HOME and XDG_CONFIG_HOME are both unset"
            ),
            Self::NoParentDir => write!(f, "config path has no parent directory"),
            Self::Io(e) => write!(f, "config save failed: {e}"),
        }
    }
}

impl std::error::Error for ConfigSaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

// ── toml_edit helper functions (S2) ─────────────────────────────────────

/// Set a string field in a toml_edit table. If the new value differs from
/// the default, write it; if it equals the default, remove any existing
/// entry so the default takes effect on reload.
fn set_string_if_diff(table: &mut toml_edit::Table, key: &str, new: &str, default: &str) {
    if new == default {
        // v1.0 fix: remove the key so the default takes effect on reload.
        // Previously this returned early without touching the table, leaving
        // any old value in the file — so saving `name = "weft-warm"` (the
        // default) didn't clear a previously-saved `name = "solarized-dark"`.
        if table.contains_key(key) {
            table.remove(key);
        }
        return;
    }
    table.insert(key, toml_edit::value(new));
}

/// Set an optional string field. If `new` is `Some`, write it; if `None`,
/// remove any existing entry for the key (the override is cleared).
fn set_opt_string(table: &mut toml_edit::Table, key: &str, new: &Option<String>) {
    match new {
        Some(s) => {
            table.insert(key, toml_edit::value(s.as_str()));
        }
        None => {
            // Don't forcibly remove — the user may have a comment they want
            // to keep. Just leave any existing entry in place.
        }
    }
}

fn set_f32_if_diff(table: &mut toml_edit::Table, key: &str, new: f32, default: f32) {
    if (new - default).abs() < f32::EPSILON {
        return;
    }
    table.insert(key, toml_edit::value(f64::from(new)));
}

fn set_u32_if_diff(table: &mut toml_edit::Table, key: &str, new: u32, default: u32) {
    if new == default {
        return;
    }
    table.insert(key, toml_edit::value(i64::from(new)));
}

fn set_usize_if_diff(table: &mut toml_edit::Table, key: &str, new: usize, default: usize) {
    if new == default {
        return;
    }
    table.insert(key, toml_edit::value(i64::try_from(new).unwrap_or(0)));
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct FontConfig {
    pub family: String,
    pub size: f32,
    pub cjk_family: String,
    pub emoji_family: String,
    pub line_height: f32,
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            family: "Menlo".into(),
            size: 14.0,
            cjk_family: "PingFang SC".into(),
            emoji_family: "Apple Color Emoji".into(),
            line_height: 1.2,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct ThemeConfig {
    pub name: String,
    pub foreground: Option<String>,
    pub background: Option<String>,
    pub cursor: Option<String>,
    pub selection: Option<String>,
    /// v0.8: signature accent (prompt ❯, scrollbar thumb, cursor glow).
    pub accent: Option<String>,
    /// v0.8: dimmed accent (chevrons, completion hover, secondary chrome).
    pub accent_dim: Option<String>,
    /// v0.8: block separator color.
    pub separator: Option<String>,
    pub palette: Vec<String>,
    /// v0.9 U-D1: follow macOS system appearance (light/dark). When true,
    /// `light_name` / `dark_name` override `name` based on the current
    /// system appearance. Manual `Cmd+Shift+T` toggle is a no-op while
    /// this is enabled (the system overrides it on the next poll).
    pub follow_system: bool,
    /// v0.9 U-D1: theme name to use when system appearance is Light.
    /// Defaults to "weft-light" when None.
    pub light_name: Option<String>,
    /// v0.9 U-D1: theme name to use when system appearance is Dark.
    /// Defaults to "weft-warm" when None.
    pub dark_name: Option<String>,
    /// v1.0 S5: per-syntax-token color overrides. Each field is an optional
    /// hex string (`"#rrggbb"`); when present it overrides the base theme's
    /// `SyntaxColors` field of the same name. Applied after the inline
    /// color overrides in [`Theme::resolve_named`].
    pub syntax: Option<SyntaxConfig>,
}

/// v1.0 S5: TOML-facing syntax color overrides. All fields optional; absent
/// fields inherit from the resolved base theme. Mirrors the 9 fields of
/// [`SyntaxColors`].
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct SyntaxConfig {
    pub command: Option<String>,
    pub flag: Option<String>,
    pub path: Option<String>,
    pub string: Option<String>,
    pub number: Option<String>,
    pub variable: Option<String>,
    pub operator: Option<String>,
    pub comment: Option<String>,
    pub default: Option<String>,
}

// Manual Default (deriving would give name = "").
impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            name: "weft-warm".into(),
            foreground: None,
            background: None,
            cursor: None,
            selection: None,
            accent: None,
            accent_dim: None,
            separator: None,
            palette: Vec::new(),
            follow_system: false,
            light_name: None,
            dark_name: None,
            syntax: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct WindowConfig {
    pub width: u32,
    pub height: u32,
    pub title: String,
    pub opacity: f32,
    pub padding_x: u32,
    pub padding_y: u32,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            width: 800,
            height: 600,
            title: "Weft".into(),
            opacity: 1.0,
            padding_x: 0,
            padding_y: 0,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct ScrollbackConfig {
    pub lines: usize,
}

impl Default for ScrollbackConfig {
    fn default() -> Self {
        Self { lines: 10_000 }
    }
}

/// Editor (input-box) options.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct EditorConfig {
    /// If true, `Ctrl+Enter` submits and plain `Enter` inserts a newline
    /// (Warp default). If false (default), `Enter` submits and `Shift+Enter`
    /// inserts a newline.
    pub submit_on_ctrl_enter: bool,
}

/// v1.0 Logo variant — the app icon shown in the Dock / app switcher.
/// Not theme-bound: the user picks a preferred variant in Settings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LogoVariant {
    /// Cool dark — `#0b0e14` bg + neon cyan W.
    /// Also the fallback for unknown config values.
    #[default]
    Cool,
    /// Warm dark — `#221c18` bg + amber W.
    Warm,
    /// Light — `#f5f5f7` bg + deep cyan W.
    Light,
    /// Transparent — no bg fill, only grid + W.
    Transparent,
}

impl<'de> Deserialize<'de> for LogoVariant {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(LogoVariant::from_str(&s))
    }
}

impl LogoVariant {
    /// All variants in display order.
    pub const ALL: [LogoVariant; 4] = [
        LogoVariant::Cool,
        LogoVariant::Warm,
        LogoVariant::Light,
        LogoVariant::Transparent,
    ];

    /// Human-readable label for the Settings UI.
    pub fn label(self) -> &'static str {
        match self {
            LogoVariant::Cool => "Cool (dark cyan)",
            LogoVariant::Warm => "Warm (dark amber)",
            LogoVariant::Light => "Light (pale cyan)",
            LogoVariant::Transparent => "Transparent",
        }
    }

    /// Identifier used in config.toml `[logo] variant = "..."`.
    pub fn as_str(self) -> &'static str {
        match self {
            LogoVariant::Cool => "cool",
            LogoVariant::Warm => "warm",
            LogoVariant::Light => "light",
            LogoVariant::Transparent => "transparent",
        }
    }

    /// Parse from a config string. Unknown values fall back to `Cool`.
    /// Infallible by design (never returns Err / always yields a valid variant).
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s.trim() {
            "warm" => LogoVariant::Warm,
            "light" => LogoVariant::Light,
            "transparent" => LogoVariant::Transparent,
            _ => LogoVariant::Cool,
        }
    }
}

/// v1.0 Logo config — Dock icon variant selection.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct LogoConfig {
    /// Selected logo variant.
    pub variant: LogoVariant,
}

impl Default for LogoConfig {
    fn default() -> Self {
        Self {
            variant: LogoVariant::Cool,
        }
    }
}

// ── Parsing helpers ────────────────────────────────────────────────────

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_dark_menlo_10000() {
        let c = Config::default();
        assert_eq!(c.theme.name, "weft-warm");
        assert_eq!(c.font.family, "Menlo");
        assert_eq!(c.font.size, 14.0);
        assert_eq!(c.scrollback.lines, 10_000);
        assert_eq!(c.window.width, 800);
        // L4 window fields: opaque by default, no content padding.
        assert_eq!(c.window.opacity, 1.0);
        assert_eq!(c.window.padding_x, 0);
        assert_eq!(c.window.padding_y, 0);
    }

    #[test]
    fn empty_toml_uses_defaults() {
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.font.family, "Menlo");
        assert_eq!(c.theme.name, "weft-warm");
    }

    #[test]
    fn partial_config_merges_defaults() {
        let toml_text = r#"
[font]
size = 18.0

[theme]
name = "weft-light"
"#;
        let c: Config = toml::from_str(toml_text).unwrap();
        assert_eq!(c.font.size, 18.0);
        assert_eq!(c.font.family, "Menlo"); // default retained
        assert_eq!(c.theme.name, "weft-light");
        assert_eq!(c.scrollback.lines, 10_000); // default retained
    }

    #[test]
    fn parse_hex_rrggbb() {
        assert_eq!(parse_hex("#ff8800"), Some(Color::rgb(255, 136, 0)));
        assert_eq!(parse_hex("1a2b3c"), Some(Color::rgb(0x1a, 0x2b, 0x3c)));
    }

    #[test]
    fn parse_hex_short_and_alpha() {
        assert_eq!(parse_hex("#f0a"), Some(Color::rgb(255, 0, 170)));
        let c = parse_hex("#80808080").unwrap();
        assert_eq!((c.r, c.g, c.b, c.a), (0x80, 0x80, 0x80, 0x80));
    }

    #[test]
    fn parse_hex_rejects_garbage() {
        assert_eq!(parse_hex("nope"), None);
        assert_eq!(parse_hex("#12345"), None);
    }

    #[test]
    fn theme_resolve_applies_overrides() {
        let cfg = ThemeConfig {
            name: "weft-dark".into(), // legacy alias → resolves to weft-warm
            foreground: Some("#abcdef".into()),
            palette: vec!["#112233".into(), "#445566".into()],
            ..Default::default()
        };
        let theme = Theme::resolve(&cfg);
        assert_eq!(theme.foreground, Color::rgb(0xab, 0xcd, 0xef));
        assert_eq!(theme.palette[0], Color::rgb(0x11, 0x22, 0x33));
        assert_eq!(theme.palette[1], Color::rgb(0x44, 0x55, 0x66));
        // v0.8: weft-dark alias resolves to weft-warm (amber accent #d4a574).
        assert_eq!(theme.accent, Color::rgb(0xd4, 0xa5, 0x74));
    }

    #[test]
    fn weft_warm_default_has_warm_palette() {
        // v0.8 visual identity: warm brown bg + amber accent.
        let t = Theme::weft_warm();
        assert_eq!(t.background, Color::rgb(0x22, 0x1c, 0x18)); // warm brown
        assert_eq!(t.accent, Color::rgb(0xd4, 0xa5, 0x74)); // amber
        assert_eq!(t.accent_dim, Color::rgb(0x7a, 0x6a, 0x58)); // warm gray
                                                                // Syntax: all 9 colors distinct from background.
        let bg = t.background;
        for c in [
            t.syntax.command,
            t.syntax.flag,
            t.syntax.path,
            t.syntax.string,
            t.syntax.number,
            t.syntax.variable,
            t.syntax.operator,
            t.syntax.comment,
            t.syntax.default,
        ] {
            assert_ne!(c, bg, "syntax color must differ from background");
        }
        // flag == accent (design intent: flags carry the signature color).
        assert_eq!(t.syntax.flag, t.accent);
        // comment == accent_dim (Quiet: comments recede like dim chrome).
        assert_eq!(t.syntax.comment, t.accent_dim);
        // default == foreground.
        assert_eq!(t.syntax.default, t.foreground);
    }

    #[test]
    fn theme_unknown_name_falls_back_to_dark() {
        let cfg = ThemeConfig {
            name: "nonsense".into(),
            ..Default::default()
        };
        assert_eq!(Theme::resolve(&cfg), Theme::weft_dark());
    }

    #[test]
    fn warp_theme_resolves_by_name() {
        // v0.9 W2+: "warp" and "warp-dark" both resolve to the Warp dark theme.
        let cfg = ThemeConfig {
            name: "warp".into(),
            ..Default::default()
        };
        let theme = Theme::resolve(&cfg);
        assert_eq!(theme, Theme::warp_dark());
        // Also test the dashed alias.
        let cfg2 = ThemeConfig {
            name: "warp-dark".into(),
            ..Default::default()
        };
        assert_eq!(Theme::resolve(&cfg2), Theme::warp_dark());
    }

    #[test]
    fn warp_theme_has_coral_accent_and_distinct_syntax() {
        // v0.9 W2+: Warp's signature coral accent + 9 distinct syntax colors.
        let t = Theme::warp_dark();
        // Coral accent (#ff5d38) is the Warp signature.
        assert_eq!(t.accent, Color::rgb(0xff, 0x5d, 0x38));
        // flag == accent (consistent with weft_warm's design intent).
        assert_eq!(t.syntax.flag, t.accent);
        // default == foreground.
        assert_eq!(t.syntax.default, t.foreground);
        // All 9 syntax colors distinct from background (must be visible).
        let bg = t.background;
        for c in [
            t.syntax.command,
            t.syntax.flag,
            t.syntax.path,
            t.syntax.string,
            t.syntax.number,
            t.syntax.variable,
            t.syntax.operator,
            t.syntax.comment,
            t.syntax.default,
        ] {
            assert_ne!(c, bg, "syntax color must differ from background");
        }
    }

    #[test]
    fn warp_theme_supports_inline_overrides() {
        // v0.9 W2+: inline overrides apply on top of the warp base, just like
        // weft-warm. Verifies resolve_named() applies cfg overrides to warp.
        let cfg = ThemeConfig {
            name: "warp".into(),
            accent: Some("#00ff00".into()),
            ..Default::default()
        };
        let theme = Theme::resolve(&cfg);
        assert_eq!(theme.accent, Color::rgb(0x00, 0xff, 0x00));
        // Background is still the warp default (override only touched accent).
        assert_eq!(theme.background, Color::rgb(0x1b, 0x1b, 0x28));
    }

    #[test]
    fn classic_themes_resolve_by_name() {
        // v0.9 W2+: Dracula / Solarized Dark / Gruvbox Dark resolve by name
        // and match their constructors.
        for (name, expected) in [
            ("dracula", Theme::dracula()),
            ("solarized-dark", Theme::solarized_dark()),
            ("solarized_dark", Theme::solarized_dark()),
            ("solarized", Theme::solarized_dark()),
            ("gruvbox-dark", Theme::gruvbox_dark()),
            ("gruvbox_dark", Theme::gruvbox_dark()),
            ("gruvbox", Theme::gruvbox_dark()),
        ] {
            let cfg = ThemeConfig {
                name: name.into(),
                ..Default::default()
            };
            assert_eq!(Theme::resolve(&cfg), expected, "name = {name}");
        }
    }

    #[test]
    fn classic_themes_have_distinct_accent_and_visible_syntax() {
        // v0.9 W2+: each classic theme has a signature accent and all syntax
        // colors differ from the background (must be visible).
        for (name, theme) in [
            ("dracula", Theme::dracula()),
            ("solarized-dark", Theme::solarized_dark()),
            ("gruvbox-dark", Theme::gruvbox_dark()),
        ] {
            // accent must differ from background (otherwise it's invisible).
            assert_ne!(
                theme.accent, theme.background,
                "{name}: accent must differ from background"
            );
            // every syntax color must differ from background.
            let bg = theme.background;
            for c in [
                theme.syntax.command,
                theme.syntax.flag,
                theme.syntax.path,
                theme.syntax.string,
                theme.syntax.number,
                theme.syntax.variable,
                theme.syntax.operator,
                theme.syntax.comment,
                theme.syntax.default,
            ] {
                assert_ne!(
                    c, bg,
                    "{name}: syntax color {c:?} must differ from background"
                );
            }
        }
    }

    #[test]
    fn dracula_signature_colors() {
        // v0.9 W2+: verify Dracula's signature palette values.
        let t = Theme::dracula();
        assert_eq!(t.background, Color::rgb(0x28, 0x2a, 0x36));
        assert_eq!(t.foreground, Color::rgb(0xf8, 0xf8, 0xf2));
        assert_eq!(t.accent, Color::rgb(0xbd, 0x93, 0xf9)); // purple
        assert_eq!(t.syntax.flag, Color::rgb(0xff, 0x79, 0xc6)); // pink
    }

    #[test]
    fn solarized_signature_colors() {
        // v0.9 W2+: verify Solarized Dark's signature base03/base1/blue accent.
        let t = Theme::solarized_dark();
        assert_eq!(t.background, Color::rgb(0x00, 0x2b, 0x36)); // base03
        assert_eq!(t.foreground, Color::rgb(0x93, 0xa1, 0xa1)); // base1
        assert_eq!(t.accent, Color::rgb(0x26, 0x8b, 0xd2)); // blue
    }

    #[test]
    fn gruvbox_signature_colors() {
        // v0.9 W2+: verify Gruvbox Dark's signature bg/fg/orange accent.
        let t = Theme::gruvbox_dark();
        assert_eq!(t.background, Color::rgb(0x28, 0x28, 0x28));
        assert_eq!(t.foreground, Color::rgb(0xeb, 0xdb, 0xb2));
        assert_eq!(t.accent, Color::rgb(0xfe, 0x80, 0x19)); // orange
    }

    #[test]
    fn community_themes_resolve_by_name() {
        // v0.9 W2+: Nord / Tokyo Night / Catppuccin / One Dark / Monokai Pro.
        for (name, expected) in [
            ("nord", Theme::nord()),
            ("tokyo-night", Theme::tokyo_night()),
            ("tokyo_night", Theme::tokyo_night()),
            ("catppuccin", Theme::catppuccin_mocha()),
            ("catppuccin-mocha", Theme::catppuccin_mocha()),
            ("one-dark", Theme::one_dark()),
            ("one_dark", Theme::one_dark()),
            ("onedark", Theme::one_dark()),
            ("monokai-pro", Theme::monokai_pro()),
            ("monokai_pro", Theme::monokai_pro()),
            ("monokai", Theme::monokai_pro()),
        ] {
            let cfg = ThemeConfig {
                name: name.into(),
                ..Default::default()
            };
            assert_eq!(Theme::resolve(&cfg), expected, "name = {name}");
        }
    }

    #[test]
    fn community_themes_have_visible_syntax() {
        // v0.9 W2+: each community theme has accent != bg and all syntax
        // colors differ from background.
        for (name, theme) in [
            ("nord", Theme::nord()),
            ("tokyo-night", Theme::tokyo_night()),
            ("catppuccin", Theme::catppuccin_mocha()),
            ("one-dark", Theme::one_dark()),
            ("monokai-pro", Theme::monokai_pro()),
        ] {
            assert_ne!(
                theme.accent, theme.background,
                "{name}: accent must differ from background"
            );
            let bg = theme.background;
            for c in [
                theme.syntax.command,
                theme.syntax.flag,
                theme.syntax.path,
                theme.syntax.string,
                theme.syntax.number,
                theme.syntax.variable,
                theme.syntax.operator,
                theme.syntax.comment,
                theme.syntax.default,
            ] {
                assert_ne!(c, bg, "{name}: syntax color must differ from background");
            }
        }
    }

    #[test]
    fn nord_signature_colors() {
        let t = Theme::nord();
        assert_eq!(t.background, Color::rgb(0x2e, 0x34, 0x40)); // nord0
        assert_eq!(t.foreground, Color::rgb(0xd8, 0xde, 0xe9)); // nord4
        assert_eq!(t.accent, Color::rgb(0x88, 0xc0, 0xd0)); // nord8 frost
    }

    #[test]
    fn tokyo_night_signature_colors() {
        let t = Theme::tokyo_night();
        assert_eq!(t.background, Color::rgb(0x1a, 0x1b, 0x26));
        assert_eq!(t.foreground, Color::rgb(0xa9, 0xb1, 0xd6));
        assert_eq!(t.accent, Color::rgb(0x7a, 0xa2, 0xf7)); // blue
    }

    #[test]
    fn catppuccin_signature_colors() {
        let t = Theme::catppuccin_mocha();
        assert_eq!(t.background, Color::rgb(0x1e, 0x1e, 0x2e)); // base
        assert_eq!(t.foreground, Color::rgb(0xcd, 0xd6, 0xf4)); // text
        assert_eq!(t.accent, Color::rgb(0xcb, 0xa6, 0xf7)); // mauve
    }

    #[test]
    fn one_dark_signature_colors() {
        let t = Theme::one_dark();
        assert_eq!(t.background, Color::rgb(0x28, 0x2c, 0x34));
        assert_eq!(t.foreground, Color::rgb(0xab, 0xb2, 0xbf));
        assert_eq!(t.accent, Color::rgb(0x61, 0xaf, 0xef)); // blue
    }

    #[test]
    fn monokai_pro_signature_colors() {
        let t = Theme::monokai_pro();
        assert_eq!(t.background, Color::rgb(0x2d, 0x2a, 0x2e));
        assert_eq!(t.foreground, Color::rgb(0xfc, 0xfc, 0xfa));
        assert_eq!(t.accent, Color::rgb(0xff, 0xd8, 0x66)); // yellow
    }

    #[test]
    fn load_theme_from_toml_file() {
        // v0.9 W2+: load a custom theme from a .toml file. Uses a unique
        // temp dir (process id + counter) to avoid parallel-test collisions.
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("weft-theme-test-{id}-toml"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("custom.toml"),
            "foreground = \"#abcdef\"\nbackground = \"#112233\"\naccent = \"#ff0000\"\n",
        )
        .unwrap();

        let theme = Theme::load_from_dir(&dir, "custom").expect("should load custom.toml");
        assert_eq!(theme.foreground, Color::rgb(0xab, 0xcd, 0xef));
        assert_eq!(theme.background, Color::rgb(0x11, 0x22, 0x33));
        assert_eq!(theme.accent, Color::rgb(0xff, 0x00, 0x00));
        // Unspecified fields (cursor, syntax, palette) inherit from weft_warm base.
        let warm = Theme::weft_warm();
        assert_eq!(theme.cursor, warm.cursor);
        assert_eq!(theme.syntax.command, warm.syntax.command);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_theme_from_yaml_file() {
        // v0.9 W2+: load a custom theme from a .yaml file.
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("weft-theme-test-{id}-yaml"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("custom.yaml"),
            "foreground: \"#abcdef\"\nbackground: \"#112233\"\naccent: \"#ff0000\"\n",
        )
        .unwrap();

        let theme = Theme::load_from_dir(&dir, "custom").expect("should load custom.yaml");
        assert_eq!(theme.foreground, Color::rgb(0xab, 0xcd, 0xef));
        assert_eq!(theme.background, Color::rgb(0x11, 0x22, 0x33));
        assert_eq!(theme.accent, Color::rgb(0xff, 0x00, 0x00));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_theme_missing_file_returns_none() {
        // v0.9 W2+: when no file matches, returns None (falls back to default).
        let dir = std::env::temp_dir().join("weft-theme-test-nonexistent");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let result = Theme::load_from_dir(&dir, "does-not-exist");
        assert!(result.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_theme_with_palette_override() {
        // v0.9 W2+: palette array in theme file overrides ANSI slots.
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("weft-theme-test-{id}-palette"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("pal.toml"),
            "palette = [\"#000000\", \"#ff0000\", \"#00ff00\"]\n",
        )
        .unwrap();

        let theme = Theme::load_from_dir(&dir, "pal").expect("should load pal.toml");
        assert_eq!(theme.palette[0], Color::rgb(0x00, 0x00, 0x00));
        assert_eq!(theme.palette[1], Color::rgb(0xff, 0x00, 0x00));
        assert_eq!(theme.palette[2], Color::rgb(0x00, 0xff, 0x00));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dark_and_light_differ() {
        assert_ne!(Theme::weft_dark(), Theme::weft_light());
    }

    #[test]
    fn parse_binding_modifiers() {
        let (k, m) = parse_binding("cmd+shift+c").unwrap();
        assert_eq!(k, KeyCode::Char('c'));
        assert!(m.contains(Modifiers::SUPER));
        assert!(m.contains(Modifiers::SHIFT));
        assert!(!m.contains(Modifiers::CONTROL));
    }

    #[test]
    fn parse_binding_named_key() {
        let (k, _) = parse_binding("shift+page_up").unwrap();
        assert_eq!(k, KeyCode::PageUp);
        let (k, _) = parse_binding("cmd+comma").unwrap();
        assert_eq!(k, KeyCode::Char(','));
        let (k, _) = parse_binding("cmd+f5").unwrap();
        assert_eq!(k, KeyCode::F(5));
    }

    #[test]
    fn parse_binding_rejects_unknown() {
        assert!(parse_binding("win+foobar").is_none());
    }

    #[test]
    fn keybindings_default_has_copy_paste() {
        let kb = KeyBindings::default();
        let copy = kb.lookup(KeyCode::Char('c'), Modifiers::SUPER);
        assert_eq!(copy, Some(Action::Copy));
        assert_eq!(
            kb.lookup(KeyCode::Char('v'), Modifiers::SUPER),
            Some(Action::Paste)
        );
        // v0.9 fix: Cmd+Shift+V also pastes.
        assert_eq!(
            kb.lookup(KeyCode::Char('v'), Modifiers::SUPER | Modifiers::SHIFT),
            Some(Action::Paste)
        );
    }

    #[test]
    fn keybindings_overrides_merge() {
        let mut overrides = HashMap::new();
        overrides.insert("cmd+x".into(), Action::Copy);
        let kb = KeyBindings::from_overrides(&overrides);
        // Override present.
        assert_eq!(
            kb.lookup(KeyCode::Char('x'), Modifiers::SUPER),
            Some(Action::Copy)
        );
        // Default retained.
        assert_eq!(
            kb.lookup(KeyCode::Char('v'), Modifiers::SUPER),
            Some(Action::Paste)
        );
    }

    #[test]
    fn config_keybindings_resolve() {
        let toml_text = r#"
[keybindings]
"cmd+x" = "copy"
"#;
        let c: Config = toml::from_str(toml_text).unwrap();
        let kb = c.keybindings();
        assert_eq!(
            kb.lookup(KeyCode::Char('x'), Modifiers::SUPER),
            Some(Action::Copy)
        );
    }

    #[test]
    fn load_missing_file_is_default() {
        // Point XDG_CONFIG_HOME to a non-existent directory so Config::load()
        // cannot find a user config file, guaranteeing we hit the default path.
        // Without this isolation the test picks up the developer's real
        // config.toml and fails on the font-family assertion.
        let tmp = std::env::temp_dir().join("weft-test-nonexistent");
        std::env::set_var("XDG_CONFIG_HOME", &tmp);
        let c = Config::load();
        assert_eq!(c.font.family, "Menlo");
    }

    #[test]
    fn editor_submit_on_ctrl_enter_parses() {
        let c: Config = toml::from_str("[editor]\nsubmit_on_ctrl_enter = true\n").unwrap();
        assert!(c.editor.submit_on_ctrl_enter);
        // default is false
        let d: Config = toml::from_str("").unwrap();
        assert!(!d.editor.submit_on_ctrl_enter);
    }

    #[test]
    fn zoom_actions_have_default_keybindings() {
        let kb = KeyBindings::default();
        // Cmd+= → ZoomIn, Cmd+- → ZoomOut, Cmd+0 → ZoomReset.
        assert_eq!(
            kb.lookup(KeyCode::Char('='), Modifiers::SUPER),
            Some(Action::ZoomIn)
        );
        assert_eq!(
            kb.lookup(KeyCode::Char('-'), Modifiers::SUPER),
            Some(Action::ZoomOut)
        );
        assert_eq!(
            kb.lookup(KeyCode::Char('0'), Modifiers::SUPER),
            Some(Action::ZoomReset)
        );
    }

    #[test]
    fn zoom_actions_serde_roundtrip() {
        // The serde rename must match what users would write in config.toml.
        // Action only derives Deserialize (config is read-only), so we
        // round-trip via a serde_json string (matching the rename attribute).
        for (action, name) in [
            (Action::ZoomIn, "zoom_in"),
            (Action::ZoomOut, "zoom_out"),
            (Action::ZoomReset, "zoom_reset"),
        ] {
            let s = format!("\"{name}\"");
            let back: Action = serde_json::from_str(&s).unwrap();
            assert_eq!(back, action, "serde roundtrip failed for {name}");
        }
    }

    // ── v1.0 S5: [theme.syntax] config override tests ─────────────────

    #[test]
    fn syntax_config_default_is_all_none() {
        let s = SyntaxConfig::default();
        assert!(s.command.is_none());
        assert!(s.flag.is_none());
        assert!(s.path.is_none());
        assert!(s.string.is_none());
        assert!(s.number.is_none());
        assert!(s.variable.is_none());
        assert!(s.operator.is_none());
        assert!(s.comment.is_none());
        assert!(s.default.is_none());
    }

    #[test]
    fn theme_config_syntax_defaults_none() {
        // ThemeConfig::default() should leave syntax as None (no overrides).
        let cfg = ThemeConfig::default();
        assert!(cfg.syntax.is_none());
    }

    #[test]
    fn syntax_override_parses_from_toml() {
        let toml_text = r##"
[theme]
name = "weft-warm"

[theme.syntax]
command = "#ff0000"
flag = "#00ff00"
path = "#0000ff"
"##;
        let c: Config = toml::from_str(toml_text).unwrap();
        let syn = c.theme.syntax.expect("syntax section should parse");
        assert_eq!(syn.command.as_deref(), Some("#ff0000"));
        assert_eq!(syn.flag.as_deref(), Some("#00ff00"));
        assert_eq!(syn.path.as_deref(), Some("#0000ff"));
        // Unspecified fields are None.
        assert!(syn.string.is_none());
        assert!(syn.number.is_none());
        assert!(syn.comment.is_none());
    }

    #[test]
    fn syntax_override_applies_to_resolved_theme() {
        // v1.0 S5: [theme.syntax] fields override the base theme's
        // SyntaxColors. Specified fields change; unspecified fields retain
        // the base theme's value.
        let warm = Theme::weft_warm();
        let cfg = ThemeConfig {
            name: "weft-warm".into(),
            syntax: Some(SyntaxConfig {
                command: Some("#ff0000".into()),
                flag: Some("#00ff00".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let theme = Theme::resolve(&cfg);
        // Overridden fields.
        assert_eq!(theme.syntax.command, Color::rgb(0xff, 0x00, 0x00));
        assert_eq!(theme.syntax.flag, Color::rgb(0x00, 0xff, 0x00));
        // Unspecified fields inherit from the base theme.
        assert_eq!(theme.syntax.path, warm.syntax.path);
        assert_eq!(theme.syntax.string, warm.syntax.string);
        assert_eq!(theme.syntax.comment, warm.syntax.comment);
        assert_eq!(theme.syntax.default, warm.syntax.default);
    }

    #[test]
    fn syntax_override_all_nine_fields() {
        let cfg = ThemeConfig {
            name: "weft-warm".into(),
            syntax: Some(SyntaxConfig {
                command: Some("#111111".into()),
                flag: Some("#222222".into()),
                path: Some("#333333".into()),
                string: Some("#444444".into()),
                number: Some("#555555".into()),
                variable: Some("#666666".into()),
                operator: Some("#777777".into()),
                comment: Some("#888888".into()),
                default: Some("#999999".into()),
            }),
            ..Default::default()
        };
        let theme = Theme::resolve(&cfg);
        assert_eq!(theme.syntax.command, Color::rgb(0x11, 0x11, 0x11));
        assert_eq!(theme.syntax.flag, Color::rgb(0x22, 0x22, 0x22));
        assert_eq!(theme.syntax.path, Color::rgb(0x33, 0x33, 0x33));
        assert_eq!(theme.syntax.string, Color::rgb(0x44, 0x44, 0x44));
        assert_eq!(theme.syntax.number, Color::rgb(0x55, 0x55, 0x55));
        assert_eq!(theme.syntax.variable, Color::rgb(0x66, 0x66, 0x66));
        assert_eq!(theme.syntax.operator, Color::rgb(0x77, 0x77, 0x77));
        assert_eq!(theme.syntax.comment, Color::rgb(0x88, 0x88, 0x88));
        assert_eq!(theme.syntax.default, Color::rgb(0x99, 0x99, 0x99));
    }

    #[test]
    fn syntax_override_works_with_inline_color_override() {
        // v1.0 S5: [theme.syntax] composes with inline color overrides —
        // both apply, and they touch independent fields.
        let cfg = ThemeConfig {
            name: "weft-warm".into(),
            accent: Some("#abcdef".into()),
            syntax: Some(SyntaxConfig {
                command: Some("#ff0000".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let theme = Theme::resolve(&cfg);
        // Inline accent override.
        assert_eq!(theme.accent, Color::rgb(0xab, 0xcd, 0xef));
        // Syntax override.
        assert_eq!(theme.syntax.command, Color::rgb(0xff, 0x00, 0x00));
        // Unspecified syntax fields inherit base.
        let warm = Theme::weft_warm();
        assert_eq!(theme.syntax.flag, warm.syntax.flag);
    }

    #[test]
    fn syntax_override_invalid_hex_is_silently_ignored() {
        // v1.0 S5: an invalid hex string is skipped (parse_hex returns None),
        // leaving the base theme's value intact. This matches the behavior of
        // the existing inline color overrides.
        let warm = Theme::weft_warm();
        let cfg = ThemeConfig {
            name: "weft-warm".into(),
            syntax: Some(SyntaxConfig {
                command: Some("not-a-color".into()),
                flag: Some("#00ff00".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let theme = Theme::resolve(&cfg);
        // Invalid command override → retained from base.
        assert_eq!(theme.syntax.command, warm.syntax.command);
        // Valid flag override → applied.
        assert_eq!(theme.syntax.flag, Color::rgb(0x00, 0xff, 0x00));
    }

    #[test]
    fn syntax_override_applies_to_all_themes() {
        // v1.0 S5: syntax overrides apply regardless of the base theme name.
        // Verify with warp_dark.
        let warp = Theme::warp_dark();
        let cfg = ThemeConfig {
            name: "warp".into(),
            syntax: Some(SyntaxConfig {
                command: Some("#abcdef".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let theme = Theme::resolve(&cfg);
        assert_eq!(theme.syntax.command, Color::rgb(0xab, 0xcd, 0xef));
        // Unspecified fields inherit from warp base.
        assert_eq!(theme.syntax.flag, warp.syntax.flag);
        assert_eq!(theme.syntax.path, warp.syntax.path);
    }

    // ── v1.0 S2: Config::save() tests ──────────────────────────────────

    fn unique_tmp_path(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("weft-config-save-{pid}-{id}-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("config.toml")
    }

    #[test]
    fn save_default_config_creates_empty_file() {
        // Saving a default Config should produce a parseable file (with no
        // sections, since nothing differs from defaults).
        let path = unique_tmp_path("default");
        let cfg = Config::default();
        cfg.save_to_path(&path).expect("save should succeed");
        let text = std::fs::read_to_string(&path).unwrap();
        // Default config writes nothing (all fields match defaults).
        // The file should be valid TOML (possibly empty).
        let reloaded: Config = toml::from_str(&text).unwrap();
        assert_eq!(reloaded.font.family, "Menlo");
        assert_eq!(reloaded.theme.name, "weft-warm");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn save_and_reload_roundtrip() {
        // A Config with non-default fields should survive a save → reload
        // cycle.
        let path = unique_tmp_path("roundtrip");
        let cfg = Config {
            font: FontConfig {
                family: "Monaco".into(),
                size: 16.0,
                ..Default::default()
            },
            theme: ThemeConfig {
                name: "warp".into(),
                accent: Some("#ff0000".into()),
                syntax: Some(SyntaxConfig {
                    command: Some("#abcdef".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            window: WindowConfig {
                width: 1024,
                height: 768,
                ..Default::default()
            },
            scrollback: ScrollbackConfig { lines: 50_000 },
            ..Default::default()
        };
        cfg.save_to_path(&path).expect("save should succeed");
        let text = std::fs::read_to_string(&path).unwrap();
        let reloaded: Config = toml::from_str(&text).unwrap();
        assert_eq!(reloaded.font.family, "Monaco");
        assert_eq!(reloaded.font.size, 16.0);
        assert_eq!(reloaded.theme.name, "warp");
        assert_eq!(reloaded.theme.accent.as_deref(), Some("#ff0000"));
        assert_eq!(
            reloaded.theme.syntax.as_ref().unwrap().command.as_deref(),
            Some("#abcdef")
        );
        assert_eq!(reloaded.window.width, 1024);
        assert_eq!(reloaded.window.height, 768);
        assert_eq!(reloaded.scrollback.lines, 50_000);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn save_preserves_user_comments() {
        // v1.0 S2: toml_edit preserves comments. Write a file with a comment,
        // save over it, and verify the comment survives.
        let path = unique_tmp_path("comments");
        std::fs::write(
            &path,
            "# this is my comment\n[font]\nfamily = \"Menlo\"\nsize = 14.0\n",
        )
        .unwrap();
        let cfg = Config {
            font: FontConfig {
                size: 18.0,
                ..Default::default()
            },
            ..Default::default()
        };
        cfg.save_to_path(&path).expect("save should succeed");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("# this is my comment"),
            "comment should be preserved: {text}"
        );
        assert!(text.contains("size = 18"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn save_preserves_unknown_fields() {
        // v1.0 S2: unknown fields the user added should survive the save.
        let path = unique_tmp_path("unknown");
        std::fs::write(
            &path,
            "[font]\nsize = 14.0\n\n[unknown_section]\nfoo = \"bar\"\n",
        )
        .unwrap();
        let cfg = Config::default();
        cfg.save_to_path(&path).expect("save should succeed");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("foo = \"bar\""),
            "unknown field should survive"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn save_only_writes_non_default_fields() {
        // v1.0 S2: fields that match defaults shouldn't appear in the output
        // (keeps the file clean for a fresh save).
        let path = unique_tmp_path("nondefault");
        let cfg = Config {
            font: FontConfig {
                size: 18.0,
                ..Default::default()
            },
            ..Default::default()
        };
        cfg.save_to_path(&path).expect("save should succeed");
        let text = std::fs::read_to_string(&path).unwrap();
        // size should be written (non-default).
        assert!(text.contains("size = 18"));
        // family should NOT be written (matches default "Menlo").
        assert!(
            !text.contains("family = \"Menlo\""),
            "default family should not be written"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn save_creates_parent_dir() {
        // v1.0 S2: save should create the parent directory if it doesn't
        // exist.
        let dir =
            std::env::temp_dir().join(format!("weft-config-save-nested-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let nested = dir.join("a/b/c/config.toml");
        let cfg = Config::default();
        cfg.save_to_path(&nested).expect("save should succeed");
        assert!(nested.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_error_display() {
        // v1.0 S2: ConfigSaveError should have a useful Display impl.
        let e = ConfigSaveError::NoConfigPath;
        assert!(format!("{e}").contains("HOME"));
        let e = ConfigSaveError::NoParentDir;
        assert!(format!("{e}").contains("parent"));
        let e = ConfigSaveError::Io(std::io::Error::from_raw_os_error(13));
        assert!(format!("{e}").contains("config save failed"));
    }

    #[test]
    fn toggle_settings_has_default_keybinding() {
        // v1.0 S1: Cmd+, opens Settings.
        let kb = KeyBindings::default();
        assert_eq!(
            kb.lookup(KeyCode::Char(','), Modifiers::SUPER),
            Some(Action::ToggleSettings)
        );
    }

    #[test]
    fn toggle_settings_serde_roundtrip() {
        let s = "\"toggle_settings\"";
        let back: Action = serde_json::from_str(s).unwrap();
        assert_eq!(back, Action::ToggleSettings);
    }

    // ── T2: per-field Config::save roundtrip tests ────────────────────

    #[test]
    fn save_font_size_change() {
        let path = unique_tmp_path("font-size");
        let cfg = Config {
            font: FontConfig {
                size: 13.0,
                ..Default::default()
            },
            ..Default::default()
        };
        cfg.save_to_path(&path).expect("save should succeed");
        let text = std::fs::read_to_string(&path).unwrap();
        let reloaded: Config = toml::from_str(&text).unwrap();
        assert_eq!(reloaded.font.size, 13.0);
        // Other fields retain defaults.
        assert_eq!(reloaded.font.family, "Menlo");
        assert_eq!(reloaded.theme.name, "weft-warm");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn save_window_opacity_change() {
        let path = unique_tmp_path("win-opacity");
        let cfg = Config {
            window: WindowConfig {
                opacity: 0.85,
                ..Default::default()
            },
            ..Default::default()
        };
        cfg.save_to_path(&path).expect("save should succeed");
        let text = std::fs::read_to_string(&path).unwrap();
        let reloaded: Config = toml::from_str(&text).unwrap();
        // f32 round-trip through TOML f64 — compare with small epsilon.
        assert!((reloaded.window.opacity - 0.85).abs() < 1e-6);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn save_theme_name_change() {
        let path = unique_tmp_path("theme-name");
        let cfg = Config {
            theme: ThemeConfig {
                name: "dracula".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        cfg.save_to_path(&path).expect("save should succeed");
        let text = std::fs::read_to_string(&path).unwrap();
        let reloaded: Config = toml::from_str(&text).unwrap();
        assert_eq!(reloaded.theme.name, "dracula");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn save_scrollback_lines_change() {
        let path = unique_tmp_path("scrollback");
        let cfg = Config {
            scrollback: ScrollbackConfig { lines: 25_000 },
            ..Default::default()
        };
        cfg.save_to_path(&path).expect("save should succeed");
        let text = std::fs::read_to_string(&path).unwrap();
        let reloaded: Config = toml::from_str(&text).unwrap();
        assert_eq!(reloaded.scrollback.lines, 25_000);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn save_preserves_other_fields_when_one_changes() {
        // Changing only window.width should leave font, theme, and scrollback
        // at their configured (non-default) values after a save + reload cycle.
        let path = unique_tmp_path("preserve");
        let cfg = Config {
            font: FontConfig {
                family: "Monaco".into(),
                size: 16.0,
                ..Default::default()
            },
            theme: ThemeConfig {
                name: "nord".into(),
                ..Default::default()
            },
            window: WindowConfig {
                width: 1200,
                ..Default::default()
            },
            scrollback: ScrollbackConfig { lines: 50_000 },
            ..Default::default()
        };
        cfg.save_to_path(&path).expect("save should succeed");
        let text = std::fs::read_to_string(&path).unwrap();
        let reloaded: Config = toml::from_str(&text).unwrap();
        // The field we changed.
        assert_eq!(reloaded.window.width, 1200);
        // Other configured fields must survive unchanged.
        assert_eq!(reloaded.font.family, "Monaco");
        assert_eq!(reloaded.font.size, 16.0);
        assert_eq!(reloaded.theme.name, "nord");
        assert_eq!(reloaded.scrollback.lines, 50_000);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    // ── Logo config ───────────────────────────────────────────────────

    #[test]
    fn logo_variant_default_is_cool() {
        let c = Config::default();
        assert_eq!(c.logo.variant, LogoVariant::Cool);
    }

    #[test]
    fn logo_variant_round_trip_all_variants() {
        for v in LogoVariant::ALL {
            let toml_str = format!("[logo]\nvariant = \"{}\"\n", v.as_str());
            let cfg: Config = toml::from_str(&toml_str).unwrap();
            assert_eq!(cfg.logo.variant, v, "round-trip failed for {:?}", v);
        }
    }

    #[test]
    fn logo_variant_unknown_falls_back_to_cool() {
        let toml_str = "[logo]\nvariant = \"nonexistent\"\n";
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.logo.variant, LogoVariant::Cool);
    }

    #[test]
    fn logo_variant_save_writes_non_default() {
        let tmp = std::env::temp_dir().join("weft_logo_save_test.toml");
        let _ = std::fs::remove_file(&tmp);
        let mut cfg = Config::default();
        cfg.logo.variant = LogoVariant::Warm;
        cfg.save_to_path(&tmp).unwrap();
        let text = std::fs::read_to_string(&tmp).unwrap();
        assert!(text.contains("[logo]"), "missing [logo] section");
        assert!(
            text.contains("variant = \"warm\""),
            "missing variant = warm"
        );
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn logo_variant_default_not_written() {
        let tmp = std::env::temp_dir().join("weft_logo_default_test.toml");
        let _ = std::fs::remove_file(&tmp);
        let cfg = Config::default();
        cfg.save_to_path(&tmp).unwrap();
        let text = std::fs::read_to_string(&tmp).unwrap();
        assert!(
            !text.contains("[logo]"),
            "default logo should not be written"
        );
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn logo_variant_from_str_round_trip() {
        for v in LogoVariant::ALL {
            assert_eq!(LogoVariant::from_str(v.as_str()), v);
        }
    }

    #[test]
    fn logo_variant_label_not_empty() {
        for v in LogoVariant::ALL {
            assert!(!v.label().is_empty());
        }
    }

    #[test]
    fn logo_variant_preserves_other_sections() {
        let tmp = std::env::temp_dir().join("weft_logo_preserve_test.toml");
        let _ = std::fs::remove_file(&tmp);
        let initial = "[font]\nfamily = \"Monaco\"\nsize = 14.0\n\n[logo]\nvariant = \"light\"\n";
        std::fs::write(&tmp, initial).unwrap();
        let text = std::fs::read_to_string(&tmp).unwrap();
        let mut cfg: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.logo.variant, LogoVariant::Light);
        cfg.logo.variant = LogoVariant::Warm;
        cfg.save_to_path(&tmp).unwrap();
        let reloaded_text = std::fs::read_to_string(&tmp).unwrap();
        let reloaded: Config = toml::from_str(&reloaded_text).unwrap();
        assert_eq!(reloaded.font.family, "Monaco");
        assert_eq!(reloaded.font.size, 14.0);
        assert_eq!(reloaded.logo.variant, LogoVariant::Warm);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn logo_variant_save_cool_clears_stale_non_default() {
        // v1.0 fix: switching back to Cool (default) after saving a non-default
        // variant must clear the stale `variant = "warm"` from the file.
        // Otherwise the saved non-default value would override the default
        // on next load.
        let tmp = std::env::temp_dir().join("weft_logo_clear_stale_test.toml");
        let _ = std::fs::remove_file(&tmp);
        // Step 1: save with Warm — writes [logo] variant = "warm".
        let mut cfg = Config::default();
        cfg.logo.variant = LogoVariant::Warm;
        cfg.save_to_path(&tmp).unwrap();
        let text = std::fs::read_to_string(&tmp).unwrap();
        assert!(text.contains("variant = \"warm\""));
        // Step 2: switch back to Cool and save — must remove the stale key.
        cfg.logo.variant = LogoVariant::Cool;
        cfg.save_to_path(&tmp).unwrap();
        let reloaded: Config = toml::from_str(&std::fs::read_to_string(&tmp).unwrap()).unwrap();
        assert_eq!(
            reloaded.logo.variant,
            LogoVariant::Cool,
            "stale non-default variant should be cleared on save"
        );
        let _ = std::fs::remove_file(&tmp);
    }
}
