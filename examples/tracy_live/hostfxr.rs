//! Thin wrapper over `hostfxr.dll` — loaded dynamically via libloading,
//! avoiding the static `libnethost` linking issues entirely.
//!
//! Ported near-verbatim from `hot_reloading/flappy/src/hostfxr.rs` — this
//! module is fully generic (nothing Flappy- or tracy_live-specific), so
//! there was nothing to change beyond the module doc comment.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use libloading::{Library, Symbol};

// ---------------------------------------------------------------------------
// Hostfxr C API types (subset we need)
// ---------------------------------------------------------------------------

/// Opaque handle returned by hostfxr_initialize_for_runtime_config.
type HostfxrHandle = *mut std::ffi::c_void;

/// Delegate type enum values.
#[allow(unused)]
#[repr(i32)]
enum HostfxrDelegateType {
    LoadAssemblyAndGetFunctionPointer = 5,
}

/// Parameters for hostfxr_initialize_for_runtime_config (we pass null).
#[repr(C)]
struct HostfxrInitializeParameters {
    size: usize,
    host_path: *const u16,
    dotnet_root: *const u16,
}

/// Signature of the delegate returned by hdt_load_assembly_and_get_function_pointer.
type LoadAssemblyAndGetFunctionPointerFn = unsafe extern "system" fn(
    assembly_path: *const u16,
    type_name: *const u16,
    method_name: *const u16,
    delegate_type_name: *const u16,
    reserved: *const std::ffi::c_void,
    delegate: *mut *const std::ffi::c_void,  // out pointer to function
) -> i32;

/// Sentinel value for UnmanagedCallersOnly methods.
const UNMANAGED_CALLERS_ONLY_METHOD: *const u16 = usize::MAX as *const u16;

// ---------------------------------------------------------------------------
// Loaded hostfxr functions (kept alive for the lifetime of HostfxrContext)
// ---------------------------------------------------------------------------

type HostfxrInitFn = unsafe extern "system" fn(
    config_path: *const u16,
    parameters: *const HostfxrInitializeParameters,
    handle: *mut HostfxrHandle,
) -> i32;

type HostfxrGetDelegateFn = unsafe extern "system" fn(
    handle: HostfxrHandle,
    delegate_type: i32,
    delegate: *mut *mut std::ffi::c_void,
) -> i32;

type HostfxrCloseFn = unsafe extern "system" fn(handle: HostfxrHandle) -> i32;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Loaded hostfxr library + resolved entry points.
pub struct HostfxrContext {
    _lib: Library, // Keep the DLL loaded.
    handle: HostfxrHandle,
}

impl HostfxrContext {
    /// Load hostfxr.dll from the .NET installation and initialize for the
    /// given runtime config.
    pub fn new(runtime_config: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let hostfxr_path = find_hostfxr()?;

        // Safety: we trust hostfxr.dll from the .NET SDK.
        unsafe {
            let lib = Library::new(&hostfxr_path)?;

            let init: Symbol<HostfxrInitFn> = lib.get(b"hostfxr_initialize_for_runtime_config")?;

            let config_wide = to_wide(runtime_config);
            let params = HostfxrInitializeParameters {
                size: std::mem::size_of::<HostfxrInitializeParameters>(),
                host_path: std::ptr::null(),
                dotnet_root: std::ptr::null(),
            };

            let mut handle: HostfxrHandle = std::ptr::null_mut();
            let hr = init(config_wide.as_ptr(), &params, &mut handle);
            if hr < 0 {
                return Err(format!(
                    "hostfxr_initialize_for_runtime_config failed: 0x{hr:08X}"
                )
                .into());
            }

            Ok(Self { _lib: lib, handle })
        }
    }

    /// Load a managed assembly and resolve an `[UnmanagedCallersOnly]` method
    /// by its type-qualified name.
    ///
    /// `type_name` must be assembly-qualified, e.g. `"Flappy.Interop, game_cs"`.
    pub fn get_unmanaged_fn<T>(
        &self,
        assembly_path: &Path,
        type_name: &str,
        method_name: &str,
    ) -> Result<T, Box<dyn std::error::Error>> {
        unsafe {
            let get_delegate: Symbol<HostfxrGetDelegateFn> =
                self._lib.get(b"hostfxr_get_runtime_delegate")?;

            let mut loader: *mut std::ffi::c_void = std::ptr::null_mut();
            let hr = get_delegate(
                self.handle,
                HostfxrDelegateType::LoadAssemblyAndGetFunctionPointer as i32,
                &mut loader,
            );
            if hr < 0 {
                return Err(format!("hostfxr_get_runtime_delegate failed: 0x{hr:08X}").into());
            }

            let loader_fn: LoadAssemblyAndGetFunctionPointerFn =
                std::mem::transmute(loader);

            let assembly_wide = to_wide(assembly_path);
            let type_wide = to_wide_str(type_name);
            let method_wide = to_wide_str(method_name);

            // Use the sentinel for UnmanagedCallersOnly methods.
            let mut fn_ptr: *const std::ffi::c_void = std::ptr::null();
            let hr = loader_fn(
                assembly_wide.as_ptr(),
                type_wide.as_ptr(),
                method_wide.as_ptr(),
                UNMANAGED_CALLERS_ONLY_METHOD,
                std::ptr::null(),
                &mut fn_ptr,
            );
            if hr < 0 {
                return Err(format!(
                    "load_assembly_and_get_function_pointer({type_name}::{method_name}) \
                     failed: 0x{hr:08X}"
                )
                .into());
            }

            Ok(std::mem::transmute_copy(&fn_ptr))
        }
    }
}

impl Drop for HostfxrContext {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                // Best-effort close; ignore errors on drop.
                if let Ok(close) = self._lib.get::<HostfxrCloseFn>(b"hostfxr_close") {
                    close(self.handle);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Locate hostfxr.dll in the installed .NET runtime.
fn find_hostfxr() -> Result<String, Box<dyn std::error::Error>> {
    // 1. DOTNET_ROOT env var.
    if let Ok(root) = std::env::var("DOTNET_ROOT") {
        let candidate = Path::new(&root).join("host").join("fxr");
        if let Some(path) = newest_hostfxr_in(&candidate) {
            return Ok(path);
        }
    }

    // 2. Default install path.
    let default = Path::new(r"C:\Program Files\dotnet\host\fxr");
    if let Some(path) = newest_hostfxr_in(default) {
        return Ok(path);
    }

    Err("hostfxr.dll not found. Install .NET SDK or set DOTNET_ROOT.".into())
}

/// Pick the newest hostfxr.dll inside a `host/fxr/<version>/` directory tree.
fn newest_hostfxr_in(fxr_dir: &Path) -> Option<String> {
    let mut best: Option<(String, String)> = None;
    let entries = std::fs::read_dir(fxr_dir).ok()?;
    for entry in entries.flatten() {
        let ver_dir = entry.path();
        if !ver_dir.is_dir() {
            continue;
        }
        let dll = ver_dir.join("hostfxr.dll");
        if dll.exists() {
            let ver = ver_dir.file_name()?.to_str()?.to_owned();
            match &best {
                None => best = Some((dll.to_str()?.to_owned(), ver)),
                Some((_, old_ver)) if ver_as_tuple(&ver) > ver_as_tuple(old_ver) => {
                    best = Some((dll.to_str()?.to_owned(), ver));
                }
                _ => {}
            }
        }
    }
    best.map(|(path, _)| path)
}

fn ver_as_tuple(v: &str) -> Vec<u32> {
    v.split('.').filter_map(|s| s.parse().ok()).collect()
}

/// Convert a Path to a null-terminated UTF-16 (Windows wide) string.
fn to_wide(path: &Path) -> Vec<u16> {
    let mut v: Vec<u16> = path.as_os_str().encode_wide().collect();
    v.push(0);
    v
}

/// Convert a &str to a null-terminated UTF-16 string.
fn to_wide_str(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = OsStr::new(s).encode_wide().collect();
    v.push(0);
    v
}
