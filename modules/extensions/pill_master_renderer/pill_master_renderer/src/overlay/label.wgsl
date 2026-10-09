// Debug labels, drawn in screen space from the bitmap font atlas.

@group(0) @binding(0) var font_texture: texture_2d<f32>;
@group(0) @binding(1) var font_sampler: sampler;

struct LabelOutput {
    @builtin(position) clip: vec4<f32>,
    @location(0) texture_coordinates: vec2<f32>,
    @location(1) color: vec4<f32>,
};

// One corner of a glyph quad, already in normalized device coordinates.
@vertex
fn label_vertex(
    @location(0) position: vec2<f32>,
    @location(1) texture_coordinates: vec2<f32>,
    @location(2) color: vec4<f32>,
) -> LabelOutput {
    var output: LabelOutput;
    output.clip = vec4<f32>(position, 0.0, 1.0);
    output.texture_coordinates = texture_coordinates;
    output.color = color;
    return output;
}

@fragment
fn label_fragment(input: LabelOutput) -> @location(0) vec4<f32> {
    let coverage = textureSample(font_texture, font_sampler, input.texture_coordinates).r;
    if coverage < 0.5 {
        discard;
    }
    return input.color;
}
