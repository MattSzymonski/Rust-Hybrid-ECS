//! Low-level .NET hosting bootstrap used by the C# project backend.
//!
//! # Responsibilities
//!
//! - Locates the newest installed `hostfxr` library.
//! - Starts .NET from a managed runtime configuration file.
//! - Resolves methods marked with `UnmanagedCallersOnly` into native pointers.
//! - Keeps the native hosting library and runtime context alive.
//!
//! # Design
//!
//! This module contains no ECS knowledge. It is a narrow wrapper around the
//! stable `hostfxr` ABI and returns typed function pointers to the managed
//! backend registration layer.
//! The file is named `csharp_runtime.rs`, but Microsoft-owned library and export
//! names remain `hostfxr` and must not be renamed.

// Standard library
use std::path::{Path, PathBuf};

// External crates
use libloading::Library;
use pill_core::error::CSharpError;

// =============================================================================
// Constants
// =============================================================================

/// `hostfxr_delegate_type::hdt_load_assembly_and_get_function_pointer`.
///
/// Requested when acquiring a runtime delegate so hostfxr resolves the bridge
/// that both loads a managed assembly and returns one native-callable method.
const LOAD_ASSEMBLY_AND_GET_FUNCTION_POINTER: i32 = 5;

// =============================================================================
// hostfxr ABI Types
// =============================================================================

/// Native character type used by `hostfxr` on the current platform.
///
/// `hostfxr` consumes UTF-16 code units on Windows and raw path bytes on Unix,
/// so the character width follows the target platform.
#[cfg(windows)]
type HostChar = u16;
#[cfg(not(windows))]
type HostChar = u8;

/// Opaque handle to an initialized hostfxr runtime context.
///
/// Created by `hostfxr_initialize_for_runtime_config` and released with
/// `hostfxr_close`; it is never dereferenced from the Rust side.
type RuntimeHandle = *mut std::ffi::c_void;

/// Signature of the `hostfxr_initialize_for_runtime_config` export.
///
/// Starts a .NET runtime from a `runtimeconfig.json` file and returns an owned
/// context handle.
type InitializeFn =
    unsafe extern "system" fn(*const HostChar, *const std::ffi::c_void, *mut RuntimeHandle) -> i32;

/// Signature of the `hostfxr_get_runtime_delegate` export.
///
/// Resolves a typed runtime delegate from a live context handle.
type GetDelegateFn =
    unsafe extern "system" fn(RuntimeHandle, i32, *mut *mut std::ffi::c_void) -> i32;

/// Signature of the `hostfxr_close` export.
///
/// Releases a previously initialized runtime context.
type CloseFn = unsafe extern "system" fn(RuntimeHandle) -> i32;

/// Signature of the `load_assembly_and_get_function_pointer` runtime delegate.
///
/// Loads a managed assembly and resolves one `UnmanagedCallersOnly` static
/// method into a native entry-point pointer.
type LoadAssemblyFn = unsafe extern "system" fn(
    *const HostChar,
    *const HostChar,
    *const HostChar,
    *const HostChar,
    *const std::ffi::c_void,
    *mut *const std::ffi::c_void,
) -> i32;

// =============================================================================
// DotnetRuntimeContext
// =============================================================================

/// Owns one initialized .NET runtime context and its assembly loader delegate.
///
/// `_library` intentionally remains stored for the full lifetime of the
/// context because `close` and `load_assembly` point into that native module.
pub struct DotnetRuntimeContext {
    /// Native hosting library kept alive so every resolved function pointer
    /// below still targets mapped executable code.
    _library: Library,
    /// Opaque hostfxr context handle returned by runtime initialization.
    handle: RuntimeHandle,
    /// `hostfxr_close` export used to release the context in `Drop`.
    close: CloseFn,
    /// `load_assembly_and_get_function_pointer` delegate used by `get_unmanaged_fn`.
    load_assembly: LoadAssemblyFn,
}

impl DotnetRuntimeContext {
    /// Load `hostfxr`, initialize .NET from `runtime_config`, and acquire the
    /// `load_assembly_and_get_function_pointer` runtime delegate.
    ///
    /// # Errors
    ///
    /// Returns [`CSharpError::HostfxrOverrideNotAFile`] when the
    /// `ECS_DOTNET_HOSTFXR` override does not point at a file,
    /// [`CSharpError::HostfxrNotFound`] when no `host/fxr` installation exists,
    /// [`CSharpError::HostfxrLoadFailed`] when the selected library cannot be
    /// loaded, [`CSharpError::HostfxrExportMissing`] when a required bootstrap
    /// export is absent, [`CSharpError::InteriorNul`] when the configuration
    /// path contains an interior NUL, and either
    /// [`CSharpError::RuntimeInitializationFailed`] or
    /// [`CSharpError::RuntimeDelegateFailed`] when hostfxr reports failure.
    pub fn new(runtime_config: &Path) -> Result<Self, CSharpError> {
        // Step 1: Locate and load the newest installed hostfxr library.
        // Resolve hostfxr at runtime rather than linking against one SDK
        // installation. This lets the host follow the machine's configured
        // .NET root and use its newest compatible hosting library.
        let library_path = find_dotnet_host()?;

        // Keep this Library inside DotnetRuntimeContext for as long as any
        // copied export or delegate can be called. Dropping it earlier would
        // invalidate every function pointer resolved below.
        // SAFETY: `find_dotnet_host` only returns paths of real files under an
        // installed .NET `host/fxr` directory, so the path references a valid
        // shared-library module image.
        let library = unsafe {
            Library::new(&library_path).map_err(|source| CSharpError::HostfxrLoadFailed {
                path: library_path.display().to_string(),
                source,
            })?
        };

        // Step 2: Copy the three bootstrap exports out of libloading's
        // temporary Symbol wrappers. Their validity is tied to `library`,
        // which is moved into the returned context and therefore outlives
        // these pointers.
        // SAFETY: These names and signatures are fixed by the hostfxr ABI.
        let initialize: InitializeFn = unsafe {
            *library
                .get(b"hostfxr_initialize_for_runtime_config")
                .map_err(|source| CSharpError::HostfxrExportMissing {
                    symbol: "hostfxr_initialize_for_runtime_config".to_string(),
                    source,
                })?
        };
        // SAFETY: These names and signatures are fixed by the hostfxr ABI.
        let get_delegate: GetDelegateFn = unsafe {
            *library
                .get(b"hostfxr_get_runtime_delegate")
                .map_err(|source| CSharpError::HostfxrExportMissing {
                    symbol: "hostfxr_get_runtime_delegate".to_string(),
                    source,
                })?
        };
        // SAFETY: These names and signatures are fixed by the hostfxr ABI.
        let close: CloseFn = unsafe {
            *library
                .get(b"hostfxr_close")
                .map_err(|source| CSharpError::HostfxrExportMissing {
                    symbol: "hostfxr_close".to_string(),
                    source,
                })?
        };

        // Step 3: Initialize a .NET runtime from the runtime configuration.
        // The runtimeconfig file selects the target framework and framework
        // version for csharp_runtime. hostfxr uses UTF-16 on Windows and raw native
        // path bytes on Unix, so conversion stays platform-specific.
        let config = host_string(runtime_config.as_os_str())?;
        let mut handle = std::ptr::null_mut();

        // Null initialization parameters request hostfxr's default host path
        // and dotnet-root resolution. The returned handle owns this context.
        // SAFETY: `config` is nul-terminated and `handle` points to writable
        // storage that remains valid for the complete call.
        let status = unsafe { initialize(config.as_ptr(), std::ptr::null(), &mut handle) };

        // hostfxr reports failures as negative status codes. Also reject a
        // nominal success that fails to provide the required context handle.
        if status < 0 || handle.is_null() {
            return Err(CSharpError::RuntimeInitializationFailed {
                status: format!("0x{status:08X}"),
            });
        }

        // Step 4: Acquire the load-assembly delegate from the live runtime.
        // Delegate kind 5 is the stable bridge that both loads a managed
        // assembly and resolves one native-callable static method from it.
        let mut delegate = std::ptr::null_mut();
        // SAFETY: `handle` was returned by successful runtime initialization.
        let status = unsafe {
            get_delegate(
                handle,
                LOAD_ASSEMBLY_AND_GET_FUNCTION_POINTER,
                &mut delegate,
            )
        };
        if status < 0 || delegate.is_null() {
            // Initialization succeeded, so this error path must explicitly
            // release the context before ownership can be placed in Self.
            // SAFETY: Initialization produced this live context handle.
            unsafe { close(handle) };
            return Err(CSharpError::RuntimeDelegateFailed {
                status: format!("0x{status:08X}"),
            });
        }

        // Step 5: Cast the untyped delegate into a typed function pointer.
        // hostfxr exposes delegates as untyped pointers. The requested enum
        // value is what establishes the concrete function signature here.
        // SAFETY: Delegate kind 5 guarantees the returned pointer has the
        // `LoadAssemblyFn` signature defined above.
        let load_assembly =
            unsafe { std::mem::transmute::<*mut std::ffi::c_void, LoadAssemblyFn>(delegate) };
        Ok(Self {
            _library: library,
            handle,
            close,
            load_assembly,
        })
    }

    /// Resolve an `UnmanagedCallersOnly` static method from a managed assembly.
    ///
    /// `T` must be the exact native signature declared on the managed method.
    ///
    /// # Errors
    ///
    /// Returns [`CSharpError::ManagedExportNotPointerSized`] when `T` is not
    /// pointer-sized, [`CSharpError::InteriorNul`] when any of the three
    /// identifiers contains an interior NUL, and
    /// [`CSharpError::ManagedEntryPointFailed`] when hostfxr reports failure
    /// or leaves the output pointer null.
    pub fn get_unmanaged_fn<T: Copy>(
        &self,
        assembly: &Path,
        type_name: &str,
        method_name: &str,
    ) -> Result<T, CSharpError> {
        // Step 1: Reject function types that cannot hold a native pointer.
        // This generic API returns function pointers by value. Reject structs,
        // references, or other accidental T choices before copying raw bytes.
        if std::mem::size_of::<T>() != std::mem::size_of::<*const std::ffi::c_void>() {
            return Err(CSharpError::ManagedExportNotPointerSized);
        }

        // Step 2: Encode the assembly, type, and method identifiers.
        // Keep all encoded buffers in local variables so their pointers remain
        // stable and live throughout the complete hostfxr call.
        let encoded_assembly = host_string(assembly.as_os_str())?;
        let encoded_type_name = host_string(std::ffi::OsStr::new(type_name))?;
        let encoded_method_name = host_string(std::ffi::OsStr::new(method_name))?;
        let mut function = std::ptr::null();

        // Step 3: Request the direct unmanaged entry point from hostfxr.
        // hostfxr assigns this sentinel meaning to the delegate-type argument;
        // it bypasses delegate thunk creation and requests the direct unmanaged
        // entry point generated for an UnmanagedCallersOnly method.
        let unmanaged_callers_only = usize::MAX as *const HostChar;

        // No reserved state is required, so that argument remains null. The
        // final pointer is an output slot written by hostfxr on success.
        // SAFETY: All strings are nul-terminated and the output slot is valid.
        let status = unsafe {
            (self.load_assembly)(
                encoded_assembly.as_ptr(),
                encoded_type_name.as_ptr(),
                encoded_method_name.as_ptr(),
                unmanaged_callers_only,
                std::ptr::null(),
                &mut function,
            )
        };

        // Treat a missing output pointer as an error even if hostfxr returned a
        // nonnegative informational code; callers cannot safely use it.
        if status < 0 || function.is_null() {
            return Err(CSharpError::ManagedEntryPointFailed {
                method: method_name.to_string(),
                status: format!("0x{status:08X}"),
            });
        }

        // Step 4: Copy the resolved pointer into the caller's function type.
        // Copy the pointer bits into the caller's declared function-pointer
        // type. Signature correctness remains the caller's ABI obligation.
        // SAFETY: The size check above guarantees a pointer-sized target. The
        // caller supplies the signature matching the managed export.
        Ok(unsafe { std::mem::transmute_copy::<*const std::ffi::c_void, T>(&function) })
    }
}

impl Drop for DotnetRuntimeContext {
    /// Close the owned hostfxr context before unloading its native library.
    fn drop(&mut self) {
        // A null handle means initialization never transferred ownership. For
        // a live handle, close the host context before `_library` is dropped so
        // the close function pointer still targets mapped executable code.
        if !self.handle.is_null() {
            // SAFETY: This object uniquely owns the live hostfxr context.
            unsafe { (self.close)(self.handle) };
        }
    }
}

// =============================================================================
// HostfxrVersion
// =============================================================================

/// One installed `hostfxr` candidate with its parsed version key.
struct HostfxrVersion {
    /// Numeric base version components used for ordering.
    numeric: Vec<u32>,
    /// Whether the version carries no prerelease suffix.
    stable: bool,
    /// Absolute path to the versioned `host/fxr` directory.
    path: PathBuf,
}

// =============================================================================
// Runtime Discovery and String Conversion
// =============================================================================

/// Locate the newest installed `hostfxr` library.
///
/// An explicit `ECS_DOTNET_HOSTFXR` path wins. Otherwise every configured
/// .NET root is scanned and the newest `hostfxr` version is selected;
/// prereleases only win when no stable release of the same base version
/// exists.
///
/// # Errors
///
/// Returns [`CSharpError::HostfxrOverrideNotAFile`] when the
/// `ECS_DOTNET_HOSTFXR` override does not point at a regular file, and
/// [`CSharpError::HostfxrNotFound`] when no `host/fxr` version directory
/// exists under any configured .NET root.
fn find_dotnet_host() -> Result<PathBuf, CSharpError> {
    // An explicit override lets users pin one hostfxr regardless of what
    // other SDKs are installed.
    if let Some(override_path) = std::env::var_os("ECS_DOTNET_HOSTFXR") {
        let path = PathBuf::from(override_path);
        if path.is_file() {
            return Ok(path);
        }
        return Err(CSharpError::HostfxrOverrideNotAFile {
            path: path.display().to_string(),
        });
    }

    let mut versions: Vec<HostfxrVersion> = Vec::new();
    for root in dotnet_roots() {
        collect_hostfxr_versions(&root, &mut versions);
    }

    // Newest version wins; a stable release beats a prerelease of the same
    // base version because `true` sorts after `false`.
    versions.sort_by(|left, right| {
        left.numeric
            .cmp(&right.numeric)
            .then(left.stable.cmp(&right.stable))
    });
    let directory = versions
        .last()
        .map(|version| version.path.clone())
        .ok_or(CSharpError::HostfxrNotFound)?;

    #[cfg(windows)]
    let filename = "hostfxr.dll";
    #[cfg(target_os = "linux")]
    let filename = "libhostfxr.so";
    #[cfg(target_os = "macos")]
    let filename = "libhostfxr.dylib";

    // The version directory layout is identical across platforms; only the
    // native shared-library filename differs.
    Ok(directory.join(filename))
}

/// Candidate .NET installation roots, in preference order.
fn dotnet_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(root) = std::env::var_os("DOTNET_ROOT").map(PathBuf::from) {
        roots.push(root);
    }
    #[cfg(windows)]
    {
        if let Some(program_files) = std::env::var_os("ProgramFiles") {
            roots.push(PathBuf::from(program_files).join("dotnet"));
        }
        if let Some(program_files_x86) = std::env::var_os("ProgramFiles(x86)") {
            roots.push(PathBuf::from(program_files_x86).join("dotnet"));
        }
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
            roots.push(
                PathBuf::from(local_app_data)
                    .join("Microsoft")
                    .join("dotnet"),
            );
        }
    }
    #[cfg(not(windows))]
    {
        if let Some(home) = std::env::var_os("HOME") {
            roots.push(PathBuf::from(home).join(".dotnet"));
        }
        roots.push(PathBuf::from("/usr/share/dotnet"));
        roots.push(PathBuf::from("/usr/lib/dotnet"));
    }
    roots
}

/// Append every `host/fxr/<version>` candidate found beneath one dotnet root.
fn collect_hostfxr_versions(root: &Path, versions: &mut Vec<HostfxrVersion>) {
    // Missing roots are normal: not every candidate location exists on every
    // machine, so unreadable directories are skipped instead of failing.
    let fxr_root = root.join("host").join("fxr");
    let Ok(entries) = std::fs::read_dir(&fxr_root) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let (numeric, stable) = parse_hostfxr_version(&entry.file_name().to_string_lossy());
        versions.push(HostfxrVersion {
            numeric,
            stable,
            path: entry.path(),
        });
    }
}

/// Split a `hostfxr` directory name into an orderable version key.
///
/// Only the numeric base version participates in comparison; a directory with
/// any non-numeric segment (e.g. `-preview.7`) is marked non-stable so a
/// stable release of the same base version always wins.
fn parse_hostfxr_version(name: &str) -> (Vec<u32>, bool) {
    let mut numeric = Vec::new();
    for part in name.split('.') {
        match part.find(|character: char| !character.is_ascii_digit()) {
            Some(_) => return (numeric, false),
            None => numeric.push(part.parse::<u32>().unwrap_or(0)),
        }
    }
    (numeric, true)
}

/// Encode an OS string as nul-terminated UTF-16 for Windows hostfxr.
///
/// # Errors
///
/// Returns [`CSharpError::InteriorNul`] when the value contains an embedded
/// NUL that would truncate the string at the ABI boundary.
#[cfg(windows)]
fn host_string(value: &std::ffi::OsStr) -> Result<Vec<HostChar>, CSharpError> {
    use std::os::windows::ffi::OsStrExt;

    // Windows hostfxr consumes native UTF-16 strings. Append the terminator
    // explicitly because OsStrExt yields only the encoded contents.
    let mut encoded: Vec<HostChar> = value.encode_wide().collect();
    // An interior terminator would truncate the string at the ABI boundary.
    if encoded.contains(&0) {
        return Err(CSharpError::InteriorNul);
    }
    encoded.push(0);
    Ok(encoded)
}

/// Encode an OS string as nul-terminated bytes for Unix hostfxr.
///
/// # Errors
///
/// Returns [`CSharpError::InteriorNul`] when the value contains an embedded
/// NUL that would truncate the string at the ABI boundary.
#[cfg(not(windows))]
fn host_string(value: &std::ffi::OsStr) -> Result<Vec<HostChar>, CSharpError> {
    use std::os::unix::ffi::OsStrExt;

    // Unix hostfxr consumes the OS string's original byte representation. It
    // is not necessarily UTF-8, so avoid a lossy textual conversion.
    let mut bytes = value.as_bytes().to_vec();

    // An interior terminator would truncate the assembly, type, or method name
    // at the ABI boundary and could resolve a different target than requested.
    if bytes.contains(&0) {
        return Err(CSharpError::InteriorNul);
    }

    bytes.push(0);
    Ok(bytes)
}
