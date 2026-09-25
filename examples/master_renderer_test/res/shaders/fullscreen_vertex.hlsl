// Fullscreen triangle: three vertices, no vertex buffer, no inputs.
//
// The clip-space corner comes from SV_VertexID and the uv is derived from it,
// which is the shape every post-processing pass in this engine draws. A pass
// that declares `PassKind::Fullscreen` must have a `vs_main` like this one; the
// renderer draws 0..3 with no vertex buffers and no depth.
//
// Edit here - `pill_assets` regenerates the .wgsl.

struct VOut {
    float4 position : SV_Position;
    float2 uv       : TEXCOORD0;
};

VOut vs_main(uint vertex_index : SV_VertexID) {
    // Oversized on purpose: the triangle covers the target without touching its
    // edges, so no interpolation runs off the end of the visible area.
    float2 corner[3] = {
        float2(-1.0, -3.0),
        float2( 3.0,  1.0),
        float2(-1.0,  1.0)
    };

    float2 position = corner[vertex_index];

    VOut output;
    output.position = float4(position, 0.0, 1.0);
    // v flips because clip space counts up and a texture counts down.
    output.uv = float2(position.x * 0.5 + 0.5, -position.y * 0.5 + 0.5);
    return output;
}
