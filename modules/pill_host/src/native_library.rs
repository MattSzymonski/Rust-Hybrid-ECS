//! Native project-library loading and Windows-safe temporary-copy handling.
//!
//! Loaded modules are copied to unique paths before opening. This permits a
//! newly compiled DLL to replace the build output while an older generation
//! remains mapped for outstanding function pointers and vtables.
//!
//! # Responsibilities
//!
//! - Load native project libraries from unique temporary paths.
//! - Validate required exports before returning a loaded library.
//! - Call native registration and per-frame update entry points.
//! - Remove temporary copies left behind by earlier host processes.
//!
//! # Design
//!
//! The native ABI is a fixed export contract:
//!
//! - `project_init(*const EngineApi) -> u32` — required. Registers components
//!   and systems before the first frame and returns zero on success; any
//!   other status aborts the load transaction.
//! - `project_update(*const EngineApi)` — optional. Called once per frame for
//!   modules that keep the legacy explicit update hook.
//!
//! Both exports are resolved and cached when the library is loaded, so the
//! frame loop never performs a dynamic lookup or panics on a missing
//! optional export. `project_init` must be idempotent: a failed generation is
//! rolled back by re-initializing the previous module.

// Standard library
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

// External crates
use libloading::{Library, Symbol};
use pill_core::error::LibraryError;
use pill_core::{debug, info};
use pill_engine::EngineApi;

// =============================================================================
// Constants
// =============================================================================

/// Directory where temporary native-library copies are stored.
const TEMPORARY_DIRECTORY: &str = "pill_standalone_temp";

/// Monotonic suffix ensuring temporary copies never collide, even when the
/// system clock repeats or moves backwards.
static TEMPORARY_COPY_COUNTER: AtomicU64 = AtomicU64::new(0);

// =============================================================================
// Types + Impls
// =============================================================================

/// Signature of the required registration entry point.
///
/// Returns zero on success; any non-zero status reports a registration
/// failure and keeps the previous generation loaded.
type ModuleInitFn = unsafe extern "C" fn(*const EngineApi) -> u32;

/// Signature of the optional per-frame entry point.
///
/// Modules that omit this export run entirely through their registered
/// scheduler systems instead of an explicit per-frame hook.
type ModuleUpdateFn = unsafe extern "C" fn(*const EngineApi);

/// Signature of the optional ABI revision export.
type ModuleAbiVersionFn = unsafe extern "C" fn() -> u32;

/// Export names one loaded native library is expected to provide.
///
/// The project module and optional engine modules use different export names
/// so one crate can implement both contracts, and so an optional module can
/// carry a version guard the older project contract never had.
pub(crate) struct NativeEntryPoints {
    /// Required registration entry point.
    pub init_symbol: &'static [u8],
    /// Optional per-frame entry point.
    pub update_symbol: &'static [u8],
    /// Optional ABI revision export, read at load time when present.
    pub abi_version_symbol: &'static [u8],
}

/// Export contract of the project module.
pub(crate) const PROJECT_ENTRY_POINTS: NativeEntryPoints = NativeEntryPoints {
    init_symbol: b"project_init",
    update_symbol: b"project_update",
    abi_version_symbol: b"project_abi_version",
};

/// Export contract of an optional engine module.
pub(crate) const OPTIONAL_MODULE_ENTRY_POINTS: NativeEntryPoints = NativeEntryPoints {
    init_symbol: b"pill_module_init",
    update_symbol: b"pill_module_update",
    abi_version_symbol: b"pill_module_abi_version",
};

/// Owns one loaded native library, either the project or an optional module.
///
/// Export symbols are resolved once during loading and stored as raw function
/// pointers. The pointers stay valid for as long as the `library` field keeps
/// the module mapped, so frame-loop calls never perform a dynamic lookup or
/// panic on a missing optional export.
pub(crate) struct NativeLibrary {
    /// Loaded module handle; keeps the native library mapped in memory.
    library: Option<Library>,
    /// Required registration entry point, resolved once at load time.
    module_init: ModuleInitFn,
    /// Optional per-frame entry point; `None` when the module has none.
    module_update: Option<ModuleUpdateFn>,
    /// ABI revision the module reports, when it exports one.
    abi_version: Option<u32>,
    /// Temporary copy backing this library; deleted when the library drops.
    temporary_path: PathBuf,
}

impl NativeLibrary {
    /// Copy and load the built shared library from a unique temporary path.
    ///
    /// # Errors
    ///
    /// Returns an error if the temporary directory cannot be created, the
    /// built library cannot be copied, or the copy is not a valid native
    /// library exporting the required `project_init` symbol.
    pub(crate) fn load_copy(
        build_output: &Path,
        workspace_root: &Path,
        module_name: &str,
        entry_points: &NativeEntryPoints,
    ) -> Result<Self, LibraryError> {
        // Step 1: Prepare this process's temporary directory and a unique
        // target path. Scoping the directory per process id keeps concurrent
        // host instances from touching each other's copies.
        let temporary_directory = process_temporary_directory(workspace_root);
        std::fs::create_dir_all(&temporary_directory).map_err(|source| {
            LibraryError::TemporaryDirectory {
                directory: temporary_directory.display().to_string(),
                source,
            }
        })?;

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let counter = TEMPORARY_COPY_COUNTER.fetch_add(1, Ordering::Relaxed);
        let extension = build_output
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("dll");
        // Prefix with the module name so several modules, each reloading on
        // its own schedule, never collide inside one process directory.
        let temporary_path =
            temporary_directory.join(format!("{module_name}_{timestamp}_{counter}.{extension}"));

        // Step 2: Copy the built library to the unique temporary path.
        std::fs::copy(build_output, &temporary_path).map_err(|source| {
            LibraryError::CopyFailed {
                source_path: build_output.display().to_string(),
                target_path: temporary_path.display().to_string(),
                source,
            }
        })?;
        debug!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            path = %temporary_path.display(),
            "copied project DLL"
        );

        // Step 3: Load the copy and validate its required exports.
        // SAFETY: `temporary_path` was just written by `std::fs::copy` from
        // the freshly built output, so it is a complete native module on
        // disk. `Self::load` validates the required exports before returning,
        // and the returned `ProjectLibrary` owns the mapping for its lifetime.
        let native_library =
            unsafe { Self::load(&temporary_path, temporary_path.clone(), entry_points) }?;
        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            module = module_name,
            abi_version = ?native_library.abi_version,
            "module DLL loaded successfully"
        );
        Ok(native_library)
    }

    /// ABI revision reported by the module, when it exports one.
    ///
    /// `None` means the library predates the versioned contract; the caller
    /// decides whether that is acceptable for its module kind.
    pub(crate) fn abi_version(&self) -> Option<u32> {
        self.abi_version
    }

    /// Load a module and verify its required exports.
    ///
    /// # Safety
    ///
    /// `path` must point to a valid native library whose `project_init` export
    /// uses the expected C ABI.
    unsafe fn load(
        path: &Path,
        temporary_path: PathBuf,
        entry_points: &NativeEntryPoints,
    ) -> Result<Self, LibraryError> {
        // Step 1: Open the native library and map it into this process.
        // SAFETY: The `# Safety` contract of `load` guarantees `path` names a
        // valid native library. `Library::new` maps the module and runs its
        // constructors; the returned handle keeps it mapped and is stored in
        // the `ProjectLibrary` for the module's whole lifetime.
        let library = unsafe {
            Library::new(path).map_err(|source| LibraryError::LoadFailed {
                path: path.display().to_string(),
                source,
            })?
        };

        // Step 2: Resolve the required `project_init` export.
        // SAFETY: `project_init` is a mandatory export of the native ABI
        // contract, so every supported module provides it, and it is resolved
        // here as a pointer with the statically known C ABI signature. The
        // pointer stays valid because the `library` handle keeps the module
        // mapped for the lifetime of the returned `ProjectLibrary`.
        let module_init: Symbol<ModuleInitFn> = unsafe {
            library
                .get(entry_points.init_symbol)
                .map_err(|source| LibraryError::MissingExport {
                    symbol: String::from_utf8_lossy(entry_points.init_symbol).to_string(),
                    source,
                })?
        };

        // Step 3: Resolve the optional `project_update` export.
        // SAFETY: `project_update` is optional; when present it is resolved as a
        // pointer with the statically known C ABI signature, and when absent
        // the lookup fails and the error is discarded, leaving `project_update`
        // as `None`. The pointer stays valid because the `library` handle
        // keeps the module mapped.
        let module_update: Option<Symbol<ModuleUpdateFn>> =
            unsafe { library.get(entry_points.update_symbol) }.ok();

        // Step 4: Read the optional ABI revision before any other call, so a
        // caller can reject an incompatible module without ever handing it a
        // pointer into engine memory.
        // SAFETY: The export is optional; when present it is resolved with the
        // statically known C ABI signature and called immediately while the
        // library is mapped. It takes no arguments and returns a plain integer,
        // so the call cannot touch host state.
        let abi_version: Option<u32> =
            unsafe { library.get::<ModuleAbiVersionFn>(entry_points.abi_version_symbol) }
                .ok()
                .map(|symbol| unsafe { symbol() });

        // Step 5: Copy the resolved pointers out of the borrowed Symbol
        // wrappers. The `library` field keeps the module mapped, so these raw
        // pointers remain valid for the complete lifetime of the returned
        // `NativeLibrary`.
        let module_init_pointer = *module_init;
        let module_update_pointer = module_update.map(|symbol| *symbol);
        Ok(Self {
            library: Some(library),
            module_init: module_init_pointer,
            module_update: module_update_pointer,
            abi_version,
            temporary_path,
        })
    }

    /// Call the module's registration entry point.
    ///
    /// Returns the module's status code: zero reports successful registration;
    /// any non-zero value means the module failed to initialize and the
    /// previous generation must remain active.
    pub(crate) fn call_init(&self, api: &EngineApi) -> u32 {
        // SAFETY: The init export was validated to exist and to use this C ABI
        // signature when the library was loaded, and the `library` field
        // keeps the module mapped for as long as this `NativeLibrary` lives.
        // `api` is borrowed immutably for the whole call and outlives it
        // because the host creates the engine API before loading the module.
        unsafe { (self.module_init)(api as *const EngineApi) }
    }

    /// Call the optional native per-frame update entry point, when exported.
    ///
    /// Modules that omit `project_update` run entirely through their registered
    /// scheduler systems, so a missing export is a no-op rather than an error.
    pub(crate) fn call_update(&self, api: &EngineApi) {
        if let Some(module_update) = self.module_update {
            // SAFETY: The update export was validated when the library was
            // loaded and the `library` field keeps the module mapped for the
            // lifetime of this `NativeLibrary`. `api` is borrowed immutably for
            // the whole call, matching the read-only access the native update
            // hook expects.
            unsafe { module_update(api as *const EngineApi) };
        }
    }
}

impl Drop for NativeLibrary {
    /// Unmap the module and delete its temporary copy.
    ///
    /// The library handle is dropped explicitly before the file is removed
    /// because Windows refuses to delete a file that is still mapped into the
    /// process. Cleanup failures are reported rather than swallowed.
    fn drop(&mut self) {
        drop(self.library.take());
        if let Err(error) = std::fs::remove_file(&self.temporary_path) {
            eprintln!(
                "[host] Failed to remove temporary DLL {}: {error}",
                self.temporary_path.display()
            );
        }
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// Directory used by this host process for temporary native-library copies.
///
/// Scoping the directory per process id means one host instance can never
/// delete or overwrite the copies of another instance running against the
/// same workspace.
fn process_temporary_directory(workspace_root: &Path) -> PathBuf {
    workspace_root
        .join(TEMPORARY_DIRECTORY)
        .join(std::process::id().to_string())
}

/// Whether a process with the given id is still running.
///
/// Linux probes `/proc` directly. Other platforms cannot probe process
/// liveness cheaply, so stale directories are detected by attempting the
/// removal: a live process keeps its mapped modules locked and the deletion
/// fails naturally.
fn process_is_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::path::Path::new("/proc").join(pid.to_string()).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        false
    }
}

/// Remove temporary copies left by earlier runs of this host process.
///
/// Directories belonging to other, still-running host instances are skipped;
/// stale directories from crashed processes are removed. Removal failures are
/// reported instead of swallowed.
pub(crate) fn cleanup_temporary_files(workspace_root: &Path) {
    let temporary_root = workspace_root.join(TEMPORARY_DIRECTORY);
    let Ok(entries) = std::fs::read_dir(&temporary_root) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if pid != std::process::id() && process_is_alive(pid) {
            continue;
        }
        match std::fs::remove_dir_all(entry.path()) {
            Ok(()) => {
                if pid != std::process::id() {
                    println!("[host] Cleaned up stale temporary files from process {pid}.");
                }
            }
            Err(error) => {
                if pid == std::process::id() {
                    eprintln!(
                        "[host] Could not remove temporary directory {}: {error}",
                        entry.path().display()
                    );
                } else {
                    // On platforms without process probing the removal may
                    // fail simply because another live host holds the files.
                    println!(
                        "[host] Temporary directory {} left in place (possibly still in use): {error}",
                        entry.path().display()
                    );
                }
            }
        }
    }
}
