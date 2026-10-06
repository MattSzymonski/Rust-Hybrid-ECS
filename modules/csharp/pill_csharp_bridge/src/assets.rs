//! Managed asset loading: the C# entry points, forwarded to the renderer data
//! crate's asset functions.
//!
//! # Responsibilities
//!
//! - Publish the asset entry points the managed runtime calls (load a mesh,
//!   texture or shader; create a material; import a texture, mesh or sound
//!   through its `.meta` file; clear the world's rendering pipeline) in
//!   `CsEngineApi`.
//! - Find the renderer data crate's function for each by name and call it with
//!   the active managed invocation's world.
//! - Report a missing invocation or a missing function with the status codes
//!   the managed side already knows.
//!
//! # Design
//!
//! The asset types these calls build (`Mesh`, `Texture`, `Shader`, `Material`)
//! belong to the renderer's data crate (`pill_master_renderer_data`), and so do
//! the functions that build them, in its `csharp_assets` module. The host names
//! no renderer data type: it only owns the managed invocation's world, which
//! the data crate's functions take as their first argument, and passes every
//! other argument through untouched - the argument structs included, as opaque
//! pointers whose layout only the managed side and the data crate share.
//!
//! Each function is found by its export name, the contract between the host
//! and the data crate:
//!
//! - **Development**: the data crate is a loaded extension. After every
//!   extension load and reload, the host resolves the names from the loaded
//!   DLLs and publishes them here ([`publish_asset_exports`]). It never falls
//!   back to its own image: a copy of the data crate linked there would build
//!   assets with that copy's type identities, not the module's.
//! - **Shipping**: the data crate is linked into the executable, which finds
//!   the descriptors it submits
//!   ([`pill_engine::component_registry::find_export`]).
//!
//! Every function runs only inside an active managed invocation - ordinarily an
//! `[EcsStartup]` method - reusing the thread-local world access
//! [`with_active_world`] already gives queries and resources. Startups run one
//! at a time, before the parallel scheduler starts, so the world is used
//! exclusively for the call.

// Standard library
use std::ffi::c_void;

// External crates
use pill_engine::component_registry::ExportAddress;
use pill_engine::World;

// Current crate
use crate::context::with_active_world;

// Status codes the host reports itself; the data crate's functions report the
// rest. The managed side's table (`Engine.ValidateAssetStatus`) knows them all.
/// No managed invocation is active.
const STATUS_NO_ACTIVE_SCOPE: u8 = 1;
/// No renderer data crate provides the asset functions.
const STATUS_RENDERER_UNAVAILABLE: u8 = 6;

/// The export names the entry points forward to: the renderer data crate's
/// (`pill_master_renderer_data::csharp_assets`) and the audio module's
/// (`pill_audio::csharp_assets`).
#[cfg(feature = "hot_reload")]
pub(crate) const ASSET_EXPORT_NAMES: [&str; 8] = [
    "pill_render_data_load_mesh_obj",
    "pill_render_data_load_texture_png",
    "pill_render_data_load_shader",
    "pill_render_data_create_material",
    "pill_render_data_import_texture",
    "pill_render_data_import_mesh",
    "pill_render_data_clear_render_pipeline",
    "pill_audio_import_sound",
];

/// The asset functions the loaded modules offer, by export name.
///
/// Replaced wholesale after every extension load and reload, so it never names
/// an image the reload retired.
#[cfg(feature = "hot_reload")]
static LOADED_ASSET_EXPORTS: std::sync::RwLock<Vec<(&'static str, ExportAddress)>> =
    std::sync::RwLock::new(Vec::new());

/// Publish the asset functions `lookup` finds among the loaded modules, and
/// return how many of [`ASSET_EXPORT_NAMES`] it found.
///
/// Call it after every extension load or reload, before any managed code can
/// run: the addresses of a reloaded module's previous generation must not be
/// used again.
#[cfg(feature = "hot_reload")]
pub fn publish_asset_exports(lookup: impl Fn(&str) -> Option<ExportAddress>) -> usize {
    let found: Vec<(&'static str, ExportAddress)> = ASSET_EXPORT_NAMES
        .iter()
        .filter_map(|name| lookup(name).map(|address| (*name, address)))
        .collect();
    let count = found.len();
    match LOADED_ASSET_EXPORTS.write() {
        Ok(mut table) => *table = found,
        Err(poisoned) => *poisoned.into_inner() = found,
    }
    count
}

/// The address of the asset function exported as `name`, for this build.
fn export_address(name: &str) -> Option<ExportAddress> {
    // Development: only what the loaded modules published.
    #[cfg(feature = "hot_reload")]
    {
        let table = match LOADED_ASSET_EXPORTS.read() {
            Ok(table) => table,
            Err(poisoned) => poisoned.into_inner(),
        };
        table
            .iter()
            .find(|(export, _)| *export == name)
            .map(|(_, address)| *address)
    }
    // Shipping: the descriptors linked into this executable.
    #[cfg(not(feature = "hot_reload"))]
    {
        pill_engine::component_registry::find_export(name)
    }
}

/// The data crate's mesh loader: world, name, bytes, handle outputs.
type LoadMeshObj =
    unsafe extern "C" fn(*mut World, *const u8, u32, *const u8, u32, *mut u32, *mut u32) -> u8;
/// The data crate's texture loader, shaped like [`LoadMeshObj`].
type LoadTexturePng = LoadMeshObj;
/// The data crate's shader builder: world, name, both stages' WGSL, the slot
/// arrays, the two parameter flags, handle outputs.
type LoadShader = unsafe extern "C" fn(
    *mut World,
    *const u8,
    u32,
    *const u8,
    u32,
    *const u8,
    u32,
    *const c_void,
    u32,
    *const c_void,
    u32,
    u8,
    u8,
    *mut u32,
    *mut u32,
) -> u8;
/// The data crate's material builder: world, name, shader handle, the
/// texture/scalar/color arrays, rendering order, handle outputs.
type CreateMaterial = unsafe extern "C" fn(
    *mut World,
    *const u8,
    u32,
    u32,
    u32,
    *const c_void,
    u32,
    *const c_void,
    u32,
    *const c_void,
    u32,
    u8,
    *mut u32,
    *mut u32,
) -> u8;

/// A module's import export: world, path, policy, settings JSON, output.
type ImportAsset =
    unsafe extern "C" fn(*mut World, *const u8, u32, u8, *const u8, u32, *mut c_void) -> u8;

/// A module's pipeline clear: world only.
type ClearRenderPipeline = unsafe extern "C" fn(*mut World) -> u8;

/// The data crate's function named `name`, as the function pointer type `F`.
///
/// # Safety
///
/// `F` must be the exact signature of the function exported under `name`.
unsafe fn resolve<F: Copy>(name: &str) -> Option<F> {
    let address = export_address(name)?;
    debug_assert_eq!(
        std::mem::size_of::<F>(),
        std::mem::size_of::<*const ()>(),
        "an export resolves to a plain function pointer"
    );
    // SAFETY: `address` was made from the function exported as `name`, and the
    // caller guarantees `F` is that function's type; a function pointer and a
    // data pointer have the same size on every target this engine builds for.
    Some(unsafe { std::mem::transmute_copy::<*const (), F>(&address.0) })
}

/// Calls the data crate's function `name` with the active invocation's world,
/// through `call`.
///
/// Reports [`STATUS_RENDERER_UNAVAILABLE`] when no linked data crate offers the
/// function and [`STATUS_NO_ACTIVE_SCOPE`] outside a managed invocation.
///
/// # Safety
///
/// `F` must be the exact signature of the function exported under `name`, and
/// `call` must uphold that function's own contract for every argument but the
/// world.
unsafe fn forward<F: Copy>(name: &str, call: impl FnOnce(F, *mut World) -> u8) -> u8 {
    // SAFETY: forwarded from this function's own contract.
    let Some(function) = (unsafe { resolve::<F>(name) }) else {
        return STATUS_RENDERER_UNAVAILABLE;
    };
    with_active_world(|world| call(function, world as *mut World)).unwrap_or(STATUS_NO_ACTIVE_SCOPE)
}

// =============================================================================
// Public FFI entry points (published in `CsEngineApi`)
// =============================================================================

/// Decodes a Wavefront OBJ buffer into a mesh and inserts it into the active
/// invocation's `AssetManager`.
///
/// # Safety
///
/// `name`/`bytes` must reference their declared lengths in readable memory for
/// the call's duration (unless the matching length is zero), and
/// `out_index`/`out_generation` must be writable.
pub(super) extern "C" fn ffi_asset_load_mesh_obj(
    name: *const u8,
    name_len: u32,
    bytes: *const u8,
    bytes_len: u32,
    out_index: *mut u32,
    out_generation: *mut u32,
) -> u8 {
    // SAFETY: `LoadMeshObj` is the export's signature, and every argument but
    // the world comes from this function's own contract unchanged.
    unsafe {
        forward::<LoadMeshObj>("pill_render_data_load_mesh_obj", |function, world| {
            function(
                world,
                name,
                name_len,
                bytes,
                bytes_len,
                out_index,
                out_generation,
            )
        })
    }
}

/// Decodes a PNG buffer into a color texture and inserts it into the active
/// invocation's `AssetManager`.
///
/// # Safety
///
/// Same contract as [`ffi_asset_load_mesh_obj`].
pub(super) extern "C" fn ffi_asset_load_texture_png(
    name: *const u8,
    name_len: u32,
    bytes: *const u8,
    bytes_len: u32,
    out_index: *mut u32,
    out_generation: *mut u32,
) -> u8 {
    // SAFETY: as in `ffi_asset_load_mesh_obj`, for `LoadTexturePng`.
    unsafe {
        forward::<LoadTexturePng>("pill_render_data_load_texture_png", |function, world| {
            function(
                world,
                name,
                name_len,
                bytes,
                bytes_len,
                out_index,
                out_generation,
            )
        })
    }
}

/// Builds a shader from managed WGSL sources and slot declarations, and
/// inserts it into the active invocation's `AssetManager`.
///
/// `parameters` and `textures` point at arrays whose element layout the
/// managed side and the data crate share; the host passes them through.
///
/// # Safety
///
/// `name`/`vertex`/`fragment` must reference their declared lengths in
/// readable memory (unless zero); `parameters`/`textures` must reference
/// `parameters_len`/`textures_len` valid, aligned elements (unless zero), and
/// every name pointer nested inside them must itself satisfy the same
/// contract. `out_index`/`out_generation` must be writable.
#[allow(clippy::too_many_arguments)]
pub(super) extern "C" fn ffi_asset_load_shader(
    name: *const u8,
    name_len: u32,
    vertex: *const u8,
    vertex_len: u32,
    fragment: *const u8,
    fragment_len: u32,
    parameters: *const c_void,
    parameters_len: u32,
    textures: *const c_void,
    textures_len: u32,
    pass_engine_parameters: u8,
    pass_camera_parameters: u8,
    out_index: *mut u32,
    out_generation: *mut u32,
) -> u8 {
    // SAFETY: as in `ffi_asset_load_mesh_obj`, for `LoadShader`.
    unsafe {
        forward::<LoadShader>("pill_render_data_load_shader", |function, world| {
            function(
                world,
                name,
                name_len,
                vertex,
                vertex_len,
                fragment,
                fragment_len,
                parameters,
                parameters_len,
                textures,
                textures_len,
                pass_engine_parameters,
                pass_camera_parameters,
                out_index,
                out_generation,
            )
        })
    }
}

/// Builds a material from already-loaded handles and per-slot parameters, and
/// inserts it into the active invocation's `AssetManager`.
///
/// `shader_index`/`shader_generation` may both be `u32::MAX` to leave the
/// renderer's default shader in place.
///
/// # Safety
///
/// Same contract as [`ffi_asset_load_shader`], applied to `name`,
/// `textures`/`scalars`/`colors` and the name pointers nested inside them.
#[allow(clippy::too_many_arguments)]
pub(super) extern "C" fn ffi_asset_create_material(
    name: *const u8,
    name_len: u32,
    shader_index: u32,
    shader_generation: u32,
    textures: *const c_void,
    textures_len: u32,
    scalars: *const c_void,
    scalars_len: u32,
    colors: *const c_void,
    colors_len: u32,
    rendering_order: u8,
    out_index: *mut u32,
    out_generation: *mut u32,
) -> u8 {
    // SAFETY: as in `ffi_asset_load_mesh_obj`, for `CreateMaterial`.
    unsafe {
        forward::<CreateMaterial>("pill_render_data_create_material", |function, world| {
            function(
                world,
                name,
                name_len,
                shader_index,
                shader_generation,
                textures,
                textures_len,
                scalars,
                scalars_len,
                colors,
                colors_len,
                rendering_order,
                out_index,
                out_generation,
            )
        })
    }
}

/// Imports a source asset through its `.meta` file by forwarding to the module
/// export `name`.
///
/// # Safety
///
/// `path`/`settings` must reference their declared lengths in readable memory
/// (unless zero), and `output` must point at a writable `NativeImportedAsset`
/// (`pill_engine::asset_ffi`).
unsafe fn forward_import(
    name: &str,
    path: *const u8,
    path_length: u32,
    policy: u8,
    settings: *const u8,
    settings_length: u32,
    output: *mut c_void,
) -> u8 {
    // SAFETY: `ImportAsset` is every import export's signature, and every
    // argument but the world comes from this function's contract unchanged.
    unsafe {
        forward::<ImportAsset>(name, |function, world| {
            function(
                world,
                path,
                path_length,
                policy,
                settings,
                settings_length,
                output,
            )
        })
    }
}

/// Imports a texture from `res` through its `.meta` file into the active
/// invocation's `AssetManager`.
///
/// # Safety
///
/// The contract of `forward_import`.
pub(super) extern "C" fn ffi_asset_import_texture(
    path: *const u8,
    path_length: u32,
    policy: u8,
    settings: *const u8,
    settings_length: u32,
    output: *mut c_void,
) -> u8 {
    // SAFETY: forwarded from this function's contract.
    unsafe {
        forward_import(
            "pill_render_data_import_texture",
            path,
            path_length,
            policy,
            settings,
            settings_length,
            output,
        )
    }
}

/// Imports a mesh from `res` through its `.meta` file; otherwise as
/// [`ffi_asset_import_texture`].
///
/// # Safety
///
/// The contract of `forward_import`.
pub(super) extern "C" fn ffi_asset_import_mesh(
    path: *const u8,
    path_length: u32,
    policy: u8,
    settings: *const u8,
    settings_length: u32,
    output: *mut c_void,
) -> u8 {
    // SAFETY: forwarded from this function's contract.
    unsafe {
        forward_import(
            "pill_render_data_import_mesh",
            path,
            path_length,
            policy,
            settings,
            settings_length,
            output,
        )
    }
}

/// Imports a sound from `res` through its `.meta` file; reports
/// "unavailable" (6) when the project does not load `pill_audio`.
///
/// # Safety
///
/// The contract of `forward_import`.
pub(super) extern "C" fn ffi_asset_import_sound(
    path: *const u8,
    path_length: u32,
    policy: u8,
    settings: *const u8,
    settings_length: u32,
    output: *mut c_void,
) -> u8 {
    // SAFETY: forwarded from this function's contract.
    unsafe {
        forward_import(
            "pill_audio_import_sound",
            path,
            path_length,
            policy,
            settings,
            settings_length,
            output,
        )
    }
}

/// Drops the active invocation's world rendering pipeline, returning the
/// renderer to its built-in pass.
///
/// # Safety
///
/// No argument crosses here; the world comes from the active invocation, as in
/// every other entry point.
pub(super) extern "C" fn ffi_asset_clear_render_pipeline() -> u8 {
    // SAFETY: `ClearRenderPipeline` is the export's signature, and it receives
    // only the active invocation's world.
    unsafe {
        forward::<ClearRenderPipeline>(
            "pill_render_data_clear_render_pipeline",
            |function, world| function(world),
        )
    }
}
