//! Native game-library loading and Windows-safe temporary-copy handling.
//!
//! Loaded modules are copied to unique paths before opening. This permits a
//! newly compiled DLL to replace the build output while an older generation
//! remains mapped for outstanding function pointers and vtables.

use std::path::Path;

use ecs_hybrid::EngineApi;
use libloading::{Library, Symbol};

/// Directory where temporary native-library copies are stored.
const TEMPORARY_DIRECTORY: &str = "standalone_temp";

/// Owns one loaded native game module.
pub(crate) struct GameLibrary {
    library: Library,
}

impl GameLibrary {
    /// Copy and load the built shared library from a unique temporary path.
    pub(crate) fn load_copy(
        build_output: &Path,
        workspace_root: &Path,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let temporary_directory = workspace_root.join(TEMPORARY_DIRECTORY);
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

        std::fs::copy(build_output, &temporary_path)?;
        println!("[host] Copied DLL to: {}", temporary_path.display());

        // SAFETY: The configured build just produced this module and its
        // required exports are validated before the handle is returned.
        let game_library = unsafe { Self::load(&temporary_path)? };
        println!("[host] Game DLL loaded successfully.");
        Ok(game_library)
    }

    /// Load a module and verify its required exports.
    ///
    /// # Safety
    ///
    /// `path` must point to a valid native library whose `game_init` export
    /// uses the expected C ABI.
    unsafe fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        // SAFETY: The caller guarantees that `path` points to a native module.
        let library = unsafe { Library::new(path)? };

        // SAFETY: Reading the export validates its name and expected type. It
        // is not invoked until the engine API has been created.
        let _: Symbol<unsafe extern "C" fn(*const EngineApi)> =
            unsafe { library.get(b"game_init")? };

        Ok(Self { library })
    }

    /// Call the module's registration entry point.
    pub(crate) fn call_game_init(&self, api: &EngineApi) {
        // SAFETY: The export was validated while loading and `api` remains
        // valid for the complete duration of this call.
        unsafe {
            let game_init: Symbol<unsafe extern "C" fn(*const EngineApi)> = self
                .library
                .get(b"game_init")
                .expect("game_init symbol missing");
            game_init(api as *const EngineApi);
        }
    }

    /// Call the optional native per-frame update entry point.
    pub(crate) fn call_game_update(&self, api: &EngineApi) {
        // SAFETY: Native game modules use the host's fixed C ABI and `api`
        // remains valid for the complete duration of this call.
        unsafe {
            let game_update: Symbol<unsafe extern "C" fn(*const EngineApi)> = self
                .library
                .get(b"game_update")
                .expect("game_update symbol missing");
            game_update(api as *const EngineApi);
        }
    }
}

/// Remove temporary native-library copies left by earlier host processes.
pub(crate) fn cleanup_temporary_files(workspace_root: &Path) {
    let temporary_directory = workspace_root.join(TEMPORARY_DIRECTORY);
    if temporary_directory.exists() {
        let _ = std::fs::remove_dir_all(&temporary_directory);
        println!("[host] Cleaned up leftover temporary files.");
    }
}
