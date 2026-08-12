//! Lifecycle management for the active native or managed game module.
//!
//! # Responsibilities
//!
//! - Builds and loads the selected backend during startup.
//! - Reloads native modules without dropping previously mapped libraries.
//! - Delegates managed reload polling to `csharp_runtime`.
//! - Keeps native component-schema migration beside the DLL swap it protects.

// Standard library
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::AtomicU64;

// External crates
use pill_engine::{Engine, EngineApi};

// Current crate
use crate::build::build_game_module;
use crate::csharp::CSharpRuntime;
use crate::native_library::GameLibrary;
use crate::{GameModuleBackend, GameModuleConfig};

// =============================================================================
// Constants
// =============================================================================

/// Maximum number of retired native generations kept mapped.
///
/// The immediately previous generation must stay mapped because its persist
/// metadata drives the next migration; anything older can be evicted and its
/// temporary file deleted.
const MAX_GRAVEYARD_GENERATIONS: usize = 2;

// =============================================================================
// LoadedGame
// =============================================================================

/// The backend-specific state kept alive by the host loop.
pub(crate) enum LoadedGame {
    Native {
        current: GameLibrary,
        /// Old DLLs intentionally remain mapped because engine-owned function
        /// pointers and vtables may still refer to their code.
        old_libraries: Vec<GameLibrary>,
    },
    CSharp(CSharpRuntime),
}

impl LoadedGame {
    /// Build and initialize the configured game backend.
    pub(crate) fn start(
        engine: &mut Engine,
        engine_api: &EngineApi,
        workspace_root: &Path,
        config: &GameModuleConfig,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Step 1: Build the module through the shared command runner.
        // Build before branching so both backends use the same command runner,
        // diagnostics, output validation, and initial failure behavior.
        let output_path = build_game_module(workspace_root, config, None)?;

        // Step 2: Initialize the backend-specific runtime.
        match &config.backend {
            GameModuleBackend::NativeLibrary { .. } => {
                // Native build outputs cannot be loaded in place on Windows:
                // the OS locks a mapped DLL. Load a uniquely named copy so the
                // next compilation remains free to replace the original.
                let library = GameLibrary::load_copy(&output_path, workspace_root)?;

                // Native modules register their components and systems through
                // the stable EngineApi table before the first frame is run.
                // A non-zero status means the module failed to initialize.
                let status = library.call_game_init(engine_api);
                if status != 0 {
                    return Err(
                        format!("game module initialization failed with status {status}").into(),
                    );
                }
                Ok(Self::Native {
                    current: library,
                    old_libraries: Vec::new(),
                })
            }
            // The managed runtime performs assembly discovery, component
            // registration, startup commands, and system registration itself.
            GameModuleBackend::CSharp(config) => Ok(Self::CSharp(CSharpRuntime::start(
                engine,
                workspace_root,
                config,
            )?)),
        }
    }

    /// Rebuild and replace the active module while preserving a working old
    /// generation whenever compilation, loading, or registration fails.
    ///
    /// `cancel_flag` is the watcher's reload signal: a newer save during the
    /// build aborts the in-flight compilation and the next frame retries.
    pub(crate) fn reload(
        &mut self,
        engine: &mut Engine,
        engine_api: &EngineApi,
        workspace_root: &Path,
        config: &GameModuleConfig,
        cancel_flag: Option<(&AtomicU64, u64)>,
    ) {
        match self {
            // Native reload owns schema migration and DLL lifetime handling, so
            // keep that transaction isolated in one dedicated function.
            Self::Native {
                current,
                old_libraries,
            } => reload_native(
                current,
                old_libraries,
                engine,
                engine_api,
                workspace_root,
                config,
                cancel_flag,
            ),
            // C# source changes are compiled by the host. The collectible
            // managed loader validates the rebuilt assembly's component
            // manifest and system signatures before swapping; poll_reload
            // reports the outcome and logs any rejection.
            Self::CSharp(runtime) => match build_game_module(workspace_root, config, cancel_flag) {
                Ok(_) => {
                    println!("[host] C# build complete; polling managed loader.");
                    runtime.poll_reload();
                }
                Err(error) => {
                    eprintln!("[host] C# build failed: {error}");
                    eprintln!("[host] Keeping the currently loaded C# game assembly.");
                }
            },
        }
    }

    /// Poll the collectible managed loader after its assembly debounce.
    pub(crate) fn poll_managed_reload(&mut self) {
        // Source and assembly watchers have independent debounce windows. Poll
        // every frame so a successful build is eventually observed even when
        // the assembly was not ready during the source-triggered reload call.
        if let Self::CSharp(runtime) = self {
            runtime.poll_reload();
        }
    }

    /// Invoke the native compatibility update hook after scheduler systems.
    pub(crate) fn update(&self, engine_api: &EngineApi) {
        // C# gameplay is represented entirely by registered ECS systems. Only
        // native modules retain the legacy explicit per-frame callback.
        if let Self::Native { current, .. } = self {
            current.call_game_update(engine_api);
        }
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// Reload one native generation and migrate components whose persisted schema
/// changed across the module boundary.
fn reload_native(
    current: &mut GameLibrary,
    old_libraries: &mut Vec<GameLibrary>,
    engine: &mut Engine,
    engine_api: &EngineApi,
    workspace_root: &Path,
    config: &GameModuleConfig,
    cancel_flag: Option<(&AtomicU64, u64)>,
) {
    // Step 1: Compile the new module before touching engine state.
    // Complete compilation before mutating engine state. A compiler error can
    // therefore never remove the systems belonging to the working generation.
    let output_path = match build_game_module(workspace_root, config, cancel_flag) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("[host] Build failed: {error}");
            eprintln!("[host] Keeping old game module. Fix compilation errors and save again.");
            return;
        }
    };

    // Step 2: Load and validate the replacement library transactionally.
    // Loading and symbol validation are also transactional. Keep `current`
    // untouched until a complete replacement library is ready to initialize.
    let new_library = match GameLibrary::load_copy(&output_path, workspace_root) {
        Ok(library) => library,
        Err(error) => {
            eprintln!("[host] Failed to load new library: {error}");
            eprintln!("[host] Keeping old game module. Fix errors and save again.");
            return;
        }
    };

    // Step 3: Capture old schemas, clear old systems, and initialize the new
    // generation while both DLLs remain mapped.
    // Capture the old generation's persistence functions and schemas while its
    // DLL is still mapped. Migration may need those functions after game_init
    // has registered the replacement generation's component definitions.
    let previous_metadata_by_name = engine.world().capture_persist_type_metadata();
    let previous_manifest = engine.world().persist_type_manifest();

    // Registered native system closures can point into the old DLL. Remove
    // them before game_init installs closures from the replacement module.
    println!("[host] === Reload step 1/4: clearing old systems ===");
    engine.clear_systems();
    println!("[host] === Reload step 2/4: calling game_init on new DLL ===");
    if new_library.call_game_init(engine_api) != 0 {
        // The new generation failed to register itself. Roll the engine back
        // to the previous module: game_init must be idempotent, re-registering
        // the same components and systems and only filling entities up to a
        // target count.
        eprintln!(
            "[host] New game module failed to initialize; rolling back to the previous generation."
        );
        engine.clear_systems();
        let rollback_status = current.call_game_init(engine_api);
        if rollback_status != 0 {
            eprintln!(
                "[host] Rollback of the previous generation also failed (status {rollback_status}); \
                 the host continues without gameplay systems."
            );
        }
        return;
    }

    // Match schemas by stable type name rather than runtime ComponentId: IDs
    // can differ across dynamically loaded generations, while names persist.
    let current_schema_by_name: HashMap<String, u64> = engine
        .world()
        .persist_type_manifest()
        .into_iter()
        .map(|entry| (entry.type_name, entry.schema_hash))
        .collect();
    let changed_type_names: HashSet<String> = previous_manifest
        .iter()
        .filter_map(|entry| {
            current_schema_by_name
                .get(&entry.type_name)
                .filter(|&&current_hash| current_hash != entry.schema_hash)
                .map(|_| entry.type_name.clone())
        })
        .collect();

    // Step 4: Migrate changed schemas and archive the old DLL.
    if changed_type_names.is_empty() {
        // Avoid touching archetype storage when every persisted layout is
        // byte-for-byte compatible with the previous generation.
        println!("[host] Schema unchanged for all persistable component types — fast path.");
    } else {
        // Migrate only changed component types. Unchanged columns keep their
        // allocations and component ticks, which makes the common reload path
        // both faster and less disruptive to change detection.
        println!(
            "[host] === Reload step 3/4: selectively migrating {} component type(s) ===",
            changed_type_names.len()
        );
        let report = engine.world_mut().migrate_changed_persistable_components(
            &previous_metadata_by_name,
            &changed_type_names,
        );

        println!(
            "[host] Selective migration complete: {} type(s), {} entities.",
            report.migrated_type_count, report.migrated_entity_count
        );
        if !report.skipped_type_names.is_empty() {
            eprintln!(
                "[host] Selective migration skipped {} type(s): {:?}",
                report.skipped_type_names.len(),
                report.skipped_type_names
            );
        }
    }

    // Do not unload the previous DLL. Persist metadata, component operations,
    // or other engine-owned pointers may still reference its executable code.
    // Moving it into the graveyard keeps those addresses valid permanently.
    println!("[host] === Reload step 4/4: archiving old DLL, swapping ===");
    old_libraries.push(std::mem::replace(current, new_library));

    // Keep the graveyard bounded. Engine-owned pointers only reference the
    // immediately previous generation (its persist metadata drives the next
    // migration), so anything older than the cap can be evicted safely.
    if old_libraries.len() > MAX_GRAVEYARD_GENERATIONS {
        let evicted = old_libraries.remove(0);
        let temporary_path = evicted.temporary_path().to_path_buf();
        // Drop the library first so the module is unmapped; Windows refuses
        // to delete a file that is still mapped into the process.
        drop(evicted);
        if let Err(error) = std::fs::remove_file(&temporary_path) {
            eprintln!(
                "[host] Failed to remove evicted temporary DLL {}: {error}",
                temporary_path.display()
            );
        }
    }
    println!(
        "[host] Hot-reload complete ({} entities, {} old libs in graveyard).",
        engine.world().entity_count(),
        old_libraries.len()
    );
}
