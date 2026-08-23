//! Lifecycle management for the active native or managed project module.
//!
//! # Responsibilities
//!
//! - Builds and loads the selected backend during startup.
//! - Reloads native modules without dropping previously mapped libraries.
//! - Delegates managed reload polling to `csharp_runtime`.
//! - Keeps native component-schema migration beside the DLL swap it protects.
//!
//! # Design
//!
//! The host keeps exactly one [`LoadedProject`] alive at a time. Native backends
//! are reloaded transactionally by [`reload_native`]: the previous DLL stays
//! mapped in a bounded graveyard while changed persist schemas migrate, so
//! engine-owned pointers into retired code remain valid. Managed backends
//! delegate assembly discovery and validation to [`CSharpRuntime`], which
//! reports success or rejection through `poll_reload`.

// Standard library
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::time::Instant;

// External crates
use pill_core::error::{HostError, LibraryError};
use pill_core::{debug, error, info, warn};
use pill_engine::{Engine, EngineApi, SystemOwner};

// Current crate
use crate::analytics;
use crate::build_runner::build_project_module;
use crate::csharp::CSharpRuntime;
use crate::native_library::{NativeLibrary, PROJECT_ENTRY_POINTS};
use crate::{ProjectModuleBackend, ProjectModuleConfig};

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
// LoadedProject
// =============================================================================

/// The backend-specific state kept alive by the host loop.
///
/// The enum lets the host hold either a mapped native library or a managed
/// runtime behind one interface; the variant in use is fixed at startup and
/// only changes across a full restart.
pub(crate) enum LoadedProject {
    /// A mapped native module plus any retired DLLs that must stay mapped.
    Native {
        current: NativeLibrary,
        /// Old DLLs intentionally remain mapped because engine-owned function
        /// pointers and vtables may still refer to their code.
        old_libraries: Vec<NativeLibrary>,
        /// Persistable component type names the last `project_init` registered,
        /// used to detect types the next generation forgets to re-register.
        registered_type_names: Vec<String>,
    },
    /// A collectible managed runtime hosting the C# project assembly.
    CSharp(CSharpRuntime),
}

impl LoadedProject {
    /// Build and initialize the configured project backend.
    ///
    /// # Errors
    ///
    /// Returns `HostError` when the module fails to compile, when the native
    /// library cannot be loaded, or when the module's `project_init` reports a
    /// non-zero initialization status.
    pub(crate) fn start(
        engine: &mut Engine,
        engine_api: &EngineApi,
        workspace_root: &Path,
        config: &ProjectModuleConfig,
        module_exposed_components: &[crate::csharp::ModuleExposedComponent],
    ) -> Result<Self, HostError> {
        // Step 1: Build the module through the shared command runner.
        // Build before branching so both backends use the same command runner,
        // diagnostics, output validation, and initial failure behavior.
        let output_path = build_project_module(workspace_root, config, None)?;

        // Step 2: Initialize the backend-specific runtime.
        match &config.backend {
            ProjectModuleBackend::NativeLibrary { .. } => {
                // Native build outputs cannot be loaded in place on Windows:
                // the OS locks a mapped DLL. Load a uniquely named copy so the
                // next compilation remains free to replace the original.
                let library = NativeLibrary::load_copy(
                    &output_path,
                    workspace_root,
                    &config.name,
                    &PROJECT_ENTRY_POINTS,
                )?;

                // Native modules register their components and systems through
                // the stable EngineApi table before the first frame is run.
                let init_started = Instant::now();
                // Capture the registration sequence before init so the exact set
                // of persistable types this generation registered is recorded.
                let registration_sequence = engine.world().persist_registration_sequence();
                let status = library.call_init(engine_api);
                analytics::record_init(&config.name, init_started.elapsed().as_secs_f64() * 1000.0);
                if status != 0 {
                    return Err(LibraryError::InitializationFailed { status }.into());
                }
                let registered_type_names = engine
                    .world()
                    .persist_type_names_registered_since(registration_sequence);
                Ok(Self::Native {
                    current: library,
                    old_libraries: Vec::new(),
                    registered_type_names,
                })
            }
            // The managed runtime performs assembly discovery, component
            // registration, startup commands, and system registration itself.
            ProjectModuleBackend::CSharp(config) => Ok(Self::CSharp(CSharpRuntime::start(
                engine,
                workspace_root,
                config,
                module_exposed_components,
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
        config: &ProjectModuleConfig,
        cancel_flag: Option<(&AtomicU64, u64)>,
    ) {
        match self {
            // Native reload owns schema migration and DLL lifetime handling, so
            // keep that transaction isolated in one dedicated function.
            Self::Native {
                current,
                old_libraries,
                registered_type_names,
            } => reload_native(
                current,
                old_libraries,
                registered_type_names,
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
            Self::CSharp(runtime) => {
                match build_project_module(workspace_root, config, cancel_flag) {
                    Ok(_) => {
                        info!(
                            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                            "C# build complete; polling managed loader"
                        );
                        runtime.poll_reload();
                    }
                    Err(error) => {
                        error!(
                            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                            error = %error,
                            "C# build failed; keeping the currently loaded C# project assembly"
                        );
                    }
                }
            }
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
            current.call_update(engine_api);
        }
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// Reload one native generation and migrate components whose persisted schema
/// changed across the module boundary.
fn reload_native(
    current: &mut NativeLibrary,
    old_libraries: &mut Vec<NativeLibrary>,
    registered_type_names: &mut Vec<String>,
    engine: &mut Engine,
    engine_api: &EngineApi,
    workspace_root: &Path,
    config: &ProjectModuleConfig,
    cancel_flag: Option<(&AtomicU64, u64)>,
) {
    // Step 1: Compile the new module before touching engine state, so a
    // compiler error can never remove the systems of the working generation.
    let output_path = match build_project_module(workspace_root, config, cancel_flag) {
        Ok(path) => path,
        Err(error) => {
            error!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                error = %error,
                "build failed; keeping the old project module"
            );
            return;
        }
    };

    // Step 2: Load and validate the replacement library transactionally.
    // Keep `current` untouched until a complete replacement library is ready
    // to initialize.
    let new_library = match NativeLibrary::load_copy(
        &output_path,
        workspace_root,
        &config.name,
        &PROJECT_ENTRY_POINTS,
    ) {
        Ok(library) => library,
        Err(error) => {
            error!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                error = %error,
                "failed to load the new library; keeping the old project module"
            );
            return;
        }
    };

    // Step 3: Capture old schemas, clear old systems, and initialize the new
    // generation while both DLLs remain mapped. Migration may need the old
    // persistence functions after project_init registers the replacement
    // generation's component definitions.
    let previous_metadata_by_name = engine.world().capture_persist_type_metadata();
    let previous_manifest = engine.world().persist_type_manifest();

    // Registered native system closures can point into the old DLL. Remove
    // them before project_init installs closures from the replacement module.
    // Only the project's own systems are cleared: optional modules are still
    // running their previous generation, and their systems must survive a
    // project reload untouched.
    debug!(
        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
        "reload step 1/4: clearing old systems"
    );
    engine.clear_systems_owned_by(SystemOwner::PROJECT);
    debug!(
        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
        "reload step 2/4: calling project_init on the new module"
    );
    let init_started = Instant::now();
    // Capture the registration sequence before init so the types this new
    // generation registered can be compared against the previous ones.
    let registration_sequence = engine.world().persist_registration_sequence();
    let component_registration_sequence = engine.world().component_registration_sequence();
    if new_library.call_init(engine_api) != 0 {
        // The new generation failed to register itself. Roll the engine back
        // to the previous module: project_init must be idempotent, re-registering
        // the same components and systems and only filling entities up to a
        // target count.
        error!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            "new project module failed to initialize; rolling back to the previous generation"
        );
        engine.clear_systems_owned_by(SystemOwner::PROJECT);
        let rollback_status = current.call_init(engine_api);
        if rollback_status != 0 {
            error!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                status = rollback_status,
                "rollback of the previous generation also failed; the host continues without gameplay systems"
            );
        }
        return;
    }
    analytics::record_init(&config.name, init_started.elapsed().as_secs_f64() * 1000.0);

    // Detect persistable component types the new generation stopped
    // registering. Such data is NOT wiped by migration — the type is absent
    // from the changed-name set, so its column and metadata linger while the
    // new generation cannot read them. Surface it instead of letting the type
    // silently orphan.
    let newly_registered = engine
        .world()
        .persist_type_names_registered_since(registration_sequence);
    let all_registered = engine
        .world()
        .registered_component_names_since(component_registration_sequence);
    let forgotten_type_names: Vec<String> = registered_type_names
        .iter()
        .filter(|name| !newly_registered.iter().any(|current| current == *name))
        .cloned()
        .collect();
    if !forgotten_type_names.is_empty() {
        warn!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            module = config.name.as_str(),
            forgotten_types = ?forgotten_type_names,
            "component type(s) no longer registered by the project; their data stays in the world \
             but is orphaned (the new generation cannot read it)"
        );

        // Drop the orphaned columns only for types the new generation does
        // not register at all (not even as a plain component). A type merely
        // downgraded from persistable to plain keeps live data, so its columns
        // must survive. This runs while the generation that last registered
        // the type is still mapped, so the drop is safe.
        let truly_forgotten: Vec<String> = forgotten_type_names
            .iter()
            .filter(|name| !all_registered.iter().any(|current| current == *name))
            .cloned()
            .collect();
        if !truly_forgotten.is_empty() {
            let dropped_entities = engine
                .world_mut()
                .drop_forgotten_components(&truly_forgotten);
            debug!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                module = config.name.as_str(),
                dropped_entities,
                "dropped orphaned columns for component types no longer registered"
            );
        }
    }
    *registered_type_names = newly_registered;

    // Step 3b: Re-home every native storage column to the freshly loaded
    // generation's function table. Columns created by older generations hold
    // function pointers into their own DLL; refreshing them here (the old
    // DLLs are still mapped) keeps drops and upcasts valid when those DLLs
    // are later evicted from the reload graveyard.
    engine.world_mut().rehome_native_columns();

    // Match schemas by stable type name rather than runtime ComponentId: IDs
    // can differ across dynamically loaded generations, while names persist.
    let migrate_started = Instant::now();
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
        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            "schema unchanged for all persistable component types - fast path"
        );
    } else {
        // Migrate only changed component types. Unchanged columns keep their
        // allocations and component ticks, which makes the common reload path
        // both faster and less disruptive to change detection.
        debug!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            changed_types = changed_type_names.len(),
            "reload step 3/4: selectively migrating changed component types"
        );
        let report = engine.world_mut().migrate_changed_persistable_components(
            &previous_metadata_by_name,
            &changed_type_names,
        );

        debug!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            migrated_types = report.migrated_type_count,
            migrated_entities = report.migrated_entity_count,
            "selective migration complete"
        );
        if !report.skipped_type_names.is_empty() {
            warn!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                skipped_types = ?report.skipped_type_names,
                "selective migration skipped some component types"
            );
        }
    }
    analytics::record_migrate(
        &config.name,
        migrate_started.elapsed().as_secs_f64() * 1000.0,
    );

    // Do not unload the previous DLL. Persist metadata, component operations,
    // or other engine-owned pointers may still reference its executable code.
    // Moving it into the graveyard keeps those addresses valid permanently.
    debug!(
        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
        "reload step 4/4: archiving old DLL, swapping generation"
    );
    old_libraries.push(std::mem::replace(current, new_library));

    // Keep the graveyard bounded. Engine-owned pointers only reference the
    // immediately previous generation (its persist metadata drives the next
    // migration), so anything older than the cap can be evicted safely.
    if old_libraries.len() > MAX_GRAVEYARD_GENERATIONS {
        // Dropping the evicted generation unmaps its module and deletes its
        // temporary copy on disk; cleanup errors are reported by the Drop.
        drop(old_libraries.remove(0));
    }
    analytics::record_reload(&config.name);
    info!(
        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
        entities = engine.world().entity_count(),
        graveyard = old_libraries.len(),
        "hot reload complete"
    );
}
