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

pub(super) fn drawable_glyphs_for_char(ct_font: &core_text::font::CTFont, ch: char) -> Vec<u16> {
    use core_foundation::string::UniChar;

    let mut utf16 = [0u16; 3];
    let encoded = ch.encode_utf16(&mut utf16);
    let chars: Vec<UniChar> = encoded.iter().map(|&unit| unit as UniChar).collect();
    let mut glyphs = vec![0u16; chars.len()];
    let mapped = unsafe {
        ct_font.get_glyphs_for_characters(
            chars.as_ptr(),
            glyphs.as_mut_ptr(),
            chars.len() as core_foundation::base::CFIndex,
        )
    };
    if !mapped {
        return Vec::new();
    }

    // Astral-plane scalars use a UTF-16 surrogate pair. CoreText maps that
    // pair to `[real_glyph, 0]`; drawing the trailing missing glyph overlays
    // a tofu rectangle around the emoji. Glyph 0 never carries drawable
    // content here, so discard it before building parallel positions.
    glyphs.retain(|glyph| *glyph != 0);
    glyphs
}

/// Rasterize a color emoji (sbix bitmap) glyph to premultiplied RGBA via
/// CoreText + CoreGraphics. font-kit's `rasterize_glyph` cannot handle color
/// bitmap fonts (Apple Color Emoji produces 0 pixels on A8/RGBA32 canvases).
/// This bypasses font-kit by creating a color-supporting CGContext and using
/// `CTFontDrawGlyphs` directly, which renders the sbix bitmap in full color.
///
/// v1.10.4: the full RGBA is returned (premultiplied alpha, row-major, Y
/// top-down as the atlas expects) instead of reducing to an alpha mask — the
/// color atlas stores the emoji's own RGB. Callers must upload with
/// bytes_per_pixel = 4 into the color texture.
///
/// Returns `None` if the glyph cannot be found or rasterized.
pub(super) fn rasterize_emoji_rgba(
    font: &Font,
    ch: char,
    scaled_size: f32,
    glyph_w: u32,
    cell_h: u32,
    primary_descent_px: f32,
) -> Option<Vec<u8>> {
    use core_graphics::color_space::CGColorSpace;
    use core_graphics::context::CGContext;
    use core_text::font::CTFont;

    // font-kit loads CoreText fonts at a nominal 16pt. Unlike its own
    // rasterizer, CTFontDrawGlyphs does not take a separate size, so drawing
    // the native font directly made every color emoji stay near 16px even
    // after terminal zoom. Clone it at the atlas's physical pixel size.
    let ct_font: CTFont = font.native_font().clone_with_font_size(scaled_size as f64);

    // Map character → CGGlyph via UTF-16 (astral-plane chars need surrogate pair).
    let glyphs = drawable_glyphs_for_char(&ct_font, ch);
    if glyphs.is_empty() {
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
    // Provide one position per drawable glyph; draw_glyphs requires parallel
    // arrays and the UTF-16 mapping helper has removed surrogate placeholders.
    let positions: Vec<core_graphics_types::geometry::CGPoint> = glyphs
        .iter()
        .map(|_| core_graphics_types::geometry::CGPoint::new(0.0, primary_descent_px as f64))
        .collect();
    ct_font.draw_glyphs(&glyphs, &positions, ctx.clone());

    // Read premultiplied RGBA with Y-flip (atlas is top-down). All 4 channels
    // are kept — the color atlas stores the emoji's own RGB.
    let bpr = ctx.bytes_per_row();
    let raw = ctx.data();
    let mut rgba = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let src_idx = y * bpr + x * 4;
            if src_idx + 3 < raw.len() {
                let dst_y = h - 1 - y; // flip Y for top-down atlas
                let dst_idx = (dst_y * w + x) * 4;
                rgba[dst_idx] = raw[src_idx]; // R
                rgba[dst_idx + 1] = raw[src_idx + 1]; // G
                rgba[dst_idx + 2] = raw[src_idx + 2]; // B
                rgba[dst_idx + 3] = raw[src_idx + 3]; // A (premultiplied)
            }
        }
    }

    let nonzero = rgba.iter().filter(|&&p| p > 0).count();
    if nonzero == 0 {
        tracing::warn!("emoji '{ch}' rasterized to 0 pixels via CoreText");
        return None;
    }
    tracing::debug!("emoji '{ch}' rasterized: {nonzero} non-zero bytes");
    Some(rgba)
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
    font_from_handle(handle)
}

/// v1.10.12: Materialize a font-kit `Handle` (Path or Memory) into a `Font`.
/// User-installed families (Fira Code, JetBrains Mono, …) commonly resolve
/// to `Handle::Memory` — Core Text hands back font data in memory — and
/// skipping it silently fell back to Menlo for every custom family.
fn font_from_handle(handle: font_kit::handle::Handle) -> Option<Font> {
    match handle {
        font_kit::handle::Handle::Path { path, font_index } => {
            Font::from_path(path, font_index).ok()
        }
        font_kit::handle::Handle::Memory { bytes, font_index } => {
            Font::from_bytes(bytes, font_index).ok()
        }
    }
}

/// v1.10.12: Resolve a styled variant (bold / italic) of a font family.
///
/// Prefers the system source's `select_best_match` with the requested
/// weight/style properties (correct for any installed family, including
/// user-configured fonts). Falls back to known TTC face indices for the
/// bundled Menlo path (face order in Menlo.ttc: 0 regular, 1 bold,
/// 2 italic, 3 bold-italic).
///
/// v1.10.12-fix: `select_best_match` approximates missing variants — a
/// family without an Italic face (e.g. Fira Code ships only Regular/Bold/
/// Light/…) silently returns the Regular face for an Italic request. We
/// verify the returned face's actual properties and reject the mismatch,
/// so callers fall back to synthetic italic instead of rendering plain.
/// Returns `None` when the family has no usable variant.
pub(super) fn resolve_font_variant(
    family: &str,
    fallback_paths: &[&str],
    weight: font_kit::properties::Weight,
    style: font_kit::properties::Style,
) -> Option<Font> {
    if !family.is_empty() {
        let source = SystemSource::new();
        // font_kit Properties exposes weight/style/stretch as public fields.
        let props = Properties {
            weight,
            style,
            ..Properties::new()
        };
        if let Ok(handle) =
            source.select_best_match(&[FamilyName::Title(family.to_string())], &props)
        {
            if let Some(f) = font_from_handle(handle) {
                if variant_properties_match(&f, weight, style) {
                    return Some(f);
                }
            }
            // Core Text's best-match can approximate badly (e.g. a Bold
            // request for Menlo returns the Italic face). Fall through to
            // the path fallback — it verifies both properties and family.
        }
    }
    let face_index = match (
        weight == font_kit::properties::Weight::BOLD,
        style == font_kit::properties::Style::Italic,
    ) {
        (true, true) => 3,
        (true, false) => 1,
        (false, true) => 2,
        _ => 0,
    };
    for path in fallback_paths {
        if let Ok(f) = Font::from_path(path, face_index) {
            if variant_properties_match(&f, weight, style) && variant_matches_family(&f, family) {
                return Some(f);
            }
        }
    }
    None
}

/// v1.10.12-fix: a path fallback (Menlo.ttc faces) must only be used for
/// the family it actually belongs to — loading Menlo's italic face for a
/// Fira Code request would visually mix two unrelated fonts mid-line.
fn variant_matches_family(font: &Font, family: &str) -> bool {
    if family.is_empty() {
        return true;
    }
    let name = font.family_name();
    name == family || name.starts_with(family)
}

/// v1.10.12-fix: True when the loaded face really is the requested variant.
/// Bold accepts any weight ≥ 600 (Semibold/Bold); italic requires the actual
/// `Style::Italic` — Core Text's approximation of a missing italic face is
/// the regular face with `Style::Normal`, which must be rejected.
fn variant_properties_match(
    font: &Font,
    weight: font_kit::properties::Weight,
    style: font_kit::properties::Style,
) -> bool {
    let p = font.properties();
    if style == font_kit::properties::Style::Italic
        && p.style != font_kit::properties::Style::Italic
    {
        return false;
    }
    if weight == font_kit::properties::Weight::BOLD
        && p.weight < font_kit::properties::Weight::SEMIBOLD
    {
        return false;
    }
    true
}

#[allow(dead_code)]
pub(super) fn is_emoji_char(ch: char) -> bool {
    matches!(ch,
        '\u{231A}'..='\u{231B}' | '\u{23E9}'..='\u{23EC}' | '\u{23F0}' | '\u{23F3}' |
        '\u{25FD}'..='\u{25FE}' | '\u{2614}'..='\u{2615}' | '\u{2648}'..='\u{2653}' |
        '\u{267F}' | '\u{2693}' | '\u{26A1}' | '\u{26AA}'..='\u{26AB}' |
        '\u{26BD}'..='\u{26BE}' | '\u{26C4}'..='\u{26C5}' | '\u{26CE}' | '\u{26D4}' |
        '\u{26EA}' | '\u{26F2}'..='\u{26F3}' | '\u{26F5}' | '\u{26FA}' | '\u{26FD}' |
        '\u{2705}' | '\u{270A}'..='\u{270B}' | '\u{2728}' | '\u{274C}' | '\u{274E}' |
        '\u{2753}'..='\u{2755}' | '\u{2757}' | '\u{2795}'..='\u{2797}' |
        '\u{27B0}' | '\u{27BF}' |
        '\u{1F300}'..='\u{1F5FF}' | // Misc Symbols and Pictographs
        '\u{1F600}'..='\u{1F64F}' | // Emoticons
        '\u{1F680}'..='\u{1F6FF}' | // Transport and Map
        '\u{1F1E0}'..='\u{1F1FF}' | // Flags
        '\u{1F900}'..='\u{1FAFF}'   // Supplemental Symbols and Extended-A
    )
}

#[cfg(test)]
mod tests {
    use super::is_emoji_char;

    #[test]
    fn default_emoji_presentation_symbols_use_the_emoji_font() {
        for ch in ['✅', '☕', '⚽'] {
            assert!(is_emoji_char(ch), "{ch} must use Apple Color Emoji");
        }
        assert!(
            !is_emoji_char('★'),
            "text-presentation star stays in symbol font"
        );
    }
}
