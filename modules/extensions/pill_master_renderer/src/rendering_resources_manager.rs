//! The GPU objects behind the asset store, kept level with it one type at a
//! time.
//!
//! # Responsibilities
//!
//! - Hold one GPU object per live asset - shader, texture, mesh, material - and
//!   the content version it was built from.
//! - Bring those objects level with the store when its revision moves, and only
//!   what moved: one edited texture is re-uploaded while its neighbours keep the
//!   objects they already have, and a removed asset frees its slot.
//! - Own the resource epoch: the counter that moves whenever a shader or texture
//!   object changes, which is what tells the chain its bind groups are stale.
//! - Resolve a frame's instances to the GPU handles the draw path needs, and
//!   hold the defaults an asset that names nothing falls back to.
//!
//! # Design
//!
//! A mirror, not a cache: `AssetManager` looks assets up by handle and this
//! caches by key, so every sync walks the store's column for its own type. That
//! walk is the only way back from a cached key to whether the asset behind it is
//! still live - a key with no asset is a removal, and its object goes with it.
//!
//! Failures are per-asset, never fatal. A texture that will not upload, a shader
//! that will not compile, a material whose pipeline will not build: each is
//! named in the log and skipped, and the rest of the revision still reaches the
//! GPU. A running game losing one asset's picture is a better outcome than the
//! frame not happening at all, and leaving the version unrecorded is what makes
//! the next revision try that one asset again.

// Standard library
use std::collections::{HashMap, HashSet};

// External crates
use pill_engine::{AssetManager, Handle};

// Current crate
use crate::{
    assets::{
        asset_key, parameter_slots_by_name, texture_slots_by_name, Material, MaterialParameter,
        Mesh, Shader, ShaderParameterSlot, ShaderParameterType, ShaderTextureSlot, Texture,
        TextureType,
    },
    error::Result,
    frame::RenderFrame,
    pipeline::ChainContext,
    render_queue::{compose_render_queue_key, RenderQueueItem},
    renderer::State,
    resources::{
        RendererMaterial, RendererMaterialHandle, RendererMesh, RendererMeshHandle, RendererShader,
        RendererShaderHandle, RendererTexture, RendererTextureHandle, Vertex,
    },
    Instance,
};

/// The GPU objects behind the asset store, and what they were built from.
pub(crate) struct RenderingResourcesManager {
    /// The store revision these caches are level with, or `None` after an
    /// explicit invalidation; the sync runs when it does not match the store's.
    synced_revision: Option<u64>,
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
    /// What an asset that names no shader, or no material, draws with.
    default_shader: RendererShaderHandle,
    default_material: RendererMaterialHandle,
    /// Bumped whenever a shader or texture object is created, recreated, or
    /// dropped: pass bind groups reference those objects by handle, so a change
    /// to one is what invalidates them.
    resource_epoch: u64,
    /// Scratch: every key the store still holds for the column being walked.
    ///
    /// One set serves all four columns, because the walks run in sequence and
    /// each clears it first.
    live: HashSet<u64>,
    /// Scratch: the keys whose object this sync rebuilt. Read once, by
    /// `invalidate_dependent_materials`, and meaningless outside the sync that
    /// filled it.
    changed: HashSet<u64>,
}

impl RenderingResourcesManager {
    /// Nothing mirrored yet, plus the two objects the fallbacks need.
    ///
    /// # Errors
    ///
    /// Returns a [`RendererError`](crate::RendererError) when the built-in lit
    /// shader or the material built from it will not build. Every later fallback
    /// depends on those two, so this is the one creation failure here that is
    /// worth refusing to start over.
    pub(crate) fn new(state: &mut State) -> Result<Self> {
        let (default_shader, default_material) = install_default_material(state)?;
        Ok(Self {
            synced_revision: None,
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
            resource_epoch: 0,
            live: HashSet::new(),
            changed: HashSet::new(),
        })
    }

    /// Bring the GPU caches level with the asset store, per asset.
    ///
    /// The manager's revision gates the pass, and its per-asset content versions
    /// decide what inside it moved: one edited texture is re-uploaded while its
    /// neighbours keep their GPU objects, and a removed asset frees its slot.
    /// Creation failures are reported and skipped rather than aborting the pass,
    /// so one bad asset neither holds the rest of the revision hostage nor is
    /// retried every frame.
    ///
    /// The scratch sets it walks with are reused across calls rather than
    /// allocated per column: this runs whenever the store moved, which for a
    /// game that edits assets is every frame.
    pub(crate) fn sync(&mut self, assets: &AssetManager, state: &mut State) {
        if self.synced_revision == Some(assets.revision()) {
            return;
        }
        self.synced_revision = Some(assets.revision());

        // Textures and shaders first: materials bind them by handle. The epoch
        // moves only when one of these two layers did, because those are the
        // objects a pass's bind groups keep - and the materials built against
        // them are invalidated by key, because their own content version alone
        // would not ask for a rebuild.
        self.changed.clear();
        let mut epoch_moved = self.sync_textures(assets, state);
        epoch_moved |= self.sync_shaders(assets, state);
        self.sync_meshes(assets, state);
        if !self.changed.is_empty() {
            self.invalidate_dependent_materials(assets);
        }
        self.sync_materials(assets, state);
        if epoch_moved {
            self.resource_epoch = self.resource_epoch.wrapping_add(1);
        }
    }

    /// Forget which versions the objects were built from, so the next sync
    /// rebuilds every one of them.
    ///
    /// The objects themselves are left in place until that rebuild replaces
    /// them: the sync removes a key's old object as it creates the new one, so
    /// clearing the records is enough to make the whole store reach the GPU
    /// again.
    pub(crate) fn invalidate(&mut self) {
        self.synced_revision = None;
        self.texture_versions.clear();
        self.shader_versions.clear();
        self.mesh_versions.clear();
        self.material_versions.clear();
    }

    /// One queue item per drawable instance, sorted for recording.
    ///
    /// An instance whose mesh never uploaded is skipped; one whose material
    /// never built falls back to the default material, and a material with no
    /// shader of its own to the default shader. A partially-broken revision
    /// therefore still draws something, which is what the two defaults are for.
    pub(crate) fn build_queue(&self, frame: &RenderFrame, state: &State) -> Vec<RenderQueueItem> {
        let mut queue = Vec::with_capacity(frame.instances.len());
        for (index, instance) in frame.instances.iter().enumerate() {
            let Some(mesh) = self.mesh_handles.get(&instance.mesh).copied() else {
                continue;
            };
            let material = self
                .material_handles
                .get(&instance.material)
                .copied()
                .unwrap_or(self.default_material);
            let shader = state
                .renderer_resource_storage
                .materials
                .get(material)
                .map(|material| material.shader_handle)
                .unwrap_or(self.default_shader);
            queue.push(RenderQueueItem {
                key: compose_render_queue_key(instance.rendering_order, shader, material, mesh),
                entity_index: index as u32,
            });
        }
        queue.sort_unstable();
        queue
    }

    /// The shader objects by key, for naming a pass's shader in the queue.
    pub(crate) fn shader_handles(&self) -> &HashMap<u64, RendererShaderHandle> {
        &self.shader_handles
    }

    /// What the chain needs of these caches to build its GPU objects.
    ///
    /// The chain reads the shader and texture caches by key and the epoch that
    /// moves when one of them is rebuilt; bundling them here keeps the borrow
    /// split in one place instead of at every call site.
    pub(crate) fn chain_context<'a>(&'a self, state: &'a mut State) -> ChainContext<'a> {
        ChainContext {
            state,
            shader_handles: &self.shader_handles,
            texture_handles: &self.texture_handles,
            resource_epoch: self.resource_epoch,
        }
    }

    /// Forget every built material that reads an asset just recreated.
    ///
    /// A material's bind groups are built against the layouts and views its
    /// shader and textures had at the time; when one of those is rebuilt or
    /// removed, the groups go stale while the material's own content version
    /// stands still. Dropping the version record is what makes `sync_materials`
    /// rebuild it in the same pass, against the new object.
    fn invalidate_dependent_materials(&mut self, assets: &AssetManager) {
        for (handle, material) in assets.iter_handles::<Material>() {
            let shader_changed = self.changed.contains(&asset_key(material.shader));
            let texture_changed = material
                .textures
                .values()
                .any(|texture| self.changed.contains(&asset_key(texture.texture)));
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
    fn sync_textures(&mut self, assets: &AssetManager, state: &mut State) -> bool {
        let mut epoch_moved = false;
        // Every key the store still holds, collected while the rebuild pass
        // walks it. The mirror caches by key and `AssetManager` looks up by handle,
        // so walking the store is the only way back from a cached key to whether
        // the asset behind it is still live.
        self.live.clear();

        for (handle, texture) in assets.iter_handles::<Texture>() {
            let key = asset_key(handle);
            self.live.insert(key);
            let version = assets.content_version(handle).unwrap_or(0);
            if self.texture_versions.get(&key) == Some(&version) {
                continue;
            }
            // Recorded before the rebuild: even a failed creation dropped the
            // texture the old materials were built against.
            self.changed.insert(key);
            if let Some(old) = self.texture_handles.remove(&key) {
                state.renderer_resource_storage.textures.remove(old);
            }
            match RendererTexture::new_texture(
                &state.device,
                &state.queue,
                Some(&texture.name),
                &texture.rgba,
                texture.width,
                texture.height,
                texture.texture_type,
            ) {
                Ok(value) => {
                    let gpu = state.renderer_resource_storage.textures.insert(value);
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
            .filter(|key| !self.live.contains(key))
            .copied()
            .collect();
        for key in removed {
            if let Some(gpu) = self.texture_handles.remove(&key) {
                state.renderer_resource_storage.textures.remove(gpu);
                self.texture_versions.remove(&key);
                self.changed.insert(key);
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
    fn sync_shaders(&mut self, assets: &AssetManager, state: &mut State) -> bool {
        let mut epoch_moved = false;
        // See `sync_textures`: walking the store is what says which cached keys
        // are still live.
        self.live.clear();

        for (handle, shader) in assets.iter_handles::<Shader>() {
            let key = asset_key(handle);
            self.live.insert(key);
            let version = assets.content_version(handle).unwrap_or(0);
            if self.shader_versions.get(&key) == Some(&version) {
                continue;
            }
            self.changed.insert(key);
            if let Some(old) = self.shader_handles.remove(&key) {
                state.renderer_resource_storage.shaders.remove(old);
            }
            let value = RendererShader::new(
                &shader.name,
                &state.device,
                state.surface.format(),
                Some(state.depth_format),
                &[
                    RendererMesh::data_layout_descriptor(),
                    Instance::data_layout_descriptor(),
                ],
                &shader.vertex_wgsl,
                &shader.fragment_wgsl,
                &shader.parameter_slots,
                &shader.texture_slots,
                &state
                    .renderer_resource_storage
                    .engine_parameters
                    .bind_group_layout,
                &state.camera_bind_group_layout,
                shader.pass_engine_parameters,
                shader.pass_camera_parameters,
            );
            match value {
                Ok(value) => {
                    let gpu = state.renderer_resource_storage.shaders.insert(value);
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
            .filter(|key| !self.live.contains(key))
            .copied()
            .collect();
        for key in removed {
            if let Some(gpu) = self.shader_handles.remove(&key) {
                state.renderer_resource_storage.shaders.remove(gpu);
                self.shader_versions.remove(&key);
                self.changed.insert(key);
                epoch_moved = true;
            }
        }
        epoch_moved
    }

    /// Drop and rebuild the meshes whose asset changed.
    ///
    /// No epoch is needed for meshes: the draw path resolves a mesh handle per
    /// frame, so a rebuilt one is picked up without invalidating any pipeline.
    fn sync_meshes(&mut self, assets: &AssetManager, state: &mut State) {
        self.live.clear();

        for (handle, mesh) in assets.iter_handles::<Mesh>() {
            let key = asset_key(handle);
            self.live.insert(key);
            let version = assets.content_version(handle).unwrap_or(0);
            if self.mesh_versions.get(&key) == Some(&version) {
                continue;
            }
            if let Some(old) = self.mesh_handles.remove(&key) {
                state.renderer_resource_storage.meshes.remove(old);
            }
            match RendererMesh::new(&state.device, &mesh.name, mesh) {
                Ok(value) => {
                    let gpu = state.renderer_resource_storage.meshes.insert(value);
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
            .filter(|key| !self.live.contains(key))
            .copied()
            .collect();
        for key in removed {
            if let Some(gpu) = self.mesh_handles.remove(&key) {
                state.renderer_resource_storage.meshes.remove(gpu);
                self.mesh_versions.remove(&key);
            }
        }
    }

    /// Drop and rebuild the materials whose asset changed.
    fn sync_materials(&mut self, assets: &AssetManager, state: &mut State) {
        self.live.clear();

        for (handle, material) in assets.iter_handles::<Material>() {
            let key = asset_key(handle);
            self.live.insert(key);
            let version = assets.content_version(handle).unwrap_or(0);
            if self.material_versions.get(&key) == Some(&version) {
                continue;
            }
            if let Some(old) = self.material_handles.remove(&key) {
                state.renderer_resource_storage.materials.remove(old);
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
                &state.device,
                &state.queue,
                &state.renderer_resource_storage,
                &material.name,
                shader,
                &textures,
                &material.parameters,
            ) {
                Ok(value) => {
                    let gpu = state.renderer_resource_storage.materials.insert(value);
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
            .filter(|key| !self.live.contains(key))
            .copied()
            .collect();
        for key in removed {
            if let Some(gpu) = self.material_handles.remove(&key) {
                state.renderer_resource_storage.materials.remove(gpu);
                self.material_versions.remove(&key);
            }
        }
    }
}

/// Build the built-in lit shader and a material from it, and return both.
///
/// The material is what a mesh whose material never built draws with, and the
/// shader is what a material that names none draws with, so the two are built
/// together and kept as one pair.
///
/// The lit WGSL is the shipped pipeline's own, not a copy of it: the vertex stage
/// from `config/common_shaders`, the fragment from
/// `config/simple_pipeline/shaders`. `simple_pipeline` names the same shader with
/// the same declared slots, so a material written for this fallback draws there
/// without being rebuilt.
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
        crate::config::DEFAULT_VERTEX.embedded_source,
        crate::config::simple_pipeline::DEFAULT_LIT_FRAGMENT.embedded_source,
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
