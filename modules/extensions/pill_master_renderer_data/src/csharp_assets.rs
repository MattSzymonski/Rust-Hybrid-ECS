//! The C# bridge's asset functions: build the master renderer's assets from
//! what a managed caller supplies.
//!
//! # Responsibilities
//!
//! - Decode a mesh, texture or shader from raw bytes a managed caller supplies,
//!   and build a material from already-loaded handles and per-slot parameters,
//!   inserting each into a world's `AssetManager`.
//! - Import a texture or a mesh from `res` through its metadata file, one
//!   named export per type, through [`pill_engine::asset_ffi`].
//! - Drop the pipeline the world's `RenderingManager` holds, so a project
//!   that loads its own shading styles returns the renderer to its built-in
//!   pass.
//! - Offer each as a named C-ABI function the host's C# bridge finds by name:
//!   always as a `#[no_mangle]` export (each name is unique to this crate, so
//!   a statically linked build pays nothing for it), and through a
//!   [`PillExportDescriptor`] when it is linked statically.
//! - Define the native argument shapes and status codes these functions share
//!   with the managed side.
//!
//! # Design
//!
//! The functions moved here from the C# backend (now
//! `pill_csharp_bridge/src/assets.rs`), because the asset types they build
//! belong to this renderer's data crate and the backend names no renderer data
//! type. The backend keeps the C# entry points:
//! it owns the managed invocation's world, resolves the function by name, and
//! forwards the call with that world as the first argument. The managed API,
//! the argument shapes and the status codes are unchanged.
//!
//! Asset mutation happens during `[EcsStartup]` methods, which run one at a
//! time before the parallel scheduler starts, so the world passed in is used
//! exclusively for the call.

// External crates
use pill_engine::asset_ffi::{import_for_ffi, NativeImportedAsset};
use pill_engine::component_registry::{ExportAddress, PillExportDescriptor};
use pill_engine::{AssetLoader, AssetManager, Handle, World};

// Current crate
use crate::{
    Material, Mesh, RenderingManager, Shader, ShaderParameterSlot, ShaderParameterType,
    ShaderTextureSlot, Texture, TextureType,
};

// =============================================================================
// Shared native argument shapes
// =============================================================================

/// One parameter slot a managed shader declaration supplies.
#[repr(C)]
pub struct NativeShaderParameterSlot {
    pub name: *const u8,
    pub name_len: u32,
    /// `0` scalar, `1` bool, `2` color.
    pub kind: u8,
}

/// One texture slot a managed shader declaration supplies.
///
/// The bound texture is always color-typed: nothing using this bridge yet
/// needs a normal map slot.
#[repr(C)]
pub struct NativeShaderTextureSlot {
    pub name: *const u8,
    pub name_len: u32,
    pub texture_binding: u32,
    pub sampler_binding: u32,
}

/// One texture a managed material declaration binds to a shader slot.
#[repr(C)]
pub struct NativeMaterialTexture {
    pub slot: *const u8,
    pub slot_len: u32,
    pub texture_index: u32,
    pub texture_generation: u32,
}

/// One scalar parameter a managed material declaration sets.
#[repr(C)]
pub struct NativeMaterialScalar {
    pub name: *const u8,
    pub name_len: u32,
    pub value: f32,
}

/// One color parameter a managed material declaration sets.
#[repr(C)]
pub struct NativeMaterialColor {
    pub name: *const u8,
    pub name_len: u32,
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

/// Handle part meaning "absent" - no shader override, no bound texture.
pub const NO_HANDLE: u32 = u32::MAX;

// =============================================================================
// Status codes
// =============================================================================

// `0` is always success; the rest are deliberately distinct from the host's
// resource-access codes so the two are never confused in a log. The managed
// side's table (`Engine.ValidateAssetStatus`) mirrors these.

/// Success.
pub const STATUS_OK: u8 = 0;
/// No managed invocation is active: `world` was null.
pub const STATUS_NO_ACTIVE_SCOPE: u8 = 1;
/// The world has no `AssetManager` resource.
pub const STATUS_ASSET_MANAGER_MISSING: u8 = 2;
/// A supplied string was not valid UTF-8.
pub const STATUS_INVALID_UTF8: u8 = 3;
/// The source data failed to decode, or the asset failed to build.
pub const STATUS_DECODE_FAILED: u8 = 4;
/// A required buffer or output was null.
pub const STATUS_NULL_OUTPUT: u8 = 5;
/// No asset functions are available. The host reports this when no renderer
/// data crate provides them; this module never returns it.
pub const STATUS_RENDERER_UNAVAILABLE: u8 = 6;
/// The name is already bound to a live asset.
pub const STATUS_NAME_IN_USE: u8 = 7;
/// The world has no `RenderingManager` resource. Continues past the import
/// codes (`8`-`13`, `pill_engine::asset_ffi::import_status`) so every code
/// the managed side's table maps stays distinct.
pub const STATUS_RENDER_MANAGER_MISSING: u8 = 14;

// =============================================================================
// Argument readers
// =============================================================================

/// Reads `len` bytes at `pointer` as owned UTF-8, or an empty string for a
/// zero-length argument.
///
/// # Safety
///
/// `pointer` must reference `len` valid, readable bytes for the call's
/// duration, unless `len` is zero.
unsafe fn read_str(pointer: *const u8, len: u32) -> Result<String, u8> {
    if len == 0 {
        return Ok(String::new());
    }
    if pointer.is_null() {
        return Err(STATUS_NULL_OUTPUT);
    }
    // SAFETY: caller contract.
    let slice = unsafe { std::slice::from_raw_parts(pointer, len as usize) };
    std::str::from_utf8(slice)
        .map(str::to_owned)
        .map_err(|_| STATUS_INVALID_UTF8)
}

/// Reads `len` bytes at `pointer` into an owned buffer, or an empty one for a
/// zero-length argument.
///
/// # Safety
///
/// `pointer` must reference `len` valid, readable bytes for the call's
/// duration, unless `len` is zero.
unsafe fn read_bytes(pointer: *const u8, len: u32) -> Result<Vec<u8>, u8> {
    if len == 0 {
        return Ok(Vec::new());
    }
    if pointer.is_null() {
        return Err(STATUS_NULL_OUTPUT);
    }
    // SAFETY: caller contract.
    Ok(unsafe { std::slice::from_raw_parts(pointer, len as usize) }.to_vec())
}

/// Reads `len` elements of `T` at `pointer`, or an empty slice for a
/// zero-length argument.
///
/// # Safety
///
/// `pointer` must reference `len` valid, readable, properly aligned `T`
/// values for the call's duration, unless `len` is zero.
unsafe fn read_slice<'a, T>(pointer: *const T, len: u32) -> Result<&'a [T], u8> {
    if len == 0 {
        return Ok(&[]);
    }
    if pointer.is_null() {
        return Err(STATUS_NULL_OUTPUT);
    }
    // SAFETY: caller contract.
    Ok(unsafe { std::slice::from_raw_parts(pointer, len as usize) })
}

/// Runs `body` against `world`'s `AssetManager`, folding a null world and a
/// missing store into the shared status codes.
///
/// # Safety
///
/// `world` must be null or point at a live `World` no one else is using for
/// the call's duration.
unsafe fn with_assets<R>(
    world: *mut World,
    body: impl FnOnce(&mut AssetManager) -> Result<R, u8>,
) -> Result<R, u8> {
    // SAFETY: the caller's contract: null, or a live world used exclusively.
    let Some(world) = (unsafe { world.as_mut() }) else {
        return Err(STATUS_NO_ACTIVE_SCOPE);
    };
    let Some(assets) = world.get_resource_mut::<AssetManager>() else {
        return Err(STATUS_ASSET_MANAGER_MISSING);
    };
    body(assets)
}

// =============================================================================
// Exported functions
// =============================================================================

/// Decodes a Wavefront OBJ buffer into a mesh and inserts it into `world`'s
/// `AssetManager`, writing the new handle to `out_index`/`out_generation`.
///
/// Returns a status code (see the `STATUS_*` constants).
///
/// # Safety
///
/// `world` must be null or point at a live `World` no one else is using for
/// the call's duration. `name`/`bytes` must reference their declared lengths
/// in readable memory (unless the matching length is zero), and
/// `out_index`/`out_generation` must be writable.
#[no_mangle]
pub unsafe extern "C" fn pill_render_data_load_mesh_obj(
    world: *mut World,
    name: *const u8,
    name_len: u32,
    bytes: *const u8,
    bytes_len: u32,
    out_index: *mut u32,
    out_generation: *mut u32,
) -> u8 {
    if out_index.is_null() || out_generation.is_null() {
        return STATUS_NULL_OUTPUT;
    }
    // SAFETY: forwarded from the caller's contract.
    let name = match unsafe { read_str(name, name_len) } {
        Ok(value) => value,
        Err(status) => return status,
    };
    // SAFETY: forwarded from the caller's contract.
    let bytes = match unsafe { read_bytes(bytes, bytes_len) } {
        Ok(value) => value,
        Err(status) => return status,
    };
    // SAFETY: forwarded from this function's own contract.
    let result = unsafe {
        with_assets(world, |assets| {
            let mesh =
                Mesh::from_obj_bytes(name.as_str(), &bytes).map_err(|_| STATUS_DECODE_FAILED)?;
            let handle = assets
                .add_named(name.as_str(), mesh)
                .map_err(|_| STATUS_NAME_IN_USE)?;
            Ok((handle.index(), handle.generation()))
        })
    };
    match result {
        Ok((index, generation)) => {
            // SAFETY: both pointers were checked non-null above.
            unsafe {
                *out_index = index;
                *out_generation = generation;
            }
            STATUS_OK
        }
        Err(status) => status,
    }
}

/// Decodes a PNG buffer into a color texture and inserts it into `world`'s
/// `AssetManager`.
///
/// # Safety
///
/// Same contract as [`pill_render_data_load_mesh_obj`].
#[no_mangle]
pub unsafe extern "C" fn pill_render_data_load_texture_png(
    world: *mut World,
    name: *const u8,
    name_len: u32,
    bytes: *const u8,
    bytes_len: u32,
    out_index: *mut u32,
    out_generation: *mut u32,
) -> u8 {
    if out_index.is_null() || out_generation.is_null() {
        return STATUS_NULL_OUTPUT;
    }
    // SAFETY: forwarded from the caller's contract.
    let name = match unsafe { read_str(name, name_len) } {
        Ok(value) => value,
        Err(status) => return status,
    };
    // SAFETY: forwarded from the caller's contract.
    let bytes = match unsafe { read_bytes(bytes, bytes_len) } {
        Ok(value) => value,
        Err(status) => return status,
    };
    // SAFETY: forwarded from this function's own contract.
    let result = unsafe {
        with_assets(world, |assets| {
            let texture = Texture::new(
                name.as_str(),
                TextureType::Color,
                AssetLoader::Bytes(bytes.into_boxed_slice()),
            )
            .map_err(|_| STATUS_DECODE_FAILED)?;
            let handle = assets
                .add_named(name.as_str(), texture)
                .map_err(|_| STATUS_NAME_IN_USE)?;
            Ok((handle.index(), handle.generation()))
        })
    };
    match result {
        Ok((index, generation)) => {
            // SAFETY: both pointers were checked non-null above.
            unsafe {
                *out_index = index;
                *out_generation = generation;
            }
            STATUS_OK
        }
        Err(status) => status,
    }
}

#[allow(clippy::too_many_arguments)]
/// Builds a shader from managed WGSL sources and slot declarations, and inserts
/// it into `world`'s `AssetManager`.
///
/// # Safety
///
/// As [`pill_render_data_load_mesh_obj`] for `world`, the strings and the
/// outputs; `parameters`/`textures` must reference `parameters_len`/
/// `textures_len` valid, aligned elements (unless zero), and every name pointer
/// nested inside them must itself satisfy the same contract.
#[no_mangle]
pub unsafe extern "C" fn pill_render_data_load_shader(
    world: *mut World,
    name: *const u8,
    name_len: u32,
    vertex: *const u8,
    vertex_len: u32,
    fragment: *const u8,
    fragment_len: u32,
    parameters: *const NativeShaderParameterSlot,
    parameters_len: u32,
    textures: *const NativeShaderTextureSlot,
    textures_len: u32,
    pass_engine_parameters: u8,
    pass_camera_parameters: u8,
    out_index: *mut u32,
    out_generation: *mut u32,
) -> u8 {
    if out_index.is_null() || out_generation.is_null() {
        return STATUS_NULL_OUTPUT;
    }
    // SAFETY: every read below forwards the caller's pointer/length contract.
    let (name, vertex_wgsl, fragment_wgsl) = unsafe {
        let name = match read_str(name, name_len) {
            Ok(value) => value,
            Err(status) => return status,
        };
        let vertex_wgsl = match read_str(vertex, vertex_len) {
            Ok(value) => value,
            Err(status) => return status,
        };
        let fragment_wgsl = match read_str(fragment, fragment_len) {
            Ok(value) => value,
            Err(status) => return status,
        };
        (name, vertex_wgsl, fragment_wgsl)
    };
    // SAFETY: forwarded from the caller's contract.
    let parameters = match unsafe { read_slice(parameters, parameters_len) } {
        Ok(value) => value,
        Err(status) => return status,
    };
    // SAFETY: forwarded from the caller's contract.
    let textures = match unsafe { read_slice(textures, textures_len) } {
        Ok(value) => value,
        Err(status) => return status,
    };

    let mut parameter_slots = Vec::with_capacity(parameters.len());
    for parameter in parameters {
        // SAFETY: `parameter.name`/`name_len` came from the same caller
        // contract as every other string in this call.
        let name = match unsafe { read_str(parameter.name, parameter.name_len) } {
            Ok(value) => value,
            Err(status) => return status,
        };
        let kind = match parameter.kind {
            0 => ShaderParameterType::Scalar,
            1 => ShaderParameterType::Bool,
            _ => ShaderParameterType::Color,
        };
        parameter_slots.push(ShaderParameterSlot::new(name, kind));
    }

    let mut texture_slots = Vec::with_capacity(textures.len());
    for texture in textures {
        // SAFETY: same contract as above.
        let name = match unsafe { read_str(texture.name, texture.name_len) } {
            Ok(value) => value,
            Err(status) => return status,
        };
        texture_slots.push(ShaderTextureSlot::new(
            name,
            TextureType::Color,
            (texture.texture_binding, texture.sampler_binding),
        ));
    }

    // SAFETY: forwarded from this function's own contract.
    let result = unsafe {
        with_assets(world, |assets| {
            let shader = Shader::new(name.as_str())
                .with_wgsl(vertex_wgsl, fragment_wgsl)
                .with_parameter_slots(parameter_slots)
                .with_texture_slots(texture_slots)
                .with_engine_parameters(pass_engine_parameters != 0)
                .with_camera_parameters(pass_camera_parameters != 0)
                .build()
                .map_err(|_| STATUS_DECODE_FAILED)?;
            let handle = assets
                .add_named(name.as_str(), shader)
                .map_err(|_| STATUS_NAME_IN_USE)?;
            Ok((handle.index(), handle.generation()))
        })
    };
    match result {
        Ok((index, generation)) => {
            // SAFETY: both pointers were checked non-null above.
            unsafe {
                *out_index = index;
                *out_generation = generation;
            }
            STATUS_OK
        }
        Err(status) => status,
    }
}

#[allow(clippy::too_many_arguments)]
/// Builds a material from already-loaded handles and per-slot parameters, and
/// inserts it into `world`'s `AssetManager`.
///
/// `shader_index`/`shader_generation` may both be [`NO_HANDLE`] to leave the
/// renderer's default shader in place.
///
/// # Safety
///
/// Same contract as [`pill_render_data_load_shader`], applied to `name`,
/// `textures`/`scalars`/`colors` and the name pointers nested inside them.
#[no_mangle]
pub unsafe extern "C" fn pill_render_data_create_material(
    world: *mut World,
    name: *const u8,
    name_len: u32,
    shader_index: u32,
    shader_generation: u32,
    textures: *const NativeMaterialTexture,
    textures_len: u32,
    scalars: *const NativeMaterialScalar,
    scalars_len: u32,
    colors: *const NativeMaterialColor,
    colors_len: u32,
    rendering_order: u8,
    out_index: *mut u32,
    out_generation: *mut u32,
) -> u8 {
    if out_index.is_null() || out_generation.is_null() {
        return STATUS_NULL_OUTPUT;
    }
    // SAFETY: forwarded from the caller's contract.
    let name = match unsafe { read_str(name, name_len) } {
        Ok(value) => value,
        Err(status) => return status,
    };
    // SAFETY: forwarded from the caller's contract.
    let textures = match unsafe { read_slice(textures, textures_len) } {
        Ok(value) => value,
        Err(status) => return status,
    };
    // SAFETY: forwarded from the caller's contract.
    let scalars = match unsafe { read_slice(scalars, scalars_len) } {
        Ok(value) => value,
        Err(status) => return status,
    };
    // SAFETY: forwarded from the caller's contract.
    let colors = match unsafe { read_slice(colors, colors_len) } {
        Ok(value) => value,
        Err(status) => return status,
    };

    let mut builder = Material::builder(name.as_str()).rendering_order(rendering_order);
    if shader_index != NO_HANDLE || shader_generation != NO_HANDLE {
        builder = builder.shader(&Handle::from_raw(shader_index, shader_generation));
    }
    for texture in textures {
        // SAFETY: same contract as every other string in this call.
        let slot = match unsafe { read_str(texture.slot, texture.slot_len) } {
            Ok(value) => value,
            Err(status) => return status,
        };
        builder = builder.texture(
            slot,
            &Handle::from_raw(texture.texture_index, texture.texture_generation),
        );
    }
    for scalar in scalars {
        // SAFETY: same contract as above.
        let name = match unsafe { read_str(scalar.name, scalar.name_len) } {
            Ok(value) => value,
            Err(status) => return status,
        };
        builder = builder.scalar_parameter(name, scalar.value);
    }
    for color in colors {
        // SAFETY: same contract as above.
        let name = match unsafe { read_str(color.name, color.name_len) } {
            Ok(value) => value,
            Err(status) => return status,
        };
        builder = builder.color_parameter(name, [color.r, color.g, color.b]);
    }
    let material = builder.build();

    // SAFETY: forwarded from this function's own contract.
    let result = unsafe {
        with_assets(world, |assets| {
            let handle = assets
                .add_named(name.as_str(), material)
                .map_err(|_| STATUS_NAME_IN_USE)?;
            Ok((handle.index(), handle.generation()))
        })
    };
    match result {
        Ok((index, generation)) => {
            // SAFETY: both pointers were checked non-null above.
            unsafe {
                *out_index = index;
                *out_generation = generation;
            }
            STATUS_OK
        }
        Err(status) => status,
    }
}

/// Imports the texture at `path` (relative to `res`) through its `.meta` file
/// and writes the result to `output`; see
/// [`import_for_ffi`](pill_engine::asset_ffi::import_for_ffi) for the
/// arguments and status codes.
///
/// # Safety
///
/// The contract of [`import_for_ffi`](pill_engine::asset_ffi::import_for_ffi).
#[no_mangle]
pub unsafe extern "C" fn pill_render_data_import_texture(
    world: *mut World,
    path: *const u8,
    path_length: u32,
    policy: u8,
    settings: *const u8,
    settings_length: u32,
    output: *mut NativeImportedAsset,
) -> u8 {
    // SAFETY: forwarded unchanged from this function's contract.
    unsafe {
        import_for_ffi::<Texture>(
            world,
            path,
            path_length,
            policy,
            settings,
            settings_length,
            output,
        )
    }
}

/// Imports the mesh at `path` through its `.meta` file; otherwise as
/// [`pill_render_data_import_texture`].
///
/// # Safety
///
/// The contract of [`import_for_ffi`](pill_engine::asset_ffi::import_for_ffi).
#[no_mangle]
pub unsafe extern "C" fn pill_render_data_import_mesh(
    world: *mut World,
    path: *const u8,
    path_length: u32,
    policy: u8,
    settings: *const u8,
    settings_length: u32,
    output: *mut NativeImportedAsset,
) -> u8 {
    // SAFETY: forwarded unchanged from this function's contract.
    unsafe {
        import_for_ffi::<Mesh>(
            world,
            path,
            path_length,
            policy,
            settings,
            settings_length,
            output,
        )
    }
}

/// Drops the pipeline the world's `RenderingManager` holds, returning the
/// renderer to its built-in chain: a single geometry pass that draws every
/// instance through its own material's shader.
///
/// The managed equivalent of the Rust project fetching the resource and
/// calling `clear` on it; a project needs it because the registered default
/// chain (the PBR one) replaces the built-in pass with its own.
///
/// Returns a status code (see the `STATUS_*` constants).
///
/// # Safety
///
/// `world` must be null or point at a live `World` no one else is using for
/// the call's duration.
#[no_mangle]
pub unsafe extern "C" fn pill_render_data_clear_render_pipeline(world: *mut World) -> u8 {
    // SAFETY: the caller's contract: null, or a live world used exclusively.
    let Some(world) = (unsafe { world.as_mut() }) else {
        return STATUS_NO_ACTIVE_SCOPE;
    };
    let Some(manager) = world.get_resource_mut::<RenderingManager>() else {
        return STATUS_RENDER_MANAGER_MISSING;
    };
    manager.clear();
    STATUS_OK
}

// Offer every function to a host that links this crate statically; a loaded
// module offers them through the `#[no_mangle]` exports above instead.
pill_engine::submit! {
    PillExportDescriptor {
        name: "pill_render_data_load_mesh_obj",
        address: ExportAddress(pill_render_data_load_mesh_obj as *const ()),
    }
}
pill_engine::submit! {
    PillExportDescriptor {
        name: "pill_render_data_load_texture_png",
        address: ExportAddress(pill_render_data_load_texture_png as *const ()),
    }
}
pill_engine::submit! {
    PillExportDescriptor {
        name: "pill_render_data_load_shader",
        address: ExportAddress(pill_render_data_load_shader as *const ()),
    }
}
pill_engine::submit! {
    PillExportDescriptor {
        name: "pill_render_data_create_material",
        address: ExportAddress(pill_render_data_create_material as *const ()),
    }
}
pill_engine::submit! {
    PillExportDescriptor {
        name: "pill_render_data_import_texture",
        address: ExportAddress(pill_render_data_import_texture as *const ()),
    }
}
pill_engine::submit! {
    PillExportDescriptor {
        name: "pill_render_data_import_mesh",
        address: ExportAddress(pill_render_data_import_mesh as *const ()),
    }
}
pill_engine::submit! {
    PillExportDescriptor {
        name: "pill_render_data_clear_render_pipeline",
        address: ExportAddress(pill_render_data_clear_render_pipeline as *const ()),
    }
}
