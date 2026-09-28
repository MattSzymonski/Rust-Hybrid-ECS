@binding(0) @group(3) var color_texture_0 : texture_2d<f32>;

@binding(1) @group(3) var color_sampler_0 : sampler;

struct MaterialParams_std140_0
{
    @align(16) posterize_level_0 : f32,
};

@binding(0) @group(2) var<uniform> material_0 : MaterialParams_std140_0;
struct PixelOutput_0
{
    @location(0) output_0 : vec4<f32>,
};

struct pixelInput_0
{
    @location(0) vertex_position_0 : vec3<f32>,
    @location(1) vertex_texture_coords_0 : vec2<f32>,
    @location(2) TBN_tangent_0 : vec3<f32>,
    @location(3) TBN_bitangent_0 : vec3<f32>,
    @location(4) TBN_normal_0 : vec3<f32>,
    @location(5) world_position_0 : vec3<f32>,
};

struct PixelInput_0
{
     vertex_position_1 : vec3<f32>,
     vertex_texture_coords_1 : vec2<f32>,
     TBN_tangent_1 : vec3<f32>,
     TBN_bitangent_1 : vec3<f32>,
     TBN_normal_1 : vec3<f32>,
     world_position_1 : vec3<f32>,
};

@fragment
fn fs_main( _S1 : pixelInput_0) -> PixelOutput_0
{
    var _S2 : PixelInput_0 = PixelInput_0( _S1.vertex_position_0, _S1.vertex_texture_coords_0, _S1.TBN_tangent_0, _S1.TBN_bitangent_0, _S1.TBN_normal_0, _S1.world_position_0 );
    var object_color_0 : vec4<f32> = (textureSample((color_texture_0), (color_sampler_0), (_S1.vertex_texture_coords_0)));
    var _S3 : f32 = max(material_0.posterize_level_0, 1.0f);
    var _S4 : vec3<f32> = vec3<f32>(_S3);
    var posterized_0 : vec3<f32> = floor(object_color_0.xyz * _S4) / _S4;
    var result_0 : PixelOutput_0;
    var _S5 : vec4<f32> = vec4<f32>(posterized_0, 1.0f);
    result_0.output_0 = _S5;
    return result_0;
}

