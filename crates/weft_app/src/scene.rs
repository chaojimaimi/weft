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
    PaletteQuery,
    Settings,
    SidebarSearch,
    ContextMenu,
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
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SemanticNode {
    pub(crate) role: SemanticRole,
    pub(crate) label: String,
    pub(crate) bounds: Rect,
    pub(crate) focus: Option<FocusId>,
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
}
