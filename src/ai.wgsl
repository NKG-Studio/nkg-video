struct Warp { area: vec4<f32>, effect: vec4<f32> }
struct Parameters {
    beauty: vec4<f32>, // smoothing, whitening, mask available, unused
    counts: vec4<f32>, // warp count
    protect: array<vec4<f32>, 3>, // eyes, mouth; center / radii
    warps: array<Warp, 16>,
}
@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var linear_sampler: sampler;
@group(0) @binding(2) var<uniform> params: Parameters;
@group(0) @binding(3) var masks: texture_2d<f32>;
struct Vertex { @builtin(position) position: vec4<f32>, @location(0) uv: vec2<f32> }
@vertex fn vs_main(@builtin(vertex_index) i: u32) -> Vertex {
    let p = array<vec2<f32>,3>(vec2(-1.,-1.),vec2(3.,-1.),vec2(-1.,3.))[i];
    return Vertex(vec4(p,0.,1.), vec2(p.x * 0.5 + 0.5, 0.5 - p.y * 0.5));
}
@fragment fn capture(v: Vertex) -> @location(0) vec4<f32> {
    let c = textureSampleLevel(source, linear_sampler, v.uv, 0.);
    return vec4(c.rgb * c.a, 1.);
}
fn load(p: vec2<i32>) -> vec4<f32> {
    let size = vec2<i32>(textureDimensions(source));
    let c = textureLoad(source, clamp(p, vec2(0), size - vec2(1)), 0);
    return vec4(c.rgb * c.a, c.a);
}
fn sample_color(uv: vec2<f32>) -> vec4<f32> {
    // Interpolate premultiplied values so invisible RGB cannot bleed into edges.
    let p = uv * vec2<f32>(textureDimensions(source)) - vec2(0.5);
    let base = vec2<i32>(floor(p));
    let f = fract(p);
    let c = mix(mix(load(base), load(base + vec2(1,0)), f.x),
                mix(load(base + vec2(0,1)), load(base + vec2(1,1)), f.x), f.y);
    return vec4(c.rgb / max(c.a, 0.00001), c.a);
}
fn skin(uv: vec2<f32>) -> f32 {
    var weight = smoothstep(0.35, 0.8, textureSampleLevel(masks, linear_sampler, uv, 0.).r);
    for(var i=0u; i<3u; i++) {
        let p = params.protect[i];
        if p.z > 0. { weight *= smoothstep(0.7, 1.2, length((uv-p.xy)/p.zw)); }
    }
    return weight * params.beauty.z;
}
@fragment fn beautify(v: Vertex) -> @location(0) vec4<f32> {
    let size = vec2<f32>(textureDimensions(source));
    let aspect = vec2(size.x / size.y, 1.);
    var uv = v.uv;
    for(var i=0u; i<u32(params.counts.x); i++) {
        let w = params.warps[i];
        let d = uv-w.area.xy;
        let falloff = 1. - smoothstep(0.,1.,length(d / w.area.zw));
        if w.effect.w < 0.5 { uv += w.effect.xy * falloff * falloff; }
        else if w.effect.w < 1.5 { uv -= d * w.effect.z * falloff * falloff; }
        else {
            // Apply body deformation only to the person, leaving distant background fixed.
            let person = smoothstep(0.05, 0.6, textureSampleLevel(masks, linear_sampler, uv, 0.).g);
            uv += w.effect.xy * dot(d * aspect, w.effect.xy) / aspect * w.effect.z * falloff * person;
        }
    }
    uv = clamp(uv, vec2(0.5) / size, vec2(1.)-vec2(0.5)/size);
    let center = sample_color(uv);
    let weight = skin(uv);
    var color = center.rgb;
    if weight > 0.001 && center.a > 0. && params.beauty.x > 0. {
        var sum = vec3(0.);
        var total = 0.;
        let step = max(1., min(size.x,size.y)/540.) / size;
        for(var y=-1; y<=1; y++) { for(var x=-1; x<=1; x++) {
            let at = uv + vec2<f32>(f32(x),f32(y)) * step;
            let c = sample_color(at);
            let delta = c.rgb-center.rgb;
            let w = exp(-dot(delta,delta)/0.0128 - f32(x*x+y*y)*0.5) * c.a * skin(at);
            sum += c.rgb*w; total += w;
        }}
        color = mix(color,sum/max(total,0.00001),params.beauty.x*weight);
    }
    color += params.beauty.y * weight * 0.4 * (sqrt(max(color,vec3(0.)))-color);
    // Straight alpha output; existing video pass does the final premultiplication.
    return vec4(color, center.a);
}
