//! Low-level .NET hosting bootstrap used by the C# game backend.
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

// =============================================================================
// hostfxr ABI Types
// =============================================================================

/// Native character type used by `hostfxr` on the current platform.
#[cfg(windows)]
type HostChar = u16;
#[cfg(not(windows))]
type HostChar = u8;

type RuntimeHandle = *mut std::ffi::c_void;
type InitializeFn =
    unsafe extern "system" fn(*const HostChar, *const std::ffi::c_void, *mut RuntimeHandle) -> i32;
type GetDelegateFn =
    unsafe extern "system" fn(RuntimeHandle, i32, *mut *mut std::ffi::c_void) -> i32;
type CloseFn = unsafe extern "system" fn(RuntimeHandle) -> i32;
type LoadAssemblyFn = unsafe extern "system" fn(
    *const HostChar,
    *const HostChar,
    *const HostChar,
    *const HostChar,
    *const std::ffi::c_void,
    *mut *const std::ffi::c_void,
) -> i32;

/// `hostfxr_delegate_type::hdt_load_assembly_and_get_function_pointer`.
const LOAD_ASSEMBLY_AND_GET_FUNCTION_POINTER: i32 = 5;

// =============================================================================
// DotnetRuntimeContext
// =============================================================================

/// Owns one initialized .NET runtime context and its assembly loader delegate.
///
/// `_library` intentionally remains stored for the full lifetime of the
/// context because `close` and `load_assembly` point into that native module.
pub struct DotnetRuntimeContext {
    _library: Library,
    handle: RuntimeHandle,
    close: CloseFn,
    load_assembly: LoadAssemblyFn,
}

impl DotnetRuntimeContext {
    /// Load `hostfxr`, initialize .NET from `runtime_config`, and acquire the
    /// `load_assembly_and_get_function_pointer` runtime delegate.
    pub fn new(runtime_config: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        // Resolve hostfxr at runtime rather than linking against one SDK
        // installation. This lets the host follow the machine's configured
        // .NET root and use its newest compatible hosting library.
        let library_path = find_dotnet_host()?;

        // Keep this Library inside DotnetRuntimeContext for as long as any
        // copied export or delegate can be called. Dropping it earlier would
        // invalidate every function pointer resolved below.
        // SAFETY: The path was selected from an installed `host/fxr` version.
        let library = unsafe { Library::new(&library_path)? };

        // Copy the three bootstrap exports out of libloading's temporary
        // Symbol wrappers. Their validity is tied to `library`, which is moved
        // into the returned context and therefore outlives these pointers.
        // SAFETY: These names and signatures are fixed by the hostfxr ABI.
        let initialize: InitializeFn =
            unsafe { *library.get(b"hostfxr_initialize_for_runtime_config")? };
        let get_delegate: GetDelegateFn = unsafe { *library.get(b"hostfxr_get_runtime_delegate")? };
        let close: CloseFn = unsafe { *library.get(b"hostfxr_close")? };

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
            return Err(format!(
                "hostfxr_initialize_for_runtime_config failed with 0x{status:08X}"
            )
            .into());
        }

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
            return Err(format!("hostfxr_get_runtime_delegate failed with 0x{status:08X}").into());
        }

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
    pub fn get_unmanaged_fn<T: Copy>(
        &self,
        assembly: &Path,
        type_name: &str,
        method_name: &str,
    ) -> Result<T, Box<dyn std::error::Error>> {
        // This generic API returns function pointers by value. Reject structs,
        // references, or other accidental T choices before copying raw bytes.
        if std::mem::size_of::<T>() != std::mem::size_of::<*const std::ffi::c_void>() {
            return Err("managed export type is not pointer-sized".into());
        }

        // Keep all encoded buffers in local variables so their pointers remain
        // stable and live throughout the complete hostfxr call.
        let assembly = host_string(assembly.as_os_str())?;
        let type_name = host_string(std::ffi::OsStr::new(type_name))?;
        let method_name = host_string(std::ffi::OsStr::new(method_name))?;
        let mut function = std::ptr::null();

        // hostfxr assigns this sentinel meaning to the delegate-type argument;
        // it bypasses delegate thunk creation and requests the direct unmanaged
        // entry point generated for an UnmanagedCallersOnly method.
        // `(char_t*)-1` requests a method marked `UnmanagedCallersOnly`.
        let unmanaged_callers_only = usize::MAX as *const HostChar;

        // No reserved state is required, so that argument remains null. The
        // final pointer is an output slot written by hostfxr on success.
        // SAFETY: All strings are nul-terminated and the output slot is valid.
        let status = unsafe {
            (self.load_assembly)(
                assembly.as_ptr(),
                type_name.as_ptr(),
                method_name.as_ptr(),
                unmanaged_callers_only,
                std::ptr::null(),
                &mut function,
            )
        };

        // Treat a missing output pointer as an error even if hostfxr returned a
        // nonnegative informational code; callers cannot safely use it.
        if status < 0 || function.is_null() {
            return Err(format!(
                "load_assembly_and_get_function_pointer failed for {method_name:?} with 0x{status:08X}"
            )
            .into());
        }

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
// Runtime Discovery and String Conversion
// =============================================================================

/// Locate the newest installed `hostfxr` beneath `DOTNET_ROOT` or Program Files.
fn find_dotnet_host() -> Result<PathBuf, Box<dyn std::error::Error>> {
    // An explicit DOTNET_ROOT takes precedence, matching .NET hosting tools.
    // Program Files is the conventional fallback for system-wide Windows SDKs.
    let dotnet_root = std::env::var_os("DOTNET_ROOT")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("ProgramFiles").map(|path| PathBuf::from(path).join("dotnet")))
        .ok_or("DOTNET_ROOT and ProgramFiles are both unset")?;
    let fxr_root = dotnet_root.join("host").join("fxr");

    // Every child directory represents one installed hostfxr version. Convert
    // its dot/dash-separated numeric pieces before sorting so, for example,
    // version 10 correctly sorts after version 9 instead of before it.
    let mut versions: Vec<(Vec<u32>, PathBuf)> = std::fs::read_dir(&fxr_root)?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
        .map(|entry| {
            let version = entry
                .file_name()
                .to_string_lossy()
                .split(['.', '-'])
                .map(|part| part.parse::<u32>().unwrap_or(0))
                .collect();
            (version, entry.path())
        })
        .collect();
    versions.sort_by(|left, right| left.0.cmp(&right.0));

    // Use the newest installed hostfxr. Framework compatibility is still
    // decided by hostfxr itself from the runtimeconfig during initialization.
    let directory = versions
        .last()
        .ok_or("no .NET hostfxr installation found")?
        .1
        .clone();

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

/// Encode an OS string as nul-terminated UTF-16 for Windows hostfxr.
#[cfg(windows)]
fn host_string(value: &std::ffi::OsStr) -> Result<Vec<HostChar>, Box<dyn std::error::Error>> {
    use std::os::windows::ffi::OsStrExt;

    // Windows hostfxr consumes native UTF-16 strings. Append the terminator
    // explicitly because OsStrExt yields only the encoded contents.
    Ok(value.encode_wide().chain(std::iter::once(0)).collect())
}

/// Encode an OS string as nul-terminated bytes for Unix hostfxr.
#[cfg(not(windows))]
fn host_string(value: &std::ffi::OsStr) -> Result<Vec<HostChar>, Box<dyn std::error::Error>> {
    use std::os::unix::ffi::OsStrExt;

    // Unix hostfxr consumes the OS string's original byte representation. It
    // is not necessarily UTF-8, so avoid a lossy textual conversion.
    let mut bytes = value.as_bytes().to_vec();

    // An interior terminator would truncate the assembly, type, or method name
    // at the ABI boundary and could resolve a different target than requested.
    if bytes.contains(&0) {
        return Err(".NET host string contains an interior NUL".into());
    }

    // hostfxr expects a C-style nul-terminated buffer.
    bytes.push(0);
    Ok(bytes)
}
