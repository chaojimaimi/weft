//! Scene model for the block context menu.

use crate::layout::ContextMenuLayout;
use crate::scene::{FocusId, HitRegion, Scene, SemanticNode, SemanticRole};

pub(crate) fn build_context_menu_scene(
    layout: ContextMenuLayout,
    labels: &[(&str, &str); 4],
) -> Scene<usize> {
    let mut scene = Scene::default();
    scene.semantics.push(SemanticNode {
        role: SemanticRole::Menu,
        label: "Block actions".into(),
        bounds: layout.menu_rect,
        focus: Some(FocusId::ContextMenu),
        state: String::new(),
    });
    for (index, (bounds, (label, _action))) in
        layout.item_rects.into_iter().zip(labels.iter()).enumerate()
    {
        let [x0, y0, x1, y1] = bounds;
        scene.hits.push(HitRegion {
            x0,
            y0,
            x1,
            y1,
            target: index,
        });
        scene.semantics.push(SemanticNode {
            role: SemanticRole::MenuItem,
            label: (*label).into(),
            bounds,
            focus: Some(FocusId::ContextMenu),
            state: String::new(),
        });
    }
    scene
}

pub(crate) fn context_menu_item_at(scene: &Scene<usize>, x: f32, y: f32) -> Option<usize> {
    scene
        .hits
        .iter()
        .find(|hit| hit.contains_half_open(x, y))
        .map(|hit| hit.target)
}

#[cfg(test)]
mod tests {
    use super::{build_context_menu_scene, context_menu_item_at};
    use crate::layout::{layout_context_menu, LayoutCtx};

    const ITEMS: &[(&str, &str); 4] = &[
        ("Copy Command", "copy_command"),
        ("Copy Output", "copy_output"),
        ("Toggle Fold", "toggle_fold"),
        ("Send to Input", "send_to_input"),
    ];

    #[test]
    fn scene_reuses_layout_bounds_for_hit_and_semantics() {
        let ctx = LayoutCtx::new((800.0, 600.0), 8.0, 18.0, 8.0, 8.0);
        let layout = layout_context_menu(&ctx, 100.0, 120.0, 1.0);
        let scene = build_context_menu_scene(layout, ITEMS);
        assert_eq!(scene.hits.len(), 4);
        assert_eq!(scene.semantics.len(), 5);
        for (index, hit) in scene.hits.iter().enumerate() {
            assert_eq!(hit.bounds(), layout.item_rects[index]);
            assert_eq!(scene.semantics[index + 1].bounds, hit.bounds());
        }
    }

    #[test]
    fn adjacent_item_boundary_keeps_old_half_open_ownership() {
        let ctx = LayoutCtx::new((800.0, 600.0), 8.0, 18.0, 8.0, 8.0);
        let layout = layout_context_menu(&ctx, 100.0, 120.0, 1.0);
        let scene = build_context_menu_scene(layout, ITEMS);
        let boundary_y = layout.item_rects[0][3];
        assert_eq!(context_menu_item_at(&scene, 110.0, boundary_y), Some(1));
        assert_eq!(
            context_menu_item_at(&scene, layout.item_rects[0][2], boundary_y),
            None
        );
    }
}
