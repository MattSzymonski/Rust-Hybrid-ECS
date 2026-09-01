//! NativeAOT library loader for the managed backend.
//!
//! The framework-dependent posture boots CoreCLR through hostfxr and resolves
//! the `LoaderInterop` exports via the `load_assembly_and_get_function_pointer`
//! delegate. The NativeAOT posture instead loads the AOT-published native
//! library (`dotnet publish -p:PublishAot=true`, which embeds a trimmed runtime)
//! with `libloading` and resolves the same `pill_*` exports directly by symbol
//! name - no hostfxr, no installed .NET, no JIT.
//!
//! Everything downstream (system discovery, scheduler registration, per-frame
//! dispatch) is identical to the hostfxr path; only how the function pointers
//! are obtained differs.

// External crates
use std::ffi::CString;
use std::path::Path;

// Workspace crates
use pill_core::error::CSharpError;

/// Owns a loaded NativeAOT library and resolves its unmanaged exports.
///
/// Only the shipping posture (no `hot_reload`) reaches this type; a dev build
/// links nothing through it.
#[cfg_attr(feature = "hot_reload", allow(dead_code))]
pub(crate) struct AotRuntimeContext {
    /// Keeps the native library mapped for the host's lifetime, so every
    /// resolved function pointer below stays valid.
    _library: libloading::Library,
}

impl AotRuntimeContext {
    /// Load the AOT native library from disk.
    ///
    /// # Errors
    ///
    /// Returns [`CSharpError::AotLibraryNotFound`] when the file is absent and
    /// [`CSharpError::AotLibraryLoadFailed`] when it cannot be mapped.
    pub(crate) fn new(library_path: &Path) -> Result<Self, CSharpError> {
        if !library_path.is_file() {
            return Err(CSharpError::AotLibraryNotFound {
                path: library_path.display().to_string(),
            });
        }
        // SAFETY: the path points at a real file produced by a NativeAOT
        // publish; loading it maps a valid module image.
        let library = unsafe {
            libloading::Library::new(library_path).map_err(|source| {
                CSharpError::AotLibraryLoadFailed {
                    path: library_path.display().to_string(),
                    source,
                }
            })?
        };
        Ok(Self { _library: library })
    }

    /// Resolve one `pill_*` export to a typed function pointer.
    ///
    /// `T` must be the exact `extern "system"` signature the export declares.
    ///
    /// # Errors
    ///
    /// Returns [`CSharpError::AotExportMissing`] when the symbol is absent.
    pub(crate) fn get_unmanaged_fn<T: Copy>(&self, symbol: &str) -> Result<T, CSharpError> {
        // `libloading` requires a nul-terminated symbol name. Export symbols
        // are fixed strings defined here and in the managed forwarders, so an
        // interior NUL is impossible.
        let symbol_bytes = CString::new(symbol)
            .expect("export symbol names never contain interior NUL")
            .into_bytes_with_nul();
        // SAFETY: the returned Symbol is a plain copy of the resolved function
        // pointer; the library is kept alive by `_library`.
        let loaded: libloading::Symbol<T> = unsafe {
            self._library
                .get(&symbol_bytes)
                .map_err(|source| CSharpError::AotExportMissing {
                    symbol: symbol.to_string(),
                    source,
                })?
        };
        Ok(*loaded)
    }
}
