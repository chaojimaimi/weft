//! Scene model and hit testing for the Find utility bar.

use crate::layout::FindLayout;
use crate::scene::{FocusId, HitRegion, Scene, SemanticNode, SemanticRole};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FindTarget {
    Previous,
    Next,
    ToggleCase,
    ToggleRegex,
}

pub(crate) fn build_find_scene(layout: FindLayout) -> Scene<FindTarget> {
    let mut scene = Scene::default();
    scene.semantics.push(SemanticNode {
        role: SemanticRole::TextField,
        label: "Find".into(),
        bounds: layout.popup_rect,
        focus: Some(FocusId::FindQuery),
        state: String::new(),
    });
    let buttons = [
        (layout.up_rect, FindTarget::Previous, "Previous match"),
        (layout.down_rect, FindTarget::Next, "Next match"),
        (Some(layout.case_rect), FindTarget::ToggleCase, "Match case"),
        (
            Some(layout.regex_rect),
            FindTarget::ToggleRegex,
            "Regular expression",
        ),
    ];
    for (bounds, target, label) in buttons {
        let Some(bounds) = bounds else { continue };
        let [x0, y0, x1, y1] = bounds;
        scene.hits.push(HitRegion {
            x0,
            y0,
            x1,
            y1,
            target,
        });
        scene.semantics.push(SemanticNode {
            role: SemanticRole::Button,
            label: label.into(),
            bounds,
            focus: Some(FocusId::FindQuery),
            state: String::new(),
        });
    }
    scene
}

pub(crate) fn find_target_at(scene: &Scene<FindTarget>, x: f32, y: f32) -> Option<FindTarget> {
    scene
        .hits
        .iter()
        .find(|hit| hit.contains_half_open(x, y))
        .map(|hit| hit.target)
}

#[cfg(test)]
mod tests {
    use super::{build_find_scene, find_target_at, FindTarget};
    use crate::layout::{layout_find, LayoutCtx};

    #[test]
    fn zero_matches_omits_navigation_but_keeps_toggles() {
        let ctx = LayoutCtx::new((1000.0, 700.0), 9.0, 20.0, 8.0, 8.0);
        let layout = layout_find(&ctx, 0);
        let scene = build_find_scene(layout);
        assert_eq!(scene.hits.len(), 2);
        assert_eq!(scene.semantics.len(), 3);
        assert_eq!(
            find_target_at(&scene, layout.regex_rect[0], layout.regex_rect[1]),
            Some(FindTarget::ToggleRegex)
        );
    }

    #[test]
    fn matches_enable_previous_and_next_with_shared_bounds() {
        let ctx = LayoutCtx::new((1000.0, 700.0), 9.0, 20.0, 8.0, 8.0);
        let layout = layout_find(&ctx, 3);
        let scene = build_find_scene(layout);
        assert_eq!(scene.hits.len(), 4);
        assert_eq!(scene.hits[0].bounds(), layout.up_rect.unwrap());
        assert_eq!(scene.hits[1].bounds(), layout.down_rect.unwrap());
        assert_eq!(scene.semantics[1].bounds, scene.hits[0].bounds());
    }
}
