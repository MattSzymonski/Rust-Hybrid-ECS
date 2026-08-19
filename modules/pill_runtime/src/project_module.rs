//! Lifecycle management for the active native or managed project module.
//!
//! # Responsibilities
//!
//! - Loads and initializes the selected backend when a runtime generation starts.
//! - Reloads native modules without dropping previously mapped libraries.
//! - Delegates managed reload polling to `csharp_runtime`.
//! - Keeps native component-schema migration beside the DLL swap it protects.
//!
//! # Design
//!
//! The runtime keeps exactly one [`LoadedProject`] alive at a time. Native
//! backends are reloaded transactionally by [`reload_native`]: the previous DLL
//! stays mapped while changed persist schemas migrate, so engine-owned pointers
//! into retired code remain valid. Managed backends delegate assembly discovery
//! and validation to [`CSharpRuntime`], which reports success or rejection
//! through `poll_reload`.
//!
//! The project module is loaded by the runtime rather than the host because
//! the `EngineApi` function pointers a project receives point into the
//! runtime's own code. An engine reload therefore always carries the project
//! with it: the replacement runtime loads the project itself as part of coming
//! up, and old and new generations can never be mixed.
//!
//! ## Why retired project libraries are never unloaded
//!
//! A retired generation is kept mapped for the whole process lifetime rather
//! than evicted after a fixed number of newer ones. Loading a project module
//! plants pointers into its code all over the world: component storage vtables
//! inside every archetype it created entities in, boxed storage factories,
//! component copiers, persistence function pointers, registered system
//! closures, and the drop glue for every value of a type it defined. Those
//! references outlive the generation that installed them for as long as any
//! entity, archetype, or registry entry from it survives - which is the normal
//! case, because a reload preserves the world.
//!
//! Evicting an older generation therefore unmaps code the live world can still
//! reach, which shows up as an access violation several reloads later, far from
//! the eviction that caused it. Nothing tracks that reachability, so the only
//! sound policy is to keep every generation mapped. The cost is bounded by how
//! many times a developer saves during one session, and the staged copies on
//! disk are cleaned up by the next host startup.

// Standard library
use std::collections::{HashMap, HashSet};
use std::path::Path;

// External crates
use pill_core::error::{HostError, LibraryError};
use pill_core::{debug, error, info, warn};
use pill_engine::{Engine, EngineApi};

// Current crate
use crate::csharp::CSharpRuntime;
use crate::native_library::ProjectLibrary;
use crate::project::ProjectDescriptor;

// =============================================================================
// LoadedProject
// =============================================================================

/// The backend-specific state kept alive by the runtime's frame loop.
///
/// The enum lets the runtime hold either a mapped native library, a managed
/// runtime, or no project at all behind one interface; the variant in use is
/// fixed when the generation starts.
pub(crate) enum LoadedProject {
    /// No project module is loaded; the world runs empty.
    None,
    /// A mapped native module plus any retired DLLs that must stay mapped.
    Native {
        current: ProjectLibrary,
        /// Old DLLs intentionally remain mapped because engine-owned function
        /// pointers and vtables may still refer to their code.
        old_libraries: Vec<ProjectLibrary>,
    },
    /// A collectible managed runtime hosting the C# project assembly.
    CSharp(CSharpRuntime),
}

impl LoadedProject {
    /// Load and initialize the configured project backend.
    ///
    /// The host has already built the module, so this never compiles anything:
    /// it maps or hosts the artifact it is given and runs its registration
    /// entry point.
    ///
    /// # Errors
    ///
    /// Returns `HostError` when the native library cannot be loaded, when the
    /// module's `project_init` reports a non-zero initialization status, or
    /// when the managed backend fails to start.
    pub(crate) fn start(
        engine: &mut Engine,
        engine_api: &EngineApi,
        workspace_root: &Path,
        descriptor: &ProjectDescriptor,
    ) -> Result<Self, HostError> {
        match descriptor {
            ProjectDescriptor::None => Ok(Self::None),
            ProjectDescriptor::Native { module_path } => {
                // Native build outputs cannot be loaded in place on Windows:
                // the OS locks a mapped DLL. Load a uniquely named copy so the
                // next compilation remains free to replace the original.
                let library = ProjectLibrary::load_copy(module_path, workspace_root)?;

                // Native modules register their components and systems through
                // the stable EngineApi table before the first frame is run.
                let status = library.call_project_init(engine_api);
                if status != 0 {
                    return Err(LibraryError::InitializationFailed { status }.into());
                }
                Ok(Self::Native {
                    current: library,
                    old_libraries: Vec::new(),
                })
            }
            // The managed runtime performs assembly discovery, component
            // registration, startup commands, and system registration itself.
            ProjectDescriptor::CSharp(paths) => {
                Ok(Self::CSharp(CSharpRuntime::start(engine, paths)?))
            }
        }
    }

    /// Replace the active module with a freshly built one, preserving state.
    ///
    /// `module_path` names the artifact the host just produced. It selects the
    /// native library to map; the managed backend ignores it because its
    /// collectible loader watches the built assembly itself.
    pub(crate) fn reload(
        &mut self,
        engine: &mut Engine,
        engine_api: &EngineApi,
        workspace_root: &Path,
        module_path: Option<&Path>,
    ) {
        match self {
            // A generation that started without a project has no schema to
            // migrate and no DLL to retire, so a reload is a plain first load.
            Self::None => {
                let Some(module_path) = module_path else {
                    return;
                };
                match load_and_initialize_native(engine_api, module_path, workspace_root) {
                    Ok(library) => {
                        *self = Self::Native {
                            current: library,
                            old_libraries: Vec::new(),
                        };
                        info!(
                            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                            "loaded the first project module generation"
                        );
                    }
                    Err(message) => error!(
                        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                        error = %message,
                        "failed to load the first project module generation"
                    ),
                }
            }
            // Native reload owns schema migration and DLL lifetime handling, so
            // keep that transaction isolated in one dedicated function.
            Self::Native {
                current,
                old_libraries,
            } => {
                let Some(module_path) = module_path else {
                    error!(
                        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                        "native project reload requested without a built module path; keeping the old project module"
                    );
                    return;
                };
                reload_native(
                    current,
                    old_libraries,
                    engine,
                    engine_api,
                    workspace_root,
                    module_path,
                );
            }
            // C# source changes are compiled by the host. The collectible
            // managed loader validates the rebuilt assembly's component
            // manifest and system signatures before swapping; poll_reload
            // reports the outcome and logs any rejection.
            Self::CSharp(runtime) => {
                info!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    "C# build complete; polling managed loader"
                );
                runtime.poll_reload();
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
            current.call_project_update(engine_api);
        }
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// Copy, map, and initialize one native project generation.
///
/// # Errors
///
/// Returns a human-readable message describing whether loading or
/// initialization failed, so callers can log it without a typed conversion.
fn load_and_initialize_native(
    engine_api: &EngineApi,
    module_path: &Path,
    workspace_root: &Path,
) -> Result<ProjectLibrary, String> {
    let library = ProjectLibrary::load_copy(module_path, workspace_root)
        .map_err(|error| format!("{error}"))?;
    let status = library.call_project_init(engine_api);
    if status != 0 {
        return Err(format!("project_init reported status {status}"));
    }
    Ok(library)
}

/// Reload one native generation and migrate components whose persisted schema
/// changed across the module boundary.
fn reload_native(
    current: &mut ProjectLibrary,
    old_libraries: &mut Vec<ProjectLibrary>,
    engine: &mut Engine,
    engine_api: &EngineApi,
    workspace_root: &Path,
    module_path: &Path,
) {
    // Step 1: Load and validate the replacement library transactionally.
    // Keep `current` untouched until a complete replacement library is ready
    // to initialize.
    let new_library = match ProjectLibrary::load_copy(module_path, workspace_root) {
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

    // Step 2: Capture old schemas, clear old systems, and initialize the new
    // generation while both DLLs remain mapped. Migration may need the old
    // persistence functions after project_init registers the replacement
    // generation's component definitions.
    let previous_metadata_by_name = engine.world().capture_persist_type_metadata();
    let previous_manifest = engine.world().persist_type_manifest();

    // Registered native system closures can point into the old DLL. Remove
    // them before project_init installs closures from the replacement module.
    debug!(
        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
        "reload step 1/4: clearing old systems"
    );
    engine.clear_systems();
    debug!(
        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
        "reload step 2/4: calling project_init on the new module"
    );
    if new_library.call_project_init(engine_api) != 0 {
        // The new generation failed to register itself. Roll the engine back
        // to the previous module: project_init must be idempotent, re-registering
        // the same components and systems and only filling entities up to a
        // target count.
        error!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            "new project module failed to initialize; rolling back to the previous generation"
        );
        engine.clear_systems();
        let rollback_status = current.call_project_init(engine_api);
        if rollback_status != 0 {
            error!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                status = rollback_status,
                "rollback of the previous generation also failed; the runtime continues without gameplay systems"
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

    // Step 3: Migrate changed schemas and archive the old DLL.
    if changed_type_names.is_empty() {
        // Avoid touching archetype storage when every persisted layout is
        // byte-for-byte compatible with the previous generation. This decision
        // is reported at info level because it is the single most useful line
        // for telling a cheap reload apart from a migrating one.
        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            "project schema unchanged for all persistable component types - fast path"
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

    // Retire the previous DLL without ever unloading it. See the module-level
    // note on why retired generations are kept mapped for the process lifetime.
    debug!(
        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
        "reload step 4/4: archiving old DLL, swapping generation"
    );
    old_libraries.push(std::mem::replace(current, new_library));

    info!(
        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
        entities = engine.world().entity_count(),
        graveyard = old_libraries.len(),
        "project hot reload complete"
    );
}
