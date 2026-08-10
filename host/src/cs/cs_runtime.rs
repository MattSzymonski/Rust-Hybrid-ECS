//! Minimal .NET runtime bootstrap used by the C# game backend.

use std::path::{Path, PathBuf};

use libloading::Library;

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

// `hostfxr_delegate_type::hdt_load_assembly_and_get_function_pointer`.
const LOAD_ASSEMBLY_AND_GET_FUNCTION_POINTER: i32 = 5;

pub struct DotnetRuntimeContext {
    _library: Library,
    handle: RuntimeHandle,
    close: CloseFn,
    load_assembly: LoadAssemblyFn,
}

impl DotnetRuntimeContext {
    pub fn new(runtime_config: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let library_path = find_dotnet_host()?;
        let library = unsafe { Library::new(&library_path)? };
        let initialize: InitializeFn =
            unsafe { *library.get(b"hostfxr_initialize_for_runtime_config")? };
        let get_delegate: GetDelegateFn = unsafe { *library.get(b"hostfxr_get_runtime_delegate")? };
        let close: CloseFn = unsafe { *library.get(b"hostfxr_close")? };

        let config = host_string(runtime_config.as_os_str())?;
        let mut handle = std::ptr::null_mut();
        let status = unsafe { initialize(config.as_ptr(), std::ptr::null(), &mut handle) };
        if status < 0 || handle.is_null() {
            return Err(format!(
                "hostfxr_initialize_for_runtime_config failed with 0x{status:08X}"
            )
            .into());
        }

        let mut delegate = std::ptr::null_mut();
        let status = unsafe {
            get_delegate(
                handle,
                LOAD_ASSEMBLY_AND_GET_FUNCTION_POINTER,
                &mut delegate,
            )
        };
        if status < 0 || delegate.is_null() {
            unsafe { close(handle) };
            return Err(format!("hostfxr_get_runtime_delegate failed with 0x{status:08X}").into());
        }

        let load_assembly =
            unsafe { std::mem::transmute::<*mut std::ffi::c_void, LoadAssemblyFn>(delegate) };
        Ok(Self {
            _library: library,
            handle,
            close,
            load_assembly,
        })
    }

    pub fn get_unmanaged_fn<T: Copy>(
        &self,
        assembly: &Path,
        type_name: &str,
        method_name: &str,
    ) -> Result<T, Box<dyn std::error::Error>> {
        if std::mem::size_of::<T>() != std::mem::size_of::<*const std::ffi::c_void>() {
            return Err("managed export type is not pointer-sized".into());
        }

        let assembly = host_string(assembly.as_os_str())?;
        let type_name = host_string(std::ffi::OsStr::new(type_name))?;
        let method_name = host_string(std::ffi::OsStr::new(method_name))?;
        let mut function = std::ptr::null();

        // (char_t*)-1 requests a method marked UnmanagedCallersOnly.
        let unmanaged_callers_only = usize::MAX as *const HostChar;
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
        if status < 0 || function.is_null() {
            return Err(format!(
                "load_assembly_and_get_function_pointer failed for {method_name:?} with 0x{status:08X}"
            )
            .into());
        }

        Ok(unsafe { std::mem::transmute_copy::<*const std::ffi::c_void, T>(&function) })
    }
}

impl Drop for DotnetRuntimeContext {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { (self.close)(self.handle) };
        }
    }
}

fn find_dotnet_host() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let dotnet_root = std::env::var_os("DOTNET_ROOT")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("ProgramFiles").map(|path| PathBuf::from(path).join("dotnet")))
        .ok_or("DOTNET_ROOT and ProgramFiles are both unset")?;
    let fxr_root = dotnet_root.join("host").join("fxr");
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

    Ok(directory.join(filename))
}

#[cfg(windows)]
fn host_string(value: &std::ffi::OsStr) -> Result<Vec<HostChar>, Box<dyn std::error::Error>> {
    use std::os::windows::ffi::OsStrExt;
    Ok(value.encode_wide().chain(std::iter::once(0)).collect())
}

#[cfg(not(windows))]
fn host_string(value: &std::ffi::OsStr) -> Result<Vec<HostChar>, Box<dyn std::error::Error>> {
    use std::os::unix::ffi::OsStrExt;
    let mut bytes = value.as_bytes().to_vec();
    if bytes.contains(&0) {
        return Err(".NET host string contains an interior NUL".into());
    }
    bytes.push(0);
    Ok(bytes)
}
