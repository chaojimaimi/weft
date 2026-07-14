//! Scene model for the block context menu.

use crate::layout::ContextMenuLayout;
use crate::scene::{FocusId, HitRegion, Scene, SemanticNode, SemanticRole};
use weft_core::input::KeyCode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ContextMenuKeyAction {
    Select(usize),
    Accept(usize),
    Cancel,
    Consume,
}

pub(crate) fn clamped_context_menu_selection(selection: usize, item_count: usize) -> Option<usize> {
    (item_count > 0).then(|| selection.min(item_count - 1))
}

pub(crate) fn context_menu_key_action(
    key: KeyCode,
    selection: usize,
    item_count: usize,
) -> ContextMenuKeyAction {
    let current = clamped_context_menu_selection(selection, item_count);
    match (key, current) {
        (KeyCode::Escape, _) => ContextMenuKeyAction::Cancel,
        (KeyCode::Enter, Some(current)) => ContextMenuKeyAction::Accept(current),
        (KeyCode::Up, Some(current)) => ContextMenuKeyAction::Select(current.saturating_sub(1)),
        (KeyCode::Down, Some(current)) => ContextMenuKeyAction::Select(
            current.saturating_add(1).min(item_count.saturating_sub(1)),
        ),
        _ => ContextMenuKeyAction::Consume,
    }
}

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
    use super::{
        build_context_menu_scene, clamped_context_menu_selection, context_menu_item_at,
        context_menu_key_action, ContextMenuKeyAction,
    };
    use crate::layout::{layout_context_menu, LayoutCtx};
    use weft_core::input::KeyCode;

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

    #[test]
    fn keyboard_navigation_clamps_and_modal_keys_are_consumed() {
        assert_eq!(
            context_menu_key_action(KeyCode::Up, 0, ITEMS.len()),
            ContextMenuKeyAction::Select(0)
        );
        assert_eq!(
            context_menu_key_action(KeyCode::Down, ITEMS.len() - 1, ITEMS.len()),
            ContextMenuKeyAction::Select(ITEMS.len() - 1)
        );
        assert_eq!(
            context_menu_key_action(KeyCode::Enter, 2, ITEMS.len()),
            ContextMenuKeyAction::Accept(2)
        );
        assert_eq!(
            context_menu_key_action(KeyCode::Escape, 2, ITEMS.len()),
            ContextMenuKeyAction::Cancel
        );
        assert_eq!(
            context_menu_key_action(KeyCode::Char('x'), 2, ITEMS.len()),
            ContextMenuKeyAction::Consume
        );
        assert_eq!(
            context_menu_key_action(KeyCode::Enter, usize::MAX, ITEMS.len()),
            ContextMenuKeyAction::Accept(ITEMS.len() - 1)
        );
        assert_eq!(
            context_menu_key_action(KeyCode::Up, usize::MAX, ITEMS.len()),
            ContextMenuKeyAction::Select(ITEMS.len() - 2)
        );
        assert_eq!(clamped_context_menu_selection(7, 0), None);
        assert_eq!(clamped_context_menu_selection(7, ITEMS.len()), Some(3));
    }
}
