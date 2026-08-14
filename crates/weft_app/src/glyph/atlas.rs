//! Atlas coordinate helpers shared by prewarmed and dynamic glyph slots.

/// UV rect for a glyph in the atlas texture.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub struct GlyphInfo {
    /// Top-left UV coordinate in atlas (normalized 0..1).
    pub uv_origin: (f32, f32),
    /// UV size in atlas (normalized 0..1).
    pub uv_size: (f32, f32),
    /// Glyph bitmap size in pixels.
    pub size: (u32, u32),
    /// Horizontal advance width in pixels.
    pub advance: f32,
    /// Whether this glyph occupies double width (CJK).
    pub is_wide: bool,
    /// v1.10.4: whether this glyph's pixels live in the RGBA color atlas
    /// (color emoji — Apple Color Emoji sbix bitmaps) instead of the R8
    /// alpha-mask atlas. Color glyphs carry their own RGB; the instance's
    /// `fg` is ignored (a sentinel alpha encodes this to the shader).
    pub is_color: bool,
}

/// Return UVs spanning the centers of the slot's first and last pixels.
/// Linear sampling at raw slot boundaries blends adjacent transparent atlas
/// cells into edge-touching block glyphs, producing horizontal hairline gaps.
pub(super) fn pixel_center_uv(
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    atlas_width: u32,
    atlas_height: u32,
) -> ((f32, f32), (f32, f32)) {
    let aw = atlas_width.max(1) as f32;
    let ah = atlas_height.max(1) as f32;
    let origin = ((x as f32 + 0.5) / aw, (y as f32 + 0.5) / ah);
    let size = (
        width.saturating_sub(1) as f32 / aw,
        height.saturating_sub(1) as f32 / ah,
    );
    (origin, size)
}

#[cfg(test)]
mod tests {
    use super::pixel_center_uv;

    #[test]
    fn uv_edges_land_on_slot_pixel_centers() {
        let ((u, v), (uw, vh)) = pixel_center_uv(8, 16, 8, 16, 2048, 2048);
        assert_eq!(u, 8.5 / 2048.0);
        assert_eq!(v, 16.5 / 2048.0);
        assert_eq!(u + uw, 15.5 / 2048.0);
        assert_eq!(v + vh, 31.5 / 2048.0);
    }
}
