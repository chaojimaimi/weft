//! Headless Metal pixel goldens for semantic UI surfaces.

use super::primitives::{color_to_normalized, push_quad};
use crate::ui_tokens::UiColors;
use metal::{
    CompileOptions, Device, MTLClearColor, MTLLoadAction, MTLOrigin, MTLPixelFormat,
    MTLPrimitiveType, MTLRegion, MTLResourceOptions, MTLSamplerAddressMode, MTLSamplerMinMagFilter,
    MTLSize, MTLStoreAction, MTLTextureType, MTLTextureUsage, RenderPassDescriptor,
    RenderPipelineDescriptor, SamplerDescriptor, TextureDescriptor, VertexDescriptor,
};
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use weft_core::config::Theme;

const LOGICAL_WIDTH: u32 = 320;
const LOGICAL_HEIGHT: u32 = 180;
const UPDATE_ENV: &str = "WEFT_UPDATE_METAL_GOLDENS";
const REQUIRE_ENV: &str = "WEFT_REQUIRE_METAL_GOLDENS";

struct Case {
    name: &'static str,
    theme: Theme,
    scale: u32,
}

fn cases() -> [Case; 4] {
    [
        Case {
            name: "metal_dark_1x",
            theme: Theme::weft_dark(),
            scale: 1,
        },
        Case {
            name: "metal_dark_2x",
            theme: Theme::weft_dark(),
            scale: 2,
        },
        Case {
            name: "metal_light_1x",
            theme: Theme::weft_light(),
            scale: 1,
        },
        Case {
            name: "metal_light_2x",
            theme: Theme::weft_light(),
            scale: 2,
        },
    ]
}

fn snapshot_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots")
        .join(format!("{name}.png"))
}

fn add_rect(vertices: &mut Vec<f32>, rect: [f32; 4], color: weft_core::grid::Color) {
    push_quad(
        vertices,
        rect,
        [0.0; 4],
        [0.0; 4],
        color_to_normalized(color),
    );
}

fn scene_vertices(theme: &Theme, scale: u32) -> Vec<f32> {
    let colors = UiColors::from_theme(theme);
    let s = scale as f32;
    let w = LOGICAL_WIDTH as f32 * s;
    let h = LOGICAL_HEIGHT as f32 * s;
    let mut vertices = Vec::new();

    add_rect(&mut vertices, [0.0, 0.0, w, h], colors.canvas);
    add_rect(&mut vertices, [0.0, 0.0, w, 28.0 * s], colors.chrome);
    add_rect(&mut vertices, [0.0, 28.0 * s, 92.0 * s, h], colors.panel);
    add_rect(
        &mut vertices,
        [112.0 * s, 48.0 * s, 300.0 * s, 154.0 * s],
        colors.raised,
    );
    add_rect(
        &mut vertices,
        [126.0 * s, 78.0 * s, 286.0 * s, 104.0 * s],
        colors.selection,
    );
    let mut translucent_focus = colors.focus;
    translucent_focus.a = 128;
    add_rect(
        &mut vertices,
        [154.0 * s, 90.0 * s, 258.0 * s, 132.0 * s],
        translucent_focus,
    );

    let focus = colors.focus;
    add_rect(
        &mut vertices,
        [112.0 * s, 48.0 * s, 300.0 * s, 50.0 * s],
        focus,
    );
    add_rect(
        &mut vertices,
        [112.0 * s, 152.0 * s, 300.0 * s, 154.0 * s],
        focus,
    );
    add_rect(
        &mut vertices,
        [112.0 * s, 50.0 * s, 114.0 * s, 152.0 * s],
        focus,
    );
    add_rect(
        &mut vertices,
        [298.0 * s, 50.0 * s, 300.0 * s, 152.0 * s],
        focus,
    );

    // v1.4.0: chrome elements that exercise the physical-pixel snap path.
    // These are drawn at fractional logical-pixel origins (e.g. y = 158.3 * s)
    // so the 1× and 2× goldens capture the snap output. Without these the
    // goldens only test integer-aligned rects (which trivially round-trip
    // through snap_physical_rect); the fractional cases prove the snap is
    // actually applied and produces integer physical pixels.
    //
    // The chrome elements live in the lower band of the scene (y ≥ 158) so
    // they don't overlap with the semantic surfaces above. Each rect's
    // pre-snap origin is fractional at 1× (e.g. 158.3); the snap rounds both
    // edges to integer physical pixels, and the golden captures the result.
    // A regression that drops the snap would show sub-pixel smearing on 1×
    // (the rect would render at y=158.3 → half-strength on pixel rows 158
    // and 159). The 2× golden is a sanity check that snapping still produces
    // integer physical pixels when the scale is 2 (158.3 * 2 = 316.6 → 317).
    let chrome_snap = crate::paint::primitives::snap_physical_rect;

    // Horizontal separator at fractional logical y=158.3 (post-snap: 158).
    let (sep_y0, sep_y1) = chrome_snap(158.3 * s, 160.3 * s);
    add_rect(&mut vertices, [0.0, sep_y0, w, sep_y1], focus);

    // Vertical scrollbar thumb at fractional logical x and y. Mimics the
    // block-view scrollbar: thumb on the right edge, 7px wide, ~30px tall.
    let (thumb_x0, thumb_x1) = chrome_snap(w - 7.3 * s, w - 0.3 * s);
    let (thumb_y0, thumb_y1) = chrome_snap(162.5 * s, 174.5 * s);
    add_rect(
        &mut vertices,
        [thumb_x0, thumb_y0, thumb_x1, thumb_y1],
        colors.selection,
    );

    // Horizontal pane divider at fractional logical y=176.4 (post-snap: 176).
    let (div_y0, div_y1) = chrome_snap(176.4 * s, 177.4 * s);
    add_rect(&mut vertices, [0.0, div_y0, w, div_y1], focus);

    vertices
}

fn flag_is_enabled(value: Option<&OsStr>) -> bool {
    value == Some(OsStr::new("1"))
}

fn render(device: &Device, case: &Case) -> (image::RgbaImage, Duration) {
    let width = LOGICAL_WIDTH * case.scale;
    let height = LOGICAL_HEIGHT * case.scale;
    let queue = device.new_command_queue();
    let library = device
        .new_library_with_source(include_str!("text_shader.metal"), &CompileOptions::new())
        .expect("compile production Metal shader");

    let pipeline_desc = RenderPipelineDescriptor::new();
    pipeline_desc.set_vertex_function(Some(
        &library
            .get_function("text_vertex", None)
            .expect("text_vertex"),
    ));
    pipeline_desc.set_fragment_function(Some(
        &library
            .get_function("text_fragment", None)
            .expect("text_fragment"),
    ));
    let vertex_desc = VertexDescriptor::new();
    for (index, offset) in [(0, 0), (1, 16), (2, 32)] {
        let attribute = vertex_desc.attributes().object_at(index).unwrap();
        attribute.set_format(metal::MTLVertexFormat::Float4);
        attribute.set_buffer_index(0);
        attribute.set_offset(offset);
    }
    vertex_desc.layouts().object_at(0).unwrap().set_stride(48);
    pipeline_desc.set_vertex_descriptor(Some(vertex_desc));
    let color = pipeline_desc.color_attachments().object_at(0).unwrap();
    color.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
    color.set_blending_enabled(true);
    color.set_source_rgb_blend_factor(metal::MTLBlendFactor::SourceAlpha);
    color.set_destination_rgb_blend_factor(metal::MTLBlendFactor::OneMinusSourceAlpha);
    color.set_source_alpha_blend_factor(metal::MTLBlendFactor::SourceAlpha);
    color.set_destination_alpha_blend_factor(metal::MTLBlendFactor::OneMinusSourceAlpha);
    let pipeline = device
        .new_render_pipeline_state(&pipeline_desc)
        .expect("create offscreen pipeline");

    let target_desc = TextureDescriptor::new();
    target_desc.set_texture_type(MTLTextureType::D2);
    target_desc.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
    target_desc.set_width(width as u64);
    target_desc.set_height(height as u64);
    target_desc.set_usage(MTLTextureUsage::RenderTarget);
    let target = device.new_texture(&target_desc);

    let atlas_desc = TextureDescriptor::new();
    atlas_desc.set_texture_type(MTLTextureType::D2);
    atlas_desc.set_pixel_format(MTLPixelFormat::R8Unorm);
    atlas_desc.set_width(1);
    atlas_desc.set_height(1);
    atlas_desc.set_usage(MTLTextureUsage::ShaderRead);
    let atlas = device.new_texture(&atlas_desc);
    let zero = [0_u8];
    atlas.replace_region(
        MTLRegion {
            origin: MTLOrigin { x: 0, y: 0, z: 0 },
            size: MTLSize {
                width: 1,
                height: 1,
                depth: 1,
            },
        },
        0,
        zero.as_ptr().cast(),
        1,
    );

    let sampler_desc = SamplerDescriptor::new();
    sampler_desc.set_min_filter(MTLSamplerMinMagFilter::Nearest);
    sampler_desc.set_mag_filter(MTLSamplerMinMagFilter::Nearest);
    sampler_desc.set_address_mode_s(MTLSamplerAddressMode::ClampToEdge);
    sampler_desc.set_address_mode_t(MTLSamplerAddressMode::ClampToEdge);
    let sampler = device.new_sampler(&sampler_desc);

    let vertices = scene_vertices(&case.theme, case.scale);
    let vertex_buffer = device.new_buffer_with_data(
        vertices.as_ptr().cast(),
        std::mem::size_of_val(vertices.as_slice()) as u64,
        MTLResourceOptions::CPUCacheModeDefaultCache,
    );
    let viewport = [width as f32, height as f32];
    let pass = RenderPassDescriptor::new();
    let attachment = pass.color_attachments().object_at(0).unwrap();
    attachment.set_texture(Some(&target));
    attachment.set_load_action(MTLLoadAction::Clear);
    attachment.set_store_action(MTLStoreAction::Store);
    attachment.set_clear_color(MTLClearColor::new(0.0, 0.0, 0.0, 1.0));

    let command_buffer = queue.new_command_buffer();
    let encoder = command_buffer.new_render_command_encoder(pass);
    encoder.set_render_pipeline_state(&pipeline);
    encoder.set_vertex_buffer(0, Some(&vertex_buffer), 0);
    encoder.set_vertex_bytes(1, 8, viewport.as_ptr().cast());
    encoder.set_fragment_texture(0, Some(&atlas));
    // v1.10.4: `text_fragment` declares color_atlas at texture(1). Snapshot
    // vertices are bg quads only (fg = [0;4], never the fg.a=2.0 sentinel),
    // so the branch is never taken — but the slot is declared, so bind a
    // benign texture rather than leaving it unbound if text quads are ever
    // added here.
    encoder.set_fragment_texture(1, Some(&atlas));
    encoder.set_fragment_sampler_state(0, Some(&sampler));
    encoder.draw_primitives(MTLPrimitiveType::Triangle, 0, (vertices.len() / 12) as u64);
    encoder.end_encoding();
    let blit = command_buffer.new_blit_command_encoder();
    blit.synchronize_resource(&target);
    blit.end_encoding();
    let submitted = Instant::now();
    command_buffer.commit();
    command_buffer.wait_until_completed();
    let gpu_completion = submitted.elapsed();

    let mut bgra = vec![0_u8; width as usize * height as usize * 4];
    target.get_bytes(
        bgra.as_mut_ptr().cast(),
        width as u64 * 4,
        MTLRegion {
            origin: MTLOrigin { x: 0, y: 0, z: 0 },
            size: MTLSize {
                width: width as u64,
                height: height as u64,
                depth: 1,
            },
        },
        0,
    );
    for pixel in bgra.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    (
        image::RgbaImage::from_raw(width, height, bgra).expect("valid offscreen image dimensions"),
        gpu_completion,
    )
}

fn assert_matches_golden(actual: &image::RgbaImage, path: &Path) {
    let expected = image::open(path)
        .unwrap_or_else(|error| panic!("open {}: {error}", path.display()))
        .into_rgba8();
    assert_eq!(actual.dimensions(), expected.dimensions());
    let mut changed = 0_usize;
    let mut max_delta = 0_u8;
    for (actual, expected) in actual.as_raw().iter().zip(expected.as_raw()) {
        let delta = actual.abs_diff(*expected);
        max_delta = max_delta.max(delta);
        changed += usize::from(delta > 1);
    }
    assert_eq!(
        changed,
        0,
        "{} changed: {changed} channels exceed tolerance; max delta {max_delta}",
        path.display()
    );
}

#[test]
fn dark_and_light_1x_2x_match_offscreen_metal_goldens() {
    let update_value = std::env::var_os(UPDATE_ENV);
    let update = flag_is_enabled(update_value.as_deref());
    let require_value = std::env::var_os(REQUIRE_ENV);
    let require = flag_is_enabled(require_value.as_deref());
    let Some(device) = Device::system_default() else {
        assert!(
            !require,
            "{REQUIRE_ENV}=1, but no Metal device is available"
        );
        eprintln!("skipping offscreen Metal goldens: no Metal device is available");
        return;
    };
    for case in cases() {
        let (actual, _) = render(&device, &case);
        let path = snapshot_path(case.name);
        if update {
            actual
                .save(&path)
                .unwrap_or_else(|error| panic!("save {}: {error}", path.display()));
        } else {
            assert_matches_golden(&actual, &path);
        }
    }
}

#[test]
#[ignore = "real Metal timing budget; run through scripts/performance_gate.sh"]
fn perf_offscreen_metal_frame_budget() {
    const SAMPLES: usize = 20;
    const P95_BUDGET: Duration = Duration::from_millis(20);

    let device = Device::system_default().expect("Metal device required for GPU frame gate");
    for case in cases() {
        let _ = render(&device, &case);
        let mut samples: Vec<_> = (0..SAMPLES).map(|_| render(&device, &case).1).collect();
        samples.sort();
        let p95_index = ((SAMPLES as f64 * 0.95).ceil() as usize).saturating_sub(1);
        let p95 = samples[p95_index];
        println!("{} Metal completion p95: {p95:?}", case.name);
        assert!(
            p95 <= P95_BUDGET,
            "{} Metal completion p95 {p95:?} exceeds {P95_BUDGET:?}",
            case.name
        );
    }
}

#[test]
fn environment_flags_require_exactly_one() {
    assert!(flag_is_enabled(Some(OsStr::new("1"))));
    for value in [
        None,
        Some(OsStr::new("")),
        Some(OsStr::new("0")),
        Some(OsStr::new("true")),
    ] {
        assert!(!flag_is_enabled(value));
    }
}
