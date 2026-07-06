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
        // HOME/XDG unset → no path → default. (Tests run with whatever env;
        // this asserts graceful handling when the path can't be resolved.)
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
}
