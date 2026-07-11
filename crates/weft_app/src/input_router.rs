//! Pure focus routing for keyboard input.
//!
//! The handlers still live on `App` during the incremental migration, but
//! ownership priority is defined and tested here instead of being inferred
//! from the order of unrelated `if` statements.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OverlayInputOwner {
    Palette,
    Settings,
    Find,
    PanelSearch,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct OverlayInputContext {
    pub(crate) palette_open: bool,
    pub(crate) settings_open: bool,
    pub(crate) find_open: bool,
    pub(crate) panel_search_focused: bool,
}

impl OverlayInputOwner {
    pub(crate) fn resolve(context: OverlayInputContext) -> Option<Self> {
        if context.palette_open {
            Some(Self::Palette)
        } else if context.settings_open {
            Some(Self::Settings)
        } else if context.find_open {
            Some(Self::Find)
        } else if context.panel_search_focused {
            Some(Self::PanelSearch)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{OverlayInputContext, OverlayInputOwner};

    #[test]
    fn modal_priority_is_deterministic_when_state_is_temporarily_inconsistent() {
        let all_open = OverlayInputContext {
            palette_open: true,
            settings_open: true,
            find_open: true,
            panel_search_focused: true,
        };
        assert_eq!(
            OverlayInputOwner::resolve(all_open),
            Some(OverlayInputOwner::Palette)
        );

        let without_palette = OverlayInputContext {
            palette_open: false,
            ..all_open
        };
        assert_eq!(
            OverlayInputOwner::resolve(without_palette),
            Some(OverlayInputOwner::Settings)
        );
    }

    #[test]
    fn focused_panel_wins_only_without_modal_overlay() {
        let panel = OverlayInputContext {
            panel_search_focused: true,
            ..OverlayInputContext::default()
        };
        assert_eq!(
            OverlayInputOwner::resolve(panel),
            Some(OverlayInputOwner::PanelSearch)
        );
        assert_eq!(
            OverlayInputOwner::resolve(OverlayInputContext::default()),
            None
        );
    }

    #[test]
    fn all_open_closed_combinations_follow_one_priority_order() {
        for bits in 0_u8..16 {
            let context = OverlayInputContext {
                palette_open: bits & 0b0001 != 0,
                settings_open: bits & 0b0010 != 0,
                find_open: bits & 0b0100 != 0,
                panel_search_focused: bits & 0b1000 != 0,
            };
            let expected = if context.palette_open {
                Some(OverlayInputOwner::Palette)
            } else if context.settings_open {
                Some(OverlayInputOwner::Settings)
            } else if context.find_open {
                Some(OverlayInputOwner::Find)
            } else if context.panel_search_focused {
                Some(OverlayInputOwner::PanelSearch)
            } else {
                None
            };
            assert_eq!(
                OverlayInputOwner::resolve(context),
                expected,
                "bits={bits:04b}"
            );
        }
    }
}
