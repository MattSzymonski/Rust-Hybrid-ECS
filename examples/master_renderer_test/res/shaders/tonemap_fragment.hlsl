// Lottes tonemap: the curve the reference renderer ends its frame with.
//
// Bindings follow the engine's convention rather than the reference's own
// numbering, so this pass binds its parameters and its input through the same
// slots a material uses: set 2 is the pass's own uniforms, set 3 the frames it
// reads. The cooked WGSL keeps those groups, and the renderer builds the bind
// groups to match.
//
// Edit here - `pill_assets` regenerates the .wgsl.

struct TonemapParams {
    // Art-side shape, then the two constants `lottes_bc` resolves them to.
    float contrast;
    float shoulder;
    float b;
    float c;
};

[[vk::binding(0, 2)]] ConstantBuffer<TonemapParams> params;
[[vk::binding(0, 3)]] Texture2D<float4> hdr_texture;
[[vk::binding(1, 3)]] SamplerState hdr_sampler;

// Lottes' filmic curve. `b` and `c` place the toe and the shoulder; the
// reference derives them from `hdr_max`, `mid_in` and `mid_out` instead of
// asking an artist to tune four coupled numbers by hand.
float lottes(float x) {
    float powered = pow(max(x, 0.0), params.contrast);
    return powered / (pow(powered, params.shoulder) * params.b + params.c);
}

[shader("fragment")]
float4 fs_main(float2 uv : TEXCOORD0) : SV_Target {
    float3 hdr = hdr_texture.Sample(hdr_sampler, uv).rgb;
    // Per channel, not per pixel: the three curves are independent, and a pixel
    // that misses one channel still has to keep the other two.
    return float4(lottes(hdr.r), lottes(hdr.g), lottes(hdr.b), 1.0);
}
