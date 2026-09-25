//! Managed asset loading: bridges `AssetManager` for C# projects that cannot
//! construct engine asset types directly.
//!
//! # Responsibilities
//!
//! - Decode a mesh, texture or shader from raw bytes a managed caller
//!   supplies and insert it into the active invocation's `AssetManager`.
//! - Build a material from already-loaded handles and per-slot parameters.
//!
//! # Design
//!
//! Every function below runs only inside an active managed invocation -
//! ordinarily an `[EcsStartup]` method - reusing the same thread-local world
//! access [`with_active_world`] already gives queries and resources. Asset
//! mutation during startup is inherently exclusive (startups run one at a
//! time, before the parallel scheduler starts), so none of the
//! access-declaration bookkeeping components and resources need applies here.
//!
//! Gated behind the `rendering` feature because the asset types these
//! functions build (`Mesh`, `Texture`, `Shader`, `Material`) belong to
//! `pill_master_renderer`, which only a windowed host links. A headless host
//! still exposes the same four ABI slots - the managed struct layout must not
//! depend on host build flags - they just always report "unavailable".

// The shared argument structs, status codes and byte-reading helpers below
// are only exercised by `rendering_impl`, which does not exist in a headless
// build; the headless stubs only need the status constant they return.
#![cfg_attr(not(feature = "rendering"), allow(dead_code))]

// =============================================================================
// Shared native argument shapes
// =============================================================================

/// One parameter slot a managed shader declaration supplies.
#[repr(C)]
pub(super) struct NativeShaderParameterSlot {
    pub(super) name: *const u8,
    pub(super) name_len: u32,
    /// `0` scalar, `1` bool, `2` color.
    pub(super) kind: u8,
}

/// One texture slot a managed shader declaration supplies.
///
/// The bound texture is always color-typed: nothing using this bridge yet
/// needs a normal map slot.
#[repr(C)]
pub(super) struct NativeShaderTextureSlot {
    pub(super) name: *const u8,
    pub(super) name_len: u32,
    pub(super) texture_binding: u32,
    pub(super) sampler_binding: u32,
}

/// One texture a managed material declaration binds to a shader slot.
#[repr(C)]
pub(super) struct NativeMaterialTexture {
    pub(super) slot: *const u8,
    pub(super) slot_len: u32,
    pub(super) texture_index: u32,
    pub(super) texture_generation: u32,
}

/// One scalar parameter a managed material declaration sets.
#[repr(C)]
pub(super) struct NativeMaterialScalar {
    pub(super) name: *const u8,
    pub(super) name_len: u32,
    pub(super) value: f32,
}

/// One color parameter a managed material declaration sets.
#[repr(C)]
pub(super) struct NativeMaterialColor {
    pub(super) name: *const u8,
    pub(super) name_len: u32,
    pub(super) r: f32,
    pub(super) g: f32,
    pub(super) b: f32,
}

/// Handle part meaning "absent" - no shader override, no bound texture.
pub(super) const NO_HANDLE: u32 = u32::MAX;

// Status codes shared by every function in this module. `0` is always
// success; the rest are deliberately distinct from `resources.rs`'s codes
// because these calls are not the resource-access path and must never be
// confused with it in a log.
const STATUS_OK: u8 = 0;
const STATUS_NO_ACTIVE_SCOPE: u8 = 1;
const STATUS_ASSET_MANAGER_MISSING: u8 = 2;
const STATUS_INVALID_UTF8: u8 = 3;
const STATUS_DECODE_FAILED: u8 = 4;
const STATUS_NULL_OUTPUT: u8 = 5;
#[cfg_attr(feature = "rendering", allow(dead_code))]
const STATUS_RENDERER_UNAVAILABLE: u8 = 6;
/// The name is already bound to a live asset in the active invocation.
#[cfg_attr(not(feature = "rendering"), allow(dead_code))]
const STATUS_NAME_IN_USE: u8 = 7;

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

#[cfg(feature = "rendering")]
mod rendering_impl {
    use super::{
        read_bytes, read_slice, read_str, NativeMaterialColor, NativeMaterialScalar,
        NativeMaterialTexture, NativeShaderParameterSlot, NativeShaderTextureSlot, NO_HANDLE,
        STATUS_ASSET_MANAGER_MISSING, STATUS_DECODE_FAILED, STATUS_NAME_IN_USE,
        STATUS_NO_ACTIVE_SCOPE, STATUS_NULL_OUTPUT, STATUS_OK,
    };
    use crate::csharp::context::with_active_world;
    use pill_engine::{AssetLoader, AssetManager, Handle};
    use pill_master_renderer::{
        Material, Mesh, Shader, ShaderParameterSlot, ShaderParameterType, ShaderTextureSlot,
        Texture, TextureType,
    };
    use std::collections::HashMap;

    /// Runs `body` against the active invocation's `AssetManager`, folding the
    /// "no scope"/"no AssetManager" cases into the shared status codes.
    fn with_assets<R>(body: impl FnOnce(&mut AssetManager) -> Result<R, u8>) -> Result<R, u8> {
        with_active_world(|world| {
            let Some(assets) = world.get_resource_mut::<AssetManager>() else {
                return Err(STATUS_ASSET_MANAGER_MISSING);
            };
            body(assets)
        })
        .unwrap_or(Err(STATUS_NO_ACTIVE_SCOPE))
    }

    /// # Safety
    /// See [`super::ffi_asset_load_mesh_obj`]; this is its rendering-enabled body.
    pub(super) unsafe fn load_mesh_obj(
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
        let result = with_assets(|assets| {
            let mesh = Mesh::from_obj_bytes(name.as_str(), &bytes).map_err(|_| STATUS_DECODE_FAILED)?;
            let handle = assets
                .add_named(name.as_str(), mesh)
                .map_err(|_| STATUS_NAME_IN_USE)?;
            Ok((handle.index(), handle.generation()))
        });
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

    /// # Safety
    /// See [`super::ffi_asset_load_texture_png`]; this is its rendering-enabled body.
    pub(super) unsafe fn load_texture_png(
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
        let result = with_assets(|assets| {
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
        });
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

    /// # Safety
    /// See [`super::ffi_asset_load_shader`]; this is its rendering-enabled body.
    #[allow(clippy::too_many_arguments)]
    pub(super) unsafe fn load_shader(
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
            parameter_slots.push((name, ShaderParameterSlot::new(kind)));
        }

        let mut texture_slots = HashMap::with_capacity(textures.len());
        for texture in textures {
            // SAFETY: same contract as above.
            let name = match unsafe { read_str(texture.name, texture.name_len) } {
                Ok(value) => value,
                Err(status) => return status,
            };
            texture_slots.insert(
                name,
                ShaderTextureSlot::new(
                    TextureType::Color,
                    (texture.texture_binding, texture.sampler_binding),
                ),
            );
        }

        let result = with_assets(|assets| {
            let shader = Shader::from_wgsl(
                name.as_str(),
                vertex_wgsl,
                fragment_wgsl,
                parameter_slots,
                texture_slots,
                pass_engine_parameters != 0,
                pass_camera_parameters != 0,
            );
            let handle = assets
                .add_named(name.as_str(), shader)
                .map_err(|_| STATUS_NAME_IN_USE)?;
            Ok((handle.index(), handle.generation()))
        });
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

    /// # Safety
    /// See [`super::ffi_asset_create_material`]; this is its rendering-enabled body.
    #[allow(clippy::too_many_arguments)]
    pub(super) unsafe fn create_material(
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

        let result = with_assets(|assets| {
            let handle = assets
                .add_named(name.as_str(), material)
                .map_err(|_| STATUS_NAME_IN_USE)?;
            Ok((handle.index(), handle.generation()))
        });
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
    #[cfg(feature = "rendering")]
    {
        // SAFETY: forwarded from this function's own contract.
        unsafe {
            rendering_impl::load_mesh_obj(name, name_len, bytes, bytes_len, out_index, out_generation)
        }
    }
    #[cfg(not(feature = "rendering"))]
    {
        let _ = (name, name_len, bytes, bytes_len, out_index, out_generation);
        STATUS_RENDERER_UNAVAILABLE
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
    #[cfg(feature = "rendering")]
    {
        // SAFETY: forwarded from this function's own contract.
        unsafe {
            rendering_impl::load_texture_png(name, name_len, bytes, bytes_len, out_index, out_generation)
        }
    }
    #[cfg(not(feature = "rendering"))]
    {
        let _ = (name, name_len, bytes, bytes_len, out_index, out_generation);
        STATUS_RENDERER_UNAVAILABLE
    }
}

/// Builds a shader from managed WGSL sources and slot declarations, and
/// inserts it into the active invocation's `AssetManager`.
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
    parameters: *const NativeShaderParameterSlot,
    parameters_len: u32,
    textures: *const NativeShaderTextureSlot,
    textures_len: u32,
    pass_engine_parameters: u8,
    pass_camera_parameters: u8,
    out_index: *mut u32,
    out_generation: *mut u32,
) -> u8 {
    #[cfg(feature = "rendering")]
    {
        // SAFETY: forwarded from this function's own contract.
        unsafe {
            rendering_impl::load_shader(
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
        }
    }
    #[cfg(not(feature = "rendering"))]
    {
        let _ = (
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
        );
        STATUS_RENDERER_UNAVAILABLE
    }
}

/// Builds a material from already-loaded handles and per-slot parameters, and
/// inserts it into the active invocation's `AssetManager`.
///
/// `shader_index`/`shader_generation` may both be [`NO_HANDLE`] to leave the
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
    #[cfg(feature = "rendering")]
    {
        // SAFETY: forwarded from this function's own contract.
        unsafe {
            rendering_impl::create_material(
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
        }
    }
    #[cfg(not(feature = "rendering"))]
    {
        let _ = (
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
        );
        STATUS_RENDERER_UNAVAILABLE
    }
}
