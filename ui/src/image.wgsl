// Textured quads from the image atlas, with a clip rect and an optional tint.
// Positions are in physical pixels; the atlas holds premultiplied RGBA.

struct Globals {
    size: vec2<f32>,
    pad: vec2<f32>,
};
@group(0) @binding(0) var<uniform> g: Globals;
@group(1) @binding(0) var atlas: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;

struct Inst {
    @location(0) rect: vec4<f32>,
    @location(1) clip: vec4<f32>,
    @location(2) uv: vec4<f32>, // x, y, w, h in texture coordinates
    @location(3) tint: vec4<f32>, // straight alpha; a < 0: no tint
};

struct Out {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) clip: vec4<f32>,
    @location(2) @interpolate(flat) tint: vec4<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32, i: Inst) -> Out {
    let corner = vec2<f32>(f32(vi & 1u), f32(vi >> 1u));
    let p = i.rect.xy + corner * i.rect.zw;
    var o: Out;
    o.pos = vec4<f32>(p.x / g.size.x * 2.0 - 1.0, 1.0 - p.y / g.size.y * 2.0, 0.0, 1.0);
    o.uv = i.uv.xy + corner * i.uv.zw;
    o.clip = i.clip;
    o.tint = i.tint;
    return o;
}

@fragment
fn fs(o: Out) -> @location(0) vec4<f32> {
    let fp = o.pos.xy;
    if fp.x < o.clip.x || fp.y < o.clip.y || fp.x >= o.clip.z || fp.y >= o.clip.w {
        discard;
    }
    let c = textureSample(atlas, samp, o.uv);
    if o.tint.a < 0.0 {
        return c;
    }
    let a = c.a * o.tint.a;
    return vec4<f32>(o.tint.rgb * a, a);
}
