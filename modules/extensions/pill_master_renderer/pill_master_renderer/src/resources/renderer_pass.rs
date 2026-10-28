//! The GPU side of one pass of the chain.
//!
//! # Responsibilities
//!
//! - Hold what recording one pass needs: its pipeline, the bind groups for the
//!   values the pass carries itself, and where it writes.
//! - Build that pipeline for whichever shape the pass takes, geometry included.
//! - Hand its values and textures to the shared bind-group builder, so a pass
//!   and a material reach a shader's slots the same way.
//!
//! # Design
//!
//! Built when the chain changes, not per frame: a pass changes when the game
//! swaps its pipeline, not when the scene moves. One type covers the three
//! shapes a pass takes, told apart by [`PassKind`]:
//!
//! - a geometry pass with a shader of its own - the mesh and instance vertex
//!   layouts, the pass's cull mode, and a depth attachment it tests and writes;
//! - a fullscreen pass - three vertices, no vertex buffers, no depth test, no
//!   instance data, reading what an earlier pass wrote. That is the shape every
//!   post-processing step takes, and it is the reason a pass can carry its own
//!   parameters and inputs at all;
//! - a skybox, which is the same triangle tested against the depth the geometry
//!   passes wrote (less-or-equal, without writing) with its shader putting the
//!   triangle at the far plane, so it draws only where no mesh did.
//!
//! A geometry pass reaches the frame through the mesh path like any other
//! drawable: the chain resolves it into a [`ResolvedPass`] and the renderer
//! records it with this pipeline, so its shader choice lives here and nowhere
//! else.

// Standard library
use std::collections::{HashMap, HashSet};

// External crates
use pill_core::warn;

// Current crate
use crate::{
    assets::Shader,
    error::{capturing_validation, ErrorContext, RendererError, Result},
    frame::{CullMode, PassKind, ResolvedPass},
    resources::{
        check_slot_shape, shader_bindings::shader_bind_groups, RendererMesh,
        RendererResourceStorage, RendererShader, RendererTexture, RendererTextureHandle, Vertex,
    },
    Instance,
};

/// The input name that means the renderer's own depth buffer rather than an
/// offscreen colour target.
pub const DEPTH_INPUT: &str = "depth";

/// One pass of the chain, in the shape its [`PassKind`] asks for: a geometry
/// pass drawing instances through its own shader, a fullscreen pass, or the
/// skybox.
pub struct RendererPass {
    pub name: String,
    pub pipeline: wgpu::RenderPipeline,
    /// Whether the pass's shader reads the engine's parameters.
    pub reads_engine_parameters: bool,
    /// Whether the pass's shader reads the camera's parameters.
    pub reads_camera_parameters: bool,
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
                Some(engine_bind_group_layout),
                Some(camera_bind_group_layout),
                Some(parameters_layout),
                Some(textures_layout),
            ],
            immediate_size: 0,
        });

        // The pipeline is built for the target the pass writes, not for the
        // surface: a lit frame lands in a half-float target and the pass that
        // tonemaps it lands on the swapchain, and one pipeline cannot serve
        // both. The vertex layout follows the pass's kind for the same reason -
        // instances for geometry, nothing at all for a fullscreen triangle,
        // which carries its corners in `SV_VertexID`.
        let vertex_layouts: Vec<wgpu::VertexBufferLayout> = match pass.settings.kind {
            PassKind::Geometry => vec![
                RendererMesh::data_layout_descriptor(),
                Instance::data_layout_descriptor(),
            ],
            PassKind::Fullscreen | PassKind::Skybox => Vec::new(),
        };
        let (cull_mode, depth_stencil) = match pass.settings.kind {
            PassKind::Geometry => (
                match pass.settings.cull {
                    CullMode::Back => Some(wgpu::Face::Back),
                    CullMode::Front => Some(wgpu::Face::Front),
                    CullMode::None => None,
                },
                Some(wgpu::DepthStencilState {
                    format: depth_format,
                    // A translucent pass reads depth and does not write it, so
                    // what is behind it still passes the test a moment later.
                    depth_write_enabled: Some(pass.settings.depth_write),
                    depth_compare: Some(wgpu::CompareFunction::Less),
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
                    depth_write_enabled: Some(false),
                    depth_compare: Some(wgpu::CompareFunction::LessEqual),
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState::default(),
                }),
            ),
        };

        // One colour target per output the pass declares, with the blend the
        // pass asked for.
        let color_targets: Vec<Option<wgpu::ColorTargetState>> = target_formats
            .iter()
            .map(|format| {
                Some(wgpu::ColorTargetState {
                    format: *format,
                    blend: Some(if pass.settings.blend {
                        wgpu::BlendState::ALPHA_BLENDING
                    } else {
                        wgpu::BlendState::REPLACE
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })
            })
            .collect();

        // Captured rather than left to wgpu's uncaptured-error handler: a
        // pipeline the driver refuses - a shader whose entry points do not
        // match the pass's kind, say - is the reason this pass is not drawn.
        let pipeline = capturing_validation(device, || {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(&format!("{}_pipeline", pass.name)),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &vertex_module,
                    entry_point: Some("vs_main"),
                    buffers: &vertex_layouts.into_iter().map(Some).collect::<Vec<_>>(),
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
                multiview_mask: None,
                cache: None,
            })
        })
        .map_err(|detail| RendererError::Other { detail })?;

        // The pass's parameter and texture groups are built by the same function
        // a material's are; all a pass adds is where an input slot reads from.
        //
        // An input is the frame an earlier pass wrote, so a name no pass
        // produces is a chain that cannot draw: failing here names the pass and
        // the target instead of binding the default texture and quietly showing
        // a flat frame.
        let (parameters_bind_group, textures_bind_group) = shader_bind_groups(
            device,
            queue,
            storage,
            &format!("pass {}", pass.name),
            renderer_shader,
            &pass.parameters,
            textures,
            |slot_name, slot| {
                let Some(target) = pass.inputs.get(slot_name) else {
                    return Ok(None);
                };
                // Depth is not an offscreen colour target, but a pass names it
                // the same way: as something an earlier pass left behind. It
                // reads through the depth buffer's own sampler, which is a plain
                // read sampler rather than a comparison one.
                if target == DEPTH_INPUT {
                    return Ok(Some((&depth.texture_view, &depth.sampler)));
                }
                // The target map holds every offscreen target the chain
                // declares, so the map alone cannot tell an earlier pass's
                // output from this pass's own or a later pass's:
                // `defined_targets` is the chain walked so far, and reading
                // outside it would bind an uninitialised texture.
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
                    slot.kind,
                    texture,
                )?;
                Ok(Some((&texture.texture_view, &texture.sampler)))
            },
        )?;

        // A name the shader has no slot for is a typo or a leftover from an
        // edited shader; saying so is the difference between a pass that is
        // missing a texture and one whose author believes it bound it.
        for slot_name in pass.inputs.keys() {
            if !renderer_shader.texture_slots.contains_key(slot_name) {
                warn!(
                    target: pill_core::telemetry::telemetry_target::RENDERING,
                    "Pass {} names input `{slot_name}`, which its shader has no slot for",
                    pass.name
                );
            }
        }
        for (slot_name, _key) in textures {
            if !renderer_shader.texture_slots.contains_key(slot_name) {
                warn!(
                    target: pill_core::telemetry::telemetry_target::RENDERING,
                    "Pass {} binds texture `{slot_name}`, which its shader has no slot for",
                    pass.name
                );
            }
        }

        Ok(Self {
            name: pass.name.clone(),
            pipeline,
            reads_engine_parameters: renderer_shader.reads_engine_parameters,
            reads_camera_parameters: renderer_shader.reads_camera_parameters,
            parameters_bind_group,
            textures_bind_group,
        })
    }
}
