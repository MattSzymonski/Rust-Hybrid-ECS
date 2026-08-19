//! Runtime-side description of the project module to load.
//!
//! # Responsibilities
//!
//! - Decode the project half of [`PillRuntimeCreateArgsV1`] into an owned,
//!   validated Rust description.
//! - Keep every path the backends need resolved once, at generation start.
//!
//! # Design
//!
//! Build commands, manifest parsing, and output-path inference all stay in the
//! host: the host is the side that never reloads, so it owns configuration and
//! compilation. The runtime only ever receives already-resolved absolute
//! paths, which keeps a reloadable binary free of project-layout knowledge and
//! makes the backend selection a single decoded discriminant.

// Standard library
use std::ffi::CStr;
use std::os::raw::c_char;
use std::path::PathBuf;

// External crates
use pill_runtime_api::{
    PillCSharpProjectV1, PillRuntimeCreateArgsV1, PILL_PROJECT_BACKEND_CSHARP,
    PILL_PROJECT_BACKEND_NATIVE, PILL_PROJECT_BACKEND_NONE,
};

// =============================================================================
// Types
// =============================================================================

/// Managed-project locations resolved by the host.
#[derive(Debug, Clone)]
pub(crate) struct CSharpProjectPaths {
    /// Absolute path of the collectible loader assembly.
    pub(crate) runtime_assembly_path: PathBuf,
    /// Absolute path of the loader's `runtimeconfig.json`.
    pub(crate) runtime_config_path: PathBuf,
    /// Assembly name of the loader, used to build the managed type name.
    pub(crate) runtime_assembly_name: String,
    /// Absolute directory holding the built project assembly.
    pub(crate) project_directory: PathBuf,
    /// File name of the project assembly inside that directory.
    pub(crate) project_assembly_file_name: String,
}

/// Which project module a runtime generation should load, and from where.
#[derive(Debug, Clone)]
pub(crate) enum ProjectDescriptor {
    /// Start without a project; the world stays empty until a later reload.
    None,
    /// A native shared library exporting `project_init`.
    Native {
        /// Absolute path of the built library.
        module_path: PathBuf,
    },
    /// A managed assembly hosted by the collectible C# loader.
    CSharp(CSharpProjectPaths),
}

// =============================================================================
// Free Functions
// =============================================================================

/// Borrow a NUL-terminated UTF-8 argument as an owned `String`.
///
/// # Errors
///
/// Returns a message naming `field` when the pointer is null or the bytes are
/// not valid UTF-8, so a malformed argument is reported instead of guessed at.
///
/// # Safety
///
/// `pointer` must either be null or address a NUL-terminated byte string that
/// stays valid for the duration of the call.
unsafe fn required_string(pointer: *const c_char, field: &str) -> Result<String, String> {
    if pointer.is_null() {
        return Err(format!("create args field '{field}' must not be null"));
    }
    // SAFETY: The caller guarantees a non-null `pointer` addresses a
    // NUL-terminated string valid for this call.
    unsafe { CStr::from_ptr(pointer) }
        .to_str()
        .map(str::to_owned)
        .map_err(|error| format!("create args field '{field}' is not valid UTF-8: {error}"))
}

/// Borrow an optional NUL-terminated UTF-8 argument.
///
/// # Errors
///
/// Returns a message naming `field` when a non-null pointer holds invalid
/// UTF-8. A null pointer yields `None`, which is a valid absence.
///
/// # Safety
///
/// Same contract as [`required_string`].
pub(crate) unsafe fn optional_string(
    pointer: *const c_char,
    field: &str,
) -> Result<Option<String>, String> {
    if pointer.is_null() {
        return Ok(None);
    }
    // SAFETY: The caller guarantees a non-null `pointer` addresses a
    // NUL-terminated string valid for this call.
    unsafe { required_string(pointer, field) }.map(Some)
}

/// Decode the managed-project sub-struct.
///
/// # Errors
///
/// Returns a message when the pointer is null, the struct was produced by a
/// different contract layout, or one of its paths is missing or malformed.
///
/// # Safety
///
/// `csharp` must either be null or point to a live [`PillCSharpProjectV1`]
/// whose string fields stay valid for the duration of the call.
unsafe fn decode_csharp_paths(
    csharp: *const PillCSharpProjectV1,
) -> Result<CSharpProjectPaths, String> {
    if csharp.is_null() {
        return Err(String::from(
            "the C# backend was selected but no managed project description was supplied",
        ));
    }

    // SAFETY: The caller guarantees a non-null `csharp` points to a live
    // struct for the duration of this call.
    let csharp = unsafe { &*csharp };
    if csharp.struct_size as usize != std::mem::size_of::<PillCSharpProjectV1>() {
        return Err(format!(
            "managed project description layout mismatch: runtime expects {} bytes, host reports {} bytes",
            std::mem::size_of::<PillCSharpProjectV1>(),
            csharp.struct_size,
        ));
    }

    // SAFETY: Every field below is a NUL-terminated UTF-8 pointer the host
    // keeps valid for the whole `create` call.
    unsafe {
        Ok(CSharpProjectPaths {
            runtime_assembly_path: PathBuf::from(required_string(
                csharp.runtime_assembly_path_utf8,
                "csharp.runtime_assembly_path",
            )?),
            runtime_config_path: PathBuf::from(required_string(
                csharp.runtime_config_path_utf8,
                "csharp.runtime_config_path",
            )?),
            runtime_assembly_name: required_string(
                csharp.runtime_assembly_name_utf8,
                "csharp.runtime_assembly_name",
            )?,
            project_directory: PathBuf::from(required_string(
                csharp.project_directory_utf8,
                "csharp.project_directory",
            )?),
            project_assembly_file_name: required_string(
                csharp.project_assembly_file_name_utf8,
                "csharp.project_assembly_file_name",
            )?,
        })
    }
}

/// Decode the project half of the creation arguments.
///
/// # Errors
///
/// Returns a message when the backend discriminant is unknown or the fields
/// the selected backend requires are missing.
///
/// # Safety
///
/// `args` must point to a live [`PillRuntimeCreateArgsV1`] whose string and
/// sub-struct pointers stay valid for the duration of the call.
pub(crate) unsafe fn decode_project_descriptor(
    args: &PillRuntimeCreateArgsV1,
) -> Result<ProjectDescriptor, String> {
    match args.project_backend {
        PILL_PROJECT_BACKEND_NONE => Ok(ProjectDescriptor::None),
        PILL_PROJECT_BACKEND_NATIVE => {
            // SAFETY: The caller guarantees the pointer is null or a live
            // NUL-terminated string for the duration of this call.
            let module_path =
                unsafe { optional_string(args.project_module_path_utf8, "project_module_path")? };
            match module_path {
                // A native generation with no artifact yet is a valid starting
                // point: the host loads one on the first successful build.
                None => Ok(ProjectDescriptor::None),
                Some(path) => Ok(ProjectDescriptor::Native {
                    module_path: PathBuf::from(path),
                }),
            }
        }
        // SAFETY: The caller guarantees the sub-struct pointer is null or live
        // for the duration of this call.
        PILL_PROJECT_BACKEND_CSHARP => {
            unsafe { decode_csharp_paths(args.csharp_project) }.map(ProjectDescriptor::CSharp)
        }
        unknown => Err(format!("unknown project backend discriminant {unknown}")),
    }
}
