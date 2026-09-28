//! The master renderer: draws a frame's resolved chain and instances, over the
//! device, surface, and frame state [`State`] owns.
//!
//! # Responsibilities
//!
//! - Keep the GPU caches level with each frame's asset store, one asset
//!   type at a time, rebuilding only what moved ([`Renderer`] and its
//!   per-type sync helpers).
//! - Decide when the chain's pipelines, bind groups, and offscreen targets
//!   have to be rebuilt, from two signatures compared against the frame's
//!   chain generation, resource epoch, and surface size.
//! - Plan each pass - which draws it takes, which pass opens each target -
//!   and name a pass that cannot be recorded once, rather than once per
//!   frame (`PassPlan`).
//! - Submit the frame through [`State`]: one command encoder, one wgpu render
//!   pass per planned pass, then the present.
//!
//! # Design
//!
//! The chain is read every frame but its GPU objects are not rebuilt every
//! frame: an unchanged frame costs one tuple comparison, and the signature
//! strings are only built once the chain generation, the resource epoch, or
//! the surface size moved. Creation failures are reported and skipped rather
//! than fatal - a shader, texture, or target that would not build is named in
//! the log while the rest of the revision still reaches the GPU.

#![allow(clippy::too_many_arguments)]

// Standard library
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    time::Instant,
};

// External crates
use pill_core::{info, PillStyle};
use pill_engine::{AssetManager, Handle};

// Current crate
use crate::{
    api::{FrameOutcome, PillRenderer, RenderCapabilities, RenderMetrics},
    assets::{
        asset_key, parameter_slots_by_name, texture_slots_by_name, Material, MaterialParameter,
        Mesh, PassKind, PassTarget, Shader, ShaderParameterSlot, ShaderParameterType,
        ShaderTextureSlot, Texture, TextureType,
    },
    components::RenderViewport,
    config::{
        CAMERA_PARAMETERS_BIND_GROUP_LAYOUT_INDEX, ENGINE_PARAMETERS_BIND_GROUP_LAYOUT_INDEX,
        INSTANCE_BATCH_SIZE, MATERIAL_PARAMETERS_BIND_GROUP_LAYOUT_INDEX,
        MATERIAL_TEXTURES_BIND_GROUP_LAYOUT_INDEX,
    },
    drawers::mesh_drawer::MeshDrawer,
    error::{capturing_validation, RendererError, Result},
    frame::{RenderFrame, ResolvedPass},
    render_queue::{compose_render_queue_key, decompose_render_queue_key, RenderQueueItem},
    resource_handles::{
        RendererCameraHandle, RendererMaterialHandle, RendererMeshHandle, RendererShaderHandle,
        RendererTextureHandle,
    },
    resources::{
        RendererCamera, RendererMaterial, RendererMesh, RendererPass, RendererResourceStorage,
        RendererShader, RendererTexture, Vertex,
    },
    surface::{RendererWindow, Surface},
    Instance,
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
    ///
    /// Borrowed when the pass takes the whole queue, which is the built-in
    /// chain's shape; only a shader-filtered pass pays for a collection.
    Geometry {
        label: &'a str,
        items: Cow<'a, [RenderQueueItem]>,
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
/// this is what decides whether they have to be. It covers the chain's content,
/// including pass parameters and the textures a pass names - both iterated in
/// slot order so a `HashMap`'s arbitrary order cannot make an unchanged chain
/// look new - plus the resource epoch, which moves whenever a shader or texture
/// the bind groups reference was recreated.
fn chain_signature(chain: &[ResolvedPass], resource_epoch: u64) -> String {
    let mut signature = format!("e{resource_epoch}");
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
        let mut inputs: Vec<_> = pass.inputs.iter().collect();
        inputs.sort();
        for (slot, target) in inputs {
            signature.push_str(&format!(" input:{slot}={target}"));
        }
        // A pass's textures are part of it for the same reason its parameters
        // are: the handle the bind group reads is resolved here, and it is
        // built once and then kept.
        let mut textures: Vec<_> = pass.textures.iter().collect();
        textures.sort();
        for (slot, texture) in textures {
            signature.push_str(&format!(" texture:{slot}={texture}"));
        }
        // Serialized, not hashed: a pass's parameters live in a uniform buffer
        // the bind group keeps, so this is what makes editing one rebuild it.
        let mut parameters: Vec<_> = pass.parameters.iter().collect();
        parameters.sort_by_key(|(name, _)| name.as_str());
        for (name, parameter) in parameters {
            signature.push_str(&format!(" param:{name}={parameter:?}"));
        }
    }
    signature
}

/// A signature of the offscreen targets a chain needs: every target name, the
/// scale of the pass that names it, and the surface size it is built at.
///
/// Kept apart from the pipeline signature on purpose: editing an asset moves
/// the pipeline signature (a pass's parameters are part of it) but changes none
/// of this, so the offscreen textures survive an asset edit.
fn offscreen_signature(chain: &[ResolvedPass], width: u32, height: u32) -> String {
    let mut signature = format!("{width}x{height}");
    for pass in chain {
        let scale = pass.target_scale.max(1);
        for target in std::iter::once(&pass.target).chain(pass.extra_targets.iter()) {
            if let PassTarget::Offscreen(name) = target {
                signature.push('|');
                signature.push_str(name);
                signature.push_str(&format!("@{scale}"));
            }
        }
    }
    signature
}

/// The master renderer: the asset caches, the chain's GPU objects, and the
/// frame submission behind the [`PillRenderer`] contract.
///
/// One renderer owns one window's surface and everything drawn to it. The
/// per-asset maps are its frame-to-GPU diff: each sync compares an asset's
/// content version against the version the object behind its key was built
/// from, and rebuilds only what moved.
pub struct Renderer {
    /// The wgpu device, surface, and frame state this renderer drives.
    pub state: State,
    /// The store revision the GPU caches are level with, or `None` after an
    /// explicit invalidation; the sync runs when it does not match the store's.
    assets_synced_revision: Option<u64>,
    /// Content version of every asset the GPU object behind a key was built
    /// from, one map per asset type. A key whose version moved - or that is not
    /// recorded here at all - is rebuilt; a key whose version matches keeps its
    /// object.
    shader_versions: HashMap<u64, u64>,
    material_versions: HashMap<u64, u64>,
    texture_versions: HashMap<u64, u64>,
    mesh_versions: HashMap<u64, u64>,
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
    /// What `pipeline_signature` was last checked against: the frame's chain
    /// generation, the resource epoch and the surface size. One tuple compare
    /// per frame - no strings built - until the frame's chain or the GPU
    /// objects behind it actually move.
    pipeline_inputs: Option<(u64, u64, u32, u32)>,
    /// The offscreen layout the current targets were built for.
    offscreen_key: Option<String>,
    /// Bumped whenever a shader or texture GPU object is created, recreated,
    /// or dropped: pass bind groups reference those objects by handle, so a
    /// change to one is what invalidates them.
    resource_epoch: u64,
    metrics: RenderMetrics,
}

impl Renderer {
    /// Creates the renderer synchronously, blocking on [`Renderer::new_async`].
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Renderer::new_async`], which does the work.
    pub fn new<W: RendererWindow + 'static>(window: W, width: u32, height: u32) -> Result<Self> {
        pollster::block_on(Self::new_async(window, width, height))
    }

    /// Creates the renderer: device and surface, the default material, and the
    /// camera.
    ///
    /// # Errors
    ///
    /// Returns a [`RendererError`] when a creation step fails: surface or
    /// adapter acquisition, device creation, surface configuration, the depth
    /// buffer, the resource storage, the default material, or the camera.
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
            assets_synced_revision: None,
            shader_versions: HashMap::new(),
            material_versions: HashMap::new(),
            texture_versions: HashMap::new(),
            mesh_versions: HashMap::new(),
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
            pipeline_inputs: None,
            offscreen_key: None,
            resource_epoch: 0,
            metrics: RenderMetrics::default(),
        })
    }

    /// Bring the GPU caches level with the asset store, per asset.
    ///
    /// The manager's revision gates the pass, and its per-asset content
    /// versions decide what inside it moved: one edited texture is re-uploaded
    /// while its neighbours keep their GPU objects, and a removed asset frees
    /// its slot. Creation failures are reported and skipped rather than
    /// aborting the pass, so one bad asset neither holds the rest of the
    /// revision hostage nor is retried every frame.
    fn sync_assets(&mut self, assets: &AssetManager) {
        if self.assets_synced_revision == Some(assets.revision()) {
            return;
        }
        self.assets_synced_revision = Some(assets.revision());

        // Textures and shaders first: materials bind them by handle. The epoch
        // moves only when one of these two layers did, because those are the
        // objects a pass's bind groups keep - and the materials built against
        // them are invalidated by key, because their own content version alone
        // would not ask for a rebuild.
        let mut changed: HashSet<u64> = HashSet::new();
        let mut epoch_moved = self.sync_textures(assets, &mut changed);
        epoch_moved |= self.sync_shaders(assets, &mut changed);
        self.sync_meshes(assets);
        if !changed.is_empty() {
            self.invalidate_dependent_materials(assets, &changed);
        }
        self.sync_materials(assets);
        if epoch_moved {
            self.resource_epoch = self.resource_epoch.wrapping_add(1);
        }
    }

    /// Forget every built material that reads an asset just recreated.
    ///
    /// A material's bind groups are built against the layouts and views its
    /// shader and textures had at the time; when one of those is rebuilt or
    /// removed, the groups go stale while the material's own content version
    /// stands still. Dropping the version record is what makes `sync_materials`
    /// rebuild it in the same pass, against the new object.
    fn invalidate_dependent_materials(&mut self, assets: &AssetManager, changed: &HashSet<u64>) {
        for (handle, material) in assets.iter_handles::<Material>() {
            let shader_changed = changed.contains(&asset_key(material.shader));
            let texture_changed = material
                .textures
                .values()
                .any(|texture| changed.contains(&asset_key(texture.texture)));
            if shader_changed || texture_changed {
                self.material_versions.remove(&asset_key(handle));
            }
        }
    }

    /// Drop and rebuild the textures whose asset changed.
    ///
    /// Returns whether any texture object was created, recreated, or dropped.
    /// Keys whose object moved are added to `changed`, so the materials built
    /// against them can be rebuilt too.
    fn sync_textures(&mut self, assets: &AssetManager, changed: &mut HashSet<u64>) -> bool {
        let mut epoch_moved = false;
        // Every key the store still holds, collected while the rebuild pass
        // walks it. The renderer caches by key and the manager looks up by
        // handle, so walking the store is the only way back from a cached key
        // to whether the asset behind it is still live.
        let mut live: HashSet<u64> = HashSet::with_capacity(self.texture_handles.len());

        for (handle, texture) in assets.iter_handles::<Texture>() {
            let key = asset_key(handle);
            live.insert(key);
            let version = assets.content_version(handle).unwrap_or(0);
            if self.texture_versions.get(&key) == Some(&version) {
                continue;
            }
            // Recorded before the rebuild: even a failed creation dropped the
            // texture the old materials were built against.
            changed.insert(key);
            if let Some(old) = self.texture_handles.remove(&key) {
                self.state.renderer_resource_storage.textures.remove(old);
            }
            match RendererTexture::new_texture(
                &self.state.device,
                &self.state.queue,
                Some(&texture.name),
                &texture.rgba,
                texture.width,
                texture.height,
                texture.texture_type,
            ) {
                Ok(value) => {
                    let gpu = self.state.renderer_resource_storage.textures.insert(value);
                    self.texture_handles.insert(key, gpu);
                    self.texture_versions.insert(key, version);
                    epoch_moved = true;
                }
                // Left unrecorded, so a later revision tries again - and named,
                // so the log says which asset is not on the GPU.
                Err(error) => pill_core::warn!(
                    target: pill_core::telemetry::telemetry_target::RENDERING,
                    "texture `{}` is not uploaded: {error}",
                    texture.name
                ),
            }
        }

        // A cached key the store no longer holds: its asset was removed, so its
        // object goes with it.
        let removed: Vec<u64> = self
            .texture_handles
            .keys()
            .filter(|key| !live.contains(key))
            .copied()
            .collect();
        for key in removed {
            if let Some(gpu) = self.texture_handles.remove(&key) {
                self.state.renderer_resource_storage.textures.remove(gpu);
                self.texture_versions.remove(&key);
                changed.insert(key);
                epoch_moved = true;
            }
        }
        epoch_moved
    }

    /// Drop and rebuild the shaders whose asset changed.
    ///
    /// Returns whether any shader object was created, recreated, or dropped.
    /// Keys whose object moved are added to `changed`, so the materials built
    /// against them can be rebuilt too.
    fn sync_shaders(&mut self, assets: &AssetManager, changed: &mut HashSet<u64>) -> bool {
        let mut epoch_moved = false;
        // See `sync_textures`: walking the store is what says which cached keys
        // are still live.
        let mut live: HashSet<u64> = HashSet::with_capacity(self.shader_handles.len());

        for (handle, shader) in assets.iter_handles::<Shader>() {
            let key = asset_key(handle);
            live.insert(key);
            let version = assets.content_version(handle).unwrap_or(0);
            if self.shader_versions.get(&key) == Some(&version) {
                continue;
            }
            changed.insert(key);
            if let Some(old) = self.shader_handles.remove(&key) {
                self.state.renderer_resource_storage.shaders.remove(old);
            }
            let value = RendererShader::new(
                &shader.name,
                &self.state.device,
                self.state.surface.format(),
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
            );
            match value {
                Ok(value) => {
                    let gpu = self.state.renderer_resource_storage.shaders.insert(value);
                    self.shader_handles.insert(key, gpu);
                    self.shader_versions.insert(key, version);
                    epoch_moved = true;
                }
                Err(error) => pill_core::warn!(
                    target: pill_core::telemetry::telemetry_target::RENDERING,
                    "shader `{}` is not compiled: {error}",
                    shader.name
                ),
            }
        }

        let removed: Vec<u64> = self
            .shader_handles
            .keys()
            .filter(|key| !live.contains(key))
            .copied()
            .collect();
        for key in removed {
            if let Some(gpu) = self.shader_handles.remove(&key) {
                self.state.renderer_resource_storage.shaders.remove(gpu);
                self.shader_versions.remove(&key);
                changed.insert(key);
                epoch_moved = true;
            }
        }
        epoch_moved
    }

    /// Drop and rebuild the meshes whose asset changed.
    ///
    /// No epoch is needed for meshes: the draw path resolves a mesh handle per
    /// frame, so a rebuilt one is picked up without invalidating any pipeline.
    fn sync_meshes(&mut self, assets: &AssetManager) {
        let mut live: HashSet<u64> = HashSet::with_capacity(self.mesh_handles.len());

        for (handle, mesh) in assets.iter_handles::<Mesh>() {
            let key = asset_key(handle);
            live.insert(key);
            let version = assets.content_version(handle).unwrap_or(0);
            if self.mesh_versions.get(&key) == Some(&version) {
                continue;
            }
            if let Some(old) = self.mesh_handles.remove(&key) {
                self.state.renderer_resource_storage.meshes.remove(old);
            }
            match RendererMesh::new(&self.state.device, &mesh.name, mesh) {
                Ok(value) => {
                    let gpu = self.state.renderer_resource_storage.meshes.insert(value);
                    self.mesh_handles.insert(key, gpu);
                    self.mesh_versions.insert(key, version);
                }
                Err(error) => pill_core::warn!(
                    target: pill_core::telemetry::telemetry_target::RENDERING,
                    "mesh `{}` is not on the GPU: {error}",
                    mesh.name
                ),
            }
        }

        let removed: Vec<u64> = self
            .mesh_handles
            .keys()
            .filter(|key| !live.contains(key))
            .copied()
            .collect();
        for key in removed {
            if let Some(gpu) = self.mesh_handles.remove(&key) {
                self.state.renderer_resource_storage.meshes.remove(gpu);
                self.mesh_versions.remove(&key);
            }
        }
    }

    /// Drop and rebuild the materials whose asset changed.
    fn sync_materials(&mut self, assets: &AssetManager) {
        let mut live: HashSet<u64> = HashSet::with_capacity(self.material_handles.len());

        for (handle, material) in assets.iter_handles::<Material>() {
            let key = asset_key(handle);
            live.insert(key);
            let version = assets.content_version(handle).unwrap_or(0);
            if self.material_versions.get(&key) == Some(&version) {
                continue;
            }
            if let Some(old) = self.material_handles.remove(&key) {
                self.state.renderer_resource_storage.materials.remove(old);
            }
            let shader_key = asset_key(material.shader);
            let shader = if material.shader == Handle::INVALID {
                // A material built without a shader is documented to draw with
                // the renderer's own; that is a choice, not a fault.
                self.default_shader
            } else if let Some(gpu) = self.shader_handles.get(&shader_key) {
                *gpu
            } else {
                pill_core::warn!(
                    target: pill_core::telemetry::telemetry_target::RENDERING,
                    "material `{}` names a shader that is not loaded; drawing with the default shader",
                    material.name
                );
                self.default_shader
            };
            let textures = material
                .textures
                .iter()
                .filter_map(|(slot, texture)| {
                    self.texture_handles
                        .get(&asset_key(texture.texture))
                        .copied()
                        .map(|gpu| (slot.clone(), gpu))
                })
                .collect::<Vec<_>>();
            match RendererMaterial::new(
                &self.state.device,
                &self.state.queue,
                &self.state.renderer_resource_storage,
                &material.name,
                shader,
                &textures,
                &material.parameters,
            ) {
                Ok(value) => {
                    let gpu = self.state.renderer_resource_storage.materials.insert(value);
                    self.material_handles.insert(key, gpu);
                    self.material_versions.insert(key, version);
                }
                Err(error) => pill_core::warn!(
                    target: pill_core::telemetry::telemetry_target::RENDERING,
                    "material `{}` has no pipeline: {error}",
                    material.name
                ),
            }
        }

        let removed: Vec<u64> = self
            .material_handles
            .keys()
            .filter(|key| !live.contains(key))
            .copied()
            .collect();
        for key in removed {
            if let Some(gpu) = self.material_handles.remove(&key) {
                self.state
                    .renderer_resource_storage
                    .materials
                    .remove(gpu);
                self.material_versions.remove(&key);
            }
        }
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
        if self.minimized {
            return;
        }
        if let Err(error) = self
            .state
            .resize(winit::dpi::PhysicalSize::new(width, height))
        {
            // Warned instead of panicking: a size the driver will not take is
            // not worth killing a running game over, and the next resize event
            // tries again.
            pill_core::warn!(
                target: pill_core::telemetry::telemetry_target::RENDERING,
                "surface resize to {width}x{height} failed: {error}"
            );
            return;
        }
        // An offscreen target is sized to the surface, so a new surface size
        // makes every one of them wrong; the chain is rebuilt with them on
        // the next frame.
        self.state.offscreen.clear();
        self.passes.clear();
        self.pipeline_signature = None;
    }

    fn set_viewport(&mut self, viewport: Option<RenderViewport>) {
        self.viewport = viewport;
    }

    fn render(&mut self, frame: &RenderFrame, assets: &AssetManager) -> Result<FrameOutcome> {
        if self.minimized || !frame.has_camera {
            return Ok(FrameOutcome::Skipped);
        }
        let prepare = Instant::now();
        self.sync_assets(assets);
        self.ensure_pipeline(&frame.passes, assets, frame.chain_generation)?;
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
            render_queue.push(RenderQueueItem {
                key: compose_render_queue_key(instance.rendering_order, shader, material, mesh),
                entity_index: index as u32,
            });
        }
        render_queue.sort_unstable();
        self.metrics.prepare_micros = prepare.elapsed().as_micros() as u64;
        self.metrics.instance_bytes = (render_queue.len() * std::mem::size_of::<Instance>()) as u64;

        // The frame's chain generation is what makes reading it every frame
        // cheap: the frame resolves the chain only when it changes, and
        // `ensure_pipeline` restarts only when the generation it built from
        // moves. An empty chain is a game that asked for nothing, not a game
        // that asked for the built-in pass: the frame's writer puts that pass
        // in the chain itself.
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
        // Forget the versions so the next sync rebuilds every GPU object from
        // the store, let that sync through the revision gate, and drop the
        // pipeline inputs so the rebuild reaches the bind groups that reference
        // the old objects.
        self.assets_synced_revision = None;
        self.texture_versions.clear();
        self.shader_versions.clear();
        self.mesh_versions.clear();
        self.material_versions.clear();
        self.pipeline_inputs = None;
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
        render_queue: &'a [RenderQueueItem],
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

                    let items: Cow<'a, [RenderQueueItem]> = match pass.shader {
                        // The pass draws the instances shaded by the shader it
                        // names, when that shader is still loaded. One that no
                        // longer is draws nothing: falling back to the default
                        // shader would hand this pass instances another pass
                        // already took.
                        Some(key) => match self.shader_handles.get(&key) {
                            Some(handle) => {
                                let index = handle.data().index as u8;
                                Cow::Owned(
                                    render_queue
                                        .iter()
                                        .copied()
                                        .filter(|item| {
                                            decompose_render_queue_key(item.key).shader_index
                                                == index
                                        })
                                        .collect::<Vec<_>>(),
                                )
                            }
                            None => Cow::Borrowed(&[] as &[RenderQueueItem]),
                        },
                        // No shader named: the pass draws every instance, each
                        // with the pipeline its own material names. This is the
                        // built-in chain, and it borrows the queue rather than
                        // copying it.
                        None => Cow::Borrowed(render_queue),
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
                items: Cow::default(),
                outputs: vec![PassOutput::Surface],
                clear: true,
                pass_index: None,
            });
        }
        plan
    }

    /// Build the GPU objects the chain needs, when something they depend on
    /// moved.
    ///
    /// Two keys decide. The offscreen layout - every target and scale the chain
    /// declares, at the surface size - decides the render target textures;
    /// everything else (the chain's serialized content and the resource epoch)
    /// decides the pass pipelines and their bind groups. An unchanged frame
    /// costs one tuple comparison: signature strings are only built when the
    /// chain generation, the resource epoch or the surface size moved.
    fn ensure_pipeline(
        &mut self,
        chain: &[ResolvedPass],
        assets: &AssetManager,
        chain_generation: u64,
    ) -> Result<()> {
        let (width, height) = self.state.surface.size();
        let inputs = (chain_generation, self.resource_epoch, width, height);
        if self.pipeline_inputs == Some(inputs) {
            return Ok(());
        }

        let offscreen_key = offscreen_signature(chain, width, height);
        let targets_changed = self.offscreen_key.as_deref() != Some(offscreen_key.as_str());
        let signature = chain_signature(chain, self.resource_epoch);
        if !targets_changed && self.pipeline_signature.as_deref() == Some(signature.as_str()) {
            self.pipeline_inputs = Some(inputs);
            return Ok(());
        }

        if targets_changed {
            self.state.ensure_offscreen_targets(chain);
            self.offscreen_key = Some(offscreen_key);
        }

        // A pass builds its own pipeline from the shader's sources, and the
        // chain names that shader by key while the manager looks assets up by
        // handle. One walk of the shader column builds the way from one to the
        // other - here rather than once per pass, and only when the chain is
        // actually rebuilt.
        let shaders_by_key: HashMap<u64, &Shader> = assets
            .iter_handles::<Shader>()
            .map(|(handle, shader)| (asset_key(handle), shader))
            .collect();

        // Passes are built in chain order and may only read targets an earlier
        // pass declares: the set grows as the chain is walked, so a pass that
        // reads its own output, or a later pass's, is refused instead of
        // binding whatever the map happens to hold.
        let mut defined_targets: HashSet<String> = HashSet::new();
        let mut passes: Vec<PassSlot> = Vec::with_capacity(chain.len());
        for pass in chain {
            passes.push(self.build_pass(pass, &shaders_by_key, &defined_targets));
            for target in std::iter::once(&pass.target).chain(pass.extra_targets.iter()) {
                if let PassTarget::Offscreen(name) = target {
                    defined_targets.insert(name.clone());
                }
            }
        }
        self.passes = passes;
        self.pipeline_signature = Some(signature);
        self.pipeline_inputs = Some(inputs);
        Ok(())
    }

    /// A pass's GPU object, or the reason there is none.
    ///
    /// `defined_targets` is what earlier passes of the chain declared so far;
    /// the pass may only read those.
    fn build_pass(
        &self,
        pass: &ResolvedPass,
        shaders_by_key: &HashMap<u64, &Shader>,
        defined_targets: &HashSet<String>,
    ) -> PassSlot {
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

        let Some(shader) = shaders_by_key.get(&shader_key).copied() else {
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
                PassTarget::Surface => self.state.surface.format(),
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
        // one texture by one key. A slot whose texture is not loaded falls
        // through to the binder's default, which the log names rather than
        // leaving a pass quietly showing the wrong map.
        let mut textures: Vec<(String, RendererTextureHandle)> = Vec::new();
        for (slot, key) in &pass.textures {
            match self.texture_handles.get(key) {
                Some(handle) => textures.push((slot.clone(), *handle)),
                None => println!(
                    "[render] Pass {} binds texture `{slot}`, which is not loaded; the shader's default is used",
                    pass.name
                ),
            }
        }

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
            defined_targets,
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
    let parameter_slots = parameter_slots_by_name(
        "pill_default_lit",
        [
            ShaderParameterSlot::new("tint", ShaderParameterType::Color),
            ShaderParameterSlot::new("specularity", ShaderParameterType::Scalar),
        ],
    );
    let texture_slots = texture_slots_by_name(
        "pill_default_lit",
        [
            ShaderTextureSlot::new("color", TextureType::Color, (0, 1)),
            ShaderTextureSlot::new("normal", TextureType::Normal, (2, 3)),
        ],
    );
    let shader = RendererShader::new(
        "pill_default_lit",
        &state.device,
        state.surface.format(),
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

/// The wgpu objects behind a [`Renderer`]: device, surface, and frame state.
///
/// Owns the device and queue, the [`Surface`] built from them, the depth
/// texture, and the offscreen colour targets the current chain declares. Kept
/// beside [`Renderer`] rather than inside it so the GPU lifetime - new, resize,
/// reconfigure - has a home independent of the asset caches.
pub struct State {
    /// The per-type cache of every GPU resource - shaders, textures, meshes,
    /// materials - and the engine parameters table.
    pub(crate) renderer_resource_storage: RendererResourceStorage,
    /// The window's swapchain, and the colour format every pipeline that
    /// renders to it declares.
    pub(crate) surface: Surface,
    /// The device every GPU object in this module is created from.
    pub(crate) device: wgpu::Device,
    /// The queue every upload and frame submission goes through.
    pub(crate) queue: wgpu::Queue,
    /// Format of the depth buffer shared by the geometry passes.
    pub(crate) depth_format: wgpu::TextureFormat,
    depth_texture: RendererTexture,
    /// The offscreen colour targets the current chain names, by that name.
    ///
    /// Owned here rather than by the passes that write them: two passes name
    /// the same target - one writes it, the next reads it - and a texture owned
    /// by either would be a lifetime the chain cannot express.
    offscreen: HashMap<String, RendererTexture>,
    mesh_drawer: MeshDrawer,
    /// Layout every camera bind group is built from.
    pub(crate) camera_bind_group_layout: wgpu::BindGroupLayout,
}

impl State {
    async fn new<W: RendererWindow + 'static>(window: W, width: u32, height: u32) -> Result<Self> {
        let (surface, device, queue) = Surface::create(window, width, height).await?;
        let depth_format = wgpu::TextureFormat::Depth32Float;
        let depth_texture =
            RendererTexture::new_depth_texture(&device, surface.configuration(), "depth_texture")?;
        // Scoped like every other creation: a refused layout would otherwise
        // reach the uncaptured-error handler and take the host down before a
        // `RendererError` exists to explain it.
        let camera_bind_group_layout = capturing_validation(&device, || {
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
            })
        })
        .map_err(|detail| RendererError::Other {
            detail: format!("camera bind group layout: {detail}"),
        })?;
        let renderer_resource_storage = RendererResourceStorage::new(&device, &queue)?;
        let mesh_drawer = MeshDrawer::new(&device, INSTANCE_BATCH_SIZE as u32);
        Ok(Self {
            renderer_resource_storage,
            surface,
            device,
            queue,
            depth_format,
            depth_texture,
            offscreen: HashMap::new(),
            mesh_drawer,
            camera_bind_group_layout,
        })
    }

    /// Resize the surface and rebuild the depth buffer that matches it.
    ///
    /// Only committed once the surface accepted the new size, so a refusal
    /// leaves the old, consistent surface and depth pair in place instead of a
    /// configuration that disagrees with the swapchain. What used to be an
    /// `expect` here killed the host on a resize it could not honour.
    fn resize(&mut self, new_window_size: winit::dpi::PhysicalSize<u32>) -> Result<()> {
        self.surface
            .resize(&self.device, new_window_size.width, new_window_size.height)?;
        self.depth_texture = RendererTexture::new_depth_texture(
            &self.device,
            self.surface.configuration(),
            "depth_texture",
        )?;
        Ok(())
    }

    /// Create a target for every offscreen output the chain declares.
    ///
    /// Rebuilt whenever the chain is, and sized to the surface: one that no
    /// longer matches the window would be sampled at a different scale from the
    /// pass that wrote it, which shows up as a picture that shrinks with the
    /// window rather than one that resizes with it.
    fn ensure_offscreen_targets(&mut self, chain: &[ResolvedPass]) {
        let (width, height) = self.surface.size();
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
                if targets.contains_key(name) {
                    continue;
                }
                match RendererTexture::new_render_target(
                    device,
                    name,
                    (width / scale).max(1),
                    (height / scale).max(1),
                    OFFSCREEN_FORMAT,
                ) {
                    Ok(target) => {
                        targets.insert(name.clone(), target);
                    }
                    // Reported instead of fatal: the passes that read it each
                    // say they are not drawn, which is the same outcome their
                    // own failure path would produce.
                    Err(error) => {
                        pill_core::warn!(
                            target: pill_core::telemetry::telemetry_target::RENDERING,
                            "offscreen target `{name}` was not created: {error}"
                        );
                    }
                }
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
        // Both borrows are of separate fields, so the surface can reconfigure
        // itself against the device that created it.
        let surface_frame = self.surface.acquire(&self.device)?;
        let (width, height) = self.surface.size();
        let view = surface_frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        self.renderer_resource_storage.engine_parameters.update(
            &self.queue,
            0.0,
            [0.0; 3],
            [width, height],
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
            .and_then(|value| value.clamped_to(width, height))
            .unwrap_or_else(|| RenderViewport::full(width, height));
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
        // The last creation-class failure a frame can hit: wgpu validates a
        // command buffer when it is submitted, and a validation failure would
        // otherwise reach the uncaptured-error handler as a panic.
        capturing_validation(&self.device, || {
            self.queue.submit(std::iter::once(encoder.finish()));
        })
        .map_err(|detail| RendererError::Other {
            detail: format!("frame submission: {detail}"),
        })?;
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
