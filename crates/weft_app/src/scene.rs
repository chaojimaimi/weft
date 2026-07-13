//! Minimal retained scene model for application chrome.
//!
//! The terminal grid keeps its specialized instance renderer. UI components
//! migrate here incrementally so paint, hit testing, focus and accessibility
//! share one bounds object.
#![allow(dead_code)]

use crate::layout::Rect;
use weft_core::grid::Color;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Layer(pub(crate) u16);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ClipId(pub(crate) usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum FocusId {
    Tab(usize),
    FindQuery,
    Completion,
    PaletteQuery,
    Settings,
    SidebarSearch,
    ContextMenu,
}

impl FocusId {
    /// F6: Map a `FocusId` to its containing [`FocusScope`]. Tab/Shift+Tab
    /// cycles only within the current scope so focus doesn't escape a modal
    /// surface or jump from the sidebar to the terminal unexpectedly.
    pub(crate) fn scope(self) -> FocusScope {
        match self {
            FocusId::Tab(_) | FocusId::Completion => FocusScope::Terminal,
            FocusId::SidebarSearch => FocusScope::Sidebar,
            FocusId::FindQuery
            | FocusId::PaletteQuery
            | FocusId::Settings
            | FocusId::ContextMenu => FocusScope::Modal,
        }
    }
}

/// F6: Top-level focus scope. Mirrors the scope stack from the design plan:
///
/// ```text
/// Window
///   Terminal | Sidebar
///   Modal(Settings | Palette | Find | ContextMenu)
/// ```
///
/// Tab/Shift+Tab cycles within the current scope. When a modal opens, the
/// previous scope is saved on the focus stack; closing the modal restores it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FocusScope {
    Terminal,
    Sidebar,
    Modal,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Primitive {
    Rect {
        bounds: Rect,
        color: Color,
        layer: Layer,
        clip: Option<ClipId>,
    },
    GlyphRun {
        origin: [f32; 2],
        text: String,
        color: Color,
        layer: Layer,
        clip: Option<ClipId>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HitRegion<T> {
    pub(crate) x0: f32,
    pub(crate) y0: f32,
    pub(crate) x1: f32,
    pub(crate) y1: f32,
    pub(crate) target: T,
}

impl<T> HitRegion<T> {
    pub(crate) fn bounds(&self) -> Rect {
        [self.x0, self.y0, self.x1, self.y1]
    }

    pub(crate) fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x0 && x <= self.x1 && y >= self.y0 && y <= self.y1
    }

    pub(crate) fn contains_half_open(&self, x: f32, y: f32) -> bool {
        x >= self.x0 && x < self.x1 && y >= self.y0 && y < self.y1
    }

    /// Construct a hit region from a `[x0, y0, x1, y1]` rect + target.
    pub(crate) fn from_rect([x0, y0, x1, y1]: Rect, target: T) -> Self {
        Self {
            x0,
            y0,
            x1,
            y1,
            target,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SemanticRole {
    Button,
    TextField,
    List,
    ListItem,
    Dialog,
    Menu,
    MenuItem,
    Tab,
    /// F6: A tab list container (the tab bar as a whole).
    TabList,
}

/// F6: Accessibility state descriptor. Conveys status beyond color so screen
/// readers and high-contrast users can perceive state changes (e.g. a block
/// exiting with an error, a row being selected, a tab being active).
///
/// Stored as a `String` rather than an enum so callers can compose arbitrary
/// status text (e.g. "exit 1", "selected", "expanded") without bloating the
/// type. Empty string means "no extra state".
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SemanticNode {
    pub(crate) role: SemanticRole,
    pub(crate) label: String,
    pub(crate) bounds: Rect,
    pub(crate) focus: Option<FocusId>,
    /// F6: Human-readable state description for accessibility. Empty when
    /// the node has no extra state to announce. Examples: "selected",
    /// "exit 1", "running", "expanded".
    pub(crate) state: String,
}

pub(crate) struct Scene<T> {
    pub(crate) primitives: Vec<Primitive>,
    pub(crate) clips: Vec<Rect>,
    pub(crate) hits: Vec<HitRegion<T>>,
    pub(crate) semantics: Vec<SemanticNode>,
}

impl<T> Default for Scene<T> {
    fn default() -> Self {
        Self {
            primitives: Vec::new(),
            clips: Vec::new(),
            hits: Vec::new(),
            semantics: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{FocusId, FocusScope, HitRegion, Scene, SemanticNode, SemanticRole};

    #[test]
    fn hit_region_and_semantic_node_can_share_exact_bounds() {
        let hit = HitRegion {
            x0: 10.0,
            y0: 20.0,
            x1: 110.0,
            y1: 52.0,
            target: "find-query",
        };
        let semantic = SemanticNode {
            role: SemanticRole::TextField,
            label: "Find".into(),
            bounds: hit.bounds(),
            focus: Some(FocusId::FindQuery),
            state: String::new(),
        };
        assert_eq!(semantic.bounds, hit.bounds());
        assert!(hit.contains(10.0, 52.0));
        assert!(!hit.contains(111.0, 52.0));
    }

    #[test]
    fn scene_keeps_paint_hit_and_semantics_in_separate_batches() {
        #[derive(PartialEq)]
        enum TargetWithoutDefault {
            Row(usize),
        }

        let mut scene: Scene<TargetWithoutDefault> = Scene::default();
        scene.hits.push(HitRegion {
            x0: 0.0,
            y0: 0.0,
            x1: 20.0,
            y1: 20.0,
            target: TargetWithoutDefault::Row(1),
        });
        assert_eq!(scene.hits.len(), 1);
        assert!(scene.primitives.is_empty());
        assert!(scene.semantics.is_empty());
    }

    // ── F6: FocusScope ────────────────────────────────────────────────

    #[test]
    fn focus_scope_maps_modal_targets_to_modal() {
        assert_eq!(FocusId::FindQuery.scope(), FocusScope::Modal);
        assert_eq!(FocusId::PaletteQuery.scope(), FocusScope::Modal);
        assert_eq!(FocusId::Settings.scope(), FocusScope::Modal);
        assert_eq!(FocusId::ContextMenu.scope(), FocusScope::Modal);
    }

    #[test]
    fn focus_scope_maps_terminal_targets_to_terminal() {
        assert_eq!(FocusId::Tab(0).scope(), FocusScope::Terminal);
        assert_eq!(FocusId::Tab(5).scope(), FocusScope::Terminal);
        assert_eq!(FocusId::Completion.scope(), FocusScope::Terminal);
    }

    #[test]
    fn focus_scope_maps_sidebar_search_to_sidebar() {
        assert_eq!(FocusId::SidebarSearch.scope(), FocusScope::Sidebar);
    }

    #[test]
    fn focus_scope_terminal_and_modal_are_distinct() {
        assert_ne!(FocusScope::Terminal, FocusScope::Modal);
        assert_ne!(FocusScope::Sidebar, FocusScope::Modal);
        assert_ne!(FocusScope::Terminal, FocusScope::Sidebar);
    }
}
