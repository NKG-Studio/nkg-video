@group(0) @binding(0) var source: texture_2d<f32>;
struct Filters {
    // exposure multiplier, contrast, saturation, sepia
    color: vec4<f32>,
    // invert, vignette, enabled, padding
    effects: vec4<f32>,
}
@group(0) @binding(1) var<uniform> filters: Filters;

// GPUImage color adjustments, ported to WGSL and fused into the alpha/beauty pass.
// Upstream revision and BSD copyright/license are retained in THIRD_PARTY.md.
fn finish(rgb: vec3<f32>, alpha: f32, pixel: vec2<f32>) -> vec4<f32> {
    if filters.effects.z == 0.0 {
        return vec4(rgb * alpha, alpha);
    }
    // GPUImage's color formulas operate on encoded RGB. Decode again before
    // linear-light alpha premultiplication for egui's sRGB texture path.
    var c = select(1.055 * pow(max(rgb, vec3(0.0)), vec3(1.0 / 2.4)) - 0.055,
                   12.92 * rgb, rgb <= vec3(0.0031308));
    c *= filters.color.x;
    c = (c - vec3(0.5)) * filters.color.y + vec3(0.5);
    c = mix(vec3(dot(c, vec3(0.2125, 0.7154, 0.0721))), c, filters.color.z);
    let sepia = vec3(dot(c, vec3(0.3588, 0.7044, 0.1368)),
                     dot(c, vec3(0.2990, 0.5870, 0.1140)),
                     dot(c, vec3(0.2392, 0.4696, 0.0912)));
    c = clamp(mix(c, sepia, filters.color.w), vec3(0.0), vec3(1.0));
    c = mix(c, vec3(1.0) - c, filters.effects.x);
    let uv = pixel / vec2<f32>(textureDimensions(source));
    c *= 1.0 - filters.effects.y * smoothstep(0.3, 0.75, distance(uv, vec2(0.5)));
    let linear = select(pow((c + vec3(0.055)) / 1.055, vec3(2.4)),
                        c / 12.92, c <= vec3(0.04045));
    return vec4(linear * alpha, alpha);
}

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let positions = array<vec2<f32>, 3>(vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
    return vec4(positions[index], 0.0, 1.0);
}

@fragment
fn fs_main(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    // sRGB sampling decodes RGB; the sRGB render target encodes it again.
    // Premultiply in linear light, matching egui::Color32::from_rgba_unmultiplied.
    let color = textureLoad(source, vec2<i32>(position.xy), 0);
    return finish(color.rgb, color.a, position.xy);
}

fn skin_weight(rgb: vec3<f32>) -> f32 {
    let srgb = select(1.055 * pow(rgb, vec3(1.0 / 2.4)) - 0.055,
                      12.92 * rgb, rgb <= vec3(0.0031308));
    let cb = dot(srgb, vec3(-0.168736, -0.331264, 0.5)) + 0.5;
    let cr = dot(srgb, vec3(0.5, -0.418688, -0.081312)) + 0.5;
    // ponytail: color-only mask also catches skin-colored objects; use face/skin
    // segmentation if semantic isolation or difficult lighting becomes necessary.
    return smoothstep(0.28, 0.32, cb) * (1.0 - smoothstep(0.48, 0.52, cb))
         * smoothstep(0.51, 0.55, cr) * (1.0 - smoothstep(0.67, 0.71, cr));
}

@fragment
fn fs_beauty(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let pixel = vec2<i32>(position.xy);
    let size = vec2<i32>(textureDimensions(source));
    let center = textureLoad(source, pixel, 0);
    let mask = skin_weight(center.rgb);
    if center.a == 0.0 || mask < 0.001 {
        return finish(center.rgb, center.a, position.xy);
    }
    let step = max(1, min(3, min(size.x, size.y) / 720));
    var sum = vec3(0.0);
    var total = 0.0;
    for (var y = -2; y <= 2; y++) {
        for (var x = -2; x <= 2; x++) {
            let offset = vec2<i32>(x, y);
            let sample = textureLoad(source, clamp(pixel + offset * step, vec2(0), size - vec2(1)), 0);
            let delta = sample.rgb - center.rgb;
            // Transparent neighbors must not bleed their hidden RGB into the image.
            let weight = exp(-f32(x * x + y * y) / 4.0 - dot(delta, delta) / 0.0128) * sample.a;
            sum += sample.rgb * weight;
            total += weight;
        }
    }
    let softened = mix(center.rgb, sum / max(total, 0.00001), 0.65 * mask);
    let bright = softened + 0.12 * mask * (sqrt(softened) - softened);
    // Keep original coverage and premultiply exactly once, in linear light.
    return finish(bright, center.a, position.xy);
}
