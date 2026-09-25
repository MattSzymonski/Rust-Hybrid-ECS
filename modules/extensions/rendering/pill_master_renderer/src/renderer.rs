#![allow(clippy::too_many_arguments)]

use crate::{
    api::{FrameOutcome, PillRenderer, RenderCapabilities, RenderMetrics},
    assets::{
        MaterialParameter, PassKind, PassTarget, ShaderParameterSlot, ShaderParameterType,
        ShaderTextureSlot, TextureType,
    },
    component::RenderViewport,
    config::{
        CAMERA_PARAMETERS_BIND_GROUP_LAYOUT_INDEX, ENGINE_PARAMETERS_BIND_GROUP_LAYOUT_INDEX,
        MATERIAL_PARAMETERS_BIND_GROUP_LAYOUT_INDEX, MATERIAL_TEXTURES_BIND_GROUP_LAYOUT_INDEX,
        MAX_INSTANCE_PER_DRAWCALL_COUNT,
    },
    drawers::mesh_drawer::MeshDrawer,
    error::{RendererError, Result},
    frame::{AssetSnapshot, RenderFrame, ResolvedPass},
    render_queue::{compose_render_queue_key, decompose_render_queue_key, RenderQueueItem},
    resources::{
        RendererCamera, RendererMaterial, RendererMesh, RendererPass, RendererResourceStorage,
        RendererShader, RendererTexture, Vertex,
    },
    slot_map::{
        RendererCameraHandle, RendererMaterialHandle, RendererMeshHandle, RendererShaderHandle,
        RendererTextureHandle,
    },
    Instance,
};
use pill_core::{info, PillStyle};
use std::{
    collections::{HashMap, HashSet},
    time::Instant,
};

/// Colour every frame starts from, whatever the first pass is.
const CLEAR_COLOR: wgpu::Color = wgpu::Color {
    r: 0.15,
    g: 0.15,
    b: 0.15,
    a: 1.0,
};

/// Format of every offscreen colour target a chain declares.
///
/// Half-float rather than the surface's `Unorm`: the values a lit frame
/// produces run well past 1.0, and a target that cannot hold them clips the
/// picture before the pass that was going to bring it back into range runs.
const OFFSCREEN_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

pub trait RendererWindow: wgpu::WindowHandle {}
impl<T> RendererWindow for T where T: wgpu::WindowHandle {}

/// Where a pass writes.
#[derive(Clone, Copy)]
enum PassOutput<'a> {
    Surface,
    Offscreen(&'a str),
}

/// A pass's GPU object, or the reason there is none.
enum PassSlot {
    /// Built and ready to record: a pipeline of its own, for the target it
    /// writes.
    Drawable(Box<RendererPass>),
    /// A geometry pass with no shader of its own. It draws every instance
    /// through the pipeline each material names, which is the renderer's
    /// built-in chain and needs nothing built per pass.
    Unshaded,
    /// Cannot be recorded, carrying the reason the log reports.
    Unsupported(String),
}

/// One pass the renderer will record.
enum PassPlan<'a> {
    /// Instances, batched by material, into the pass's targets.
    Geometry {
        label: &'a str,
        items: Vec<RenderQueueItem>,
        outputs: Vec<PassOutput<'a>>,
        clear: bool,
        /// The pass's own pipeline, when it built one. `None` leaves each
        /// instance to the pipeline its material's shader carries.
        pass_index: Option<usize>,
    },
    /// Three vertices and no vertex buffers, reading what earlier passes wrote.
    Fullscreen {
        label: &'a str,
        pass_index: usize,
        outputs: Vec<PassOutput<'a>>,
        clear: bool,
    },
}

impl PassPlan<'_> {
    /// Pass name, used as the wgpu render-pass label and in the chain log.
    fn label(&self) -> &str {
        match self {
            PassPlan::Geometry { label, .. } | PassPlan::Fullscreen { label, .. } => label,
        }
    }

    /// Where this pass writes, its own target first.
    fn outputs(&self) -> &[PassOutput<'_>] {
        match self {
            PassPlan::Geometry { outputs, .. } | PassPlan::Fullscreen { outputs, .. } => outputs,
        }
    }

    /// Whether this pass opens the targets it writes.
    fn clears(&self) -> bool {
        match self {
            PassPlan::Geometry { clear, .. } | PassPlan::Fullscreen { clear, .. } => *clear,
        }
    }

    /// How many draws this pass records: one per instance batch, or the single
    /// triangle a fullscreen pass is.
    fn draws(&self) -> usize {
        match self {
            PassPlan::Geometry { items, .. } => items.len(),
            PassPlan::Fullscreen { .. } => 1,
        }
    }
}

/// A signature of everything a chain's GPU objects depend on.
///
/// The chain is read every frame but its pipelines are not rebuilt every frame:
/// this is what decides whether they have to be. The asset revision is part of
/// it because a pass's parameters live in the asset, so editing one has to
/// rebuild the bind group that carries it.
fn chain_signature(chain: &[ResolvedPass], asset_revision: u64) -> String {
    let mut signature = format!("r{asset_revision}");
    for pass in chain {
        signature.push('|');
        signature.push_str(&pass.name);
        signature.push_str(match pass.kind {
            PassKind::Geometry => " geometry",
            PassKind::Fullscreen => " fullscreen",
        });
        match &pass.target {
            PassTarget::Surface => signature.push_str(" surface"),
            PassTarget::Offscreen(name) => {
                signature.push_str(" offscreen:");
                signature.push_str(name);
            }
        }
        if let Some(shader) = pass.shader {
            signature.push_str(&format!(" shader:{shader}"));
        }
        for extra in &pass.extra_targets {
            match extra {
                PassTarget::Surface => signature.push_str(" also:surface"),
                PassTarget::Offscreen(name) => {
                    signature.push_str(" also:offscreen:");
                    signature.push_str(name);
                }
            }
        }
        signature.push_str(&format!(
            " scale:{} blend:{} depth_write:{} cull:{:?}",
            pass.target_scale, pass.blend, pass.depth_write, pass.cull
        ));
        for (slot, target) in &pass.inputs {
            signature.push_str(&format!(" input:{slot}={target}"));
        }
        // A pass's textures are part of it for the same reason its parameters
        // are: the pointer to one lives in the asset, and the bind group that
        // reads it is built once and then kept.
        for (slot, texture) in &pass.textures {
            signature.push_str(&format!(" texture:{slot}={texture}"));
        }
    }
    signature
}

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
    /// Passes this renderer cannot record yet, remembered so each one is named
    /// once in the log instead of once per frame.
    skipped_passes: HashSet<String>,
    /// The chain last written to the log, so a chain that does not change does
    /// not write a line per frame.
    chain_log: Option<String>,
    /// One entry per pass of the current chain, in the chain's order.
    passes: Vec<PassSlot>,
    /// The chain those passes were built from.
    pipeline_signature: Option<String>,
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
            skipped_passes: HashSet::new(),
            chain_log: None,
            passes: Vec::new(),
            pipeline_signature: None,
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
            // An offscreen target is sized to the surface, so a new surface size
            // makes every one of them wrong; the chain is rebuilt with them on
            // the next frame.
            self.state.offscreen.clear();
            self.passes.clear();
            self.pipeline_signature = None;
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
        self.ensure_pipeline(&frame.passes, &frame.assets)?;
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

        // Read from the frame every time. A game can swap the pipeline, or
        // toggle a pass inside one, between any two frames, and a chain cached
        // here would keep drawing the previous one until something else
        // happened to invalidate it. An empty chain is a game that asked for
        // nothing, not a game that asked for the built-in pass: the frame's
        // writer puts that pass in the chain itself.
        let plan = self.plan_passes(&frame.passes, &render_queue);
        self.log_chain(&plan);
        self.metrics.draw_calls = plan.iter().filter(|entry| entry.draws() > 0).count() as u32;
        self.metrics.passes = plan.len() as u32;

        let submitted = Instant::now();
        self.state
            .render(self.camera, &plan, &self.passes, frame, self.viewport)?;
        self.metrics.submit_micros = submitted.elapsed().as_micros() as u64;
        Ok(FrameOutcome::Presented)
    }

    fn invalidate_assets(&mut self) {
        self.asset_revision = u64::MAX;
    }
}

impl Renderer {
    /// Hand each pass the draws it was given.
    ///
    /// A geometry pass that names a shader draws the instances shaded by that
    /// shader and leaves the rest to the other passes; one that names none draws
    /// every instance, each with the pipeline its own material names, which is
    /// what the built-in chain is. A fullscreen pass draws nothing but its
    /// triangle, and reads what earlier passes left in the targets it names.
    fn plan_passes<'a>(
        &mut self,
        chain: &'a [ResolvedPass],
        render_queue: &[RenderQueueItem],
    ) -> Vec<PassPlan<'a>> {
        let mut plan = Vec::with_capacity(chain.len());
        for (index, pass) in chain.iter().enumerate() {
            // The pass's own target first, then the ones it writes beside it.
            let outputs: Vec<PassOutput<'_>> = std::iter::once(&pass.target)
                .chain(pass.extra_targets.iter())
                .map(|target| match target {
                    PassTarget::Surface => PassOutput::Surface,
                    PassTarget::Offscreen(name) => PassOutput::Offscreen(name.as_str()),
                })
                .collect();

            match pass.kind {
                PassKind::Geometry => {
                    let reason = match self.passes.get(index) {
                        Some(PassSlot::Drawable(_)) => None,
                        Some(PassSlot::Unshaded) => None,
                        Some(PassSlot::Unsupported(reason)) => Some(reason.clone()),
                        _ => Some("it has no pipeline".to_owned()),
                    };
                    if let Some(reason) = reason {
                        self.report_skip(&pass.name, &reason);
                        continue;
                    }

                    let items = match pass.shader {
                        // The pass draws the instances shaded by the shader it
                        // names, when that shader is still loaded. One that no
                        // longer is draws nothing: falling back to the default
                        // shader would hand this pass instances another pass
                        // already took.
                        Some(key) => match self.shader_handles.get(&key) {
                            Some(handle) => {
                                let index = handle.data().index as u8;
                                render_queue
                                    .iter()
                                    .copied()
                                    .filter(|item| {
                                        decompose_render_queue_key(item.key).shader_index == index
                                    })
                                    .collect()
                            }
                            None => Vec::new(),
                        },
                        // No shader named: the pass draws every instance, each
                        // with the pipeline its own material names. This is the
                        // built-in chain.
                        None => render_queue.to_vec(),
                    };
                    let pass_index = match self.passes.get(index) {
                        Some(PassSlot::Drawable(_)) => Some(index),
                        _ => None,
                    };
                    plan.push(PassPlan::Geometry {
                        label: pass.name.as_str(),
                        items,
                        outputs,
                        clear: plan.is_empty(),
                        pass_index,
                    });
                }
                PassKind::Fullscreen => {
                    let reason = match self.passes.get(index) {
                        Some(PassSlot::Drawable(_)) => None,
                        Some(PassSlot::Unsupported(reason)) => Some(reason.clone()),
                        _ => Some("it has no fullscreen pipeline".to_owned()),
                    };
                    match reason {
                        None => plan.push(PassPlan::Fullscreen {
                            label: pass.name.as_str(),
                            pass_index: index,
                            outputs,
                            clear: plan.is_empty(),
                        }),
                        Some(reason) => self.report_skip(&pass.name, &reason),
                    }
                }
            }
        }

        // A chain with nothing recordable in it still has to open the frame's
        // targets: a swapchain image nobody cleared presents whatever was in it.
        if plan.is_empty() {
            plan.push(PassPlan::Geometry {
                label: "frame.clear",
                items: Vec::new(),
                outputs: vec![PassOutput::Surface],
                clear: true,
                pass_index: None,
            });
        }
        plan
    }

    /// Build the GPU objects the chain needs.
    ///
    /// One offscreen target per output the chain declares, and one pipeline per
    /// fullscreen pass. Keyed by a signature of the chain, because this is the
    /// expensive part of a frame's setup and an unchanged chain needs none of it
    /// a second time.
    fn ensure_pipeline(&mut self, chain: &[ResolvedPass], assets: &AssetSnapshot) -> Result<()> {
        let signature = chain_signature(chain, assets.revision);
        if self.pipeline_signature.as_deref() == Some(signature.as_str()) {
            return Ok(());
        }

        self.state.ensure_offscreen_targets(chain);
        let passes: Vec<PassSlot> = chain
            .iter()
            .map(|pass| self.build_pass(pass, assets))
            .collect();
        self.passes = passes;
        self.pipeline_signature = Some(signature);
        Ok(())
    }

    /// A pass's GPU object, or the reason there is none.
    fn build_pass(&self, pass: &ResolvedPass, assets: &AssetSnapshot) -> PassSlot {
        // A geometry pass with no shader of its own draws through each
        // material's pipeline, and that is the only path that needs no object.
        let Some(shader_key) = pass.shader else {
            return match pass.kind {
                PassKind::Geometry => PassSlot::Unshaded,
                PassKind::Fullscreen => {
                    PassSlot::Unsupported("it names no shader the renderer loaded".to_owned())
                }
            };
        };

        let Some(shader) = assets.shaders.get(&shader_key) else {
            return PassSlot::Unsupported("it names no shader the renderer loaded".to_owned());
        };
        let Some(renderer_shader) = self
            .shader_handles
            .get(&shader_key)
            .and_then(|handle| self.state.renderer_resource_storage.shaders.get(*handle))
        else {
            return PassSlot::Unsupported("its shader has no pipeline".to_owned());
        };
        // One format per target the pass writes, in the order the shader's
        // `SV_TARGET` list names them. A target the pass writes beside its own
        // is what a geometry pass leaves a normal buffer in.
        let target_formats: Vec<wgpu::TextureFormat> = std::iter::once(&pass.target)
            .chain(pass.extra_targets.iter())
            .map(|target| match target {
                PassTarget::Surface => self.state.color_format,
                PassTarget::Offscreen(name) => self
                    .state
                    .offscreen
                    .get(name)
                    .map(|texture| texture.texture.format())
                    .unwrap_or(OFFSCREEN_FORMAT),
            })
            .collect();

        // The pass's own committed textures, by slot. They resolve to the same
        // GPU handles a material's textures do, so a pass and a material reach
        // one texture by one key.
        let textures: Vec<(String, RendererTextureHandle)> = pass
            .textures
            .iter()
            .filter_map(|(slot, key)| {
                self.texture_handles
                    .get(key)
                    .map(|handle| (slot.clone(), *handle))
            })
            .collect();

        let storage = &self.state.renderer_resource_storage;
        match RendererPass::new(
            &self.state.device,
            &self.state.queue,
            storage,
            &storage.engine_parameters.bind_group_layout,
            &self.state.camera_bind_group_layout,
            pass,
            shader,
            renderer_shader,
            &target_formats,
            self.state.depth_format,
            &self.state.offscreen,
            &self.state.depth_texture,
            &textures,
        ) {
            Ok(value) => PassSlot::Drawable(Box::new(value)),
            Err(error) => PassSlot::Unsupported(error.to_string()),
        }
    }

    /// Name a pass the renderer cannot record, once rather than once per frame.
    fn report_skip(&mut self, name: &str, reason: &str) {
        if !self.skipped_passes.insert(name.to_owned()) {
            return;
        }

        // Printed, like the renderer's other once-per-change diagnostics: the
        // log target this would otherwise use is filtered out of the host's log.
        println!("[render] Pass {name} is not drawn: {reason}");
    }

    /// Write the chain to the log, once per change.
    ///
    /// Each pass is named with the number of draws it was handed, which is the
    /// one thing that tells a chain doing what the game asked apart from one
    /// quietly falling back to the built-in pass. A line per frame would bury
    /// everything else, and an unchanged chain is the normal case.
    fn log_chain(&mut self, plan: &[PassPlan]) {
        let mut signature = String::new();
        for entry in plan {
            if !signature.is_empty() {
                signature.push(' ');
            }
            signature.push_str(entry.label());
            signature.push('(');
            signature.push_str(&entry.draws().to_string());
            signature.push(')');
        }
        if self.chain_log.as_deref() == Some(signature.as_str()) {
            return;
        }

        // Printed, like the renderer's other once-per-change diagnostics: the
        // log target this would otherwise use is filtered out of the host's log,
        // and this line is what a reader has when the window shows the wrong
        // thing.
        println!("[render] Pass chain: {signature}");
        self.chain_log = Some(signature);
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
    /// The offscreen colour targets the current chain names, by that name.
    ///
    /// Owned here rather than by the passes that write them: two passes name
    /// the same target - one writes it, the next reads it - and a texture owned
    /// by either would be a lifetime the chain cannot express.
    offscreen: HashMap<String, RendererTexture>,
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
        // Preference order taken from the reference: an sRGB target where the
        // surface offers one, Rgba ahead of Bgra because it needs no channel
        // swizzle, and finally whatever the surface does advertise rather than
        // refusing to start.
        let color_format = [
            wgpu::TextureFormat::Rgba8UnormSrgb,
            wgpu::TextureFormat::Bgra8UnormSrgb,
            wgpu::TextureFormat::Bgra8Unorm,
        ]
        .into_iter()
        .find(|format| capabilities.formats.contains(format))
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
            offscreen: HashMap::new(),
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

    /// Create a target for every offscreen output the chain declares.
    ///
    /// Rebuilt whenever the chain is, and sized to the surface: one that no
    /// longer matches the window would be sampled at a different scale from the
    /// pass that wrote it, which shows up as a picture that shrinks with the
    /// window rather than one that resizes with it.
    fn ensure_offscreen_targets(&mut self, chain: &[ResolvedPass]) {
        let width = self.surface_configuration.width;
        let height = self.surface_configuration.height;
        let device = &self.device;
        let targets = &mut self.offscreen;
        targets.clear();

        for pass in chain {
            // The pass's own target first, then the ones it writes beside it.
            // The scale is the first pass's: a target belongs to the chain, and
            // the pass that names it first is asking on everyone's behalf.
            let scale = pass.target_scale.max(1);
            let written = std::iter::once(&pass.target).chain(pass.extra_targets.iter());
            for target in written {
                let PassTarget::Offscreen(name) = target else {
                    continue;
                };
                targets.entry(name.clone()).or_insert_with(|| {
                    RendererTexture::new_render_target(
                        device,
                        name,
                        (width / scale).max(1),
                        (height / scale).max(1),
                        OFFSCREEN_FORMAT,
                    )
                });
            }
        }
    }

    fn render(
        &mut self,
        camera_handle: RendererCameraHandle,
        plan: &[PassPlan],
        passes: &[PassSlot],
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
        self.renderer_resource_storage.engine_parameters.update(
            &self.queue,
            0.0,
            [0.0; 3],
            [
                self.surface_configuration.width,
                self.surface_configuration.height,
            ],
            [
                frame.seconds,
                frame.delta_seconds,
                frame.sequence as f32,
                0.0,
            ],
        );
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
        // One wgpu render pass per pass of the chain, into the same encoder: the
        // first clears the targets it writes, the rest add to what is there.
        for entry in plan {
            // Every target the pass writes, in the order the shader's
            // `SV_TARGET` list names them.
            let mut views = Vec::with_capacity(entry.outputs().len());
            let mut missing_target = false;
            for output in entry.outputs() {
                match output {
                    PassOutput::Surface => views.push(&view),
                    // A pass whose target was never created draws nothing rather
                    // than into the wrong one; the chain log already named the
                    // pass that could not be built.
                    PassOutput::Offscreen(name) => match self.offscreen.get(*name) {
                        Some(texture) => views.push(&texture.texture_view),
                        None => missing_target = true,
                    },
                }
            }
            if missing_target {
                continue;
            }

            let clear = entry.clears();
            let color_attachments: Vec<Option<wgpu::RenderPassColorAttachment>> = views
                .iter()
                .map(|target| {
                    Some(wgpu::RenderPassColorAttachment {
                        view: target,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: color_load(clear),
                            store: wgpu::StoreOp::Store,
                        },
                    })
                })
                .collect();

            match entry {
                PassPlan::Geometry {
                    label,
                    items,
                    pass_index,
                    ..
                } => {
                    let depth_stencil_attachment = wgpu::RenderPassDepthStencilAttachment {
                        view: &self.depth_texture.texture_view,
                        depth_ops: Some(wgpu::Operations {
                            load: if clear {
                                wgpu::LoadOp::Clear(1.0)
                            } else {
                                wgpu::LoadOp::Load
                            },
                            store: wgpu::StoreOp::Store,
                        }),
                        stencil_ops: None,
                    };
                    // A pass that built a pipeline draws through it: the
                    // pipeline holds the targets' formats and the pass's depth
                    // and culling, none of which an instance may choose.
                    let pipeline = pass_index.and_then(|index| match passes.get(index) {
                        Some(PassSlot::Drawable(pass)) => Some(&pass.pipeline),
                        _ => None,
                    });
                    self.mesh_drawer.record_draw_commands(
                        &self.device,
                        &self.queue,
                        &mut encoder,
                        &self.renderer_resource_storage,
                        label,
                        pipeline,
                        &color_attachments,
                        depth_stencil_attachment,
                        camera,
                        items,
                        &frame.instances,
                        viewport,
                    )?;
                }
                PassPlan::Fullscreen {
                    label, pass_index, ..
                } => {
                    let Some(PassSlot::Drawable(pass)) = passes.get(*pass_index) else {
                        continue;
                    };
                    let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some(label),
                        color_attachments: &color_attachments,
                        // No depth: a fullscreen pass overwrites every pixel it
                        // covers, and the depth left behind describes geometry
                        // that is not what this triangle is.
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                    });
                    render_pass.set_pipeline(&pass.pipeline);
                    if pass.pass_engine_parameters {
                        render_pass.set_bind_group(
                            ENGINE_PARAMETERS_BIND_GROUP_LAYOUT_INDEX,
                            &self.renderer_resource_storage.engine_parameters.bind_group,
                            &[],
                        );
                    }
                    if pass.pass_camera_parameters {
                        render_pass.set_bind_group(
                            CAMERA_PARAMETERS_BIND_GROUP_LAYOUT_INDEX,
                            &camera.bind_group,
                            &[],
                        );
                    }
                    if let Some(bind_group) = &pass.parameters_bind_group {
                        render_pass.set_bind_group(
                            MATERIAL_PARAMETERS_BIND_GROUP_LAYOUT_INDEX,
                            bind_group,
                            &[],
                        );
                    }
                    if let Some(bind_group) = &pass.textures_bind_group {
                        render_pass.set_bind_group(
                            MATERIAL_TEXTURES_BIND_GROUP_LAYOUT_INDEX,
                            bind_group,
                            &[],
                        );
                    }
                    render_pass.draw(0..3, 0..1);
                }
            }
        }
        self.queue.submit(std::iter::once(encoder.finish()));
        surface_frame.present();
        Ok(())
    }
}

/// The colour a pass opens its target with, or the values already in it.
fn color_load(clear: bool) -> wgpu::LoadOp<wgpu::Color> {
    if clear {
        wgpu::LoadOp::Clear(CLEAR_COLOR)
    } else {
        wgpu::LoadOp::Load
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pass(kind: PassKind, target: PassTarget) -> ResolvedPass {
        ResolvedPass {
            name: "p".to_owned(),
            shader: None,
            kind,
            target,
            extra_targets: Vec::new(),
            target_scale: 1,
            blend: false,
            depth_write: true,
            cull: crate::assets::CullMode::Back,
            parameters: HashMap::new(),
            inputs: HashMap::new(),
            textures: HashMap::new(),
            order: 0,
        }
    }

    #[test]
    fn a_chain_signature_covers_everything_the_gpu_objects_depend_on() {
        let surface = pass(PassKind::Geometry, PassTarget::Surface);
        let offscreen = pass(PassKind::Geometry, PassTarget::Offscreen("hdr".to_owned()));
        let mut reading = pass(PassKind::Geometry, PassTarget::Surface);
        reading.inputs.insert("hdr".to_owned(), "hdr".to_owned());

        let base = chain_signature(std::slice::from_ref(&surface), 1);

        assert_eq!(base, chain_signature(std::slice::from_ref(&surface), 1));
        assert_ne!(base, chain_signature(std::slice::from_ref(&offscreen), 1));
        assert_ne!(base, chain_signature(std::slice::from_ref(&reading), 1));
        assert_ne!(
            base,
            chain_signature(std::slice::from_ref(&surface), 2),
            "a pass's parameters live in the asset, so the asset revision is part of it"
        );
    }
}
