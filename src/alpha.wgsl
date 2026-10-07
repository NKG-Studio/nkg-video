@group(0) @binding(0) var source: texture_2d<f32>;

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
    return vec4(color.rgb * color.a, color.a);
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
        return vec4(center.rgb * center.a, center.a);
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
    return vec4(bright * center.a, center.a);
}
