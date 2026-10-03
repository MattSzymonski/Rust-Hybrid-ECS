//! The GPU side of one pass of the chain.
//!
//! # Responsibilities
//!
//! - Hold what recording a fullscreen pass needs: its pipeline, the bind groups
//!   for the values the pass carries itself, and where it writes.
//! - Keep those values on a material's packing, so a pass and a material are the
//!   same thing to a shader and the shader layouts are shared rather than
//!   duplicated.
//!
//! # Design
//!
//! Built when the chain changes, not per frame: a pass changes when the game
//! swaps its pipeline, not when the scene moves. Geometry passes are not here -
//! they draw instances through the mesh path, with the pipeline their shader
//! already has. What is built here is the fullscreen pass: three vertices, no
//! vertex buffers, no depth test, no instance data, reading what an earlier pass
//! wrote. That is the shape every post-processing step takes, and it is the
//! reason a pass can carry its own parameters and inputs at all.
//!
//! A skybox pass is the same triangle with one difference: it is tested against
//! the depth the geometry passes wrote (less-or-equal, without writing), and its
//! shader puts the triangle at the far plane, so it draws only where no mesh
//! did.

use std::collections::{HashMap, HashSet};

use crate::{
    assets::Shader,
    error::{capturing_validation, ErrorContext, RendererError, Result},
    frame::{CullMode, PassKind, ResolvedPass},
    resources::{
        check_slot_shape, RendererMaterial, RendererMesh, RendererResourceStorage, RendererShader,
        RendererTexture, RendererTextureHandle, Vertex,
    },
    Instance,
};

/// The input name that means the renderer's own depth buffer rather than an
/// offscreen colour target.
pub const DEPTH_INPUT: &str = "depth";

/// A pass the renderer records on its own, without instances.
pub struct RendererPass {
    pub name: String,
    pub pipeline: wgpu::RenderPipeline,
    /// Whether the pass's shader reads the engine's parameters.
    pub pass_engine_parameters: bool,
    /// Whether the pass's shader reads the camera's parameters.
    pub pass_camera_parameters: bool,
    pub parameters_bind_group: Option<wgpu::BindGroup>,
    pub textures_bind_group: Option<wgpu::BindGroup>,
}

impl RendererPass {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        storage: &RendererResourceStorage,
        engine_bind_group_layout: &wgpu::BindGroupLayout,
        camera_bind_group_layout: &wgpu::BindGroupLayout,
        pass: &ResolvedPass,
        shader: &Shader,
        renderer_shader: &RendererShader,
        target_formats: &[wgpu::TextureFormat],
        depth_format: wgpu::TextureFormat,
        targets: &HashMap<String, RendererTexture>,
        depth: &RendererTexture,
        textures: &[(String, RendererTextureHandle)],
        defined_targets: &HashSet<String>,
    ) -> Result<Self> {
        let vertex_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pass_vertex_shader"),
            source: wgpu::ShaderSource::Wgsl(shader.vertex_wgsl.as_str().into()),
        });
        let fragment_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pass_fragment_shader"),
            source: wgpu::ShaderSource::Wgsl(shader.fragment_wgsl.as_str().into()),
        });

        // A fullscreen pass that declares no parameters, or no textures, still
        // has to leave the group its neighbours occupy: a shader puts its
        // parameters at group 2 and its inputs at group 3 because that is where
        // every other shader in the engine puts them, so the layouts are padded
        // here rather than the shader being asked to move.
        let empty_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("pass_empty_bind_group_layout"),
            entries: &[],
        });
        let parameters_layout = renderer_shader
            .parameters_bind_group_layout
            .as_ref()
            .unwrap_or(&empty_layout);
        let textures_layout = renderer_shader
            .textures_bind_group_layout
            .as_ref()
            .unwrap_or(&empty_layout);

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some(&format!("{}_pipeline_layout", pass.name)),
            bind_group_layouts: &[
                engine_bind_group_layout,
                camera_bind_group_layout,
                parameters_layout,
                textures_layout,
            ],
            push_constant_ranges: &[],
        });

        // The pipeline is built for the target the pass writes, not for the
        // surface: a lit frame lands in a half-float target and the pass that
        // tonemaps it lands on the swapchain, and one pipeline cannot serve
        // both. The vertex layout follows the pass's kind for the same reason -
        // instances for geometry, nothing at all for a fullscreen triangle,
        // which carries its corners in `SV_VertexID`.
        let vertex_layouts: Vec<wgpu::VertexBufferLayout> = match pass.kind {
            PassKind::Geometry => vec![
                RendererMesh::data_layout_descriptor(),
                Instance::data_layout_descriptor(),
            ],
            PassKind::Fullscreen | PassKind::Skybox => Vec::new(),
        };
        let (cull_mode, depth_stencil) = match pass.kind {
            PassKind::Geometry => (
                match pass.cull {
                    CullMode::Back => Some(wgpu::Face::Back),
                    CullMode::Front => Some(wgpu::Face::Front),
                    CullMode::None => None,
                },
                Some(wgpu::DepthStencilState {
                    format: depth_format,
                    // A translucent pass reads depth and does not write it, so
                    // what is behind it still passes the test a moment later.
                    depth_write_enabled: pass.depth_write,
                    depth_compare: wgpu::CompareFunction::Less,
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState::default(),
                }),
            ),
            // No depth: a fullscreen pass overwrites every pixel it covers, and
            // the depth another pass left describes geometry that is not what
            // this triangle is.
            PassKind::Fullscreen => (None, None),
            // Tested, never written: the triangle sits at the far plane (depth
            // 1.0), and less-or-equal lets it through exactly where the depth
            // buffer still holds the 1.0 it was cleared to - where no mesh drew.
            PassKind::Skybox => (
                None,
                Some(wgpu::DepthStencilState {
                    format: depth_format,
                    depth_write_enabled: false,
                    depth_compare: wgpu::CompareFunction::LessEqual,
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState::default(),
                }),
            ),
        };

        // Captured rather than left to wgpu's uncaptured-error handler: a
        // pipeline the driver refuses - a shader whose entry points do not
        // match the pass's kind, say - has to come back as the reason this pass
        // is not drawn.
        let color_targets: Vec<Option<wgpu::ColorTargetState>> = target_formats
            .iter()
            .map(|format| {
                Some(wgpu::ColorTargetState {
                    format: *format,
                    blend: Some(if pass.blend {
                        wgpu::BlendState::ALPHA_BLENDING
                    } else {
                        wgpu::BlendState::REPLACE
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })
            })
            .collect();

        let pipeline = capturing_validation(device, || {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(&format!("{}_pipeline", pass.name)),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &vertex_module,
                    entry_point: Some("vs_main"),
                    buffers: &vertex_layouts,
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &fragment_module,
                    entry_point: Some("fs_main"),
                    targets: &color_targets,
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    strip_index_format: None,
                    front_face: wgpu::FrontFace::Ccw,
                    cull_mode,
                    polygon_mode: wgpu::PolygonMode::Fill,
                    conservative: false,
                    unclipped_depth: false,
                },
                depth_stencil,
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
                cache: None,
            })
        })
        .map_err(|detail| RendererError::Other { detail })?;

        let parameters_bind_group = if renderer_shader.parameter_slots.is_empty() {
            None
        } else {
            let size = RendererMaterial::calculate_uniform_size(&renderer_shader.parameter_slots);
            // Not stored: a bind group holds what it references, so the buffer
            // lives as long as the group that reads it and a second handle here
            // would only be a way to forget which one is real.
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(&format!("{}_pass_parameters_buffer", pass.name)),
                size: size as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            RendererMaterial::write_parameters_to_buffer(
                queue,
                &buffer,
                &renderer_shader.parameter_slots,
                &pass.parameters,
            )?;
            // The layout is looked up outside the closure: `?` belongs to this
            // function, which returns a `Result`, not to a block that only has a
            // bind group to hand back.
            let layout = renderer_shader
                .parameters_bind_group_layout
                .as_ref()
                .context("a shader with parameter slots has a parameters layout")?;
            let bind_group = capturing_validation(device, || {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some(&format!("{}_pass_parameters_bind_group", pass.name)),
                    layout,
                    entries: &[wgpu::BindGroupEntry {
                        binding: 0,
                        resource: buffer.as_entire_binding(),
                    }],
                })
            })
            .map_err(|detail| RendererError::Other { detail })?;
            Some(bind_group)
        };

        let textures_bind_group = if renderer_shader.texture_slots.is_empty() {
            None
        } else {
            let mut entries = Vec::new();
            for (slot_name, slot) in &renderer_shader.texture_slots {
                let (view, sampler) = match pass.inputs.get(slot_name) {
                    // An input is the frame an earlier pass wrote, so a name no
                    // pass produces is a chain that cannot draw: failing here
                    // names the pass and the target instead of binding the
                    // default texture and quietly showing a flat frame.
                    Some(target) => {
                        // Depth is not an offscreen colour target, but a pass
                        // names it the same way: as something an earlier pass
                        // left behind. It reads through the depth buffer's own
                        // sampler, which is a plain read sampler rather than a
                        // comparison one.
                        let (view, sampler) = if target == DEPTH_INPUT {
                            (&depth.texture_view, &depth.sampler)
                        } else {
                            // The target map holds every offscreen target the
                            // chain declares, so the map alone cannot tell an
                            // earlier pass's output from this pass's own or a
                            // later pass's: `defined_targets` is the chain
                            // walked so far, and reading outside it would bind
                            // an uninitialised texture.
                            if !defined_targets.contains(target.as_str()) {
                                return Err(RendererError::Other {
                                    detail: format!(
                                        "pass {} reads `{target}`, which no earlier pass writes",
                                        pass.name
                                    ),
                                });
                            }
                            let texture = targets.get(target).context(format!(
                                "pass {} reads `{target}`, which no earlier pass writes",
                                pass.name
                            ))?;
                            check_slot_shape(
                                &format!("pass {}", pass.name),
                                slot_name,
                                slot.texture_type,
                                texture,
                            )?;
                            (&texture.texture_view, &texture.sampler)
                        };
                        (view, sampler)
                    }
                    // Otherwise the pass's own texture for the slot, if it
                    // bound one: a committed asset, resolved by the same key a
                    // material resolves its own with.
                    None => match textures.iter().find(|(name, _)| name == slot_name) {
                        Some((_, handle)) => {
                            let texture = storage.textures.get(*handle).context(format!(
                                "pass {} binds `{slot_name}` to a texture that is not loaded",
                                pass.name
                            ))?;
                            check_slot_shape(
                                &format!("pass {}", pass.name),
                                slot_name,
                                slot.texture_type,
                                texture,
                            )?;
                            (&texture.texture_view, &texture.sampler)
                        }
                        // Neither: the shader declared the slot and asked for
                        // nothing in particular, so it gets the renderer's
                        // stand-in for that kind of map. A depth slot names no
                        // input and no texture, so there is nothing to read:
                        // the renderer's own buffer is not a stand-in for a
                        // colour map, and only the pass's own `inputs` can say
                        // which depth it meant.
                        None => {
                            let handle = storage.default_texture_for(slot.texture_type).ok_or_else(|| {
                                RendererError::Other {
                                    detail: format!(
                                        "pass {} declares texture slot `{slot_name}` as depth but names no input for it",
                                        pass.name
                                    ),
                                }
                            })?;
                            let texture = storage
                                .textures
                                .get(handle)
                                .context("the renderer's default textures exist")?;
                            (&texture.texture_view, &texture.sampler)
                        }
                    },
                };
                entries.push(wgpu::BindGroupEntry {
                    binding: slot.texture_binding,
                    resource: wgpu::BindingResource::TextureView(view),
                });
                entries.push(wgpu::BindGroupEntry {
                    binding: slot.sampler_binding,
                    resource: wgpu::BindingResource::Sampler(sampler),
                });
            }
            // A name the shader has no slot for is a typo or a leftover from an
            // edited shader; saying so is the difference between a pass that is
            // missing a texture and one whose author believes it bound it.
            for slot_name in pass.inputs.keys() {
                if !renderer_shader.texture_slots.contains_key(slot_name) {
                    println!(
                        "[render] Pass {} names input `{slot_name}`, which its shader has no slot for",
                        pass.name
                    );
                }
            }
            for (slot_name, _key) in textures {
                if !renderer_shader.texture_slots.contains_key(slot_name) {
                    println!(
                        "[render] Pass {} binds texture `{slot_name}`, which its shader has no slot for",
                        pass.name
                    );
                }
            }
            let layout = renderer_shader
                .textures_bind_group_layout
                .as_ref()
                .context("a shader with texture slots has a textures layout")?;
            let bind_group = capturing_validation(device, || {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some(&format!("{}_pass_textures_bind_group", pass.name)),
                    layout,
                    entries: &entries,
                })
            })
            .map_err(|detail| RendererError::Other { detail })?;
            Some(bind_group)
        };

        Ok(Self {
            name: pass.name.clone(),
            pipeline,
            pass_engine_parameters: renderer_shader.pass_engine_parameters,
            pass_camera_parameters: renderer_shader.pass_camera_parameters,
            parameters_bind_group,
            textures_bind_group,
        })
    }
}
