//! Shared keycap rendering for keyboard-hint footers.

use crate::paint::primitives::{push_filled_triangle, push_line, push_quad};
use crate::renderer::MetalRenderer;
use crate::settings_component::{
    settings_footer_hint_width, settings_keycap_width, FOOTER_HINT_GAP_CELLS,
    KEYCAP_DESCRIPTION_GAP_CELLS, KEYCAP_GAP_CELLS, KEYCAP_PAD_X_CELLS, SETTINGS_FOOTER_HINTS,
};

impl MetalRenderer {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn push_settings_footer_hints(
        &self,
        vertices: &mut Vec<f32>,
        start_x: f32,
        end_x: f32,
        row_y: f32,
        cell_w: f32,
        cell_h: f32,
        bg_uv: [f32; 4],
        accent: [f32; 4],
        description_color: [f32; 4],
    ) {
        let columns = (((end_x - start_x) / cell_w).max(1.0)) as usize;
        let mut x = start_x;
        for hint in SETTINGS_FOOTER_HINTS {
            let hint_width = settings_footer_hint_width(hint, cell_w);
            if x + hint_width > end_x {
                break;
            }
            let mut key_x = x;
            for (index, key) in hint.keys.iter().enumerate() {
                let key_w = settings_keycap_width(key, cell_w);
                let rect = [
                    key_x,
                    row_y + cell_h * 0.12,
                    key_x + key_w,
                    row_y + cell_h * 0.88,
                ];
                self.push_keycap(vertices, rect, key, bg_uv, accent, columns);
                key_x += key_w;
                if index + 1 < hint.keys.len() {
                    key_x += cell_w * KEYCAP_GAP_CELLS;
                }
            }
            let description_x = key_x + cell_w * KEYCAP_DESCRIPTION_GAP_CELLS;
            self.push_text(
                vertices,
                description_x,
                row_y,
                hint.description,
                description_color,
                columns,
            );
            x += hint_width + cell_w * FOOTER_HINT_GAP_CELLS;
        }
    }

    fn push_keycap(
        &self,
        vertices: &mut Vec<f32>,
        rect: [f32; 4],
        key: &str,
        bg_uv: [f32; 4],
        accent: [f32; 4],
        columns: usize,
    ) {
        let fill = [accent[0], accent[1], accent[2], 0.10];
        let border = [accent[0], accent[1], accent[2], 0.48];
        push_quad(vertices, rect, bg_uv, [0.0; 4], fill);
        let stroke = self.scale() as f32;
        for edge in [
            [rect[0], rect[1], rect[2], rect[1] + stroke],
            [rect[0], rect[3] - stroke, rect[2], rect[3]],
            [rect[0], rect[1], rect[0] + stroke, rect[3]],
            [rect[2] - stroke, rect[1], rect[2], rect[3]],
        ] {
            push_quad(vertices, edge, bg_uv, [0.0; 4], border);
        }
        if matches!(key, "↑" | "↓" | "←" | "→") {
            push_vector_arrow(vertices, rect, key, stroke.max(1.0), accent);
        } else {
            self.push_text(
                vertices,
                rect[0] + self.cell_width() as f32 * KEYCAP_PAD_X_CELLS,
                rect[1] - self.cell_height() as f32 * 0.12,
                key,
                accent,
                columns,
            );
        }
    }
}

fn push_vector_arrow(
    vertices: &mut Vec<f32>,
    rect: [f32; 4],
    key: &str,
    stroke: f32,
    color: [f32; 4],
) {
    let Some(geometry) = vector_arrow_geometry(rect, key) else {
        return;
    };
    push_line(
        vertices,
        geometry.shaft[0],
        geometry.shaft[1],
        geometry.shaft[2],
        geometry.shaft[3],
        stroke,
        color,
    );
    push_filled_triangle(
        vertices,
        geometry.tip[0],
        geometry.tip[1],
        geometry.base_a[0],
        geometry.base_a[1],
        geometry.base_b[0],
        geometry.base_b[1],
        color,
    );
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct VectorArrowGeometry {
    shaft: [f32; 4],
    tip: [f32; 2],
    base_a: [f32; 2],
    base_b: [f32; 2],
}

fn vector_arrow_geometry(rect: [f32; 4], key: &str) -> Option<VectorArrowGeometry> {
    let cx = (rect[0] + rect[2]) * 0.5;
    let cy = (rect[1] + rect[3]) * 0.5;
    let radius = (rect[2] - rect[0]).min(rect[3] - rect[1]) * 0.24;
    let wing = radius * 0.55;
    let (shaft, tip, base_a, base_b) = match key {
        "↑" => (
            [cx, cy + radius, cx, cy - radius * 0.35],
            [cx, cy - radius],
            [cx - wing, cy - radius * 0.25],
            [cx + wing, cy - radius * 0.25],
        ),
        "↓" => (
            [cx, cy - radius, cx, cy + radius * 0.35],
            [cx, cy + radius],
            [cx - wing, cy + radius * 0.25],
            [cx + wing, cy + radius * 0.25],
        ),
        "←" => (
            [cx + radius, cy, cx - radius * 0.35, cy],
            [cx - radius, cy],
            [cx - radius * 0.25, cy - wing],
            [cx - radius * 0.25, cy + wing],
        ),
        "→" => (
            [cx - radius, cy, cx + radius * 0.35, cy],
            [cx + radius, cy],
            [cx + radius * 0.25, cy - wing],
            [cx + radius * 0.25, cy + wing],
        ),
        _ => return None,
    };
    Some(VectorArrowGeometry {
        shaft,
        tip,
        base_a,
        base_b,
    })
}

#[cfg(test)]
mod tests {
    use super::vector_arrow_geometry;

    #[test]
    fn vector_arrow_pairs_share_one_centerline_and_mirrored_extents() {
        let rect = [10.0, 20.0, 30.0, 40.0];
        let up = vector_arrow_geometry(rect, "↑").unwrap();
        let down = vector_arrow_geometry(rect, "↓").unwrap();
        let left = vector_arrow_geometry(rect, "←").unwrap();
        let right = vector_arrow_geometry(rect, "→").unwrap();
        assert_eq!(up.tip[0], down.tip[0]);
        assert_eq!(up.tip[1] + down.tip[1], 60.0);
        assert_eq!(left.tip[1], right.tip[1]);
        assert_eq!(left.tip[0] + right.tip[0], 40.0);
        assert!(vector_arrow_geometry(rect, "esc").is_none());
    }
}
