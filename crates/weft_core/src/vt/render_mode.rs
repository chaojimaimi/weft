//! v1.11.7 (PLAN_v1117_SHADOW_BLOCK_VIEW §二 D-d): primary-screen TUI render
//! mode — the `[experimental] tui_render_mode` switch.
//!
//! Three tiers:
//! - [`TuiRenderMode::Classic`] — v1.11.6 behavior, verbatim (a screen-owned
//!   TUI always takes over the live grid; `show_block_view()` returns false).
//!   The default of `Terminal::new` so every existing test/consumer keeps the
//!   pre-v1.11.7 semantics with zero changes.
//! - [`TuiRenderMode::Noninteractive`] — the factory default of the config
//!   file. Screen-owned sessions keep rendering as blocks UNLESS the user has
//!   interacted with the command via stdin (`interactive_stdin_seen`) or the
//!   app negotiated a mouse protocol (both are interaction capabilities, not
//!   content sniffing — D-c). Interactive TUIs fall back to Classic, keeping
//!   the v1.11.6 UX for pi/openclaw/claude.
//! - [`TuiRenderMode::All`] — Warp's terminal state: primary-screen sessions
//!   always render in the BlockView regardless of stdin/mouse evidence
//!   (`show_block_view()` drops the three false items entirely, P0-1).
//!
//! Unknown TOML values fall back to `Noninteractive` (a typo must never fail
//! the whole config parse — same philosophy as `runtime_*` clamps and
//! `Osc52Mode::parse`).

/// Primary-screen TUI render mode (`[experimental] tui_render_mode`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TuiRenderMode {
    /// v1.11.6 behavior: screen-owned TUIs take over the live grid (original
    /// `show_block_view` formula untouched).
    Classic,
    /// Default: screen-owned sessions keep the BlockView unless interactive
    /// stdin or mouse reporting exempts them back to Classic.
    #[default]
    Noninteractive,
    /// Screen-owned sessions always render in the BlockView (no stdin/mouse
    /// exemption; Warp's terminal state).
    All,
}

impl TuiRenderMode {
    /// Canonical TOML spelling (lowercase).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Classic => "classic",
            Self::Noninteractive => "noninteractive",
            Self::All => "all",
        }
    }

    /// Parse a TOML value; anything unrecognized → `Noninteractive` (never
    /// fails, matching `Osc52Mode::parse`).
    pub fn parse(s: &str) -> Self {
        match s {
            "classic" => Self::Classic,
            "all" => Self::All,
            _ => Self::Noninteractive,
        }
    }
}

impl<'de> serde::Deserialize<'de> for TuiRenderMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Ok(Self::parse(&raw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_noninteractive() {
        assert_eq!(TuiRenderMode::default(), TuiRenderMode::Noninteractive);
    }

    #[test]
    fn parse_round_trips_all_spellings() {
        assert_eq!(TuiRenderMode::Classic.as_str(), "classic");
        assert_eq!(TuiRenderMode::Noninteractive.as_str(), "noninteractive");
        assert_eq!(TuiRenderMode::All.as_str(), "all");
        for (spelling, mode) in [
            ("classic", TuiRenderMode::Classic),
            ("noninteractive", TuiRenderMode::Noninteractive),
            ("all", TuiRenderMode::All),
        ] {
            assert_eq!(TuiRenderMode::parse(spelling), mode);
        }
    }

    #[test]
    fn unknown_spelling_falls_back_to_noninteractive() {
        assert_eq!(
            TuiRenderMode::parse("absolute_only"),
            TuiRenderMode::Noninteractive
        );
        assert_eq!(TuiRenderMode::parse(""), TuiRenderMode::Noninteractive);
    }

    #[test]
    fn serde_deserializes_from_toml_strings() {
        use serde::Deserialize;
        let table: toml::Table = toml::from_str("tui_render_mode = \"classic\"").unwrap();
        let mode = TuiRenderMode::deserialize(table["tui_render_mode"].clone()).unwrap();
        assert_eq!(mode, TuiRenderMode::Classic);
        let table: toml::Table = toml::from_str("tui_render_mode = \"all\"").unwrap();
        let mode = TuiRenderMode::deserialize(table["tui_render_mode"].clone()).unwrap();
        assert_eq!(mode, TuiRenderMode::All);
        let table: toml::Table = toml::from_str("tui_render_mode = \"bogus\"").unwrap();
        let mode = TuiRenderMode::deserialize(table["tui_render_mode"].clone()).unwrap();
        assert_eq!(mode, TuiRenderMode::Noninteractive);
    }
}
