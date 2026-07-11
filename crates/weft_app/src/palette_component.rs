//! Shared scene and layout selection for the command palette.

use crate::layout::{
    layout_palette_form_rect, layout_palette_search, LayoutCtx, PaletteSearchLayout, Rect,
};
use crate::scene::{FocusId, HitRegion, Scene, SemanticNode, SemanticRole};

const RESIZE_HOT_ZONE: f32 = 8.0;

#[derive(Clone, Copy, Debug)]
pub(crate) enum PaletteLayout {
    Search(PaletteSearchLayout),
    Form(Rect),
}

impl PaletteLayout {
    pub(crate) fn popup_rect(self) -> Rect {
        match self {
            Self::Search(layout) => layout.popup_rect,
            Self::Form(rect) => rect,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PaletteTarget {
    Item(usize),
    ResizeWidth,
    ResizeHeight,
}

pub(crate) fn derive_palette_layout(
    ctx: &LayoutCtx,
    entries_len: usize,
    selection: usize,
    form_fields: Option<usize>,
    popup_max_rows: usize,
    popup_width_scale: f32,
) -> PaletteLayout {
    match form_fields {
        Some(fields) => {
            PaletteLayout::Form(layout_palette_form_rect(ctx, fields, popup_width_scale))
        }
        None => PaletteLayout::Search(layout_palette_search(
            ctx,
            entries_len,
            selection,
            popup_max_rows,
            popup_width_scale,
        )),
    }
}

pub(crate) fn build_palette_scene(
    layout: PaletteLayout,
    item_labels: &[String],
    cell_h: f32,
) -> Scene<PaletteTarget> {
    let mut scene = Scene::default();
    let popup_rect = layout.popup_rect();
    scene.semantics.push(SemanticNode {
        role: SemanticRole::Dialog,
        label: "Command Palette".into(),
        bounds: popup_rect,
        focus: Some(FocusId::PaletteQuery),
    });

    if let PaletteLayout::Search(search) = layout {
        scene.semantics.push(SemanticNode {
            role: SemanticRole::TextField,
            label: "Palette query".into(),
            bounds: [
                search.popup_rect[0],
                search.query_y,
                search.popup_rect[2],
                search.query_y + cell_h,
            ],
            focus: Some(FocusId::PaletteQuery),
        });
        for index in search.start..search.end {
            let y0 = search.results_y + (index - search.start) as f32 * cell_h;
            let bounds = [search.popup_rect[0], y0, search.popup_rect[2], y0 + cell_h];
            scene.hits.push(HitRegion {
                x0: bounds[0],
                y0: bounds[1],
                x1: bounds[2],
                y1: bounds[3],
                target: PaletteTarget::Item(index),
            });
            scene.semantics.push(SemanticNode {
                role: SemanticRole::ListItem,
                label: item_labels.get(index).cloned().unwrap_or_default(),
                bounds,
                focus: Some(FocusId::PaletteQuery),
            });
        }
    }

    let [x0, y0, x1, y1] = popup_rect;
    scene.hits.push(HitRegion {
        x0,
        y0: y0 - RESIZE_HOT_ZONE,
        x1,
        y1: y0 + RESIZE_HOT_ZONE,
        target: PaletteTarget::ResizeHeight,
    });
    scene.hits.push(HitRegion {
        x0: x1 - RESIZE_HOT_ZONE,
        y0,
        x1: x1 + RESIZE_HOT_ZONE,
        y1,
        target: PaletteTarget::ResizeWidth,
    });
    scene
}

pub(crate) fn palette_target_at(
    scene: &Scene<PaletteTarget>,
    x: f32,
    y: f32,
) -> Option<PaletteTarget> {
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

    #[test]
    fn search_scene_maps_scrolled_rows_to_original_indices() {
        let ctx = LayoutCtx::new((1000.0, 700.0), 10.0, 20.0, 8.0, 8.0);
        let labels: Vec<String> = (0..20).map(|i| format!("item {i}")).collect();
        let layout = derive_palette_layout(&ctx, labels.len(), 15, None, 5, 0.6);
        let scene = build_palette_scene(layout, &labels, ctx.cell_h);
        let PaletteLayout::Search(search) = layout else {
            panic!("search layout")
        };
        assert_eq!((search.start, search.end), (11, 16));
        assert_eq!(
            palette_target_at(&scene, search.popup_rect[0] + 1.0, search.results_y + 1.0),
            Some(PaletteTarget::Item(11))
        );
        assert_eq!(scene.semantics[2].label, "item 11");
    }

    #[test]
    fn form_scene_has_no_item_targets_but_keeps_resize_targets() {
        let ctx = LayoutCtx::new((1000.0, 700.0), 10.0, 20.0, 8.0, 8.0);
        let layout = derive_palette_layout(&ctx, 10, 4, Some(3), 5, 0.6);
        let scene = build_palette_scene(layout, &[], ctx.cell_h);
        assert_eq!(scene.hits.len(), 2);
        let [_, top, right, _] = layout.popup_rect();
        assert_eq!(
            palette_target_at(&scene, right - 1.0, top),
            Some(PaletteTarget::ResizeWidth)
        );
    }

    #[test]
    fn click_below_last_visible_row_is_not_an_item() {
        let ctx = LayoutCtx::new((1000.0, 700.0), 10.0, 20.0, 8.0, 8.0);
        let labels = vec!["one".into(), "two".into()];
        let layout = derive_palette_layout(&ctx, labels.len(), 0, None, 8, 0.6);
        let scene = build_palette_scene(layout, &labels, ctx.cell_h);
        let PaletteLayout::Search(search) = layout else {
            panic!("search layout")
        };
        assert_eq!(
            palette_target_at(
                &scene,
                search.popup_rect[0] + 2.0,
                search.results_y + 2.0 * ctx.cell_h
            ),
            None
        );
    }
}
