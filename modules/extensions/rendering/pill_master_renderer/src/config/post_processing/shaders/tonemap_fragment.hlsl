// Lottes tonemap: the curve the reference renderer ends its frame with.
//
// Bindings follow the engine's convention rather than the reference's own
// numbering, so this pass binds its parameters and its input through the same
// slots a material uses: set 2 is the pass's own uniforms, set 3 the frames it
// reads. The cooked WGSL keeps those groups, and the renderer builds the bind
// groups to match.
//
// Edit here - `pill_assets` regenerates the .wgsl.

struct MaterialParams
{
    // Art-side shape, then the two constants `lottes_bc` resolves them to.
    float contrast;
    float shoulder;
    float b;
    float c;
};

[[vk::binding(0, 2)]]
ConstantBuffer<MaterialParams> material;

[[vk::binding(0, 3)]]
Texture2D<float4> hdr_texture;

[[vk::binding(1, 3)]]
SamplerState hdr_sampler;

struct PixelInput
{
    [[vk::location(0)]] float2 texture_coords : TEXCOORD0;
};

struct PixelOutput
{
    [[vk::location(0)]] float4 output : SV_TARGET0;
};

// Lottes' filmic curve. `b` and `c` place the toe and the shoulder; the
// reference derives them from `hdr_max`, `mid_in` and `mid_out` instead of
// asking an artist to tune four coupled numbers by hand.
//
// The shape constants are arguments rather than reads of the uniform, so the
// curve does not have to know which slot layout it is sitting behind.
float lottes(float x, float contrast, float shoulder, float b, float c)
{
    float powered = pow(max(x, 0.0), contrast);
    return powered / (pow(powered, shoulder) * b + c);
}

[shader("fragment")]
PixelOutput fs_main(PixelInput input)
{
    PixelOutput result;

    float3 hdr = hdr_texture.Sample(hdr_sampler, input.texture_coords).rgb;

    float contrast = material.contrast;
    float shoulder = material.shoulder;
    float b = material.b;
    float c = material.c;

    // Per channel, not per pixel: the three curves are independent, and a pixel
    // that misses one channel still has to keep the other two.
    result.output = float4(
        lottes(hdr.r, contrast, shoulder, b, c),
        lottes(hdr.g, contrast, shoulder, b, c),
        lottes(hdr.b, contrast, shoulder, b, c),
        1.0
    );

    return result;
}
