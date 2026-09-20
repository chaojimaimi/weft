// arch-gate: allow-over-800
// Theme + impl Theme: 11 built-in theme constructors (weft_warm/dracula/
// nord/etc.), each ~50-80 lines of palette literals. Intrinsic volume.
// ── Theme (resolved colors the renderer needs) ─────────────────────────

use std::path::PathBuf;

use crate::grid::Color;

use super::{
    sections::ThemeConfig,
    theme_import::{apply_overrides, resolve_theme_file},
};

/// Guard for user-config-supplied theme names: reject empty names and any
/// path-separator or NUL so `dir.join(name.ext)` can never leave the
/// themes directory. Defense in depth — the only caller passes a name from
/// the user's own config.toml.
fn is_safe_theme_name(name: &str) -> bool {
    !name.is_empty() && !name.contains('/') && !name.contains('\\') && !name.contains('\0')
}

/// Read cap for a single theme file (defensive: config files are trusted,
/// but a multi-GiB "theme" would stall config load).
const THEME_FILE_MAX_BYTES: u64 = 1024 * 1024;

/// Syntax-highlight color palette. Theme-driven so every theme
/// can define its own command/flag/path/string colors; replaces the hardcoded
/// `syntax_color()` from renderer.rs v0.5. Conventions match the "Warm
/// Terminal" direction (v0.8 §0.3) but each theme fills its own values.
///
/// v1.7.0-B: added `argument` (plain arguments get their own role, distinct
/// from `default`). The visual hierarchy contract (V17 §2.4) requires
/// `command` ≠ `argument` ≠ `default` in every built-in theme.
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
    /// v1.7.0-B: a plain argument (non-command-position word that is not a
    /// flag/path/number/string/variable). Must be distinguishable from
    /// `command` and `default` in every built-in theme.
    pub argument: Color,
    /// Anything else (rare fallback) — usually == theme.foreground.
    pub default: Color,
}

/// v1.7.0-B: Output semantic color roles for unstyled (no-ANSI) command
/// output. The semantic fallback classifier (v1.7.0-C) maps tokens to these
/// roles; they are also used directly for block metadata and exit-code
/// status rendering. The CWD color, by contrast, is deliberately NOT a theme
/// role: the painter derives a neutral gray from the foreground (`fg × 0.65`,
/// `weft_app::paint::primitives::derive_cwd_gray`) so it stays theme-agnostic
/// (v1.11.0 removed the dead `output.cwd` key — see AUDIT_v1.10.39 / PLAN_v111).
/// All roles must be visually distinguishable from each
/// other and from `SyntaxColors::command`/`argument`/`default` in every
/// built-in theme (V17 §2.4 visual hierarchy contract).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputSemanticColors {
    /// Plain output text with no semantic role and no ANSI styling.
    pub output_default: Color,
    /// Label / metadata key — secondary semantic color. Weaker than `command`
    /// but distinguishable from `output_default`.
    pub metadata: Color,
    /// Success status (ok/ready/running/passed). Must not rely on color alone
    /// — brightness/weight signal also required (V17 §2.4).
    pub success: Color,
    /// Failure status (error/failed/stopped). Must not rely on color alone.
    pub failure: Color,
}

/// v1.11.6 (PLAN_v1116 M6/D-f): per-theme UI seed colors consumed by
/// `UiColors::from_theme` (weft_app). All `None` by default — the UiColors
/// light/dark dual-branch hardcodes stay as the fallback, so zero-config
/// visuals are unchanged. A user `Some(...)` value replaces the dual-branch
/// INPUT and still passes through the existing `ensure_contrast(..., 4.5)`
/// accessibility gate — the stored hex is NOT the final painted color.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThemeUi {
    pub success: Option<Color>,
    pub warning: Option<Color>,
    pub error: Option<Color>,
    pub find_match: Option<Color>,
}

/// A fully-resolved theme: the colors the renderer paints with.
#[derive(Clone, Debug)]
pub struct Theme {
    pub foreground: Color,
    pub background: Color,
    pub cursor: Color,
    /// v1.10.22: terminal selection highlight base color (grid/block/prompt).
    /// Previously dead config — the renderer derived selection from accent;
    /// now the adaptive pipeline in `weft_app::paint::selection_color` uses
    /// it as the base and guarantees WCAG 3:1 against the background.
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
    /// v1.7.0-B: output semantic color roles for unstyled command output.
    pub output: OutputSemanticColors,
    /// v1.11.6 (PLAN_v1116 M6/D-f): OSC 8 hyperlink underline color. The
    /// default is the f32 literal `[0.36, 0.62, 0.94, 1.0]` (the old
    /// `HYPERLINK_COLOR` const in grid_instances.rs) — architect P1-4:
    /// must NOT be carried as a u8 `Color`, because 0.36/0.62 have no
    /// integer-byte representation. TOML overrides (`[theme] link = "#hex"`)
    /// are u8-granular by nature (parsed to `Color`, then /255-normalized).
    pub link: [f32; 4],
    /// v1.11.6 (PLAN_v1116 M6/D-f): UI seed colors; all `None` by default.
    pub ui: ThemeUi,
}

// f32 `link` has no `Eq`, so the derived impls are replaced by a manual
// `PartialEq` with identical field-by-field semantics + a marker `Eq`.
impl PartialEq for Theme {
    fn eq(&self, other: &Self) -> bool {
        self.foreground == other.foreground
            && self.background == other.background
            && self.cursor == other.cursor
            && self.selection == other.selection
            && self.palette == other.palette
            && self.accent == other.accent
            && self.accent_dim == other.accent_dim
            && self.separator == other.separator
            && self.syntax == other.syntax
            && self.output == other.output
            && self.link == other.link
            && self.ui == other.ui
    }
}

impl Eq for Theme {}

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
            (0xf1, 0xf1, 0xf1), // 7 white (neutral near-white — dimmer than default fg)
            (0x4a, 0x3f, 0x35), // 8 bright black (warm dark brown)
            (0xe0, 0x88, 0x78), // 9 bright red
            (0xd0, 0xe0, 0x90), // 10 bright green
            (0xe8, 0xc8, 0x70), // 11 bright yellow
            (0xd8, 0xc0, 0xe0), // 12 bright blue
            (0xe8, 0xa8, 0x90), // 13 bright magenta
            (0xe8, 0xc8, 0x9c), // 14 bright cyan
            (0xff, 0xff, 0xff), // 15 bright white (pure — must not dim below SGR 37)
        ];
        for (i, (r, g, b)) in ansi.iter().enumerate() {
            palette[i] = Color::rgb(*r, *g, *b);
        }
        Self {
            // v1.12.2 (PLAN_S2_render A1): default fg is now the brightest
            // neutral — plain text must lead the brightness hierarchy
            // (Warp convention: default fg ≥ ANSI white ≥ everything else).
            foreground: Color::rgb(0xff, 0xff, 0xff), // pure white
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
                argument: Color::rgb(0xc8, 0xa8, 0x88), // warm sand — distinct from olive command
                default: Color::rgb(0xff, 0xff, 0xff),  // == foreground
            },
            output: OutputSemanticColors {
                output_default: Color::rgb(0xff, 0xff, 0xff), // == syntax.default
                metadata: Color::rgb(0xa8, 0x90, 0x70),       // muted amber
                success: Color::rgb(0xb8, 0xc8, 0x78),        // ANSI 2 green (olive)
                failure: Color::rgb(0xc8, 0x68, 0x58),        // ANSI 1 red (brick)
            },
            link: [0.36, 0.62, 0.94, 1.0], // v1.11.6 M6: OSC 8 hyperlink underline (old HYPERLINK_COLOR)
            ui: ThemeUi::default(),
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
                argument: Color::rgb(0x80, 0x68, 0x50), // warm khaki — distinct from olive command
                default: Color::rgb(0x3a, 0x32, 0x28),  // == foreground
            },
            output: OutputSemanticColors {
                output_default: Color::rgb(0x3a, 0x32, 0x28), // == syntax.default
                metadata: Color::rgb(0x70, 0x58, 0x40),       // muted brown
                success: Color::rgb(0x6a, 0x80, 0x40),        // ANSI 2 green
                failure: Color::rgb(0xa8, 0x48, 0x38),        // ANSI 1 red
            },
            link: [0.36, 0.62, 0.94, 1.0], // v1.11.6 M6: OSC 8 hyperlink underline (old HYPERLINK_COLOR)
            ui: ThemeUi::default(),
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
                command: Color::rgb(0xe8, 0xe8, 0xf0), // bright off-white — distinct from default fg
                flag: Color::rgb(0xff, 0x5d, 0x38),    // coral (== accent)
                path: Color::rgb(0x5a, 0xb9, 0xf8),    // sky blue
                string: Color::rgb(0xc7, 0xa5, 0x5c),  // warm yellow
                number: Color::rgb(0xff, 0xc7, 0x4a),  // bright yellow
                variable: Color::rgb(0xb3, 0x87, 0xff), // lavender
                operator: Color::rgb(0x7a, 0x7a, 0x90), // cool gray
                comment: Color::rgb(0x5a, 0x5a, 0x72), // muted purple-gray
                argument: Color::rgb(0x7a, 0xc4, 0xc8), // soft teal — distinct from foreground command
                default: Color::rgb(0xd9, 0xd9, 0xe3),  // == foreground
            },
            output: OutputSemanticColors {
                output_default: Color::rgb(0xd9, 0xd9, 0xe3), // == syntax.default
                metadata: Color::rgb(0xa8, 0x6a, 0x5a),       // muted coral
                success: Color::rgb(0x3e, 0xd9, 0xa4),        // ANSI 2 green (mint)
                failure: Color::rgb(0xff, 0x5d, 0x38),        // ANSI 1 red (coral)
            },
            link: [0.36, 0.62, 0.94, 1.0], // v1.11.6 M6: OSC 8 hyperlink underline (old HYPERLINK_COLOR)
            ui: ThemeUi::default(),
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
                command: Color::rgb(0x50, 0xfa, 0x7b), // Dracula green — distinct from fg
                flag: Color::rgb(0xff, 0x79, 0xc6),    // pink
                path: Color::rgb(0x8b, 0xe9, 0xfd),    // cyan
                string: Color::rgb(0xf1, 0xfa, 0x8c),  // yellow
                number: Color::rgb(0xbd, 0x93, 0xf9),  // purple
                variable: Color::rgb(0xff, 0xb8, 0x6c), // orange
                operator: Color::rgb(0xff, 0x55, 0x55), // red
                comment: Color::rgb(0x62, 0x72, 0xa4), // comment
                argument: Color::rgb(0xa8, 0xc8, 0xe8), // soft blue — distinct from foreground command
                default: Color::rgb(0xf8, 0xf8, 0xf2),  // == foreground
            },
            output: OutputSemanticColors {
                output_default: Color::rgb(0xf8, 0xf8, 0xf2), // == syntax.default
                metadata: Color::rgb(0x8a, 0x7a, 0xc4),       // muted purple
                success: Color::rgb(0x50, 0xfa, 0x7b),        // ANSI 2 green
                failure: Color::rgb(0xff, 0x55, 0x55),        // ANSI 1 red
            },
            link: [0.36, 0.62, 0.94, 1.0], // v1.11.6 M6: OSC 8 hyperlink underline (old HYPERLINK_COLOR)
            ui: ThemeUi::default(),
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
            (0x65, 0x7b, 0x83), // 8 bright black (base00, dim but visible)
            (0xcb, 0x4b, 0x16), // 9 bright red (orange)
            (0x58, 0x6e, 0x75), // 10 bright green (base01)
            (0x83, 0x94, 0x96), // 11 bright yellow (base0)
            (0x93, 0xa1, 0xa1), // 12 bright blue (base1)
            (0x6c, 0x71, 0xc4), // 13 bright magenta (violet)
            (0x93, 0xa1, 0xa1), // 14 bright cyan (base1)
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
                command: Color::rgb(0x85, 0x99, 0x00), // solarized green — distinct from fg
                flag: Color::rgb(0x26, 0x8b, 0xd2),    // blue
                path: Color::rgb(0x2a, 0xa1, 0x98),    // cyan
                string: Color::rgb(0x85, 0x99, 0x00),  // green
                number: Color::rgb(0xb5, 0x89, 0x00),  // yellow (magenta)
                variable: Color::rgb(0x6c, 0x71, 0xc4), // violet
                operator: Color::rgb(0xdc, 0x32, 0x2f), // red
                comment: Color::rgb(0x58, 0x6e, 0x75), // base01
                argument: Color::rgb(0x5a, 0x90, 0x88), // muted teal — distinct from base1 command
                default: Color::rgb(0x93, 0xa1, 0xa1), // base1
            },
            output: OutputSemanticColors {
                output_default: Color::rgb(0x93, 0xa1, 0xa1), // == syntax.default
                metadata: Color::rgb(0x4a, 0x6a, 0x8a),       // muted blue
                success: Color::rgb(0x85, 0x99, 0x00),        // ANSI 2 green
                failure: Color::rgb(0xdc, 0x32, 0x2f),        // ANSI 1 red
            },
            link: [0.36, 0.62, 0.94, 1.0], // v1.11.6 M6: OSC 8 hyperlink underline (old HYPERLINK_COLOR)
            ui: ThemeUi::default(),
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
                command: Color::rgb(0xb8, 0xbb, 0x26), // gruvbox green — distinct from fg
                flag: Color::rgb(0xfe, 0x80, 0x19),    // orange
                path: Color::rgb(0x83, 0xa5, 0x98),    // blue
                string: Color::rgb(0xb8, 0xbb, 0x26),  // green
                number: Color::rgb(0xd3, 0x86, 0x9b),  // purple
                variable: Color::rgb(0xfa, 0xbd, 0x2f), // yellow
                operator: Color::rgb(0xfb, 0x49, 0x34), // red
                comment: Color::rgb(0x92, 0x83, 0x74), // gray
                argument: Color::rgb(0x6a, 0x9a, 0x8a), // muted teal — distinct from fg command
                default: Color::rgb(0xeb, 0xdb, 0xb2), // fg
            },
            output: OutputSemanticColors {
                output_default: Color::rgb(0xeb, 0xdb, 0xb2), // == syntax.default
                metadata: Color::rgb(0xb5, 0x80, 0x40),       // muted orange
                success: Color::rgb(0x98, 0x97, 0x1a),        // ANSI 2 green
                failure: Color::rgb(0xcc, 0x24, 0x1d),        // ANSI 1 red
            },
            link: [0.36, 0.62, 0.94, 1.0], // v1.11.6 M6: OSC 8 hyperlink underline (old HYPERLINK_COLOR)
            ui: ThemeUi::default(),
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
                command: Color::rgb(0x88, 0xc0, 0xd0), // nord8 cyan — distinct from fg
                flag: Color::rgb(0x88, 0xc0, 0xd0),    // nord8 cyan-blue
                path: Color::rgb(0x81, 0xa1, 0xc1),    // nord9 frost
                string: Color::rgb(0xa3, 0xbe, 0x8c),  // nord14 green
                number: Color::rgb(0xeb, 0xcb, 0x8b),  // nord13 yellow
                variable: Color::rgb(0xb4, 0x8e, 0xad), // nord15 purple
                operator: Color::rgb(0xbf, 0x61, 0x6a), // nord11 red
                comment: Color::rgb(0x61, 0x69, 0x80), // nord3 dimmed
                argument: Color::rgb(0x7a, 0xa8, 0xa8), // muted teal — distinct from nord4 command
                default: Color::rgb(0xd8, 0xde, 0xe9), // nord4
            },
            output: OutputSemanticColors {
                output_default: Color::rgb(0xd8, 0xde, 0xe9), // == syntax.default
                metadata: Color::rgb(0x6a, 0x88, 0x98),       // muted frost
                success: Color::rgb(0xa3, 0xbe, 0x8c),        // ANSI 2 green (nord14)
                failure: Color::rgb(0xbf, 0x61, 0x6a),        // ANSI 1 red (nord11)
            },
            link: [0.36, 0.62, 0.94, 1.0], // v1.11.6 M6: OSC 8 hyperlink underline (old HYPERLINK_COLOR)
            ui: ThemeUi::default(),
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
                command: Color::rgb(0x7a, 0xa2, 0xf7), // tokyo night blue — distinct from fg
                flag: Color::rgb(0x7a, 0xa2, 0xf7),    // blue
                path: Color::rgb(0x7d, 0xcf, 0xff),    // cyan
                string: Color::rgb(0x9e, 0xce, 0x6a),  // green
                number: Color::rgb(0xff, 0x9e, 0x64),  // orange
                variable: Color::rgb(0xbb, 0x9a, 0xf7), // magenta
                operator: Color::rgb(0xf7, 0x76, 0x8e), // red
                comment: Color::rgb(0x56, 0x5f, 0x89), // comment
                argument: Color::rgb(0x7a, 0xc0, 0xc8), // muted teal — distinct from fg command
                default: Color::rgb(0xa9, 0xb1, 0xd6), // fg
            },
            output: OutputSemanticColors {
                output_default: Color::rgb(0xa9, 0xb1, 0xd6), // == syntax.default
                metadata: Color::rgb(0x6a, 0x7a, 0xa8),       // muted blue
                success: Color::rgb(0x9e, 0xce, 0x6a),        // ANSI 2 green
                failure: Color::rgb(0xf7, 0x76, 0x8e),        // ANSI 1 red
            },
            link: [0.36, 0.62, 0.94, 1.0], // v1.11.6 M6: OSC 8 hyperlink underline (old HYPERLINK_COLOR)
            ui: ThemeUi::default(),
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
                command: Color::rgb(0xcb, 0xa6, 0xf7), // catppuccin mauve — distinct from fg
                flag: Color::rgb(0xcb, 0xa6, 0xf7),    // mauve
                path: Color::rgb(0x89, 0xb4, 0xfa),    // blue
                string: Color::rgb(0xa6, 0xe3, 0xa1),  // green
                number: Color::rgb(0xfa, 0xb3, 0x87),  // peach
                variable: Color::rgb(0xf9, 0xe2, 0xaf), // yellow
                operator: Color::rgb(0xf3, 0x8b, 0xa8), // red
                comment: Color::rgb(0x6c, 0x70, 0x86), // overlay0
                argument: Color::rgb(0x9a, 0xb8, 0xd8), // soft blue — distinct from text command
                default: Color::rgb(0xcd, 0xd6, 0xf4), // text
            },
            output: OutputSemanticColors {
                output_default: Color::rgb(0xcd, 0xd6, 0xf4), // == syntax.default
                metadata: Color::rgb(0x8a, 0x8a, 0xa8),       // muted mauve
                success: Color::rgb(0xa6, 0xe3, 0xa1),        // ANSI 2 green
                failure: Color::rgb(0xf3, 0x8b, 0xa8),        // ANSI 1 red
            },
            link: [0.36, 0.62, 0.94, 1.0], // v1.11.6 M6: OSC 8 hyperlink underline (old HYPERLINK_COLOR)
            ui: ThemeUi::default(),
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
                command: Color::rgb(0x61, 0xaf, 0xef), // one dark blue — distinct from fg
                flag: Color::rgb(0xc6, 0x78, 0xdd),    // purple
                path: Color::rgb(0x56, 0xb6, 0xc2),    // cyan
                string: Color::rgb(0x98, 0xc3, 0x79),  // green
                number: Color::rgb(0xd1, 0x9a, 0x66),  // orange
                variable: Color::rgb(0xe5, 0xc0, 0x7b), // yellow
                operator: Color::rgb(0xe0, 0x6c, 0x75), // red
                comment: Color::rgb(0x5c, 0x63, 0x70), // comment
                argument: Color::rgb(0x6a, 0xa8, 0xa0), // muted teal — distinct from fg command
                default: Color::rgb(0xab, 0xb2, 0xbf), // fg
            },
            output: OutputSemanticColors {
                output_default: Color::rgb(0xab, 0xb2, 0xbf), // == syntax.default
                metadata: Color::rgb(0x6a, 0x88, 0xa8),       // muted blue
                success: Color::rgb(0x98, 0xc3, 0x79),        // ANSI 2 green
                failure: Color::rgb(0xe0, 0x6c, 0x75),        // ANSI 1 red
            },
            link: [0.36, 0.62, 0.94, 1.0], // v1.11.6 M6: OSC 8 hyperlink underline (old HYPERLINK_COLOR)
            ui: ThemeUi::default(),
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
                command: Color::rgb(0xa9, 0xdc, 0x76), // monokai green — distinct from fg
                flag: Color::rgb(0xff, 0x61, 0x88),    // red
                path: Color::rgb(0x78, 0xdc, 0xe8),    // cyan
                string: Color::rgb(0xa9, 0xdc, 0x76),  // green
                number: Color::rgb(0xab, 0x9d, 0xf2),  // purple
                variable: Color::rgb(0xab, 0x9d, 0xf2), // purple
                operator: Color::rgb(0xff, 0x61, 0x88), // red
                comment: Color::rgb(0x72, 0x70, 0x72), // comment
                argument: Color::rgb(0x7a, 0xb8, 0xb0), // muted teal — distinct from fg command
                default: Color::rgb(0xfc, 0xfc, 0xfa), // fg
            },
            output: OutputSemanticColors {
                output_default: Color::rgb(0xfc, 0xfc, 0xfa), // == syntax.default
                metadata: Color::rgb(0xa8, 0xa8, 0x70),       // muted yellow
                success: Color::rgb(0xa9, 0xdc, 0x76),        // ANSI 2 green
                failure: Color::rgb(0xff, 0x61, 0x88),        // ANSI 1 red
            },
            link: [0.36, 0.62, 0.94, 1.0], // v1.11.6 M6: OSC 8 hyperlink underline (old HYPERLINK_COLOR)
            ui: ThemeUi::default(),
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
                None => {
                    // v1.12: 此前静默回落到 weft-warm，用户拼错主题名时毫无
                    // 反馈（看起来像"换主题没生效"）。这里明确告警。
                    tracing::warn!(
                        theme = other,
                        themes_dir = ?Self::themes_dir(),
                        "unknown theme name — falling back to weft-warm",
                    );
                    Self::weft_warm()
                }
            },
        };
        apply_overrides(base, cfg)
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

    /// Extensions `load_from_dir` probes, in order. `available_theme_names`
    /// (weft_app) filters directory listings with the same set so the picker
    /// can never offer a stem that would fail to load.
    pub const THEME_FILE_EXTENSIONS: [&str; 3] = ["toml", "yaml", "yml"];

    /// True when `file_name` has a non-empty stem and one of
    /// [`Self::THEME_FILE_EXTENSIONS`]. Exact-match on the lowercase
    /// extension — `load_from_dir` probes the same literals, so anything
    /// this rejects would not have loaded anyway. The non-empty-stem
    /// requirement keeps `".toml"` (a hidden file) out, matching
    /// `Path::extension()`, so the picker and the loader share ONE
    /// predicate.
    pub fn is_theme_file_name(file_name: &str) -> bool {
        file_name.rsplit_once('.').is_some_and(|(stem, ext)| {
            !stem.is_empty() && Self::THEME_FILE_EXTENSIONS.contains(&ext)
        })
    }

    /// v0.9 W2+: Internal loader that reads from an explicit `dir`. Used by
    /// [`load_from_file`] (which resolves the dir from env) and by unit
    /// tests (which pass a tempdir). See [`load_from_file`] for the file
    /// format and resolution semantics.
    pub(super) fn load_from_dir(dir: &std::path::Path, name: &str) -> Option<Self> {
        // Path-traversal guard: `name` comes from the user's config, and a
        // crafted `/`/`..` name must not be able to steer dir.join() outside
        // the themes directory.
        if !is_safe_theme_name(name) {
            tracing::warn!(
                name = %name,
                "unsafe theme name rejected (empty or contains a path separator)"
            );
            return None;
        }
        // Try each supported extension in order: toml, yaml, yml.
        for ext in Self::THEME_FILE_EXTENSIONS {
            let path = dir.join(format!("{name}.{ext}"));
            if !path.exists() {
                continue;
            }
            // A metadata probe failure is the same race as a failed exists()
            // above: fall through to the next extension. Only a confirmed
            // over-cap file is a hard stop, matching the read-failure path.
            let len = match std::fs::metadata(&path) {
                Ok(meta) => meta.len(),
                Err(_) => continue,
            };
            if len > THEME_FILE_MAX_BYTES {
                tracing::warn!(
                    path = %path.display(),
                    len,
                    "theme file exceeds the read cap; refusing to load",
                );
                return None;
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
            // v1.12: 先嗅探外部格式（wezterm TOML / alacritty TOML /
            // iTerm2 generic YAML / base16-24 YAML），再退回 weft 自有
            // schema；顺序与冲突原因见 `resolve_theme_file` 的文档。
            let (base, overrides) = match resolve_theme_file(name, &text, ext) {
                Some(resolved) => resolved,
                None => {
                    tracing::warn!(
                        path = %path.display(),
                        "failed to parse theme file (neither a known external \
                         format nor a weft theme schema)",
                    );
                    return None;
                }
            };
            tracing::info!(
                path = %path.display(),
                "loaded custom theme from file",
            );
            return Some(apply_overrides(base, &overrides));
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

// Inline (not config/tests.rs) because `is_safe_theme_name` is
// module-private by design — the sibling test module cannot see it.
#[cfg(test)]
mod theme_guard_tests {
    use super::{is_safe_theme_name, THEME_FILE_MAX_BYTES};

    #[test]
    fn safe_theme_name_accepts_ordinary_names() {
        assert!(is_safe_theme_name("dracula"));
        assert!(is_safe_theme_name("my-theme_2"));
    }

    #[test]
    fn safe_theme_name_rejects_empty_traversal_and_separators() {
        // Escaping dir.join() requires a separator — a bare ".." name only
        // produces the in-dir file "...toml", so it stays accepted.
        assert!(!is_safe_theme_name(""));
        assert!(!is_safe_theme_name("../x"));
        assert!(!is_safe_theme_name("a/b"));
        assert!(!is_safe_theme_name("a\\b"));
        assert!(!is_safe_theme_name("a\0b"));
    }

    #[test]
    fn theme_file_read_cap_is_one_mib() {
        // Large enough for any real theme, small enough that a multi-GiB
        // "theme" can never stall config load.
        assert_eq!(THEME_FILE_MAX_BYTES, 1024 * 1024);
    }

    #[test]
    fn is_theme_file_name_requires_non_empty_stem() {
        use super::Theme;
        // PLAN_audit_fix_batch2 B5 boundary: the stem requirement makes this
        // predicate agree with Path::extension() on bare dot files, so the
        // picker and loader can share one implementation.
        assert!(!Theme::is_theme_file_name(".toml"));
        assert!(Theme::is_theme_file_name("a.b.toml"), "dotted stem is fine");
        // Stem stays required alongside the existing extension rules.
        assert!(Theme::is_theme_file_name("dracula.toml"));
        assert!(!Theme::is_theme_file_name("foo."));
    }
}
