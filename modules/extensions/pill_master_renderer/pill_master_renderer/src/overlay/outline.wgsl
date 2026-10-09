// The selection outline, traced in screen space around the mask the
// outlined meshes drew.

// The outline's colour, and its width in pixels in `settings.x`.
struct Outline {
    color: vec4<f32>,
    settings: vec4<f32>,
};

@group(0) @binding(0) var outline_mask: texture_2d<f32>;
@group(0) @binding(1) var<uniform> outline: Outline;

// One triangle that covers the whole target.
@vertex
fn fullscreen_vertex(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let x = f32((index << 1u) & 2u);
    let y = f32(index & 2u);
    return vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
}

// A pixel outside the mask with a masked pixel within the outline's width.
@fragment
fn outline_fragment(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let size = vec2<i32>(textureDimensions(outline_mask));
    let center = vec2<i32>(position.xy);
    var found = false;
    if textureLoad(outline_mask, center, 0).r < 0.5 {
        let radius = i32(outline.settings.x);
        for (var offset_y = -radius; offset_y <= radius; offset_y += 1) {
            for (var offset_x = -radius; offset_x <= radius; offset_x += 1) {
                let sample = clamp(center + vec2<i32>(offset_x, offset_y), vec2<i32>(0, 0), size - vec2<i32>(1, 1));
                if textureLoad(outline_mask, sample, 0).r > 0.5 {
                    found = true;
                }
            }
        }
    }
    if !found {
        discard;
    }
    return outline.color;
}
