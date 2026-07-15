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
    sampler atlas_sampler [[sampler(0)]]
) {
    float mask = atlas.sample(atlas_sampler, in.tex_coord).r;
    float4 color = mix(in.bg_color, in.fg_color, mask);
    color.a = mix(in.bg_color.a, 1.0, mask);
    return color;
}
