//! Font resolution and emoji rasterization helpers for the glyph atlas.
//!
//! Split from `glyph/mod.rs` (Batch 6 Step 4) to keep the main file under
//! the 800-line limit. All items are `pub(super)` — visible only within
//! the `glyph` module tree.

use font_kit::family_name::FamilyName;
use font_kit::loaders::core_text::Font;
use font_kit::properties::Properties;
use font_kit::source::SystemSource;
use tracing::{debug, warn};

pub(super) fn nonzero_cell_dimension(dimension: u32) -> u32 {
    dimension.max(1)
}

/// Rasterize a color emoji (sbix bitmap) glyph to an alpha mask via CoreText +
/// CoreGraphics. font-kit's `rasterize_glyph` cannot handle color bitmap fonts
/// (Apple Color Emoji produces 0 pixels on A8/RGBA32 canvases). This bypasses
/// font-kit by creating a color-supporting CGContext and using
/// `CTFontDrawGlyphs` directly, which renders the sbix bitmap in full color.
/// The RGBA result is reduced to a single alpha channel for the R8 atlas.
///
/// Returns `None` if the glyph cannot be found or rasterized.
pub(super) fn rasterize_emoji_alpha(
    font: &Font,
    ch: char,
    glyph_w: u32,
    cell_h: u32,
) -> Option<Vec<u8>> {
    use core_foundation::string::UniChar;
    use core_graphics::color_space::CGColorSpace;
    use core_graphics::context::CGContext;
    use core_text::font::CTFont;

    let ct_font: CTFont = font.native_font();

    // Map character → CGGlyph via UTF-16 (astral-plane chars need surrogate pair).
    let mut utf16 = [0u16; 3];
    let encoded = ch.encode_utf16(&mut utf16);
    let chars: Vec<UniChar> = encoded.iter().map(|&u| u as UniChar).collect();
    let mut glyphs = vec![0u16; chars.len()];
    let got = unsafe {
        ct_font.get_glyphs_for_characters(
            chars.as_ptr(),
            glyphs.as_mut_ptr(),
            chars.len() as core_foundation::base::CFIndex,
        )
    };
    if !got || glyphs.iter().all(|&g| g == 0) {
        return None;
    }

    // Create a color-supporting RGBA bitmap context. sbix bitmaps only render
    // in RGB color spaces — device-gray yields 0 pixels.
    let cs = CGColorSpace::create_device_rgb();
    let w = glyph_w as usize;
    let h = cell_h as usize;
    let mut ctx = CGContext::create_bitmap_context(
        None,
        w,
        h,
        8,     // bitsPerComponent
        w * 4, // bytesPerRow
        &cs,
        core_graphics::base::kCGImageAlphaPremultipliedLast,
    );

    // Draw glyph at bottom-left with descent offset (CG is Y-up).
    // Provide one position per glyph (astral-plane chars may produce 2 glyphs
    // from a surrogate pair — draw_glyphs asserts glyphs.len() == positions.len()).
    let descent = ct_font.descent();
    let positions: Vec<core_graphics_types::geometry::CGPoint> = glyphs
        .iter()
        .map(|_| core_graphics_types::geometry::CGPoint::new(0.0, descent.abs()))
        .collect();
    ct_font.draw_glyphs(&glyphs, &positions, ctx.clone());

    // Read RGBA → alpha mask with Y-flip (atlas is top-down).
    let bpr = ctx.bytes_per_row();
    let raw = ctx.data();
    let mut alpha = vec![0u8; w * h];
    for y in 0..h {
        for x in 0..w {
            let src_idx = y * bpr + x * 4;
            if src_idx + 3 < raw.len() {
                let dst_y = h - 1 - y; // flip Y for top-down atlas
                alpha[dst_y * w + x] = raw[src_idx + 3];
            }
        }
    }

    let nonzero = alpha.iter().filter(|&&p| p > 0).count();
    if nonzero == 0 {
        tracing::warn!("emoji '{ch}' rasterized to 0 pixels via CoreText");
        return None;
    }
    tracing::debug!("emoji '{ch}' rasterized: {nonzero} non-zero pixels");
    Some(alpha)
}

/// Resolve a font by family name via the system source, falling back to a list
/// of absolute `.ttc` paths (the bundled macOS defaults). Returns the first
/// loadable font.
///
/// Logging policy: a family-name miss is common and harmless on macOS —
/// font-kit's `FamilyName::Title` lookup doesn't always index built-in fonts
/// (Menlo, PingFang, Apple Color Emoji, Apple Symbols) on every locale / OS
/// version, so the path fallback is the de-facto primary path in practice.
/// Therefore:
///   - family miss + path hit  → `debug!` (noise-free in normal runs)
///   - family miss + path miss → `warn!` (genuine load failure worth surfacing)
pub(super) fn resolve_font(family: &str, fallback_paths: &[&str]) -> Option<Font> {
    if !family.is_empty() {
        if let Some(f) = load_by_family(family) {
            return Some(f);
        }
    }
    // Family lookup missed (or was empty) — try the bundled paths. We log at
    // debug here because the path fallback is expected to succeed; if every
    // path also fails we escalate to warn below.
    for path in fallback_paths {
        match Font::from_path(path, 0) {
            Ok(f) => {
                debug!(
                    family,
                    path, "font family not found; loaded via path fallback"
                );
                return Some(f);
            }
            Err(e) => {
                debug!(family, path, error = %e, "path fallback failed");
            }
        }
    }
    warn!(
        family,
        fallback_count = fallback_paths.len(),
        "font load failed: family not found and no path fallback succeeded"
    );
    None
}

/// Look up a font by family name using the Core Text system source.
fn load_by_family(family: &str) -> Option<Font> {
    let source = SystemSource::new();
    let handle = source
        .select_best_match(&[FamilyName::Title(family.to_string())], &Properties::new())
        .ok()?;
    match handle {
        font_kit::handle::Handle::Path { path, font_index } => {
            Font::from_path(path, font_index).ok()
        }
        // Memory handles are rare (in-process fonts); skip them.
        font_kit::handle::Handle::Memory { .. } => None,
    }
}

#[allow(dead_code)]
pub(super) fn is_emoji_char(ch: char) -> bool {
    matches!(ch,
        '\u{1F600}'..='\u{1F64F}' | // Emoticons
        '\u{1F300}'..='\u{1F5FF}' | // Misc Symbols and Pictographs
        '\u{1F680}'..='\u{1F6FF}' | // Transport and Map
        '\u{1F1E0}'..='\u{1F1FF}' | // Flags
        '\u{2600}'..='\u{26FF}'   | // Misc symbols
        '\u{2700}'..='\u{27BF}'     // Dingbats
    )
}
