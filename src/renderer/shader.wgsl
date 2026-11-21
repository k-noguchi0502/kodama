// Every visible element (cell background, glyph, underline, cursor) is one
// instanced quad. `kind` selects a solid fill, an alpha-mask glyph sampled
// from the atlas, a colour glyph (emoji) from the RGBA atlas, or an image.

struct Uniforms {
    screen_size: vec2<f32>,
    // 1.0 when the surface is sRGB and colors must be linearised.
    srgb: f32,
    _pad: f32,
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var atlas: texture_2d<f32>;
@group(0) @binding(2) var atlas_sampler: sampler;
@group(0) @binding(3) var color_atlas: texture_2d<f32>;
// The inline image (kitty graphics protocol) drawn by kind 3 quads.
@group(1) @binding(0) var image_tex: texture_2d<f32>;
@group(1) @binding(1) var image_sampler: sampler;

struct InstanceIn {
    @builtin(vertex_index) vertex: u32,
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) uv: vec4<f32>,
    @location(3) color: vec4<f32>,
    @location(4) kind: u32,
};

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) @interpolate(flat) kind: u32,
};

@vertex
fn vs_main(in: InstanceIn) -> VertexOut {
    let corner = vec2<f32>(f32(in.vertex & 1u), f32((in.vertex >> 1u) & 1u));
    let pixel = in.pos + corner * in.size;
    let ndc = vec2<f32>(
        pixel.x / u.screen_size.x * 2.0 - 1.0,
        1.0 - pixel.y / u.screen_size.y * 2.0,
    );

    var out: VertexOut;
    out.position = vec4<f32>(ndc, 0.0, 1.0);
    out.uv = in.uv.xy + corner * in.uv.zw;
    var color = in.color;
    if (u.srgb > 0.5) {
        color = vec4<f32>(pow(color.rgb, vec3<f32>(2.2)), color.a);
    }
    out.color = color;
    out.kind = in.kind;
    return out;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    if (in.kind == 0u) {
        return in.color;
    }
    if (in.kind >= 2u) {
        var texel: vec4<f32>;
        if (in.kind == 2u) {
            texel = textureSampleLevel(color_atlas, atlas_sampler, in.uv, 0.0);
        } else {
            texel = textureSampleLevel(image_tex, image_sampler, in.uv, 0.0);
        }
        if (u.srgb > 0.5) {
            texel = vec4<f32>(pow(texel.rgb, vec3<f32>(2.2)), texel.a);
        }
        return vec4<f32>(texel.rgb, texel.a * in.color.a);
    }
    let coverage = textureSampleLevel(atlas, atlas_sampler, in.uv, 0.0).r;
    return vec4<f32>(in.color.rgb, in.color.a * coverage);
}
