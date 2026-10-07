//! Shared scene, layout derivation and hit testing for editor completion.

use crate::layout::{completion_window, layout_completion, CompletionLayout, LayoutCtx};
use crate::scene::{FocusId, HitRegion, Scene, SemanticNode, SemanticRole};
use weft_core::complete::Match;
use weft_core::grid::terminal_text_width;

const RESIZE_HOT_ZONE: f32 = 8.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CompletionTarget {
    Item(usize),
    ResizeWidth,
    ResizeHeight,
}

pub(crate) fn derive_completion_layout(
    ctx: &LayoutCtx,
    matches: &[Match],
    selected: usize,
    prompt_line_count: usize,
    cursor: (usize, usize),
    popup_max_rows: usize,
    popup_width_scale: f32,
) -> Option<CompletionLayout> {
    if matches.is_empty() {
        return None;
    }
    let anchor_y =
        crate::layout::layout_prompt(ctx, prompt_line_count, cursor.0, cursor.1, 0).box_rect[1];
    let prompt_indent = usize::from(cursor.0 == 0) * 2;
    // v1.12.28 (P1-01 ②): anchor at the PANE's content left edge —
    // `ctx.left()` = padding_x + chrome_left + pane_origin.0. The old
    // window-level basis dropped pane_origin.0, so with the active pane in
    // the right half of a vertical split the popup sat one pane-width too
    // far left (visually detached from the prompt, crossing the divider).
    let box_x0 = (prompt_indent + cursor.1) as f32 * ctx.cell_w + ctx.left();
    let (start, end, _) = completion_window(
        anchor_y,
        ctx.cell_h,
        popup_max_rows,
        selected,
        matches.len(),
    );
    let max_label_cols = matches[start..end]
        .iter()
        .map(|candidate| terminal_text_width(candidate.label.as_str()))
        .max()
        .unwrap_or(10);
    Some(layout_completion(
        ctx,
        start,
        end,
        max_label_cols,
        anchor_y,
        box_x0,
        popup_width_scale,
    ))
}

pub(crate) fn build_completion_scene(
    layout: CompletionLayout,
    matches: &[Match],
    cell_h: f32,
) -> Scene<CompletionTarget> {
    let mut scene = Scene::default();
    scene.semantics.push(SemanticNode {
        role: SemanticRole::List,
        label: "Completions".into(),
        bounds: layout.popup_rect,
        focus: None,
        state: String::new(),
    });

    let [x0, y0, x1, y1] = layout.popup_rect;
    // Rows are added bottom-to-top so their target indices match rendering.
    for index in (layout.start..layout.end).rev() {
        let row = layout.end - 1 - index;
        let bottom = y1 - row as f32 * cell_h;
        let bounds = [x0, bottom - cell_h, x1, bottom];
        scene.hits.push(HitRegion {
            x0,
            y0: bounds[1],
            x1,
            y1: bounds[3],
            target: CompletionTarget::Item(index),
        });
        scene.semantics.push(SemanticNode {
            role: SemanticRole::ListItem,
            label: matches[index].label.clone(),
            bounds,
            focus: Some(FocusId::Completion),
            state: String::new(),
        });
    }
    scene.hits.push(HitRegion {
        x0,
        y0: y0 - RESIZE_HOT_ZONE,
        x1,
        y1: y0 + RESIZE_HOT_ZONE,
        target: CompletionTarget::ResizeHeight,
    });
    // Width is appended last so it retains the legacy priority at the
    // top-right corner when the two resize hot zones overlap.
    scene.hits.push(HitRegion {
        x0: x1 - RESIZE_HOT_ZONE,
        y0,
        x1: x1 + RESIZE_HOT_ZONE,
        y1,
        target: CompletionTarget::ResizeWidth,
    });
    scene
}

pub(crate) fn completion_target_at(
    scene: &Scene<CompletionTarget>,
    x: f32,
    y: f32,
) -> Option<CompletionTarget> {
    scene
        .hits
        .iter()
        .rev()
        .find(|hit| hit.contains_half_open(x, y))
        .map(|hit| hit.target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use weft_core::complete::{Match, MatchKind};

    fn candidate(label: &str) -> Match {
        Match {
            label: label.into(),
            kind: MatchKind::Command,
            insert: label.into(),
            is_dir: false,
            match_quality: weft_core::complete::MATCH_QUALITY_PREFIX,
        }
    }

    #[test]
    fn layout_keeps_selected_candidate_visible_and_uses_cjk_width() {
        let ctx = LayoutCtx::new((1000.0, 700.0), 10.0, 20.0, 8.0, 8.0);
        let matches = [candidate("a"), candidate("中中中中中中"), candidate("c")];
        let layout = derive_completion_layout(&ctx, &matches, 2, 1, (0, 4), 2, 0.6).unwrap();
        assert_eq!((layout.start, layout.end), (1, 3));
        assert_eq!(layout.label_cols, 12);
        assert_eq!(layout.popup_rect[0], 68.0);
    }

    #[test]
    fn scene_rows_follow_rendered_bottom_to_top_indices() {
        let ctx = LayoutCtx::new((1000.0, 700.0), 10.0, 20.0, 8.0, 8.0);
        let matches = [candidate("a"), candidate("b"), candidate("c")];
        let layout = derive_completion_layout(&ctx, &matches, 1, 1, (0, 0), 8, 0.6).unwrap();
        let scene = build_completion_scene(layout, &matches, ctx.cell_h);
        let bottom_y = layout.popup_rect[3] - 1.0;
        assert_eq!(
            completion_target_at(&scene, layout.popup_rect[0] + 2.0, bottom_y),
            Some(CompletionTarget::Item(2))
        );
        let top_row_y = layout.popup_rect[1] + ctx.cell_h * 0.5 + 1.0;
        assert_eq!(
            completion_target_at(&scene, layout.popup_rect[0] + 2.0, top_row_y),
            Some(CompletionTarget::Item(0))
        );
    }

    #[test]
    fn width_resize_handle_keeps_corner_priority_and_edges_are_half_open() {
        let ctx = LayoutCtx::new((1000.0, 700.0), 10.0, 20.0, 8.0, 8.0);
        let matches = [candidate("a")];
        let layout = derive_completion_layout(&ctx, &matches, 0, 1, (0, 0), 8, 0.6).unwrap();
        let scene = build_completion_scene(layout, &matches, ctx.cell_h);
        let [_, top, right, _] = layout.popup_rect;
        assert_eq!(
            completion_target_at(&scene, right - 1.0, top),
            Some(CompletionTarget::ResizeWidth)
        );
        assert_eq!(
            completion_target_at(&scene, right + RESIZE_HOT_ZONE, top + 10.0),
            None
        );
    }

    #[test]
    fn long_visual_prompt_anchors_completion_above_clamped_box() {
        let ctx = LayoutCtx::new((1000.0, 400.0), 10.0, 20.0, 8.0, 8.0);
        let matches = [candidate("cargo")];
        let layout = derive_completion_layout(&ctx, &matches, 0, 50, (49, 3), 8, 0.6).unwrap();
        let prompt_top = crate::layout::layout_prompt(&ctx, 50, 49, 3, 44).box_rect[1];
        assert_eq!(layout.popup_rect[3], prompt_top);
        assert!(layout.popup_rect[3] > 0.0);
    }

    /// v1.12.28 (P1-01 ②) double assertion: with the active pane in the right
    /// half of a split, the popup's left edge must be ≥ the pane's left edge
    /// (the old box_x0 dropped pane_origin.0 — the popup sat one pane-width
    /// too far left) and its right edge must be ≤ the pane's right edge (the
    /// width budget now rides ctx.width(), the clamp rides ctx.right()).
    /// for_pane numbers follow the layout/tests.rs:90 precedent.
    #[test]
    fn completion_popup_stays_inside_its_pane_in_a_split() {
        let mut ctx = LayoutCtx::new((1600.0, 1200.0), 10.0, 20.0, 16.0, 12.0);
        ctx.chrome_top = 45.0;
        ctx.chrome_left = 240.0;
        let pane = [800.0, 600.0, 1584.0, 1188.0];
        let pane_ctx = ctx.for_pane(pane);
        let matches = [candidate("cargo"), candidate("clippy")];
        let layout = derive_completion_layout(&pane_ctx, &matches, 0, 1, (0, 0), 8, 0.6).unwrap();
        assert!(
            layout.popup_rect[0] >= pane_ctx.left(),
            "popup left edge {} must not cross the pane's left edge {}",
            layout.popup_rect[0],
            pane_ctx.left()
        );
        assert!(
            layout.popup_rect[2] <= pane_ctx.right(),
            "popup right edge {} must not cross the pane's right edge {}",
            layout.popup_rect[2],
            pane_ctx.right()
        );
    }
}
