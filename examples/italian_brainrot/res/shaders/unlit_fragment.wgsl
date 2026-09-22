struct MaterialParameters {
    tint: vec3<f32>,
};

@group(2) @binding(0)
var<uniform> material: MaterialParameters;

@group(3) @binding(0)
var color_texture: texture_2d<f32>;

@group(3) @binding(1)
var color_sampler: sampler;

struct FragmentInput {
    @location(0) vertex_position: vec3<f32>,
    @location(1) texture_coordinates: vec2<f32>,
    @location(2) tbn_tangent: vec3<f32>,
    @location(3) tbn_bitangent: vec3<f32>,
    @location(4) tbn_normal: vec3<f32>,
    @location(5) world_position: vec3<f32>,
};

@fragment
fn fs_main(input: FragmentInput) -> @location(0) vec4<f32> {
    let color = textureSample(color_texture, color_sampler, input.texture_coordinates);
    return vec4<f32>(color.rgb * material.tint, color.a);
}
