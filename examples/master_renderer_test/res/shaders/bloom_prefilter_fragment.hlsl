// Bright pass: the one bloom step this project runs.
//
// Edit here - `pill_assets` regenerates the .wgsl.
//
// The reference builds bloom as a pyramid: a chain of downsampled targets and a
// matching upsample chain. This engine holds one target per name at frame size,
// so the chain here is a single level, and what survives is the part the
// composite needs - the light above the cut-off, softened so the bloom does not
// look like it was cut out with scissors. The reference's threshold-free
// downsample would need the pyramid to be a threshold at all.

struct BloomPrefilterParams {
    // x = cut-off, y = knee. z and w are unused.
    float4 threshold;
};

[[vk::binding(0, 2)]] ConstantBuffer<BloomPrefilterParams> params;
[[vk::binding(0, 3)]] Texture2D<float4> hdr_texture;
[[vk::binding(1, 3)]] SamplerState hdr_sampler;

[shader("fragment")]
float4 fs_main(float2 uv : TEXCOORD0) : SV_Target {
    // One texel, taken from the uv's own derivative rather than from a frame
    // size a pass parameter would have to be told and would owe an update on
    // every resize.
    float2 texel = float2(ddx(uv.x), ddy(uv.y));

    // Nine taps, centre-heavy, so the mask has a soft edge without a second
    // pass.
    float3 sum = float3(0.0, 0.0, 0.0);
    static const float weights[9] = { 1.0, 2.0, 1.0, 2.0, 4.0, 2.0, 1.0, 2.0, 1.0 };

    int tap = 0;
    [unroll] for (int y = -1; y <= 1; y++) {
        [unroll] for (int x = -1; x <= 1; x++) {
            float2 offset = float2(x, y) * texel;
            sum += hdr_texture.Sample(hdr_sampler, uv + offset).rgb * weights[tap];
            tap++;
        }
    }
    float3 colour = sum / 16.0;

    // Knee: what is over the cut-off keeps a fraction of how far over it is, so
    // a bright surface fades into the bloom instead of switching it on.
    float brightness = max(max(colour.r, colour.g), colour.b);
    float contribution = max(brightness - params.threshold.x, 0.0);
    contribution /= max(contribution + params.threshold.y, 1e-5);

    return float4(colour * contribution, 1.0);
}
