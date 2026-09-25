#![allow(clippy::too_many_arguments)]

use crate::{
    api::{FrameOutcome, PillRenderer, RenderCapabilities, RenderMetrics},
    assets::{
        MaterialParameter, ShaderParameterSlot, ShaderParameterType, ShaderTextureSlot, TextureType,
    },
    component::RenderViewport,
    config::MAX_INSTANCE_PER_DRAWCALL_COUNT,
    drawers::mesh_drawer::MeshDrawer,
    error::{RendererError, Result},
    frame::{AssetSnapshot, RenderFrame},
    render_queue::{compose_render_queue_key, RenderQueueItem},
    resources::{
        RendererCamera, RendererMaterial, RendererMesh, RendererResourceStorage, RendererShader,
        RendererTexture, Vertex,
    },
    slot_map::{
        RendererCameraHandle, RendererMaterialHandle, RendererMeshHandle, RendererShaderHandle,
        RendererTextureHandle,
    },
    Instance,
};
use pill_core::{info, PillStyle};
use std::{collections::HashMap, time::Instant};

pub trait RendererWindow: wgpu::WindowHandle {}
impl<T> RendererWindow for T where T: wgpu::WindowHandle {}

pub struct Renderer {
    pub state: State,
    asset_revision: u64,
    shader_handles: HashMap<u64, RendererShaderHandle>,
    material_handles: HashMap<u64, RendererMaterialHandle>,
    texture_handles: HashMap<u64, RendererTextureHandle>,
    mesh_handles: HashMap<u64, RendererMeshHandle>,
    default_shader: RendererShaderHandle,
    default_material: RendererMaterialHandle,
    camera: RendererCameraHandle,
    viewport: Option<RenderViewport>,
    minimized: bool,
    metrics: RenderMetrics,
}

impl Renderer {
    pub fn new<W: RendererWindow + 'static>(window: W, width: u32, height: u32) -> Result<Self> {
        pollster::block_on(Self::new_async(window, width, height))
    }

    pub async fn new_async<W: RendererWindow + 'static>(
        window: W,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        info!(target: pill_core::telemetry::telemetry_target::RENDERING, "Initializing {}", "Renderer".module_object_style());
        let mut state = State::new(window, width, height).await?;
        let (default_shader, default_material) = install_default_material(&mut state)?;
        let camera = state
            .renderer_resource_storage
            .cameras
            .insert(RendererCamera::new(
                &state.device,
                state.camera_bind_group_layout.clone(),
            )?);
        Ok(Self {
            state,
            asset_revision: u64::MAX,
            shader_handles: HashMap::new(),
            material_handles: HashMap::new(),
            texture_handles: HashMap::new(),
            mesh_handles: HashMap::new(),
            default_shader,
            default_material,
            camera,
            viewport: None,
            minimized: width == 0 || height == 0,
            metrics: RenderMetrics::default(),
        })
    }

    fn sync_assets(&mut self, assets: &AssetSnapshot) -> Result<()> {
        if self.asset_revision == assets.revision {
            return Ok(());
        }
        self.state
            .renderer_resource_storage
            .clear_assets(&self.state.device, &self.state.queue)?;
        self.shader_handles.clear();
        self.material_handles.clear();
        self.texture_handles.clear();
        self.mesh_handles.clear();
        (self.default_shader, self.default_material) = install_default_material(&mut self.state)?;

        for (key, texture) in &assets.textures {
            let handle =
                self.state
                    .renderer_resource_storage
                    .textures
                    .insert(RendererTexture::new_texture(
                        &self.state.device,
                        &self.state.queue,
                        Some(&texture.name),
                        &texture.rgba,
                        texture.width,
                        texture.height,
                        texture.texture_type,
                    )?);
            self.texture_handles.insert(*key, handle);
        }
        for (key, shader) in &assets.shaders {
            let value = RendererShader::new(
                &shader.name,
                &self.state.device,
                self.state.color_format,
                Some(self.state.depth_format),
                &[
                    RendererMesh::data_layout_descriptor(),
                    Instance::data_layout_descriptor(),
                ],
                &shader.vertex_wgsl,
                &shader.fragment_wgsl,
                &shader.parameter_slots,
                &shader.texture_slots,
                &self
                    .state
                    .renderer_resource_storage
                    .engine_parameters
                    .bind_group_layout,
                &self.state.camera_bind_group_layout,
                shader.pass_engine_parameters,
                shader.pass_camera_parameters,
            )?;
            self.shader_handles.insert(
                *key,
                self.state.renderer_resource_storage.shaders.insert(value),
            );
        }
        for (key, mesh) in &assets.meshes {
            let value = RendererMesh::new(&self.state.device, &mesh.name, mesh)?;
            self.mesh_handles.insert(
                *key,
                self.state.renderer_resource_storage.meshes.insert(value),
            );
        }
        for (key, material) in &assets.materials {
            let shader_key = crate::assets::asset_key(material.shader);
            let shader = self
                .shader_handles
                .get(&shader_key)
                .copied()
                .unwrap_or(self.default_shader);
            let textures = material
                .textures
                .iter()
                .filter_map(|(slot, texture)| {
                    self.texture_handles
                        .get(&crate::assets::asset_key(texture.texture))
                        .copied()
                        .map(|handle| (slot.clone(), handle))
                })
                .collect::<Vec<_>>();
            let value = RendererMaterial::new(
                &self.state.device,
                &self.state.queue,
                &self.state.renderer_resource_storage,
                &material.name,
                shader,
                &textures,
                &material.parameters,
            )?;
            self.material_handles.insert(
                *key,
                self.state.renderer_resource_storage.materials.insert(value),
            );
        }
        self.asset_revision = assets.revision;
        Ok(())
    }
}

impl PillRenderer for Renderer {
    fn capabilities(&self) -> RenderCapabilities {
        let limits = self.state.device.limits();
        RenderCapabilities {
            max_texture_size: limits.max_texture_dimension_2d,
            max_buffer_bytes: limits.max_buffer_size,
            hdr: false,
        }
    }

    fn metrics(&self) -> RenderMetrics {
        self.metrics
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.minimized = width == 0 || height == 0;
        if !self.minimized {
            self.state
                .resize(winit::dpi::PhysicalSize::new(width, height));
        }
    }

    fn set_viewport(&mut self, viewport: Option<RenderViewport>) {
        self.viewport = viewport;
    }

    fn render(&mut self, frame: &RenderFrame) -> Result<FrameOutcome> {
        if self.minimized || !frame.has_camera {
            return Ok(FrameOutcome::Skipped);
        }
        let prepare = Instant::now();
        self.sync_assets(&frame.assets)?;
        let mut render_queue = Vec::with_capacity(frame.instances.len());
        for (index, instance) in frame.instances.iter().enumerate() {
            let Some(mesh) = self.mesh_handles.get(&instance.mesh).copied() else {
                continue;
            };
            let material = self
                .material_handles
                .get(&instance.material)
                .copied()
                .unwrap_or(self.default_material);
            let shader = self
                .state
                .renderer_resource_storage
                .materials
                .get(material)
                .map(|material| material.shader_handle)
                .unwrap_or(self.default_shader);
            let order = frame
                .assets
                .materials
                .get(&instance.material)
                .map(|material| material.rendering_order)
                .unwrap_or(u8::MAX);
            render_queue.push(RenderQueueItem {
                key: compose_render_queue_key(order, shader, material, mesh),
                entity_index: index as u32,
            });
        }
        render_queue.sort_unstable();
        self.metrics.prepare_micros = prepare.elapsed().as_micros() as u64;
        self.metrics.instance_bytes = (render_queue.len() * std::mem::size_of::<Instance>()) as u64;
        let submitted = Instant::now();
        self.state
            .render(self.camera, &render_queue, frame, self.viewport)?;
        self.metrics.submit_micros = submitted.elapsed().as_micros() as u64;
        self.metrics.draw_calls = u32::from(!render_queue.is_empty());
        Ok(FrameOutcome::Presented)
    }

    fn invalidate_assets(&mut self) {
        self.asset_revision = u64::MAX;
    }
}

fn install_default_material(
    state: &mut State,
) -> Result<(RendererShaderHandle, RendererMaterialHandle)> {
    let parameter_slots = vec![
        (
            "tint".to_owned(),
            ShaderParameterSlot::new(ShaderParameterType::Color),
        ),
        (
            "specularity".to_owned(),
            ShaderParameterSlot::new(ShaderParameterType::Scalar),
        ),
    ];
    let texture_slots = HashMap::from([
        (
            "color".to_owned(),
            ShaderTextureSlot::new(TextureType::Color, (0, 1)),
        ),
        (
            "normal".to_owned(),
            ShaderTextureSlot::new(TextureType::Normal, (2, 3)),
        ),
    ]);
    let shader = RendererShader::new(
        "pill_default_lit",
        &state.device,
        state.color_format,
        Some(state.depth_format),
        &[
            RendererMesh::data_layout_descriptor(),
            Instance::data_layout_descriptor(),
        ],
        include_str!("shaders/default_vertex.wgsl"),
        include_str!("shaders/default_lit_fragment.wgsl"),
        &parameter_slots,
        &texture_slots,
        &state
            .renderer_resource_storage
            .engine_parameters
            .bind_group_layout,
        &state.camera_bind_group_layout,
        true,
        true,
    )?;
    let shader = state.renderer_resource_storage.shaders.insert(shader);
    let parameters = HashMap::from([
        ("tint".to_owned(), MaterialParameter::Color([1.0; 3])),
        ("specularity".to_owned(), MaterialParameter::Scalar(0.5)),
    ]);
    let material = RendererMaterial::new(
        &state.device,
        &state.queue,
        &state.renderer_resource_storage,
        "pill_default_lit",
        shader,
        &[],
        &parameters,
    )?;
    let material = state.renderer_resource_storage.materials.insert(material);
    Ok((shader, material))
}

pub struct State {
    pub(crate) renderer_resource_storage: RendererResourceStorage,
    surface: wgpu::Surface<'static>,
    pub(crate) device: wgpu::Device,
    pub(crate) queue: wgpu::Queue,
    surface_configuration: wgpu::SurfaceConfiguration,
    window_size: winit::dpi::PhysicalSize<u32>,
    pub(crate) color_format: wgpu::TextureFormat,
    pub(crate) depth_format: wgpu::TextureFormat,
    depth_texture: RendererTexture,
    mesh_drawer: MeshDrawer,
    pub(crate) camera_bind_group_layout: wgpu::BindGroupLayout,
}

impl State {
    async fn new<W: RendererWindow + 'static>(window: W, width: u32, height: u32) -> Result<Self> {
        let window_size = winit::dpi::PhysicalSize::new(width.max(1), height.max(1));
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            flags: wgpu::InstanceFlags::from_build_config().with_env(),
            backend_options: wgpu::BackendOptions::default(),
        });
        let surface =
            instance
                .create_surface(window)
                .map_err(|error| RendererError::SurfaceCreation {
                    detail: error.to_string(),
                })?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::default(),
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .map_err(|error| RendererError::AdapterRequest {
                detail: error.to_string(),
            })?;
        let info = adapter.get_info();
        info!(target: pill_core::telemetry::telemetry_target::RENDERING, "Using GPU: {} ({:?})", info.name, info.backend);
        let wanted = wgpu::Features::DEPTH_CLIP_CONTROL;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("pill renderer device"),
                required_features: wanted & adapter.features(),
                required_limits: wgpu::Limits::default().using_resolution(adapter.limits()),
                memory_hints: wgpu::MemoryHints::default(),
                trace: wgpu::Trace::default(),
            })
            .await
            .map_err(|error| RendererError::DeviceCreation {
                detail: error.to_string(),
            })?;
        let capabilities = surface.get_capabilities(&adapter);
        let color_format = capabilities
            .formats
            .iter()
            .copied()
            .find(wgpu::TextureFormat::is_srgb)
            .or_else(|| capabilities.formats.first().copied())
            .ok_or(RendererError::NoTextureFormats)?;
        let alpha_mode = capabilities
            .alpha_modes
            .first()
            .copied()
            .ok_or(RendererError::NoAlphaModes)?;
        // `Fifo` is the one present mode a surface is required to support, and
        // a mode listed by `Surface::get_capabilities` is not thereby
        // creatable: the NVIDIA Vulkan driver on Windows advertises `Mailbox`
        // and then fails the flip-model swapchain with "Not enough memory
        // left", which reaches wgpu's uncaptured-error path and aborts the host
        // before a `RendererError` can be constructed. An uncapped mode stays
        // an opt-in for a driver that has been verified to create one.
        let present_mode = wgpu::PresentMode::Fifo;
        println!("[render] Present mode: {present_mode:?}");
        let mut surface_configuration = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: color_format,
            width: window_size.width,
            height: window_size.height,
            desired_maximum_frame_latency: 2,
            present_mode,
            alpha_mode,
            view_formats: vec![color_format],
        };
        configure_surface(&surface, &device, &mut surface_configuration)?;
        let depth_format = wgpu::TextureFormat::Depth32Float;
        let depth_texture =
            RendererTexture::new_depth_texture(&device, &surface_configuration, "depth_texture")?;
        let camera_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("camera_parameters_bind_group_layout"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });
        let renderer_resource_storage = RendererResourceStorage::new(&device, &queue)?;
        let mesh_drawer = MeshDrawer::new(&device, MAX_INSTANCE_PER_DRAWCALL_COUNT as u32);
        Ok(Self {
            renderer_resource_storage,
            surface,
            device,
            queue,
            surface_configuration,
            window_size,
            color_format,
            depth_format,
            depth_texture,
            mesh_drawer,
            camera_bind_group_layout,
        })
    }

    fn resize(&mut self, new_window_size: winit::dpi::PhysicalSize<u32>) {
        self.window_size = new_window_size;
        self.surface_configuration.width = new_window_size.width;
        self.surface_configuration.height = new_window_size.height;
        self.surface
            .configure(&self.device, &self.surface_configuration);
        self.depth_texture = RendererTexture::new_depth_texture(
            &self.device,
            &self.surface_configuration,
            "depth_texture",
        )
        .expect("depth texture recreation must succeed");
    }

    fn render(
        &mut self,
        camera_handle: RendererCameraHandle,
        render_queue: &[RenderQueueItem],
        frame: &RenderFrame,
        viewport: Option<RenderViewport>,
    ) -> Result<()> {
        let surface_frame = self
            .surface
            .get_current_texture()
            .map_err(|error| match error {
                wgpu::SurfaceError::Lost => RendererError::SurfaceLost,
                wgpu::SurfaceError::OutOfMemory => RendererError::SurfaceOutOfMemory,
                other => RendererError::SurfaceTextureFailed {
                    detail: other.to_string(),
                },
            })?;
        let view = surface_frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        self.renderer_resource_storage
            .engine_parameters
            .update(&self.queue, 0.0, [0.0; 3]);
        let camera = self
            .renderer_resource_storage
            .cameras
            .get_mut(camera_handle)
            .ok_or(RendererError::RendererResourceNotFound)?;
        let viewport = viewport
            .and_then(|value| {
                value.clamped_to(
                    self.surface_configuration.width,
                    self.surface_configuration.height,
                )
            })
            .unwrap_or_else(|| {
                RenderViewport::full(
                    self.surface_configuration.width,
                    self.surface_configuration.height,
                )
            });
        camera.update(
            &self.queue,
            &frame.camera,
            &frame.camera_transform,
            viewport.width as f32 / viewport.height as f32,
        );
        let camera = self
            .renderer_resource_storage
            .cameras
            .get(camera_handle)
            .ok_or(RendererError::RendererResourceNotFound)?;
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("render_encoder"),
            });
        let color_attachment = wgpu::RenderPassColorAttachment {
            view: &view,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color {
                    r: 0.15,
                    g: 0.15,
                    b: 0.15,
                    a: 1.0,
                }),
                store: wgpu::StoreOp::Store,
            },
        };
        let depth_stencil_attachment = wgpu::RenderPassDepthStencilAttachment {
            view: &self.depth_texture.texture_view,
            depth_ops: Some(wgpu::Operations {
                load: wgpu::LoadOp::Clear(1.0),
                store: wgpu::StoreOp::Store,
            }),
            stencil_ops: None,
        };
        self.mesh_drawer.record_draw_commands(
            &self.queue,
            &mut encoder,
            &self.renderer_resource_storage,
            color_attachment,
            depth_stencil_attachment,
            camera,
            render_queue,
            &frame.instances,
            viewport,
        )?;
        self.queue.submit(std::iter::once(encoder.finish()));
        surface_frame.present();
        Ok(())
    }
}

/// Configure the surface, giving up the compositing alpha mode if the driver
/// refuses it.
///
/// `Surface::configure` returns nothing: a refused configuration is reported
/// through the device's uncaptured-error path, which panics by default and
/// takes the whole host down before any frontend sees a `RendererError`. A
/// refusal is realistic because a compositing alpha mode needs a compositing
/// window, which a capability list does not promise. The requested mode is
/// therefore tried inside its own error scopes, and `Opaque` - the mode every
/// surface must support - is the fallback.
fn configure_surface(
    surface: &wgpu::Surface<'static>,
    device: &wgpu::Device,
    surface_configuration: &mut wgpu::SurfaceConfiguration,
) -> Result<()> {
    let requested_alpha_mode = surface_configuration.alpha_mode;
    let mut alpha_modes = vec![requested_alpha_mode];
    if requested_alpha_mode != wgpu::CompositeAlphaMode::Opaque {
        alpha_modes.push(wgpu::CompositeAlphaMode::Opaque);
    }

    let mut failures = Vec::new();
    for alpha_mode in alpha_modes {
        surface_configuration.alpha_mode = alpha_mode;

        // One scope per filter class, so a refusal is captured here rather than
        // reaching the uncaptured-error handler.
        device.push_error_scope(wgpu::ErrorFilter::Validation);
        device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        device.push_error_scope(wgpu::ErrorFilter::Internal);
        surface.configure(device, surface_configuration);
        // Acquiring a frame is what materialises the swapchain: wgpu-core
        // accepting the configuration does not mean the driver created one, and
        // the refusal only shows up here.
        let probe = surface.get_current_texture();
        let internal = pollster::block_on(device.pop_error_scope());
        let out_of_memory = pollster::block_on(device.pop_error_scope());
        let validation = pollster::block_on(device.pop_error_scope());

        let reported = internal
            .or(out_of_memory)
            .or(validation)
            .map(|error| error.to_string());
        let failure = match probe {
            Ok(frame) => {
                // Dropped rather than presented: `render` acquires its own.
                drop(frame);
                reported
            }
            Err(error) => Some(reported.unwrap_or_else(|| error.to_string())),
        };

        match failure {
            None => {
                println!("[render] Surface configured: {alpha_mode:?}");
                return Ok(());
            }
            Some(failure) => failures.push(format!("{alpha_mode:?} ({failure})")),
        }
    }

    Err(RendererError::SurfaceConfigurationRefused {
        detail: failures.join("; "),
    })
}
