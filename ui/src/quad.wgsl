// Instanced rounded rectangles with an optional border and a clip rect.
// All inputs are in physical pixels.

struct Globals {
    size: vec2<f32>,
    pad: vec2<f32>,
};
@group(0) @binding(0) var<uniform> g: Globals;

struct Inst {
    @location(0) rect: vec4<f32>,
    @location(1) clip: vec4<f32>,
    @location(2) color: vec4<f32>,
    @location(3) border_color: vec4<f32>,
    @location(4) params: vec4<f32>, // radius, border width, blur
};

struct Out {
    @builtin(position) pos: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) @interpolate(flat) half: vec2<f32>,
    @location(2) @interpolate(flat) clip: vec4<f32>,
    @location(3) @interpolate(flat) color: vec4<f32>,
    @location(4) @interpolate(flat) border_color: vec4<f32>,
    @location(5) @interpolate(flat) params: vec4<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32, i: Inst) -> Out {
    let corner = vec2<f32>(f32(vi & 1u), f32(vi >> 1u));
    let p = i.rect.xy + corner * i.rect.zw;
    var o: Out;
    o.pos = vec4<f32>(p.x / g.size.x * 2.0 - 1.0, 1.0 - p.y / g.size.y * 2.0, 0.0, 1.0);
    o.half = i.rect.zw * 0.5;
    o.local = p - (i.rect.xy + o.half);
    o.clip = i.clip;
    o.color = i.color;
    o.border_color = i.border_color;
    o.params = i.params;
    return o;
}

fn rounded_box(p: vec2<f32>, half: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - half + vec2<f32>(r);
    return length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
}

@fragment
fn fs(o: Out) -> @location(0) vec4<f32> {
    let fp = o.pos.xy;
    if fp.x < o.clip.x || fp.y < o.clip.y || fp.x >= o.clip.z || fp.y >= o.clip.w {
        discard;
    }
    let r = min(o.params.x, min(o.half.x, o.half.y));
    let d = rounded_box(o.local, o.half, r);
    var coverage = clamp(0.5 - d, 0.0, 1.0);
    if o.params.z > 0.0 {
        // Soft shadow: fades out over the last 2 x blur pixels.
        coverage = 1.0 - smoothstep(-2.0 * o.params.z, 0.0, d);
    }
    var c = o.color;
    if o.params.y > 0.0 {
        let inner = clamp(0.5 - (d + o.params.y), 0.0, 1.0);
        c = mix(o.border_color, o.color, inner);
    }
    return vec4<f32>(c.rgb, c.a * coverage);
}
