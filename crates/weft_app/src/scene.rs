//! Minimal retained scene model for application chrome.
//!
//! The terminal grid keeps its specialized instance renderer. UI components
//! migrate here incrementally so paint, hit testing, focus and accessibility
//! share one bounds object.

use crate::layout::Rect;

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
    /// Visible terminal text exposed as one navigable text area rather than
    /// thousands of per-cell accessibility elements.
    TextArea,
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
    pub(crate) hits: Vec<HitRegion<T>>,
    pub(crate) semantics: Vec<SemanticNode>,
}

impl<T> Default for Scene<T> {
    fn default() -> Self {
        Self {
            hits: Vec::new(),
            semantics: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{FocusId, HitRegion, Scene, SemanticNode, SemanticRole};

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
        // contains_half_open: [x0, x1) × [y0, y1)
        assert!(hit.contains_half_open(10.0, 20.0)); // top-left corner included
        assert!(!hit.contains_half_open(110.0, 52.0)); // x1 (right edge) excluded
        assert!(!hit.contains_half_open(111.0, 52.0)); // outside
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
        assert!(scene.semantics.is_empty());
    }
}
