//! Cooked asset readers and CPU-side renderer asset storage.
//!
//! # Responsibilities
//!
//! - Decodes mesh, texture, material, and shader payloads from owned bytes.
//! - Maps stable logical IDs to the existing engine's generational asset handles.
//! - Queues complete manifest generations without exposing filesystem paths to the GPU.
//!
//! # Design
//!
//! Logical names are hashed independently of generation directories, so scene
//! references survive a recook. Runtime decoding accepts bytes through an
//! [`AssetSource`]; the native directory source is just one implementation.
//!
//! [`RenderAssets`] holds decoded CPU values, while GPU allocations live in the
//! backend. Extraction builds a candidate snapshot before publishing a batch,
//! so an invalid payload cannot replace only part of the active generation.

// External crates
use pill_engine::Resource;
use serde::{Deserialize, Serialize};

// Standard library
use std::collections::BTreeMap;

// =============================================================================
// Decoded Asset Types
// =============================================================================

/// Validated RMSH v1 triangle mesh in its cooked vertex layout.
#[derive(Clone, Debug)]
pub struct MeshData {
    /// Fourteen floats per vertex: position3, UV2, normal3, tangent3, bitangent3.
    pub vertices: Vec<[f32; 14]>,
    /// Triangle-list indices into `vertices`, checked during decoding.
    pub indices: Vec<u32>,
}

/// Decoded RTEX pixels, retained on the CPU for later upload or cache rebuild.
#[derive(Clone, Debug)]
pub struct TextureData {
    /// Width of the base mip in texels.
    pub width: u32,
    /// Height of the base mip in texels.
    pub height: u32,
    /// True for little-endian RGBA32Float payloads; false for RGBA8 bytes.
    pub float: bool,
    /// Explicit color-space tag for v5 textures; legacy versions have no tag.
    pub srgb: Option<bool>,
    /// Tightly packed mip payloads, largest first, with each axis halved to at least one.
    pub mips: Vec<Vec<u8>>,
}

/// Metallic-roughness material factors and stable texture references.
///
/// Missing JSON fields use neutral defaults; unknown fields are rejected.
/// Texture ID zero selects the neutral map for that material role.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Material {
    /// Linear RGBA factor multiplied by the entity tint and albedo sample.
    pub base_color: [f32; 4],
    /// Metallic factor multiplied by the metallic-roughness map's blue channel.
    pub metallic: f32,
    /// Perceptual roughness factor multiplied by the map's green channel.
    pub roughness: f32,
    /// Linear RGB emission multiplier.
    pub emissive: [f32; 3],
    /// Discard fragments below this alpha; zero disables normal alpha masking.
    pub alpha_cutoff: f32,
    /// Base-color texture ID, sampled with sRGB decoding.
    pub albedo: u64,
    /// Tangent-space normal texture ID, sampled as linear data.
    pub normal: u64,
    /// Packed material map ID: green is roughness and blue is metallic.
    pub metallic_roughness: u64,
    /// Ambient occlusion texture ID, using its linear red channel.
    pub occlusion: u64,
    /// Emission texture ID, sampled with sRGB decoding.
    pub emissive_map: u64,
}

impl Default for Material {
    fn default() -> Self {
        Self {
            base_color: [1.0; 4],
            metallic: 0.0,
            roughness: 1.0,
            emissive: [0.0; 3],
            alpha_cutoff: 0.0,
            albedo: 0,
            normal: 0,
            metallic_roughness: 0,
            occlusion: 0,
            emissive_map: 0,
        }
    }
}

/// CPU snapshot of one accepted set of runtime assets.
#[derive(Clone, Debug, Default)]
pub struct RenderAssets {
    /// Decoded meshes keyed by logical asset ID.
    pub meshes: AssetTable<MeshData>,
    /// Decoded textures keyed by logical asset ID.
    pub textures: AssetTable<TextureData>,
    /// Decoded material descriptions keyed by logical asset ID.
    pub materials: AssetTable<Material>,
    /// UTF-8 WGSL source keyed by logical path for named shader overrides.
    pub shaders: BTreeMap<String, String>,
    /// Wrapping change counter used to invalidate renderer-owned upload caches.
    pub revision: u64,
    /// Last loaded texture whose name identifies an equirectangular background.
    pub environment: u64,
    /// Last loaded texture whose name identifies diffuse irradiance.
    pub diffuse_ibl: u64,
    /// Last loaded texture whose name identifies prefiltered reflections.
    pub specular_ibl: u64,
    /// Last loaded split-sum BRDF lookup texture.
    pub brdf_lut: u64,
}

// =============================================================================
// Asset Transport
// =============================================================================

/// Shared transport only. The host owns decoded assets and GPU caches.
#[derive(Default)]
#[repr(C)]
pub struct RenderAssetRequests {
    /// Cooked bytes keyed by logical name; later inserts replace a pending value.
    pub pending: BTreeMap<String, Vec<u8>>,
    /// Replace the entire snapshot, including deletions; an empty batch clears it.
    pub replace_all: bool,
}

impl Resource for RenderAssetRequests {
    fn shared_name() -> Option<&'static str> {
        Some("pill_master_renderer::RenderAssetRequests")
    }
}

impl RenderAssetRequests {
    /// Queue bytes for decoding at the next rendering-system extraction.
    pub fn insert(&mut self, name: &str, bytes: Vec<u8>) {
        self.pending.insert(name.to_owned(), bytes);
    }
}

// =============================================================================
// Identity and Binary Readers
// =============================================================================

/// Hash a logical asset name using stable 64-bit FNV-1a.
///
/// Names are case-sensitive and are not normalized here. Manifest producers
/// use forward slashes and omit generation directories before hashing.
pub fn asset_id(name: &str) -> u64 {
    name.bytes().fold(0xcbf29ce484222325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
    })
}

/// Read one checked little-endian header word from a cooked payload.
fn word(bytes: &[u8], offset: usize) -> Result<u32, String> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or("truncated asset")?
            .try_into()
            .unwrap(),
    ))
}

impl RenderAssets {
    /// Decode one asset, update automatic environment selection, and bump revision.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported suffixes, invalid binary payloads, invalid
    /// material JSON, or shader bytes that are not UTF-8. WGSL validation is deferred
    /// to pipeline creation by the GPU backend.
    pub fn load(&mut self, name: &str, bytes: &[u8]) -> Result<(), String> {
        let id = asset_id(name);
        if name.ends_with(".cooked_mesh") {
            self.meshes.insert(id, decode_mesh(bytes)?);
        } else if name.ends_with(".cooked_tex") {
            self.textures.insert(id, decode_texture(bytes)?);
            if name.contains("_equirect.") {
                self.environment = id;
            }
            if name.contains("_diffuse_ibl.") {
                self.diffuse_ibl = id;
            }
            if name.contains("_specular_ibl.") {
                self.specular_ibl = id;
            }
            if name.ends_with("brdf_lut.cooked_tex") {
                self.brdf_lut = id;
            }
        } else if name.ends_with(".material") {
            self.materials.insert(
                id,
                serde_json::from_slice(bytes).map_err(|e| e.to_string())?,
            );
        } else if name.ends_with(".wgsl") {
            self.shaders.insert(
                name.to_string(),
                std::str::from_utf8(bytes)
                    .map_err(|e| e.to_string())?
                    .to_owned(),
            );
        } else {
            return Err(format!("unsupported runtime asset: {name}"));
        }
        self.revision = self.revision.wrapping_add(1);
        Ok(())
    }
}

/// Decode RMSH v1 without relying on host alignment or native struct casts.
///
/// # Errors
///
/// Rejects unknown versions, incorrect sizes, non-finite vertices, incomplete
/// triangles, and indices outside the vertex array.
pub fn decode_mesh(b: &[u8]) -> Result<MeshData, String> {
    if b.get(..4) != Some(b"RMSH") || word(b, 4)? != 1 {
        return Err("unsupported RMSH format/version".into());
    }
    let nv = word(b, 8)? as usize;
    let ni = word(b, 12)? as usize;
    let expected = nv
        .checked_mul(56)
        .and_then(|v| ni.checked_mul(4).and_then(|i| v.checked_add(i)))
        .and_then(|n| n.checked_add(16))
        .ok_or("mesh size overflow")?;
    if b.len() != expected || ni % 3 != 0 {
        return Err("invalid mesh payload size".into());
    }
    let mut vertices = Vec::with_capacity(nv);
    for chunk in b[16..16 + nv * 56].chunks_exact(56) {
        let mut v = [0.0; 14];
        for (i, f) in v.iter_mut().enumerate() {
            *f = f32::from_le_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap());
            if !f.is_finite() {
                return Err("non-finite vertex".into());
            }
        }
        vertices.push(v);
    }
    let mut indices = Vec::with_capacity(ni);
    for c in b[16 + nv * 56..].chunks_exact(4) {
        let i = u32::from_le_bytes(c.try_into().unwrap());
        if i as usize >= nv {
            return Err("mesh index out of bounds".into());
        }
        indices.push(i);
    }
    Ok(MeshData { vertices, indices })
}

/// Decode RTEX v1/v2 single images and v4/v5 mip chains.
///
/// # Errors
///
/// Rejects unsupported versions, invalid extents or mip counts, truncated or
/// trailing bytes, unknown color-space tags, and non-finite float channels.
pub fn decode_texture(b: &[u8]) -> Result<TextureData, String> {
    if b.get(..4) != Some(b"RTEX") {
        return Err("invalid RTEX magic".into());
    }
    let version = word(b, 4)?;
    let width = word(b, 8)?;
    let height = word(b, 12)?;
    if width == 0 || height == 0 || width > 16384 || height > 16384 {
        return Err("invalid texture extent".into());
    }
    let (float, count, mut offset) = match version {
        1 => (false, 1, 16usize),
        2 => (true, 1, 16),
        4 => (true, word(b, 16)?, 20),
        5 => (false, word(b, 16)?, 24),
        _ => return Err("unsupported RTEX version".into()),
    };
    let srgb = if version == 5 {
        match word(b, 20)? {
            0 => Some(false),
            1 => Some(true),
            _ => return Err("invalid texture color space".into()),
        }
    } else {
        None
    };
    if count == 0 || count > width.max(height).ilog2() + 1 {
        return Err("invalid mip count".into());
    }
    let mut mips = Vec::new();
    for mip in 0..count {
        let size = (width >> mip).max(1) as usize
            * (height >> mip).max(1) as usize
            * if float { 16 } else { 4 };
        let end = offset.checked_add(size).ok_or("texture overflow")?;
        mips.push(b.get(offset..end).ok_or("truncated texture")?.to_vec());
        offset = end;
    }
    if float
        && mips
            .iter()
            .flat_map(|m| m.chunks_exact(4))
            .any(|b| !f32::from_le_bytes(b.try_into().unwrap()).is_finite())
    {
        return Err("non-finite texture value".into());
    }
    if offset != b.len() {
        return Err("unexpected texture bytes".into());
    }
    Ok(TextureData {
        width,
        height,
        float,
        srgb,
        mips,
    })
}

// =============================================================================
// Runtime Sources and Manifests
// =============================================================================

/// Runtime source seam; a future network loader can supply identical cooked bytes.
pub trait AssetSource {
    /// Read a manifest-relative path, reporting source or path-resolution failures.
    fn read(&self, path: &str) -> Result<Vec<u8>, String>;
}

/// Native asset source rooted at one directory.
///
/// Canonical path checks reject reads through traversal or symlinks that resolve
/// outside the root. Missing files and filesystem errors are returned to callers.
pub struct DirectorySource(pub std::path::PathBuf);
impl AssetSource for DirectorySource {
    fn read(&self, path: &str) -> Result<Vec<u8>, String> {
        let root = self.0.canonicalize().map_err(|e| e.to_string())?;
        let p = root.join(path).canonicalize().map_err(|e| e.to_string())?;
        if !p.starts_with(root) {
            return Err("asset path escapes root".into());
        }
        std::fs::read(p).map_err(|e| e.to_string())
    }
}

/// Queue a complete cooked manifest, preserving logical asset IDs across generations.
///
/// Reads into a temporary map before changing the request resource. Existing
/// pending requests survive a read failure; a successful manifest replaces them.
/// Payload decoding happens later in the rendering system.
///
/// # Errors
///
/// Returns source errors or rejects malformed manifests, unsupported manifest
/// versions, mismatched declared IDs, and duplicate logical names.
pub fn load_manifest(
    source: &dyn AssetSource,
    requests: &mut RenderAssetRequests,
) -> Result<(), String> {
    let manifest: serde_json::Value =
        serde_json::from_slice(&source.read("manifest.json")?).map_err(|e| e.to_string())?;
    if manifest["version"].as_u64() != Some(1) {
        return Err("unsupported asset manifest".into());
    }
    let mut pending = BTreeMap::new();
    for a in manifest["assets"].as_array().ok_or("missing assets")? {
        let path = a["path"].as_str().ok_or("asset path missing")?;
        let name = a["name"].as_str().ok_or("asset name missing")?;
        if let Some(id) = a["id"].as_str() {
            if id != format!("{:016x}", asset_id(name)) {
                return Err(format!("manifest identity mismatch: {name}"));
            }
        }
        if pending
            .insert(name.to_string(), source.read(path)?)
            .is_some()
        {
            return Err(format!("duplicate asset name: {name}"));
        }
    }
    requests.pending = pending;
    requests.replace_all = true;
    Ok(())
}

// =============================================================================
// Generational Asset Storage
// =============================================================================

impl pill_engine::Asset for MeshData {}
impl pill_engine::Asset for TextureData {}
impl pill_engine::Asset for Material {}
trait_type_map::impl_trait_accessible!(dyn pill_engine::Asset; MeshData, TextureData, Material);
/// CPU assets use the existing engine store and reject stale generations.
/// Only stable IDs cross project-artifact boundaries.
pub struct AssetTable<
    T: pill_engine::Asset + trait_type_map::TraitAccessible<dyn pill_engine::Asset>,
> {
    /// Engine store that owns values and tracks handle generations.
    store: pill_engine::AssetManager,
    /// Stable logical IDs mapped to handles local to this particular store.
    handles: BTreeMap<u64, pill_engine::Handle<T>>,
}

impl<T: pill_engine::Asset + trait_type_map::TraitAccessible<dyn pill_engine::Asset>> Default
    for AssetTable<T>
{
    fn default() -> Self {
        Self {
            store: Default::default(),
            handles: Default::default(),
        }
    }
}

impl<T: pill_engine::Asset + trait_type_map::TraitAccessible<dyn pill_engine::Asset>>
    AssetTable<T>
{
    /// Replace an asset and invalidate its previous generational handle.
    pub fn insert(&mut self, id: u64, asset: T) {
        if let Some(old) = self.handles.remove(&id) {
            self.store.remove(old);
        }
        let handle = self
            .store
            .add_with_guid(pill_engine::AssetGuid::new(id as u128), asset);
        self.handles.insert(id, handle);
    }

    /// Resolve a stable ID, returning `None` for an absent or stale handle.
    pub fn get(&self, id: &u64) -> Option<&T> {
        self.handles.get(id).and_then(|h| self.store.get(*h))
    }

    /// Visit live entries in stable ID order, skipping any stale handles.
    pub fn iter(&self) -> impl Iterator<Item = (&u64, &T)> {
        self.handles
            .iter()
            .filter_map(|(id, h)| self.store.get(*h).map(|v| (id, v)))
    }
}

impl<T: pill_engine::Asset + trait_type_map::TraitAccessible<dyn pill_engine::Asset> + Clone> Clone
    for AssetTable<T>
{
    fn clone(&self) -> Self {
        let mut result = Self::default();
        for (id, value) in self.iter() {
            result.insert(*id, value.clone());
        }
        result
    }
}

impl<T: pill_engine::Asset + trait_type_map::TraitAccessible<dyn pill_engine::Asset>>
    std::fmt::Debug for AssetTable<T>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssetTable")
            .field("count", &self.handles.len())
            .finish()
    }
}

// =============================================================================
// Color-Correct Mip Generation
// =============================================================================

/// Generate an RGBA8 mip chain from the base image using a 2-by-2 box filter.
///
/// The caller must supply a valid nonempty RGBA8 image. Color channels are
/// decoded to linear light before averaging when `srgb` is true, then encoded
/// again; alpha is always averaged directly. Existing lower mips are replaced.
#[cfg(any(feature = "gpu", feature = "asset-cooking"))]
pub(crate) fn generate_mips(base: &TextureData, srgb: bool) -> TextureData {
    let mut output = base.clone();
    output.mips.truncate(1);
    output.srgb = Some(srgb);
    let mut width = base.width;
    let mut height = base.height;
    while width > 1 || height > 1 {
        let nw = (width / 2).max(1);
        let nh = (height / 2).max(1);
        let old = output.mips.last().unwrap();
        let mut next = Vec::with_capacity((nw * nh * 4) as usize);
        for y in 0..nh {
            for x in 0..nw {
                for c in 0..4 {
                    let mut sum = 0.0f32;
                    for dy in 0..2 {
                        for dx in 0..2 {
                            let px = (2 * x + dx).min(width - 1);
                            let py = (2 * y + dy).min(height - 1);
                            let v = old[((py * width + px) * 4 + c) as usize] as f32 / 255.0;
                            sum += if srgb && c < 3 {
                                if v <= 0.04045 {
                                    v / 12.92
                                } else {
                                    ((v + 0.055) / 1.055).powf(2.4)
                                }
                            } else {
                                v
                            };
                        }
                    }
                    let v = sum * 0.25;
                    let v = if srgb && c < 3 {
                        if v <= 0.0031308 {
                            v * 12.92
                        } else {
                            1.055 * v.powf(1.0 / 2.4) - 0.055
                        }
                    } else {
                        v
                    };
                    next.push((v.clamp(0.0, 1.0) * 255.0).round() as u8);
                }
            }
        }
        output.mips.push(next);
        width = nw;
        height = nh;
    }
    output
}
