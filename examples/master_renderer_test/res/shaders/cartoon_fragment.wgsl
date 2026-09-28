@binding(0) @group(3) var color_texture : texture_2d<f32>;

@binding(1) @group(3) var color_sampler : sampler;

struct MaterialParams
{
    @align(16) posterize_level : f32,
};

@binding(0) @group(2) var<uniform> material : MaterialParams

struct PixelInput
{
    @location(0) in_vertex_position : vec3<f32>,
    @location(1) in_vertex_texture_coords : vec2<f32>,
    @location(2) in_TBN_tangent : vec3<f32>,
    @location(3) in_TBN_bitangent : vec3<f32>,
    @location(4) in_TBN_normal : vec3<f32>,
    @location(5) in_world_position : vec3<f32>,
};

struct PixelOutput
{
    @location(0) output : vec4<f32>,
};

@fragment
fn fs_main(input : PixelInput) -> PixelOutput
{
    var color : vec4<f32> = (textureSample((color_texture), (color_sampler), (input.in_vertex_texture_coords)));
    var levels : f32 = max(material.posterize_level, 1.0f);
    var posterized : vec3<f32> = floor(color.rgb * levels) / levels;
    var output : PixelOutput = PixelOutput(posterized, 1.0f);
    return output;
}

