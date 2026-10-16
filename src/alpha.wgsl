@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var<uniform> exposure_multiplier: f32;

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let positions = array<vec2<f32>, 3>(vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
    return vec4(positions[index], 0.0, 1.0);
}

@fragment
fn fs_main(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    // sRGB sampling decodes RGB; premultiply in linear light for egui.
    let color = textureLoad(source, vec2<i32>(position.xy), 0);
    if exposure_multiplier == 1.0 {
        return vec4(color.rgb * color.a, color.a);
    }
    // Keep GPUImage's encoded-RGB exposure behavior. License: THIRD_PARTY.md.
    let srgb = select(1.055 * pow(max(color.rgb, vec3(0.0)), vec3(1.0 / 2.4)) - 0.055,
                      12.92 * color.rgb, color.rgb <= vec3(0.0031308));
    let adjusted = clamp(srgb * exposure_multiplier, vec3(0.0), vec3(1.0));
    let linear = select(pow((adjusted + vec3(0.055)) / 1.055, vec3(2.4)),
                        adjusted / 12.92, adjusted <= vec3(0.04045));
    return vec4(linear * color.a, color.a);
}
