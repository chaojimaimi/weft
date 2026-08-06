#include <metal_stdlib>
using namespace metal;

struct TextVertexIn {
    float4 position_tex [[attribute(0)]];  // xy = position, zw = tex_coord
    float4 fg_color [[attribute(1)]];
    float4 bg_color [[attribute(2)]];
};

struct TextVertexOut {
    float4 position [[position]];
    float2 tex_coord;
    float4 fg_color;
    float4 bg_color;
    float is_bg;
};

vertex TextVertexOut text_vertex(
    TextVertexIn vin [[stage_in]],
    constant float2& viewport_size [[buffer(1)]]
) {
    TextVertexOut out;

    float2 position = vin.position_tex.xy;
    float2 tex_coord = vin.position_tex.zw;

    // Map logical pixels (origin top-left) to clip space. The CAMetalLayer on this
    // NSView composites with an extra vertical flip, so we sample each glyph's V
    // inverted (swapped in build_grid_vertices) to keep letters upright on screen.
    float2 clip = (position / viewport_size) * 2.0 - 1.0;
    clip.y = -clip.y;

    out.position = float4(clip, 0.0, 1.0);
    out.tex_coord = tex_coord;
    out.fg_color = vin.fg_color;
    out.bg_color = vin.bg_color;
    out.is_bg = 0.0;
    return out;
}

// v1.0 P1.5-B1: Instanced vertex shader for grid cells. Each instance is
// one cell: a quad (4 verts indexed as 0,1,2,0,2,3) with per-instance
// origin/size/uv_rect/fg/bg. Corners are derived from vertex_id so no
// static vertex buffer is needed — only the index buffer + instance buffer.
struct CellInstance {
    float2 origin;
    float2 size;
    float4 uv_rect;
    float4 fg;
    float4 bg;
};

vertex TextVertexOut text_vertex_instanced(
    uint vid [[vertex_id]],
    uint iid [[instance_id]],
    constant float2& viewport_size [[buffer(1)]],
    constant CellInstance* instances [[buffer(2)]]
) {
    TextVertexOut out;
    float2 corner;
    switch (vid) {
        case 0: corner = float2(0.0, 0.0); break;
        case 1: corner = float2(0.0, 1.0); break;
        case 2: corner = float2(1.0, 1.0); break;
        default: corner = float2(1.0, 0.0); break;
    }
    CellInstance inst = instances[iid];
    float2 position = inst.origin + corner * inst.size;
    float2 tex_coord = float2(
        mix(inst.uv_rect.x, inst.uv_rect.z, corner.x),
        mix(inst.uv_rect.y, inst.uv_rect.w, corner.y)
    );
    float2 clip = (position / viewport_size) * 2.0 - 1.0;
    clip.y = -clip.y;
    out.position = float4(clip, 0.0, 1.0);
    out.tex_coord = tex_coord;
    out.fg_color = inst.fg;
    out.bg_color = inst.bg;
    out.is_bg = 0.0;
    return out;
}

fragment float4 text_fragment(
    TextVertexOut in [[stage_in]],
    texture2d<float> atlas [[texture(0)]],
    texture2d<float> color_atlas [[texture(1)]],
    sampler atlas_sampler [[sampler(0)]]
) {
    // v1.10.4: fg.a > 1.5 is the color-emoji sentinel — `push_row` emits
    // fg = [0,0,0,2.0] for glyphs stored in the RGBA color atlas (normal
    // fg alpha is ≤ 1.0, verified by unit test). Sample the color texture
    // and blend the emoji's own RGB over the background. The atlas stores
    // premultiplied RGBA, so divide by alpha to recover straight RGB for
    // the same blend math as the mask path below.
    if (in.fg_color.a > 1.5) {
        float4 c = color_atlas.sample(atlas_sampler, in.tex_coord);
        float3 rgb = c.a > 0.001 ? c.rgb / c.a : float3(0.0);
        float4 color = mix(in.bg_color, float4(rgb, 1.0), c.a);
        color.a = mix(in.bg_color.a, 1.0, c.a);
        return color;
    }
    float mask = atlas.sample(atlas_sampler, in.tex_coord).r;
    float4 color = mix(in.bg_color, in.fg_color, mask);
    color.a = mix(in.bg_color.a, 1.0, mask);
    return color;
}

// v1.4.2 Phase B3: Background-stream pipeline. Draws solid color quads
// without atlas sampling. Each instance is one run of same-bg cells
// (8 floats: origin(2) + size(2) + bg(4) = 32 bytes). The bg stream is
// drawn first per pane, then the glyph stream (instanced pipeline above)
// draws text/decoration on top with transparent bg.
//
// Pixel equivalence with the single-stream path:
//   single: color = mix(bg, fg, mask), alpha = mix(bg.a, 1, mask)
//   dual:   bg stream draws bg (mask=0), then glyph stream draws
//           mix(transparent, fg, mask) on top via alpha blending.
//   Result: bg * (1-mask) + fg * mask — identical to single-stream.
struct BgInstance {
    float2 origin;
    float2 size;
    float4 bg;
};

struct BgVertexOut {
    float4 position [[position]];
    float4 bg_color;
};

vertex BgVertexOut bg_vertex(
    uint vid [[vertex_id]],
    uint iid [[instance_id]],
    constant float2& viewport_size [[buffer(1)]],
    constant BgInstance* instances [[buffer(2)]]
) {
    BgVertexOut out;
    float2 corner;
    switch (vid) {
        case 0: corner = float2(0.0, 0.0); break;
        case 1: corner = float2(0.0, 1.0); break;
        case 2: corner = float2(1.0, 1.0); break;
        default: corner = float2(1.0, 0.0); break;
    }
    BgInstance inst = instances[iid];
    float2 position = inst.origin + corner * inst.size;
    float2 clip = (position / viewport_size) * 2.0 - 1.0;
    clip.y = -clip.y;
    out.position = float4(clip, 0.0, 1.0);
    out.bg_color = inst.bg;
    return out;
}

fragment float4 bg_fragment(
    BgVertexOut in [[stage_in]]
) {
    return in.bg_color;
}
