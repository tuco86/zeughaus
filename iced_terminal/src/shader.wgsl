// One terminal surface, drawn as instanced quads.
//
// Two pipelines share this module and the per-view uniform: `quad_*` fills
// rectangles (cell backgrounds, the selection, underlines, strikethrough, the
// cursor) and `glyph_*` blits from the glyph atlas. There is no vertex buffer;
// six vertices per instance are generated from the vertex index, because the
// instance already carries everything a rectangle needs.
//
// iced sets the render pass viewport to the widget's bounds before calling us,
// so positions are physical pixels measured from the top-left of the pane and
// `size` is the pane, not the window.

struct Uniforms {
    size: vec2<f32>,
    padding: vec2<f32>,
}

@group(0) @binding(0) var<uniform> uniforms: Uniforms;

@group(1) @binding(0) var atlas_texture: texture_2d<f32>;
@group(1) @binding(1) var atlas_sampler: sampler;

const KIND_SOLID: u32 = 0u;
const KIND_CURLY: u32 = 1u;
const KIND_DOTTED: u32 = 2u;
const KIND_DASHED: u32 = 3u;

const FLAG_COLOR_GLYPH: u32 = 1u;

const TAU: f32 = 6.2831855;

fn unit_corner(index: u32) -> vec2<f32> {
    var xs = array<f32, 6>(0.0, 1.0, 0.0, 1.0, 1.0, 0.0);
    var ys = array<f32, 6>(0.0, 0.0, 1.0, 0.0, 1.0, 1.0);
    return vec2<f32>(xs[index], ys[index]);
}

fn to_clip(position: vec2<f32>) -> vec4<f32> {
    let x = position.x / max(uniforms.size.x, 1.0) * 2.0 - 1.0;
    let y = 1.0 - position.y / max(uniforms.size.y, 1.0) * 2.0;
    return vec4<f32>(x, y, 0.0, 1.0);
}

struct QuadInstance {
    @location(0) rect: vec4<f32>,
    @location(1) color: vec4<f32>,
    @location(2) kind: u32,
}

struct QuadVertex {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) local: vec2<f32>,
    @location(2) @interpolate(flat) kind: u32,
    @location(3) @interpolate(flat) extent: vec2<f32>,
}

@vertex
fn quad_vertex(
    instance: QuadInstance,
    @builtin(vertex_index) index: u32,
) -> QuadVertex {
    let corner = unit_corner(index);
    var out: QuadVertex;
    out.clip = to_clip(instance.rect.xy + corner * instance.rect.zw);
    out.color = instance.color;
    out.local = corner;
    out.kind = instance.kind;
    out.extent = instance.rect.zw;
    return out;
}

@fragment
fn quad_fragment(in: QuadVertex) -> @location(0) vec4<f32> {
    if in.kind == KIND_SOLID {
        return in.color;
    }

    let x = in.local.x * in.extent.x;

    if in.kind == KIND_DOTTED {
        let period = max(in.extent.y * 3.0, 3.0);
        if fract(x / period) > 0.5 {
            discard;
        }
        return in.color;
    }

    if in.kind == KIND_DASHED {
        let period = max(in.extent.y * 8.0, 8.0);
        if fract(x / period) > 0.6 {
            discard;
        }
        return in.color;
    }

    // Curly: the instance is three strokes tall so the wave has room.
    let thickness = in.extent.y / 3.0;
    let amplitude = (in.extent.y - thickness) * 0.5;
    let period = max(in.extent.y * 2.0, 4.0);
    let centre = in.extent.y * 0.5 + sin(x / period * TAU) * amplitude;
    let distance = abs(in.local.y * in.extent.y - centre);
    let coverage = clamp(thickness * 0.5 + 0.5 - distance, 0.0, 1.0);
    if coverage <= 0.0 {
        discard;
    }
    return vec4<f32>(in.color.rgb, in.color.a * coverage);
}

struct GlyphInstance {
    @location(0) rect: vec4<f32>,
    @location(1) uv: vec4<f32>,
    @location(2) color: vec4<f32>,
    @location(3) flags: u32,
}

struct GlyphVertex {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) @interpolate(flat) flags: u32,
}

@vertex
fn glyph_vertex(
    instance: GlyphInstance,
    @builtin(vertex_index) index: u32,
) -> GlyphVertex {
    let corner = unit_corner(index);
    var out: GlyphVertex;
    out.clip = to_clip(instance.rect.xy + corner * instance.rect.zw);
    out.uv = instance.uv.xy + corner * instance.uv.zw;
    out.color = instance.color;
    out.flags = instance.flags;
    return out;
}

@fragment
fn glyph_fragment(in: GlyphVertex) -> @location(0) vec4<f32> {
    let texel = textureSample(atlas_texture, atlas_sampler, in.uv);
    if (in.flags & FLAG_COLOR_GLYPH) != 0u {
        // Emoji and other colour bitmaps carry their own colour.
        if texel.a <= 0.0 {
            discard;
        }
        return texel;
    }
    let alpha = texel.a * in.color.a;
    if alpha <= 0.0 {
        discard;
    }
    return vec4<f32>(in.color.rgb, alpha);
}
