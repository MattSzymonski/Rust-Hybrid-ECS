//! The GPU pipeline a shader asset becomes, and the layouts its draws bind.
//!
//! # Responsibilities
//!
//! - Build the render pipeline and group layouts from a shader's cooked WGSL,
//!   capturing wgpu validation so a shader the driver refuses is reported
//!   against the asset that named it instead of taking the host down
//!   ([`RendererShader::new`]).
//! - Create the parameters group layout (set 2) and the textures group layout
//!   (set 3), each absent when the shader declares none of that kind, and pad
//!   the pipeline layout so every shader keeps the engine's fixed groups.
//! - Carry the flags that say whether the shader reads the engine's or the
//!   camera's uniforms, so a drawer binds those groups only when they are
//!   read.
//!
//! # Design
//!
//! Built during asset sync, when a shader asset is loaded or its version
//! changes, not per frame. The four group slots are fixed by the engine's
//! convention - engine parameters at 0, camera at 1, a material's or pass's
//! parameters at 2, its textures at 3 - and an empty layout keeps a slot the
//! shader does not declare, so every pipeline has the same shape the drawers
//! bind against. The WGSL the modules are created from is cooked at build
//! time; nothing compiles a shader at runtime.

// External crates
use indexmap::IndexMap;
use pill_core::{debug, PillStyle};

// Current crate
use crate::{
    assets::{ShaderParameterSlot, ShaderTextureSlot, TextureType},
    error::{RendererError, Result},
};

// --- Handle ---

pill_core::define_slot_key!(RendererShaderHandle);

/// One shader's GPU pipeline and the layout of the groups its draws bind.
///
/// Built during asset sync from the shader asset, beside every other GPU
/// resource. The pipeline is what a draw switches to; the layouts are what a
/// material or pass built against this shader binds its parameters and
/// textures through.
pub struct RendererShader {
    /// Label used in logs and error messages, taken from the shader asset.
    pub name: String,
    /// Pipeline a draw using this shader binds.
    ///
    /// A pass that owns its pipeline can still draw through its own instead,
    /// because only the pass knows the target's format and its depth and
    /// culling.
    pub render_pipeline: wgpu::RenderPipeline,

    /// The uniforms the shader declares, by name, in declaration order.
    pub parameter_slots: IndexMap<String, ShaderParameterSlot>,
    /// Layout of the parameters group (set 2), or `None` when the shader
    /// declares no parameters.
    pub parameters_bind_group_layout: Option<wgpu::BindGroupLayout>,

    /// The textures the shader declares, by name, each with its bindings.
    pub texture_slots: IndexMap<String, ShaderTextureSlot>,
    /// Layout of the textures group (set 3), or `None` when the shader
    /// declares no textures.
    pub textures_bind_group_layout: Option<wgpu::BindGroupLayout>,

    /// Whether the shader reads the engine's parameters, in which case a
    /// drawer binds that group before the draw.
    pub pass_engine_parameters: bool,
    /// Whether the shader reads the camera's parameters, in which case a
    /// drawer binds that group before the draw.
    pub pass_camera_parameters: bool,
}

impl RendererShader {
    /// Builds a shader's render pipeline and group layouts from its cooked
    /// WGSL.
    ///
    /// The vertex and fragment modules are created from the WGSL the build
    /// cooked from the authored HLSL, and the pipeline is built for the given
    /// color and depth formats. The parameters and textures layouts exist only
    /// when the shader declares slots of that kind; the pipeline layout pads a
    /// missing one with an empty layout, so the shader keeps the engine's
    /// fixed group convention either way.
    ///
    /// # Errors
    ///
    /// Returns [`RendererError::Other`] when the pipeline fails wgpu
    /// validation - a mistake in the shader asset. Validation is scoped so
    /// the refusal is reported against the shader instead of panicking
    /// through wgpu's uncaptured-error handler.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: &str,
        device: &wgpu::Device,
        color_format: wgpu::TextureFormat,
        depth_format: Option<wgpu::TextureFormat>,
        vertex_layouts: &[wgpu::VertexBufferLayout],
        vertex_wgsl: &str,
        fragment_wgsl: &str,
        parameter_slots: &IndexMap<String, ShaderParameterSlot>,
        texture_slots: &IndexMap<String, ShaderTextureSlot>,
        engine_bind_group_layout: &wgpu::BindGroupLayout,
        camera_bind_group_layout: &wgpu::BindGroupLayout,
        pass_engine_parameters: bool,
        pass_camera_parameters: bool,
    ) -> Result<Self> {
        // Print shader information
        {
            let mut shader_info = format!(
                "Creating shader {}:\n - Settings:\n   - Pass engine parameters: {}\n   - Pass camera parameters: {}",
                name.name_style(),
                pass_engine_parameters,
                pass_camera_parameters,
            );

            shader_info.push_str("\n - Parameter slots:");
            for (slot_name, slot) in parameter_slots {
                shader_info.push_str(&format!("\n   - {}: {:?}", slot_name, slot.parameter_type));
            }

            shader_info.push_str("\n - Texture slots:");
            for (slot_name, slot) in texture_slots {
                shader_info.push_str(&format!(
                    "\n   - {}: texture_binding={}, sampler_binding={}",
                    slot_name, slot.texture_binding, slot.sampler_binding
                ));
            }

            // Log with context
            debug!(target: pill_core::telemetry::telemetry_target::RENDERING, "{}", shader_info);
        }

        // Create shader modules from the cooked WGSL. Nothing compiles a shader
        // at runtime: the authored sources are HLSL next to the pipeline that
        // draws through them, the build script's `slangc` rule produces the WGSL
        // these strings carry, and wgpu parses that WGSL through naga like any
        // other `ShaderSource::Wgsl`.
        let vertex_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("master_vertex_shader"),
            source: wgpu::ShaderSource::Wgsl(vertex_wgsl.into()),
        });
        let fragment_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("master_fragment_shader"),
            source: wgpu::ShaderSource::Wgsl(fragment_wgsl.into()),
        });

        debug!(target: pill_core::telemetry::telemetry_target::RENDERING, "Shader modules created");

        let parameters_bind_group_layout = {
            if !parameter_slots.is_empty() {
                let bind_group_layout_entry = wgpu::BindGroupLayoutEntry {
                    binding: 0, // (set = 2, binding = 0)
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                };

                Some(
                    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                        label: Some(&format!("{}_parameters_bind_group_layout", name)),
                        entries: &[bind_group_layout_entry],
                    }),
                )
            } else {
                None
            }
        };

        debug!(target: pill_core::telemetry::telemetry_target::RENDERING, "Parameters bind group layout created");

        // Create bind group layout entries for textures - Bind group slot 3
        let textures_bind_group_layout = {
            if !texture_slots.is_empty() {
                let mut entries = Vec::new();

                for texture_slot in texture_slots.values() {
                    // A depth slot is bound as depth, with a sampler that does not
                    // filter: a depth buffer is not a filterable colour texture,
                    // and wgpu refuses the pipeline that pretends otherwise.
                    let (sample_type, sampler_type) = match texture_slot.texture_type {
                        TextureType::Depth => (
                            wgpu::TextureSampleType::Depth,
                            wgpu::SamplerBindingType::NonFiltering,
                        ),
                        TextureType::Color | TextureType::Normal => (
                            wgpu::TextureSampleType::Float { filterable: true },
                            wgpu::SamplerBindingType::Filtering,
                        ),
                    };

                    // Texture binding
                    entries.push(wgpu::BindGroupLayoutEntry {
                        binding: texture_slot.texture_binding,
                        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            multisampled: false,
                            view_dimension: wgpu::TextureViewDimension::D2,
                            sample_type,
                        },
                        count: None,
                    });

                    // Sampler binding
                    entries.push(wgpu::BindGroupLayoutEntry {
                        binding: texture_slot.sampler_binding,
                        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                        ty: wgpu::BindingType::Sampler(sampler_type),
                        count: None,
                    });
                }

                Some(
                    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                        label: Some(&format!("{}_textures_bind_group_layout", name)),
                        entries: &entries,
                    }),
                )
            } else {
                None
            }
        };

        debug!(target: pill_core::telemetry::telemetry_target::RENDERING, "Textures bind group layout created");

        // Create pipeline layout. The four group slots are fixed by the
        // engine's convention - engine parameters at 0, camera at 1, a
        // material's or pass's parameters at 2, its textures at 3 - and an
        // empty layout keeps a slot the shader does not declare, so every
        // pipeline has the same shape the drawers bind against. The pass path
        // pads its layout the same way.
        let pipeline_layout = {
            let empty_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("shader_empty_bind_group_layout"),
                entries: &[],
            });

            let bind_group_layouts = [
                engine_bind_group_layout,
                camera_bind_group_layout,
                parameters_bind_group_layout
                    .as_ref()
                    .unwrap_or(&empty_layout),
                textures_bind_group_layout.as_ref().unwrap_or(&empty_layout),
            ];

            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some(&format!("{}_pipeline_layout", name)),
                bind_group_layouts: &bind_group_layouts,
                push_constant_ranges: &[],
            })
        };

        // Create color target states that specify what color outputs wgpu should set up
        let color_target_states = &[Some(wgpu::ColorTargetState {
            format: color_format,
            blend: Some(wgpu::BlendState {
                alpha: wgpu::BlendComponent::REPLACE,
                color: wgpu::BlendComponent::REPLACE,
            }),
            write_mask: wgpu::ColorWrites::ALL,
        })];

        let render_pipeline_descriptor = wgpu::RenderPipelineDescriptor {
            label: Some(&format!("{}_render_pipeline", name)),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &vertex_shader,
                entry_point: Some("vs_main"),
                buffers: vertex_layouts, // Specifies structure of vertices that will be passed to the vertex shader
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &fragment_shader,
                entry_point: Some("fs_main"),
                targets: color_target_states,
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                // Specifies how to interpret vertices when converting them into triangles
                topology: wgpu::PrimitiveTopology::TriangleList, // Each three vertices will correspond to one triangle
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw, // Specifies how to determine whether a given triangle is facing forward or not (FrontFace::Ccw means that a triangle is facing forward if the vertices are arranged in a counter clockwise direction)
                cull_mode: Some(wgpu::Face::Back), // Triangles that are not considered facing forward are culled (not included in the render) as specified by CullMode::Back
                polygon_mode: wgpu::PolygonMode::Fill, // Setting this to anything other than Fill requires Features::NON_FILL_POLYGON_MODE
                conservative: false, // Requires Features::CONSERVATIVE_RASTERIZATION
                unclipped_depth: false,
            },
            depth_stencil: depth_format.map(|format| wgpu::DepthStencilState {
                format,
                depth_write_enabled: true,
                depth_compare: wgpu::CompareFunction::Less, // Specifies when to discard a new pixel. Using LESS means pixels will be drawn front to back
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState {
                count: 1, // Determines how many samples pipeline will use (Multisampling)
                mask: !0, // Specifies which samples should be active
                alpha_to_coverage_enabled: false,
            },
            multiview: None,
            cache: None,
        };

        // Captured rather than left to wgpu's uncaptured-error handler: a shader
        // the driver refuses is a mistake in the asset, and the pass or material
        // that named it is what the message should be attached to.
        let render_pipeline = crate::error::capturing_validation(device, || {
            device.create_render_pipeline(&render_pipeline_descriptor)
        })
        .map_err(|detail| RendererError::Other { detail })?;

        let pipeline = Self {
            name: name.to_string(),
            render_pipeline,
            parameter_slots: parameter_slots.clone(),
            textures_bind_group_layout,
            texture_slots: texture_slots.clone(),
            parameters_bind_group_layout,
            pass_engine_parameters,
            pass_camera_parameters,
        };

        debug!(target: pill_core::telemetry::telemetry_target::RENDERING, "Render pipeline created");

        Ok(pipeline)
    }
}
