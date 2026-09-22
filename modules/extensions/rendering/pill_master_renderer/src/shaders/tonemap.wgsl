// Exposure and display conversion for the linear HDR scene target.
//
// # Responsibilities
//
// - Samples the HDR target at matching physical pixel coordinates.
// - Applies exposure, an ACES-style fitted curve, and optional sRGB encoding.
//
// # Design
//
// An sRGB output attachment performs transfer encoding itself. The host sets
// encode_srgb only for other formats, preventing double encoding. The oversized
// triangle is clipped by the host viewport/scissor, and the pass clears pixels
// outside that rectangle to transparent before this shader runs.


// =============================================================================
// HDR Input and Exposure
// =============================================================================

@group(0) @binding(0) var hdr:texture_2d<f32>;
// Matches the 16-byte tone uniform in pbr.rs; pad preserves uniform alignment.
struct Params { exposure:f32, encode_srgb:u32, pad:vec2<f32> };
@group(0) @binding(1) var<uniform> params:Params;

// =============================================================================
// Fullscreen Tonemap
// =============================================================================

@vertex fn vs_main(@builtin(vertex_index) i:u32)->@builtin(position) vec4<f32> {
 let x=f32((i<<1u)&2u);let y=f32(i&2u);return vec4(x*2.0-1.0,1.0-y*2.0,0.0,1.0);
}
@fragment fn fs_main(@builtin(position) p:vec4<f32>)->@location(0) vec4<f32> {
 // Step 1: load the same physical pixel from the HDR target and apply exposure.
 let c=textureLoad(hdr,vec2<i32>(p.xy),0).rgb*params.exposure;
 // Step 2: compress HDR radiance into display range with the fitted ACES curve.
 var mapped=clamp((c*(2.51*c+vec3(0.03)))/(c*(2.43*c+vec3(0.59))+vec3(0.14)),vec3(0.0),vec3(1.0));
 // Step 3: encode only when the output attachment does not perform sRGB conversion.
 if params.encode_srgb!=0u { mapped=select(12.92*mapped,1.055*pow(mapped,vec3(1.0/2.4))-vec3(0.055),mapped>vec3(0.0031308)); }
 return vec4(mapped,1.0);
}
