//! Renderer-owned uploads for a decoded CPU asset snapshot.
//!
//! # Responsibilities
//!
//! - Creates indexed mesh buffers and material texture bind groups.
//! - Uploads the four environment-lighting textures with neutral fallbacks.
//! - Retains the accepted CPU generation alongside its GPU representation.
//!
//! # Design
//!
//! The PBR pass replaces this cache when the asset revision or environment
//! selection changes. Bind groups retain their texture views and samplers, while
//! the CPU snapshot keeps material factors consistent with the uploaded maps.
//! Native handles never enter shared ECS components or persistence payloads.

// Current crate
use crate::assets::{Material, RenderAssets, TextureData};

// Standard library
use std::collections::BTreeMap;

// External crates
use wgpu::util::DeviceExt;

// =============================================================================
// Uploaded Asset Types
// =============================================================================

/// GPU buffers for a nonempty indexed triangle mesh.
pub struct Mesh {
    /// Interleaved position, normal, UV, tangent, and bitangent vertex stream.
    pub vertices: wgpu::Buffer,
    /// Triangle indices uploaded as unsigned 32-bit values.
    pub indices: wgpu::Buffer,
    /// Number of indices used by an indexed draw.
    pub count: u32,
}

/// Complete upload cache for one accepted CPU snapshot and environment selection.
pub struct GpuAssets {
    /// Accepted CPU values; retained even if a newer generation exceeds device limits.
    pub cpu: std::sync::Arc<RenderAssets>,
    /// Uploaded meshes indexed by stable logical asset ID.
    pub meshes: BTreeMap<u64, Mesh>,
    /// Material bindings, including the always-present default at ID zero.
    pub materials: BTreeMap<u64, wgpu::BindGroup>,
    /// Background, diffuse IBL, specular IBL, BRDF LUT, and their sampler.
    pub environment: wgpu::BindGroup,
    /// CPU revision represented by these uploads.
    pub revision: u64,
    /// Selected environment texture IDs in shader binding order.
    pub environment_ids: [u64; 4],
}

// =============================================================================
// Shader Binding Layouts
// =============================================================================

/// Declare five sampled maps, a filtering sampler, and material parameters.
///
/// Binding order must match group 1 in the PBR shader.
pub fn material_layout(d: &wgpu::Device) -> wgpu::BindGroupLayout {
    let mut entries = Vec::new();
    for binding in 0..5 {
        entries.push(wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        });
    }
    entries.push(wgpu::BindGroupLayoutEntry {
        binding: 5,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
    });
    entries.push(wgpu::BindGroupLayoutEntry {
        binding: 6,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    });
    d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("PBR material"),
        entries: &entries,
    })
}

/// Declare four sampled environment maps and their shared filtering sampler.
///
/// Binding order must match group 2 in the PBR shader.
pub fn environment_layout(d: &wgpu::Device) -> wgpu::BindGroupLayout {
    let mut entries = Vec::new();
    for binding in 0..4 {
        entries.push(wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        });
    }
    entries.push(wgpu::BindGroupLayoutEntry {
        binding: 4,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
    });
    d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("PBR environment"),
        entries: &entries,
    })
}

// =============================================================================
// Texture Upload
// =============================================================================

/// Upload a decoded texture or a one-pixel neutral fallback.
///
/// Material role selects sRGB interpretation. Legacy RGBA8 images receive mips;
/// existing mips are regenerated if their color-space tag conflicts with the
/// role. Float payloads are converted from f32 to finite-range f16 for the GPU.
fn upload(
    d: &wgpu::Device,
    q: &wgpu::Queue,
    data: Option<&TextureData>,
    fallback: [u8; 4],
    srgb: bool,
) -> wgpu::TextureView {
    let fallback = TextureData {
        width: 1,
        height: 1,
        float: false,
        srgb: None,
        mips: vec![fallback.to_vec()],
    };
    let data = data.unwrap_or(&fallback);
    let generated;
    let data = if !data.float
        && (data.mips.len() == 1 || data.srgb.is_some_and(|tag| tag != srgb))
        && (data.width > 1 || data.height > 1)
    {
        generated = crate::assets::generate_mips(data, srgb);
        &generated
    } else {
        data
    };
    let format = if data.float {
        wgpu::TextureFormat::Rgba16Float
    } else if srgb {
        wgpu::TextureFormat::Rgba8UnormSrgb
    } else {
        wgpu::TextureFormat::Rgba8Unorm
    };
    let texture = d.create_texture(&wgpu::TextureDescriptor {
        label: Some("cooked texture"),
        size: wgpu::Extent3d {
            width: data.width,
            height: data.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: data.mips.len() as u32,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    for (mip, bytes) in data.mips.iter().enumerate() {
        let width = (data.width >> mip).max(1);
        let height = (data.height >> mip).max(1);
        let converted;
        let bytes = if data.float {
            converted = bytes
                .chunks_exact(4)
                .flat_map(|b| {
                    half::f16::from_f32(
                        f32::from_le_bytes(b.try_into().unwrap()).clamp(-65504.0, 65504.0),
                    )
                    .to_le_bytes()
                })
                .collect::<Vec<_>>();
            &converted
        } else {
            bytes
        };
        q.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: mip as u32,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * if data.float { 8 } else { 4 }),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
    }
    texture.create_view(&Default::default())
}

// =============================================================================
// Cache Construction
// =============================================================================

impl GpuAssets {
    /// Build a cache using assets already checked against device limits.
    ///
    /// Missing texture IDs use neutral maps so materials remain drawable while
    /// content is incomplete. Mesh ID zero is handled by the pass's built-in sphere.
    pub fn new(
        d: &wgpu::Device,
        q: &wgpu::Queue,
        assets: &std::sync::Arc<RenderAssets>,
        ml: &wgpu::BindGroupLayout,
        el: &wgpu::BindGroupLayout,
        environment_ids: [u64; 4],
    ) -> Self {
        let sampler = d.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        // Step 1: reorder cooked vertices to match the GPU vertex attribute layout.
        let mut meshes = BTreeMap::new();
        for (&id, mesh) in assets.meshes.iter() {
            let vertices: Vec<f32> = mesh
                .vertices
                .iter()
                .flat_map(|v| {
                    [
                        v[0], v[1], v[2], v[5], v[6], v[7], v[3], v[4], v[8], v[9], v[10], v[11],
                        v[12], v[13],
                    ]
                })
                .collect();
            if vertices.is_empty() || mesh.indices.is_empty() {
                continue;
            }
            meshes.insert(
                id,
                Mesh {
                    vertices: d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("cooked mesh"),
                        contents: bytemuck::cast_slice(&vertices),
                        usage: wgpu::BufferUsages::VERTEX,
                    }),
                    indices: d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("cooked indices"),
                        contents: bytemuck::cast_slice(&mesh.indices),
                        usage: wgpu::BufferUsages::INDEX,
                    }),
                    count: mesh.indices.len() as u32,
                },
            );
        }
        // Step 2: create a complete material binding even when individual maps are absent.
        let default = Material::default();
        let mut materials = BTreeMap::new();
        for (&id, m) in std::iter::once((&0, &default)).chain(assets.materials.iter()) {
            for id in [
                m.albedo,
                m.normal,
                m.metallic_roughness,
                m.occlusion,
                m.emissive_map,
            ] {
                if id != 0 && assets.textures.get(&id).is_none() {
                    eprintln!("[render] missing texture {id}; using a neutral map");
                }
            }
            let views = [
                upload(d, q, assets.textures.get(&m.albedo), [255; 4], true),
                upload(
                    d,
                    q,
                    assets.textures.get(&m.normal),
                    [128, 128, 255, 255],
                    false,
                ),
                upload(
                    d,
                    q,
                    assets.textures.get(&m.metallic_roughness),
                    [255; 4],
                    false,
                ),
                upload(d, q, assets.textures.get(&m.occlusion), [255; 4], false),
                upload(d, q, assets.textures.get(&m.emissive_map), [255; 4], true),
            ];
            let params = [m.emissive[0], m.emissive[1], m.emissive[2], m.alpha_cutoff];
            let uniform = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: None,
                contents: bytemuck::cast_slice(&params),
                usage: wgpu::BufferUsages::UNIFORM,
            });
            let mut entries: Vec<_> = views
                .iter()
                .enumerate()
                .map(|(i, v)| wgpu::BindGroupEntry {
                    binding: i as u32,
                    resource: wgpu::BindingResource::TextureView(v),
                })
                .collect();
            entries.push(wgpu::BindGroupEntry {
                binding: 5,
                resource: wgpu::BindingResource::Sampler(&sampler),
            });
            entries.push(wgpu::BindGroupEntry {
                binding: 6,
                resource: uniform.as_entire_binding(),
            });
            materials.insert(
                id,
                d.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("material"),
                    layout: ml,
                    entries: &entries,
                }),
            );
        }
        // Step 3: bind the selected lighting generation with usable fallback maps.
        let views = [
            upload(
                d,
                q,
                assets.textures.get(&environment_ids[0]),
                [45, 55, 70, 255],
                true,
            ),
            upload(
                d,
                q,
                assets.textures.get(&environment_ids[1]),
                [45, 55, 70, 255],
                true,
            ),
            upload(
                d,
                q,
                assets.textures.get(&environment_ids[2]),
                [45, 55, 70, 255],
                true,
            ),
            upload(
                d,
                q,
                assets.textures.get(&environment_ids[3]),
                [200, 10, 0, 255],
                false,
            ),
        ];
        let mut entries: Vec<_> = views
            .iter()
            .enumerate()
            .map(|(i, v)| wgpu::BindGroupEntry {
                binding: i as u32,
                resource: wgpu::BindingResource::TextureView(v),
            })
            .collect();
        entries.push(wgpu::BindGroupEntry {
            binding: 4,
            resource: wgpu::BindingResource::Sampler(&sampler),
        });
        let environment = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("IBL"),
            layout: el,
            entries: &entries,
        });
        Self {
            cpu: assets.clone(),
            meshes,
            materials,
            environment,
            revision: assets.revision,
            environment_ids,
        }
    }
}
