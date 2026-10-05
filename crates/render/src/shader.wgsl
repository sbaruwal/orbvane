// All positions are in physical pixels, origin top-left.

struct Globals {
    viewport: vec2<f32>,
    _pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(1) @binding(0) var mask_tex: texture_2d<f32>;
@group(1) @binding(1) var color_tex: texture_2d<f32>;
@group(1) @binding(2) var atlas_sampler: sampler;

fn to_ndc(p: vec2<f32>) -> vec4<f32> {
    return vec4<f32>(p.x / globals.viewport.x * 2.0 - 1.0, 1.0 - p.y / globals.viewport.y * 2.0, 0.0, 1.0);
}

fn corner(vi: u32) -> vec2<f32> {
    return vec2<f32>(f32(vi & 1u), f32(vi >> 1u));
}

fn clipped(p: vec2<f32>, clip: vec4<f32>) -> bool {
    return p.x < clip.x || p.y < clip.y || p.x > clip.z || p.y > clip.w;
}

// ---------------------------------------------------------------- quads

struct QuadIn {
    @location(0) rect: vec4<f32>,
    @location(1) color: vec4<f32>,
    @location(2) clip: vec4<f32>,
    @location(3) params: vec4<f32>, // x: corner radius, y: border width
    @location(4) border_color: vec4<f32>,
};

struct QuadOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) clip: vec4<f32>,
    @location(2) rect: vec4<f32>,
    @location(3) params: vec4<f32>,
    @location(4) border_color: vec4<f32>,
};

@vertex
fn vs_quad(@builtin(vertex_index) vi: u32, q: QuadIn) -> QuadOut {
    var out: QuadOut;
    out.pos = to_ndc(q.rect.xy + corner(vi) * q.rect.zw);
    out.color = q.color;
    out.clip = q.clip;
    out.rect = q.rect;
    out.params = q.params;
    out.border_color = q.border_color;
    return out;
}

fn rounded_box_sdf(p: vec2<f32>, half: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - half + vec2<f32>(r);
    return length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
}

@fragment
fn fs_quad(in: QuadOut) -> @location(0) vec4<f32> {
    let p = in.pos.xy;
    if clipped(p, in.clip) {
        discard;
    }
    let half = in.rect.zw * 0.5;
    let center = in.rect.xy + half;
    let radius = min(in.params.x, min(half.x, half.y));
    let d = rounded_box_sdf(p - center, half, radius);
    let coverage = clamp(0.5 - d, 0.0, 1.0);
    var color = in.color;
    let border = in.params.y;
    if border > 0.0 {
        // Inside the border ring when -border < d <= 0.
        let t = clamp(0.5 + d + border, 0.0, 1.0);
        color = mix(in.color, in.border_color, t);
    }
    return vec4<f32>(color.rgb, color.a * coverage);
}

// ---------------------------------------------------------------- glyphs & icons

struct GlyphIn {
    @location(0) rect: vec4<f32>,
    @location(1) uv: vec4<f32>,
    @location(2) color: vec4<f32>,
    @location(3) clip: vec4<f32>,
    @location(4) kind: u32,
};

struct GlyphOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) clip: vec4<f32>,
    @location(3) @interpolate(flat) kind: u32,
};

@vertex
fn vs_glyph(@builtin(vertex_index) vi: u32, g: GlyphIn) -> GlyphOut {
    var out: GlyphOut;
    let c = corner(vi);
    out.pos = to_ndc(g.rect.xy + c * g.rect.zw);
    out.uv = g.uv.xy + c * g.uv.zw;
    out.color = g.color;
    out.clip = g.clip;
    out.kind = g.kind;
    return out;
}

@fragment
fn fs_glyph(in: GlyphOut) -> @location(0) vec4<f32> {
    // Sample before any discard so texture sampling stays in uniform control flow.
    let mask = textureSample(mask_tex, atlas_sampler, in.uv).r;
    let rgba = textureSample(color_tex, atlas_sampler, in.uv);
    if clipped(in.pos.xy, in.clip) {
        discard;
    }
    if in.kind == 0u {
        return vec4<f32>(in.color.rgb, in.color.a * mask);
    }
    return vec4<f32>(rgba.rgb, rgba.a * in.color.a);
}

// ---------------------------------------------------------------- images

@group(2) @binding(0) var image_tex: texture_2d<f32>;
@group(2) @binding(1) var image_sampler: sampler;

@fragment
fn fs_image(in: GlyphOut) -> @location(0) vec4<f32> {
    let rgba = textureSample(image_tex, image_sampler, in.uv);
    if clipped(in.pos.xy, in.clip) {
        discard;
    }
    return rgba;
}
