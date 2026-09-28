// Cartoon fragment shader. Edit here — `pill_assets` regenerates the .wgsl.
// Posterizes the diffuse texture per channel. Ported from cartoon_fragment.glsl.
//
// Bind groups follow the engine convention (see include/common.hlsl):
//   group 0 = EngineParams, group 1 = CameraParams (declared, unused here),
//   group 2 = material parameters, group 3 = material textures.
// The material parameter `posterize_level` is a Scalar slot at slot 0, which the
// renderer packs at byte offset 0 (each slot is vec4-aligned / 16 bytes).

#include "include/common.hlsl"

struct MaterialParams
{
    float posterize_level;
};

[[vk::binding(0, 2)]]
ConstantBuffer<MaterialParams> material;

// Texture slot "color": texture_binding = 0, sampler_binding = 1 (group 3).
[[vk::binding(0, 3)]]
Texture2D color_texture;

[[vk::binding(1, 3)]]
SamplerState color_sampler;

struct PixelInput
{
    [[vk::location(0)]] float3 vertex_position       : TEXCOORD0;
    [[vk::location(1)]] float2 vertex_texture_coords : TEXCOORD1;
    [[vk::location(2)]] float3 TBN_tangent           : TEXCOORD2;
    [[vk::location(3)]] float3 TBN_bitangent         : TEXCOORD3;
    [[vk::location(4)]] float3 TBN_normal            : TEXCOORD4;
    [[vk::location(5)]] float3 world_position        : TEXCOORD5;
};

struct PixelOutput
{
    [[vk::location(0)]] float4 output : SV_TARGET0;
};

[shader("fragment")]
PixelOutput fs_main(PixelInput input)
{
    PixelOutput result;

    float4 object_color = color_texture.Sample(
        color_sampler,
        input.vertex_texture_coords
    );

    // Posterize per channel: floor(rgb * levels) / levels.
    float levels = max(material.posterize_level, 1.0);
    float3 posterized = floor(object_color.xyz * levels) / levels;

    result.output = float4(posterized, 1.0);
    return result;
}
