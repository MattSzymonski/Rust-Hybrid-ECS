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
//! caches by key, so every sync walks the store's column for its own type - the
//! only way back from a cached key to whether the asset behind it is still live.
//!
//! Failures are per-asset, never fatal: a texture that will not upload, a shader
//! that will not compile, a material whose pipeline will not build is named in
//! the log and skipped, and the unrecorded version is what makes the next
//! revision try it again.

// Standard library
use std::collections::{HashMap, HashSet};

// External crates
use pill_engine::{AssetManager, Handle};

// Current crate
use crate::{
    assets::{
        asset_key, slots_by_name, Material, MaterialParameter, Mesh, Shader, ShaderParameterSlot,
        ShaderParameterType, ShaderTextureKind, ShaderTextureSlot, Texture,
    },
    chain::ChainContext,
    error::Result,
    frame::RenderFrame,
    render_queue::{compose_render_queue_key, RenderQueueItem},
    renderer::State,
    resources::{
        RendererMaterial, RendererMaterialHandle, RendererMesh, RendererMeshHandle, RendererShader,
        RendererShaderHandle, RendererTexture, RendererTextureHandle, UniformGroups, Vertex,
    },
    Instance,
};

/// One asset type's GPU objects, keyed by asset key.
///
/// An entry is the handle of the object on the GPU plus the content version it
/// was built from. `None` for the version is the fourth state the sync needs:
/// the handle is still there, but the object behind it is stale, so the next sync
/// rebuilds it - which is what a material whose shader or texture moved asks for
/// without its own content changing.
pub(crate) struct GpuColumn<H> {
    entries: HashMap<u64, (H, Option<u64>)>,
}

impl<H: Copy> GpuColumn<H> {
    /// A column with nothing mirrored.
    pub(crate) fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// The object behind `key`, when one is there.
    pub(crate) fn handle(&self, key: u64) -> Option<H> {
        self.entries.get(&key).map(|(handle, _)| *handle)
    }

    /// Whether `key`'s object was built from content version `version`.
    fn is_current(&self, key: u64, version: u64) -> bool {
        matches!(self.entries.get(&key), Some((_, Some(built))) if *built == version)
    }

    /// Records `handle` as `key`'s object, built from `version`.
    fn insert(&mut self, key: u64, handle: H, version: u64) {
        self.entries.insert(key, (handle, Some(version)));
    }

    /// Takes `key`'s object out, for the rebuild that replaces or drops it.
    fn remove(&mut self, key: u64) -> Option<H> {
        self.entries.remove(&key).map(|(handle, _)| handle)
    }

    /// Keeps `key`'s handle but forgets the version, so the next sync rebuilds
    /// it against whatever the object now depends on.
    fn invalidate(&mut self, key: u64) {
        if let Some((_, version)) = self.entries.get_mut(&key) {
            *version = None;
        }
    }

    /// Every key the column holds.
    fn keys(&self) -> impl Iterator<Item = u64> + '_ {
        self.entries.keys().copied()
    }

    /// Forgets every object's version, keeping the handles: the next sync
    /// rebuilds all of them.
    fn invalidate_all(&mut self) {
        for (_, version) in self.entries.values_mut() {
            *version = None;
        }
    }
}

/// The GPU objects behind the asset store, and what they were built from.
pub(crate) struct RenderingResourcesManager {
    /// The store revision these caches are level with, or `None` after an
    /// explicit invalidation; the sync runs when it does not match the store's.
    synced_revision: Option<u64>,
    /// Content version of every asset the GPU object behind a key was built
    /// from, one column per asset type. A key whose version moved - or that is
    /// not recorded here at all - is rebuilt; a key whose version matches keeps
    /// its object.
    shaders: GpuColumn<RendererShaderHandle>,
    materials: GpuColumn<RendererMaterialHandle>,
    textures: GpuColumn<RendererTextureHandle>,
    meshes: GpuColumn<RendererMeshHandle>,
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
            shaders: GpuColumn::new(),
            materials: GpuColumn::new(),
            textures: GpuColumn::new(),
            meshes: GpuColumn::new(),
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
        self.textures.invalidate_all();
        self.shaders.invalidate_all();
        self.meshes.invalidate_all();
        self.materials.invalidate_all();
    }

    /// One queue item per drawable instance, sorted for recording.
    ///
    /// An instance whose mesh never uploaded is skipped; one whose material
    /// never built falls back to the default material, and a material with no
    /// shader of its own to the default shader. A partially-broken revision
    /// therefore still draws something, which is what the two defaults are for.
    pub(crate) fn build_queue(&self, frame: &RenderFrame, state: &State) -> Vec<RenderQueueItem> {
        let resolve_zone = pill_core::profile_scope!("build queue: resolve instances");
        let mut queue = Vec::with_capacity(frame.instances.len());
        // The last mesh and material keys resolved, with what they resolved to.
        // Instances that share both are usually many and adjacent (one
        // archetype's worth), so remembering one pair skips nearly every
        // lookup - and those lookups are the whole of this function's time once
        // thousands of instances share one mesh and material. Nothing changes
        // the caches during the call, so a remembered answer is the answer a
        // fresh lookup would give.
        let mut last_resolved = None;
        for (index, instance) in frame.instances.iter().enumerate() {
            let resolved = match last_resolved {
                Some((mesh_key, material_key, resolved))
                    if mesh_key == instance.mesh && material_key == instance.material =>
                {
                    resolved
                }
                _ => {
                    // The renderer's own caches key by the raw value; the frame
                    // names it as a typed key.
                    let resolved =
                        self.resolve_draw_handles(instance.mesh.0, instance.material.0, state);
                    last_resolved = Some((instance.mesh, instance.material, resolved));
                    resolved
                }
            };
            let Some((shader, material, mesh)) = resolved else {
                continue;
            };
            queue.push(RenderQueueItem {
                key: compose_render_queue_key(instance.rendering_order, shader, material, mesh),
                frame_instance_index: index as u32,
                shader,
                material,
                mesh,
            });
        }
        drop(resolve_zone);
        let _sort_zone = pill_core::profile_scope!("build queue: sort");
        queue.sort_unstable();
        queue
    }

    /// The shader, material and mesh objects one instance draws with, or `None`
    /// when its mesh never uploaded.
    ///
    /// A material that never built falls back to the default material, and a
    /// material with no shader of its own to the default shader.
    fn resolve_draw_handles(
        &self,
        mesh_key: u64,
        material_key: u64,
        state: &State,
    ) -> Option<(
        RendererShaderHandle,
        RendererMaterialHandle,
        RendererMeshHandle,
    )> {
        let mesh = self.meshes.handle(mesh_key)?;
        let material = self
            .materials
            .handle(material_key)
            .unwrap_or(self.default_material);
        let shader = state
            .renderer_resource_storage
            .materials
            .get(material)
            .map(|material| material.shader_handle)
            .unwrap_or(self.default_shader);
        Some((shader, material, mesh))
    }

    /// The shader column, for naming a pass's shader in the queue.
    pub(crate) fn shader_handles(&self) -> &GpuColumn<RendererShaderHandle> {
        &self.shaders
    }

    /// What the chain needs of these caches to build its GPU objects.
    ///
    /// The chain reads the shader and texture caches by key and the epoch that
    /// moves when one of them is rebuilt, beside the surface it is built for;
    /// bundling them here keeps the borrow split in one place instead of at
    /// every call site. The surface arrives as the pieces the chain reads -
    /// its format, its size, its depth buffer and its offscreen targets -
    /// rather than as the whole target, so the chain they belong to can be
    /// rebuilt at the same time.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn chain_context<'a>(
        &'a self,
        state: &'a mut State,
        surface_format: wgpu::TextureFormat,
        size: (u32, u32),
        depth_texture: &'a RendererTexture,
        offscreen: &'a mut HashMap<String, RendererTexture>,
    ) -> ChainContext<'a> {
        ChainContext {
            state,
            surface_format,
            size,
            depth_texture,
            offscreen,
            shader_handles: &self.shaders,
            texture_handles: &self.textures,
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
                self.materials.invalidate(asset_key(handle));
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
            if self.textures.is_current(key, version) {
                continue;
            }
            // Recorded before the rebuild: even a failed creation dropped the
            // texture the old materials were built against.
            self.changed.insert(key);
            if let Some(old) = self.textures.remove(key) {
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
                    self.textures.insert(key, gpu, version);
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
            .textures
            .keys()
            .filter(|key| !self.live.contains(key))
            .collect();
        for key in removed {
            if let Some(gpu) = self.textures.remove(key) {
                state.renderer_resource_storage.textures.remove(gpu);
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
            if self.shaders.is_current(key, version) {
                continue;
            }
            self.changed.insert(key);
            if let Some(old) = self.shaders.remove(key) {
                state.renderer_resource_storage.shaders.remove(old);
            }
            let value = RendererShader::new(
                &shader.name,
                &state.device,
                state.surface_format,
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
                UniformGroups {
                    engine: shader.pass_engine_parameters,
                    camera: shader.pass_camera_parameters,
                },
            );
            match value {
                Ok(value) => {
                    let gpu = state.renderer_resource_storage.shaders.insert(value);
                    self.shaders.insert(key, gpu, version);
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
            .shaders
            .keys()
            .filter(|key| !self.live.contains(key))
            .collect();
        for key in removed {
            if let Some(gpu) = self.shaders.remove(key) {
                state.renderer_resource_storage.shaders.remove(gpu);
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
            if self.meshes.is_current(key, version) {
                continue;
            }
            if let Some(old) = self.meshes.remove(key) {
                state.renderer_resource_storage.meshes.remove(old);
            }
            match RendererMesh::new(&state.device, &mesh.name, mesh) {
                Ok(value) => {
                    let gpu = state.renderer_resource_storage.meshes.insert(value);
                    self.meshes.insert(key, gpu, version);
                }
                Err(error) => pill_core::warn!(
                    target: pill_core::telemetry::telemetry_target::RENDERING,
                    "mesh `{}` is not on the GPU: {error}",
                    mesh.name
                ),
            }
        }

        let removed: Vec<u64> = self
            .meshes
            .keys()
            .filter(|key| !self.live.contains(key))
            .collect();
        for key in removed {
            if let Some(gpu) = self.meshes.remove(key) {
                state.renderer_resource_storage.meshes.remove(gpu);
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
            if self.materials.is_current(key, version) {
                continue;
            }
            if let Some(old) = self.materials.remove(key) {
                state.renderer_resource_storage.materials.remove(old);
            }
            let shader_key = asset_key(material.shader);
            let shader = if material.shader == Handle::INVALID {
                // A material built without a shader is documented to draw with
                // the renderer's own; that is a choice, not a fault.
                self.default_shader
            } else if let Some(gpu) = self.shaders.handle(shader_key) {
                gpu
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
                    self.textures
                        .handle(asset_key(texture.texture))
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
                    self.materials.insert(key, gpu, version);
                }
                Err(error) => pill_core::warn!(
                    target: pill_core::telemetry::telemetry_target::RENDERING,
                    "material `{}` has no pipeline: {error}",
                    material.name
                ),
            }
        }

        let removed: Vec<u64> = self
            .materials
            .keys()
            .filter(|key| !self.live.contains(key))
            .collect();
        for key in removed {
            if let Some(gpu) = self.materials.remove(key) {
                state.renderer_resource_storage.materials.remove(gpu);
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
/// from `shader_sources/common_shaders/default_vertex.wgsl`, the fragment from
/// `shader_sources/simple_pipeline/shaders/default_lit_fragment.wgsl`.
/// `simple_pipeline` names the same shader with the same declared slots, so a
/// material written for this fallback draws there without being rebuilt.
fn install_default_material(
    state: &mut State,
) -> Result<(RendererShaderHandle, RendererMaterialHandle)> {
    let parameter_slots = slots_by_name(
        "pill_default_lit",
        "parameter",
        [
            ShaderParameterSlot::new("tint", ShaderParameterType::Color),
            ShaderParameterSlot::new("specularity", ShaderParameterType::Scalar),
        ],
    );
    let texture_slots = slots_by_name(
        "pill_default_lit",
        "texture",
        [
            ShaderTextureSlot::new("color", ShaderTextureKind::Color, (0, 1)),
            ShaderTextureSlot::new("normal", ShaderTextureKind::Normal, (2, 3)),
        ],
    );
    let shader = RendererShader::new(
        "pill_default_lit",
        &state.device,
        state.surface_format,
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
        UniformGroups {
            engine: true,
            camera: true,
        },
    )?;
    let shader = state.renderer_resource_storage.shaders.insert(shader);
    let parameters = std::collections::BTreeMap::from([
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The fourth state the sync needs: a material whose shader or texture moved
    /// keeps its handle but loses its version, so the next sync rebuilds it
    /// against the new object.
    #[test]
    fn forgetting_a_version_keeps_the_handle_and_asks_for_a_rebuild() {
        let mut column: GpuColumn<u32> = GpuColumn::new();
        column.insert(7, 42, 3);
        assert!(column.is_current(7, 3));

        column.invalidate(7);

        assert_eq!(column.handle(7), Some(42), "the handle stays");
        assert!(!column.is_current(7, 3), "and the next sync rebuilds it");
    }

    /// A column hands back the object it drops, which is what lets the sync
    /// remove it from the resource storage.
    #[test]
    fn removing_an_entry_hands_back_its_handle() {
        let mut column: GpuColumn<u32> = GpuColumn::new();
        column.insert(1, 10, 0);

        assert_eq!(column.remove(1), Some(10));
        assert_eq!(column.handle(1), None);
        assert_eq!(column.remove(1), None, "removing twice is harmless");
    }

    /// Versions are compared per key, so one asset moving leaves its
    /// neighbours' objects alone.
    #[test]
    fn a_moved_version_only_touches_its_own_key() {
        let mut column: GpuColumn<u32> = GpuColumn::new();
        column.insert(1, 10, 0);
        column.insert(2, 20, 0);

        column.insert(1, 11, 1);

        assert!(column.is_current(1, 1));
        assert!(!column.is_current(1, 0));
        assert!(column.is_current(2, 0), "its neighbour is untouched");
        assert_eq!(column.handle(2), Some(20));
    }
}
