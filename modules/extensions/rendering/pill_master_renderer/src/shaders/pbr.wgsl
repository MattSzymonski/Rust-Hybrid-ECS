// Metallic-roughness GGX shading with linear HDR output.
//
// # Responsibilities
//
// - Transforms indexed mesh instances and evaluates tangent-space normal maps.
// - Combines directional lighting, split-sum IBL, occlusion, and emission.
// - Draws an equirectangular environment behind the scene.
//
// # Design
//
// Bindings mirror gpu_assets.rs and the uniform/vertex layouts in pbr.rs.
// Matrices are column-major; the camera is right-handed and looks along -Z.
// Color textures are already sRGB-decoded by their GPU views. This shader emits
// linear radiance; exposure and display conversion belong to tonemap.wgsl.


// =============================================================================
// Frame and Material Bindings
// =============================================================================

struct Frame { vp:mat4x4<f32>, eye:vec4<f32>, light_dir:vec4<f32>, light_color:vec4<f32>, inverse_vp:mat4x4<f32> };
@group(0) @binding(0) var<uniform> frame:Frame;
@group(1) @binding(0) var albedo:texture_2d<f32>;
@group(1) @binding(1) var normal_map:texture_2d<f32>;
@group(1) @binding(2) var mr:texture_2d<f32>;
@group(1) @binding(3) var ao:texture_2d<f32>;
@group(1) @binding(4) var emissive:texture_2d<f32>;
@group(1) @binding(5) var material_sampler:sampler;
// RGB contains the emission factor; W contains the alpha-mask cutoff.
@group(1) @binding(6) var<uniform> material_params:vec4<f32>;
@group(2) @binding(0) var environment:texture_2d<f32>;
@group(2) @binding(1) var diffuse_ibl:texture_2d<f32>;
@group(2) @binding(2) var specular_ibl:texture_2d<f32>;
@group(2) @binding(3) var brdf_lut:texture_2d<f32>;
@group(2) @binding(4) var environment_sampler:sampler;
// Match the cooker: +Y is the north pole and longitude wraps around the panorama.
fn env_uv(n:vec3<f32>)->vec2<f32> {return vec2(atan2(n.z,n.x)/6.2831853+0.5,acos(clamp(n.y,-1.0,1.0))/3.14159265);}

// =============================================================================
// Vertex Transformation
// =============================================================================

// Locations 3 through 8 advance per instance; the other attributes advance per vertex.
struct VertexIn {
 @location(0) position:vec3<f32>, @location(1) normal:vec3<f32>, @location(2) uv:vec2<f32>,
 @location(9) tangent:vec3<f32>, @location(10) bitangent:vec3<f32>,
 @location(3) m0:vec4<f32>, @location(4) m1:vec4<f32>, @location(5) m2:vec4<f32>, @location(6) m3:vec4<f32>,
 @location(7) color:vec4<f32>, @location(8) params:vec4<f32>
};
struct VertexOut {
 @builtin(position) clip:vec4<f32>, @location(0) position:vec3<f32>, @location(1) normal:vec3<f32>,
 @location(2) color:vec4<f32>, @location(3) params:vec4<f32>, @location(4) uv:vec2<f32>, @location(5) tangent:vec3<f32>, @location(6) bitangent:vec3<f32>
};
@vertex fn vs_main(v:VertexIn)->VertexOut {
 let model=mat4x4<f32>(v.m0,v.m1,v.m2,v.m3); let p=model*vec4(v.position,1.0);
 // Inverse transpose for orthogonal TRS columns, including nonuniform scale.
 let normal_matrix=mat3x3<f32>(v.m0.xyz/max(dot(v.m0.xyz,v.m0.xyz),0.00001),v.m1.xyz/max(dot(v.m1.xyz,v.m1.xyz),0.00001),v.m2.xyz/max(dot(v.m2.xyz,v.m2.xyz),0.00001));
 var o:VertexOut; o.clip=frame.vp*p; o.position=p.xyz; o.normal=normalize(normal_matrix*v.normal);o.color=v.color;o.params=v.params;o.uv=v.uv;o.tangent=normalize((model*vec4(v.tangent,0.0)).xyz);o.bitangent=normalize((model*vec4(v.bitangent,0.0)).xyz);return o;
}

// =============================================================================
// PBR Surface Shading
// =============================================================================

const PI:f32=3.14159265;
@fragment fn fs_main(i:VertexOut)->@location(0) vec4<f32> {
 // Step 1: sample material factors and discard masked fragments before shading.
 let tex=textureSample(albedo,material_sampler,i.uv);
 if i.color.a*tex.a<material_params.w { discard; }
 let base=max(i.color.rgb*tex.rgb,vec3(0.0)); let metallic=clamp(i.params.x*textureSample(mr,material_sampler,i.uv).b,0.0,1.0); let rough=clamp(i.params.y*textureSample(mr,material_sampler,i.uv).g,0.045,1.0);
 // Step 2: transform the sampled tangent-space normal into world space.
 let t=normalize(i.tangent);let bn=normalize(i.bitangent);let n=normalize(mat3x3<f32>(t,bn,normalize(i.normal))*(textureSample(normal_map,material_sampler,i.uv).xyz*2.0-vec3(1.0)));let v=normalize(frame.eye.xyz-i.position);let l=normalize(-frame.light_dir.xyz);let h=normalize(l+v);
 let nl=max(dot(n,l),0.0);let nv=max(dot(n,v),0.0001);let nh=max(dot(n,h),0.0);let vh=max(dot(v,h),0.0);
 // Step 3: evaluate GGX distribution, Smith masking, and Schlick Fresnel.
 let a=rough*rough;let a2=a*a;let denom=nh*nh*(a2-1.0)+1.0;let d=a2/(PI*denom*denom);
 let k=(rough+1.0)*(rough+1.0)/8.0;let g=(nv/(nv*(1.0-k)+k))*(nl/(nl*(1.0-k)+k));
 // Dielectrics use 4% normal-incidence reflectance; metals use their base color.
 let f0=mix(vec3(0.04),base,metallic);let f=f0+(vec3(1.0)-f0)*pow(1.0-vh,5.0);
 let diffuse=(vec3(1.0)-f)*(1.0-metallic)*base/PI;let spec=d*g*f/max(4.0*nv*nl,0.0001);
 // Step 4: add diffuse irradiance and roughness-prefiltered split-sum reflections.
 let irradiance=textureSample(diffuse_ibl,environment_sampler,env_uv(n)).rgb;
 let reflection=reflect(-v,n);
 let prefiltered=textureSampleLevel(specular_ibl,environment_sampler,env_uv(reflection),rough*f32(textureNumLevels(specular_ibl)-1u)).rgb;
 let brdf=textureSample(brdf_lut,environment_sampler,vec2(nv,rough)).rg;
 let ambient=((vec3(1.0)-f0)*(1.0-metallic)*irradiance*base/PI+prefiltered*(f0*brdf.x+vec3(brdf.y)))*textureSample(ao,material_sampler,i.uv).r;
 // Step 5: add emission in linear space; ambient occlusion affects only the IBL term.
 let emission=textureSample(emissive,material_sampler,i.uv).rgb*material_params.xyz;
 return vec4((diffuse+spec)*frame.light_color.rgb*nl+ambient+emission,1.0);
}


// =============================================================================
// Environment Background
// =============================================================================

struct Background { @builtin(position) clip:vec4<f32>, @location(0) ndc:vec2<f32> };
// Generate one oversized fullscreen triangle without a vertex buffer.
@vertex fn vs_background(@builtin(vertex_index) i:u32)->Background {
 let p=vec2(f32((i<<1u)&2u)*2.0-1.0,1.0-f32(i&2u)*2.0);var o:Background;o.clip=vec4(p,1.0,1.0);o.ndc=p;return o;
}
// Unproject a far-plane point and sample the direction from the camera toward it.
@fragment fn fs_background(i:Background)->@location(0) vec4<f32> {
 let world=frame.inverse_vp*vec4(i.ndc,1.0,1.0);let ray=normalize(world.xyz/world.w-frame.eye.xyz);
 return vec4(textureSample(environment,environment_sampler,env_uv(ray)).rgb,1.0);
}
