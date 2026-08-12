//! Native game-library loading and Windows-safe temporary-copy handling.
//!
//! Loaded modules are copied to unique paths before opening. This permits a
//! newly compiled DLL to replace the build output while an older generation
//! remains mapped for outstanding function pointers and vtables.
//!
//! # Responsibilities
//!
//! - Load native game libraries from unique temporary paths.
//! - Validate required exports before returning a loaded library.
//! - Call native registration and per-frame update entry points.
//! - Remove temporary copies left behind by earlier host processes.
//!
//! # Design
//!
//! The native ABI is a fixed export contract:
//!
//! - `game_init(*const EngineApi) -> u32` — required. Registers components
//!   and systems before the first frame and returns zero on success; any
//!   other status aborts the load transaction.
//! - `game_update(*const EngineApi)` — optional. Called once per frame for
//!   modules that keep the legacy explicit update hook.
//!
//! Both exports are resolved and cached when the library is loaded, so the
//! frame loop never performs a dynamic lookup or panics on a missing
//! optional export. `game_init` must be idempotent: a failed generation is
//! rolled back by re-initializing the previous module.

// Standard library
use std::path::{Path, PathBuf};

// External crates
use libloading::{Library, Symbol};
use pill_engine::EngineApi;

// =============================================================================
// Constants
// =============================================================================

/// Directory where temporary native-library copies are stored.
const TEMPORARY_DIRECTORY: &str = "standalone_temp";

// =============================================================================
// Types + Impls
// =============================================================================

/// Signature of the required `game_init` registration entry point.
///
/// Returns zero on success; any non-zero status reports a registration
/// failure and keeps the previous generation loaded.
type GameInitFn = unsafe extern "C" fn(*const EngineApi) -> u32;

/// Signature of the optional `game_update` per-frame entry point.
type GameUpdateFn = unsafe extern "C" fn(*const EngineApi);

/// Owns one loaded native game module.
///
/// Export symbols are resolved once during loading and stored as raw function
/// pointers. The pointers stay valid for as long as the `library` field keeps
/// the module mapped, so frame-loop calls never perform a dynamic lookup or
/// panic on a missing optional export.
pub(crate) struct GameLibrary {
    library: Option<Library>,
    game_init: GameInitFn,
    game_update: Option<GameUpdateFn>,
    /// Temporary copy backing this library; deleted when the library drops.
    temporary_path: PathBuf,
}

impl GameLibrary {
    /// Copy and load the built shared library from a unique temporary path.
    ///
    /// # Errors
    ///
    /// Returns an error if the temporary directory cannot be created, the
    /// built library cannot be copied, or the copy is not a valid native
    /// library exporting the required `game_init` symbol.
    pub(crate) fn load_copy(
        build_output: &Path,
        workspace_root: &Path,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Step 1: Prepare this process's temporary directory and a unique
        // target path. Scoping the directory per process id keeps concurrent
        // host instances from touching each other's copies.
        let temporary_directory = process_temporary_directory(workspace_root);
        std::fs::create_dir_all(&temporary_directory)?;

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let extension = build_output
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("dll");
        let temporary_path = temporary_directory.join(format!("game_{timestamp}.{extension}"));

        // Step 2: Copy the built library to the unique temporary path.
        std::fs::copy(build_output, &temporary_path)?;
        println!("[host] Copied DLL to: {}", temporary_path.display());

        // Step 3: Load the copy and validate its required exports.
        // SAFETY: The configured build just produced this module and its
        // required exports are validated before the handle is returned.
        let game_library = unsafe { Self::load(&temporary_path, temporary_path.clone()) }?;
        println!("[host] Game DLL loaded successfully.");
        Ok(game_library)
    }

    /// Load a module and verify its required exports.
    ///
    /// # Safety
    ///
    /// `path` must point to a valid native library whose `game_init` export
    /// uses the expected C ABI.
    unsafe fn load(
        path: &Path,
        temporary_path: PathBuf,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // SAFETY: The caller guarantees that `path` points to a native module.
        let library = unsafe { Library::new(path)? };

        // SAFETY: Resolving the export validates its name and expected type.
        // The symbol is not invoked until the engine API has been created.
        let game_init: Symbol<GameInitFn> = unsafe { library.get(b"game_init")? };

        // `game_update` is optional: modules without the legacy per-frame
        // update simply run their registered scheduler systems.
        let game_update: Option<Symbol<GameUpdateFn>> = unsafe { library.get(b"game_update") }.ok();

        // Copy the resolved pointers out of the borrowed Symbol wrappers. The
        // `library` field keeps the module mapped, so these raw pointers
        // remain valid for the complete lifetime of the returned GameLibrary.
        let game_init_pointer = *game_init;
        let game_update_pointer = game_update.map(|symbol| *symbol);
        Ok(Self {
            library: Some(library),
            game_init: game_init_pointer,
            game_update: game_update_pointer,
            temporary_path,
        })
    }

    /// Call the module's registration entry point.
    ///
    /// Returns the module's status code: zero reports successful registration;
    /// any non-zero value means the module failed to initialize and the
    /// previous generation must remain active.
    pub(crate) fn call_game_init(&self, api: &EngineApi) -> u32 {
        // SAFETY: The export was validated while loading and `api` remains
        // valid for the complete duration of this call.
        unsafe { (self.game_init)(api as *const EngineApi) }
    }

    /// Call the optional native per-frame update entry point, when exported.
    ///
    /// Modules that omit `game_update` run entirely through their registered
    /// scheduler systems, so a missing export is a no-op rather than an error.
    pub(crate) fn call_game_update(&self, api: &EngineApi) {
        if let Some(game_update) = self.game_update {
            // SAFETY: The export was validated while loading and `api`
            // remains valid for the complete duration of this call.
            unsafe { game_update(api as *const EngineApi) };
        }
    }
}

impl Drop for GameLibrary {
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
